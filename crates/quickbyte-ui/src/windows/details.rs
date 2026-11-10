//! A download's own window — port of `Forms/DownloadDetailsForm.cs`, styled
//! after IDM's: a header with the file and its address, three flat tabs
//! (Download status / Speed Limiter / Options on completion), the field grid,
//! an animated progress bar, a collapsible section with the "start positions
//! and download progress by connections" bar and the connections grid (with a
//! per-connection Pause / Resume / Restart menu), and the same progress on the
//! window's taskbar button.
//!
//! Several can be open at once, one per download. Each reads the shared view
//! model, so every open window shows the numbers the main list shows.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use egui::{
    Align, Align2, Color32, Layout, RichText, Sense, Stroke, Ui, ViewportBuilder, ViewportCommand,
    vec2,
};
use egui_extras::{Column, TableBuilder};
use quickbyte_core::enums::{
    CompletionActions, ConnectionCommand, ConnectionStatus, DownloadStatus, PowerAction,
};
use quickbyte_core::helpers::byte_formatter::{
    BYTES_PER_KILOBYTE, format_bytes, format_eta, format_percentage, format_speed,
};
use quickbyte_core::helpers::file_category::get_category;
use quickbyte_core::models::{ConnectionInfo, DownloadItem};
use uuid::Uuid;

use super::prompt::{Prompt, PromptIcon};
use super::{AppWindow, WindowCx, WindowKind};
use crate::actions;
use crate::icons::{self, Icon};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::animator::Animator;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::segments_bar::SegmentsBar;
use crate::widgets::{flat_tabs, progress_bar};

const WIDTH: f32 = 540.0;
const HEIGHT_EXPANDED: f32 = 476.0;
const HEIGHT_COLLAPSED: f32 = 300.0;

#[derive(Clone, Debug, PartialEq)]
enum Answer {
    Cancel,
    Restart(i32),
}

pub struct DetailsWindow {
    id: Uuid,
    tab: usize,
    expanded: bool,
    segments: SegmentsBar,
    connection_progress: Animator<i32>,
    last_connections: Option<Arc<Vec<ConnectionInfo>>>,
    selected_connection: Option<i32>,
    prompt: Option<Prompt<Answer>>,
    speed_limited: bool,
    speed_kbps: i64,
    last_taskbar: Option<(DownloadStatus, i64)>,
}

impl DetailsWindow {
    pub fn new(id: Uuid, vm: &ViewModel) -> Self {
        // Seeded rather than applied: opening the window must not rewrite (and
        // re-persist) a limit the user has not touched.
        let limit = vm.item(id).map_or(0, |i| i.speed_limit_bytes_per_second);
        Self {
            id,
            tab: 0,
            expanded: true,
            segments: SegmentsBar::new(),
            connection_progress: Animator::percentages(),
            last_connections: None,
            selected_connection: None,
            prompt: None,
            speed_limited: limit > 0,
            speed_kbps: if limit > 0 {
                (limit / BYTES_PER_KILOBYTE).clamp(1, 1_000_000)
            } else {
                500
            },
            last_taskbar: None,
        }
    }

    fn percentage(item: &DownloadItem) -> f64 {
        if item.total_bytes > 0 {
            (item.downloaded_bytes as f64 * 100.0 / item.total_bytes as f64).min(100.0)
        } else {
            0.0
        }
    }

    /// Takes a new connections snapshot (only when the engine sent one).
    fn sync_connections(&mut self, connections: &Arc<Vec<ConnectionInfo>>) {
        if self
            .last_connections
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, connections))
        {
            return;
        }
        for c in connections.iter() {
            self.connection_progress
                .set_target(c.connection_id, c.progress_percentage(), false);
        }
        self.segments.update(connections);
        self.last_connections = Some(connections.clone());
    }
}

/// The details window's status line (`DownloadDetailsForm.StatusText`).
fn status_text(status: DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Connecting => "Connecting…",
        DownloadStatus::Downloading => "Receiving data…",
        DownloadStatus::Merging => "Merging file parts…",
        DownloadStatus::Verifying => "Verifying checksum…",
        DownloadStatus::Paused => "Paused",
        DownloadStatus::Completed => "Complete",
        DownloadStatus::Failed => "Failed",
        DownloadStatus::Cancelled => "Cancelled",
        DownloadStatus::Queued => "Queued",
    }
}

