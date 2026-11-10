//! Batch Download — port of `Forms/BatchDownloadForm.cs`.
//!
//! Adds many downloads at once: a pasted list of links, a web page's source, a
//! text file of links, or a numbered pattern such as
//! `https://example.com/photo[001-250].jpg` (`helpers::batch_links`).
//!
//! Every link is probed before anything is added, six at a time, for the same
//! reason Add Download probes one: the name a download is saved under is
//! settled when it is added. The list shows what each link turned out to be;
//! one that failed — or is not wanted — is left unticked.
//!
//! The batch goes into a new queue by default, an existing one, or starts at
//! once — and opens no details window per download: two hundred of them
//! opening at once is not a batch, it is a denial of service on the desktop.

use std::any::Any;
use std::sync::Arc;

use egui::{Align, ComboBox, Layout, Margin, RichText, TextEdit, Ui, ViewportBuilder, vec2};
use egui_extras::{Column, TableBuilder};
use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::byte_formatter::format_bytes;
use quickbyte_core::helpers::{batch_links, url_credentials, user_path};
use quickbyte_core::models::{
    DownloadCredentials, DownloadRequest, RemoteFileInfo, RequestOptions,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{AppCommand, AppWindow, WindowCx, WindowKind};
use crate::actions;
use crate::icons::Icon;
use crate::shell::{NativePick, Waker};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{connection_count, form};

/// How many links are probed at once: quick for a long list, polite to one server.
const PARALLEL_PROBES: usize = 6;
const LABEL_WIDTH: f32 = 84.0;
const SIDE_BUTTON: f32 = 104.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Muted,
    Text,
    Success,
    Warning,
    Danger,
}

impl Tone {
    fn color(self) -> egui::Color32 {
        let p = theme::palette();
        match self {
            Tone::Muted => p.text_muted,
            Tone::Text => p.text,
            Tone::Success => p.success,
            Tone::Warning => p.warning,
            Tone::Danger => p.danger,
        }
    }
}

/// One pasted link: where it points, the login lifted out of it, and what the
/// probe found.
struct Entry {
    url: String,
    credentials: Option<DownloadCredentials>,
    name: String,
    size: String,
    status: String,
    tone: Tone,
    info: Option<RemoteFileInfo>,
    checked: bool,
}

enum Report {
    Checking(usize),
    Done(usize, Result<RemoteFileInfo, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    NewQueue,
    StartNow,
    Existing(Uuid),
}

pub struct BatchWindow {
    links: String,
    status: String,
    tone: Tone,
    entries: Vec<Entry>,
    checking: bool,
    done: usize,
    reports: Option<crossbeam_channel::Receiver<Report>>,
    closing: CancellationToken,
    save_folder: String,
    /// Set once the user edits Save to; until then each file goes to its
    /// category's folder.
    save_folder_chosen: bool,
    connections: i32,
    target: Target,
    new_queue_name: String,
    folder_pick: Option<NativePick>,
    import_pick: Option<NativePick>,
    check_on_next_frame: bool,
}

impl BatchWindow {
    pub fn new(vm: &ViewModel) -> Self {
        Self {
            links: String::new(),
            status: "One link per line, or anything with links in it.".into(),
            tone: Tone::Muted,
            entries: Vec::new(),
            checking: false,
            done: 0,
            reports: None,
            closing: CancellationToken::new(),
            save_folder: user_path::shorten(Some(&vm.settings.default_download_folder)),
            save_folder_chosen: false,
            connections: connection_count::normalize(vm.settings.default_connections_count),
            target: Target::NewQueue,
            new_queue_name: default_queue_name(),
            folder_pick: None,
            import_pick: None,
            check_on_next_frame: false,
        }
    }

    /// Pre-filled (the demo's `--open batch`).
    pub fn with_links(mut self, links: &str) -> Self {
        self.links = links.to_string();
        self.check_on_next_frame = true;
        self
    }

    fn set_status(&mut self, text: impl Into<String>, tone: Tone) {
        self.status = text.into();
        self.tone = tone;
    }

