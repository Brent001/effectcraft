//! Settings > Startup & Repair > Window Graphics and crash recovery.
//!
//! Some graphics drivers can crash while a window's device is created or its surface is
//! recreated. Each launch leaves a marker until its window has drawn for SETTLE or the app
//! exits normally. A stale marker selects a safer backend for the next launch. OpenGL can draw
//! the window, but the EffectCraft compute compositor deliberately rejects GL and uses CPU
//! compositing instead.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use effectcraft_engine::prefs::{PREFS_FILE, Prefs};
use eframe::wgpu::Backends;

const MARKER: &str = "launch-pending";

/// Time the window must keep drawing before the launch is considered stable.
pub const SETTLE: Duration = Duration::from_secs(3);

/// Resolve the saved Window Graphics value to the actual wgpu backend mask.
fn backends_for_choice(choice: &str) -> Option<Backends> {
    match choice {
        "dx12" if cfg!(target_os = "windows") => Some(Backends::DX12),
        "vulkan" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(Backends::VULKAN),
        "gl" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(Backends::GL),
        _ => None,
    }
}

/// Choose the next backend after a crash during window startup.
fn crash_fallback(choice: &str) -> Option<(&'static str, &'static str)> {
    match choice {
        "auto" if cfg!(target_os = "windows") => Some(("vulkan", "Vulkan")),
        "dx12" if cfg!(target_os = "windows") => Some(("vulkan", "Vulkan")),
        "auto" if cfg!(target_os = "linux") => Some(("gl", "OpenGL compatibility")),
        "vulkan" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(("gl", "OpenGL compatibility")),
        "gl" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(("auto", "Automatic")),
        _ => None,
    }
}

#[derive(Debug, Default)]
pub struct Launch {
    /// None lets eframe use its default backend selection.
    pub backends: Option<Backends>,
    pub notice: Option<String>,
    marker: Option<PathBuf>,
    lock: Option<File>,
    first_frame: Option<Instant>,
}

impl Launch {
    /// Read Window Graphics, apply crash recovery, and leave this launch's marker.
    /// WGPU_BACKEND overrides saved preferences and disables the launch guard.
    pub fn begin(dir: Option<&Path>, env_override: bool) -> Launch {
        let Some(dir) = dir.filter(|_| !env_override) else {
            return Launch::default();
        };
        let prefs_path = dir.join(PREFS_FILE);
        let mut prefs = std::fs::read_to_string(&prefs_path).map(|text| Prefs::from_json(&text)).unwrap_or_default();
        let marker = dir.join(MARKER);
        let held = File::open(&marker).is_ok_and(|file| matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        let mut notice = None;

        if marker.exists() && !held {
            if let Some((next, label)) = crash_fallback(&prefs.startup.window_graphics) {
                prefs.startup.window_graphics = next.into();
                match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&prefs_path, prefs.to_json())) {
                    Ok(()) => log::warn!("the previous launch stopped while opening the window; trying {label}"),
                    Err(error) => log::warn!("could not save Window Graphics fallback: {error}"),
                }
                notice = Some(format!(
                    "EffectCraft's previous launch stopped while starting graphics. It will try {label} this time. You can change this in Settings > Startup & Repair > Window Graphics."
                ));
            }
        }

        let backends = backends_for_choice(&prefs.startup.window_graphics);
        let (marker, lock) = match std::fs::create_dir_all(dir).and_then(|()| File::create(&marker)) {
            Ok(file) => (Some(marker), file.try_lock().is_ok().then_some(file)),
            Err(error) => {
                log::warn!("launch marker: {error}");
                (None, None)
            }
        };
        Launch { backends, notice, marker, lock, first_frame: None }
    }

    /// Called when the window draws a frame. Removes the marker after SETTLE.
    pub fn frame_drawn(&mut self) -> Option<Duration> {
        self.frame_drawn_at(Instant::now())
    }

    fn frame_drawn_at(&mut self, now: Instant) -> Option<Duration> {
        self.marker.as_ref()?;
        let first = *self.first_frame.get_or_insert(now);
        let left = SETTLE.saturating_sub(now.saturating_duration_since(first));
        if left.is_zero() {
            self.settled();
            return None;
        }
        Some(left)
    }

    /// A normal exit is not a crash, even if the user closes the window immediately.
    pub fn exited(&mut self) {
        self.settled();
    }

    fn settled(&mut self) {
        self.lock = None;
        if let Some(marker) = self.marker.take()
            && let Err(error) = std::fs::remove_file(&marker)
        {
            log::warn!("launch marker: {error}");
        }
    }
}

