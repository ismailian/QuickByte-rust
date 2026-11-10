//! The main window — port of `Forms/MainForm.cs`: menu bar
//! (File/Tasks/Downloads/View/Help), icon toolbar with a search box, a category
//! tree sidebar (All Downloads by type, Unfinished/Finished/Failed, and a Queues
//! branch), the downloads table with per-type icons and owner-drawn progress
//! cells, the row context menu, and a status bar with counts and total speed.
//!
//! Everything it shows is read from the shared [`ViewModel`] each frame; the
//! table is virtualised (only visible rows are laid out) so a long list costs
//! what a short one does.

use std::collections::HashSet;

use egui::{
    Align, Align2, Color32, Key, KeyboardShortcut, Layout, Modifiers, Rect, RichText, Sense,
    Stroke, Ui, vec2,
};
use egui_extras::{Column, TableBuilder};
use quickbyte_core::enums::DownloadStatus;
use quickbyte_core::helpers::byte_formatter::{
    format_bytes, format_eta, format_percentage, format_speed,
};
use quickbyte_core::helpers::file_category::{FileCategory, get_category};
use quickbyte_core::helpers::file_checksum::ChecksumResult;
use quickbyte_core::models::{DownloadItem, UpdateCheckResult};
use uuid::Uuid;

use super::prompt::{Prompt, PromptIcon};
use super::{AppCommand, DialogRequest, WindowCx};
use crate::actions;
use crate::icons::{self, Icon};
use crate::services::Pending;
use crate::theme;
use crate::view_model::{DownloadRow, ViewModel};
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::progress_bar;

const ROW_HEIGHT: f32 = theme::ROW_HEIGHT;
const SEARCH_ID: &str = "main-search";

/// What the sidebar has selected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Category(FileCategory),
    Unfinished,
    Finished,
    Failed,
    /// Everything in any queue.
    Queues,
    Queue(Uuid),
}

/// The table's columns, in C# order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnId {
    Name,
    Size,
    Progress,
    Speed,
    Eta,
    Status,
    Description,
}

const COLUMNS: [(ColumnId, &str, f32); 7] = [
    (ColumnId::Name, "File Name", 230.0),
    (ColumnId::Size, "Size", 76.0),
    (ColumnId::Progress, "Progress", 112.0),
    (ColumnId::Speed, "Transfer Rate", 92.0),
    (ColumnId::Eta, "Time Left", 72.0),
    (ColumnId::Status, "Status", 100.0),
    (ColumnId::Description, "Description", 160.0),
];

/// What a main-window prompt's buttons mean.
#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Cancel,
    Remove { ids: Vec<Uuid>, delete_files: bool },
    Stop(Vec<Uuid>),
    DeleteCompleted(Vec<Uuid>),
    Ok,
}

pub struct MainWindow {
    version: String,
    has_tray: bool,
    show_toolbar: bool,
    show_sidebar: bool,
    filter: Filter,
    all_expanded: bool,
    queues_expanded: bool,
    search_text: String,
    focus_search: bool,
    sort: Option<(ColumnId, bool)>,
    selected: HashSet<Uuid>,
    anchor: Option<Uuid>,
    cursor: Option<Uuid>,
    scroll_to: Option<usize>,
    prompt: Option<Prompt<Answer>>,
}

impl MainWindow {
    pub fn new(version: &str, has_tray: bool) -> Self {
        Self {
            version: version.to_string(),
            has_tray,
            show_toolbar: true,
            show_sidebar: true,
            filter: Filter::All,
            all_expanded: true,
            queues_expanded: true,
            search_text: String::new(),
            focus_search: false,
            sort: None,
            selected: HashSet::new(),
            anchor: None,
            cursor: None,
            scroll_to: None,
            prompt: None,
        }
    }

    pub fn title(&self) -> String {
        format!("QuickByte Download Manager {}", self.version)
    }

    pub fn is_animating(&self) -> bool {
        false
    }

    /// A row went away: it can no longer be selected.
    pub fn forget(&mut self, id: Uuid) {
        self.selected.remove(&id);
        if self.anchor == Some(id) {
            self.anchor = None;
        }
        if self.cursor == Some(id) {
            self.cursor = None;
        }
    }

    pub fn on_status_changed(&mut self, _id: Uuid) {}

    pub fn show_message(&mut self, title: &str, text: &str) {
        self.prompt = Some(
            Prompt::new(title, text, PromptIcon::Info, Answer::Ok).button(
                "OK",
                Answer::Ok,
                ButtonKind::Primary,
            ),
        );
    }

    /// Help > Check for Updates reports back either way; an update goes straight
    /// to the update window.
    pub fn on_manual_update_check(
        &mut self,
        result: quickbyte_core::Result<UpdateCheckResult>,
        version: &str,
        commands: &mut Vec<AppCommand>,
    ) {
        match result {
            Err(e) => {
                self.prompt = Some(
                    Prompt::new(
                        "Check for Updates",
                        format!("Could not check for updates.\n\n{e}"),
                        PromptIcon::Warning,
                        Answer::Ok,
                    )
                    .button("OK", Answer::Ok, ButtonKind::Primary),
                )
            }
            Ok(UpdateCheckResult {
                update_available: true,
                manifest: Some(manifest),
                ..
            }) => commands.push(AppCommand::Open(DialogRequest::Update {
                manifest: Box::new(manifest),
                automatic: true,
            })),
            Ok(_) => self.show_message(
                "Check for Updates",
                &format!("QuickByte {version} is the latest version."),
            ),
        }
    }

    // ------------------------------------------------------------ the list --

    fn passes_filter(&self, item: &DownloadItem, vm: &ViewModel) -> bool {
        match self.filter {
            Filter::All => true,
            Filter::Category(c) => get_category(&item.file_name) == c,
            Filter::Unfinished => matches!(
                item.status,
                DownloadStatus::Queued
                    | DownloadStatus::Connecting
                    | DownloadStatus::Downloading
                    | DownloadStatus::Paused
                    | DownloadStatus::Merging
                    | DownloadStatus::Verifying
            ),
            Filter::Finished => item.status == DownloadStatus::Completed,
            Filter::Failed => matches!(
                item.status,
                DownloadStatus::Failed | DownloadStatus::Cancelled
            ),
            Filter::Queues => vm.queue_id_of(item.id).is_some(),
            Filter::Queue(q) => vm.queue_id_of(item.id) == Some(q),
        }
    }

