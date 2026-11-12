//! The windows, one module each (MIGRATION.md §7.4), and the small framework
//! that lets them be real OS windows.
//!
//! # How a secondary window works
//!
//! Every window other than the main one implements [`AppWindow`] and is shown as
//! an egui **deferred viewport** — its own native window with its own taskbar
//! button, unowned and modeless, as the C# forms were (`MainForm.ShowIndependently`).
//! The app keeps the open ones in a registry keyed by [`WindowKind`]; a kind is
//! also the window's identity, so asking for a window that is already open
//! brings that one forward instead of opening a second (one details window per
//! download, one Options window, …).
//!
//! Each frame of a window: the app locks the shared state, drains the event
//! streams into the one [`crate::view_model::ViewModel`], takes the window out
//! of the registry, calls [`AppWindow::ui`] with a [`WindowCx`], and puts it
//! back. A window never talks to another window directly: it pushes an
//! [`AppCommand`] (open a dialog, add a download, close a window, exit …) that
//! the app carries out after the frame.
//!
//! # Dialogs
//!
//! Every dialog is asked for with an [`AppCommand::Open`] carrying a
//! [`DialogRequest`] — the main window's menus, toolbar, tray and hand-offs
//! do — and [`build_dialog`] turns a request into its window. A dialog that
//! needs an answer from another window (Add Download asking Sign In for a
//! login) opens it with [`AppCommand::OpenWindow`] and holds the receiving end
//! of a channel; the answering window wakes it when it replies.

pub mod about;
pub mod batch;
pub mod checksum;
pub mod complete;
pub mod credentials;
pub mod details;
pub mod main;
pub mod new_download;
pub mod power_countdown;
pub mod prompt;
pub mod queues;
pub mod refresh_address;
pub mod settings;
pub mod update;
pub mod virus_total;

use std::any::Any;

use egui::{Ui, ViewportBuilder, ViewportId};
use quickbyte_core::enums::PowerAction;
use quickbyte_core::models::{CapturedDownload, DownloadRequest, UpdateManifest};
use uuid::Uuid;

use crate::services::AppServices;
use crate::shell::Shell;
use crate::view_model::ViewModel;

/// Which window — and, for the ones there can be several of, which instance.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WindowKind {
    /// One per download (`DownloadDetailsForm`).
    Details(Uuid),
    /// Replaces the details window when a download finishes (`DownloadCompleteForm`).
    Complete(Uuid),
    /// Add New Download. Several may be open: each browser capture brings its own link.
    NewDownload(u64),
    Batch,
    Settings,
    Queues,
    Update,
    /// A sign-in prompt; one per refused probe.
    Credentials(u64),
    Checksum(Uuid),
    RefreshAddress(Uuid),
    About,
    PowerCountdown,
    /// VirusTotal flagged a download and asks what to do with it; one per download.
    VirusTotal(Uuid),
}

impl WindowKind {
    /// The egui viewport this window lives in.
    pub fn viewport_id(&self) -> ViewportId {
        ViewportId::from_hash_of(("quickbyte-window", self))
    }
}

/// A dialog someone asked for. Part 2 implements the windows these open.
#[derive(Clone, Debug)]
pub enum DialogRequest {
    /// Add New Download, optionally pre-filled with a link or a browser capture.
    NewDownload {
        url: Option<String>,
        captured: Option<Box<CapturedDownload>>,
    },
    Batch,
    Settings,
    Queues,
    /// The update window. `automatic`: the user asked (Help > Check for Updates),
    /// so it goes straight to downloading; otherwise it is the startup prompt.
    Update {
        manifest: Box<UpdateManifest>,
        automatic: bool,
    },
    /// Sign in to `host`, repeating what the server said.
    Credentials {
        host: String,
        message: String,
    },
    Checksum(Uuid),
    RefreshAddress(Uuid),
    About,
    /// The countdown before a power action. `reason`: "x.iso finished downloading".
    PowerCountdown {
        action: PowerAction,
        reason: String,
        still_running: usize,
    },
}

/// What a window (or the tray, or a hand-off) asks the app to do after the frame.
#[derive(Debug)]
pub enum AppCommand {
    /// Opens (or brings forward) a dialog.
    Open(DialogRequest),
    OpenDetails(Uuid),
    /// Opens a window built elsewhere (part 2 dialogs that open further windows).
    OpenWindow(Box<dyn AppWindow>),
    Close(WindowKind),
    /// Adds a download as Add Download's OK does: started now, or filed into a
    /// queue without starting (and without a details window).
    AddDownload {
        request: Box<DownloadRequest>,
        queue: Option<Uuid>,
    },
    /// Adds a batch, all into one queue (an existing one, or a new one named
    /// `new_queue_name`) or all started now — with no details windows.
    AddBatch {
        requests: Vec<DownloadRequest>,
        queue: Option<Uuid>,
        new_queue_name: Option<String>,
    },
    /// The main window back on screen, with the secondary windows it hid.
    ShowMain,
    HideToTray,
    /// The only path that really closes QuickByte.
    Exit,
    /// Stop All: stops running queues, then pauses every download.
    PauseEverything,
    ResumeAll,
    /// The power countdown ran out (or "do it now" was pressed).
    PerformPowerAction(PowerAction),
    /// Re-reads the settings-dependent bits (theme, clipboard watching) now.
    SettingsChanged,
}

