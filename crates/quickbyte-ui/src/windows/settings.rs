//! Options — port of `Forms/SettingsForm.cs`.
//!
//! Every setting in the README table, grouped into tabs: Connection, Folders,
//! Interface, System, Proxy and Browser. Save starts from a copy of what is
//! saved and overwrites only the fields this window has a control for, so a
//! setting with no row here keeps its value.
//!
//! Applied on Save rather than at the next download: the global speed limit,
//! the browser bridge (on/off and its port), the proxy and the custom
//! User-Agent (see `app::UiState::apply_live_settings`), and — unlike WinForms —
//! the appearance, since egui restyles every frame. Start with Windows is the
//! one registry write, made after the save and never allowed to fail it.

use std::any::Any;
use std::collections::BTreeMap;

use egui::{Align, ComboBox, Layout, Margin, RichText, TextEdit, Ui, ViewportBuilder, vec2};
use quickbyte_core::enums::AppTheme;
use quickbyte_core::helpers::byte_formatter::BYTES_PER_KILOBYTE;
use quickbyte_core::helpers::clipboard_links;
use quickbyte_core::helpers::file_category::FileCategory;
use quickbyte_core::helpers::user_path;
use quickbyte_core::models::{
    DownloadSettings, ProxyKind, ProxyMode, ProxySettings, is_path_fully_qualified,
};

use super::prompt::{Prompt, PromptIcon};
use super::{AppWindow, WindowCx, WindowKind};
use crate::actions;
use crate::icons::Icon;
use crate::shell::{NativePick, Waker};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{connection_count, flat_tabs, form};

const TABS: [&str; 6] = [
    "Connection",
    "Folders",
    "Interface",
    "System",
    "Proxy",
    "Browser",
];
const CAPTION_WIDTH: f32 = 186.0;
const CONTROL_WIDTH: f32 = 84.0;
const OPTION_ROW: f32 = 42.0;
const BROWSE_WIDTH: f32 = 74.0;

/// Which box a folder dialog fills in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FolderTarget {
    Default,
    Category(FileCategory),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Ok,
    /// The warning after a save that went through: closing follows.
    CloseAfter,
}

pub struct SettingsWindow {
    tab: usize,
    default_connections: i32,
    max_concurrent: i64,
    global_speed_kbps: i64,
    max_retries: i64,
    retry_delay: i64,
    custom_user_agent: String,
    dynamic_segmentation: bool,

    download_folder: String,
    sort_by_category: bool,
    category_folders: BTreeMap<FileCategory, String>,

    appearance: AppTheme,
    progress_interval: i64,
    auto_open_details: bool,
    show_completion: bool,
    monitor_clipboard: bool,
    clipboard_types: String,

    start_with_windows: bool,
    start_minimized: bool,
    keep_awake: bool,

    proxy_mode: ProxyMode,
    proxy_kind: ProxyKind,
    proxy_host: String,
    proxy_port: i64,
    proxy_user: String,
    proxy_password: String,
    proxy_bypass: String,

    browser_enabled: bool,
    browser_port: i64,
    browser_token: String,

    folder_pick: Option<(FolderTarget, NativePick)>,
    prompt: Option<Prompt<Answer>>,
}

