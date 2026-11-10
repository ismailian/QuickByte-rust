//! Update Available — port of `Forms/UpdateForm.cs`.
//!
//! One window for both ways an update is found. The difference is a single
//! question — does the download start on its own?
//!
//! * **Prompt** (the silent startup check found something): nothing is
//!   fetched until the user clicks Update Now. A download manager quietly
//!   spending the user's bandwidth on itself at every launch is the thing this
//!   feature must not be.
//! * **Automatic** (Help > Check for Updates): the user asked, so the download
//!   starts as the window opens and setup runs the moment it lands.
//!
//! The installer is verified by the update service; once setup has started,
//! QuickByte exits (`AppCommand::Exit`) — setup cannot replace files the
//! running process holds open. Closing the window cancels a download in flight.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use egui::{Align, Layout, Margin, RichText, Ui, ViewportBuilder, vec2};
use parking_lot::Mutex;
use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::byte_formatter::{format_bytes, format_speed};
use quickbyte_core::models::{UpdateDownloadProgress, UpdateManifest};
use tokio_util::sync::CancellationToken;

use super::{AppCommand, AppWindow, WindowCx, WindowKind};
use crate::icons::Icon;
use crate::services::Pending;
use crate::shell::Waker;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::animator::Animator;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{form, progress_bar};

pub struct UpdateWindow {
    manifest: UpdateManifest,
    automatic: bool,
    started: bool,
    download: Option<Pending<quickbyte_core::Result<String>>>,
    cancel: Option<CancellationToken>,
    progress: Arc<Mutex<Option<UpdateDownloadProgress>>>,
    bar: Animator<u8>,
    /// Shown once a download has begun.
    show_progress: bool,
    show_bar: bool,
    overlay: Option<String>,
    status: String,
    status_is_error: bool,
    update_text: &'static str,
    later_text: &'static str,
}

impl UpdateWindow {
    pub fn new(manifest: UpdateManifest, automatic: bool) -> Self {
        Self {
            manifest,
            automatic,
            started: false,
            download: None,
            cancel: None,
            progress: Arc::new(Mutex::new(None)),
            bar: Animator::percentages(),
            show_progress: false,
            show_bar: true,
            overlay: None,
            status: String::new(),
            status_is_error: false,
            update_text: "Update Now",
            later_text: "Later",
        }
    }

    fn has_notes(&self) -> bool {
        self.manifest
            .release_notes
            .as_deref()
            .is_some_and(|n| !n.trim().is_empty())
    }

    /// Version, plus whatever else the manifest said about it. Dates are in
    /// English, as every other string in the window is.
    fn describe_release(&self) -> String {
        let mut text = self.manifest.version.clone();
        if self.manifest.file_size_bytes > 0 {
            text += &format!("  ·  {}", format_bytes(self.manifest.file_size_bytes));
        }
        if let Some(date) = &self.manifest.release_date {
            text += &format!("  ·  released {}", date.local_naive().format("%-d %b %Y"));
        }
        text
    }

    fn start_download(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        if self.download.is_some() {
            return;
        }
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        self.update_text = "Update Now";
        self.later_text = "Cancel";
        self.show_progress = true;
        self.show_bar = true;
        self.overlay = None;
        self.bar.set_target(0, 0.0, true);
        self.status = "Connecting...".into();
        self.status_is_error = false;
        *self.progress.lock() = None;

        let wake = Waker::new(ui.ctx(), cx.viewport);
        let sink = self.progress.clone();
        let progress_wake = wake.clone();
        let report: quickbyte_core::traits::ProgressFn<UpdateDownloadProgress> =
            Arc::new(move |p| {
                *sink.lock() = Some(p);
                progress_wake.wake();
            });
        let updates = cx.services.updates.clone();
        let manifest = self.manifest.clone();
        self.download = Some(cx.services.run_waking(wake, async move {
            updates
                .download_installer(&manifest, Some(report), &cancel)
                .await
        }));
    }

    fn report_progress(&mut self) {
        let Some(progress) = *self.progress.lock() else {
            return;
        };
        match progress.percentage() {
            Some(pct) => {
                self.bar.set_target(0, pct, false);
                self.overlay = None;
                self.status = format!(
                    "Downloading  {} of {}  ·  {}",
                    format_bytes(progress.bytes_received),
                    format_bytes(progress.total_bytes),
                    format_speed(progress.speed_bytes_per_second)
                );
            }
            None => {
                // No Content-Length: the bar carries the byte count instead of
                // pretending to know how far along it is.
                self.overlay = Some(format_bytes(progress.bytes_received));
                self.status = format!(
                    "Downloading  ·  {}",
                    format_speed(progress.speed_bytes_per_second)
                );
            }
        }
    }

    fn reset_after_failure(&mut self, message: String, is_error: bool) {
        self.cancel = None;
        self.overlay = None;
        // A bar frozen part-way is no longer telling the truth, and the message
        // needs the room more than it does.
        self.show_bar = false;
        self.status = message;
        self.status_is_error = is_error;
        self.update_text = "Try Again";
        self.later_text = "Close";
    }

