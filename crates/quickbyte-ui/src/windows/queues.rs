//! Queues & Scheduler — port of `Forms/QueueManagerForm.cs`.
//!
//! Every queue on the left; on the right what is in the selected one (Files),
//! how it transfers (Options), and when it starts itself (Schedule).
//!
//! Edits apply as they are made rather than behind an OK button: a queue is a
//! live object — it may be running while this window is open, and its
//! schedule is read by another process — so each change goes straight through
//! `QueueManager::update`, and the lists are rebuilt from the view model, which
//! the queue events keep current.

use std::any::Any;
use std::collections::HashSet;

use egui::{Align, ComboBox, Layout, Margin, RichText, Sense, TextEdit, Ui, ViewportBuilder, vec2};
use egui_extras::{Column, TableBuilder};
use quickbyte_core::enums::{DownloadStatus, PowerAction, QueueState, ScheduleDays};
use quickbyte_core::helpers::byte_formatter::{BYTES_PER_KILOBYTE, format_bytes};
use quickbyte_core::models::{DownloadItem, DownloadQueue, QueueSchedule};
use quickbyte_core::serde_compat::TimeSpan;
use uuid::Uuid;

use super::prompt::{Prompt, PromptIcon};
use super::{AppWindow, WindowCx, WindowKind};
use crate::icons::Icon;
use crate::shell::Shell;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{flat_tabs, form};

const DAYS: [(chrono::Weekday, &str); 7] = [
    (chrono::Weekday::Mon, "Mon"),
    (chrono::Weekday::Tue, "Tue"),
    (chrono::Weekday::Wed, "Wed"),
    (chrono::Weekday::Thu, "Thu"),
    (chrono::Weekday::Fri, "Fri"),
    (chrono::Weekday::Sat, "Sat"),
    (chrono::Weekday::Sun, "Sun"),
];
const CAPTION_WIDTH: f32 = 250.0;
const CONTROL_WIDTH: f32 = 84.0;

#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Cancel,
    Delete(Uuid),
}

/// The editor's copy of the selected queue's settings.
#[derive(Clone, Default)]
struct Editor {
    name: String,
    concurrent: i64,
    speed_kbps: i64,
    when_finished: PowerAction,
    enabled: bool,
    days: ScheduleDays,
    start: i64,
    stop_at: bool,
    stop: i64,
    wake: bool,
}

impl Editor {
    fn load(queue: Option<&DownloadQueue>) -> Self {
        let Some(q) = queue else {
            return Self {
                concurrent: DownloadQueue::MIN_CONCURRENT_DOWNLOADS as i64,
                ..Self::default()
            };
        };
        let s = &q.schedule;
        Self {
            name: q.name.clone(),
            concurrent: q.concurrent_downloads.clamp(
                DownloadQueue::MIN_CONCURRENT_DOWNLOADS,
                DownloadQueue::MAX_CONCURRENT_DOWNLOADS,
            ) as i64,
            speed_kbps: (q.speed_limit_bytes_per_second / BYTES_PER_KILOBYTE).clamp(0, 1_000_000),
            when_finished: q.when_finished,
            enabled: s.enabled,
            days: s.days,
            start: time_of_day(s.start_time),
            stop_at: s.stop_at_enabled,
            stop: time_of_day(s.stop_time),
            wake: s.wake_computer,
        }
    }
}

/// A time of day in seconds; anything outside one day reads as midnight.
fn time_of_day(t: TimeSpan) -> i64 {
    let s = t.total_seconds() as i64;
    if (0..86_400).contains(&s) { s } else { 0 }
}

pub struct QueuesWindow {
    selected: Option<Uuid>,
    tab: usize,
    editor: Editor,
    selected_files: HashSet<Uuid>,
    prompt: Option<Prompt<Answer>>,
    focus_name: bool,
    initialised: bool,
}

impl QueuesWindow {
    pub fn new() -> Self {
        Self {
            selected: None,
            tab: 0,
            editor: Editor::default(),
            selected_files: HashSet::new(),
            prompt: None,
            focus_name: false,
            initialised: false,
        }
    }

