//! Add New Download — port of `Forms/NewDownloadForm.cs`.
//!
//! Takes an http(s) or ftp(s) address, probes it through the
//! `RemoteFileInfoProvider` (name, size, content type, whether ranges work),
//! and lets the user adjust the name, folder, connection count, queue and an
//! optional checksum before handing a `DownloadRequest` to the app. Pre-fills
//! from the clipboard the way a download manager is expected to.
//!
//! * The probe runs on the runtime; the window draws "Fetching file info…"
//!   until it answers, and editing the address cancels it.
//! * A streaming video's playlist (HLS / DASH) grows a **Quality** row, read
//!   through the `StreamResolver`; a live or DRM stream is refused with the
//!   resolver's message as the answer.
//! * There are no login fields. A `user:pass@` address is split so the
//!   password never reaches `DownloadItem.url`; a server that asks for a login
//!   (`CoreError::AuthenticationRequired`) gets the Sign In window, and the
//!   probe is retried with what it collected.
//! * Save to shows a profile folder as `~\…` and follows the file name into
//!   its category folder until the user edits the box.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{
    Align, Color32, ComboBox, Layout, Margin, RichText, TextEdit, Ui, ViewportBuilder,
    ViewportCommand, vec2,
};
use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::byte_formatter::format_bytes;
use quickbyte_core::helpers::{file_checksum, url_credentials, user_path};
use quickbyte_core::models::{
    CapturedDownload, DownloadCredentials, DownloadRequest, Headers, RemoteFileInfo,
    RequestOptions, StreamFormat,
};
use quickbyte_core::streaming::StreamDescription;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::credentials::CredentialsWindow;
use super::{AppCommand, AppWindow, WindowCx, WindowKind};
use crate::actions;
use crate::icons::Icon;
use crate::services::Pending;
use crate::shell::{NativePick, Waker};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{connection_count, form};

const WIDTH: f32 = 548.0;
const BASE_HEIGHT: f32 = 404.0;
const QUALITY_ROW: f32 = form::ROW_HEIGHT;
const LABEL_WIDTH: f32 = 84.0;
const SIDE_BUTTON: f32 = 86.0;

const HINT: &str = "Enter a URL and press Enter or click Fetch Info.";

/// What one fetch learnt, decided on the runtime and drawn on the UI thread.
enum Outcome {
    /// An ordinary file.
    File(RemoteFileInfo),
    /// A playlist, read into its qualities.
    Stream(RemoteFileInfo, StreamDescription),
    /// A live or DRM-protected stream; the message is the answer.
    StreamRefused(String),
    /// The server wants a login.
    Auth(String),
    Failed(String),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Muted,
    Success,
    Warning,
    Danger,
}

impl Tone {
    fn color(self) -> Color32 {
        let p = theme::palette();
        match self {
            Tone::Muted => p.text_muted,
            Tone::Success => p.success,
            Tone::Warning => p.warning,
            Tone::Danger => p.danger,
        }
    }
}

pub struct NewDownloadWindow {
    instance: u64,
    url: String,
    status: String,
    tone: Tone,
    file_name: String,
    save_folder: String,
    /// Set once the user typed in or browsed to a Save to folder; until then
    /// the box follows the file name into its category's folder.
    save_folder_chosen: bool,
    connections: i32,
    connections_enabled: bool,
    /// `None`: start now.
    queue: Option<Uuid>,
    checksum: String,

    info: Option<RemoteFileInfo>,
    stream: Option<StreamDescription>,
    quality: usize,
    type_text: String,
    size_text: String,
    resumable_text: String,
    resumable_tone: Tone,

    /// Cookie, referrer and user agent of a browser capture — often the only
    /// reason the address resolves to a file rather than a sign-in page.
    captured_headers: Option<Headers>,
    /// The name the browser resolved for a capture; it outranks the probe's.
    preferred_file_name: Option<String>,
    /// The login from a `user:pass@` address or the Sign In window.
    credentials: Option<DownloadCredentials>,

    probe: Option<Pending<Outcome>>,
    cancel: Option<CancellationToken>,
    reading_playlist: Arc<AtomicBool>,
    /// The Sign In window's answer, and what the server said.
    awaiting_login: Option<(
        crossbeam_channel::Receiver<Option<DownloadCredentials>>,
        String,
    )>,
    folder_pick: Option<NativePick>,

