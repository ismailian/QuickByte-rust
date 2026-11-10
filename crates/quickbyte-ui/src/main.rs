//! QuickByte.exe — the composition root (port of `src/QuickByte.UI/Program.cs`).
//!
//! The only place that knows concrete service types. The order follows
//! `Program.cs` step for step: single instance first (hand off and leave if a
//! copy is running), then settings and everything that hangs off them, the
//! engine, queues, the registrations, the background sweeps, the browser
//! bridge, and finally the window with the startup hand-offs posted to its
//! first frame.
//!
//! The real Core services are being written in parallel; until they land,
//! [`build_services`] returns the in-memory demo services and every run is a
//! demo run (`--demo` forces it later). Each place a real service goes is
//! marked `// INTEGRATION:`.
#![cfg_attr(not(test), windows_subsystem = "windows")]

use std::sync::Arc;

use quickbyte_core::events::Dispatcher;

use quickbyte_ui::app::{QuickByteApp, StartupOptions, UiState};
use quickbyte_ui::dispatcher::UiDispatcher;
use quickbyte_ui::services::AppServices;
use quickbyte_ui::shell::Shell;
use quickbyte_ui::{demo, icons, theme};

/// The real Core services and the shell are wired; `--demo` still runs on the
/// in-memory fakes without touching the machine.
const REAL_SERVICES: bool = true;

