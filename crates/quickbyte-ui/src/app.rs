//! The root `eframe::App`: owns the shared UI state — the view model, the main
//! window, and the registry of open secondary windows — and runs the event pump.
//!
//! # Event flow
//!
//! Core services publish into channels and call the [`UiDispatcher`], which
//! asks egui to repaint the root viewport. eframe then runs `App::logic` —
//! even while the main window is hidden in the tray — which calls
//! [`UiState::pump`]: every stream (downloads, queues, settings, browser
//! captures, tray, second-instance hand-offs, external requests) is drained on
//! the UI thread, in order, into the one [`ViewModel`]. Reactions follow
//! (open a details window on Added, swap it for the completion window on
//! Completed, start a power countdown when a queue drains), and every open
//! secondary window that shows an affected download is asked to repaint. A
//! secondary window's own frame pumps first too, so whichever window draws
//! first applies the events and all of them read the same numbers.
//!
//! # Windows
//!
//! Secondary windows are deferred viewports registered by the root's `ui()`
//! each frame from the registry ([`UiState::open_window`]). Close-to-tray marks
//! every open one hidden (they stop being registered, so their OS windows go)
//! and hides the root; showing the main window again re-registers the same set,
//! at the positions they had.
//!
//! # Repainting
//!
//! Nothing ticks. A frame happens on input or an event; while the eased
//! progress is still moving (or a window says it is animating), the viewport
//! that shows it asks for the next frame in 16 ms, and stops asking when it
//! arrives. A hidden or minimised window renders nothing.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Pos2, Ui, ViewportBuilder, ViewportCommand, ViewportId};
use parking_lot::Mutex;
use quickbyte_core::enums::{DownloadStatus, PowerAction};
use quickbyte_core::events::{Dispatcher, DownloadEvent, EventStream, QueueEvent};
use quickbyte_core::models::{
    CapturedDownload, DownloadItem, DownloadRequest, DownloadSettings, UpdateCheckResult,
};
use quickbyte_core::traits::BrowserEvent;
use quickbyte_shell::tray::TrayEvent;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::actions::{self, CompletionPlan};
use crate::dispatcher::UiDispatcher;
use crate::services::{AppServices, Pending};
use crate::shell::{Shell, TrayChannels};
use crate::theme;
use crate::view_model::{ViewModel, VmEffect};
use crate::widgets::animator;
use crate::windows::complete::CompleteWindow;
use crate::windows::details::DetailsWindow;
use crate::windows::main::MainWindow;
use crate::windows::refresh_address::RefreshAddressWindow;
use crate::windows::{AppCommand, AppWindow, DialogRequest, WindowCx, WindowKind, build_dialog};

/// Requests from outside the UI thread: the composition root's hand-offs (a
/// second launch, a browser capture, the scheduler agent, `qb`). See [`AppHandle`].
pub enum External {
    /// A later launch's payload: a URL, `run-queue:<guid>`, or nothing.
    SecondInstance(String),
    /// A link the browser extension intercepted.
    Captured(Box<CapturedDownload>),
    /// The scheduler agent says this queue is due.
    StartQueue(Uuid),
    /// `qb add`: added as Add Download would, without the details window, and
    /// never started by the add itself. The answer goes back on `reply`.
    AddFromCommandLine {
        request: Box<DownloadRequest>,
        reply: crossbeam_channel::Sender<Result<DownloadItem, String>>,
    },
    /// "QuickByte is still running" for a launch that put nothing on screen.
    NotifyStartedMinimized,
}

/// A thread-safe handle the composition root keeps: anything learnt on a
/// background thread goes through here and is carried out on the UI thread.
#[derive(Clone)]
pub struct AppHandle {
    tx: crossbeam_channel::Sender<External>,
    dispatcher: Arc<UiDispatcher>,
}

impl AppHandle {
    pub fn send(&self, request: External) {
        let _ = self.tx.send(request);
        self.dispatcher.notify();
    }
}

/// What the composition root decided before the window existed.
#[derive(Default)]
pub struct StartupOptions {
    pub start_minimized: bool,
    /// The scheduler agent started this launch for a queue (`--run-queue`).
    pub startup_queue: Option<Uuid>,
    /// A URL passed to the *first* launch still deserves the Add window.
    pub startup_url: Option<String>,
    /// `--theme light|dark` (demo screenshots); `None` follows the setting.
    pub theme_override: Option<bool>,
    /// Second-launch payloads from the single-instance pipe.
    pub handoffs: Option<crossbeam_channel::Receiver<String>>,
    /// Windows to open at once (demo: `--open details`).
    pub open_details_of_first_active: bool,
    /// `--open <dialog>[:<detail>]`: a dialog to open at start (screenshots).
    pub open_dialog: Option<String>,
}

struct Streams {
    downloads: EventStream<DownloadEvent>,
    queues: EventStream<QueueEvent>,
    settings: EventStream<Arc<DownloadSettings>>,
    browser: EventStream<BrowserEvent>,
    tray: Option<TrayChannels>,
    handoffs: Option<crossbeam_channel::Receiver<String>>,
    external: crossbeam_channel::Receiver<External>,
}

/// One open secondary window.
struct OpenWindow {
    kind: WindowKind,
    viewport: ViewportId,
    /// Taken out while the window draws.
    window: Option<Box<dyn AppWindow>>,
    builder: ViewportBuilder,
    title: String,
    /// Put away by close-to-tray; comes back with the main window.
    hidden: bool,
    closing: bool,
    position: Option<Pos2>,
    last_frame: Option<Instant>,
}

