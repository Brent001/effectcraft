use super::{Arc, EffectcraftApp, RenderSource, gpu_failure, menus, panels};

impl EffectcraftApp {
    pub fn render_source(&self) -> RenderSource {
        RenderSource {
            project: self.session.project.clone(),
            footage: self.session.footage.clone(),
            expr: self.session.expr.clone(),
            layer_cache: self.session.layer_cache.clone(),
            gpu: self.gpu.clone(),
            gpu_display: self.gpu_display(),
            disk: self.session.disk_cache.clone(),
        }
    }

    /// Keep GPU-composited viewer frames on the GPU (drawn from their texture). The browser can't
    /// read frames back (its thread can't wait for the GPU), so there the viewer shows GPU frames
    /// directly whenever it draws them plainly (RGB, no exposure, no region of interest); the
    /// desktop reads them back (see [`frames::RenderSource::gpu_display`]).
    fn gpu_display_eligible(
        channel: effectcraft_engine::viewer::Channel,
        exposure_zero: bool,
        has_roi: bool,
        extended_view: bool,
        needs_display_color_conversion: bool,
    ) -> bool {
        channel == effectcraft_engine::viewer::Channel::Rgb
            && exposure_zero
            && !has_roi
            && !extended_view
            && !needs_display_color_conversion
    }

    fn gpu_display(&self) -> bool {
        let v = &self.session.state.viewer;
        let plain_rgb = Self::gpu_display_eligible(
            v.channel,
            v.exposure == 0.0,
            self.session.state.region_of_interest.is_some(),
            self.ui.viewer.extended.is_some(),
            effectcraft_engine::viewer::DisplayColor::of(&self.session).is_some(),
        );
        plain_rgb && (cfg!(target_arch = "wasm32") || self.session.disk_cache.is_none())
    }

    /// The UI draws its panels and handles input such as dropped files. Until the fonts are
    /// installed (the first frames) it draws nothing, and that input goes unhandled.
    pub fn is_ready(&self) -> bool {
        self.fonts_ready
    }

    /// Provide the executable/embedding host's device notifications. The host installs the
    /// handlers once; this frontend never replaces handlers on a borrowed device.
    pub fn set_gpu_failure_bridge(&mut self, bridge: gpu_failure::GpuFailureBridge) {
        if let Some(gpu) = &self.gpu {
            bridge.forward_to(gpu.context().device_failure_notifier());
        }
        self.gpu_failures = Some(bridge);
    }

    pub(super) fn apply_gpu_failure(&mut self, ctx: &egui::Context) {
        let Some(bridge) = &self.gpu_failures else { return };
        // Readback deadlines can retire the compositor without a host device notification.
        if let Some(gpu) = &self.gpu
            && let Err(reason) = gpu.context().check_health()
        {
            bridge.report(&reason, false);
        }
        let Some(failure) = bridge.failure() else { return };
        if self.gpu_failure_applied == Some(failure.presentation_lost) {
            return;
        }
        self.gpu_failure_applied = Some(failure.presentation_lost);
        self.gpu_checked = true;
        self.frames.retire_gpu();
        self.session.accel = None;
        self.session.accel_note = Some(format!("GPU preview retired: {}", failure.reason));
        self.session.layer_cache.clear();
        self.gpu = None;
        if let Some(rs) = &self.wgpu {
            let mut renderer = rs.renderer.write();
            if let Some((id, _, _)) = &self.viewer_native {
                renderer.free_texture(id);
            }
            for (_, texture) in self.passive_tex.values() {
                if let panels::viewers::PassiveTexture::Gpu(id, _) = texture {
                    renderer.free_texture(id);
                }
            }
        }
        self.viewer_native = None;
        self.passive_tex.retain(|_, (_, texture)| matches!(texture, panels::viewers::PassiveTexture::Cpu(_)));
        self.viewer_shown = self.viewer_tex.as_ref().map(|(texture, key, _)| (texture.id(), *key));
        self.viewer_image = None;
        // Old completion closures retain only their detached slot, not the replacement.
        self.viewer_readback = Default::default();
        self.wgpu = None;
        self.ui.status = if failure.presentation_lost {
            "Graphics device lost. The project is still open; presentation requires restarting the application.".into()
        } else {
            "Shared GPU preview disabled; CPU compositing remains available.".into()
        };
        log::warn!("GPU preview retired: {}", failure.reason);
        ctx.request_repaint();
    }