    /// Opens on a given tab (the demo's `--open queues:schedule`).
    pub fn on_tab(mut self, name: &str) -> Self {
        self.tab = match name {
            "options" => 1,
            "schedule" => 2,
            _ => 0,
        };
        self
    }

    fn select(&mut self, id: Option<Uuid>, cx: &WindowCx<'_>) {
        self.selected = id;
        self.selected_files.clear();
        self.editor = Editor::load(id.and_then(|id| cx.vm.queues.iter().find(|q| q.id == id)));
    }

    /// Pushes every control's value back onto the queue in one go.
    fn apply_edits(&mut self, cx: &WindowCx<'_>) {
        let Some(id) = self.selected else { return };
        let Some(mut queue) = cx.services.queues.find(id) else {
            return;
        };
        let e = &self.editor;
        queue.name = e.name.clone();
        queue.concurrent_downloads = e.concurrent as i32;
        queue.speed_limit_bytes_per_second = e.speed_kbps * BYTES_PER_KILOBYTE;
        queue.when_finished = e.when_finished;
        queue.schedule = QueueSchedule {
            enabled: e.enabled,
            days: e.days,
            start_time: TimeSpan::from_seconds(e.start),
            stop_at_enabled: e.stop_at,
            stop_time: TimeSpan::from_seconds(e.stop),
            wake_computer: e.wake,
            extra: queue.schedule.extra.clone(),
        };
        cx.services.queues.update(queue);
    }

    fn next_queue_name(cx: &WindowCx<'_>) -> String {
        let existing: HashSet<String> =
            cx.vm.queues.iter().map(|q| q.name.to_lowercase()).collect();
        (1..)
            .map(|i| format!("Queue {i}"))
            .find(|c| !existing.contains(&c.to_lowercase()))
            .expect("an unused name")
    }

    fn on_new_queue(&mut self, cx: &mut WindowCx<'_>) {
        let queue = cx.services.queues.create(&Self::next_queue_name(cx));
        // Straight into renaming it: "Queue 3" is a placeholder.
        self.selected = Some(queue.id);
        self.selected_files.clear();
        self.editor = Editor::load(Some(&queue));
        self.tab = 1;
        self.focus_name = true;
    }

    fn on_delete_queue(&mut self, cx: &WindowCx<'_>) {
        let Some(id) = self.selected else { return };
        let Some(queue) = cx.services.queues.find(id) else {
            return;
        };
        let text = if queue.item_ids.is_empty() {
            format!("Delete the queue \"{}\"?", queue.name)
        } else {
            format!(
                "Delete the queue \"{}\"?\n\nIts {} download(s) stay in the list — they simply stop belonging to a queue.",
                queue.name,
                queue.item_ids.len()
            )
        };
        self.prompt = Some(
            Prompt::new("Delete Queue", text, PromptIcon::Question, Answer::Cancel)
                .button("No", Answer::Cancel, ButtonKind::Secondary)
                .button("Yes", Answer::Delete(id), ButtonKind::Primary),
        );
    }

    // ------------------------------------------------------------- panes --

