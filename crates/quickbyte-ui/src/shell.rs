//! The one gate between the UI and `quickbyte_shell` — every Windows
//! integration call goes through here.
//!
//! In demo mode (`--demo`, and for now every run: see `main::REAL_SERVICES`)
//! nothing here touches the system: no single-instance mutex, no Run keys, no
//! wake task, no tray icon, no taskbar progress, no Explorer, and nothing under
//! `%APPDATA%\QuickByte`. The shell's bodies are being written in parallel and
//! are `todo!()` in this tree, so demo mode is also what keeps the UI runnable.
//!
//! Shell objects that may hold COM pointers or window handles (the tray, the
//! keep-awake request, taskbar buttons) live in a thread-local on the UI thread
//! rather than in the shared UI state, which has to be `Send` for egui's
//! deferred viewports. Every caller is on the UI thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use egui::ViewportId;
use quickbyte_core::enums::{DownloadStatus, PowerAction};
use quickbyte_core::events::Dispatcher;
use quickbyte_core::models::DownloadQueue;
use quickbyte_shell::taskbar_progress::{TaskbarProgress, TaskbarProgressState};
use quickbyte_shell::tray::{Tray, TrayEvent};

#[derive(Default)]
struct ShellLocal {
    tray: Option<Tray>,
    keep_awake: Option<quickbyte_shell::keep_awake::KeepAwake>,
    taskbars: HashMap<ViewportId, TaskbarSlot>,
}

enum TaskbarSlot {
    /// The window's HWND has not been found yet (it is looked up by title).
    Searching(u32),
    Attached(TaskbarProgress),
    Unavailable,
}

thread_local! {
    static LOCAL: RefCell<ShellLocal> = RefCell::new(ShellLocal::default());
}

/// The tray's channels, handed to the UI state (they are `Send`).
pub struct TrayChannels {
    pub events: crossbeam_channel::Receiver<TrayEvent>,
    pub clipboard: crossbeam_channel::Receiver<String>,
}

#[derive(Clone, Copy, Debug)]
pub struct Shell {
    demo: bool,
}

impl Shell {
    pub fn new(demo: bool) -> Self {
        Self { demo }
    }

    pub fn is_demo(&self) -> bool {
        self.demo
    }

    // ------------------------------------------------------------ identity --

