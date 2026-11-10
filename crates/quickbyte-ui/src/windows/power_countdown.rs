//! The last chance to stop a power action — port of
//! `Forms/PowerActionCountdownForm.cs`.
//!
//! These actions are set up hours ahead ("shut down when the queue finishes"),
//! and the person who chose them may be back at the keyboard when they fire.
//! So nothing turns a machine off without saying so on screen for long enough
//! to reach it — a minute, or fifteen seconds to exit — with **Cancel** as the
//! default button and the Escape key: a stray Enter must not be what shuts
//! the computer down. Downloads still running are named, since the action
//! pauses them.

use std::any::Any;
use std::time::{Duration, Instant};

use egui::{Align, Layout, Margin, RichText, Ui, ViewportBuilder, vec2};
use quickbyte_core::enums::PowerAction;

use super::{AppCommand, AppWindow, WindowCx, WindowKind};
use crate::icons::Icon;
use crate::shell::Shell;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::form;

pub struct PowerCountdownWindow {
    action: PowerAction,
    reason: String,
    still_running: usize,
    deadline: Option<Instant>,
    seconds: u32,
    focused: bool,
}

impl PowerCountdownWindow {
    /// `reason`: what finished — "Nightly finished", "setup.exe finished downloading".
    pub fn new(action: PowerAction, reason: String, still_running: usize, shell: Shell) -> Self {
        Self {
            action,
            reason,
            still_running,
            deadline: None,
            seconds: shell.power_countdown_seconds(action),
            focused: false,
        }
    }

    fn proceed(&mut self, cx: &mut WindowCx<'_>) {
        cx.command(AppCommand::PerformPowerAction(self.action));
        cx.close();
    }
}

impl AppWindow for PowerCountdownWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::PowerCountdown
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("QuickByte")
            .with_inner_size(vec2(
                460.0,
                if self.still_running > 0 { 196.0 } else { 168.0 },
            ))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
            .with_always_on_top()
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "QuickByte".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + Duration::from_secs(self.seconds as u64));
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            self.proceed(cx);
            return;
        }
        let seconds_left = left.as_secs_f32().ceil() as u64;
        // Wake for the next whole second, not every frame.
        let to_next = left.as_secs_f32() - (seconds_left as f32 - 1.0);
        ui.ctx()
            .request_repaint_after(Duration::from_secs_f32(to_next.max(0.01)));

        form::header(ui, "pwr-header", Icon::Exit, &self.reason, "QuickByte");
        let mut cancel = false;
        let mut now = false;
        controls::footer(ui, "pwr-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let c = controls::button_sized(ui, "Cancel", ButtonKind::Primary, true, 88.0);
                if !self.focused {
                    self.focused = true;
                    c.request_focus();
                }
                cancel |= c.clicked();
                now |= controls::button_sized(
                    ui,
                    cx.shell.power_now_button(self.action),
                    ButtonKind::Secondary,
                    true,
                    120.0,
                )
                .clicked();
            });
        });
        form::body(
            ui,
            Margin {
                left: 18,
                right: 18,
                top: 12,
                bottom: 8,
            },
            |ui| {
                ui.label(
                    RichText::new(format!(
                        "QuickByte will {} in {seconds_left} second{}.",
                        cx.shell.power_verb(self.action),
                        if seconds_left == 1 { "" } else { "s" }
                    ))
                    .color(p.text),
                );
                if self.still_running > 0 {
                    ui.add_space(6.0);
                    form::note_colored(
                        ui,
                        &if self.still_running == 1 {
                            "1 other download is still running. It will be paused and can be resumed later.".to_string()
                        } else {
                            format!(
                                "{} other downloads are still running. They will be paused and can be resumed later.",
                                self.still_running
                            )
                        },
                        p.warning,
                    );
                }
            },
        );
        // Enter and Escape both mean Cancel.
        if form::enter_pressed(ui) || form::escape_pressed(ui) {
            cancel = true;
        }
        if now {
            self.proceed(cx);
        } else if cancel {
            cx.close();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