/// The command line, as this binary reads it.
struct Args {
    demo: bool,
    /// `--theme light|dark`: overrides the setting (screenshots).
    theme: Option<bool>,
    /// `--open-details`: opens the first running download's window at start (screenshots).
    open_details: bool,
    /// `--open <dialog>[:<detail>]`: opens a dialog at start (screenshots; see `app::UiState::open_named_dialog`).
    open: Option<String>,
    raw: Vec<String>,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        demo: false,
        theme: None,
        open_details: false,
        open: None,
        raw: raw.clone(),
    };
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--demo" => args.demo = true,
            "--open-details" => args.open_details = true,
            "--open" => args.open = it.next().cloned(),
            "--theme" => {
                args.theme = match it.next().map(String::as_str) {
                    Some("dark") => Some(true),
                    Some("light") => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    args
}

/// `--minimized`, `-minimized`, `/minimized` or `-m`.
fn has_minimized_switch(args: &[String], shell: Shell) -> bool {
    if !shell.is_demo() {
        return quickbyte_shell::single_instance::has_minimized_switch(args);
    }
    args.iter().any(|a| {
        matches!(
            a.trim().trim_matches('"'),
            "--minimized" | "-minimized" | "/minimized" | "-m"
        )
    })
}

/// Builds every Core service in `Program.cs` order. For now: the demo set.
fn build_services(
    demo: bool,
    dispatcher: Arc<dyn Dispatcher>,
    runtime: tokio::runtime::Handle,
) -> AppServices {
    if demo || !REAL_SERVICES {
        return demo::services(dispatcher, runtime);
    }
    use quickbyte_core::helpers::app_user_agent;
    use quickbyte_core::net::app_proxy::AppProxy;
    use quickbyte_core::services::{
        browser_integration_server::BrowserIntegrationServer, download_manager::DownloadManager,
        download_repository::DownloadRepository, file_merger::FileMerger,
        protocol_dispatch::{ProtocolConnectionFactory, ProtocolFileInfoProvider},
        queue_manager::QueueManager, queue_repository::QueueRepository,
        settings_service::SettingsService, update_service::UpdateService,
    };
    use quickbyte_core::streaming::stream_resolver::StreamResolver;
    use quickbyte_core::traits::{self, DownloadManager as _, QueueManager as _, SettingsService as _};

    // Settings first: everything below reads them. Start with Windows is
    // re-asserted at every launch, which keeps the Run value pointing at this
    // executable after an update (MIGRATION.md §6).
    let settings = Arc::new(SettingsService::new(None));
    // Every publisher the UI listens to must wake it; a hub without the
    // dispatcher leaves its events queued until something else repaints.
    settings.events().set_dispatcher(dispatcher.clone());
    settings.load();
    let current = settings.current();
    quickbyte_shell::startup_registration::sync(current.start_with_windows);
    // Proxy and User-Agent apply to every request; Options re-applies them on Save.
    AppProxy::shared().configure(Some(&current.proxy));
    app_user_agent::configure(current.custom_user_agent.as_deref());

    let settings_dyn: Arc<dyn traits::SettingsService> = settings.clone();
    let file_info: Arc<dyn traits::RemoteFileInfoProvider> =
        Arc::new(ProtocolFileInfoProvider::new(None, None));
    let updates: Arc<dyn traits::UpdateService> = Arc::new(UpdateService::new(None, None));
    let streams: Arc<dyn traits::StreamResolver> = Arc::new(StreamResolver::new());

    let downloads = DownloadManager::new(
        Arc::new(DownloadRepository::new(None)),
        settings_dyn.clone(),
        file_info.clone(),
        Arc::new(ProtocolConnectionFactory::new(None, None)),
        Arc::new(FileMerger::new()),
        dispatcher.clone(),
        quickbyte_shell::zone_information_policy::preserves_zone_information(),
        runtime.clone(),
    );
    downloads.load_persisted_downloads();
    let downloads_dyn: Arc<dyn traits::DownloadManager> = downloads.clone();

    let queues = QueueManager::new(
        Arc::new(QueueRepository::new(None)),
        downloads_dyn.clone(),
        dispatcher.clone(),
        runtime.clone(),
    );
    queues.load();
    // The scheduler agent's Run value and the wake task follow the queues; the
    // UI re-syncs both on every queue change.
    quickbyte_shell::queue_agent_registration::sync(queues.has_scheduled_queues());
    quickbyte_shell::queue_wake_registration::sync(&queues.queues());

    // Background sweeps, not awaited: chunk folders no download owns, and
    // installers a finished update left behind.
    {
        let downloads = downloads_dyn.clone();
        runtime.spawn(async move { downloads.cleanup_orphaned_temp_folders().await });
        let updates = updates.clone();
        runtime.spawn(async move { updates.cleanup_downloaded_installers().await });
    }

    // Started from main() once the UI has subscribed, so a capture during
    // startup is not raised into nothing.
    let bridge = BrowserIntegrationServer::new(settings_dyn.clone(), runtime.clone());
    bridge.set_dispatcher(dispatcher.clone());
    let browser: Arc<dyn traits::BrowserIntegrationService> = Arc::new(bridge);

    AppServices {
        downloads: downloads_dyn,
        queues,
        settings: settings_dyn,
        file_info,
        streams,
        updates,
        browser,
        runtime,
    }
}

fn main() -> eframe::Result {
    let args = parse_args();
    let demo = args.demo || !REAL_SERVICES;
    let shell = Shell::new(demo);
    let dispatcher = Arc::new(UiDispatcher::new());

    // Before anything is constructed: if QuickByte is already running, hand this
    // launch's URL (or due queue) to that window and get out. Two processes
    // sharing one downloads.json would overwrite each other's state.
    let single_instance = if demo {
        None
    } else {
        match quickbyte_shell::single_instance::SingleInstance::acquire(dispatcher.clone()) {
            Some(s) => Some(s),
            None => {
                let payload = quickbyte_shell::single_instance::build_handoff_payload(&args.raw);
                quickbyte_shell::single_instance::SingleInstance::send_to_running_instance(
                    &payload,
                );
                return Ok(());
            }
        }
    };

    // One multi-threaded runtime, min(4, CPUs) named workers (MIGRATION.md §4.3).
    let workers = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .min(4);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("quickbyte-worker")
        .enable_all()
        .build()
        .expect("the tokio runtime");

    let services = build_services(demo, dispatcher.clone(), runtime.handle().clone());

    // A launch that goes straight to the notification area: the setting, or
    // whatever started us said so. (Demo mode has no tray to go to.)
    let start_minimized = !demo
        && (services.settings.current().start_minimized || has_minimized_switch(&args.raw, shell));
    let startup_queue = if demo {
        None
    } else {
        quickbyte_shell::single_instance::find_queue_id(&args.raw)
    };
    let startup_url = if demo {
        None
    } else {
        quickbyte_shell::single_instance::find_url(&args.raw)
    };
    let handoffs = single_instance.as_ref().map(|s| s.payloads());

    let viewport = egui::ViewportBuilder::default()
        .with_title(format!("QuickByte Download Manager {}", shell.version()))
        .with_app_id("QuickByte")
        .with_inner_size([1080.0, 620.0])
        .with_min_inner_size([820.0, 460.0])
        .with_icon(Arc::new(icons::window_icon()))
        .with_visible(!start_minimized);

    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        centered: true,
        // Window geometry lives beside the profile; a demo run never touches it.
        persist_window: !demo,
        persistence_path: Some(if demo {
            std::env::temp_dir().join("QuickByte-demo-ui.ron")
        } else {
            quickbyte_core::helpers::app_paths::data().join("ui.ron")
        }),
        ..Default::default()
    };

    let browser = services.browser.clone();
    let command_services = services.clone();
    let stop_commands = tokio_util::sync::CancellationToken::new();
    let stop_commands_at_exit = stop_commands.clone();
    let result = eframe::run_native(
        "QuickByte",
        options,
        Box::new(move |cc| {
            dispatcher.attach(cc.egui_ctx.clone());
            theme::install_fonts(&cc.egui_ctx);
            let startup = StartupOptions {
                start_minimized,
                startup_queue,
                startup_url,
                theme_override: args.theme,
                handoffs,
                open_details_of_first_active: args.open_details,
                open_dialog: args.open.clone(),
            };
            let (state, handle) = UiState::new(
                cc.egui_ctx.clone(),
                services,
                shell,
                dispatcher.clone(),
                startup,
            );
            if !demo {
                serve_command_pipe(&command_services, handle, stop_commands.clone());
            }
            // After the UI subscribed, so a capture during startup is not raised
            // into nothing. A failure to bind is shown in Options; it must not stop the app.
            browser.start();
            Ok(Box::new(QuickByteApp::new(state)))
        }),
    );

    // The single-instance mutex is released last (dropped at the end of main,
    // after the window and the engine).
    stop_commands_at_exit.cancel();

    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// `qb.exe`'s channel (Program.cs: `CommandPipe.ServeAsync`). Commands run on
/// the engine's managers, which are thread-safe; an `add` goes through the main
/// window exactly as Add Download would, without opening a details window.
fn serve_command_pipe(
    services: &AppServices,
    handle: quickbyte_ui::app::AppHandle,
    stopping: tokio_util::sync::CancellationToken,
) {
    use quickbyte_core::commands::processor::{AddFn, CommandProcessor};
    use quickbyte_core::error::CoreError;
    use quickbyte_ui::app::External;

    let handle = Arc::new(handle);
    let add: AddFn = Arc::new(move |request| {
        let handle = Arc::clone(&handle);
        Box::pin(async move {
            let (reply, answer) = crossbeam_channel::bounded(1);
            handle.send(External::AddFromCommandLine { request: Box::new(request), reply });
            tokio::task::spawn_blocking(move || answer.recv())
                .await
                .map_err(|e| CoreError::other(e.to_string()))?
                .map_err(|_| CoreError::other("QuickByte closed before the download was added."))?
                .map_err(CoreError::other)
        })
    });
    let processor = Arc::new(CommandProcessor::new(
        services.downloads.clone(),
        services.queues.clone(),
        services.file_info.clone(),
        Some(services.streams.clone()),
        services.settings.clone(),
        Some(add),
        quickbyte_shell::app_version::display(),
    ));
    let cancel = stopping.clone();
    services.runtime.spawn(async move {
        quickbyte_core::commands::pipe::serve(
            move |request| {
                let processor = Arc::clone(&processor);
                let cancel = cancel.clone();
                async move { processor.execute(request, &cancel).await }
            },
            stopping,
            None,
        )
        .await;
    });
}