    fn sidebar(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        form::section_caption(ui, "QUEUES");
        ui.add_space(2.0);
        let list_height = ui.available_height() - 34.0;
        let mut clicked = None;
        form::list_frame(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.set_height(list_height);
            egui::ScrollArea::vertical()
                .id_salt("queue-list")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    for queue in &cx.vm.queues {
                        let selected = self.selected == Some(queue.id);
                        let (rect, response) = ui
                            .allocate_exact_size(vec2(ui.available_width(), 22.0), Sense::click());
                        let focused = ui.ctx().input(|i| i.focused);
                        let (fill, color) = if selected {
                            (
                                if focused {
                                    p.row_selected
                                } else {
                                    p.row_selected_inactive
                                },
                                if focused { p.text_on_accent } else { p.text },
                            )
                        } else if response.hovered() {
                            (p.accent_soft, p.text)
                        } else {
                            (p.surface, p.text)
                        };
                        ui.painter().rect_filled(rect, 0.0, fill);
                        let scheduled = if queue.schedule.enabled { "  ⏱" } else { "" };
                        let text = format!("{}  ({}){scheduled}", queue.name, queue.item_ids.len());
                        let galley = ui.painter().layout_no_wrap(text, theme::body(), color);
                        ui.painter().galley(
                            rect.left_center() + vec2(6.0, -galley.size().y / 2.0),
                            galley,
                            color,
                        );
                        if response.clicked() {
                            clicked = Some(queue.id);
                        }
                    }
                });
        });
        if let Some(id) = clicked
            && self.selected != Some(id)
        {
            self.commit_name(cx);
            self.select(Some(id), cx);
        }
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if controls::button_sized(ui, "New queue", ButtonKind::Secondary, true, 92.0).clicked()
            {
                self.commit_name(cx);
                self.on_new_queue(cx);
            }
            if controls::button_sized(
                ui,
                "Delete",
                ButtonKind::Secondary,
                self.selected.is_some(),
                84.0,
            )
            .clicked()
            {
                self.on_delete_queue(cx);
            }
        });
    }

    /// The name is applied when the box is left (or on Enter) rather than per
    /// keystroke: each apply persists the queue and rebuilds the lists.
    fn commit_name(&mut self, cx: &WindowCx<'_>) {
        if let Some(id) = self.selected
            && cx
                .vm
                .queues
                .iter()
                .any(|q| q.id == id && q.name != self.editor.name)
        {
            self.apply_edits(cx);
        }
    }

    fn state_line(&self, ui: &mut Ui, cx: &WindowCx<'_>, queue: Option<&DownloadQueue>) {
        let p = theme::palette();
        let (text, color) = match queue {
            None => (
                "No queue selected. Create one to group downloads and give them their own schedule.".to_string(),
                p.text_muted,
            ),
            Some(q) if cx.vm.queue_state(q.id) == QueueState::Running => (
                format!("\"{}\" is running — {} download(s) at a time.", q.name, q.clamp_concurrency()),
                p.accent,
            ),
            Some(q) => match cx.services.queues.next_run_at(q.id) {
                Some(when) => (
                    format!("\"{}\" is idle. Next scheduled start: {}.", q.name, format_when(when)),
                    p.text_muted,
                ),
                None => (
                    format!("\"{}\" is idle, with no schedule. Start it here whenever you like.", q.name),
                    p.text_muted,
                ),
            },
        };
        form::status_line(ui, &text, color);
    }

    fn files_tab(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>, queue: Option<&DownloadQueue>) {
        let items: Vec<DownloadItem> = queue
            .map(|q| {
                q.item_ids
                    .iter()
                    .filter_map(|id| cx.vm.item(*id).cloned())
                    .collect()
            })
            .unwrap_or_default();
        self.selected_files
            .retain(|id| items.iter().any(|i| i.id == *id));
        let side = 112.0;
        let list_width = ui.available_width() - side - 12.0;
        let height = ui.available_height();
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(vec2(list_width, height), Layout::top_down(Align::Min), |ui| {
                self.files_list(ui, &items, height);
            });
            ui.add_space(12.0 - ui.spacing().item_spacing.x);
            ui.allocate_ui_with_layout(vec2(side, height), Layout::top_down(Align::Min), |ui| {
                ui.set_width(side);
                ui.spacing_mut().item_spacing.y = 6.0;
                let one = self.selected_files.len() == 1;
                if controls::button_sized(ui, "Move up", ButtonKind::Secondary, one, side).clicked() {
                    self.move_selected(-1, cx);
                }
                if controls::button_sized(ui, "Move down", ButtonKind::Secondary, one, side).clicked() {
                    self.move_selected(1, cx);
                }
                let any = !self.selected_files.is_empty();
                if controls::button_sized(ui, "Take out", ButtonKind::Secondary, any, side).clicked() {
                    let ids: Vec<Uuid> = self.selected_files.drain().collect();
                    cx.services.queues.remove_from_queues(&ids);
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    form::note(
                        ui,
                        "Downloads join a queue from the main window: right-click a download and choose \"Add to queue\".",
                    );
                });
            });
        });
    }

    fn move_selected(&mut self, offset: i32, cx: &WindowCx<'_>) {
        let (Some(queue), Some(id)) = (self.selected, self.selected_files.iter().next().copied())
        else {
            return;
        };
        cx.services.queues.move_item(queue, id, offset);
    }

    fn files_list(&mut self, ui: &mut Ui, items: &[DownloadItem], height: f32) {
        let p = theme::palette();
        let mut clicked: Option<(Uuid, bool)> = None;
        form::list_frame(ui, |ui| {
            ui.set_height(height - 2.0);
            form::list_visuals(ui);
            TableBuilder::new(ui)
                .id_salt("queue-files")
                .striped(true)
                .sense(Sense::click())
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(30.0))
                .column(Column::initial(230.0).at_least(80.0).clip(true))
                .column(Column::initial(80.0).at_least(50.0).clip(true))
                .column(Column::remainder().at_least(60.0).clip(true))
                .header(22.0, |mut header| {
                    for title in ["#", "File Name", "Size", "Status"] {
                        header.col(|ui| form::header_cell(ui, title));
                    }
                })
                .body(|body| {
                    body.rows(theme::ROW_HEIGHT, items.len(), |mut row| {
                        let item = &items[row.index()];
                        let selected = self.selected_files.contains(&item.id);
                        row.set_selected(selected);
                        let color = if selected { ui_selected_text() } else { p.text };
                        let cell = |ui: &mut Ui, text: &str| {
                            ui.add_space(6.0);
                            ui.add(
                                egui::Label::new(RichText::new(text).color(color))
                                    .truncate()
                                    .selectable(false),
                            );
                        };
                        let position = (row.index() + 1).to_string();
                        row.col(|ui| cell(ui, &position));
                        row.col(|ui| cell(ui, &item.file_name));
                        row.col(|ui| cell(ui, &format_bytes(item.total_bytes)));
                        row.col(|ui| cell(ui, &status_text(item)));
                        let response = row.response();
                        if response.clicked() {
                            let ctrl = response.ctx.input(|i| i.modifiers.command);
                            clicked = Some((item.id, ctrl));
                        }
                    });
                });
        });
        if let Some((id, ctrl)) = clicked {
            if ctrl {
                if !self.selected_files.remove(&id) {
                    self.selected_files.insert(id);
                }
            } else {
                self.selected_files.clear();
                self.selected_files.insert(id);
            }
        }
    }

    fn options_tab(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>, has_queue: bool) {
        let mut changed = false;
        ui.add_enabled_ui(has_queue, |ui| {
            row(ui, "Queue name", "Shown in the sidebar and in the Add to queue menu.", |ui, width| {
                let response = form::text_with(
                    ui,
                    TextEdit::singleline(&mut self.editor.name).char_limit(DownloadQueue::MAX_NAME_LENGTH),
                    width,
                );
                if std::mem::take(&mut self.focus_name) {
                    response.request_focus();
                }
                if response.lost_focus() {
                    changed = true;
                }
            });
            right_row(ui, "Downloads at once", "How many of this queue's files transfer together. The app-wide limit still applies.", |ui| {
                changed |= form::spinner(
                    ui,
                    "q-concurrent",
                    &mut self.editor.concurrent,
                    DownloadQueue::MIN_CONCURRENT_DOWNLOADS as i64,
                    DownloadQueue::MAX_CONCURRENT_DOWNLOADS as i64,
                    1,
                    CONTROL_WIDTH,
                );
            });
            right_row(ui, "Queue speed limit (KB/s)", "Shared by this queue's downloads; 0 = no limit. Applies at once.", |ui| {
                changed |= form::spinner(ui, "q-speed", &mut self.editor.speed_kbps, 0, 1_000_000, 50, CONTROL_WIDTH);
            });
            let shell = cx.shell;
            row(ui, "When the queue finishes", "After every file is done.", |ui, width| {
                ComboBox::from_id_salt("q-when-finished")
                    .width(width)
                    .selected_text(shell.power_label(self.editor.when_finished))
                    .show_ui(ui, |ui| {
                        for action in PowerAction::ALL {
                            changed |= ui
                                .selectable_value(&mut self.editor.when_finished, action, shell.power_label(action))
                                .changed();
                        }
                    });
            });
        });
        if changed {
            self.apply_edits(cx);
        }
    }

    fn schedule_tab(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>, queue: Option<&DownloadQueue>) {
        let p = theme::palette();
        let has = queue.is_some();
        let mut changed = false;
        let e = &mut self.editor;
        ui.add_enabled_ui(has, |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            changed |= ui
                .checkbox(
                    &mut e.enabled,
                    RichText::new("Start this queue automatically").font(theme::body_strong()),
                )
                .changed();
            ui.add_space(2.0);
            let scheduled = e.enabled;
            ui.add_enabled_ui(scheduled, |ui| {
                form::row(ui, 130.0, "On these days", |ui| {
                    for (day, label) in DAYS {
                        let flag = ScheduleDays::from_weekday(day);
                        let mut on = e.days.contains(flag);
                        if ui.checkbox(&mut on, label).changed() {
                            e.days = e.days.with(flag, on);
                            changed = true;
                        }
                        ui.add_space(4.0);
                    }
                });
                form::row(ui, 130.0, "Start at", |ui| {
                    changed |= form::time_picker(ui, "q-start", &mut e.start);
                });
            });
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), form::ROW_HEIGHT),
                Layout::left_to_right(Align::Center),
                |ui| {
                    ui.allocate_ui_with_layout(
                        vec2(130.0, 22.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.set_width(130.0);
                            changed |= ui.checkbox(&mut e.stop_at, "Stop at").changed();
                        },
                    );
                    ui.add_enabled_ui(scheduled && e.stop_at, |ui| {
                        changed |= form::time_picker(ui, "q-stop", &mut e.stop);
                    });
                },
            );
            ui.add_enabled_ui(scheduled, |ui| {
                changed |= ui
                    .checkbox(&mut e.wake, "Wake the computer to start it")
                    .changed();
            });
        });
        ui.add_space(8.0);
        if let Some(q) = queue {
            let next = cx.services.queues.next_run_at(q.id);
            ui.add(
                egui::Label::new(
                    RichText::new(schedule_note(&self.editor, next, cx.shell))
                        .font(theme::small())
                        .color(p.text_muted),
                )
                .wrap(),
            );
        }
        if changed {
            self.apply_edits(cx);
        }
    }
}