    /// The rows on show: the sidebar's filter, then the search, then the sort.
    fn visible_rows(&self, vm: &ViewModel) -> Vec<Uuid> {
        let terms = actions::search_terms(&self.search_text);
        let mut rows: Vec<&DownloadRow> = vm
            .rows()
            .filter(|r| self.passes_filter(&r.item, vm) && actions::search_matches(&r.item, &terms))
            .collect();
        if let Some((column, ascending)) = self.sort {
            rows.sort_by(|a, b| {
                let (a, b) = (&a.item, &b.item);
                let ord = match column {
                    ColumnId::Name => a.file_name.to_lowercase().cmp(&b.file_name.to_lowercase()),
                    ColumnId::Size => a.total_bytes.cmp(&b.total_bytes),
                    ColumnId::Progress => {
                        a.progress_percentage().total_cmp(&b.progress_percentage())
                    }
                    ColumnId::Speed => a
                        .current_speed_bytes_per_second
                        .total_cmp(&b.current_speed_bytes_per_second),
                    ColumnId::Eta => eta_key(a).cmp(&eta_key(b)),
                    ColumnId::Status => (a.status as i32).cmp(&(b.status as i32)),
                    ColumnId::Description => description_for(a).cmp(&description_for(b)),
                };
                if ascending { ord } else { ord.reverse() }
            });
        }
        rows.into_iter().map(|r| r.item.id).collect()
    }

