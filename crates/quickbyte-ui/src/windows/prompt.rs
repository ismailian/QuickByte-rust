//! The in-window message box (`MessageBox.Show` with Yes/No/Cancel): a modal
//! over the window that asked, so the question stays with what it is about.

use egui::{Id, Modal, RichText, Ui, vec2};

use crate::theme;
use crate::widgets::controls::{self, ButtonKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptIcon {
    Question,
    Warning,
    Info,
}

/// A question with its buttons. `A` is what each button means to the caller.
#[derive(Clone, Debug)]
pub struct Prompt<A> {
    pub title: String,
    pub text: String,
    pub icon: PromptIcon,
    /// Left to right; the last one is the default (Enter).
    pub buttons: Vec<(String, A, ButtonKind)>,
    /// What Esc or clicking outside means.
    pub cancel: A,
}

impl<A: Clone> Prompt<A> {
    pub fn new(title: &str, text: impl Into<String>, icon: PromptIcon, cancel: A) -> Self {
        Self {
            title: title.into(),
            text: text.into(),
            icon,
            buttons: Vec::new(),
            cancel,
        }
    }

    pub fn button(mut self, label: &str, answer: A, kind: ButtonKind) -> Self {
        self.buttons.push((label.into(), answer, kind));
        self
    }

    /// Draws the modal; returns the answer once one is given.
    pub fn show(&self, ui: &Ui) -> Option<A> {
        let p = theme::palette();
        let mut answer = None;
        let response = Modal::new(Id::unique(("quickbyte-prompt", &self.title)))
            .frame(
                egui::Frame::popup(ui.style())
                    .fill(p.surface)
                    .inner_margin(egui::Margin::same(14)),
            )
            .show(ui.ctx(), |ui| {
                ui.set_max_width(360.0);
                ui.horizontal(|ui| {
                    let (color, glyph) = match self.icon {
                        PromptIcon::Question => (p.accent, "?"),
                        PromptIcon::Warning => (p.warning, "!"),
                        PromptIcon::Info => (p.accent, "i"),
                    };
                    let (rect, _) = ui.allocate_exact_size(vec2(28.0, 28.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 13.0, color);
                    ui.painter().text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        glyph,
                        theme::heading(),
                        p.text_on_accent,
                    );
                    ui.add_space(4.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(&self.title)
                                .font(theme::heading())
                                .color(p.text),
                        );
                        ui.add_space(2.0);
                        ui.add(egui::Label::new(RichText::new(&self.text).color(p.text)).wrap());
                    });
                });
                ui.add_space(12.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (index, (label, value, kind)) in self.buttons.iter().enumerate().rev() {
                        let response = controls::button(ui, label, *kind, true);
                        if index == self.buttons.len() - 1 && ui.memory(|m| m.focused().is_none()) {
                            response.request_focus();
                        }
                        if response.clicked() {
                            answer = Some(value.clone());
                        }
                    }
                });
            });
        if answer.is_none() && response.should_close() {
            answer = Some(self.cancel.clone());
        }
        answer
    }
}