impl Default for QueuesWindow {
    fn default() -> Self {
        Self::new()
    }
}

fn ui_selected_text() -> egui::Color32 {
    theme::palette().text_on_accent
}

fn status_text(item: &DownloadItem) -> String {
    match item.status {
        DownloadStatus::Queued => "Waiting".into(),
        DownloadStatus::Completed => "Complete".into(),
        s => format!("{s:?}"),
    }
}

/// `ddd d MMM, HH:mm`.
fn format_when(when: chrono::NaiveDateTime) -> String {
    when.format("%a %-d %b, %H:%M").to_string()
}

/// The paragraph under the schedule fields. It answers the question a
/// schedule raises — "does this work when QuickByte is closed?" — honestly,
/// which depends on whether the background scheduler is installed.
fn schedule_note(e: &Editor, next_run: Option<chrono::NaiveDateTime>, shell: Shell) -> String {
    if !e.enabled {
        return "Not scheduled. The queue only runs when you start it here.".into();
    }
    if e.days.is_empty() {
        return "No days are selected, so this schedule will never fire. Tick at least one day."
            .into();
    }
    let next = next_run
        .map(|w| format!("Next start: {}. ", format_when(w)))
        .unwrap_or_default();
    // Said only when asked for: the caveat lives in Windows' power options.
    let wake = if e.wake {
        " A sleeping computer is woken for it, if Windows' power options allow wake timers."
    } else {
        ""
    };
    if !shell.agent_available() {
        return format!(
            "{next}QuickByte must be running (a window or the notification area) at that time — \
             the background scheduler is not installed with this copy.{wake}"
        );
    }
    format!(
        "{next}QuickByte does not need to be open: the background scheduler starts with Windows \
         and launches QuickByte when the time comes.{wake}"
    )
}