    /// Extracts the links and probes every one, six at a time, filling the
    /// list in as the answers come back.
    fn check(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        if self.checking {
            return;
        }
        let links = match batch_links::extract_links(&self.links) {
            Ok(links) => links,
            Err(e) => {
                self.set_status(e.to_string(), Tone::Danger);
                return;
            }
        };
        if links.is_empty() {
            self.set_status("No http, https, ftp or ftps links found.", Tone::Danger);
            return;
        }
        self.checking = true;
        self.done = 0;
        self.entries = links
            .iter()
            .map(|link| {
                // The address with any login taken out of it — the column is on
                // screen, and the request carries the login instead.
                let split = url_credentials::extract(link);
                Entry {
                    name: actions::file_name_from_url(&split.url),
                    url: split.url,
                    credentials: split.credentials,
                    size: String::new(),
                    status: "Waiting…".into(),
                    tone: Tone::Muted,
                    info: None,
                    checked: false,
                }
            })
            .collect();
        let n = links.len();
        self.set_status(
            format!("Checking {n} link{}…", if n == 1 { "" } else { "s" }),
            Tone::Muted,
        );

        let (tx, rx) = crossbeam_channel::unbounded();
        self.reports = Some(rx);
        let gate = Arc::new(tokio::sync::Semaphore::new(PARALLEL_PROBES));
        let wake = Waker::new(ui.ctx(), cx.viewport);
        for (index, entry) in self.entries.iter().enumerate() {
            let (gate, tx, wake) = (gate.clone(), tx.clone(), wake.clone());
            let file_info = cx.services.file_info.clone();
            let cancel = self.closing.clone();
            let url = entry.url.clone();
            let options = RequestOptions {
                credentials: entry.credentials.clone(),
                ..RequestOptions::default()
            };
            cx.services.spawn(async move {
                let Ok(_permit) = gate.acquire().await else {
                    return;
                };
                if cancel.is_cancelled() {
                    return;
                }
                let _ = tx.send(Report::Checking(index));
                wake.wake();
                let result = file_info.get_file_info(&url, &options, &cancel).await;
                if cancel.is_cancelled() {
                    return;
                }
                let result = result.map_err(|e| match e {
                    CoreError::AuthenticationRequired { .. } => "Needs a login".to_string(),
                    e => e.to_string(),
                });
                let _ = tx.send(Report::Done(index, result));
                wake.wake();
            });
        }
    }

    fn drain_reports(&mut self) {
        let Some(rx) = &self.reports else { return };
        let reports: Vec<Report> = rx.try_iter().collect();
        for report in reports {
            match report {
                Report::Checking(i) => {
                    if let Some(e) = self.entries.get_mut(i) {
                        e.status = "Checking…".into();
                    }
                }
                Report::Done(i, result) => {
                    if let Some(e) = self.entries.get_mut(i) {
                        match result {
                            Ok(info) => {
                                e.name = info.file_name.clone();
                                e.size = if info.has_known_size() {
                                    format_bytes(info.content_length)
                                } else {
                                    "Unknown".into()
                                };
                                e.status = if info.supports_range_requests {
                                    "Ready".into()
                                } else {
                                    "Ready (one connection)".into()
                                };
                                e.tone = Tone::Text;
                                e.checked = true;
                                e.info = Some(info);
                            }
                            Err(message) => {
                                e.status = message;
                                e.tone = Tone::Danger;
                            }
                        }
                    }
                    self.done += 1;
                    let total = self.entries.len();
                    self.set_status(format!("Checked {} of {total}…", self.done), Tone::Muted);
                }
            }
        }
        if self.checking && self.done >= self.entries.len() {
            self.checking = false;
            self.reports = None;
            let ready = self.entries.iter().filter(|e| e.info.is_some()).count();
            let failed = self.entries.len() - ready;
            if failed == 0 {
                self.set_status(
                    format!(
                        "All {ready} link{} ready.",
                        if ready == 1 { " is" } else { "s are" }
                    ),
                    Tone::Success,
                );
            } else {
                self.set_status(
                    format!("{ready} ready, {failed} could not be checked and will be left out."),
                    Tone::Warning,
                );
            }
        }
    }

