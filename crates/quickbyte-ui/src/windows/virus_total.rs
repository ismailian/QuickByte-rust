//! The question the VirusTotal check asks when a download comes back malicious
//! or suspicious and Options says to ask: delete it, or carry on with it.
//!
//! A window rather than a balloon: the download is held until it is answered,
//! and the answer can delete a file — not a choice to make from a notification
//! that vanishes after a few seconds. Closing it without answering leaves the
//! download as it is (paused, if it was running); the question comes back the
//! next time QuickByte starts.

use std::any::Any;

use egui::{Align, Layout, RichText, Stroke, Ui, ViewportBuilder, vec2};
use quickbyte_core::enums::DownloadStatus;
use quickbyte_core::events::VirusTotalNotice;
use quickbyte_core::models::{VirusTotalSource, VirusTotalVerdict};
use uuid::Uuid;

use super::{AppWindow, WindowCx, WindowKind};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};

pub struct VirusTotalWindow {
    notice: VirusTotalNotice,
}

impl VirusTotalWindow {
    pub fn new(notice: VirusTotalNotice) -> Self {
        Self { notice }
    }

    fn id(&self) -> Uuid {
        self.notice.download_id
    }
}

impl AppWindow for VirusTotalWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::VirusTotal(self.id())
    }

    fn builder(&self, vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title(self.title(vm))
            .with_inner_size(vec2(480.0, 268.0))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
            // It is holding a download up: it should not open behind the window
            // the user happens to be in.
            .with_always_on_top()
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "VirusTotal Warning".into()
    }

    fn download(&self) -> Option<Uuid> {
        Some(self.id())
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        // Removed from the list some other way: nothing left to decide.
        let Some(item) = cx.vm.item(self.id()).cloned() else {
            cx.close();
            return;
        };
        let p = theme::palette();
        // The live report when there is one (it may have moved on), else the one asked about.
        let report = item
            .virus_total
            .clone()
            .filter(|r| r.verdict.is_negative())
            .unwrap_or_else(|| self.notice.report.clone());
        let finished = item.status == DownloadStatus::Completed;
        let malicious = report.verdict == VirusTotalVerdict::Malicious;

        let mut answer = None;
        controls::footer(ui, "vt-footer", |ui| {
            ui.horizontal_centered(|ui| {
                if let Some(link) = report.permalink.clone()
                    && controls::link(ui, "View the report on VirusTotal", theme::body())
                        .on_hover_text(&link)
                        .clicked()
                {
                    cx.shell.open_web_page(&link);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    // The safe choice is the prominent one.
                    let delete = if finished { "Delete File" } else { "Delete Download" };
                    if controls::button(ui, delete, ButtonKind::Primary, true).clicked() {
                        answer = Some(false);
                    }
                    let keep = if finished { "Keep File" } else { "Continue Download" };
                    if controls::button(ui, keep, ButtonKind::Secondary, true).clicked() {
                        answer = Some(true);
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
                        let color = if malicious { p.danger } else { p.warning };
                        let (rect, _) =
                            ui.allocate_exact_size(vec2(30.0, 30.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 14.0, color);
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "!",
                            theme::heading(),
                            p.text_on_accent,
                        );
                        ui.add_space(6.0);
                        let heading = if malicious {
                            "VirusTotal flagged this download as malicious"
                        } else {
                            "VirusTotal flagged this download as suspicious"
                        };
                        ui.label(RichText::new(heading).font(theme::title()).color(p.text));
                    });
                    ui.add_space(6.0);
                    // The sentence that matters most: never cut short.
                    ui.add(
                        egui::Label::new(
                            RichText::new(report.summary())
                                .font(theme::body_strong())
                                .color(if malicious { p.danger } else { p.warning }),
                        )
                        .wrap(),
                    );
                    ui.add_space(8.0);
                    egui::Frame::NONE
                        .fill(p.header_back)
                        .stroke(Stroke::new(1.0, p.border))
                        .inner_margin(egui::Margin::symmetric(12, 8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::Grid::new("vt-grid")
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
                                    caption(ui, "Checked:");
                                    let what = match report.source {
                                        Some(VirusTotalSource::Url) => "The address it downloads from",
                                        _ => "The downloaded file",
                                    };
                                    controls::field(ui, width, RichText::new(what).color(p.text));
                                    ui.end_row();
                                });
                        });
                    ui.add_space(10.0);
                    let advice = if finished {
                        "The file is complete. Delete it, or keep it if you trust where it came from."
                    } else if item.status == DownloadStatus::Paused {
                        "QuickByte paused the download. Delete it, or continue if you trust where it came from."
                    } else {
                        "Delete the download, or continue if you trust where it came from."
                    };
                    ui.add(egui::Label::new(RichText::new(advice).color(p.text)).wrap());
                });
        });

        if let Some(keep) = answer {
            cx.services.virus_total.resolve(self.id(), keep);
            cx.close();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