impl SettingsWindow {
    pub fn new(vm: &ViewModel, cx_token: String, startup_registered: bool) -> Self {
        let s = &vm.settings;
        let category_folders = FileCategory::ALL
            .into_iter()
            .filter(|c| *c != FileCategory::Other)
            .map(|c| {
                let chosen = s
                    .category_folders
                    .get(&c)
                    .map(|f| user_path::shorten(Some(f)))
                    .unwrap_or_default();
                (c, chosen)
            })
            .collect();
        Self {
            tab: 0,
            default_connections: connection_count::normalize(s.default_connections_count),
            max_concurrent: s.max_concurrent_downloads.clamp(1, 20) as i64,
            global_speed_kbps: (s.global_speed_limit_bytes_per_second / BYTES_PER_KILOBYTE)
                .clamp(0, 1_000_000),
            max_retries: s.max_retries.clamp(0, 20) as i64,
            retry_delay: s.retry_delay_milliseconds.clamp(100, 60_000) as i64,
            // Null is the default, and the box shows the built-in value as its
            // placeholder rather than pre-filling it.
            custom_user_agent: s.custom_user_agent.clone().unwrap_or_default(),
            dynamic_segmentation: s.dynamic_segmentation,
            download_folder: user_path::shorten(Some(&s.default_download_folder)),
            sort_by_category: s.sort_into_category_folders,
            category_folders,
            appearance: s.appearance,
            progress_interval: s.progress_update_interval_milliseconds.clamp(50, 2000) as i64,
            auto_open_details: s.auto_open_details_window,
            show_completion: s.show_completion_window,
            monitor_clipboard: s.monitor_clipboard,
            clipboard_types: s.clipboard_file_types.clone(),
            // Either half is enough to say yes: the Run entry is what Windows
            // acts on, the flag is what the next launch re-asserts.
            start_with_windows: startup_registered || s.start_with_windows,
            start_minimized: s.start_minimized,
            keep_awake: s.keep_awake_while_downloading,
            proxy_mode: s.proxy.mode,
            proxy_kind: s.proxy.kind,
            proxy_host: s.proxy.host.clone(),
            proxy_port: s.proxy.port.clamp(1, 65535) as i64,
            proxy_user: s.proxy.user_name.clone(),
            proxy_password: s.proxy.password.clone(),
            proxy_bypass: s.proxy.bypass.clone(),
            browser_enabled: s.browser_integration_enabled,
            browser_port: s.browser_integration_port.clamp(1024, 65535) as i64,
            // Reading the token is what mints it on a fresh install.
            browser_token: cx_token,
            folder_pick: None,
            prompt: None,
        }
    }

    /// Opens on a given tab (the demo's `--open settings:proxy`).
    pub fn on_tab(mut self, name: &str) -> Self {
        if let Some(i) = TABS.iter().position(|t| t.eq_ignore_ascii_case(name)) {
            self.tab = i;
        }
        self
    }

    fn error(&mut self, text: impl Into<String>) {
        self.prompt = Some(
            Prompt::new("Options", text, PromptIcon::Warning, Answer::Ok).button(
                "OK",
                Answer::Ok,
                ButtonKind::Primary,
            ),
        );
    }

    fn browse(&mut self, target: FolderTarget, current: &str, ui: &Ui, cx: &WindowCx<'_>) {
        // Expanded on the way in — a dialog handed "~\Downloads" opens on nothing.
        let start = user_path::expand(Some(current));
        self.folder_pick = Some((
            target,
            cx.shell
                .pick_folder(&start, Waker::new(ui.ctx(), cx.viewport)),
        ));
    }