    fn checked_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.checked && e.info.is_some())
            .count()
    }

    fn on_add(&mut self, cx: &mut WindowCx<'_>) {
        if self.save_folder.trim().is_empty() {
            self.set_status("Choose a folder to save to.", Tone::Danger);
            return;
        }
        let folder = user_path::expand(Some(&self.save_folder));
        let start_now = self.target == Target::StartNow;
        let settings = cx.vm.settings.clone();
        let requests: Vec<DownloadRequest> = self
            .entries
            .iter()
            .filter(|e| e.checked)
            .filter_map(|e| {
                let info = e.info.clone()?;
                let mut request = DownloadRequest::new(
                    e.url.clone(),
                    info.clone(),
                    if self.save_folder_chosen {
                        folder.clone()
                    } else {
                        settings.folder_for(&info.file_name)
                    },
                    info.file_name.clone(),
                    if info.supports_range_requests {
                        self.connections
                    } else {
                        1
                    },
                );
                request.credentials = e.credentials.clone();
                request.start_immediately = start_now;
                Some(request)
            })
            .collect();
        cx.command(AppCommand::AddBatch {
            requests,
            queue: match self.target {
                Target::Existing(id) => Some(id),
                _ => None,
            },
            new_queue_name: (self.target == Target::NewQueue).then(|| self.new_queue_name.clone()),
        });
        cx.close();
    }

    fn results(&mut self, ui: &mut Ui, height: f32) {
        let p = theme::palette();
        form::list_frame(ui, |ui| {
            ui.set_height(height);
            form::list_visuals(ui);
            let mut toggled = None;
            TableBuilder::new(ui)
                .id_salt("batch-results")
                .striped(true)
                .resizable(true)
                .auto_shrink([false, false])
                .max_scroll_height(height - 22.0)
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(24.0))
                .column(Column::initial(210.0).at_least(60.0).clip(true))
                .column(Column::initial(80.0).at_least(40.0).clip(true))
                .column(Column::initial(140.0).at_least(60.0).clip(true))
                .column(Column::remainder().at_least(80.0).clip(true))
                .header(22.0, |mut header| {
                    header.col(|ui| form::header_cell(ui, ""));
                    for title in ["File name", "Size", "Status", "Address"] {
                        header.col(|ui| form::header_cell(ui, title));
                    }
                })
                .body(|body| {
                    body.rows(theme::ROW_HEIGHT, self.entries.len(), |mut row| {
                        let i = row.index();
                        let e = &self.entries[i];
                        let color = e.tone.color();
                        row.col(|ui| {
                            ui.add_space(5.0);
                            // A link that could not be resolved has nothing to add.
                            let mut checked = e.checked;
                            let enabled = e.info.is_some();
                            if ui
                                .add_enabled(enabled, egui::Checkbox::without_text(&mut checked))
                                .changed()
                            {
                                toggled = Some((i, checked));
                            }
                        });
                        let cell = |ui: &mut Ui, text: &str| {
                            ui.add_space(6.0);
                            ui.add(egui::Label::new(RichText::new(text).color(color)).truncate());
                        };
                        row.col(|ui| cell(ui, &e.name));
                        row.col(|ui| cell(ui, &e.size));
                        row.col(|ui| cell(ui, &e.status));
                        row.col(|ui| {
                            ui.add_space(6.0);
                            ui.add(
                                egui::Label::new(RichText::new(&e.url).color(
                                    if e.tone == Tone::Text {
                                        p.text_muted
                                    } else {
                                        color
                                    },
                                ))
                                .truncate(),
                            );
                        });
                    });
                });
            if let Some((i, checked)) = toggled {
                self.entries[i].checked = checked;
            }
        });
    }
}

fn default_queue_name() -> String {
    chrono::Local::now()
        .format("Batch %-d %b %H:%M")
        .to_string()
}

