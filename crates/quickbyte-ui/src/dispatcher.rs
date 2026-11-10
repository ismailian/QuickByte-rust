//! `UiDispatcher` — what is left of the C# `AppDispatcher`.
//!
//! Core publishes its events from worker tasks into unbounded channels, then
//! calls [`Dispatcher::notify`]. This implementation asks egui to repaint the
//! root viewport; the root's `App::logic` — which eframe runs even while the
//! window is hidden in the tray — drains every stream on the UI thread, in
//! order, applies the events to the one shared view model, and asks the open
//! secondary windows that show the affected downloads to repaint too. That is
//! how every open window shows identical numbers.
//!
//! The Core services are built before eframe creates its `egui::Context`, so the
//! context is attached later ([`UiDispatcher::attach`]). A notification that
//! arrives before then is not lost: the first frame drains everything anyway.

use std::sync::OnceLock;

use egui::ViewportId;
use quickbyte_core::events::Dispatcher;

#[derive(Default)]
pub struct UiDispatcher {
    ctx: OnceLock<egui::Context>,
}

impl UiDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Hands over the context once eframe has created it.
    pub fn attach(&self, ctx: egui::Context) {
        let _ = self.ctx.set(ctx);
    }
}

impl Dispatcher for UiDispatcher {
    fn notify(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint_of(ViewportId::ROOT);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notification_before_the_context_exists_is_harmless() {
        let d = UiDispatcher::new();
        d.notify();
        d.attach(egui::Context::default());
        d.notify();
    }
}
