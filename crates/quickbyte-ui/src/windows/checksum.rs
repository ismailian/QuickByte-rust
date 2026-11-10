//! Verify Checksum — port of `Forms/ChecksumForm.cs`.
//!
//! Checks a finished download against a checksum given after the fact — the
//! publisher's page is often read only once the file is on disk. The check
//! runs through `DownloadManager::verify_checksum`, so its result is kept with
//! the download and shown in the list, and it can be cancelled: hashing a 40 GB
//! file takes a while.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use egui::{Align, Color32, Layout, Margin, RichText, TextEdit, Ui, ViewportBuilder, vec2};
use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::file_checksum::{self, ChecksumResult};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{AppWindow, WindowCx, WindowKind};
use crate::icons::Icon;
use crate::services::Pending;
use crate::shell::Waker;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::animator::Animator;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::{form, progress_bar};

const LABEL_WIDTH: f32 = 84.0;

pub struct ChecksumWindow {
    id: Uuid,
    file_name: String,
    total_bytes: i64,
    checksum: String,
    status: String,
    color: Option<Color32>,
    running: Option<(
        CancellationToken,
        Pending<quickbyte_core::Result<ChecksumResult>>,
    )>,
    read: Arc<AtomicI64>,
    bar: Animator<u8>,
    bar_color: Option<Color32>,
    show_bar: bool,
}

impl ChecksumWindow {
    pub fn new(id: Uuid, vm: &ViewModel, clipboard: Option<String>) -> Self {
        let item = vm.item(id);
        // A checksum the download was added with is the one worth checking again.
        let checksum =
            match file_checksum::from_stored(item.and_then(|i| i.expected_checksum.as_deref())) {
                Some(stored) => stored.hex,
                None => clipboard
                    .and_then(|text| file_checksum::parse(Some(&text)))
                    .map(|c| c.hex)
                    .unwrap_or_default(),
            };
        Self {
            id,
            file_name: item.map(|i| i.file_name.clone()).unwrap_or_default(),
            total_bytes: item.map(|i| i.total_bytes).unwrap_or(0),
            checksum,
            status:
                "Paste the checksum from the download page. The file is read once, start to finish."
                    .into(),
            color: None,
            running: None,
            read: Arc::new(AtomicI64::new(0)),
            bar: Animator::percentages(),
            bar_color: None,
            show_bar: false,
        }
    }

    fn set_status(&mut self, text: impl Into<String>, color: Option<Color32>) {
        self.status = text.into();
        self.color = color;
    }

    fn verify(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        if let Some((cancel, _)) = &self.running {
            cancel.cancel();
            return;
        }
        let p = theme::palette();
        let Some(expected) = file_checksum::parse(Some(&self.checksum)) else {
            self.set_status(
                "That is not a checksum QuickByte recognises. Paste the hex value from the download page.",
                Some(p.danger),
            );
            return;
        };
        self.show_bar = true;
        self.bar_color = None;
        self.bar.set_target(0, 0.0, true);
        self.read.store(0, Ordering::Relaxed);
        self.set_status(
            format!("Checking the {}…", file_checksum::name(expected.algorithm)),
            None,
        );
        let cancel = CancellationToken::new();
        let wake = Waker::new(ui.ctx(), cx.viewport);
        let (read, progress_wake) = (self.read.clone(), wake.clone());
        let progress: quickbyte_core::traits::ProgressFn<i64> = Arc::new(move |bytes| {
            read.store(bytes, Ordering::Relaxed);
            progress_wake.wake();
        });
        let downloads = cx.services.downloads.clone();
        let (id, token) = (self.id, cancel.clone());
        let pending = cx.services.run_waking(wake, async move {
            downloads
                .verify_checksum(id, expected, Some(progress), token)
                .await
        });
        self.running = Some((cancel, pending));
    }

