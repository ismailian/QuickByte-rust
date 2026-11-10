//! Sign In — port of `Forms/CredentialsForm.cs`.
//!
//! Asks for the user name and password a server has just refused a download
//! for. Shown only in answer to `CoreError::AuthenticationRequired` — never as
//! part of adding a download — and it names the host and repeats what the
//! server said, which is the difference between "this file needs a password"
//! and "that password was wrong".
//!
//! There is no "remember me": a download paused for three days has to present
//! the same login when it resumes, so the credentials are kept either way
//! (DPAPI-encrypted on the item).
//!
//! The window that asked (Add Download, Refresh Address) holds the other end
//! of a channel and greys itself out until an answer arrives; closing this
//! window any way but Sign In answers `None`.

use std::any::Any;
use std::sync::atomic::{AtomicU64, Ordering};

use egui::{Align, Layout, Margin, RichText, TextEdit, Ui, ViewportBuilder, ViewportId, vec2};
use quickbyte_core::models::DownloadCredentials;

use super::{AppWindow, WindowCx, WindowKind};
use crate::icons::Icon;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::form;

const LABEL_WIDTH: f32 = 84.0;

/// The answer: the login typed, or `None` for Cancel.
pub type CredentialsReply = crossbeam_channel::Sender<Option<DownloadCredentials>>;

static NEXT: AtomicU64 = AtomicU64::new(1 << 32);

pub struct CredentialsWindow {
    instance: u64,
    host: String,
    message: String,
    message_is_error: bool,
    user_name: String,
    password: String,
    reply: Option<CredentialsReply>,
    /// The window waiting for the answer; it is woken when one is sent.
    parent: Option<ViewportId>,
    ctx: Option<egui::Context>,
    focused: bool,
}

impl CredentialsWindow {
    /// `url`: the address being refused (only its host is shown). `existing`:
    /// what was already tried, so a rejected password reopens on a filled-in
    /// user name with only the password waiting.
    pub fn new(
        url: &str,
        message: &str,
        existing: Option<&DownloadCredentials>,
        reply: Option<CredentialsReply>,
        parent: Option<ViewportId>,
    ) -> Self {
        Self {
            instance: NEXT.fetch_add(1, Ordering::Relaxed),
            host: host_of(url),
            message: message.to_string(),
            message_is_error: false,
            user_name: existing.map(|c| c.user_name.clone()).unwrap_or_default(),
            password: existing.map(|c| c.password.clone()).unwrap_or_default(),
            reply,
            parent,
            ctx: None,
            focused: false,
        }
    }

    /// For a request that named a host directly (`DialogRequest::Credentials`).
    pub fn for_host(host: &str, message: &str, instance: u64) -> Self {
        let mut w = Self::new(host, message, None, None, None);
        w.instance = instance;
        w
    }

    fn answer(&mut self, credentials: Option<DownloadCredentials>, ctx: &egui::Context) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(credentials);
            if let Some(parent) = self.parent {
                ctx.request_repaint_of(parent);
            }
        }
    }

    fn on_ok(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        let credentials = DownloadCredentials::new(self.user_name.trim(), self.password.clone());
        if credentials.is_empty() {
            // Nothing typed. Saying so beats closing on an empty login and
            // letting the fetch come back with the same 401 a moment later.
            self.message = "Enter the user name and password the server is asking for.".into();
            self.message_is_error = true;
            return;
        }
        self.answer(Some(credentials), ui.ctx());
        cx.close();
    }
}

/// A URL that will not parse is still worth heading the dialog with: seeing it
/// back tells the user which of several open Add windows is asking.
fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| url.to_string())
}

impl AppWindow for CredentialsWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::Credentials(self.instance)
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Sign In")
            .with_inner_size(vec2(400.0, 214.0))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
            .with_always_on_top()
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Sign In".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        if self.ctx.is_none() {
            self.ctx = Some(ui.ctx().clone());
        }
        let p = theme::palette();
        form::header(
            ui,
            "cred-header",
            Icon::SignIn,
            "Sign In Required",
            &self.host,
        );
        let mut ok = false;
        let mut cancel = false;
        controls::footer(ui, "cred-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ok |= controls::button(ui, "Sign In", ButtonKind::Primary, true).clicked();
                cancel |= controls::button(ui, "Cancel", ButtonKind::Secondary, true).clicked();
            });
        });
        form::body(ui, Margin::symmetric(16, 10), |ui| {
            let color = if self.message_is_error {
                p.danger
            } else {
                p.warning
            };
            ui.allocate_ui_with_layout(
                vec2(ui.available_width(), 30.0),
                Layout::top_down(Align::Min),
                |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(&self.message)
                                .font(theme::small())
                                .color(color),
                        )
                        .wrap(),
                    );
                },
            );
            let width = form::field_width(ui, LABEL_WIDTH);
            let user = form::row(ui, LABEL_WIDTH, "User name", |ui| {
                form::text(ui, &mut self.user_name, width)
            });
            let pass = form::row(ui, LABEL_WIDTH, "Password", |ui| {
                form::text_with(
                    ui,
                    TextEdit::singleline(&mut self.password).password(true),
                    width,
                )
            });
            // Focus goes to the field actually missing something: the user name
            // on a first challenge, the password when the user name survived a
            // rejection.
            if !self.focused {
                self.focused = true;
                if self.user_name.is_empty() {
                    user.request_focus();
                } else {
                    pass.request_focus();
                }
            }
        });
        if form::enter_pressed(ui) {
            ok = true;
        }
        if form::escape_pressed(ui) {
            cancel = true;
        }
        if ok {
            self.on_ok(ui, cx);
        } else if cancel {
            self.answer(None, ui.ctx());
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        // Closed by its X: a cancel.
        if let Some(ctx) = self.ctx.clone() {
            self.answer(None, &ctx);
        } else if let Some(reply) = self.reply.take() {
            let _ = reply.send(None);
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