    fn on_save(&mut self, cx: &mut WindowCx<'_>) {
        // A copy of what is saved, not a fresh DownloadSettings: a field with no
        // row here (the stream buffer, the token New token already persisted)
        // keeps its value.
        let current = cx.services.settings.current();
        let mut updated: DownloadSettings = (*current).clone();
        updated.default_connections_count = self.default_connections;
        updated.max_retries = self.max_retries as i32;
        updated.retry_delay_milliseconds = self.retry_delay as i32;
        updated.dynamic_segmentation = self.dynamic_segmentation;
        updated.max_concurrent_downloads = self.max_concurrent as i32;
        updated.global_speed_limit_bytes_per_second = self.global_speed_kbps * BYTES_PER_KILOBYTE;
        // Normalized: a blank box persists as None ("use QuickByte's own").
        updated.custom_user_agent = actions::user_agent_normalize(Some(&self.custom_user_agent));
        updated.progress_update_interval_milliseconds = self.progress_interval as i32;
        // Expanded, never the displayed text: a literal "~" reaching
        // create_dir_all makes a folder called "~" beside the executable.
        updated.default_download_folder = user_path::expand(Some(&self.download_folder));
        updated.sort_into_category_folders = self.sort_by_category;
        // An empty row is "use the default subfolder", so only chosen ones are stored.
        updated.category_folders = self
            .category_folders
            .iter()
            .filter(|(_, f)| !f.trim().is_empty())
            .map(|(c, f)| (*c, user_path::expand(Some(f.trim()))))
            .collect();
        updated.auto_open_details_window = self.auto_open_details;
        updated.show_completion_window = self.show_completion;
        updated.appearance = self.appearance;
        updated.monitor_clipboard = self.monitor_clipboard;
        // Normalised on the way in, so what is saved is what is used.
        updated.clipboard_file_types =
            clipboard_links::parse_file_types(Some(&self.clipboard_types))
                .into_iter()
                .collect::<Vec<_>>()
                .join(" ");
        updated.start_with_windows = self.start_with_windows;
        updated.start_minimized = self.start_minimized;
        updated.keep_awake_while_downloading = self.keep_awake;
        updated.browser_integration_enabled = self.browser_enabled;
        updated.browser_integration_port = self.browser_port as i32;
        updated.proxy = ProxySettings {
            mode: self.proxy_mode,
            kind: self.proxy_kind,
            host: self.proxy_host.trim().to_string(),
            port: self.proxy_port as i32,
            user_name: self.proxy_user.trim().to_string(),
            password: self.proxy_password.clone(),
            bypass: self.proxy_bypass.trim().to_string(),
            extra: current.proxy.extra.clone(),
        };

        // Kept, not cleared, when another mode is chosen: switching back should
        // not mean typing the proxy in again.
        if updated.proxy.mode == ProxyMode::Manual && !updated.proxy.is_complete() {
            self.error("Enter the proxy's host name or address and its port, or choose another way to connect.");
            return;
        }
        // Refused rather than dropped: a User-Agent that did not take is
        // indistinguishable from one the server ignored.
        if !actions::user_agent_is_acceptable(&self.custom_user_agent) {
            self.error(format!(
                "The User-Agent has to be a single line of at most {} characters. Leave it blank for QuickByte's own.",
                actions::APP_USER_AGENT_MAX_LENGTH
            ));
            return;
        }
        if let Some((category, _)) = updated
            .category_folders
            .iter()
            .find(|(_, f)| !is_path_fully_qualified(f))
        {
            self.error(format!(
                "The {category} folder has to be a full path, such as D:\\Videos."
            ));
            return;
        }
        if !cx.shell.is_demo()
            && let Err(e) = std::fs::create_dir_all(&updated.default_download_folder)
        {
            self.error(format!("Could not create folder: {e}"));
            return;
        }

        // Saved before the registry write, and the write may not fail the save:
        // everything else here is still valid.
        let start_with_windows = updated.start_with_windows;
        cx.services.settings.save(updated);
        if let Err(e) = cx.shell.apply_startup_registration(start_with_windows) {
            self.prompt = Some(
                Prompt::new(
                    "Options",
                    format!(
                        "Your options were saved, but Windows would not {} QuickByte's startup entry: {e}",
                        if start_with_windows { "add" } else { "remove" }
                    ),
                    PromptIcon::Warning,
                    Answer::CloseAfter,
                )
                .button("OK", Answer::CloseAfter, ButtonKind::Primary),
            );
            return;
        }
        cx.close();
    }

    // ------------------------------------------------------------ the tabs --