    fn on_result(&mut self, result: quickbyte_core::Result<ChecksumResult>) {
        let p = theme::palette();
        let Some(expected) = file_checksum::parse(Some(&self.checksum)) else {
            return;
        };
        let name = file_checksum::name(expected.algorithm);
        match result {
            Ok(ChecksumResult::Match) => {
                self.bar.set_target(0, 100.0, true);
                self.bar_color = Some(p.success);
                let text = if file_checksum::is_weak(expected.algorithm) {
                    format!(
                        "The {name} matches — the file arrived intact. This hash cannot rule out a file forged to match it."
                    )
                } else {
                    format!("The {name} matches. This is the file that was published.")
                };
                self.set_status(text, Some(p.success));
            }
            Ok(ChecksumResult::Mismatch) => {
                self.bar.set_target(0, 100.0, true);
                self.bar_color = Some(p.danger);
                self.set_status(
                    format!(
                        "The {name} does not match. The file is damaged or not the one published — don't open it."
                    ),
                    Some(p.danger),
                );
            }
            Ok(ChecksumResult::NotChecked) => {
                self.bar.set_target(0, 100.0, true);
                self.set_status("The file is no longer where it was saved.", Some(p.danger));
            }
            Err(CoreError::Cancelled) => self.set_status("Cancelled.", None),
            Err(e) => self.set_status(format!("Could not read the file: {e}"), Some(p.danger)),
        }
    }
}

impl AppWindow for ChecksumWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Checksum(self.id)
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Verify Checksum")
            .with_inner_size(vec2(540.0, 214.0))
            .with_resizable(false)
            .with_maximize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Verify Checksum".into()
    }

    fn download(&self) -> Option<Uuid> {
        Some(self.id)
    }

    fn is_animating(&self) -> bool {
        self.bar.is_animating()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if let Some((_, pending)) = &mut self.running {
            if let Some(result) = pending.poll() {
                self.running = None;
                self.on_result(result);
            } else {
                let size = self.total_bytes.max(1) as f64;
                let read = self.read.load(Ordering::Relaxed) as f64;
                self.bar
                    .set_target(0, (read * 100.0 / size).min(100.0), false);
            }
        }
        self.bar
            .advance_by(Duration::from_secs_f32(cx.dt.clamp(0.0, 0.1)));

        form::header(
            ui,
            "sum-header",
            Icon::CheckCircle,
            "Verify Checksum",
            &self.file_name,
        );
        let running = self.running.is_some();
        let mut verify = false;
        let mut close = false;
        controls::footer(ui, "sum-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                verify |= controls::button(
                    ui,
                    if running { "Cancel" } else { "Verify" },
                    ButtonKind::Primary,
                    true,
                )
                .clicked();
                close |= controls::button(ui, "Close", ButtonKind::Secondary, true).clicked();
            });
        });
        form::body(
            ui,
            Margin {
                left: 16,
                right: 16,
                top: 12,
                bottom: 8,
            },
            |ui| {
                let width = form::field_width(ui, LABEL_WIDTH);
                form::row(ui, LABEL_WIDTH, "Checksum", |ui| {
                    ui.add_enabled_ui(!running, |ui| {
                        form::text_with(
                            ui,
                            TextEdit::singleline(&mut self.checksum)
                                .font(egui::FontId::monospace(theme::BODY))
                                .hint_text(
                                    RichText::new("SHA-256, SHA-512, SHA-1 or MD5")
                                        .color(p.text_disabled),
                                ),
                            width,
                        );
                    });
                });
                ui.horizontal(|ui| {
                    ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                    ui.allocate_ui_with_layout(
                        vec2(width - 6.0, 32.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            form::note_colored(
                                ui,
                                &self.status,
                                self.color.unwrap_or(p.text_muted),
                            );
                        },
                    );
                });
                if self.show_bar {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(LABEL_WIDTH + ui.spacing().item_spacing.x);
                        ui.allocate_ui_with_layout(
                            vec2(width - 6.0, 18.0),
                            Layout::left_to_right(Align::Center),
                            |ui| {
                                progress_bar::show(
                                    ui,
                                    18.0,
                                    self.bar.displayed(&0),
                                    self.bar_color.unwrap_or(p.accent),
                                    None,
                                );
                            },
                        );
                    });
                }
            },
        );

        if form::enter_pressed(ui) {
            verify = true;
        }
        if form::escape_pressed(ui) {
            close = true;
        }
        if verify {
            self.verify(ui, cx);
        }
        if close {
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        if let Some((cancel, _)) = self.running.take() {
            cancel.cancel();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