    /// The selection, in list order (a snapshot: commands change status, which
    /// re-filters rows out from under it).
    fn selection<'a>(&self, vm: &'a ViewModel) -> Vec<&'a DownloadItem> {
        vm.items()
            .filter(|i| self.selected.contains(&i.id))
            .collect()
    }

    fn single<'a>(&self, vm: &'a ViewModel) -> Option<&'a DownloadItem> {
        let s = self.selection(vm);
        if s.len() == 1 { Some(s[0]) } else { None }
    }

    // ---------------------------------------------------------------- frame --

    pub fn ui(
        &mut self,
        ui: &mut Ui,
        cx: &mut WindowCx<'_>,
        manual_update: &mut Option<Pending<quickbyte_core::Result<UpdateCheckResult>>>,
    ) {
        // The selection only names rows that still exist.
        self.selected.retain(|id| cx.vm.get(*id).is_some());
        let visible = self.visible_rows(cx.vm);
        let commands = CommandStates::new(self, cx);

        self.shortcuts(ui, cx, &visible, &commands);

        let p = theme::palette();
        egui::Panel::top("menu")
            .exact_size(24.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::NONE.fill(p.chrome).inner_margin(egui::Margin {
                left: 4,
                right: 4,
                top: 1,
                bottom: 1,
            }))
            .show(ui, |ui| self.menu_bar(ui, cx, &commands, manual_update));

        if self.show_toolbar {
            egui::Panel::top("toolbar")
                .exact_size(34.0)
                .resizable(false)
                .frame(
                    egui::Frame::NONE
                        .fill(p.chrome)
                        .inner_margin(egui::Margin::symmetric(6, 5)),
                )
                .show(ui, |ui| self.toolbar(ui, cx, &commands));
        }

        egui::Panel::bottom("status")
            .exact_size(22.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.chrome)
                    .inner_margin(egui::Margin::symmetric(10, 0)),
            )
            .show(ui, |ui| self.status_bar(ui, cx.vm));

        if self.show_sidebar {
            egui::Panel::left("sidebar")
                .default_size(178.0)
                .size_range(140.0..=320.0)
                .resizable(true)
                .frame(egui::Frame::NONE.fill(p.sidebar))
                .show(ui, |ui| self.sidebar(ui, cx.vm));
        }

        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(p.surface))
            .show(ui, |ui| self.table(ui, cx, &visible));

        if let Some(prompt) = &self.prompt
            && let Some(answer) = prompt.show(ui)
        {
            self.prompt = None;
            self.on_answer(answer, cx);
        }
    }

    // ---------------------------------------------------------- shortcuts --

    fn shortcuts(
        &mut self,
        ui: &mut Ui,
        cx: &mut WindowCx<'_>,
        visible: &[Uuid],
        commands: &CommandStates,
    ) {
        let typing = ui.memory(|m| m.focused()) == Some(egui::Id::unique(SEARCH_ID));
        let modal = self.prompt.is_some();
        if modal {
            return;
        }
        let shortcut = |ui: &mut Ui, m: Modifiers, k: Key| {
            ui.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)))
        };

        // Ctrl+N — the shortcut the Add URL tooltip has always promised.
        if shortcut(ui, Modifiers::COMMAND, Key::N) {
            cx.open(DialogRequest::NewDownload {
                url: None,
                captured: None,
            });
        }
        if shortcut(ui, Modifiers::COMMAND, Key::F) {
            self.focus_search = true;
        }
        if typing {
            if shortcut(ui, Modifiers::NONE, Key::Escape) {
                self.search_text.clear();
                ui.memory_mut(|m| m.surrender_focus(egui::Id::unique(SEARCH_ID)));
            }
            // Enter or Down: straight into the results.
            if shortcut(ui, Modifiers::NONE, Key::Enter)
                || shortcut(ui, Modifiers::NONE, Key::ArrowDown)
            {
                ui.memory_mut(|m| m.surrender_focus(egui::Id::unique(SEARCH_ID)));
                if self.selected.is_empty()
                    && let Some(first) = visible.first()
                {
                    self.select_only(*first);
                }
            }
            return;
        }
        if shortcut(ui, Modifiers::COMMAND, Key::A) {
            self.selected = visible.iter().copied().collect();
        }
        if shortcut(ui, Modifiers::NONE, Key::Delete) && commands.delete {
            self.ask_remove(cx.vm);
        }
        if shortcut(ui, Modifiers::NONE, Key::Enter) {
            self.activate(cx);
        }
        for (key, step) in [(Key::ArrowDown, 1i64), (Key::ArrowUp, -1)] {
            let shift = ui.input(|i| i.modifiers.shift);
            if shortcut(
                ui,
                if shift {
                    Modifiers::SHIFT
                } else {
                    Modifiers::NONE
                },
                key,
            ) && !visible.is_empty()
            {
                let at = self
                    .cursor
                    .and_then(|c| visible.iter().position(|v| *v == c));
                let next = match at {
                    Some(i) => (i as i64 + step).clamp(0, visible.len() as i64 - 1) as usize,
                    None => 0,
                };
                if shift {
                    self.extend_to(visible, visible[next]);
                } else {
                    self.select_only(visible[next]);
                }
                self.scroll_to = Some(next);
            }
        }
    }

    // -------------------------------------------------------------- menus --

    fn menu_bar(
        &mut self,
        ui: &mut Ui,
        cx: &mut WindowCx<'_>,
        c: &CommandStates,
        manual_update: &mut Option<Pending<quickbyte_core::Result<UpdateCheckResult>>>,
    ) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if controls::menu_item(ui, Some(Icon::OpenFile), "Open File", None, c.open_file) {
                    self.open_selected_file(cx);
                }
                if controls::menu_item(
                    ui,
                    Some(Icon::OpenFolder),
                    "Open Containing Folder",
                    None,
                    c.open_folder,
                ) {
                    self.open_selected_folder(cx);
                }
                ui.separator();
                if controls::menu_item(ui, None, "Close to Tray", None, self.has_tray) {
                    cx.command(AppCommand::HideToTray);
                }
                if controls::menu_item(ui, Some(Icon::Exit), "Exit", None, true) {
                    cx.command(AppCommand::Exit);
                }
            });
            ui.menu_button("Tasks", |ui| {
                if controls::menu_item(ui, Some(Icon::AddUrl), "Add URL...", Some("Ctrl+N"), true) {
                    cx.open(DialogRequest::NewDownload {
                        url: None,
                        captured: None,
                    });
                }
                if controls::menu_item(
                    ui,
                    Some(Icon::BatchDownload),
                    "Add Batch Download...",
                    None,
                    true,
                ) {
                    cx.open(DialogRequest::Batch);
                }
                ui.separator();
                if controls::menu_item(ui, Some(Icon::Resume), "Resume", None, c.resume) {
                    self.resume_selected(cx);
                }
                if controls::menu_item(ui, Some(Icon::Pause), "Pause", None, c.pause) {
                    self.pause_selected(cx);
                }
                if controls::menu_item(ui, Some(Icon::StopAll), "Stop All", None, c.stop_all) {
                    cx.command(AppCommand::PauseEverything);
                }
                ui.separator();
                if controls::menu_item(ui, Some(Icon::Delete), "Delete", Some("Del"), c.delete) {
                    self.ask_remove(cx.vm);
                }
                if controls::menu_item(
                    ui,
                    Some(Icon::DeleteCompleted),
                    "Delete Completed",
                    None,
                    c.delete_completed,
                ) {
                    self.ask_delete_completed(cx.vm);
                }
                ui.separator();
                if controls::menu_item(ui, Some(Icon::Queue), "Queues & Scheduler...", None, true) {
                    cx.open(DialogRequest::Queues);
                }
                if controls::menu_item(ui, Some(Icon::Settings), "Options...", None, true) {
                    cx.open(DialogRequest::Settings);
                }
            });
            ui.menu_button("Downloads", |ui| {
                if controls::menu_item(
                    ui,
                    Some(Icon::Properties),
                    "Properties...",
                    Some("Enter"),
                    c.properties,
                ) {
                    self.open_selected_details(cx);
                }
                if controls::menu_item(ui, Some(Icon::Retry), "Retry", None, c.retry) {
                    self.retry_selected(cx);
                }
                if controls::menu_item(
                    ui,
                    Some(Icon::RefreshAddress),
                    "Refresh Download Address...",
                    None,
                    c.refresh_address,
                ) {
                    self.refresh_address(cx);
                }
            });
            ui.menu_button("View", |ui| {
                controls::menu_check(ui, "Toolbar", &mut self.show_toolbar);
                controls::menu_check(ui, "Categories Panel", &mut self.show_sidebar);
            });
            ui.menu_button("Help", |ui| {
                let idle = manual_update.is_none();
                if controls::menu_item(ui, Some(Icon::Update), "Check for Updates...", None, idle) {
                    let updates = cx.services.updates.clone();
                    let version = self.version.clone();
                    *manual_update = Some(cx.services.run(async move {
                        updates
                            .check_for_update(&version, &tokio_util::sync::CancellationToken::new())
                            .await
                    }));
                }
                ui.separator();
                if controls::menu_item(ui, Some(Icon::Properties), "About QuickByte...", None, true)
                {
                    cx.open(DialogRequest::About);
                }
            });
        });
    }

    // ------------------------------------------------------------ toolbar --

    fn toolbar(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>, c: &CommandStates) {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 1.0;
            if controls::toolbar_button(
                ui,
                Icon::AddUrl,
                "Add URL",
                "Add a new download (Ctrl+N)",
                true,
            )
            .clicked()
            {
                cx.open(DialogRequest::NewDownload {
                    url: None,
                    captured: None,
                });
            }
            controls::toolbar_separator(ui);
            if controls::toolbar_button(
                ui,
                Icon::Resume,
                "Resume",
                "Resume the selected downloads",
                c.resume,
            )
            .clicked()
            {
                self.resume_selected(cx);
            }
            if controls::toolbar_button(
                ui,
                Icon::Pause,
                "Pause",
                "Pause the selected downloads",
                c.pause,
            )
            .clicked()
            {
                self.pause_selected(cx);
            }
            if controls::toolbar_button(
                ui,
                Icon::StopAll,
                "Stop All",
                "Stop every queue and pause every download",
                c.stop_all,
            )
            .clicked()
            {
                cx.command(AppCommand::PauseEverything);
            }
            controls::toolbar_separator(ui);
            if controls::toolbar_button(
                ui,
                Icon::Delete,
                "Delete",
                "Remove the selected downloads (Del)",
                c.delete,
            )
            .clicked()
            {
                self.ask_remove(cx.vm);
            }
            if controls::toolbar_button(
                ui,
                Icon::DeleteCompleted,
                "Delete Completed",
                "Remove every finished download from the list",
                c.delete_completed,
            )
            .clicked()
            {
                self.ask_delete_completed(cx.vm);
            }
            controls::toolbar_separator(ui);
            if controls::toolbar_button(
                ui,
                Icon::Properties,
                "Properties",
                "Show the download's details (Enter)",
                c.properties,
            )
            .clicked()
            {
                self.open_selected_details(cx);
            }
            if controls::toolbar_button(ui, Icon::Queue, "Queues", "Queues and scheduler", true)
                .clicked()
            {
                cx.open(DialogRequest::Queues);
            }
            if controls::toolbar_button(ui, Icon::Settings, "Options", "Options", true).clicked() {
                cx.open(DialogRequest::Settings);
            }

            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let id = egui::Id::unique(SEARCH_ID);
                let edit = egui::TextEdit::singleline(&mut self.search_text)
                    .id(id)
                    .hint_text(
                        RichText::new("Search downloads  (Ctrl+F)")
                            .color(theme::palette().text_muted),
                    )
                    .desired_width(200.0)
                    .margin(egui::Margin::symmetric(6, 3));
                let response = ui
                    .add(edit)
                    .on_hover_text("Search file names and addresses (Ctrl+F)");
                if std::mem::take(&mut self.focus_search) {
                    response.request_focus();
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                        let all = egui::text::CCursorRange::two(
                            egui::text::CCursor::new(0),
                            egui::text::CCursor::new(self.search_text.chars().count()),
                        );
                        state.cursor.set_char_range(Some(all));
                        state.store(ui.ctx(), id);
                    }
                }
            });
        });
    }

    // ---------------------------------------------------------- status bar --

    fn status_bar(&self, ui: &mut Ui, vm: &ViewModel) {
        let p = theme::palette();
        let t = vm.totals();
        ui.horizontal_centered(|ui| {
            let plural = if t.total == 1 { "" } else { "s" };
            ui.label(
                RichText::new(format!(
                    "{} download{plural}   ·   {} active   ·   {} complete",
                    t.total, t.active, t.completed
                ))
                .font(theme::small())
                .color(p.text_muted),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if t.active > 0 {
                    ui.label(
                        RichText::new(format!("▼ {}", format_speed(t.speed)))
                            .font(theme::small_strong())
                            .color(p.accent),
                    );
                }
            });
        });
    }

    // ------------------------------------------------------------- sidebar --

    fn sidebar(&mut self, ui: &mut Ui, vm: &ViewModel) {
        let p = theme::palette();
        ui.spacing_mut().item_spacing.y = 0.0;
        // Header: CATEGORIES and a close cross.
        let (header, _) = ui.allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
        ui.painter().text(
            header.left_center() + vec2(12.0, 0.0),
            Align2::LEFT_CENTER,
            "CATEGORIES",
            theme::small_strong(),
            p.text_muted,
        );
        let close =
            Rect::from_center_size(header.right_center() - vec2(14.0, 0.0), vec2(18.0, 18.0));
        let close_response = ui
            .interact(close, ui.scope_id().with("close-sidebar"), Sense::click())
            .on_hover_text("Hide the categories panel");
        let color = if close_response.hovered() {
            p.danger
        } else {
            p.text_muted
        };
        let c = close.center();
        for (a, b) in [
            (vec2(-3.5, -3.5), vec2(3.5, 3.5)),
            (vec2(-3.5, 3.5), vec2(3.5, -3.5)),
        ] {
            ui.painter()
                .line_segment([c + a, c + b], Stroke::new(1.3, color));
        }
        if close_response.clicked() {
            self.show_sidebar = false;
        }
        ui.painter().hline(
            header.x_range(),
            header.bottom() - 0.5,
            Stroke::new(1.0, p.border),
        );
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut all_expanded = self.all_expanded;
                if self.tree_row(
                    ui,
                    0,
                    Some(&mut all_expanded),
                    Icon::Folder(Some(p.accent_light)),
                    "All Downloads",
                    Filter::All,
                ) {
                    self.filter = Filter::All;
                }
                self.all_expanded = all_expanded;
                if self.all_expanded {
                    for category in FileCategory::ALL {
                        if self.tree_row(
                            ui,
                            1,
                            None,
                            Icon::Category(category),
                            category.name(),
                            Filter::Category(category),
                        ) {
                            self.filter = Filter::Category(category);
                        }
                    }
                }
                for (icon, label, filter) in [
                    (Icon::StatusDot(p.accent), "Unfinished", Filter::Unfinished),
                    (Icon::StatusDot(p.success), "Finished", Filter::Finished),
                    (Icon::StatusDot(p.danger), "Failed", Filter::Failed),
                ] {
                    if self.tree_row(ui, 0, None, icon, label, filter) {
                        self.filter = filter;
                    }
                }
                let mut queues_expanded = self.queues_expanded;
                let expander = if vm.queues.is_empty() {
                    None
                } else {
                    Some(&mut queues_expanded)
                };
                if self.tree_row(ui, 0, expander, Icon::Queue, "Queues", Filter::Queues) {
                    self.filter = Filter::Queues;
                }
                self.queues_expanded = queues_expanded;
                if self.queues_expanded {
                    for queue in &vm.queues {
                        let label = format!("{} ({})", queue.name, queue.item_ids.len());
                        if self.tree_row(ui, 1, None, Icon::Queue, &label, Filter::Queue(queue.id))
                        {
                            self.filter = Filter::Queue(queue.id);
                        }
                    }
                }
                // The queue that was being looked at is gone: fall back to the branch.
                if let Filter::Queue(id) = self.filter
                    && !vm.queues.iter().any(|q| q.id == id)
                {
                    self.filter = Filter::Queues;
                }
            });
    }

    /// One sidebar node: an expander (for a branch), its icon and its label.
    /// Returns true when the node was clicked.
    fn tree_row(
        &self,
        ui: &mut Ui,
        depth: usize,
        expanded: Option<&mut bool>,
        icon: Icon,
        label: &str,
        filter: Filter,
    ) -> bool {
        let p = theme::palette();
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), ROW_HEIGHT + 2.0), Sense::click());
        let selected = self.filter == filter;
        let painter = ui.painter();
        if selected {
            painter.rect_filled(
                rect.shrink2(vec2(4.0, 0.0)),
                theme::CORNER_RADIUS,
                p.accent_pressed,
            );
        } else if response.hovered() {
            painter.rect_filled(
                rect.shrink2(vec2(4.0, 0.0)),
                theme::CORNER_RADIUS,
                p.accent_soft,
            );
        }
        let x = rect.left() + 8.0 + depth as f32 * 16.0;
        let mut toggled = false;
        if let Some(expanded) = expanded {
            let arrow =
                Rect::from_center_size(egui::pos2(x + 5.0, rect.center().y), vec2(14.0, 14.0));
            let arrow_response = ui.interact(arrow, response.id.with("expander"), Sense::click());
            let c = arrow.center();
            let color = if arrow_response.hovered() {
                p.accent
            } else {
                p.text_muted
            };
            let points = if *expanded {
                vec![
                    c + vec2(-3.5, -1.5),
                    c + vec2(3.5, -1.5),
                    c + vec2(0.0, 2.5),
                ]
            } else {
                vec![
                    c + vec2(-1.5, -3.5),
                    c + vec2(2.5, 0.0),
                    c + vec2(-1.5, 3.5),
                ]
            };
            ui.painter()
                .add(egui::Shape::convex_polygon(points, color, Stroke::NONE));
            if arrow_response.clicked() || response.double_clicked() {
                *expanded = !*expanded;
                toggled = true;
            }
        }
        let icon_rect = Rect::from_min_size(
            egui::pos2(x + 12.0, rect.center().y - 8.0),
            vec2(16.0, 16.0),
        );
        icons::paint(ui.painter(), icon_rect, icon);
        let font = if selected {
            theme::body_strong()
        } else {
            theme::body()
        };
        ui.painter().text(
            egui::pos2(icon_rect.right() + 6.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            font,
            if selected { p.accent } else { p.text },
        );
        response.clicked() && !toggled
    }

    // --------------------------------------------------------------- table --

    fn table(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>, visible: &[Uuid]) {
        let p = theme::palette();
        let focused = ui.ctx().input(|i| i.focused);
        {
            // The list's own colours: banding, selection (white on accent while the
            // window has focus, a grey band when it does not), and hover.
            let v = ui.visuals_mut();
            v.faint_bg_color = p.row_alt;
            v.selection.bg_fill = if focused {
                p.row_selected
            } else {
                p.row_selected_inactive
            };
            v.selection.stroke.color = if focused { p.text_on_accent } else { p.text };
            v.widgets.hovered.bg_fill = p.accent_soft;
            let s = ui.spacing_mut();
            s.item_spacing = vec2(0.0, 0.0);
        }

        let mut table = TableBuilder::new(ui)
            .id_salt("downloads")
            .striped(true)
            .resizable(true)
            .sense(Sense::click())
            .auto_shrink([false, false])
            .cell_layout(Layout::left_to_right(Align::Center));
        for (i, (_, _, width)) in COLUMNS.iter().enumerate() {
            table = table.column(if i == COLUMNS.len() - 1 {
                Column::remainder().at_least(80.0).clip(true)
            } else {
                Column::initial(*width).at_least(44.0).clip(true)
            });
        }
        if let Some(row) = self.scroll_to.take() {
            table = table.scroll_to_row(row, None);
        }

        let mut clicked_header = None;
        let mut clicked_row: Option<(Uuid, Modifiers, bool, bool)> = None;
        let mut context_target: Option<Uuid> = None;

        table
            .header(22.0, |mut header| {
                for (column, title, _) in COLUMNS {
                    header.col(|ui| {
                        let rect = ui.max_rect();
                        let response = ui.interact(
                            rect,
                            ui.scope_id().with(("header", title)),
                            Sense::click(),
                        );
                        let painter = ui.painter();
                        painter.rect_filled(
                            rect,
                            0.0,
                            if response.hovered() {
                                p.accent_soft
                            } else {
                                p.header_back
                            },
                        );
                        painter.hline(
                            rect.x_range(),
                            rect.bottom() - 0.5,
                            Stroke::new(1.0, p.border),
                        );
                        painter.vline(
                            rect.right() - 0.5,
                            rect.y_range().shrink(4.0),
                            Stroke::new(1.0, p.border),
                        );
                        painter.text(
                            rect.left_center() + vec2(7.0, 0.0),
                            Align2::LEFT_CENTER,
                            title,
                            theme::body_strong(),
                            p.text,
                        );
                        if let Some((sorted, ascending)) = self.sort
                            && sorted == column
                        {
                            let c = rect.right_center() - vec2(10.0, 0.0);
                            let points = if ascending {
                                vec![c + vec2(-3.5, 2.0), c + vec2(3.5, 2.0), c + vec2(0.0, -2.0)]
                            } else {
                                vec![
                                    c + vec2(-3.5, -2.0),
                                    c + vec2(3.5, -2.0),
                                    c + vec2(0.0, 2.0),
                                ]
                            };
                            painter.add(egui::Shape::convex_polygon(
                                points,
                                p.text_muted,
                                Stroke::NONE,
                            ));
                        }
                        if response.clicked() {
                            clicked_header = Some(column);
                        }
                    });
                }
            })
            .body(|body| {
                body.rows(ROW_HEIGHT, visible.len(), |mut row| {
                    let id = visible[row.index()];
                    let Some(data) = cx.vm.get(id) else { return };
                    let item = &data.item;
                    row.set_selected(self.selected.contains(&id));
                    let displayed = if item.status == DownloadStatus::Completed {
                        100.0
                    } else {
                        cx.vm.progress.displayed_or(&id, item.progress_percentage())
                    };
                    for (column, _, _) in COLUMNS {
                        row.col(|ui| paint_cell(ui, column, data, displayed));
                    }
                    let response = row.response();
                    if response.clicked() {
                        clicked_row = Some((id, ui_modifiers(&response), false, false));
                    }
                    if response.double_clicked() {
                        clicked_row = Some((id, Modifiers::NONE, true, false));
                    }
                    if response.secondary_clicked() {
                        clicked_row = Some((id, Modifiers::NONE, false, true));
                    }
                    if response.context_menu_opened() || response.secondary_clicked() {
                        context_target = Some(id);
                    }
                    response.context_menu(|ui| self.row_menu(ui, cx));
                });
            });

        if let Some(column) = clicked_header {
            // Ascending, descending, then back to list order.
            self.sort = match self.sort {
                Some((c, true)) if c == column => Some((column, false)),
                Some((c, false)) if c == column => None,
                _ => Some((column, true)),
            };
        }
        if let Some((id, modifiers, double, secondary)) = clicked_row {
            if secondary {
                // Leave an existing multi-selection intact if the click landed inside it.
                if !self.selected.contains(&id) {
                    self.select_only(id);
                }
            } else if modifiers.command {
                if !self.selected.remove(&id) {
                    self.selected.insert(id);
                }
                self.anchor = Some(id);
                self.cursor = Some(id);
            } else if modifiers.shift {
                self.extend_to(visible, id);
            } else {
                self.select_only(id);
            }
            if double {
                self.activate(cx);
            }
        }
        let _ = context_target;

        // A click on the empty space below the rows clears the selection.
        let rest = ui.available_rect_before_wrap();
        if rest.height() > 0.0 {
            let response = ui.interact(rest, ui.scope_id().with("table-empty"), Sense::click());
            if response.clicked() || response.secondary_clicked() {
                self.selected.clear();
            }
        }
    }

    /// The row context menu: every entry that cannot act on the selection is
    /// left out (a right-click menu reads as a list of what you can do here),
    /// and the lifecycle commands say how many downloads they will touch.
    fn row_menu(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let selection: Vec<DownloadItem> = self.selection(cx.vm).into_iter().cloned().collect();
        if selection.is_empty() {
            ui.close();
            return;
        }
        let single = (selection.len() == 1).then(|| &selection[0]);
        let many = selection.len() > 1;
        let count = |f: fn(&DownloadItem) -> bool| selection.iter().filter(|i| f(i)).count();
        let label = |name: &str, n: usize| {
            if many {
                format!("{name} ({n})")
            } else {
                name.to_string()
            }
        };
        let mut group = MenuGroups::default();

        if let Some(item) = single {
            if actions::can_open_file(item, &cx.shell)
                && group.item(ui, Some(Icon::OpenFile), "Open File")
            {
                self.open_selected_file(cx);
            }
            if actions::can_open_folder(item, &cx.shell)
                && group.item(ui, Some(Icon::OpenFolder), "Open Containing Folder")
            {
                self.open_selected_folder(cx);
            }
            if actions::can_verify_checksum(item, &cx.shell)
                && group.item(ui, Some(Icon::CheckCircle), "Verify Checksum...")
            {
                cx.open(DialogRequest::Checksum(item.id));
            }
        }
        group.next();
        if count(actions::can_resume) > 0
            && group.item(
                ui,
                Some(Icon::Resume),
                &label("Resume", count(actions::can_resume)),
            )
        {
            self.resume_selected(cx);
        }
        if count(actions::can_pause) > 0
            && group.item(
                ui,
                Some(Icon::Pause),
                &label("Pause", count(actions::can_pause)),
            )
        {
            self.pause_selected(cx);
        }
        if count(actions::can_stop) > 0
            && group.item(
                ui,
                Some(Icon::Stop),
                &label("Stop", count(actions::can_stop)),
            )
        {
            self.stop_selected(cx);
        }
        if count(actions::can_retry) > 0
            && group.item(
                ui,
                Some(Icon::Retry),
                &label("Retry", count(actions::can_retry)),
            )
        {
            self.retry_selected(cx);
        }
        if let Some(item) = single
            && actions::can_refresh_address(item)
            && group.item(
                ui,
                Some(Icon::RefreshAddress),
                "Refresh Download Address...",
            )
        {
            cx.open(DialogRequest::RefreshAddress(item.id));
        }
        group.next();
        group.separator(ui);
        ui.menu_button(RichText::new("Add to Queue"), |ui| {
            // Rebuilt per opening: a stale queue name here would file a download
            // into the wrong place.
            for queue in cx.vm.queues.clone() {
                if controls::menu_item(
                    ui,
                    Some(Icon::Queue),
                    &format!("{} ({})", queue.name, queue.item_ids.len()),
                    None,
                    true,
                ) {
                    let ids: Vec<Uuid> = selection.iter().map(|i| i.id).collect();
                    cx.services.queues.add_to_queue(queue.id, &ids);
                }
            }
            if !cx.vm.queues.is_empty() {
                ui.separator();
            }
            // Always offered: a submenu that can only say "you have no queues" is a dead end.
            if controls::menu_item(ui, Some(Icon::Queue), "New Queue...", None, true) {
                cx.open(DialogRequest::Queues);
            }
        });
        if selection.iter().any(|i| cx.vm.queue_id_of(i.id).is_some())
            && group.item(ui, Some(Icon::Queue), "Take Out of Queue")
        {
            let ids: Vec<Uuid> = selection.iter().map(|i| i.id).collect();
            cx.services.queues.remove_from_queues(&ids);
        }
        group.next();
        let remove = if many {
            format!("Remove {} Downloads", selection.len())
        } else {
            "Remove".to_string()
        };
        if group.item(ui, Some(Icon::Delete), &remove) {
            self.ask_remove(cx.vm);
        }
        group.next();
        if single.is_some() && group.item(ui, Some(Icon::Properties), "Properties...") {
            self.open_selected_details(cx);
        }
    }

    // ------------------------------------------------------------ actions --

    fn select_only(&mut self, id: Uuid) {
        self.selected.clear();
        self.selected.insert(id);
        self.anchor = Some(id);
        self.cursor = Some(id);
    }

    fn extend_to(&mut self, visible: &[Uuid], id: Uuid) {
        let anchor = self.anchor.unwrap_or(id);
        let (Some(a), Some(b)) = (
            visible.iter().position(|v| *v == anchor),
            visible.iter().position(|v| *v == id),
        ) else {
            self.select_only(id);
            return;
        };
        let (lo, hi) = (a.min(b), a.max(b));
        self.selected = visible[lo..=hi].iter().copied().collect();
        self.cursor = Some(id);
    }

    /// Double-click / Enter: a finished file opens; anything else shows its details.
    fn activate(&mut self, cx: &mut WindowCx<'_>) {
        let Some(item) = self.single(cx.vm).cloned() else {
            return;
        };
        if item.status == DownloadStatus::Completed
            && actions::can_open_file(&item, &cx.shell)
            && !cx.shell.is_demo()
        {
            cx.shell.open_file(&item.full_path());
            return;
        }
        cx.command(AppCommand::OpenDetails(item.id));
    }

    fn open_selected_details(&mut self, cx: &mut WindowCx<'_>) {
        if let Some(item) = self.single(cx.vm) {
            cx.command(AppCommand::OpenDetails(item.id));
        }
    }

    fn open_selected_file(&self, cx: &mut WindowCx<'_>) {
        if let Some(item) = self.single(cx.vm) {
            cx.shell.open_file(&item.full_path());
        }
    }

    fn open_selected_folder(&self, cx: &mut WindowCx<'_>) {
        if let Some(item) = self.single(cx.vm) {
            cx.shell
                .reveal_in_explorer(&item.full_path(), &item.save_folder);
        }
    }

    /// Resumes every selected download that can be — started together, never
    /// one after another (a resume completes when the *download* does).
    fn resume_selected(&self, cx: &mut WindowCx<'_>) {
        let ids: Vec<Uuid> = self
            .selection(cx.vm)
            .into_iter()
            .filter(|i| actions::can_resume(i))
            .map(|i| i.id)
            .collect();
        for id in ids {
            let downloads = cx.services.downloads.clone();
            cx.services.spawn(async move { downloads.resume(id).await });
        }
    }

    fn pause_selected(&self, cx: &mut WindowCx<'_>) {
        for item in self
            .selection(cx.vm)
            .into_iter()
            .filter(|i| actions::can_pause(i))
        {
            cx.services.downloads.pause(item.id);
        }
    }

    fn retry_selected(&self, cx: &mut WindowCx<'_>) {
        let ids: Vec<Uuid> = self
            .selection(cx.vm)
            .into_iter()
            .filter(|i| actions::can_retry(i))
            .map(|i| i.id)
            .collect();
        for id in ids {
            let downloads = cx.services.downloads.clone();
            cx.services.spawn(async move { downloads.retry(id).await });
        }
    }

    /// Stop deletes the chunk files, so unlike Pause it cannot be undone: one row
    /// stays one click; a whole selection's worth asks first.
    fn stop_selected(&mut self, cx: &mut WindowCx<'_>) {
        let ids: Vec<Uuid> = self
            .selection(cx.vm)
            .into_iter()
            .filter(|i| actions::can_stop(i))
            .map(|i| i.id)
            .collect();
        match ids.len() {
            0 => {}
            1 => cx.services.downloads.stop(ids[0]),
            n => {
                self.prompt = Some(
                    Prompt::new(
                        "Stop Downloads",
                        format!(
                            "Stop {n} downloads?\nTheir partially downloaded data is discarded."
                        ),
                        PromptIcon::Warning,
                        Answer::Cancel,
                    )
                    .button("Cancel", Answer::Cancel, ButtonKind::Secondary)
                    .button("Stop", Answer::Stop(ids), ButtonKind::Primary),
                )
            }
        }
    }

    fn refresh_address(&self, cx: &mut WindowCx<'_>) {
        if let Some(item) = self.single(cx.vm)
            && actions::can_refresh_address(item)
        {
            cx.open(DialogRequest::RefreshAddress(item.id));
        }
    }

    fn ask_remove(&mut self, vm: &ViewModel) {
        let items = self.selection(vm);
        if items.is_empty() {
            return;
        }
        let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
        let text = if items.len() == 1 {
            format!(
                "Remove \"{}\" from the list?\nAlso delete the downloaded file?",
                items[0].file_name
            )
        } else {
            format!(
                "Remove {} downloads from the list?\nAlso delete the downloaded files?",
                items.len()
            )
        };
        self.prompt = Some(
            Prompt::new(
                "Remove Download",
                text,
                PromptIcon::Question,
                Answer::Cancel,
            )
            .button("Cancel", Answer::Cancel, ButtonKind::Secondary)
            .button(
                "Keep Files",
                Answer::Remove {
                    ids: ids.clone(),
                    delete_files: false,
                },
                ButtonKind::Secondary,
            )
            .button(
                "Delete Files",
                Answer::Remove {
                    ids,
                    delete_files: true,
                },
                ButtonKind::Primary,
            ),
        );
    }

    fn ask_delete_completed(&mut self, vm: &ViewModel) {
        let ids: Vec<Uuid> = vm
            .items()
            .filter(|i| i.status == DownloadStatus::Completed)
            .map(|i| i.id)
            .collect();
        if ids.is_empty() {
            return;
        }
        self.prompt = Some(
            Prompt::new(
                "Delete Completed",
                format!(
                    "Remove {} completed download(s) from the list?\n(Downloaded files are kept.)",
                    ids.len()
                ),
                PromptIcon::Question,
                Answer::Cancel,
            )
            .button("No", Answer::Cancel, ButtonKind::Secondary)
            .button("Yes", Answer::DeleteCompleted(ids), ButtonKind::Primary),
        );
    }

    fn on_answer(&mut self, answer: Answer, cx: &mut WindowCx<'_>) {
        match answer {
            Answer::Cancel | Answer::Ok => {}
            Answer::Remove { ids, delete_files } => {
                for id in ids {
                    cx.services.downloads.remove(id, delete_files);
                }
            }
            Answer::Stop(ids) => {
                for id in ids {
                    cx.services.downloads.stop(id);
                }
            }
            Answer::DeleteCompleted(ids) => {
                for id in ids {
                    cx.services.downloads.remove(id, false);
                }
            }
        }
    }
}