fn status_color(status: DownloadStatus) -> Color32 {
    let p = theme::palette();
    match status {
        DownloadStatus::Completed => p.success,
        DownloadStatus::Failed | DownloadStatus::Cancelled => p.danger,
        DownloadStatus::Paused => p.warning,
        _ => p.accent,
    }
}

fn connection_color(status: ConnectionStatus) -> Color32 {
    let p = theme::palette();
    match status {
        ConnectionStatus::Finished => p.success,
        ConnectionStatus::Failed => p.danger,
        ConnectionStatus::Paused => p.warning,
        ConnectionStatus::Idle => p.text_muted,
        _ => p.accent,
    }
}

fn connection_info(c: &ConnectionInfo) -> String {
    match c.status {
        ConnectionStatus::SendingRequest if c.retry_count > 0 => {
            format!("Retrying (attempt {})…", c.retry_count)
        }
        ConnectionStatus::SendingRequest => "Sending GET…".into(),
        ConnectionStatus::ReceivingData => "Receiving data…".into(),
        ConnectionStatus::Finished => "Finished".into(),
        ConnectionStatus::Failed => {
            format!("Failed — {}", c.last_error.as_deref().unwrap_or("error"))
        }
        ConnectionStatus::Paused => "Paused — right-click to resume".into(),
        ConnectionStatus::Idle => "Idle".into(),
    }
}

