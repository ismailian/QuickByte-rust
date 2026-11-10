//! Refresh Download Address — port of `Forms/RefreshAddressForm.cs`.
//!
//! Gives a download a new link without losing the bytes it already fetched.
//! Signed URLs expire, so a download paused overnight resumes into a `403`
//! while gigabytes of it sit on disk.
//!
//! Two ways in: paste the new link, or click the download link again in the
//! browser while this window is open — the app routes a capture that
//! `matches_capture` here instead of opening Add Download, and the capture
//! brings the page's fresh cookie with it.
//!
//! Whatever arrives is probed and judged (`judge_address`) before anything
//! changes:
//!
//! | The new address serves… | Result |
//! |---|---|
//! | the same size, no contradicting validator from the same host | **Update** — carries on from its chunks |
//! | a different size | Refused: add it as a new download |
//! | the same size, a different version from the same host | **Start Over** |
//! | an unknown size, with progress on disk | the user decides |

use std::any::Any;

use egui::{Align, Color32, Layout, Margin, Ui, ViewportBuilder, vec2};
use quickbyte_core::error::CoreError;
use quickbyte_core::helpers::byte_formatter::format_bytes;
use quickbyte_core::helpers::url_credentials;
use quickbyte_core::models::{
    AddressChange, CapturedDownload, DownloadCredentials, DownloadItem, Headers, RemoteFileInfo,
    RequestOptions, header_get,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::credentials::CredentialsWindow;
use super::prompt::{Prompt, PromptIcon};
use super::{AppCommand, AppWindow, WindowCx, WindowKind};
use crate::actions::{self, AddressVerdict};
use crate::icons::Icon;
use crate::services::Pending;
use crate::shell::Waker;
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::form;

const LABEL_WIDTH: f32 = 84.0;

struct Candidate {
    url: String,
    info: RemoteFileInfo,
    verdict: AddressVerdict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Cancel,
    Keep,
    StartOver,
}

pub struct RefreshAddressWindow {
    id: Uuid,
    item: DownloadItem,
    new_url: String,
    status: String,
    color: Option<Color32>,
    resume: bool,
    /// Headers that came with a browser capture; `None` keeps the download's own.
    captured_headers: Option<Headers>,
    /// A login lifted out of the URL or typed into Sign In; `None` keeps the download's own.
    credentials: Option<DownloadCredentials>,
    candidate: Option<Candidate>,
    probe: Option<(
        String,
        CancellationToken,
        Pending<quickbyte_core::Result<RemoteFileInfo>>,
    )>,
    awaiting_login: Option<crossbeam_channel::Receiver<Option<DownloadCredentials>>>,
    prompt: Option<Prompt<Answer>>,
    check_now: bool,
    focus: bool,
    last_url: String,
}

impl RefreshAddressWindow {
    pub fn new(id: Uuid, vm: &ViewModel, clipboard: Option<String>) -> Self {
        let item = vm.item(id).cloned().unwrap_or_default();
        // Only a link that is not the one already failing: pre-filling the
        // dead address would invite checking it.
        let new_url = clipboard
            .map(|t| t.trim().to_string())
            .filter(|t| actions::is_supported_url(t) && *t != item.url)
            .unwrap_or_default();
        let mut w = Self {
            id,
            new_url: new_url.clone(),
            status: String::new(),
            color: None,
            resume: true,
            captured_headers: None,
            credentials: None,
            candidate: None,
            probe: None,
            awaiting_login: None,
            prompt: None,
            check_now: false,
            focus: true,
            last_url: new_url,
            item,
        };
        w.status = w.progress_summary();
        w
    }

    pub fn download_id(&self) -> Uuid {
        self.id
    }

    pub fn item(&self) -> &DownloadItem {
        &self.item
    }

    /// Whether a browser capture may be routed here. Not while a check runs —
    /// the capture would be answered by the result of the link it replaced.
    pub fn is_waiting_for_capture(&self) -> bool {
        self.probe.is_none() && self.awaiting_login.is_none()
    }

    /// A link the browser handed over while this window was open. Checked at once.
    pub fn accept_capture(&mut self, captured: &CapturedDownload) {
        self.captured_headers = Some(captured.to_headers());
        self.new_url = captured.url.clone();
        self.last_url = self.new_url.clone();
        self.candidate = None;
        self.check_now = true;
    }

    fn progress_summary(&self) -> String {
        if actions::has_progress(&self.item) {
            format!(
                "{} of {} is already downloaded and will be kept.",
                format_bytes(self.item.downloaded_bytes),
                format_bytes(self.item.total_bytes)
            )
        } else {
            "Nothing has been downloaded yet.".into()
        }
    }

    /// The page this download came from, when the browser said.
    fn page_url(&self) -> Option<String> {
        let referer = header_get(&self.item.headers, "Referer")?;
        let uri = url::Url::parse(referer).ok()?;
        matches!(uri.scheme(), "http" | "https").then(|| uri.to_string())
    }

    fn set_status(&mut self, text: impl Into<String>, color: Option<Color32>) {
        self.status = text.into();
        self.color = color;
    }

    fn check(&mut self, ui: &Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        let split = url_credentials::extract(self.new_url.trim());
        if split.credentials.is_some() {
            self.credentials = split.credentials;
            self.new_url = split.url.clone();
            self.last_url = self.new_url.clone();
        }
        if !actions::is_supported_url(&split.url) {
            self.set_status(
                "Enter a valid http, https, ftp or ftps address.",
                Some(p.danger),
            );
            return;
        }
        self.candidate = None;
        self.set_status("Checking the new address…", None);
        // Asked the way the download itself will ask: its own login and
        // headers, unless the new link brought its own.
        let options = RequestOptions {
            credentials: self
                .credentials
                .clone()
                .or_else(|| self.item.credentials.clone()),
            headers: self
                .captured_headers
                .clone()
                .or_else(|| (!self.item.headers.is_empty()).then(|| self.item.headers.clone())),
            ..RequestOptions::default()
        };
        let cancel = CancellationToken::new();
        let file_info = cx.services.file_info.clone();
        let (url, token) = (split.url.clone(), cancel.clone());
        let pending = cx
            .services
            .run_waking(Waker::new(ui.ctx(), cx.viewport), async move {
                file_info.get_file_info(&url, &options, &token).await
            });
        self.probe = Some((split.url, cancel, pending));
    }

    fn on_probed(
        &mut self,
        url: String,
        result: quickbyte_core::Result<RemoteFileInfo>,
        cx: &mut WindowCx<'_>,
    ) {
        let p = theme::palette();
        match result {
            Ok(info) => {
                let verdict = actions::judge_address(&self.item, &info);
                self.show_verdict(Candidate { url, info, verdict });
            }
            // The new link asks for a login — also how a download whose password
            // changed gets the new one: there is nowhere else to type it.
            Err(CoreError::AuthenticationRequired { message, .. }) => {
                self.set_status(message.clone(), Some(p.warning));
                let (tx, rx) = crossbeam_channel::bounded(1);
                let existing = self
                    .credentials
                    .clone()
                    .or_else(|| self.item.credentials.clone());
                cx.command(AppCommand::OpenWindow(Box::new(CredentialsWindow::new(
                    &url,
                    &message,
                    existing.as_ref(),
                    Some(tx),
                    Some(cx.viewport),
                ))));
                self.awaiting_login = Some(rx);
            }
            Err(CoreError::Cancelled) => {}
            Err(e) => self.set_status(
                format!("Could not check the new address: {e}"),
                Some(p.danger),
            ),
        }
    }

    fn show_verdict(&mut self, candidate: Candidate) {
        let p = theme::palette();
        let size = if candidate.info.has_known_size() {
            format_bytes(candidate.info.content_length)
        } else {
            "unknown size".into()
        };
        match candidate.verdict {
            AddressVerdict::DifferentSize => {
                self.set_status(
                    format!(
                        "This address points to a different file ({size}, not {}). Add it as a new download instead.",
                        format_bytes(self.item.total_bytes)
                    ),
                    Some(p.danger),
                );
                return;
            }
            AddressVerdict::NewVersion => self.set_status(
                "The file at this address has changed since the download started. Updating starts it over.",
                Some(p.warning),
            ),
            AddressVerdict::Unverifiable => self.set_status(
                "The server did not say how big the file is, so it cannot be confirmed as the same file.",
                Some(p.warning),
            ),
            AddressVerdict::SameFile => {
                let summary = self.progress_summary();
                self.set_status(format!("Same file ({size}). {summary}"), Some(p.success));
            }
        }
        self.candidate = Some(candidate);
    }

    fn on_update(&mut self, cx: &mut WindowCx<'_>) {
        let Some(candidate) = &self.candidate else {
            return;
        };
        if candidate.verdict == AddressVerdict::Unverifiable {
            // The user may well know it is the same file — a mirror of an ISO
            // they recognise. Their call, but the question says what the risk is.
            self.prompt = Some(
                Prompt::new(
                    "Refresh Download Address",
                    "QuickByte cannot confirm that the new address serves the same file.\n\n\
                     Yes — keep what is downloaded and continue (a different file would be corrupted).\n\
                     No — start the download over from the new address.",
                    PromptIcon::Warning,
                    Answer::Cancel,
                )
                .button("Cancel", Answer::Cancel, ButtonKind::Secondary)
                .button("No", Answer::StartOver, ButtonKind::Secondary)
                .button("Yes", Answer::Keep, ButtonKind::Primary),
            );
            return;
        }
        let discard = candidate.verdict == AddressVerdict::NewVersion;
        self.apply(discard, cx);
    }

    fn apply(&mut self, discard: bool, cx: &mut WindowCx<'_>) {
        let Some(candidate) = &self.candidate else {
            return;
        };
        let change = AddressChange {
            url: candidate.url.clone(),
            file_info: candidate.info.clone(),
            credentials: self.credentials.clone(),
            headers: self.captured_headers.clone(),
            discard_progress: discard,
        };
        if !cx.services.downloads.change_address(self.id, change) {
            let p = theme::palette();
            self.set_status(
                "The download is running. Pause it, then update the address.",
                Some(p.danger),
            );
            return;
        }
        if self.resume {
            let (downloads, id) = (cx.services.downloads.clone(), self.id);
            cx.services.spawn(async move { downloads.resume(id).await });
        }
        cx.close();
    }
}

impl AppWindow for RefreshAddressWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::RefreshAddress(self.id)
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("Refresh Download Address")
            .with_inner_size(vec2(560.0, 300.0))
            .with_resizable(false)
            .with_maximize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "Refresh Download Address".into()
    }

    fn download(&self) -> Option<Uuid> {
        Some(self.id)
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        if let Some(item) = cx.vm.item(self.id) {
            self.item = item.clone();
        }
        if std::mem::take(&mut self.check_now) {
            if let Some((_, cancel, _)) = self.probe.take() {
                cancel.cancel();
            }
            self.check(ui, cx);
        }
        if let Some((_, _, pending)) = &mut self.probe
            && let Some(result) = pending.poll()
        {
            let (url, _, _) = self.probe.take().expect("probe");
            self.on_probed(url, result, cx);
        }
        if let Some(answer) = self
            .awaiting_login
            .as_ref()
            .and_then(|rx| rx.try_recv().ok())
        {
            self.awaiting_login = None;
            if let Some(credentials) = answer {
                self.credentials = Some(credentials);
                self.check(ui, cx);
            }
        }

        form::header(
            ui,
            "ref-header",
            Icon::RefreshAddress,
            "Refresh Download Address",
            &self.item.file_name,
        );
        let can_update = self.candidate.is_some() && self.probe.is_none();
        let update_text = match &self.candidate {
            Some(c) if c.verdict == AddressVerdict::NewVersion => "Start Over",
            _ => "Update",
        };
        let mut update = false;
        let mut cancel = false;
        controls::footer(ui, "ref-footer", |ui| {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                update |=
                    controls::button_sized(ui, update_text, ButtonKind::Primary, can_update, 88.0)
                        .clicked();
                cancel |= controls::button(ui, "Cancel", ButtonKind::Secondary, true).clicked();
            });
        });

        let mut check = false;
        let mut url_focused = false;
        let page = self.page_url();
        let waiting = self.awaiting_login.is_some();
        form::body(
            ui,
            Margin {
                left: 16,
                right: 16,
                top: 10,
                bottom: 8,
            },
            |ui| {
                if waiting {
                    ui.disable();
                }
                let full = form::field_width(ui, LABEL_WIDTH);
                form::row(ui, LABEL_WIDTH, "Current", |ui| {
                    form::read_only(ui, &self.item.url, full, false).on_hover_text(&self.item.url);
                });
                form::row(ui, LABEL_WIDTH, "New address", |ui| {
                    let response = form::text(ui, &mut self.new_url, full - 86.0 - 6.0);
                    if std::mem::take(&mut self.focus) {
                        response.request_focus();
                    }
                    url_focused = response.has_focus();
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        check = true;
                    }
                    ui.add_space(6.0 - ui.spacing().item_spacing.x);
                    check |= controls::button_sized(
                        ui,
                        "Check",
                        ButtonKind::Primary,
                        self.probe.is_none(),
                        86.0,
                    )
                    .clicked();
                });
                let indent = LABEL_WIDTH + ui.spacing().item_spacing.x;
                let width = full - 6.0;
                ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.allocate_ui_with_layout(
                        vec2(width, 32.0),
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
                ui.horizontal(|ui| {
                ui.add_space(indent);
                ui.allocate_ui_with_layout(vec2(width, 30.0), Layout::top_down(Align::Min), |ui| {
                    form::note(
                        ui,
                        "Paste the new link, or click the download link again in your browser — \
                         QuickByte catches it here rather than starting a new download.",
                    );
                });
            });
                // Offered only when the download came from a page the browser told
                // us about: that page is where a fresh link comes from.
                ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.allocate_ui_with_layout(
                        vec2(width, 20.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            if let Some(page) = &page
                                && controls::link(
                                    ui,
                                    "Open the page this download came from",
                                    theme::small(),
                                )
                                .clicked()
                            {
                                cx.shell.open_web_page(page);
                            }
                        },
                    );
                });
                ui.horizontal(|ui| {
                    ui.add_space(indent);
                    ui.checkbox(
                        &mut self.resume,
                        "Resume the download once the address is updated",
                    );
                });
            },
        );

        // A new address invalidates the verdict on the old one.
        if self.new_url != self.last_url {
            self.last_url = self.new_url.clone();
            if self.probe.is_none() {
                self.candidate = None;
            }
        }

        if let Some(prompt) = &self.prompt {
            if let Some(answer) = prompt.show(ui) {
                self.prompt = None;
                match answer {
                    Answer::Keep => self.apply(false, cx),
                    Answer::StartOver => self.apply(true, cx),
                    Answer::Cancel => {}
                }
            }
            return;
        }
        if waiting {
            return;
        }
        if check && self.probe.is_none() {
            self.check(ui, cx);
        } else if !url_focused && can_update && form::enter_pressed(ui) {
            update = true;
        }
        if form::escape_pressed(ui) {
            cancel = true;
        }
        if update && can_update {
            self.on_update(cx);
        } else if cancel {
            cx.close();
        }
    }

    fn on_closed(&mut self, _cx: &mut WindowCx<'_>) {
        if let Some((_, cancel, _)) = self.probe.take() {
            cancel.cancel();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