    fn on_downloaded(&mut self, result: quickbyte_core::Result<String>, cx: &mut WindowCx<'_>) {
        match result {
            Err(CoreError::Cancelled) => {
                self.reset_after_failure("Update cancelled.".into(), false)
            }
            Err(e) => self.reset_after_failure(format!("Download failed: {e}"), true),
            Ok(path) => {
                self.bar.set_target(0, 100.0, true);
                self.overlay = None;
                self.status = "Starting installer...".into();
                if !cx.shell.run_installer(&path) {
                    self.reset_after_failure(
                        "The installer could not be started. It may have been blocked or the prompt was dismissed."
                            .into(),
                        true,
                    );
                    return;
                }
                // Setup cannot replace files this process holds open.
                cx.close();
                cx.command(AppCommand::Exit);
            }
        }
    }
}

impl AppWindow for UpdateWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Update
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        // The notes box is the only variable-height thing in here.
        ViewportBuilder::default()
            .with_title("Update Available")
            .with_inner_size(vec2(500.0, if self.has_notes() { 372.0 } else { 262.0 }))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Update Available".into()
    }

    fn is_animating(&self) -> bool {
        self.bar.is_animating()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if !self.started {
            self.started = true;
            if self.automatic {
                self.start_download(ui, cx);
            }
        }
        self.report_progress();
        if let Some(result) = self.download.as_mut().and_then(Pending::poll) {
            self.download = None;
            self.on_downloaded(result, cx);
        }
        self.bar
            .advance_by(Duration::from_secs_f32(cx.dt.clamp(0.0, 0.1)));

        form::header(
            ui,
            "upd-header",
            Icon::Update,
            "Update available",
            &format!(
                "QuickByte {} is available to install",
                self.manifest.version
            ),
        );
        let downloading = self.download.is_some();
        let mut update = false;
        let mut later = false;
        controls::footer(ui, "upd-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                update |= controls::button_sized(
                    ui,
                    self.update_text,
                    ButtonKind::Primary,
                    !downloading,
                    96.0,
                )
                .clicked();
                later |=
                    controls::button(ui, self.later_text, ButtonKind::Secondary, true).clicked();
            });
        });
        form::body(
            ui,
            Margin {
                left: 16,
                right: 16,
                top: 10,
                bottom: 8,
            },
            |ui| {
                form::card(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 3.0;
                    let width = ui.available_width() - 92.0;
                    ui.horizontal(|ui| {
                        form::info_cell(
                            ui,
                            84.0,
                            width,
                            "Installed:",
                            RichText::new(cx.shell.version()).color(p.text),
                        );
                    });
                    ui.horizontal(|ui| {
                        form::info_cell(
                            ui,
                            84.0,
                            width,
                            "New version:",
                            RichText::new(self.describe_release()).color(p.text),
                        );
                    });
                });
                let progress_height = if self.show_progress { 46.0 } else { 0.0 };
                if self.has_notes() {
                    ui.add_space(8.0);
                    form::section_caption(ui, "What's new");
                    let height = (ui.available_height() - progress_height - 4.0).max(40.0);
                    egui::Frame::NONE
                        .fill(p.surface_alt)
                        .stroke(egui::Stroke::new(1.0, p.border_strong))
                        .inner_margin(Margin::symmetric(8, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::vertical()
                                .id_salt("upd-notes")
                                .max_height(height - 14.0)
                                .min_scrolled_height(height - 14.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    let notes = self
                                        .manifest
                                        .release_notes
                                        .as_deref()
                                        .unwrap_or_default()
                                        .replace("\r\n", "\n")
                                        .replace('\r', "\n");
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(notes).font(theme::small()).color(p.text),
                                        )
                                        .wrap(),
                                    );
                                });
                        });
                }
                if self.show_progress {
                    ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                        let color = if self.status_is_error {
                            p.danger
                        } else {
                            p.text_muted
                        };
                        if self.show_bar {
                            form::status_line(ui, &self.status, color);
                            ui.add_space(2.0);
                            progress_bar::show(
                                ui,
                                18.0,
                                self.bar.displayed(&0),
                                p.accent,
                                self.overlay.as_deref(),
                            );
                        } else {
                            // The failure gets the whole panel: the difference between
                            // reading the error and reading half of it.
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&self.status)
                                        .font(theme::small())
                                        .color(color),
                                )
                                .wrap(),
                            );
                        }
                    });
                }
            },
        );

        if form::escape_pressed(ui) {
            later = true;
        }
        if !downloading && ui.memory(|m| m.focused().is_none()) && form::enter_pressed(ui) {
            update = true;
        }
        if update && !downloading {
            self.start_download(ui, cx);
        }
        if later {
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        // Without this the socket keeps pulling an installer nobody is waiting for.
        if let Some(cancel) = self.cancel.take() {
            cancel.cancel();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