    /// After the graphics device is lost, eframe's renderer cannot draw again: any mesh makes
    /// egui-wgpu panic on the main thread when it cannot stage its index buffer (#591). Draw
    /// nothing from then on, auto-save a project with unsaved changes (keeping the crash
    /// recovery marker, so the next launch offers it) and close the window.
    pub(super) fn close_after_device_loss(&mut self, ctx: &egui::Context) {
        if self.device_lost_close.is_some() {
            return;
        }
        let keep_recovery = self.session.is_dirty();
        let saved = keep_recovery.then(|| match self.session.autosave_now().and_then(|path| self.session.autosave_wait().map(|()| path)) {
            Ok(path) => {
                log::warn!("graphics device lost: auto-saved {path}; the next launch offers to recover it");
                Ok(path)
            }
            Err(e) => {
                log::error!("graphics device lost: {e}");
                Err(e.to_string())
            }
        });
        self.device_lost_close = Some(keep_recovery);
        log::warn!("graphics device lost: closing the window");
        if let Some(tell) = &self.hooks.device_lost {
            tell(saved);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Run the commands whose non-blocking file dialogs have been answered.
    pub(crate) fn run_dialog_answers(&mut self) {
        while let Ok(answer) = self.dialog_answers.1.try_recv() {
            self.file_dialogs.answered();
            menus::dialog_answered(self, answer);
        }
    }

    /// Set up the GPU compositor on egui-wgpu's device (once). Without an adapter that runs
    /// compute shaders (WebGL2, software GL) everything stays on the CPU.
    pub(crate) fn init_gpu(&mut self, ctx: &egui::Context, frame: &eframe::Frame) {
        if self.gpu_checked {
            #[cfg(not(target_arch = "wasm32"))]
            self.poll_gpu_init(ctx);
            return;
        }
        self.gpu_checked = true;
        if self.gpu_failures.as_ref().is_some_and(|bridge| bridge.failure().is_some()) {
            return;
        }
        let Some(rs) = frame.wgpu_render_state() else { return };
        let info = rs.adapter.get_info();
        self.session.window_adapter = Some(format!(
            "{} ({:?}, {:?}{})",
            info.name,
            info.backend,
            info.device_type,
            if info.driver.is_empty() { String::new() } else { format!(", {} {}", info.driver, info.driver_info) }
        ));

        #[cfg(not(target_arch = "wasm32"))]
        {
            let adapter = rs.adapter.clone();
            let device = rs.device.clone();
            let queue = rs.queue.clone();
            let worker_label = format!("{} ({:?}, {:?})", info.name, info.backend, info.device_type);
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::Builder::new().name("effectcraft-gpu-init".into()).spawn(move || {
                let worker_started = std::time::Instant::now();
                log::info!("GPU compositor initialization worker: entering Gpu::new for {worker_label}");
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| effectcraft_gpu::Gpu::new(&adapter, device, queue)))
                    .map_err(|payload| {
                        let reason = payload
                            .downcast_ref::<String>()
                            .map(String::as_str)
                            .or_else(|| payload.downcast_ref::<&str>().copied())
                            .unwrap_or("backend panicked");
                        format!("GPU compositor setup panicked: {reason}")
                    })
                    .and_then(|result| result);
                match &result {
                    Ok(_) => log::info!("GPU compositor initialization worker: Gpu::new succeeded in {:?}", worker_started.elapsed()),
                    Err(error) => log::warn!("GPU compositor initialization worker: Gpu::new failed after {:?}: {error}", worker_started.elapsed()),
                }
                let _ = tx.send(result);
            });
            match worker {
                Ok(_) => {
                    self.gpu_init_rx = Some(rx);
                    self.gpu_init_render_state = Some(rs.clone());
                    self.gpu_init_started = Some(std::time::Instant::now());
                    self.session.accel_note =
                        Some("GPU compositor is initializing in the background; CPU compositing remains active until it is ready.".into());
                    log::info!("GPU compositor: pipeline initialization started in background for {} ({:?})", info.name, info.backend);
                    ctx.request_repaint_after(std::time::Duration::from_millis(80));
                }
                Err(error) => {
                    let reason = format!("could not start GPU initialization worker: {error}");
                    log::warn!("{reason}; using CPU compositing");
                    self.session.accel_note = Some(reason);
                }
            }
            return;
        }

        #[cfg(target_arch = "wasm32")]
        {
            // Browser initialization stays on its existing event-loop path; native threads are
            // unavailable there.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| effectcraft_gpu::Gpu::new(&rs.adapter, rs.device.clone(), rs.queue.clone())))
                .map_err(|payload| {
                    let reason =
                        payload.downcast_ref::<String>().map(String::as_str).or_else(|| payload.downcast_ref::<&str>().copied()).unwrap_or("backend panicked");
                    format!("GPU compositor setup panicked: {reason}")
                })
                .and_then(|result| result);
            self.complete_gpu_init(rs.clone(), result);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn poll_gpu_init(&mut self, ctx: &egui::Context) {
        if self.gpu_failures.as_ref().is_some_and(|bridge| bridge.failure().is_some()) {
            self.gpu_init_rx = None;
            self.gpu_init_render_state = None;
            self.gpu_init_started = None;
            return;
        }

        const GPU_INIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
        if self.gpu_init_started.is_some_and(|started| started.elapsed() >= GPU_INIT_TIMEOUT) {
            // This is a warning threshold, not a hard cancellation: a slow shader compiler may
            // still finish. Keep the receiver and render state so poll_gpu_init can adopt a late
            // success without forcing a restart. Setting the timestamp to None ensures we report
            // the slow initialization only once.
            self.gpu_init_started = None;
            let reason = concat!(
                "GPU compositor initialization is taking longer than 30 seconds; CPU compositing remains ",
                "active while initialization continues. Check Logs/effectcraft.log for GPU errors. If it ",
                "never becomes ready, change Window Graphics and restart to retry."
            )
            .to_string();
            log::warn!("{reason}");
            self.session.accel_note = Some(reason);
        }

        let result = match self.gpu_init_rx.as_ref().map(|rx| rx.try_recv()) {
            Some(Ok(result)) => result,
            Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {
                ctx.request_repaint_after(std::time::Duration::from_millis(80));
                return;
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => Err("GPU initialization worker exited without a result".to_string()),
            None => return,
        };

        self.gpu_init_rx = None;
        self.gpu_init_started = None;
        let Some(rs) = self.gpu_init_render_state.take() else {
            self.session.accel_note = Some("GPU initialization finished without a presentation state; using CPU compositing.".into());
            return;
        };
        self.complete_gpu_init(rs, result);
    }

    fn complete_gpu_init(&mut self, rs: eframe::egui_wgpu::RenderState, result: Result<effectcraft_gpu::Gpu, String>) {
        match result {
            Ok(g) => {
                if let Some(bridge) = &self.gpu_failures {
                    bridge.forward_to(g.context().device_failure_notifier());
                    if bridge.failure().is_some() {
                        return;
                    }
                }
                log::info!("GPU compositor: {}", effectcraft_engine::render::Accelerator::name(&g));
                self.session.accel_note = None;
                self.session.accel = Some(Arc::new(g.clone()));
                self.gpu = Some(g);
                self.wgpu = Some(rs);
            }
            Err(error) => {
                log::info!("GPU compositor unavailable: {error}");
                self.session.accel_note = Some(format!("GPU compositor unavailable: {error}"));
            }
        }
    }

    /// The viewer shows a frame composited on the GPU (drawn from its wgpu texture).
    pub fn viewer_on_gpu(&self) -> bool {
        matches!((&self.viewer_shown, &self.viewer_native), (Some(s), Some(n)) if s.1 == n.1)
    }

    /// The GPU compositor's adapter, when the viewer has one.
    pub fn gpu_adapter(&self) -> Option<String> {
        self.gpu.as_ref().map(effectcraft_engine::render::Accelerator::name)
    }
}

#[cfg(test)]
mod gpu_failure_tests {
    use super::*;
    use crate::{FrameKey, ItemId, RenderOpts, Session, Tick, json};

    #[test]
    fn host_failure_preserves_document_undo_and_cpu_pixels_and_detaches_late_readbacks() {
        let mut session = Session::default();
        let comp = ItemId(session.execute("comp.new", json!({"name": "Recovery", "width": 4, "height": 4, "duration": 1})).unwrap()["comp"].as_u64().unwrap());
        session.execute("layer.newSolid", json!({"comp": comp.0, "color": "#ff0000"})).unwrap();
        let before = session.render(comp, Tick::ZERO, RenderOpts::default());
        let project = session.project.clone();
        let revision = session.revision;
        let prefs = serde_json::to_value(&session.prefs).unwrap();
        let mut app = EffectcraftApp::new(session);
        let ctx = egui::Context::default();
        let bridge = gpu_failure::GpuFailureBridge::new(&ctx);
        app.set_gpu_failure_bridge(bridge.clone());
        let late_slot = app.viewer_readback.1.clone();
        let key = FrameKey { revision, content: 1, comp: comp.0, frame: 0, scale: 1000, view: 0, opts: 0 };
        app.viewer_readback.0 = Some(key);
        bridge.report("synthetic host device failure", true);
        app.apply_gpu_failure(&ctx);
        assert!(Arc::ptr_eq(&project, &app.session.project));
        assert_eq!(app.session.revision, revision);
        assert_eq!(serde_json::to_value(&app.session.prefs).unwrap(), prefs);
        assert!(app.gpu_checked, "failed device cannot be initialized again next frame");
        assert!(app.session.accel.is_none());
        assert!(app.ui.status.contains("presentation requires restarting"));
        assert_eq!(app.session.render(comp, Tick::ZERO, RenderOpts::default()).data, before.data);
        *late_slot.lock().unwrap() = Some((key, Arc::new(egui::ColorImage::new([1, 1], vec![egui::Color32::RED]))));
        assert!(app.viewer_readback.0.is_none());
        assert!(app.viewer_readback.1.lock().unwrap().is_none(), "a late completion cannot refill the replacement slot");
        app.session.execute("edit.undo", json!({})).unwrap();
        assert!(app.session.project.comp(comp).unwrap().layers.is_empty());
        app.session.execute("edit.redo", json!({})).unwrap();
        assert_eq!(app.session.render(comp, Tick::ZERO, RenderOpts::default()).data, before.data);
    }
}