/// Which commands the selection allows — the one pass that greys the toolbar
/// and the menu bar alike (`UpdateCommandStates`).
struct CommandStates {
    resume: bool,
    pause: bool,
    retry: bool,
    stop_all: bool,
    delete: bool,
    delete_completed: bool,
    properties: bool,
    refresh_address: bool,
    open_file: bool,
    open_folder: bool,
}

impl CommandStates {
    fn new(main: &MainWindow, cx: &WindowCx<'_>) -> Self {
        let selection = main.selection(cx.vm);
        let single = (selection.len() == 1).then(|| selection[0]);
        Self {
            resume: selection.iter().any(|i| actions::can_resume(i)),
            pause: selection.iter().any(|i| actions::can_pause(i)),
            retry: selection.iter().any(|i| actions::can_retry(i)),
            stop_all: cx.vm.items().any(actions::can_pause),
            delete: !selection.is_empty(),
            delete_completed: cx.vm.items().any(|i| i.status == DownloadStatus::Completed),
            properties: single.is_some(),
            refresh_address: single.is_some_and(actions::can_refresh_address),
            open_file: single.is_some_and(|i| actions::can_open_file(i, &cx.shell)),
            open_folder: single.is_some_and(|i| actions::can_open_folder(i, &cx.shell)),
        }
    }
}