/// The settings applied live, as last applied.
#[derive(Clone, PartialEq)]
struct LiveSettings {
    speed_limit: i64,
    /// Enabled, port.
    bridge: (bool, i32),
    proxy: quickbyte_core::models::ProxySettings,
    user_agent: Option<String>,
}

impl LiveSettings {
    fn of(s: &DownloadSettings) -> Self {
        Self {
            speed_limit: s.global_speed_limit_bytes_per_second,
            bridge: (s.browser_integration_enabled, s.browser_integration_port),
            proxy: s.proxy.clone(),
            user_agent: s.custom_user_agent.clone(),
        }
    }
}

struct ThemeTracker {
    dark: bool,
    override_dark: Option<bool>,
    system_dark: Option<bool>,
    last_system_check: Instant,
}

/// Everything the windows share. Behind one mutex because egui's deferred
/// viewports draw from callbacks; all of them run on the UI thread, one at a time.
pub struct UiState {
    pub services: AppServices,
    pub vm: ViewModel,
    pub shell: Shell,
    ctx: egui::Context,
    streams: Streams,
    main: MainWindow,
    windows: Vec<OpenWindow>,
    commands: Vec<AppCommand>,
    next_instance: u64,
    has_tray: bool,
    root_hidden: bool,
    exiting: bool,
    /// URLs whose next Added event must not open a details window (queued and
    /// batch adds, `qb add`). The C# code raised a flag around a synchronous
    /// call; here the add completes on a worker, so it is a count per address.
    suppress_details: HashMap<String, usize>,
    last_clipboard_offer: Option<String>,
    /// Secondary windows were opened, closed or hidden: the root re-registers them.
    windows_changed: bool,
    /// Where the main window is (points), to centre new windows on it.
    root_rect: Option<egui::Rect>,
    monitor_size: Option<egui::Vec2>,
    theme: ThemeTracker,
    startup: Option<StartupOptions>,
    startup_update_check: Option<Pending<quickbyte_core::Result<UpdateCheckResult>>>,
    manual_update_check: Option<Pending<quickbyte_core::Result<UpdateCheckResult>>>,
    live: Option<LiveSettings>,
}