    /// Fetch on the first frame (a link that arrived from outside).
    fetch_on_open: bool,
    focus_url: bool,
    shown_quality: bool,
    /// The name Save to last followed (C# reacted to every TextChanged).
    followed_name: Option<String>,
}

impl NewDownloadWindow {
    /// `url` (a command line or hand-off) or `captured` (the browser extension)
    /// seed the window and start the fetch at once; with neither, the
    /// clipboard is looked at.
    pub fn new(
        instance: u64,
        url: Option<String>,
        captured: Option<CapturedDownload>,
        vm: &ViewModel,
        clipboard: Option<String>,
    ) -> Self {
        let settings = &vm.settings;
        let mut w = Self {
            instance,
            url: String::new(),
            status: HINT.into(),
            tone: Tone::Muted,
            file_name: String::new(),
            save_folder: user_path::shorten(Some(&settings.default_download_folder)),
            save_folder_chosen: false,
            connections: connection_count::normalize(settings.default_connections_count),
            connections_enabled: true,
            queue: None,
            checksum: String::new(),
            info: None,
            stream: None,
            quality: 0,
            type_text: "—".into(),
            size_text: "—".into(),
            resumable_text: "—".into(),
            resumable_tone: Tone::Muted,
            captured_headers: captured.as_ref().map(CapturedDownload::to_headers),
            preferred_file_name: captured
                .as_ref()
                .and_then(|c| c.file_name.as_deref())
                .filter(|n| !n.trim().is_empty())
                .map(actions::sanitize_file_name),
            credentials: None,
            probe: None,
            cancel: None,
            reading_playlist: Arc::new(AtomicBool::new(false)),
            awaiting_login: None,
            folder_pick: None,
            fetch_on_open: false,
            focus_url: true,
            shown_quality: false,
            followed_name: None,
        };
        let seed = captured
            .map(|c| c.url)
            .or(url)
            .filter(|u| !u.trim().is_empty());
        match seed {
            Some(url) => w.apply_seed(url.trim()),
            None => w.prefill_from_clipboard(clipboard),
        }
        w
    }

    /// A link from outside — the user did not type it, so it is checked
    /// without waiting to be asked.
    fn apply_seed(&mut self, url: &str) {
        // A password embedded in the URL is lifted out of the address: the URL
        // is persisted, displayed and put in tooltips.
        let split = url_credentials::extract(url);
        self.url = split.url;
        if split.credentials.is_some() {
            self.credentials = split.credentials;
        }
        if let Some(name) = &self.preferred_file_name {
            self.file_name = name.clone();
        }
        self.set_status("Checking the link…", Tone::Muted);
        self.fetch_on_open = true;
    }

    fn prefill_from_clipboard(&mut self, clipboard: Option<String>) {
        let Some(text) = clipboard.map(|t| t.trim().to_string()) else {
            return;
        };
        if actions::is_supported_url(&text) {
            self.url = text;
            self.status = "Pasted a link from the clipboard — click Fetch Info to check it.".into();
        }
    }

    fn set_status(&mut self, text: &str, tone: Tone) {
        self.status = text.to_string();
        self.tone = tone;
    }

    fn busy(&self) -> bool {
        self.probe.is_some() || self.awaiting_login.is_some()
    }

    fn request_options(&self) -> RequestOptions {
        RequestOptions {
            credentials: self.credentials.clone(),
            headers: self.captured_headers.clone(),
            ..RequestOptions::default()
        }
    }

    // -------------------------------------------------------------- fetch --

