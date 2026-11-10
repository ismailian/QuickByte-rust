//! About QuickByte — port of `Forms/AboutForm.cs`: the brand mark, the name,
//! the tagline and version, a paragraph, and the four-part build with the
//! copyright along the footer (the string that matches the .exe's Properties
//! page, which is the one worth having on a bug report).

use std::any::Any;

use egui::{Align, Layout, Margin, RichText, Ui, ViewportBuilder, vec2};

use super::{AppWindow, WindowCx, WindowKind};
use crate::icons::{self, Icon};
use crate::theme;
use crate::view_model::ViewModel;
use crate::widgets::controls::{self, ButtonKind};
use crate::widgets::form;

pub struct AboutWindow;

impl AppWindow for AboutWindow {
    fn kind(&self) -> WindowKind {
        WindowKind::About
    }

    fn builder(&self, _vm: &ViewModel) -> ViewportBuilder {
        ViewportBuilder::default()
            .with_title("About QuickByte")
            .with_inner_size(vec2(420.0, 262.0))
            .with_resizable(false)
            .with_maximize_button(false)
            .with_minimize_button(false)
    }

    fn title(&self, _vm: &ViewModel) -> String {
        "About QuickByte".into()
    }

    fn ui(&mut self, ui: &mut Ui, cx: &mut WindowCx<'_>) {
        let p = theme::palette();
        let mut close = false;
        controls::footer(ui, "about-footer", |ui| {
            ui.horizontal_centered(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(format!(
                            "Build {}  ·  {}",
                            cx.shell.file_version(),
                            cx.shell.copyright()
                        ))
                        .font(theme::small())
                        .color(p.text_muted),
                    )
                    .truncate(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    close |= controls::button(ui, "Close", ButtonKind::Primary, true).clicked();
                });
            });
        });
        form::body(
            ui,
            Margin {
                left: 20,
                right: 20,
                top: 18,
                bottom: 8,
            },
            |ui| {
                ui.horizontal(|ui| {
                    icons::show(ui, Icon::Brand, 48.0);
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.label(
                            RichText::new("QuickByte")
                                .font(egui::FontId::new(18.0, theme::semibold_family()))
                                .color(p.text),
                        );
                        ui.label(
                            RichText::new("Multi-connection download manager")
                                .font(theme::small())
                                .color(p.text_muted),
                        );
                        ui.label(
                            RichText::new(format!("Version {}", cx.shell.version()))
                                .font(theme::small_strong())
                                .color(p.accent),
                        );
                    });
                });
                ui.add_space(14.0);
                ui.add(
                egui::Label::new(
                    RichText::new(
                        "Segmented HTTP and FTP downloads with pause, resume and retry — built on \
                         Rust and egui.\n\nThe download engine runs headless in quickbyte-core; every \
                         window you see is a view over the same event stream.",
                    )
                    .color(p.text_muted),
                )
                .wrap(),
            );
            },
        );
        if form::escape_pressed(ui) || form::enter_pressed(ui) {
            close = true;
        }
        if close {
            cx.close();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