    /// The product version shown in the title bar and About.
    pub fn version(&self) -> &'static str {
        if self.demo {
            env!("QB_PRODUCT_VERSION")
        } else {
            quickbyte_shell::app_version::display()
        }
    }

    /// Windows' app mode: `Some(true)` for dark. `None` when it cannot be read
    /// (and always in demo mode, which then shows the light palette unless told otherwise).
    pub fn windows_prefers_dark(&self) -> Option<bool> {
        if self.demo {
            None
        } else {
            Some(!quickbyte_shell::system_theme::apps_use_light_theme())
        }
    }

    // ---------------------------------------------------------------- tray --

    /// Creates the notification-area icon. `None` in demo mode or when it could
    /// not be created — the main window then really closes instead of hiding.
    pub fn create_tray(&self, waker: Arc<dyn Dispatcher>) -> Option<TrayChannels> {
        if self.demo {
            return None;
        }
        let tray = Tray::new(waker)?;
        let channels = TrayChannels {
            events: tray.events(),
            clipboard: tray.clipboard_text(),
        };
        LOCAL.with(|l| l.borrow_mut().tray = Some(tray));
        Some(channels)
    }

    fn with_tray(&self, f: impl FnOnce(&Tray)) {
        if self.demo {
            return;
        }
        LOCAL.with(|l| {
            if let Some(tray) = l.borrow().tray.as_ref() {
                f(tray);
            }
        });
    }

    pub fn tray_tooltip(&self, active: usize, speed: f64) {
        self.with_tray(|t| t.update_tooltip(active, speed));
    }
    pub fn tray_notify_hidden(&self) {
        self.with_tray(|t| t.notify_window_hidden());
    }
    pub fn tray_notify_started_minimized(&self) {
        self.with_tray(|t| t.notify_started_minimized());
    }
    pub fn tray_offer_clipboard_link(&self, link: &str, file_name: &str) {
        self.with_tray(|t| t.offer_clipboard_link(link, file_name));
    }
    pub fn set_clipboard_listening(&self, listen: bool) {
        self.with_tray(|t| t.set_clipboard_listening(listen));
    }
    pub fn drop_tray(&self) {
        LOCAL.with(|l| l.borrow_mut().tray = None);
    }

    // ---------------------------------------------------------- keep awake --

    /// Holds a power request while anything is downloading.
    pub fn keep_awake(&self, downloading: bool) {
        if self.demo {
            return;
        }
        LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            if l.keep_awake.is_none() && downloading {
                l.keep_awake = Some(quickbyte_shell::keep_awake::KeepAwake::new());
            }
            if let Some(k) = l.keep_awake.as_mut() {
                k.update(downloading);
            }
        });
    }

    // ------------------------------------------------------ taskbar progress --

    /// Reports a download's progress on its details window's taskbar button —
    /// green, yellow when paused, red on failure, a marquee while connecting.
    /// The window is found by its title (`dark_mode::find_own_windows`), so the
    /// caller passes the title it has just set. Returns false while the window is
    /// still being looked for (the caller reports again next frame).
    pub fn taskbar_report(
        &self,
        viewport: ViewportId,
        title: &str,
        status: DownloadStatus,
        size_known: bool,
        percentage: f64,
    ) -> bool {
        if self.demo {
            return true;
        }
        LOCAL.with(|l| {
            let mut l = l.borrow_mut();
            let slot = l
                .taskbars
                .entry(viewport)
                .or_insert(TaskbarSlot::Searching(0));
            if let TaskbarSlot::Searching(tries) = slot {
                *slot = match quickbyte_shell::dark_mode::find_own_windows(title).first() {
                    Some(&hwnd) => match TaskbarProgress::attach_to(hwnd) {
                        Some(progress) => TaskbarSlot::Attached(progress),
                        None => TaskbarSlot::Unavailable,
                    },
                    // The window may not exist yet on its first frame; try a few more.
                    None if *tries < 120 => TaskbarSlot::Searching(*tries + 1),
                    None => TaskbarSlot::Unavailable,
                };
            }
            match slot {
                TaskbarSlot::Attached(progress) => {
                    let state = quickbyte_shell::taskbar_progress::state_for(
                        status, size_known, percentage,
                    );
                    progress.report(state, percentage);
                    true
                }
                TaskbarSlot::Searching(_) => false,
                TaskbarSlot::Unavailable => true,
            }
        })
    }

    pub fn taskbar_forget(&self, viewport: ViewportId) {
        LOCAL.with(|l| {
            if let Some(TaskbarSlot::Attached(mut p)) = l.borrow_mut().taskbars.remove(&viewport) {
                if !self.demo {
                    p.report(TaskbarProgressState::None, 0.0);
                }
            }
        });
    }

    // ------------------------------------------------------------- launching --

    pub fn open_file(&self, path: &str) -> bool {
        !self.demo && quickbyte_shell::file_launcher::open_file(path)
    }
    pub fn reveal_in_explorer(&self, path: &str, fallback_folder: &str) {
        if !self.demo {
            quickbyte_shell::file_launcher::reveal_in_explorer(path, fallback_folder);
        }
    }
    pub fn open_folder(&self, folder: &str) -> bool {
        !self.demo && quickbyte_shell::file_launcher::open_folder(folder)
    }
    pub fn open_web_page(&self, url: &str) -> bool {
        !self.demo && quickbyte_shell::file_launcher::open_web_page(url)
    }
    pub fn play_notification_sound(&self) {
        if !self.demo {
            quickbyte_shell::file_launcher::play_notification_sound();
        }
    }
    /// Whether a file exists on disk — read-only, so allowed in demo mode, where
    /// the demo's paths simply do not exist.
    pub fn file_exists(&self, path: &str) -> bool {
        !self.demo && !path.is_empty() && std::path::Path::new(path).is_file()
    }
    pub fn folder_exists(&self, path: &str) -> bool {
        if self.demo {
            return !path.trim().is_empty();
        }
        !path.trim().is_empty() && std::path::Path::new(path).is_dir()
    }

    // ---------------------------------------------------------------- power --

    // Labels, verbs and the countdown length are pure functions: the same in
    // demo mode. Only `perform` touches the system.

    pub fn power_label(&self, action: PowerAction) -> &'static str {
        quickbyte_shell::power_actions::label(action)
    }

    pub fn power_verb(&self, action: PowerAction) -> &'static str {
        quickbyte_shell::power_actions::verb(action)
    }

    pub fn power_now_button(&self, action: PowerAction) -> &'static str {
        quickbyte_shell::power_actions::now_button(action)
    }

    /// 15 s before exiting, 60 s before anything that turns the machine off.
    pub fn power_countdown_seconds(&self, action: PowerAction) -> u32 {
        quickbyte_shell::power_actions::countdown_seconds(action)
    }

    pub fn perform_power_action(&self, action: PowerAction) -> bool {
        !self.demo && quickbyte_shell::power_actions::perform(action)
    }

    // --------------------------------------------------------- registrations --

    /// Installs or removes the background scheduler and the wake task.
    pub fn sync_scheduler(&self, has_scheduled_queues: bool, queues: &[DownloadQueue]) {
        if self.demo {
            return;
        }
        quickbyte_shell::queue_agent_registration::sync(has_scheduled_queues);
        quickbyte_shell::queue_wake_registration::sync(queues);
    }

    /// Whether the background scheduler is installed with this copy. The demo
    /// says yes, so the queue window shows the common case.
    pub fn agent_available(&self) -> bool {
        self.demo || quickbyte_shell::queue_agent_registration::is_available()
    }

    /// Whether the Run value is there (Options > System reads it first).
    pub fn startup_registered(&self) -> bool {
        !self.demo && quickbyte_shell::startup_registration::is_enabled()
    }

    /// Writes or deletes the Run value; the reason when Windows refuses.
    pub fn apply_startup_registration(&self, enabled: bool) -> Result<(), String> {
        if self.demo {
            return Ok(());
        }
        quickbyte_shell::startup_registration::try_apply(enabled)
    }

    // ------------------------------------------------------------ identity --

    /// The four-part file version for About.
    pub fn file_version(&self) -> &'static str {
        if self.demo {
            concat!(env!("QB_PRODUCT_VERSION"), ".0")
        } else {
            quickbyte_shell::app_version::file()
        }
    }

    pub fn copyright(&self) -> &'static str {
        quickbyte_shell::app_version::copyright()
    }

    /// Starts setup with the elevation prompt; true when it started.
    pub fn run_installer(&self, path: &str) -> bool {
        !self.demo && quickbyte_shell::file_launcher::run_installer(path)
    }

    // ------------------------------------------------- clipboard, pickers --

    /// The clipboard's text, for pre-filling a link. `None` in demo mode (which
    /// reads nothing of the user's) or when another program holds the clipboard.
    pub fn clipboard_text(&self) -> Option<String> {
        if self.demo {
            return None;
        }
        arboard::Clipboard::new().ok()?.get_text().ok()
    }

    /// A native "pick a folder" dialog, run on its own thread so no window
    /// freezes behind it. The answer arrives through the returned handle, and
    /// the asking window is repainted when it does.
    pub fn pick_folder(&self, start: &str, wake: Waker) -> NativePick {
        let start = start.to_string();
        NativePick::spawn(wake, move || {
            let mut dialog = rfd::FileDialog::new();
            if std::path::Path::new(&start).is_dir() {
                dialog = dialog.set_directory(&start);
            }
            dialog
                .pick_folder()
                .map(|p| p.to_string_lossy().into_owned())
        })
    }

    /// A native "open file" dialog for importing links.
    pub fn pick_import_file(&self, wake: Waker) -> NativePick {
        NativePick::spawn(wake, || {
            rfd::FileDialog::new()
                .set_title("Import links")
                .add_filter(
                    "Text and web pages (*.txt;*.htm;*.html)",
                    &["txt", "htm", "html"],
                )
                .add_filter("All files (*.*)", &["*"])
                .pick_file()
                .map(|p| p.to_string_lossy().into_owned())
        })
    }
}

/// Asks one window to draw again, from any thread.
#[derive(Clone)]
pub struct Waker {
    ctx: egui::Context,
    viewport: ViewportId,
}

impl Waker {
    pub fn new(ctx: &egui::Context, viewport: ViewportId) -> Self {
        Self {
            ctx: ctx.clone(),
            viewport,
        }
    }

    pub fn wake(&self) {
        self.ctx.request_repaint_of(self.viewport);
    }
}

/// A native dialog in flight.
pub struct NativePick {
    rx: crossbeam_channel::Receiver<Option<String>>,
}

impl NativePick {
    fn spawn(wake: Waker, f: impl FnOnce() -> Option<String> + Send + 'static) -> Self {
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send(f());
            wake.wake();
        });
        Self { rx }
    }

    /// `Some(None)` once the dialog was cancelled, `Some(Some(path))` once chosen.
    pub fn poll(&self) -> Option<Option<String>> {
        self.rx.try_recv().ok()
    }
}