    fn fetch(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        // A URL typed with credentials in it is split here too, not just on the
        // seeded path — people paste ftp://user:pass@host links by hand.
        let split = url_credentials::extract(self.url.trim());
        if split.credentials.is_some() {
            self.url = split.url.clone();
            self.credentials = split.credentials;
        }
        let url = split.url;
        if !actions::is_supported_url(&url) {
            self.set_status(
                "Please enter a valid http, https, ftp or ftps URL.",
                Tone::Danger,
            );
            return;
        }
        self.cancel_probe();
        self.set_status("Fetching file info…", Tone::Muted);

        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let options = self.request_options();
        let file_info = cx.services.file_info.clone();
        let streams = cx.services.streams.clone();
        let reading = self.reading_playlist.clone();
        reading.store(false, Ordering::Relaxed);
        let wake = Waker::new(ui.ctx(), cx.viewport);
        let wake_inner = wake.clone();
        self.probe = Some(cx.services.run_waking(wake, async move {
            let info = match file_info.get_file_info(&url, &options, &cancel).await {
                Ok(info) => info,
                Err(e) => return failure(e),
            };
            if !(actions::is_stream(&info.final_url, Some(&info.content_type))
                || actions::is_stream(&url, None))
            {
                return Outcome::File(info);
            }
            reading.store(true, Ordering::Relaxed);
            wake_inner.wake();
            match streams.describe(&url, &options, &cancel).await {
                Ok(description) => Outcome::Stream(info, description),
                Err(CoreError::StreamNotSupported(message)) => Outcome::StreamRefused(message),
                // Named like a playlist, but not one (C#: FormatException):
                // offered as the file it is.
                // INTEGRATION: confirm the error StreamResolver::describe uses
                // for "not a playlist" once it is ported (assumed `Invalid`).
                Err(CoreError::Invalid(_)) => Outcome::File(info),
                Err(e) => failure(e),
            }
        }));
    }

    fn cancel_probe(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
        self.probe = None;
    }

    fn on_outcome(&mut self, outcome: Outcome, ui: &Ui, cx: &mut WindowCx<'_>) {
        self.cancel = None;
        match outcome {
            Outcome::Cancelled => {}
            Outcome::File(info) => {
                self.show_qualities(None, ui);
                self.file_name = self
                    .preferred_file_name
                    .clone()
                    .unwrap_or_else(|| info.file_name.clone());
                self.type_text = info.content_type.clone();
                self.size_text = if info.has_known_size() {
                    format_bytes(info.content_length)
                } else {
                    "Unknown".into()
                };
                if info.supports_range_requests {
                    self.resumable_text = "Yes — multiple connections will be used".into();
                    self.resumable_tone = Tone::Success;
                } else {
                    self.resumable_text = "No — a single connection will be used".into();
                    self.resumable_tone = Tone::Warning;
                    self.connections = 1;
                }
                self.connections_enabled = info.supports_range_requests;
                self.info = Some(info);
                self.set_status("File info retrieved successfully.", Tone::Success);
            }
            Outcome::Stream(info, description) => {
                self.file_name = self
                    .preferred_file_name
                    .clone()
                    .unwrap_or_else(|| info.file_name.clone());
                // Named before the qualities are shown: choosing one swaps the extension.
                if self.preferred_file_name.is_none() {
                    self.file_name =
                        actions::suggest_stream_file_name(self.url.trim(), &self.file_name, "");
                }
                let format = if description.format == StreamFormat::Hls {
                    "HLS"
                } else {
                    "DASH"
                };
                let separate_audio = description
                    .choices
                    .iter()
                    .any(|c| c.source.has_separate_audio());
                self.resumable_text = "Yes — finished pieces are kept".into();
                self.resumable_tone = Tone::Success;
                self.size_text = "Not known yet".into();
                self.connections_enabled = true;
                self.info = Some(info);
                self.show_qualities(Some(description), ui);
                self.set_status(
                    &if separate_audio {
                        format!("{format} video — its sound is saved as a second file.")
                    } else {
                        format!("{format} video — choose a quality.")
                    },
                    Tone::Success,
                );
            }
            Outcome::StreamRefused(message) => {
                self.show_qualities(None, ui);
                self.info = None;
                self.set_status(&message, Tone::Danger);
            }
            Outcome::Auth(message) => self.on_authentication_required(message, cx),
            Outcome::Failed(message) => {
                self.info = None;
                self.set_status(
                    &format!("Could not retrieve file info: {message}"),
                    Tone::Danger,
                );
            }
        }
    }

    /// The server asked for a login: Sign In comes up with what it said, and an
    /// answer fetches again straight away. A rejected password comes back
    /// through here — that loop is the user's to break, and Cancel breaks it.
    fn on_authentication_required(&mut self, message: String, cx: &mut WindowCx<'_>) {
        self.info = None;
        self.set_status(&message, Tone::Warning);
        let (tx, rx) = crossbeam_channel::bounded(1);
        cx.command(AppCommand::OpenWindow(Box::new(CredentialsWindow::new(
            self.url.trim(),
            &message,
            self.credentials.as_ref(),
            Some(tx),
            Some(cx.viewport),
        ))));
        self.awaiting_login = Some((rx, message));
    }

