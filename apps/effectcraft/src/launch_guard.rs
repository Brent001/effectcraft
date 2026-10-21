//! Settings ▸ Startup & Repair ▸ Window Graphics, and the launch guard behind it.
//!
//! Some graphics drivers crash EffectCraft as its window's device is created (an access
//! violation in an older Intel UHD driver with DirectX 12 / Vulkan, #243), or a moment after
//! its first frame, as the window is shown and its surface re-created (AMD's Vulkan driver for
//! a Radeon RX 6800M, Adrenalin 26.9.2: an access violation in amdvlk64.dll on every launch).
//! A crash in a driver can't be caught, so each launch leaves a marker in the settings folder
//! until its window has kept drawing for [`SETTLE`] (or the app quits normally before then). A
//! launch that finds the previous one's marker switches Window Graphics to OpenGL, which those
//! drivers run, and says so once the window is up. `WGPU_BACKEND` (wgpu's own override) still
//! wins over both. The marker is locked while its launch runs, so a second copy opened meanwhile
//! isn't taken for a crash; and a window OpenGL can't open either puts Window Graphics back to
//! Automatic ([`opengl_failed`]) rather than failing every launch.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use effectcraft_engine::prefs::{PREFS_FILE, Prefs};
use eframe::wgpu::Backends;

/// Left in the settings folder from a launch until its window has settled.
const MARKER: &str = "launch-pending";

/// How long the window keeps drawing after its first frame before the launch counts as made.
/// A driver that fails as the window is shown and its surface re-created does so within the
/// first second; a user quitting sooner than this is told the next launch uses OpenGL and can
/// switch back in Settings.
pub const SETTLE: Duration = Duration::from_secs(3);

/// Resolve a saved Window Graphics choice. OpenGL may draw the UI, but the compute compositor
/// rejects it; Windows users need a working DX12 or Vulkan path for accelerated effects.
fn backends_for_choice(choice: &str) -> Option<Backends> {
    match choice {
        "dx12" if cfg!(target_os = "windows") => Some(Backends::DX12),
        "vulkan" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(Backends::VULKAN),
        "gl" if cfg!(any(target_os = "windows", target_os = "linux")) => Some(Backends::GL),
        _ => None,
    }
}

/// On an unsuccessful launch, try another backend on the next start. Automatic/DX12 tries Vulkan
/// on Windows first; Vulkan then tries OpenGL compatibility; OpenGL returns to Automatic to avoid
/// a retry loop. Linux has no DX12 backend.
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

/// This launch's window graphics.
#[derive(Debug, Default)]
pub struct Launch {
    /// The graphics APIs the window may use (`None`: eframe's default).
    pub backends: Option<Backends>,
    /// Switched to OpenGL because the previous launch stopped as its window opened: the message
    /// to show.
    pub notice: Option<String>,
    marker: Option<PathBuf>,
    /// Holds the marker's lock until the launch settles (or the process ends).
    lock: Option<File>,
    /// When the window drew its first frame, until the launch settles.
    first_frame: Option<Instant>,
}

impl Launch {
    /// Read Window Graphics in the settings folder `dir`, switch it to OpenGL when the previous
    /// launch stopped as its window opened (where OpenGL exists: not macOS), and leave this
    /// launch's marker. `env_override`: `WGPU_BACKEND` is set.
    pub fn begin(dir: Option<&Path>, env_override: bool) -> Launch {
        let Some(dir) = dir.filter(|_| !env_override) else { return Launch::default() };
        let prefs_path = dir.join(PREFS_FILE);
        let mut prefs = std::fs::read_to_string(&prefs_path).map(|t| Prefs::from_json(&t)).unwrap_or_default();
        let marker = dir.join(MARKER);
        // Locked by another launch that is still opening its window: not a crash.
        let held = File::open(&marker).is_ok_and(|f| matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock)));
        let mut notice = None;
        if marker.exists() && !held {
            if let Some((next, label)) = crash_fallback(&prefs.startup.window_graphics) {
                prefs.startup.window_graphics = next.into();
                match std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&prefs_path, prefs.to_json())) {
                    Ok(()) => log::warn!("the last launch stopped as its window opened: trying {label}"),
                    Err(e) => log::warn!("the last launch stopped as its window opened; saving Window Graphics failed: {e}"),
                }
                notice = Some(format!(
                    "EffectCraft's last launch stopped while starting graphics. It will try {label} this time. \\
                     You can choose another backend in Settings ▸ Startup & Repair ▸ Window Graphics."
                ));
            }
        }
        let backends = backends_for_choice(&prefs.startup.window_graphics);
        let (marker, lock) = match std::fs::create_dir_all(dir).and_then(|()| File::create(&marker)) {
            // Where locks aren't available the guard works as before.
            Ok(f) => (Some(marker), f.try_lock().is_ok().then_some(f)),
            Err(e) => {
                log::warn!("launch marker: {e}");
                (None, None)
            }
        };
        Launch { backends, notice, marker, lock, first_frame: None }
    }

    /// The window drew a frame. Until the launch has settled, returns how long that still takes
    /// (ask for a repaint then, so that an idle window settles too).
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

    /// The app quits normally: however soon after opening, that's no crash.
    pub fn exited(&mut self) {
        self.settled();
    }

    /// The launch made it: release and remove its marker.
    fn settled(&mut self) {
        self.lock = None;
        if let Some(m) = self.marker.take()
            && let Err(e) = std::fs::remove_file(&m)
        {
            log::warn!("launch marker: {e}");
        }
    }
}

/// The selected backend failed to create the window. Save the next fallback and clear the
/// launch marker so returned startup errors don't cause repeated failures with the same backend.
pub fn backend_failed(dir: Option<&Path>, backend: Option<Backends>) {
    let Some(dir) = dir else { return };
    let fallback = match backend {
        Some(Backends::DX12) => "vulkan",
        Some(Backends::VULKAN) => "gl",
        Some(Backends::GL) => "auto",
        _ if cfg!(target_os = "windows") => "vulkan",
        _ if cfg!(target_os = "linux") => "gl",
        _ => "#[cfg(test)]
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
    fn explicit_window_graphics_choices_map_to_the_selected_backend() {
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
    fn crash_fallback_prefers_vulkan_before_opengl_on_windows() {
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
        if cfg!(not(target_os = "windows") && not(target_os = "linux")) {
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