impl UiState {
    pub fn new(
        ctx: egui::Context,
        services: AppServices,
        shell: Shell,
        dispatcher: Arc<UiDispatcher>,
        mut startup: StartupOptions,
    ) -> (Self, AppHandle) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let tray = shell.create_tray(dispatcher.clone());
        let has_tray = tray.is_some();
        let streams = Streams {
            downloads: services.downloads.subscribe(),
            queues: services.queues.subscribe(),
            settings: services.settings.subscribe(),
            browser: services.browser.subscribe(),
            tray,
            handoffs: startup.handoffs.take(),
            external: rx,
        };
        let vm = ViewModel::load(&services);
        let override_dark = startup.theme_override;
        let system_dark = shell.windows_prefers_dark();
        let dark =
            override_dark.unwrap_or_else(|| theme::resolve(vm.settings.appearance, system_dark));
        theme::apply(&ctx, dark);
        let main = MainWindow::new(shell.version(), has_tray);
        let state = Self {
            services,
            vm,
            shell,
            ctx,
            streams,
            main,
            windows: Vec::new(),
            commands: Vec::new(),
            next_instance: 1,
            has_tray,
            root_hidden: startup.start_minimized,
            exiting: false,
            suppress_details: HashMap::new(),
            last_clipboard_offer: None,
            windows_changed: false,
            theme: ThemeTracker {
                dark,
                override_dark,
                system_dark,
                last_system_check: Instant::now(),
            },
            root_rect: None,
            monitor_size: None,
            startup: Some(startup),
            startup_update_check: None,
            manual_update_check: None,
            live: None,
        };
        state.sync_clipboard_watcher();
        (state, AppHandle { tx, dispatcher })
    }

    // ------------------------------------------------------------ startup --

    /// The startup hand-offs of `Program.cs`, posted to the first frame.
    fn run_startup(&mut self) {
        let Some(startup) = self.startup.take() else {
            return;
        };
        if let Some(queue) = startup.startup_queue {
            self.start_queue_from_scheduler(queue);
        }
        if let Some(url) = startup.startup_url {
            self.handle_second_instance(&url);
        } else if startup.start_minimized {
            self.shell.tray_notify_started_minimized();
        }
        let first_active = self
            .vm
            .items()
            .find(|i| i.status == DownloadStatus::Downloading)
            .map(|i| i.id);
        if startup.open_details_of_first_active
            && let Some(id) = first_active
        {
            self.open_details(id);
        }
        self.apply_live_settings();
        if let Some(name) = startup.open_dialog.as_deref() {
            self.open_named_dialog(name);
        }
        // The unattended update check: it only ever *offers*, and a failure is
        // swallowed — nobody asked for it.
        let updates = self.services.updates.clone();
        let version = self.shell.version().to_string();
        self.startup_update_check = Some(self.services.run(async move {
            updates
                .check_for_update(&version, &CancellationToken::new())
                .await
        }));
    }

    /// `--open <name>[:<detail>]` — a dialog straight away, for screenshots and
    /// for trying a window without clicking to it.
    fn open_named_dialog(&mut self, spec: &str) {
        let (name, detail) = match spec.split_once(':') {
            Some((n, d)) => (n, Some(d)),
            None => (spec, None),
        };
        let add = |url: &str| DialogRequest::NewDownload {
            url: Some(url.to_string()),
            captured: None,
        };
        let request = match name {
            "add" => DialogRequest::NewDownload {
                url: detail.map(str::to_string),
                captured: None,
            },
            "add-file" => add("https://downloads.example.org/tools/quickbyte-sample-setup.exe"),
            "add-hls" => add(crate::demo::HLS_URL),
            "add-auth" => add(crate::demo::AUTH_URL),
            "add-live" => add(crate::demo::LIVE_URL),
            "batch" => {
                let window = crate::windows::batch::BatchWindow::new(&self.vm)
                    .with_links(crate::demo::BATCH_LINKS);
                self.open_window(Box::new(window));
                return;
            }
            "settings" => {
                let window = crate::windows::settings::SettingsWindow::new(
                    &self.vm,
                    self.services.browser.token(),
                    self.shell.startup_registered(),
                )
                .on_tab(detail.unwrap_or("connection"));
                self.open_window(Box::new(window));
                return;
            }
            "queues" => {
                let window =
                    crate::windows::queues::QueuesWindow::new().on_tab(detail.unwrap_or("files"));
                self.open_window(Box::new(window));
                return;
            }
            "update" | "update-auto" => DialogRequest::Update {
                manifest: Box::new(crate::demo::update_manifest()),
                automatic: name == "update-auto",
            },
            "credentials" => DialogRequest::Credentials {
                host: crate::demo::AUTH_URL.to_string(),
                message: crate::demo::AUTH_MESSAGE.to_string(),
            },
            "checksum" => match self
                .vm
                .items()
                .find(|i| i.status == DownloadStatus::Completed)
            {
                Some(item) => DialogRequest::Checksum(item.id),
                None => return,
            },
            // The failed one with bytes on disk: the case the window exists for.
            "refresh" => match self
                .vm
                .items()
                .filter(|i| actions::can_refresh_address(i))
                .max_by_key(|i| (i.status == DownloadStatus::Failed, i.downloaded_bytes))
            {
                Some(item) => DialogRequest::RefreshAddress(item.id),
                None => return,
            },
            "complete" => {
                // The verified one, so the ✓ line shows.
                let verified = self
                    .vm
                    .items()
                    .find(|i| {
                        i.checksum_result
                            == quickbyte_core::helpers::file_checksum::ChecksumResult::Match
                    })
                    .map(|i| i.id);
                if let Some(id) = verified {
                    self.open_window(Box::new(CompleteWindow::new(id)));
                }
                return;
            }
            "about" => DialogRequest::About,
            "power" => DialogRequest::PowerCountdown {
                action: PowerAction::ShutDown,
                reason: "Queue \"Overnight\" finished".into(),
                still_running: 2,
            },
            _ => return,
        };
        self.commands.push(AppCommand::Open(request));
    }

    // --------------------------------------------------------------- pump --

    /// Drains every stream into the view model and reacts. `current` is the
    /// viewport drawing right now (it does not need asking to repaint).
    pub fn pump(&mut self, current: ViewportId) {
        let mut effects = Vec::new();
        while let Ok(event) = self.streams.downloads.try_recv() {
            if let Some(effect) = self.vm.apply_download_event(event, &self.services) {
                effects.push(effect);
            }
        }
        while let Ok(event) = self.streams.queues.try_recv() {
            if let Some(effect) = self.vm.apply_queue_event(event, &self.services) {
                effects.push(effect);
            }
        }
        let mut settings_changed = false;
        while let Ok(settings) = self.streams.settings.try_recv() {
            self.vm.apply_settings(settings);
            settings_changed = true;
        }
        for effect in effects {
            self.on_effect(effect);
        }
        if settings_changed {
            self.on_settings_changed();
        }
        while let Ok(event) = self.streams.browser.try_recv() {
            if let BrowserEvent::DownloadCaptured(captured) = event {
                self.handle_captured_download(captured);
            }
        }
        let tray_events: Vec<TrayEvent> = self
            .streams
            .tray
            .as_ref()
            .map(|t| t.events.try_iter().collect())
            .unwrap_or_default();
        for event in tray_events {
            self.on_tray_event(event);
        }
        let clipboard: Vec<String> = self
            .streams
            .tray
            .as_ref()
            .map(|t| t.clipboard.try_iter().collect())
            .unwrap_or_default();
        for text in clipboard {
            self.on_clipboard_changed(&text);
        }
        let handoffs: Vec<String> = self
            .streams
            .handoffs
            .as_ref()
            .map(|h| h.try_iter().collect())
            .unwrap_or_default();
        for payload in handoffs {
            self.handle_second_instance(&payload);
        }
        let external: Vec<External> = self.streams.external.try_iter().collect();
        for request in external {
            self.on_external(request);
        }
        self.poll_update_checks();

        // Every window showing a download that changed draws again.
        let dirty = self.vm.take_dirty();
        if !dirty.is_empty() {
            for w in &self.windows {
                if w.viewport != current
                    && !w.hidden
                    && w.window
                        .as_ref()
                        .and_then(|x| x.download())
                        .is_some_and(|d| dirty.contains(&d))
                {
                    self.ctx.request_repaint_of(w.viewport);
                }
            }
            if current != ViewportId::ROOT && !self.root_hidden {
                self.ctx.request_repaint_of(ViewportId::ROOT);
            }
        }
        self.vm.advance_animation();
    }

    fn on_effect(&mut self, effect: VmEffect) {
        match effect {
            VmEffect::Added(id) => {
                let Some(url) = self.vm.item(id).map(|i| i.url.clone()) else {
                    return;
                };
                let suppressed = match self.suppress_details.get_mut(&url) {
                    Some(n) if *n > 0 => {
                        *n -= 1;
                        true
                    }
                    _ => false,
                };
                self.suppress_details.retain(|_, n| *n > 0);
                if self.vm.settings.auto_open_details_window && !suppressed {
                    self.open_details(id);
                }
            }
            VmEffect::Removed(id) => {
                self.main.forget(id);
                self.close_window(&WindowKind::Details(id));
            }
            VmEffect::StatusChanged { id, new, .. } => {
                self.main.on_status_changed(id);
                if new == DownloadStatus::Completed {
                    self.on_download_completed(id);
                }
            }
            VmEffect::QueuesChanged => {
                self.shell
                    .sync_scheduler(self.services.queues.has_scheduled_queues(), &self.vm.queues);
            }
            VmEffect::QueueStateChanged {
                queue_id,
                state,
                drained,
            } => {
                if state != quickbyte_core::enums::QueueState::Idle {
                    return;
                }
                let Some(queue) = self.vm.queues.iter().find(|q| q.id == queue_id).cloned() else {
                    return;
                };
                let plan = CompletionPlan::for_queue(&queue, drained, self.vm.items());
                if plan.has_power_action() {
                    self.start_power_countdown(&plan, format!("Queue \"{}\" finished", queue.name));
                }
            }
        }
    }

    /// IDM-style hand-off: the progress window makes way for a completion window
    /// rather than leaving two windows describing the same download.
    fn on_download_completed(&mut self, id: Uuid) {
        self.close_window(&WindowKind::Details(id));
        let Some(item) = self.vm.item(id).cloned() else {
            return;
        };
        let plan = CompletionPlan::for_download(&item, self.vm.items());
        if plan.reveal_folder {
            self.shell
                .reveal_in_explorer(&item.full_path(), &item.save_folder);
        }
        if plan.play_sound {
            self.shell.play_notification_sound();
        }
        if self.vm.settings.show_completion_window {
            self.open_window(Box::new(CompleteWindow::new(id)));
        }
        if plan.has_power_action() {
            self.start_power_countdown(&plan, format!("{} finished downloading", item.file_name));
        }
    }

    fn start_power_countdown(&mut self, plan: &CompletionPlan, reason: String) {
        // One at a time: a queue draining as its last download completes asks
        // twice for what is one decision.
        if self
            .windows
            .iter()
            .any(|w| w.kind == WindowKind::PowerCountdown)
        {
            return;
        }
        self.shell.play_notification_sound();
        self.commands
            .push(AppCommand::Open(DialogRequest::PowerCountdown {
                action: plan.power,
                reason,
                still_running: plan.still_running,
            }));
    }

    fn on_settings_changed(&mut self) {
        self.sync_clipboard_watcher();
        self.update_theme(true);
        self.apply_live_settings();
    }

    /// The settings that apply on Save rather than at the next download: the
    /// global speed limit, the browser bridge (on/off, port), the proxy and the
    /// custom User-Agent. Only what changed is touched.
    fn apply_live_settings(&mut self) {
        let s = self.vm.settings.clone();
        let applied = LiveSettings::of(&s);
        let previous = self.live.replace(applied.clone());
        if previous.as_ref() == Some(&applied) {
            return;
        }
        if previous
            .as_ref()
            .is_none_or(|p| p.speed_limit != applied.speed_limit)
        {
            self.services
                .downloads
                .set_global_speed_limit(applied.speed_limit);
        }
        // At startup the composition root has already started the bridge and
        // configured the proxy and User-Agent; after that, only changes count.
        let Some(previous) = previous else { return };
        // The real BrowserIntegrationServer watches the settings itself (as the C#
        // one did) and re-listens on its own; only the demo fake needs telling.
        if self.shell.is_demo() && previous.bridge != applied.bridge {
            // Moving the port means listening again; turning it off stops it.
            self.services.browser.stop();
            if applied.bridge.0 {
                self.services.browser.start();
            }
        }
        if !self.shell.is_demo()
            && (previous.proxy != applied.proxy || previous.user_agent != applied.user_agent)
        {
            // Program.cs re-applied both on every save; the next request uses them.
            quickbyte_core::net::app_proxy::AppProxy::shared().configure(Some(&s.proxy));
            quickbyte_core::helpers::app_user_agent::configure(s.custom_user_agent.as_deref());
        }
    }

    fn sync_clipboard_watcher(&self) {
        self.shell
            .set_clipboard_listening(self.vm.settings.monitor_clipboard);
    }

    fn on_tray_event(&mut self, event: TrayEvent) {
        match event {
            TrayEvent::Open => self.show_main(),
            // Nothing else comes back with it: the Add window stands on its own.
            TrayEvent::AddUrl => self
                .commands
                .push(AppCommand::Open(DialogRequest::NewDownload {
                    url: None,
                    captured: None,
                })),
            TrayEvent::PauseAll => self.pause_everything(),
            TrayEvent::ResumeAll => self.resume_all(),
            TrayEvent::Exit => self.exit(),
            TrayEvent::ClipboardLinkAccepted(link) => {
                self.commands
                    .push(AppCommand::Open(DialogRequest::NewDownload {
                        url: Some(link),
                        captured: None,
                    }))
            }
        }
    }

    /// Something was copied. Only a clipboard holding a single link to one of the
    /// chosen file types is looked at, and a link that is already a download is
    /// not offered back. A notification rather than a window: the user was in
    /// the middle of copying something, and a window taking focus would catch
    /// their next keystrokes.
    fn on_clipboard_changed(&mut self, text: &str) {
        if !self.vm.settings.monitor_clipboard {
            return;
        }
        let Some(link) =
            actions::clipboard_download_link(text, &self.vm.settings.clipboard_file_types)
        else {
            return;
        };
        if self.last_clipboard_offer.as_deref() == Some(link.as_str()) {
            return;
        }
        let address = quickbyte_core::helpers::url_credentials::extract(&link).url;
        if self.vm.items().any(|i| i.url == address) {
            return;
        }
        self.shell
            .tray_offer_clipboard_link(&link, &actions::file_name_from_url(&address));
        self.last_clipboard_offer = Some(link);
    }

    fn on_external(&mut self, request: External) {
        match request {
            External::SecondInstance(payload) => self.handle_second_instance(&payload),
            External::Captured(captured) => self.handle_captured_download(*captured),
            External::StartQueue(id) => self.start_queue_from_scheduler(id),
            External::AddFromCommandLine { request, reply } => {
                self.add_from_command_line(*request, reply)
            }
            External::NotifyStartedMinimized => self.shell.tray_notify_started_minimized(),
        }
    }

    fn poll_update_checks(&mut self) {
        if let Some(pending) = self.startup_update_check.as_mut()
            && let Some(result) = pending.poll()
        {
            self.startup_update_check = None;
            if let Ok(UpdateCheckResult {
                update_available: true,
                manifest: Some(manifest),
                ..
            }) = result
            {
                self.commands.push(AppCommand::Open(DialogRequest::Update {
                    manifest: Box::new(manifest),
                    automatic: false,
                }));
            }
        }
        if let Some(pending) = self.manual_update_check.as_mut()
            && let Some(result) = pending.poll()
        {
            self.manual_update_check = None;
            self.main
                .on_manual_update_check(result, self.shell.version(), &mut self.commands);
        }
    }

    // --------------------------------------------- composition-root entry points --

    /// A second QuickByte launch handed its command line over: a queue the
    /// scheduler says is due starts silently; a link opens Add Download on its
    /// own; a bare launch means "show me the window".
    pub fn handle_second_instance(&mut self, payload: &str) {
        let args = [payload.to_string()];
        if let Some(queue) = find_queue_id(&args, self.shell) {
            self.start_queue_from_scheduler(queue);
            return;
        }
        if let Some(url) = find_url(&args, self.shell) {
            self.commands
                .push(AppCommand::Open(DialogRequest::NewDownload {
                    url: Some(url),
                    captured: None,
                }));
            return;
        }
        self.show_main();
    }

    /// The browser extension handed over a link. Only the Add window comes up:
    /// the click happened in the browser and the user is still looking at it.
    pub fn handle_captured_download(&mut self, captured: CapturedDownload) {
        // A Refresh Address window waiting for exactly this link — the user went
        // back to the page and clicked the download again — gets it instead. The
        // most recently opened one wins: it is the one the user just asked for.
        let waiting = self
            .windows
            .iter_mut()
            .rev()
            .filter(|w| !w.closing)
            .find_map(|w| {
                let viewport = w.viewport;
                let window = w
                    .window
                    .as_mut()?
                    .as_any_mut()
                    .downcast_mut::<RefreshAddressWindow>()?;
                (window.is_waiting_for_capture()
                    && actions::matches_capture(window.item(), &captured))
                .then(|| {
                    window.accept_capture(&captured);
                    viewport
                })
            });
        if let Some(viewport) = waiting {
            self.ctx
                .send_viewport_cmd_to(viewport, ViewportCommand::Minimized(false));
            self.ctx
                .send_viewport_cmd_to(viewport, ViewportCommand::Focus);
            self.ctx.request_repaint_of(viewport);
            return;
        }
        self.commands
            .push(AppCommand::Open(DialogRequest::NewDownload {
                url: None,
                captured: Some(Box::new(captured)),
            }));
    }

    /// Starts a queue because the scheduler agent said so — silently.
    pub fn start_queue_from_scheduler(&mut self, queue: Uuid) {
        self.services.queues.start_from_scheduler(queue);
    }

    /// How `qb add` adds a download: as Add Download would, without the details
    /// window, and without starting it.
    pub fn add_from_command_line(
        &mut self,
        mut request: DownloadRequest,
        reply: crossbeam_channel::Sender<Result<DownloadItem, String>>,
    ) {
        request.start_immediately = false;
        *self
            .suppress_details
            .entry(request.url.clone())
            .or_default() += 1;
        let downloads = self.services.downloads.clone();
        self.services.spawn(async move {
            let _ = reply.send(
                downloads
                    .add_download(request)
                    .await
                    .map_err(|e| e.to_string()),
            );
        });
    }

    // ----------------------------------------------------------- commands --

    pub fn process_commands(&mut self) {
        // Commands can produce commands (opening a dialog may ask for another).
        for _ in 0..8 {
            if self.commands.is_empty() {
                break;
            }
            for command in std::mem::take(&mut self.commands) {
                self.execute(command);
            }
        }
    }

    fn execute(&mut self, command: AppCommand) {
        match command {
            AppCommand::Open(request) => {
                let window = build_dialog(
                    request,
                    &self.services,
                    &self.vm,
                    self.shell,
                    self.next_instance,
                );
                self.next_instance += 1;
                self.open_window(window);
            }
            AppCommand::OpenDetails(id) => self.open_details(id),
            AppCommand::OpenWindow(window) => self.open_window(window),
            AppCommand::Close(kind) => self.close_window(&kind),
            AppCommand::AddDownload { request, queue } => self.add_download(*request, queue),
            AppCommand::AddBatch {
                requests,
                queue,
                new_queue_name,
            } => self.add_batch(requests, queue, new_queue_name),
            AppCommand::ShowMain => self.show_main(),
            AppCommand::HideToTray => self.hide_to_tray(),
            AppCommand::Exit => self.exit(),
            AppCommand::PauseEverything => self.pause_everything(),
            AppCommand::ResumeAll => self.resume_all(),
            AppCommand::PerformPowerAction(action) => self.perform_power_action(action),
            AppCommand::SettingsChanged => self.on_settings_changed(),
        }
    }

    /// Add Download's OK. A queued add is awaited (it returns as soon as the item
    /// exists) so the item can be filed; a started one is not — it completes
    /// when the download does.
    fn add_download(&mut self, mut request: DownloadRequest, queue: Option<Uuid>) {
        let downloads = self.services.downloads.clone();
        match queue {
            Some(queue_id) => {
                request.start_immediately = false;
                *self
                    .suppress_details
                    .entry(request.url.clone())
                    .or_default() += 1;
                let queues = self.services.queues.clone();
                self.services.spawn(async move {
                    if let Ok(item) = downloads.add_download(request).await {
                        queues.add_to_queue(queue_id, &[item.id]);
                    }
                });
            }
            None => self
                .services
                .spawn(async move { downloads.add_download(request).await }),
        }
    }

    /// A batch: all into one queue, or all started now — and no details window
    /// for any of them (two hundred windows opening at once is no use to anyone).
    fn add_batch(
        &mut self,
        requests: Vec<DownloadRequest>,
        queue: Option<Uuid>,
        new_queue_name: Option<String>,
    ) {
        let queue = match new_queue_name {
            Some(name) => Some(self.services.queues.create(&name).id),
            None => queue,
        };
        for request in &requests {
            *self
                .suppress_details
                .entry(request.url.clone())
                .or_default() += 1;
        }
        let downloads = self.services.downloads.clone();
        let queues = self.services.queues.clone();
        self.services.spawn(async move {
            let mut added = Vec::new();
            for mut request in requests {
                request.start_immediately = queue.is_none();
                if queue.is_some() {
                    if let Ok(item) = downloads.add_download(request).await {
                        added.push(item.id);
                    }
                } else {
                    let downloads = downloads.clone();
                    tokio::spawn(async move { downloads.add_download(request).await });
                }
            }
            if let Some(queue) = queue
                && !added.is_empty()
            {
                queues.add_to_queue(queue, &added);
            }
        });
    }

    /// Stop All, from the toolbar or the tray. Running queues are stopped first:
    /// pausing everything while a queue is still walking its list would only hand
    /// the freed slot to the next file in that queue.
    pub fn pause_everything(&mut self) {
        for q in &self.vm.queues {
            self.services.queues.stop(q.id);
        }
        self.services.downloads.pause_all();
    }

    fn resume_all(&mut self) {
        let ids: Vec<Uuid> = self
            .vm
            .items()
            .filter(|i| actions::can_resume(i))
            .map(|i| i.id)
            .collect();
        let downloads = self.services.downloads.clone();
        self.services.spawn(async move {
            futures_join(ids.into_iter().map(|id| {
                let d = downloads.clone();
                async move { d.resume(id).await }
            }))
            .await
        });
    }

    fn perform_power_action(&mut self, action: PowerAction) {
        if action == PowerAction::ExitApp {
            self.exit();
            return;
        }
        // Sleeping on top of open connections leaves them to time out into failures.
        self.pause_everything();
        if !self.shell.perform_power_action(action) {
            self.main.show_message(
                "QuickByte",
                &format!(
                    "Windows did not let QuickByte {}.",
                    self.shell.power_verb(action)
                ),
            );
            self.show_main();
        }
    }

    // ------------------------------------------------------------ windows --

    /// Opens a secondary window, or brings forward the one of the same kind.
    pub fn open_window(&mut self, window: Box<dyn AppWindow>) {
        let kind = window.kind();
        if let Some(existing) = self
            .windows
            .iter_mut()
            .find(|w| w.kind == kind && !w.closing)
        {
            existing.hidden = false;
            let viewport = existing.viewport;
            self.ctx
                .send_viewport_cmd_to(viewport, ViewportCommand::Minimized(false));
            self.ctx
                .send_viewport_cmd_to(viewport, ViewportCommand::Focus);
            self.windows_changed = true;
            return;
        }
        let mut builder = window
            .builder(&self.vm)
            .with_icon(Arc::new(crate::icons::window_icon()))
            .with_taskbar(true);
        // Centred on the main window (or the screen while it is in the tray), and
        // stepped diagonally so several opened together do not land in one stack.
        let cascade = self
            .windows
            .iter()
            .filter(|w| !w.closing && !w.hidden)
            .count();
        let centre = match (self.root_hidden, self.root_rect) {
            (false, Some(rect)) => Some(rect.center()),
            _ => self.monitor_size.map(|m| (m / 2.0).to_pos2()),
        };
        if let Some(centre) = centre {
            let size = builder.inner_size.unwrap_or(egui::vec2(480.0, 320.0));
            let offset = (cascade % 6) as f32 * 26.0;
            builder =
                builder.with_position(centre - size / 2.0 + egui::vec2(offset, offset - 16.0));
        }
        let title = window.title(&self.vm);
        self.windows.push(OpenWindow {
            kind: kind.clone(),
            viewport: kind.viewport_id(),
            window: Some(window),
            builder,
            title,
            hidden: false,
            closing: false,
            position: None,
            last_frame: None,
        });
        self.windows_changed = true;
    }

    pub fn open_details(&mut self, id: Uuid) {
        if self.vm.get(id).is_none() {
            return;
        }
        self.open_window(Box::new(DetailsWindow::new(id, &self.vm)));
    }

    pub fn close_window(&mut self, kind: &WindowKind) {
        if let Some(w) = self.windows.iter_mut().find(|w| &w.kind == kind) {
            w.closing = true;
            self.windows_changed = true;
            self.ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    /// A window of type `T`, for delivering something to it.
    pub fn window_mut<T: AppWindow>(&mut self) -> Option<&mut T> {
        self.windows
            .iter_mut()
            .filter(|w| !w.closing)
            .find_map(|w| {
                w.window
                    .as_mut()
                    .and_then(|x| x.as_any_mut().downcast_mut::<T>())
            })
    }

    /// Puts the whole application out of sight, not just the main window: the
    /// secondary windows are independent, so they are hidden too and remembered.
    pub fn hide_to_tray(&mut self) {
        for w in &mut self.windows {
            if !w.closing {
                w.hidden = true;
            }
        }
        self.windows_changed = true;
        self.root_hidden = true;
        self.ctx
            .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(false));
        self.shell.tray_notify_hidden();
    }

    pub fn show_main(&mut self) {
        let was_hidden = self.root_hidden;
        self.root_hidden = false;
        for w in &mut self.windows {
            w.hidden = false;
        }
        self.windows_changed = true;
        let ctx = &self.ctx;
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Visible(true));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Focus);
        if was_hidden {
            ctx.send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::RequestPaintWhileHidden);
        }
    }

    /// The only path that really closes QuickByte.
    pub fn exit(&mut self) {
        self.exiting = true;
        self.ctx
            .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::Close);
    }

    // -------------------------------------------------------------- theme --

    /// Follows the setting (and Windows' app mode when the setting says System),
    /// re-checked at most every few seconds and on every settings change.
    fn update_theme(&mut self, force: bool) {
        let t = &mut self.theme;
        if self.vm.settings.appearance == quickbyte_core::enums::AppTheme::System
            && (force || t.last_system_check.elapsed() > Duration::from_secs(3))
        {
            t.system_dark = self.shell.windows_prefers_dark();
            t.last_system_check = Instant::now();
        }
        let dark = t
            .override_dark
            .unwrap_or_else(|| theme::resolve(self.vm.settings.appearance, t.system_dark));
        if dark != t.dark {
            t.dark = dark;
            theme::apply(&self.ctx, dark);
            self.apply_title_bar_theme();
        }
    }

    /// Dark (or light) native title bars on every window. winit asks DWM for the
    /// immersive dark caption, which is what `DarkMode.UseDarkTitleBar` did.
    fn apply_title_bar_theme(&self) {
        let theme = theme::system_theme();
        self.ctx
            .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::SetTheme(theme));
        for w in &self.windows {
            self.ctx
                .send_viewport_cmd_to(w.viewport, ViewportCommand::SetTheme(theme));
        }
    }

    // ------------------------------------------------------------- frames --

    /// The root's `logic` (runs even while the main window is hidden).
    fn logic(&mut self) {
        self.run_startup();
        self.pump(ViewportId::ROOT);
        self.process_commands();
        self.update_theme(false);
        let totals = self.vm.totals();
        self.shell.tray_tooltip(totals.active, totals.speed);
        self.shell
            .keep_awake(totals.active > 0 && self.vm.settings.keep_awake_while_downloading);
        if self.windows_changed && self.root_hidden {
            // A window must open while the main one is in the tray (a download
            // finished): the root has to run a pass to register it.
            self.ctx
                .send_viewport_cmd_to(ViewportId::ROOT, ViewportCommand::RequestPaintWhileHidden);
            self.ctx.request_repaint_of(ViewportId::ROOT);
        }
    }

    /// The root's `ui`: the main window. Returns the viewports to register.
    fn root_ui(&mut self, ui: &mut Ui) -> Vec<(ViewportId, ViewportBuilder, WindowKind)> {
        let (rect, monitor) = ui
            .ctx()
            .input(|i| (i.viewport().outer_rect, i.viewport().monitor_size));
        self.root_rect = rect.or(self.root_rect);
        self.monitor_size = monitor.or(self.monitor_size);
        // The title bar's X is a hide button, unless Exit asked for it.
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            if self.exiting || !self.has_tray {
                self.exiting = true;
                // Pausing leaves the chunks on disk and persists the paused status,
                // so the next launch resumes exactly where this one stopped.
                self.services.downloads.pause_all();
            } else {
                ui.ctx().send_viewport_cmd(ViewportCommand::CancelClose);
                self.hide_to_tray();
            }
        }

        let mut cx = WindowCx {
            services: &self.services,
            vm: &mut self.vm,
            shell: self.shell,
            viewport: ViewportId::ROOT,
            dt: ui.input(|i| i.stable_dt),
            commands: &mut self.commands,
            close: false,
        };
        self.main.ui(ui, &mut cx, &mut self.manual_update_check);
        self.process_commands();

        if !self.root_hidden && (self.vm.progress.is_animating() || self.main.is_animating()) {
            ui.ctx().request_repaint_after(animator::frame_interval());
        }

        // Windows that finished closing go; the rest are registered.
        let mut closed = Vec::new();
        self.windows.retain_mut(|w| {
            if w.closing {
                closed.push(w.viewport);
                if let Some(mut window) = w.window.take() {
                    let mut cx = WindowCx {
                        services: &self.services,
                        vm: &mut self.vm,
                        shell: self.shell,
                        viewport: w.viewport,
                        dt: 0.0,
                        commands: &mut self.commands,
                        close: false,
                    };
                    window.on_closed(&mut cx);
                }
                false
            } else {
                true
            }
        });
        for viewport in closed {
            self.shell.taskbar_forget(viewport);
        }
        self.windows_changed = false;
        self.windows
            .iter()
            .filter(|w| !w.hidden)
            .map(|w| {
                let builder = match w.position {
                    Some(pos) => w.builder.clone().with_position(pos),
                    None => w.builder.clone(),
                };
                (w.viewport, builder, w.kind.clone())
            })
            .collect()
    }

    /// One frame of a secondary window.
    fn window_ui(&mut self, kind: &WindowKind, ui: &mut Ui) {
        let Some(index) = self.windows.iter().position(|w| &w.kind == kind) else {
            return;
        };
        let viewport = self.windows[index].viewport;
        self.pump(viewport);
        self.process_commands();
        // The pump may have opened or closed windows: find this one again.
        let Some(index) = self.windows.iter().position(|w| &w.kind == kind) else {
            return;
        };
        let Some(mut window) = self.windows[index].window.take() else {
            return;
        };

        let now = Instant::now();
        let dt = self.windows[index]
            .last_frame
            .map_or(0.016, |t| (now - t).as_secs_f32());
        self.windows[index].last_frame = Some(now);
        let first_frame = self.windows[index].position.is_none();
        if first_frame {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::SetTheme(theme::system_theme()));
        }

        let mut cx = WindowCx {
            services: &self.services,
            vm: &mut self.vm,
            shell: self.shell,
            viewport,
            dt,
            commands: &mut self.commands,
            close: false,
        };
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            if window.on_close_requested(&mut cx) {
                cx.close = true;
            } else {
                ui.ctx().send_viewport_cmd(ViewportCommand::CancelClose);
            }
        }
        window.ui(ui, &mut cx);
        let close = cx.close;
        let title = window.title(&self.vm);
        let animating = window.is_animating()
            || window
                .download()
                .is_some_and(|d| self.vm.progress.is_key_animating(&d));

        let entry = &mut self.windows[index];
        if title != entry.title {
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::Title(title.clone()));
            entry.title = title;
        }
        if let Some(rect) = ui.ctx().input(|i| i.viewport().outer_rect) {
            entry.position = Some(rect.min);
        }
        entry.window = Some(window);
        if close {
            entry.closing = true;
            self.windows_changed = true;
            self.ctx.request_repaint_of(ViewportId::ROOT);
        } else if animating {
            ui.ctx().request_repaint_after(animator::frame_interval());
        }
        self.process_commands();
        if self.windows_changed {
            self.ctx.request_repaint_of(ViewportId::ROOT);
        }
    }
}