impl AppWindow for DetailsWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Details(self.id)
    }

    fn builder(&self, vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title(self.title(vm))
            .with_inner_size(vec2(WIDTH, HEIGHT_EXPANDED))
            .with_min_inner_size(vec2(WIDTH - 40.0, HEIGHT_COLLAPSED))
            .with_maximize_button(false)
    }

    fn title(&self, vm: &ViewModel) -> String {
        match vm.item(self.id) {
            Some(item) if item.status == DownloadStatus::Completed => {
                format!("Complete — {}", item.file_name)
            }
            Some(item) => format!(
                "{}  {}",
                format_percentage(Self::percentage(item)),
                item.file_name
            ),
            None => "QuickByte".into(),
        }
    }

    fn download(&self) -> Option<Uuid> {
        Some(self.id)
    }

    fn is_animating(&self) -> bool {
        self.connection_progress.is_animating() || self.segments.is_animating()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let Some(row) = cx.vm.get(self.id).cloned() else {
            cx.close();
            return;
        };
        let item = &row.item;
        self.sync_connections(&row.connections);
        let elapsed = Duration::from_secs_f32(cx.dt.clamp(0.0, 0.1));
        self.connection_progress.advance_by(elapsed);
        self.segments.advance(elapsed);

        // The same number on this window's taskbar button, so a minimised
        // download still reads at a glance — reported on status changes too (a
        // pause turns it yellow without a byte arriving).
        let pct = Self::percentage(item);
        let key = (item.status, (pct * 10.0) as i64);
        if self.last_taskbar != Some(key) {
            let title = self.title(cx.vm);
            if cx
                .shell
                .taskbar_report(cx.viewport, &title, item.status, item.total_bytes > 0, pct)
            {
                self.last_taskbar = Some(key);
            } else {
                // Its window is not found yet: look again on the next frame.
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }

        let p = theme::palette();
        self.header(ui, item, cx);
        self.footer(ui, item, cx);
        egui::CentralPanel::no_frame()
            .frame(egui::Frame::NONE.fill(p.surface))
            .show(ui, |ui| {
                flat_tabs::show(
                    ui,
                    &["Download status", "Speed Limiter", "Options on completion"],
                    &mut self.tab,
                    28.0,
                );
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        top: 10,
                        bottom: 10,
                    })
                    .show(ui, |ui| match self.tab {
                        0 => self.status_tab(ui, &row, cx),
                        1 => self.speed_tab(ui, item, cx),
                        _ => self.options_tab(ui, item, cx),
                    });
            });

        if let Some(prompt) = &self.prompt
            && let Some(answer) = prompt.show(ui)
        {
            self.prompt = None;
            if let Answer::Restart(connection) = answer {
                cx.services.downloads.control_connection(
                    self.id,
                    connection,
                    ConnectionCommand::Restart,
                );
            }
        }
    }

    fn on_closed(&mut self, cx: &mut WindowCx<'_>) {
        cx.shell.taskbar_forget(cx.viewport);
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl DetailsWindow {
    fn header(&mut self, ui: &mut Ui, item: &DownloadItem, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        egui::Panel::top("header")
            .exact_size(50.0)
            .resizable(false)
            .frame(
                egui::Frame::NONE
                    .fill(p.surface)
                    .inner_margin(egui::Margin {
                        left: 14,
                        right: 14,
                        top: 8,
                        bottom: 6,
                    }),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    icons::show(ui, Icon::Category(get_category(&item.file_name)), 30.0);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        controls::truncated(
                            ui,
                            RichText::new(&item.file_name)
                                .font(theme::heading())
                                .color(p.text),
                        )
                        .on_hover_text(item.full_path());
                        let link = controls::link(ui, &item.url, theme::small())
                            .on_hover_text("Click to copy the download URL");
                        if link.clicked() {
                            ui.ctx().copy_text(item.url.clone());
                        }
                    });
                });
            });
        let _ = cx;
    }

    fn footer(&mut self, ui: &mut Ui, item: &DownloadItem, cx: &mut WindowCx<'_>) {
        let status = item.status;
        let active = matches!(
            status,
            DownloadStatus::Downloading | DownloadStatus::Connecting
        );
        let can_pause_resume = matches!(
            status,
            DownloadStatus::Downloading
                | DownloadStatus::Connecting
                | DownloadStatus::Paused
                | DownloadStatus::Queued
        );
        let can_cancel = matches!(
            status,
            DownloadStatus::Downloading | DownloadStatus::Connecting | DownloadStatus::Paused
        );
        let can_retry = matches!(status, DownloadStatus::Failed | DownloadStatus::Cancelled);
        let can_open_folder =
            status == DownloadStatus::Completed || cx.shell.file_exists(&item.full_path());
        controls::footer(ui, "details-footer", |ui| {
            ui.horizontal_centered(|ui| {
                if controls::button(ui, "Open Folder", ButtonKind::Secondary, can_open_folder)
                    .clicked()
                {
                    cx.shell
                        .reveal_in_explorer(&item.full_path(), &item.save_folder);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if controls::button(
                        ui,
                        if active { "Pause" } else { "Resume" },
                        ButtonKind::Primary,
                        can_pause_resume,
                    )
                    .clicked()
                    {
                        if active {
                            cx.services.downloads.pause(self.id);
                        } else {
                            let (downloads, id) = (cx.services.downloads.clone(), self.id);
                            cx.services.spawn(async move { downloads.resume(id).await });
                        }
                    }
                    if controls::button(ui, "Cancel", ButtonKind::Secondary, can_cancel).clicked() {
                        cx.services.downloads.stop(self.id);
                    }
                    if controls::button(ui, "Retry", ButtonKind::Secondary, can_retry).clicked() {
                        let (downloads, id) = (cx.services.downloads.clone(), self.id);
                        cx.services.spawn(async move { downloads.retry(id).await });
                    }
                    if controls::button(ui, "Close", ButtonKind::Secondary, true).clicked() {
                        cx.close();
                    }
                });
            });
        });
    }

    // ------------------------------------------------------- status tab --

    fn status_tab(
        &mut self,
        ui: &mut Ui,
        row: &crate::view_model::DownloadRow,
        cx: &mut WindowCx<'_>,
    ) {
        let p = theme::palette();
        let item = &row.item;
        let merging = matches!(
            item.status,
            DownloadStatus::Merging | DownloadStatus::Verifying
        );
        let pct = Self::percentage(item);

        egui::Grid::new("fields")
            .num_columns(4)
            .min_col_width(10.0)
            .spacing(vec2(10.0, 5.0))
            .show(ui, |ui| {
                let caption = |ui: &mut Ui, text: &str| {
                    controls::field(
                        ui,
                        80.0,
                        RichText::new(text).font(theme::small()).color(p.text_muted),
                    );
                };
                let value = |ui: &mut Ui, text: RichText| {
                    controls::field(ui, 150.0, text);
                };

                caption(ui, "Status:");
                let status = if item.status == DownloadStatus::Failed {
                    "Failed"
                } else {
                    status_text(item.status)
                };
                let status_label = RichText::new(status)
                    .font(theme::body_strong())
                    .color(status_color(item.status));
                let response = controls::field(ui, 150.0, status_label);
                if let Some(error) = item
                    .error_message
                    .as_deref()
                    .filter(|_| item.status == DownloadStatus::Failed)
                {
                    response.on_hover_text(error);
                }
                caption(ui, "File size:");
                value(
                    ui,
                    RichText::new(if item.total_bytes > 0 {
                        format_bytes(item.total_bytes)
                    } else {
                        "Unknown".into()
                    })
                    .color(p.text),
                );
                ui.end_row();

                caption(ui, "Downloaded:");
                value(
                    ui,
                    RichText::new(format!(
                        "{}  ({})",
                        format_bytes(item.downloaded_bytes),
                        format_percentage(pct)
                    ))
                    .color(p.text),
                );
                caption(ui, "Transfer rate:");
                let speed = if merging || item.current_speed_bytes_per_second <= 0.0 {
                    "—".into()
                } else {
                    format_speed(item.current_speed_bytes_per_second)
                };
                value(ui, RichText::new(speed).color(p.text));
                ui.end_row();

                caption(ui, "Time left:");
                let eta = if merging || !item.status.is_active() {
                    "—".into()
                } else {
                    format_eta(item.estimated_time_remaining)
                };
                value(ui, RichText::new(eta).color(p.text));
                caption(ui, "Connections:");
                value(
                    ui,
                    RichText::new(item.connections_count.to_string()).color(p.text),
                );
                ui.end_row();

                caption(ui, "Resumable:");
                value(
                    ui,
                    RichText::new(if item.supports_resume { "Yes" } else { "No" }).color(
                        if item.supports_resume {
                            p.success
                        } else {
                            p.warning
                        },
                    ),
                );
                ui.end_row();
            });

        ui.add_space(8.0);
        // While merging the bar stays full and reports merge progress as text —
        // the bytes are already on disk, so rewinding the bar would be a lie.
        let overlay = merging.then(|| {
            format!(
                "{}  {}",
                status_text(item.status),
                format_percentage(
                    row.merge_percentage
                        .unwrap_or(item.merge_progress_percentage)
                )
            )
        });
        let displayed = if item.status == DownloadStatus::Completed {
            100.0
        } else {
            cx.vm.progress.displayed_or(&self.id, pct)
        };
        progress_bar::show(
            ui,
            18.0,
            if merging { 100.0 } else { displayed },
            progress_bar::color_for_status(item.status),
            overlay.as_deref(),
        );
        ui.add_space(4.0);

        // The details toggle.
        let label = if self.expanded {
            "Hide details"
        } else {
            "Show details"
        };
        let (rect, response) = ui.allocate_exact_size(vec2(110.0, 20.0), Sense::click());
        if response.hovered() {
            ui.painter()
                .rect_filled(rect, theme::CORNER_RADIUS, p.accent_soft);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let c = egui::pos2(rect.left() + 9.0, rect.center().y);
        let points = if self.expanded {
            vec![c + vec2(-4.0, 2.0), c + vec2(4.0, 2.0), c + vec2(0.0, -2.5)]
        } else {
            vec![
                c + vec2(-4.0, -2.0),
                c + vec2(4.0, -2.0),
                c + vec2(0.0, 2.5),
            ]
        };
        ui.painter()
            .add(egui::Shape::convex_polygon(points, p.accent, Stroke::NONE));
        ui.painter().text(
            egui::pos2(rect.left() + 19.0, rect.center().y),
            Align2::LEFT_CENTER,
            label,
            theme::small(),
            p.accent,
        );
        if response.clicked() {
            self.expanded = !self.expanded;
            let height = if self.expanded {
                HEIGHT_EXPANDED
            } else {
                HEIGHT_COLLAPSED
            };
            let width = ui
                .ctx()
                .input(|i| i.viewport().inner_rect)
                .map_or(WIDTH, |r| r.width());
            ui.ctx()
                .send_viewport_cmd(ViewportCommand::InnerSize(vec2(width, height)));
        }

        if !self.expanded {
            return;
        }
        ui.add_space(4.0);
        controls::caption(ui, "Start positions and download progress by connections");
        self.segments
            .show(ui, 16.0, &row.connections, item.total_bytes);
        ui.add_space(8.0);
        self.connections_table(ui, item, &row.connections, cx);
    }

    fn connections_table(
        &mut self,
        ui: &mut Ui,
        item: &DownloadItem,
        connections: &[ConnectionInfo],
        cx: &mut WindowCx<'_>,
    ) {
        let p = theme::palette();
        let mut ordered: Vec<&ConnectionInfo> = connections.iter().collect();
        ordered.sort_by_key(|c| c.connection_id);
        let accepts = actions::accepts_connection_commands(item);

        egui::Frame::NONE.stroke(Stroke::new(1.0, p.border)).inner_margin(1).show(ui, |ui| {
            let focused = ui.ctx().input(|i| i.focused);
            {
                let v = ui.visuals_mut();
                v.faint_bg_color = p.row_alt;
                v.selection.bg_fill = if focused { p.row_selected } else { p.row_selected_inactive };
                v.selection.stroke.color = if focused { p.text_on_accent } else { p.text };
                v.widgets.hovered.bg_fill = p.accent_soft;
                ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
            }
            let mut clicked = None;
            TableBuilder::new(ui)
                .id_salt("connections")
                .striped(true)
                .sense(Sense::click())
                .auto_shrink([false, false])
                .cell_layout(Layout::left_to_right(Align::Center))
                .column(Column::exact(34.0))
                .column(Column::initial(92.0).at_least(60.0).clip(true))
                .column(Column::initial(170.0).at_least(80.0).clip(true))
                .column(Column::remainder().clip(true))
                .header(20.0, |mut header| {
                    for (i, title) in ["#", "Downloaded", "Progress", "Info"].into_iter().enumerate() {
                        header.col(|ui| {
                            let rect = ui.max_rect();
                            ui.painter().rect_filled(rect, 0.0, p.header_back);
                            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
                            if i < 3 {
                                ui.painter().vline(rect.right() - 0.5, rect.y_range().shrink(4.0), Stroke::new(1.0, p.border));
                            }
                            let (anchor, x) = if i == 1 { (Align2::RIGHT_CENTER, rect.right() - 8.0) } else { (Align2::LEFT_CENTER, rect.left() + 8.0) };
                            ui.painter().text(egui::pos2(x, rect.center().y), anchor, title, theme::body_strong(), p.text);
                        });
                    }
                })
                .body(|body| {
                    body.rows(theme::ROW_HEIGHT, ordered.len(), |mut row| {
                        let c = ordered[row.index()];
                        row.set_selected(self.selected_connection == Some(c.connection_id));
                        let displayed = self.connection_progress.displayed_or(&c.connection_id, c.progress_percentage());
                        row.col(|ui| {
                            let color = ui.visuals().override_text_color.unwrap_or(p.text_muted);
                            let r = ui.max_rect();
                            ui.painter().text(r.left_center() + vec2(8.0, 0.0), Align2::LEFT_CENTER, (c.connection_id + 1).to_string(), theme::body(), color);
                        });
                        row.col(|ui| {
                            let color = ui.visuals().override_text_color.unwrap_or(p.text);
                            let r = ui.max_rect();
                            ui.painter().text(r.right_center() - vec2(8.0, 0.0), Align2::RIGHT_CENTER, format_bytes(c.bytes_downloaded), theme::body(), color);
                        });
                        row.col(|ui| {
                            let r = ui.max_rect().shrink2(vec2(5.0, 3.0));
                            progress_bar::paint(ui.painter(), r, displayed, progress_bar::color_for_connection(c.status), None, theme::small_strong(), p.text);
                        });
                        row.col(|ui| {
                            let color = ui.visuals().override_text_color.unwrap_or(connection_color(c.status));
                            ui.add_space(8.0);
                            ui.add(egui::Label::new(RichText::new(connection_info(c)).font(theme::small()).color(color)).truncate());
                        });
                        let response = row.response();
                        if response.clicked() || response.secondary_clicked() {
                            clicked = Some(c.connection_id);
                        }
                        // Only a transferring download has connections to command.
                        if accepts {
                            response.context_menu(|ui| {
                                let info = c;
                                let pause = actions::connection_can_pause(info);
                                let resume = actions::connection_can_resume(info);
                                if pause && controls::menu_item(ui, Some(Icon::Pause), "Pause Connection", None, true) {
                                    cx.services.downloads.control_connection(self.id, info.connection_id, ConnectionCommand::Pause);
                                }
                                if resume && controls::menu_item(ui, Some(Icon::Resume), "Resume Connection", None, true) {
                                    cx.services.downloads.control_connection(self.id, info.connection_id, ConnectionCommand::Resume);
                                }
                                if (pause || resume) && actions::connection_can_restart(info) {
                                    ui.separator();
                                }
                                if actions::connection_can_restart(info) && controls::menu_item(ui, Some(Icon::Retry), "Restart Connection", None, true) {
                                    // A restart throws away what this connection has fetched,
                                    // which can be most of a big file on a slow server.
                                    if info.bytes_downloaded > 0 {
                                        self.prompt = Some(
                                            Prompt::new(
                                                "Restart Connection",
                                                format!(
                                                    "Restart connection #{}? The {} it has downloaded will be discarded and fetched again.",
                                                    info.connection_id + 1,
                                                    format_bytes(info.bytes_downloaded)
                                                ),
                                                PromptIcon::Question,
                                                Answer::Cancel,
                                            )
                                            .button("Cancel", Answer::Cancel, ButtonKind::Secondary)
                                            .button("Restart", Answer::Restart(info.connection_id), ButtonKind::Primary),
                                        );
                                    } else {
                                        cx.services.downloads.control_connection(self.id, info.connection_id, ConnectionCommand::Restart);
                                    }
                                }
                            });
                        }
                    });
                });
            if let Some(id) = clicked {
                self.selected_connection = Some(id);
            }
        });
    }

    // ------------------------------------------------- speed limiter tab --

    /// The per-download cap. Pushed straight through with no Apply button: the
    /// limiter is live, and the new rate reaches a transfer already running.
    fn speed_tab(&mut self, ui: &mut Ui, item: &DownloadItem, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        ui.add_space(4.0);
        let mut changed = ui
            .checkbox(
                &mut self.speed_limited,
                "Limit the maximum download speed for this download",
            )
            .changed();
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.add_enabled_ui(self.speed_limited, |ui| {
                ui.label("Maximum speed:");
                changed |= ui
                    .add(
                        egui::DragValue::new(&mut self.speed_kbps)
                            .range(1..=1_000_000)
                            .speed(10.0),
                    )
                    .changed();
                ui.label(RichText::new("KB/s").color(p.text_muted));
            });
        });
        let limit = if self.speed_limited {
            self.speed_kbps * BYTES_PER_KILOBYTE
        } else {
            0
        };
        if changed && limit != item.speed_limit_bytes_per_second {
            cx.services.downloads.set_speed_limit(self.id, limit);
        }
        ui.add_space(14.0);
        let own = if limit > 0 {
            format!("Capped at {}.", format_speed(limit as f64))
        } else {
            "Running at full speed.".into()
        };
        let global = cx.services.downloads.global_speed_limit();
        let summary = if global > 0 {
            format!(
                "{own}  A global limit of {} is also shared across all downloads.",
                format_speed(global as f64)
            )
        } else {
            own
        };
        ui.add(
            egui::Label::new(
                RichText::new(summary)
                    .font(theme::small())
                    .color(if limit > 0 { p.accent } else { p.text_muted }),
            )
            .wrap(),
        );
        ui.add_space(6.0);
        ui.add(
            egui::Label::new(
                RichText::new("Applies immediately, including to a transfer already in progress. The limit is shared by all of this download's connections.")
                    .font(theme::small())
                    .color(p.text_muted),
            )
            .wrap(),
        );
    }

    // ------------------------------------------- options on completion --

    /// Persisted through the manager on every change rather than held here:
    /// this window closes the moment the download completes, which is exactly
    /// when these are read.
    fn options_tab(&mut self, ui: &mut Ui, item: &DownloadItem, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        let mut open_folder = item.on_completion.contains(CompletionActions::OPEN_FOLDER);
        let mut sound = item.on_completion.contains(CompletionActions::PLAY_SOUND);
        let mut power = item.power_action_when_done;
        ui.add_space(4.0);
        let mut changed = ui
            .checkbox(
                &mut open_folder,
                "Open the containing folder when the download completes",
            )
            .changed();
        changed |= ui
            .checkbox(&mut sound, "Play a sound when the download completes")
            .changed();
        ui.add_space(8.0);
        ui.label("When the download completes");
        egui::ComboBox::from_id_salt("power")
            .width(220.0)
            .selected_text(cx.shell.power_label(power))
            .show_ui(ui, |ui| {
                for action in PowerAction::ALL {
                    changed |= ui
                        .selectable_value(&mut power, action, cx.shell.power_label(action))
                        .changed();
                }
            });
        ui.add_space(10.0);
        ui.label(
            RichText::new("Kept with the download, so they apply even if this window is closed.")
                .font(theme::small())
                .color(p.text_muted),
        );
        if changed {
            let actions = CompletionActions::NONE
                .with(CompletionActions::OPEN_FOLDER, open_folder)
                .with(CompletionActions::PLAY_SOUND, sound);
            cx.services
                .downloads
                .set_completion_actions(self.id, actions, power);
        }
    }
}