/// A returned window-startup error should persist the next backend and clear the stale marker.
/// Automatic on Windows advances to Vulkan; a failed Vulkan run advances to GL; failed GL returns
/// to Automatic so the app cannot become stranded on compatibility mode.
pub fn backend_failed(dir: Option<&Path>, backend: Option<Backends>) {
    let Some(dir) = dir else {
        return;
    };
    let fallback = match backend {
        Some(Backends::DX12) => "vulkan",
        Some(Backends::VULKAN) => "gl",
        Some(Backends::GL) => "auto",
        _ if cfg!(target_os = "windows") => "vulkan",
        _ if cfg!(target_os = "linux") => "gl",
        _ => "auto",
    };
    let prefs_path = dir.join(PREFS_FILE);
    let mut prefs = std::fs::read_to_string(&prefs_path).map(|text| Prefs::from_json(&text)).unwrap_or_default();
    prefs.startup.window_graphics = fallback.into();
    if let Err(error) = std::fs::write(&prefs_path, prefs.to_json()) {
        log::warn!("saving Window Graphics failed: {error}");
    }
    let _ = std::fs::remove_file(dir.join(MARKER));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window_graphics(dir: &Path) -> String {
        Prefs::from_json(&std::fs::read_to_string(dir.join(PREFS_FILE)).unwrap_or_default()).startup.window_graphics
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ec-launch-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn explicit_window_graphics_choices_map_to_backends() {
        assert_eq!(backends_for_choice("auto"), None);
        if cfg!(target_os = "windows") {
            assert_eq!(backends_for_choice("dx12"), Some(Backends::DX12));
            assert_eq!(backends_for_choice("vulkan"), Some(Backends::VULKAN));
            assert_eq!(backends_for_choice("gl"), Some(Backends::GL));
        } else if cfg!(target_os = "linux") {
            assert_eq!(backends_for_choice("vulkan"), Some(Backends::VULKAN));
            assert_eq!(backends_for_choice("gl"), Some(Backends::GL));
            assert_eq!(backends_for_choice("dx12"), None);
        } else {
            assert_eq!(backends_for_choice("dx12"), None);
            assert_eq!(backends_for_choice("vulkan"), None);
            assert_eq!(backends_for_choice("gl"), None);
        }
    }

    #[test]
    fn crash_fallback_prefers_vulkan_before_gl_on_windows() {
        if cfg!(target_os = "windows") {
            assert_eq!(crash_fallback("auto"), Some(("vulkan", "Vulkan")));
            assert_eq!(crash_fallback("dx12"), Some(("vulkan", "Vulkan")));
            assert_eq!(crash_fallback("vulkan"), Some(("gl", "OpenGL compatibility")));
            assert_eq!(crash_fallback("gl"), Some(("auto", "Automatic")));
        } else if cfg!(target_os = "linux") {
            assert_eq!(crash_fallback("auto"), Some(("gl", "OpenGL compatibility")));
            assert_eq!(crash_fallback("vulkan"), Some(("gl", "OpenGL compatibility")));
            assert_eq!(crash_fallback("gl"), Some(("auto", "Automatic")));
        } else {
            assert_eq!(crash_fallback("auto"), None);
        }
    }

    #[test]
    fn crashed_automatic_launch_persists_the_next_backend() {
        let dir = temp_dir("fallback");
        let first = Launch::begin(Some(&dir), false);
        assert_eq!(first.backends, None);
        drop(first);

        let mut next = Launch::begin(Some(&dir), false);
        if cfg!(target_os = "windows") {
            assert_eq!(next.backends, Some(Backends::VULKAN));
            assert_eq!(window_graphics(&dir), "vulkan");
            assert!(next.notice.as_deref().unwrap_or_default().contains("Vulkan"));
        } else if cfg!(target_os = "linux") {
            assert_eq!(next.backends, Some(Backends::GL));
            assert_eq!(window_graphics(&dir), "gl");
        } else {
            assert_eq!(next.backends, None);
        }
        next.exited();
        assert!(!dir.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_window_startup_removes_its_marker() {
        let dir = temp_dir("settle");
        let mut launch = Launch::begin(Some(&dir), false);
        assert!(dir.join(MARKER).exists());
        let now = Instant::now();
        assert_eq!(launch.frame_drawn_at(now), Some(SETTLE));
        assert_eq!(launch.frame_drawn_at(now + SETTLE), None);
        assert!(!dir.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn environment_backend_override_skips_saved_choices_and_guard() {
        let dir = temp_dir("env-override");
        let launch = Launch::begin(Some(&dir), true);
        assert_eq!(launch.backends, None);
        assert!(!dir.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returned_startup_error_advances_saved_choice() {
        if !cfg!(target_os = "windows") && !cfg!(target_os = "linux") {
            return;
        }
        let dir = temp_dir("returned-failure");
        std::fs::create_dir_all(&dir).unwrap();
        let mut prefs = Prefs::default();
        if cfg!(target_os = "windows") {
            prefs.startup.window_graphics = "dx12".into();
            std::fs::write(dir.join(PREFS_FILE), prefs.to_json()).unwrap();
            backend_failed(Some(&dir), Some(Backends::DX12));
            assert_eq!(window_graphics(&dir), "vulkan");
            backend_failed(Some(&dir), Some(Backends::VULKAN));
            assert_eq!(window_graphics(&dir), "gl");
        } else {
            prefs.startup.window_graphics = "vulkan".into();
            std::fs::write(dir.join(PREFS_FILE), prefs.to_json()).unwrap();
            backend_failed(Some(&dir), Some(Backends::VULKAN));
            assert_eq!(window_graphics(&dir), "gl");
        }
        backend_failed(Some(&dir), Some(Backends::GL));
        assert_eq!(window_graphics(&dir), "auto");
        assert!(!dir.join(MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