    fn connection_tab(&mut self, ui: &mut Ui) {
        option_row(
            ui,
            "Default connections",
            "Segments for each new download; Add Download can override.",
            |ui| {
                connection_count::show_width(
                    ui,
                    "opt-connections",
                    &mut self.default_connections,
                    CONTROL_WIDTH,
                );
            },
        );
        option_row(
            ui,
            "Max concurrent downloads",
            "How many run at once; the rest queue. Applies after restart.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-concurrent",
                    &mut self.max_concurrent,
                    1,
                    20,
                    1,
                    CONTROL_WIDTH,
                );
            },
        );
        option_row(
            ui,
            "Global speed limit (KB/s)",
            "Shared by all downloads; 0 = no limit. Applies at once.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-speed",
                    &mut self.global_speed_kbps,
                    0,
                    1_000_000,
                    50,
                    CONTROL_WIDTH,
                );
            },
        );
        option_row(
            ui,
            "Max retries per connection",
            "Attempts before a connection is marked failed.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-retries",
                    &mut self.max_retries,
                    0,
                    20,
                    1,
                    CONTROL_WIDTH,
                );
            },
        );
        option_row(
            ui,
            "Retry base delay (ms)",
            "Grows with exponential backoff on each successive retry.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-delay",
                    &mut self.retry_delay,
                    100,
                    60_000,
                    100,
                    CONTROL_WIDTH,
                );
            },
        );
        text_row(
            ui,
            "Custom User-Agent",
            "Blank is QuickByte's own.",
            |ui, width| {
                form::text_hint(ui, &mut self.custom_user_agent, width, actions::APP_USER_AGENT_DEFAULT).on_hover_text(
                "Sent with every request QuickByte makes. Set it to a browser's User-Agent for a server \
                 that refuses downloaders. A link captured by the browser extension keeps the browser's \
                 own value instead, since that is the one its cookies were issued to.",
            );
            },
        );
        check_row(ui, &mut self.dynamic_segmentation, "Split the slowest segment when a connection finishes early")
            .on_hover_text("Keeps every connection busy until the end. Turn off for a server that refuses new connections mid-download.");
    }

    fn folders_tab(&mut self, ui: &mut Ui, cx: &WindowCx<'_>) {
        let mut browse = None;
        text_row(
            ui,
            "Default download folder",
            "Where finished files are saved.",
            |ui, width| {
                form::text(ui, &mut self.download_folder, width - BROWSE_WIDTH - 6.0);
                ui.add_space(6.0 - ui.spacing().item_spacing.x);
                if controls::button_sized(
                    ui,
                    "Browse…",
                    ButtonKind::Secondary,
                    self.folder_pick.is_none(),
                    BROWSE_WIDTH,
                )
                .clicked()
                {
                    browse = Some(FolderTarget::Default);
                }
            },
        );
        ui.add_space(4.0);
        ui.checkbox(
            &mut self.sort_by_category,
            "Sort downloads into a folder for each kind of file",
        );
        ui.add_space(4.0);
        // Greyed while sorting is off; each empty row shows the subfolder it
        // would get, which moves when the default folder does.
        let preview = DownloadSettings {
            default_download_folder: user_path::expand(Some(&self.download_folder)),
            ..DownloadSettings::default()
        };
        let enabled = self.sort_by_category;
        let p = theme::palette();
        let width = form::field_width(ui, 104.0) - BROWSE_WIDTH - 6.0;
        for (category, field) in self.category_folders.iter_mut() {
            form::row(ui, 104.0, category.name(), |ui| {
                ui.add_enabled_ui(enabled, |ui| {
                    let hint =
                        user_path::shorten(Some(&preview.default_category_folder(*category)));
                    form::text_with(
                        ui,
                        TextEdit::singleline(field)
                            .hint_text(RichText::new(hint).color(p.text_disabled)),
                        width,
                    );
                    ui.add_space(6.0 - ui.spacing().item_spacing.x);
                    if controls::button_sized(
                        ui,
                        "Browse…",
                        ButtonKind::Secondary,
                        enabled,
                        BROWSE_WIDTH,
                    )
                    .clicked()
                    {
                        browse = Some(FolderTarget::Category(*category));
                    }
                });
            });
        }
        ui.add_space(6.0);
        form::note(
            ui,
            "Empty rows use a subfolder of the default folder; other files stay in it.",
        );
        if let Some(target) = browse {
            let current = match target {
                FolderTarget::Default => self.download_folder.clone(),
                FolderTarget::Category(c) => {
                    let chosen = self.category_folders.get(&c).cloned().unwrap_or_default();
                    if chosen.trim().is_empty() {
                        preview.default_category_folder(c)
                    } else {
                        chosen
                    }
                }
            };
            self.browse(target, &current, ui, cx);
        }
    }

    fn interface_tab(&mut self, ui: &mut Ui) {
        option_row(
            ui,
            "Appearance",
            "Light, dark, or as Windows is set. Applies as soon as you save.",
            |ui| {
                let label = |t: AppTheme| match t {
                    AppTheme::System => "As Windows",
                    AppTheme::Light => "Light",
                    AppTheme::Dark => "Dark",
                };
                ComboBox::from_id_salt("opt-appearance")
                    .width(CONTROL_WIDTH + 12.0)
                    .selected_text(label(self.appearance))
                    .show_ui(ui, |ui| {
                        for t in [AppTheme::System, AppTheme::Light, AppTheme::Dark] {
                            ui.selectable_value(&mut self.appearance, t, label(t));
                        }
                    });
            },
        );
        option_row(
            ui,
            "Progress sampling (ms)",
            "Windows interpolate between samples — lower is smoother.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-progress",
                    &mut self.progress_interval,
                    50,
                    2000,
                    50,
                    CONTROL_WIDTH,
                );
            },
        );
        check_row(
            ui,
            &mut self.auto_open_details,
            "Open the details window automatically when a download starts",
        );
        check_row(
            ui,
            &mut self.show_completion,
            "Show the download complete window when a download finishes",
        );
        check_row(
            ui,
            &mut self.monitor_clipboard,
            "Watch the clipboard and offer to download copied links to these file types:",
        );
        let enabled = self.monitor_clipboard;
        text_row(ui, "File types", "Separated by spaces.", |ui, width| {
            ui.add_enabled_ui(enabled, |ui| {
                form::text(ui, &mut self.clipboard_types, width)
            });
        });
    }

    fn system_tab(&mut self, ui: &mut Ui) {
        check_row(
            ui,
            &mut self.start_with_windows,
            "Start QuickByte when Windows starts",
        );
        note_row(
            ui,
            "Adds QuickByte to this account's startup programs. No administrator rights needed, \
             and Task Manager's Startup tab can turn it off again.",
        );
        check_row(
            ui,
            &mut self.start_minimized,
            "Start minimized to the notification area",
        );
        note_row(
            ui,
            "Applies to every launch, not only the one Windows performs at sign-in. \
             Downloads still resume; double-click the tray icon to open the window.",
        );
        check_row(
            ui,
            &mut self.keep_awake,
            "Keep the computer awake while downloads are running",
        );
        note_row(
            ui,
            "Stops Windows sleeping when idle until the last download stops. \
             Closing the lid or choosing Sleep still sleeps.",
        );
    }

    fn proxy_tab(&mut self, ui: &mut Ui) {
        let mode_label = |m: ProxyMode| match m {
            ProxyMode::None => "Directly, without a proxy",
            ProxyMode::Manual => "Through this proxy",
            ProxyMode::System => "Using Windows' proxy settings",
        };
        text_row(ui, "Connect", "Route to the internet.", |ui, width| {
            ComboBox::from_id_salt("opt-proxy-mode")
                .width(width)
                .selected_text(mode_label(self.proxy_mode))
                .show_ui(ui, |ui| {
                    for m in [ProxyMode::System, ProxyMode::None, ProxyMode::Manual] {
                        ui.selectable_value(&mut self.proxy_mode, m, mode_label(m));
                    }
                });
        });
        // The server fields mean something only for a proxy named here.
        let manual = self.proxy_mode == ProxyMode::Manual;
        let kind_label = |k: ProxyKind| match k {
            ProxyKind::Socks5 => "SOCKS5",
            ProxyKind::Socks4 => "SOCKS4",
            ProxyKind::Http => "HTTP",
        };
        let p = theme::palette();
        text_row(ui, "Server", "Type, host and port.", |ui, width| {
            ui.add_enabled_ui(manual, |ui| {
                ComboBox::from_id_salt("opt-proxy-kind")
                    .width(72.0)
                    .selected_text(kind_label(self.proxy_kind))
                    .show_ui(ui, |ui| {
                        for k in [ProxyKind::Http, ProxyKind::Socks5, ProxyKind::Socks4] {
                            ui.selectable_value(&mut self.proxy_kind, k, kind_label(k));
                        }
                    });
                form::text_with(
                    ui,
                    TextEdit::singleline(&mut self.proxy_host)
                        .hint_text(RichText::new("proxy.example.com").color(p.text_disabled)),
                    width - 72.0 - CONTROL_WIDTH - 2.0 * ui.spacing().item_spacing.x,
                );
                form::spinner(
                    ui,
                    "opt-proxy-port",
                    &mut self.proxy_port,
                    1,
                    65535,
                    1,
                    CONTROL_WIDTH,
                );
            });
        });
        text_row(
            ui,
            "User name",
            "If the proxy asks for one.",
            |ui, width| {
                ui.add_enabled_ui(manual, |ui| form::text(ui, &mut self.proxy_user, width));
            },
        );
        text_row(ui, "Password", "Stored encrypted.", |ui, width| {
            ui.add_enabled_ui(manual, |ui| {
                form::text_with(
                    ui,
                    TextEdit::singleline(&mut self.proxy_password).password(true),
                    width,
                )
            });
        });
        text_row(ui, "Not for", "Hosts it skips, e.g. *.lan", |ui, width| {
            ui.add_enabled_ui(manual, |ui| form::text(ui, &mut self.proxy_bypass, width));
        });
        ui.add_space(4.0);
        form::note(
            ui,
            "A proxy sees every download's address, and HTTP and SOCKS send its password unencrypted. \
             FTP uses only a proxy set here, never Windows' settings.",
        );
    }

    fn browser_tab(&mut self, ui: &mut Ui, cx: &WindowCx<'_>) {
        check_row(
            ui,
            &mut self.browser_enabled,
            "Let the browser extension send downloads to QuickByte",
        );
        option_row(
            ui,
            "Bridge port (127.0.0.1)",
            "Must match the port set in the extension. Applies at once.",
            |ui| {
                form::spinner(
                    ui,
                    "opt-bridge-port",
                    &mut self.browser_port,
                    1024,
                    65535,
                    1,
                    CONTROL_WIDTH,
                );
            },
        );
        let mut copy = false;
        text_row(
            ui,
            "Pairing token",
            "Paste into the extension.",
            |ui, width| {
                form::read_only(ui, &self.browser_token, width - BROWSE_WIDTH - 6.0, true);
                ui.add_space(6.0 - ui.spacing().item_spacing.x);
                copy = controls::button_sized(
                    ui,
                    "Copy",
                    ButtonKind::Secondary,
                    !self.browser_token.is_empty(),
                    BROWSE_WIDTH,
                )
                .clicked();
            },
        );
        if copy {
            ui.ctx().copy_text(self.browser_token.clone());
        }
        // A live status line with the New token button on the right.
        let p = theme::palette();
        let browser = &cx.services.browser;
        let (status, color) = if !self.browser_enabled {
            (
                "Bridge off - the extension will report QuickByte as unavailable.".to_string(),
                p.text_muted,
            )
        } else if browser.is_running() {
            (
                format!("Listening on 127.0.0.1:{}.", browser.port()),
                p.success,
            )
        } else {
            // Nearly always the port being taken; the fix is the number above.
            match browser.last_error() {
                None => ("Not listening yet.".to_string(), p.danger),
                Some(e) => (format!("Could not listen: {e}"), p.danger),
            }
        };
        let mut regenerate = false;
        ui.allocate_ui_with_layout(
            vec2(ui.available_width(), OPTION_ROW),
            Layout::left_to_right(Align::Center),
            |ui| {
                let width = ui.available_width() - 90.0;
                ui.allocate_ui_with_layout(
                    vec2(width, 20.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        ui.set_width(width);
                        form::status_line(ui, &status, color);
                    },
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    regenerate = controls::button(ui, "New token", ButtonKind::Secondary, true)
                        .on_hover_text(
                            "Issues a new secret and unpairs every browser using the old one.",
                        )
                        .clicked();
                });
            },
        );
        if regenerate {
            self.browser_token = cx.services.browser.regenerate_token();
        }
    }
}