/// A caption block, then a field filling the rest.
fn row(ui: &mut Ui, caption: &str, hint: &str, add: impl FnOnce(&mut Ui, f32)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 42.0),
        Layout::left_to_right(Align::Center),
        |ui| {
            form::caption_block(ui, CAPTION_WIDTH, caption, hint);
            let width = ui.available_width();
            add(ui, width);
        },
    );
}

/// A caption block spanning, and a control on the right.
fn right_row(ui: &mut Ui, caption: &str, hint: &str, add: impl FnOnce(&mut Ui)) {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 42.0),
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

impl AppWindow for QueuesWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Queues
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Queues & Scheduler")
            .with_inner_size(vec2(820.0, 540.0))
            .with_min_inner_size(vec2(740.0, 480.0))
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Queues & Scheduler".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if !self.initialised {
            self.initialised = true;
            let first = cx.vm.queues.first().map(|q| q.id);
            self.select(first, cx);
        }
        // A deleted queue (here or elsewhere) leaves nothing selected.
        if self
            .selected
            .is_some_and(|id| !cx.vm.queues.iter().any(|q| q.id == id))
        {
            let first = cx.vm.queues.first().map(|q| q.id);
            self.select(first, cx);
        }
        let queue = self
            .selected
            .and_then(|id| cx.vm.queues.iter().find(|q| q.id == id).cloned());
        let running = queue
            .as_ref()
            .is_some_and(|q| cx.vm.queue_state(q.id) == QueueState::Running);

        form::header(
            ui,
            "q-header",
            Icon::Queue,
            "Queues & Scheduler",
            "Group downloads into queues, cap them, and start them on a schedule.",
        );
        let mut close = false;
        controls::footer(ui, "q-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                close |= controls::button(ui, "Close", ButtonKind::Secondary, true).clicked();
                ui.add_space(4.0);
                if controls::button_sized(ui, "Stop queue", ButtonKind::Secondary, running, 96.0)
                    .clicked()
                    && let Some(q) = &queue
                {
                    cx.services.queues.stop(q.id);
                }
                if controls::button_sized(
                    ui,
                    "Start queue",
                    ButtonKind::Primary,
                    queue.is_some() && !running,
                    96.0,
                )
                .clicked()
                    && let Some(q) = &queue
                {
                    cx.services.queues.start(q.id);
                }
            });
        });
        egui::Panel::left("q-side")
            .exact_size(214.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(p.surface).inner_margin(Margin {
                left: 14,
                right: 12,
                top: 10,
                bottom: 10,
            }))
            .show(ui, |ui| self.sidebar(ui, cx));
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(p.surface).inner_margin(Margin {
                left: 0,
                right: 14,
                top: 8,
                bottom: 10,
            }))
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    vec2(ui.available_width(), 22.0),
                    Layout::left_to_right(Align::Center),
                    |ui| {
                        self.state_line(ui, cx, queue.as_ref());
                    },
                );
                ui.add_space(2.0);
                flat_tabs::show(ui, &["Files", "Options", "Schedule"], &mut self.tab, 28.0);
                egui::Frame::NONE
                    .inner_margin(Margin {
                        left: 12,
                        right: 4,
                        top: 10,
                        bottom: 0,
                    })
                    .show(ui, |ui| match self.tab {
                        0 => self.files_tab(ui, cx, queue.as_ref()),
                        1 => self.options_tab(ui, cx, queue.is_some()),
                        _ => self.schedule_tab(ui, cx, queue.as_ref()),
                    });
            });

        if let Some(prompt) = &self.prompt {
            if let Some(answer) = prompt.show(ui) {
                self.prompt = None;
                if let Answer::Delete(id) = answer {
                    cx.services.queues.delete(id);
                    let first = cx.vm.queues.iter().find(|q| q.id != id).map(|q| q.id);
                    self.select(first, cx);
                }
            }
            return;
        }
        if ui.memory(|m| m.focused().is_some()) && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            self.commit_name(cx);
        }
        if form::escape_pressed(ui) {
            close = true;
        }
        if close {
            self.commit_name(cx);
            cx.close();
        }
    }

    fn on_close_requested(&mut self, cx: &mut WindowCx<'_>) -> bool {
        self.commit_name(cx);
        true
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