    /// Shows the quality row with `description`'s choices, or collapses it,
    /// growing or shrinking the window by the row's height.
    fn show_qualities(&mut self, description: Option<StreamDescription>, ui: &Ui) {
        let show = description.as_ref().is_some_and(|d| !d.choices.is_empty());
        self.stream = description;
        self.quality = 0;
        if show != self.shown_quality {
            self.shown_quality = show;
            let height = BASE_HEIGHT + if show { QUALITY_ROW } else { 0.0 };
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::InnerSize(vec2(WIDTH, height)));
        }
        if show {
            self.on_quality_changed();
        }
    }

    /// The file name follows the chosen quality's container — .ts, .mp4, .m4a.
    fn on_quality_changed(&mut self) {
        let Some(stream) = &self.stream else { return };
        let Some(choice) = stream.choices.get(self.quality) else {
            return;
        };
        let mut name = self.file_name.trim().to_string();
        if name.is_empty() {
            name = "video".into();
        }
        self.file_name = format!("{}{}", actions::stem_of(&name), choice.extension);
        let format = if stream.format == StreamFormat::Dash {
            "DASH"
        } else {
            "HLS"
        };
        self.type_text = format!("{format} · {}", choice.content_type);
    }

    /// Points Save to at the file's category folder, unless the user chose one.
    fn follow_category_folder(&mut self, cx: &WindowCx<'_>) {
        if self.followed_name.as_deref() == Some(self.file_name.as_str()) {
            return;
        }
        self.followed_name = Some(self.file_name.clone());
        if self.save_folder_chosen {
            return;
        }
        self.save_folder =
            user_path::shorten(Some(&cx.vm.settings.folder_for(self.file_name.trim())));
    }

    fn on_ok(&mut self, cx: &mut WindowCx<'_>) {
        let Some(fetched) = self.info.clone() else {
            self.set_status("Please fetch file info first.", Tone::Danger);
            return;
        };
        if self.file_name.trim().is_empty() || self.save_folder.trim().is_empty() {
            self.set_status("File name and save folder are required.", Tone::Danger);
            return;
        }
        let checksum = if self.checksum.trim().is_empty() {
            None
        } else {
            match file_checksum::parse(Some(&self.checksum)) {
                Some(c) => Some(c),
                None => {
                    self.set_status(
                        "That checksum is not one QuickByte recognises. Paste the hex value, or leave it empty.",
                        Tone::Danger,
                    );
                    return;
                }
            }
        };
        // A stream's probe described its playlist. What is added is the video,
        // whose size nobody knows until its pieces are down.
        let choice = self
            .stream
            .as_ref()
            .and_then(|s| s.choices.get(self.quality))
            .cloned();
        let info = match &choice {
            None => fetched,
            Some(choice) => RemoteFileInfo {
                file_name: self.file_name.trim().to_string(),
                content_type: choice.content_type.clone(),
                final_url: fetched.final_url.clone(),
                supports_range_requests: true,
                ..RemoteFileInfo::default()
            },
        };
        let mut request = DownloadRequest::new(
            self.url.trim(),
            info,
            user_path::expand(Some(&self.save_folder)),
            self.file_name.trim(),
            self.connections,
        );
        request.credentials = self.credentials.clone();
        request.headers = self.captured_headers.clone();
        request.expected_checksum = checksum;
        request.start_immediately = self.queue.is_none();
        request.stream = choice.map(|c| c.source);
        cx.command(AppCommand::AddDownload {
            request: Box::new(request),
            queue: self.queue,
        });
        cx.close();
    }
}

fn failure(e: CoreError) -> Outcome {
    match e {
        CoreError::AuthenticationRequired { message, .. } => Outcome::Auth(message),
        CoreError::Cancelled => Outcome::Cancelled,
        CoreError::StreamNotSupported(message) => Outcome::StreamRefused(message),
        e => Outcome::Failed(e.to_string()),
    }
}