/// A caption block on the left and a control right-aligned in the last column
/// (`AddNumericRow` / `AddConnectionsRow`).
fn option_row(ui: &mut Ui, caption: &str, hint: &str, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), OPTION_ROW),
        Layout::left_to_right(Align::Center),
        |ui| {
            let width = ui.available_width() - CONTROL_WIDTH - 24.0;
            form::caption_block(ui, width, caption, hint);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.allocate_ui_with_layout(
                    vec2(CONTROL_WIDTH, 22.0),
                    Layout::left_to_right(Align::Center),
                    add,
                );
            });
        },
    );
}

/// A caption block in the first column, and a field filling the rest
/// (`AddTextRow`). `add` gets the width it may use.
fn text_row(ui: &mut Ui, caption: &str, hint: &str, add: impl FnOnce(&mut Ui, f32)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), OPTION_ROW),
        Layout::left_to_right(Align::Center),
        |ui| {
            form::caption_block(ui, CAPTION_WIDTH, caption, hint);
            let width = ui.available_width();
            add(ui, width);
        },
    );
}

fn check_row(ui: &mut Ui, value: &mut bool, caption: &str) -> egui::Response {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 26.0),
        Layout::left_to_right(Align::Center),
        |ui| ui.checkbox(value, caption),
    )
    .inner
}