/// Awaits every future (a tiny `join_all`, so the crate needs no `futures`).
async fn futures_join<F: std::future::Future>(futures: impl Iterator<Item = F>) {
    for f in futures {
        f.await;
    }
}

/// `run-queue:<guid>` / `--run-queue <guid>`.
fn find_queue_id(args: &[String], shell: Shell) -> Option<Uuid> {
    if !shell.is_demo() {
        return quickbyte_shell::single_instance::find_queue_id(args);
    }
    args.iter().find_map(|a| {
        a.trim()
            .strip_prefix("run-queue:")
            .and_then(|g| Uuid::parse_str(g.trim()).ok())
    })
}

/// The first http/https/ftp/ftps absolute URL.
fn find_url(args: &[String], shell: Shell) -> Option<String> {
    if !shell.is_demo() {
        return quickbyte_shell::single_instance::find_url(args);
    }
    args.iter()
        .map(|a| a.trim().trim_matches('"'))
        .find(|a| {
            url::Url::parse(a)
                .is_ok_and(|u| matches!(u.scheme(), "http" | "https" | "ftp" | "ftps"))
        })
        .map(str::to_string)
}

// ----------------------------------------------------------------------------
// eframe::App
// ----------------------------------------------------------------------------

pub struct QuickByteApp {
    shared: Arc<Mutex<UiState>>,
}

impl QuickByteApp {
    pub fn new(state: UiState) -> Self {
        Self {
            shared: Arc::new(Mutex::new(state)),
        }
    }
}

impl eframe::App for QuickByteApp {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.shared.lock().logic();
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        // Collected under the lock, registered after it is released: an embedded
        // viewport (no multi-window support) would call back into the lock.
        let registrations = self.shared.lock().root_ui(ui);
        for (viewport, builder, kind) in registrations {
            let shared = self.shared.clone();
            ui.ctx()
                .show_viewport_deferred(viewport, builder, move |ui, _class| {
                    shared.lock().window_ui(&kind, ui);
                });
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let state = self.shared.lock();
        // Stops the schedule timer; the downloads were paused on close.
        state.services.queues.shutdown();
        state.services.downloads.flush();
        state.shell.drop_tray();
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::palette().window.to_normalized_gamma_f32()
    }

    fn persist_egui_memory(&self) -> bool {
        false
    }
}