impl std::fmt::Debug for dyn AppWindow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AppWindow({:?})", self.kind())
    }
}

/// Everything a window can reach while it draws.
pub struct WindowCx<'a> {
    pub services: &'a AppServices,
    pub vm: &'a mut ViewModel,
    pub shell: Shell,
    /// The viewport being drawn.
    pub viewport: ViewportId,
    /// Seconds since the window's previous frame (for animation).
    pub dt: f32,
    pub(crate) commands: &'a mut Vec<AppCommand>,
    pub(crate) close: bool,
}

impl WindowCx<'_> {
    /// Asks the app to do something after this frame.
    pub fn command(&mut self, command: AppCommand) {
        self.commands.push(command);
    }

    /// Closes this window after this frame.
    pub fn close(&mut self) {
        self.close = true;
    }

    pub fn open(&mut self, request: DialogRequest) {
        self.commands.push(AppCommand::Open(request));
    }
}

/// A secondary window. Implementations hold their own state; the shared state
/// is reached through [`WindowCx`]. `Send` because egui keeps the viewport's
/// callback on its own; the window itself is only ever touched on the UI thread.
pub trait AppWindow: Send + Any {
    fn kind(&self) -> WindowKind;

    /// The native window as it first opens: title, size, minimum size,
    /// resizability. Read once.
    fn builder(&self, vm: &ViewModel) -> ViewportBuilder;

    /// The live title; the app sends it to the OS window whenever it changes.
    fn title(&self, vm: &ViewModel) -> String;

    /// Draws the window. `ui` covers the whole client area, with no margin and
    /// no background.
    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>);

    /// Whether something is still animating — the app then asks for the next
    /// frame in ~16 ms, and stops asking the moment this turns false.
    fn is_animating(&self) -> bool {
        false
    }

    /// The download this window describes; it is repainted when that download's
    /// events arrive.
    fn download(&self) -> Option<Uuid> {
        None
    }

    /// The title bar's X. Return false to keep the window open.
    fn on_close_requested(&mut self, _cx: &mut WindowCx<'_>) -> bool {
        true
    }

    /// Called once as the window goes away.
    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {}

    /// For delivering something to a specific window type (a browser
    /// capture to the Refresh Address window waiting for it).
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Turns a dialog request into its window. `next_instance` numbers the kinds
/// there can be several of.
pub fn build_dialog(
    request: DialogRequest,
    services: &AppServices,
    vm: &ViewModel,
    shell: Shell,
    next_instance: u64,
) -> Box<dyn AppWindow> {
    match request {
        DialogRequest::NewDownload { url, captured } => {
            // An explicit link wins over the clipboard, which is read only when
            // there is neither.
            let clipboard = if url.is_none() && captured.is_none() {
                shell.clipboard_text()
            } else {
                None
            };
            Box::new(new_download::NewDownloadWindow::new(
                next_instance,
                url,
                captured.map(|c| *c),
                vm,
                clipboard,
            ))
        }
        DialogRequest::Batch => Box::new(batch::BatchWindow::new(vm)),
        DialogRequest::Settings => Box::new(settings::SettingsWindow::new(
            vm,
            services.browser.token(),
            shell.startup_registered(),
        )),
        DialogRequest::Queues => Box::new(queues::QueuesWindow::new()),
        DialogRequest::Update {
            manifest,
            automatic,
        } => Box::new(update::UpdateWindow::new(*manifest, automatic)),
        DialogRequest::Credentials { host, message } => Box::new(
            credentials::CredentialsWindow::for_host(&host, &message, next_instance),
        ),
        DialogRequest::Checksum(id) => Box::new(checksum::ChecksumWindow::new(
            id,
            vm,
            shell.clipboard_text(),
        )),
        DialogRequest::RefreshAddress(id) => Box::new(refresh_address::RefreshAddressWindow::new(
            id,
            vm,
            shell.clipboard_text(),
        )),
        DialogRequest::About => Box::new(about::AboutWindow),
        DialogRequest::PowerCountdown {
            action,
            reason,
            still_running,
        } => Box::new(power_countdown::PowerCountdownWindow::new(
            action,
            reason,
            still_running,
            shell,
        )),
    }
}