/// A muted paragraph under a checkbox, indented to its text.
fn note_row(ui: &mut Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.add_space(20.0);
        ui.vertical(|ui| {
            form::note(ui, text);
        });
    });
    ui.add_space(8.0);
}

impl AppWindow for SettingsWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Settings
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("QuickByte Options")
            .with_inner_size(vec2(560.0, 446.0))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "QuickByte Options".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if let Some((target, pick)) = &self.folder_pick
            && let Some(answer) = pick.poll()
        {
            let target = *target;
            self.folder_pick = None;
            if let Some(folder) = answer {
                let folder = user_path::shorten(Some(&folder));
                match target {
                    FolderTarget::Default => self.download_folder = folder,
                    FolderTarget::Category(c) => {
                        self.category_folders.insert(c, folder);
                    }
                }
            }
        }

        form::header(
            ui,
            "opt-header",
            Icon::Settings,
            "Options",
            "Defaults for new downloads and how the app behaves.",
        );
        let mut save = false;
        let mut cancel = false;
        controls::footer(ui, "opt-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                save |= controls::button(ui, "Save", ButtonKind::Primary, true).clicked();
                cancel |= controls::button(ui, "Cancel", ButtonKind::Secondary, true).clicked();
            });
        });
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(p.surface))
            .show(ui, |ui| {
                flat_tabs::show(ui, &TABS, &mut self.tab, 28.0);
                egui::Frame::NONE
                    .inner_margin(Margin {
                        left: 18,
                        right: 18,
                        top: 10,
                        bottom: 8,
                    })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        match self.tab {
                            0 => self.connection_tab(ui),
                            1 => self.folders_tab(ui, cx),
                            2 => self.interface_tab(ui),
                            3 => self.system_tab(ui),
                            4 => self.proxy_tab(ui),
                            _ => self.browser_tab(ui, cx),
                        }
                    });
            });

        if let Some(prompt) = &self.prompt {
            if let Some(answer) = prompt.show(ui) {
                self.prompt = None;
                if answer == Answer::CloseAfter {
                    cx.close();
                }
            }
            return;
        }
        if ui.memory(|m| m.focused().is_none()) && form::enter_pressed(ui) {
            save = true;
        }
        if form::escape_pressed(ui) {
            cancel = true;
        }
        if save {
            self.on_save(cx);
        } else if cancel {
            cx.close();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