impl AppWindow for NewDownloadWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::NewDownload(self.instance)
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Add New Download")
            .with_inner_size(vec2(WIDTH, BASE_HEIGHT))
            .with_resizable(false)
            .with_maximize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Add New Download".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if std::mem::take(&mut self.fetch_on_open) {
            self.fetch(ui, cx);
        }
        // Answers that arrived since the last frame.
        if let Some(outcome) = self.probe.as_mut().and_then(Pending::poll) {
            self.probe = None;
            self.on_outcome(outcome, ui, cx);
        } else if self.probe.is_some() && self.reading_playlist.load(Ordering::Relaxed) {
            self.status = "Reading the video's playlist…".into();
        }
        if let Some(answer) = self
            .awaiting_login
            .as_ref()
            .and_then(|(rx, _)| rx.try_recv().ok())
        {
            let (_, message) = self.awaiting_login.take().expect("awaiting");
            match answer {
                Some(credentials) => {
                    self.credentials = Some(credentials);
                    self.fetch(ui, cx);
                }
                // Cancelled: Fetch Info is there for a second try.
                None => self.set_status(
                    &format!("{message} Click Fetch Info to try again."),
                    Tone::Warning,
                ),
            }
        }
        if let Some(answer) = self.folder_pick.as_ref().and_then(NativePick::poll) {
            self.folder_pick = None;
            if let Some(folder) = answer {
                self.save_folder = user_path::shorten(Some(&folder));
                self.save_folder_chosen = true;
            }
        }

        form::header(
            ui,
            "add-header",
            Icon::AddUrl,
            "Add New Download",
            "Paste an HTTP or FTP link, then choose where it lands.",
        );

        let ok_enabled = self.info.is_some() && !self.busy();
        let mut ok = false;
        let mut cancel = false;
        controls::footer(ui, "add-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ok |= controls::button(ui, "Download", ButtonKind::Primary, ok_enabled).clicked();
                cancel |= controls::button(ui, "Cancel", ButtonKind::Secondary, true).clicked();
            });
        });

        let mut fetch = false;
        let mut url_focused = false;
        let waiting_login = self.awaiting_login.is_some();
        form::body(
            ui,
            Margin {
                left: 16,
                right: 16,
                top: 10,
                bottom: 8,
            },
            |ui| {
                if waiting_login {
                    // Nothing to do here until the Sign In question is answered.
                    ui.disable();
                }
                let full = form::field_width(ui, LABEL_WIDTH);

                // --- Address ------------------------------------------------------
                form::row(ui, LABEL_WIDTH, "Address", |ui| {
                    let response = form::text(ui, &mut self.url, full - SIDE_BUTTON - 6.0);
                    if self.focus_url {
                        self.focus_url = false;
                        response.request_focus();
                    }
                    url_focused = response.has_focus();
                    if response.changed() && self.probe.is_some() {
                        // A new address: the answer for the old one is no use.
                        self.cancel_probe();
                        self.set_status(HINT, Tone::Muted);
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        fetch = true;
                    }
                    ui.add_space(6.0 - ui.spacing().item_spacing.x);
                    fetch |= controls::button_sized(
                        ui,
                        "Fetch Info",
                        ButtonKind::Primary,
                        self.probe.is_none(),
                        SIDE_BUTTON,
                    )
                    .clicked();
                });

                // --- Status -------------------------------------------------------
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    form::status_line(ui, &self.status, self.tone.color());
                });
                ui.add_space(4.0);

                // --- Resolved file info card -------------------------------------
                form::card(ui, |ui| {
                    let half = (ui.available_width() - 70.0 - 44.0) / 2.0;
                    ui.horizontal(|ui| {
                        form::info_cell(
                            ui,
                            70.0,
                            half,
                            "Type:",
                            RichText::new(&self.type_text).color(p.text),
                        );
                        form::info_cell(
                            ui,
                            40.0,
                            half,
                            "Size:",
                            RichText::new(&self.size_text).color(p.text),
                        );
                    });
                    ui.horizontal(|ui| {
                        form::info_cell(
                            ui,
                            70.0,
                            half * 2.0,
                            "Resumable:",
                            RichText::new(&self.resumable_text).color(
                                if self.resumable_tone == Tone::Muted {
                                    p.text
                                } else {
                                    self.resumable_tone.color()
                                },
                            ),
                        );
                    });
                });
                ui.add_space(6.0);

                // --- Quality (streaming video only) ------------------------------
                if let Some(stream) = &self.stream
                    && !stream.choices.is_empty()
                {
                    let mut changed = false;
                    form::row(ui, LABEL_WIDTH, "Quality", |ui| {
                        ComboBox::from_id_salt(("quality", self.instance))
                            .width(full)
                            .selected_text(
                                stream
                                    .choices
                                    .get(self.quality)
                                    .map(|c| c.label.as_str())
                                    .unwrap_or(""),
                            )
                            .show_ui(ui, |ui| {
                                for (i, choice) in stream.choices.iter().enumerate() {
                                    changed |= ui
                                        .selectable_value(&mut self.quality, i, &choice.label)
                                        .changed();
                                }
                            });
                    });
                    if changed {
                        self.on_quality_changed();
                    }
                }

                // --- Save as ------------------------------------------------------
                form::row(ui, LABEL_WIDTH, "Save as", |ui| {
                    form::text(ui, &mut self.file_name, full);
                });

                // --- Save to ------------------------------------------------------
                form::row(ui, LABEL_WIDTH, "Save to", |ui| {
                    if form::text(ui, &mut self.save_folder, full - SIDE_BUTTON - 6.0).changed() {
                        self.save_folder_chosen = true;
                    }
                    ui.add_space(6.0 - ui.spacing().item_spacing.x);
                    if controls::button_sized(
                        ui,
                        "Browse…",
                        ButtonKind::Secondary,
                        self.folder_pick.is_none(),
                        SIDE_BUTTON,
                    )
                    .clicked()
                    {
                        // Expanded on the way in: a dialog handed "~\Downloads" opens on nothing.
                        let start = user_path::expand(Some(&self.save_folder));
                        self.folder_pick = Some(
                            cx.shell
                                .pick_folder(&start, Waker::new(ui.ctx(), cx.viewport)),
                        );
                    }
                });

                // --- Connections --------------------------------------------------
                form::row(ui, LABEL_WIDTH, "Connections", |ui| {
                    ui.add_enabled_ui(self.connections_enabled, |ui| {
                        connection_count::show(ui, ("conn", self.instance), &mut self.connections);
                    });
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new("parallel segments")
                            .font(theme::small())
                            .color(p.text_muted),
                    );
                });

                // --- Queue --------------------------------------------------------
                // "Start now" is first and the default: it is what Download has
                // always meant. Choosing a queue is the deliberate act, and it is
                // the one that leaves the file waiting.
                let queues: Vec<(Uuid, String)> = cx
                    .vm
                    .queues
                    .iter()
                    .map(|q| (q.id, q.name.clone()))
                    .collect();
                if self
                    .queue
                    .is_some_and(|id| !queues.iter().any(|(q, _)| *q == id))
                {
                    self.queue = None;
                }
                form::row(ui, LABEL_WIDTH, "Queue", |ui| {
                    let label = |id: Option<Uuid>| match id
                        .and_then(|id| queues.iter().find(|(q, _)| *q == id))
                    {
                        Some((_, name)) => format!("Add to \"{name}\""),
                        None => "Start now — don't queue it".to_string(),
                    };
                    ui.add_enabled_ui(!queues.is_empty(), |ui| {
                        ComboBox::from_id_salt(("queue", self.instance))
                            .width(full)
                            .selected_text(label(self.queue))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.queue, None, label(None));
                                for (id, _) in &queues {
                                    ui.selectable_value(
                                        &mut self.queue,
                                        Some(*id),
                                        label(Some(*id)),
                                    );
                                }
                            });
                    });
                });

                // --- Checksum (optional) -----------------------------------------
                form::row(ui, LABEL_WIDTH, "Checksum", |ui| {
                    form::text_with(
                        ui,
                        TextEdit::singleline(&mut self.checksum)
                            .font(egui::FontId::monospace(theme::BODY))
                            .hint_text(
                                RichText::new(
                                    "Optional — SHA-256, SHA-1 or MD5 from the download page",
                                )
                                .color(p.text_disabled),
                            ),
                        full,
                    );
                });
            },
        );

        self.follow_category_folder(cx);
        if !waiting_login {
            if fetch && self.probe.is_none() {
                self.fetch(ui, cx);
            } else if !url_focused && form::enter_pressed(ui) && ok_enabled {
                ok = true;
            }
            if form::escape_pressed(ui) {
                cancel = true;
            }
        }
        if ok {
            self.on_ok(cx);
        } else if cancel {
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        self.cancel_probe();
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