/// Lays out menu entries in groups with a separator only *between* groups that
/// have something in them — what `TrimSeparators` did for hidden entries.
#[derive(Default)]
struct MenuGroups {
    pending: bool,
    in_group: bool,
}

impl MenuGroups {
    fn item(&mut self, ui: &mut Ui, icon: Option<Icon>, text: &str) -> bool {
        self.flush(ui);
        self.in_group = true;
        controls::menu_item(ui, icon, text, None, true)
    }
    fn separator(&mut self, ui: &mut Ui) {
        self.flush(ui);
        self.in_group = true;
    }
    fn flush(&mut self, ui: &mut Ui) {
        if self.pending {
            ui.separator();
            self.pending = false;
        }
    }
    fn next(&mut self) {
        if self.in_group {
            self.pending = true;
            self.in_group = false;
        }
    }
}

/// The modifiers held when the click happened — read off the button event
/// itself, so a Ctrl that was let go in the same frame still counts.
fn ui_modifiers(response: &egui::Response) -> Modifiers {
    response.ctx.input(|i| {
        i.events
            .iter()
            .rev()
            .find_map(|e| match e {
                egui::Event::PointerButton {
                    button: egui::PointerButton::Primary,
                    modifiers,
                    ..
                } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or(i.modifiers)
    })
}

fn eta_key(item: &DownloadItem) -> i64 {
    match item.estimated_time_remaining {
        Some(t) if item.status == DownloadStatus::Downloading => t.ticks(),
        _ => i64::MAX,
    }
}

// --------------------------------------------------------------------------
// Cells
// --------------------------------------------------------------------------

/// The status column's colour (`MainForm.StatusColor`).
pub fn status_color(status: DownloadStatus) -> Color32 {
    let p = theme::palette();
    match status {
        DownloadStatus::Completed => p.success,
        DownloadStatus::Failed | DownloadStatus::Cancelled => p.danger,
        DownloadStatus::Paused => p.warning,
        DownloadStatus::Downloading
        | DownloadStatus::Connecting
        | DownloadStatus::Merging
        | DownloadStatus::Verifying => p.accent,
        DownloadStatus::Queued => p.text_muted,
    }
}

/// The status column's text (`MainForm.StatusText`).
pub fn status_text(item: &DownloadItem) -> String {
    match item.status {
        DownloadStatus::Merging => format!(
            "Merging {}",
            format_percentage(item.merge_progress_percentage)
        ),
        DownloadStatus::Verifying => format!(
            "Verifying {}",
            format_percentage(item.merge_progress_percentage)
        ),
        DownloadStatus::Completed => "Complete".into(),
        s => s.name().to_string(),
    }
}

/// The description column: the error of a failed download, the completion time
/// (and checksum verdict) of a finished one, the stream quality, or the type.
pub fn description_for(item: &DownloadItem) -> String {
    if item.status == DownloadStatus::Failed
        && let Some(error) = item
            .error_message
            .as_deref()
            .filter(|e| !e.trim().is_empty())
    {
        return error.to_string();
    }
    if item.status == DownloadStatus::Completed
        && let Some(done) = item.completed_at
    {
        let when = format_short_date_time(done);
        return match item.checksum_result {
            ChecksumResult::Match
                if actions::checksum_name(item.expected_checksum.as_deref()).is_some() =>
            {
                format!(
                    "✓ {} verified · {when}",
                    actions::checksum_name(item.expected_checksum.as_deref()).unwrap_or_default()
                )
            }
            ChecksumResult::Mismatch => format!("✗ Checksum does not match · {when}"),
            _ => when,
        };
    }
    if let Some(stream) = &item.stream {
        return if stream.quality.is_empty() {
            "Streaming video".into()
        } else {
            format!("Streaming video · {}", stream.quality)
        };
    }
    if item.content_type.is_empty() {
        "—".into()
    } else {
        item.content_type.clone()
    }
}

/// .NET's "g" format in en-US: `10/30/2026 2:50 PM`, in local time.
pub fn format_short_date_time(at: quickbyte_core::serde_compat::DateTimeOffset) -> String {
    at.0.with_timezone(&chrono::Local)
        .format("%-m/%-d/%Y %-I:%M %p")
        .to_string()
}

fn paint_cell(ui: &mut Ui, column: ColumnId, row: &DownloadRow, displayed: f64) {
    let p = theme::palette();
    let item = &row.item;
    let rect = ui.max_rect();
    let selected_text = ui.visuals().override_text_color;
    let color = |preferred: Color32| selected_text.unwrap_or(preferred);
    let live = matches!(
        item.status,
        DownloadStatus::Downloading | DownloadStatus::Connecting
    );
    let painter = ui.painter();

    // Single line, ending in an ellipsis when the column is too narrow.
    let text_cell = |text: &str, font: egui::FontId, c: Color32, left: f32| {
        let clip = rect.shrink2(vec2(3.0, 0.0));
        let x = rect.left() + left;
        let mut job = egui::text::LayoutJob::single_section(
            text.to_string(),
            egui::TextFormat::simple(font, c),
        );
        job.wrap = egui::text::TextWrapping::truncate_at_width((clip.right() - x).max(1.0));
        let galley = painter.layout_job(job);
        let pos = egui::pos2(x, rect.center().y - galley.size().y / 2.0);
        painter
            .with_clip_rect(clip.intersect(painter.clip_rect()))
            .galley(pos, galley, c);
    };

    match column {
        ColumnId::Name => {
            let icon_rect = Rect::from_min_size(
                egui::pos2(rect.left() + 6.0, rect.center().y - 8.0),
                vec2(16.0, 16.0),
            );
            icons::paint(
                painter,
                icon_rect,
                Icon::Category(get_category(&item.file_name)),
            );
            text_cell(&item.file_name, theme::body(), color(p.text), 28.0);
        }
        ColumnId::Size => {
            let size = if item.total_bytes > 0 {
                format_bytes(item.total_bytes)
            } else {
                "Unknown".into()
            };
            text_cell(&size, theme::body(), color(p.text_muted), 7.0);
        }
        ColumnId::Progress => {
            let track = rect.shrink2(vec2(5.0, 3.0));
            let fill = progress_bar::color_for_status(item.status);
            progress_bar::paint(
                painter,
                track,
                displayed,
                fill,
                None,
                theme::small_strong(),
                p.text,
            );
        }
        ColumnId::Speed => {
            // Meaningless for anything not transferring (a persisted row would
            // otherwise show the rate it finished at), and during a merge.
            let text = if live
                && row.merge_percentage.is_none()
                && item.current_speed_bytes_per_second > 0.0
            {
                format_speed(item.current_speed_bytes_per_second)
            } else {
                "—".into()
            };
            text_cell(&text, theme::body(), color(p.text_muted), 7.0);
        }
        ColumnId::Eta => {
            let text = if live && row.merge_percentage.is_none() {
                format_eta(item.estimated_time_remaining)
            } else {
                "—".into()
            };
            text_cell(&text, theme::body(), color(p.text_muted), 7.0);
        }
        ColumnId::Status => {
            text_cell(
                &status_text(item),
                theme::body_strong(),
                color(status_color(item.status)),
                7.0,
            );
        }
        ColumnId::Description => {
            let preferred = if item.status == DownloadStatus::Failed {
                p.danger
            } else {
                p.text_muted
            };
            text_cell(
                &description_for(item),
                theme::small(),
                color(preferred),
                7.0,
            );
        }
    }
}
