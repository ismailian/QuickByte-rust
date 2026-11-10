//! The window IDM pops when a transfer finishes — port of
//! `Forms/DownloadCompleteForm.cs`: a green tick, the file's final stats, and
//! the two things anyone actually wants next, open the file or show it in
//! Explorer. It replaces the details window (which closes), so one download
//! never leaves two windows describing it.
//!
//! "Show this window next time" writes straight through the settings service,
//! so opting out here is permanent.

use std::any::Any;

use egui::{Align, Layout, RichText, Stroke, Ui, ViewportBuilder, vec2};
use quickbyte_core::enums::DownloadStatus;
use quickbyte_core::helpers::byte_formatter::format_bytes;
use quickbyte_core::helpers::file_checksum::ChecksumResult;
use uuid::Uuid;

use super::{AppWindow, DialogRequest, WindowCx, WindowKind};
use crate::actions;
use crate::icons::{self, Icon};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};

pub struct CompleteWindow {
    id: Uuid,
}

impl CompleteWindow {
    pub fn new(id: Uuid) -> Self {
        Self { id }
    }
}

impl AppWindow for CompleteWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Complete(self.id)
    }

    fn builder(&self, vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title(self.title(vm))
            .with_inner_size(vec2(460.0, 246.0))
            .with_resizable(false)
            .with_maximize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Download Complete".into()
    }

    fn download(&self) -> Option<Uuid> {
        Some(self.id)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let Some(item) = cx.vm.item(self.id).cloned() else {
            cx.close();
            return;
        };
        let p = theme::palette();
        let can_open =
            item.status == DownloadStatus::Completed && actions::can_open_file(&item, &cx.shell);

        controls::footer(ui, "complete-footer", |ui| {
            ui.horizontal_centered(|ui| {
                let mut show = cx.vm.settings.show_completion_window;
                if ui
                    .checkbox(
                        &mut show,
                        RichText::new("Show this window next time")
                            .font(theme::small())
                            .color(p.text_muted),
                    )
                    .changed()
                    && show != cx.vm.settings.show_completion_window
                {
                    let mut settings = cx.services.settings.create_copy();
                    settings.show_completion_window = show;
                    cx.services.settings.save(settings);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let open = controls::button(ui, "Open File", ButtonKind::Primary, can_open);
                    if open.clicked() {
                        cx.shell.open_file(&item.full_path());
                        cx.close();
                    }
                    if controls::button(ui, "Open Folder", ButtonKind::Secondary, true).clicked() {
                        cx.shell
                            .reveal_in_explorer(&item.full_path(), &item.save_folder);
                        cx.close();
                    }
                    if controls::button(ui, "Close", ButtonKind::Secondary, true).clicked() {
                        cx.close();
                    }
                });
            });
        });

        controls::body(ui, 0, |ui| {
            egui::Frame::NONE
                .inner_margin(egui::Margin {
                    left: 18,
                    right: 18,
                    top: 14,
                    bottom: 10,
                })
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        icons::show(ui, Icon::CheckCircle, 34.0);
                        ui.add_space(8.0);
                        ui.label(
                            RichText::new("Download complete")
                                .font(theme::title())
                                .color(p.text),
                        );
                    });
                    ui.add_space(10.0);
                    egui::Frame::NONE
                        .fill(p.header_back)
                        .stroke(Stroke::new(1.0, p.border))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::Grid::new("complete-grid")
                                .num_columns(2)
                                .spacing(vec2(10.0, 5.0))
                                .show(ui, |ui| {
                                    let caption = |ui: &mut Ui, t: &str| {
                                        controls::field(
                                            ui,
                                            62.0,
                                            RichText::new(t)
                                                .font(theme::small())
                                                .color(p.text_muted),
                                        );
                                    };
                                    let width = (ui.available_width() - 72.0).max(200.0);
                                    caption(ui, "File:");
                                    controls::field(
                                        ui,
                                        width,
                                        RichText::new(&item.file_name).color(p.text),
                                    )
                                    .on_hover_text(&item.file_name);
                                    ui.end_row();
                                    caption(ui, "Size:");
                                    controls::field(
                                        ui,
                                        width,
                                        RichText::new(format_bytes(item.total_bytes)).color(p.text),
                                    );
                                    ui.end_row();
                                    caption(ui, "Saved to:");
                                    controls::field(
                                        ui,
                                        width,
                                        RichText::new(&item.save_folder).color(p.text),
                                    )
                                    .on_hover_text(&item.save_folder);
                                    ui.end_row();
                                    caption(ui, "Checksum:");
                                    match (
                                        item.checksum_result,
                                        actions::checksum_name(item.expected_checksum.as_deref()),
                                    ) {
                                        (ChecksumResult::Match, Some(name)) => {
                                            ui.label(
                                                RichText::new(format!("✓ {name} verified"))
                                                    .font(theme::body_strong())
                                                    .color(p.success),
                                            );
                                        }
                                        _ => {
                                            if controls::link(
                                                ui,
                                                "Verify against the publisher's checksum…",
                                                theme::body(),
                                            )
                                            .clicked()
                                            {
                                                cx.open(DialogRequest::Checksum(self.id));
                                            }
                                        }
                                    }
                                    ui.end_row();
                                });
                        });
                });
        });

        // Enter opens the file when it can, otherwise closes; Esc closes.
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            cx.close();
        }
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter)) {
            if can_open {
                cx.shell.open_file(&item.full_path());
            }
            cx.close();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