impl AppWindow for BatchWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Batch
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Batch Download")
            .with_inner_size(vec2(720.0, 560.0))
            .with_min_inner_size(vec2(600.0, 460.0))
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Batch Download".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        self.drain_reports();
        if let Some(answer) = self.folder_pick.as_ref().and_then(NativePick::poll) {
            self.folder_pick = None;
            if let Some(folder) = answer {
                self.save_folder = user_path::shorten(Some(&folder));
                self.save_folder_chosen = true;
            }
        }
        if let Some(answer) = self.import_pick.as_ref().and_then(NativePick::poll) {
            self.import_pick = None;
            if let Some(path) = answer {
                match batch_links::read_import_file(std::path::Path::new(&path)) {
                    Ok(text) => {
                        self.links = text;
                        self.check_on_next_frame = true;
                    }
                    Err(e) => {
                        self.set_status(format!("Could not import that file: {e}"), Tone::Danger)
                    }
                }
            }
        }
        if std::mem::take(&mut self.check_on_next_frame) {
            self.check(ui, cx);
        }

        form::header(
            ui,
            "batch-header",
            Icon::BatchDownload,
            "Batch Download",
            "Paste links or a page's source, import a file, or use a pattern like photo[001-250].jpg.",
        );

        let count = self.checked_count();
        let mut add = false;
        let mut cancel = false;
        controls::footer(ui, "batch-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let text = if count == 1 {
                    "Add 1 Download".to_string()
                } else {
                    format!("Add {count} Downloads")
                };
                add |=
                    controls::button(ui, &text, ButtonKind::Primary, !self.checking && count > 0)
                        .clicked();
                cancel |= controls::button(ui, "Cancel", ButtonKind::Secondary, true).clicked();
            });
        });

        let mut check = false;
        form::body(
            ui,
            Margin {
                left: 16,
                right: 16,
                top: 10,
                bottom: 8,
            },
            |ui| {
                let full = form::field_width(ui, LABEL_WIDTH);

                // --- Links + the two buttons beside them --------------------------
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), 72.0),
                    Layout::left_to_right(Align::Min),
                    |ui| {
                        ui.allocate_ui_with_layout(
                            vec2(LABEL_WIDTH, 72.0),
                            Layout::top_down(Align::Min),
                            |ui| {
                                ui.set_width(LABEL_WIDTH);
                                ui.add_space(3.0);
                                ui.label(
                                    RichText::new("Links")
                                        .font(theme::small())
                                        .color(p.text_muted),
                                );
                            },
                        );
                        let text_width = full - SIDE_BUTTON - 6.0;
                        egui::ScrollArea::vertical()
                            .id_salt("batch-links")
                            .max_width(text_width)
                            .max_height(72.0)
                            .show(ui, |ui| {
                                ui.add_sized(
                                    vec2(text_width, 72.0),
                                    TextEdit::multiline(&mut self.links)
                                        .desired_rows(4)
                                        .char_limit(batch_links::MAX_IMPORT_BYTES as usize)
                                        .margin(Margin::symmetric(5, 3)),
                                );
                            });
                        ui.add_space(6.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 6.0;
                            check |=
                                wide_button(ui, "Check Links", ButtonKind::Primary, !self.checking);
                            if wide_button(
                                ui,
                                "Import File…",
                                ButtonKind::Secondary,
                                self.import_pick.is_none(),
                            ) {
                                self.import_pick = Some(
                                    cx.shell.pick_import_file(Waker::new(ui.ctx(), cx.viewport)),
                                );
                            }
                        });
                    },
                );

                // --- Status ---------------------------------------------------------
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    form::status_line(ui, &self.status, self.tone.color());
                });
                ui.add_space(4.0);

                // --- Results (fills) ------------------------------------------------
                let bottom = 3.0 * (form::ROW_HEIGHT + ui.spacing().item_spacing.y) + 10.0;
                let height = (ui.available_height() - bottom).max(80.0);
                self.results(ui, height);
                ui.add_space(6.0);

                // --- Save to --------------------------------------------------------
                let sorting = cx.vm.settings.sort_into_category_folders;
                form::row(ui, LABEL_WIDTH, "Save to", |ui| {
                    let response = form::text(ui, &mut self.save_folder, full - SIDE_BUTTON - 6.0);
                    if response.changed() {
                        self.save_folder_chosen = true;
                    }
                    if sorting {
                        response.on_hover_text(
                            "Each file goes to its category's folder unless you change this.",
                        );
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
                        let start = user_path::expand(Some(&self.save_folder));
                        self.folder_pick = Some(
                            cx.shell
                                .pick_folder(&start, Waker::new(ui.ctx(), cx.viewport)),
                        );
                    }
                });

                // --- Connections ----------------------------------------------------
                form::row(ui, LABEL_WIDTH, "Connections", |ui| {
                    connection_count::show(ui, "batch-connections", &mut self.connections);
                });

                // --- Queue ------------------------------------------------------------
                // A new queue is the default here, unlike on Add Download: a batch is
                // usually something to leave running, a few at a time, on a schedule.
                let queues: Vec<(Uuid, String)> = cx
                    .vm
                    .queues
                    .iter()
                    .map(|q| (q.id, q.name.clone()))
                    .collect();
                if let Target::Existing(id) = self.target
                    && !queues.iter().any(|(q, _)| *q == id)
                {
                    self.target = Target::NewQueue;
                }
                let name = self.new_queue_name.clone();
                let label = |t: Target| match t {
                    Target::NewQueue => format!("Add to a new queue, \"{name}\""),
                    Target::StartNow => "Start them all now".to_string(),
                    Target::Existing(id) => format!(
                        "Add to \"{}\"",
                        queues
                            .iter()
                            .find(|(q, _)| *q == id)
                            .map(|(_, n)| n.as_str())
                            .unwrap_or("")
                    ),
                };
                form::row(ui, LABEL_WIDTH, "Queue", |ui| {
                    ComboBox::from_id_salt("batch-queue")
                        .width(full)
                        .selected_text(label(self.target))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.target,
                                Target::NewQueue,
                                label(Target::NewQueue),
                            );
                            ui.selectable_value(
                                &mut self.target,
                                Target::StartNow,
                                label(Target::StartNow),
                            );
                            for (id, _) in &queues {
                                let t = Target::Existing(*id);
                                ui.selectable_value(&mut self.target, t, label(t));
                            }
                        });
                });
            },
        );

        if check {
            self.check(ui, cx);
        }
        if form::escape_pressed(ui) {
            cancel = true;
        }
        if add {
            self.on_add(cx);
        } else if cancel {
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        self.closing.cancel();
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// A button filling the side column.
fn wide_button(ui: &mut Ui, text: &str, kind: ButtonKind, enabled: bool) -> bool {
    ui.allocate_ui_with_layout(
        vec2(SIDE_BUTTON, controls::BUTTON_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(SIDE_BUTTON);
            let r = controls::button_sized(ui, text, kind, enabled, SIDE_BUTTON);
            r.clicked()
        },
    )
    .inner
        && enabled
}
