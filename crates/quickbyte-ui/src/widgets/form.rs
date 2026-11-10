//! The pieces every dialog repeats — the port of `Controls/FormChrome.cs` plus
//! the small form controls WinForms gave for free: the icon + title header
//! band, field labels and flat text boxes in a label column, the bold
//! caption-with-hint block Options and Queues use, a numeric spinner
//! (`NumericUpDown`), a time picker (`DateTimePicker` in time mode), a status
//! line, and the bordered info card.
//!
//! Everything is sized to the compact scale (MIGRATION.md §7.2): 12 px text,
//! 20–22 px controls.

use egui::{
    Align, Color32, CursorIcon, Layout, Margin, Response, RichText, Sense, Stroke, StrokeKind,
    TextEdit, Ui, vec2,
};

use crate::icons::{self, Icon};
use crate::theme;

/// Height of a text box, combo box and spinner.
pub const FIELD_HEIGHT: f32 = 22.0;
/// Height of one form row (field plus its breathing room).
pub const ROW_HEIGHT: f32 = 28.0;

/// The header band across the top of a dialog: a 32 px icon, the title, and a
/// muted subtitle, over a hairline (`FormChrome.Header`, compacted from 74 px).
pub fn header(ui: &mut Ui, id: &str, icon: Icon, title: &str, subtitle: &str) {
    let p = theme::palette();
    egui::Panel::top(id.to_owned())
        .exact_size(54.0)
        .resizable(false)
        .frame(egui::Frame::NONE.fill(p.surface).inner_margin(Margin {
            left: 16,
            right: 16,
            top: 10,
            bottom: 8,
        }))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                icons::show(ui, icon, 32.0);
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        egui::Label::new(RichText::new(title).font(theme::title()).color(p.text))
                            .truncate(),
                    );
                    ui.add(
                        egui::Label::new(
                            RichText::new(subtitle)
                                .font(theme::small())
                                .color(p.text_muted),
                        )
                        .truncate(),
                    );
                });
            });
        });
}

/// A muted field label in a fixed-width label column, vertically centred on a
/// form row (`FormChrome.FieldLabel`).
pub fn label_cell(ui: &mut Ui, width: f32, text: &str) {
    let p = theme::palette();
    ui.allocate_ui_with_layout(
        vec2(width, FIELD_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.label(RichText::new(text).font(theme::small()).color(p.text_muted));
        },
    );
}

/// A form row: a label column of `label_width`, then `add` laid out left to
/// right in what is left. Rows are a fixed [`ROW_HEIGHT`] so labels line up
/// with their fields whatever the field is.
pub fn row<R>(ui: &mut Ui, label_width: f32, label: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), ROW_HEIGHT),
        Layout::left_to_right(Align::Center),
        |ui| {
            label_cell(ui, label_width, label);
            add(ui)
        },
    )
    .inner
}

/// The width a [`row`]'s field may take so it ends flush with the right margin.
pub fn field_width(ui: &Ui, label_width: f32) -> f32 {
    ui.available_width() - label_width - ui.spacing().item_spacing.x
}

/// A flat single-line text box (`FormChrome.Field`), `width` wide.
pub fn text(ui: &mut Ui, value: &mut String, width: f32) -> Response {
    text_with(ui, TextEdit::singleline(value), width)
}

/// A text box with a placeholder.
pub fn text_hint(ui: &mut Ui, value: &mut String, width: f32, hint: &str) -> Response {
    text_with(
        ui,
        TextEdit::singleline(value)
            .hint_text(RichText::new(hint).color(theme::palette().text_disabled)),
        width,
    )
}

/// A configured [`TextEdit`] at the standard height.
pub fn text_with(ui: &mut Ui, edit: TextEdit<'_>, width: f32) -> Response {
    ui.add_sized(
        vec2(width.max(20.0), FIELD_HEIGHT),
        edit.margin(Margin::symmetric(5, 3))
            .align(egui::Align2::LEFT_CENTER),
    )
}

/// A read-only box (the current address, the pairing token): header colour,
/// muted text, selectable but not editable.
pub fn read_only(ui: &mut Ui, value: &str, width: f32, mono: bool) -> Response {
    let p = theme::palette();
    let mut text = value.to_string();
    let mut edit = TextEdit::singleline(&mut text)
        .background_color(p.header_back)
        .text_color(p.text_muted);
    if mono {
        edit = edit.font(egui::FontId::monospace(theme::BODY));
    }
    // Edits are thrown away: `text` is a copy.
    text_with(ui, edit, width)
}

/// A bold caption over a muted hint, in a column of `width`
/// (`SettingsForm.CaptionBlock`).
pub fn caption_block(ui: &mut Ui, width: f32, caption: &str, hint: &str) {
    let p = theme::palette();
    ui.allocate_ui_with_layout(vec2(width, 34.0), Layout::top_down(Align::Min), |ui| {
        ui.set_width(width);
        ui.spacing_mut().item_spacing.y = 1.0;
        ui.label(
            RichText::new(caption)
                .font(theme::body_strong())
                .color(p.text),
        );
        ui.add(
            egui::Label::new(RichText::new(hint).font(theme::small()).color(p.text_muted))
                .truncate(),
        );
    });
}

/// A wrapped muted note (small type).
pub fn note(ui: &mut Ui, text: &str) -> Response {
    note_colored(ui, text, theme::palette().text_muted)
}

pub fn note_colored(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::small()).color(color)).wrap())
}

/// The one-line status under an address box: small, coloured, ellipsised.
pub fn status_line(ui: &mut Ui, text: &str, color: Color32) -> Response {
    ui.add(egui::Label::new(RichText::new(text).font(theme::small()).color(color)).truncate())
        .on_hover_text(text)
}

/// A bordered card on the header colour (the Add window's info card, the
/// update window's version card).
pub fn card<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette();
    egui::Frame::NONE
        .fill(p.header_back)
        .stroke(Stroke::new(1.0, p.border))
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// A caption + value pair inside a card (`FormChrome.AddInfoCell`).
pub fn info_cell(
    ui: &mut Ui,
    caption_width: f32,
    value_width: f32,
    caption: &str,
    value: RichText,
) {
    let p = theme::palette();
    crate::widgets::controls::field(
        ui,
        caption_width,
        RichText::new(caption)
            .font(theme::small())
            .color(p.text_muted),
    );
    crate::widgets::controls::field(ui, value_width, value);
}

/// `NumericUpDown`: a right-aligned number box with stacked arrows. Typing,
/// dragging, the arrows, and the mouse wheel over it all change the value in
/// `step`s, clamped to `min..=max`. Returns true when the value changed.
pub fn spinner(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    value: &mut i64,
    min: i64,
    max: i64,
    step: i64,
    width: f32,
) -> bool {
    let p = theme::palette();
    let before = *value;
    *value = (*value).clamp(min, max);
    ui.push_id(id, |ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        let drag = ui.add_sized(
            vec2(width - 16.0, FIELD_HEIGHT),
            egui::DragValue::new(value)
                .range(min..=max)
                .speed(step as f64 / 4.0)
                .update_while_editing(false),
        );
        if drag.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll > 0.0 {
                *value = (*value + step).min(max);
            } else if scroll < 0.0 {
                *value = (*value - step).max(min);
            }
        }
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, FIELD_HEIGHT), Sense::hover());
        let enabled = ui.is_enabled();
        for (half, delta) in [(0, step), (1, -step)] {
            let r = egui::Rect::from_min_size(
                rect.min + vec2(0.0, half as f32 * rect.height() / 2.0),
                vec2(rect.width(), rect.height() / 2.0),
            );
            let response = ui.interact(r, ui.scope_id().with(("spin", half)), Sense::click());
            let fill = if enabled && response.is_pointer_button_down_on() {
                p.accent_pressed
            } else if enabled && response.hovered() {
                p.accent_soft
            } else {
                p.surface
            };
            ui.painter().rect(
                r,
                0.0,
                fill,
                Stroke::new(1.0, p.border_strong),
                StrokeKind::Inside,
            );
            let c = r.center();
            let dir = if half == 0 { -1.0 } else { 1.0 };
            let color = if enabled { p.text } else { p.text_disabled };
            ui.painter().add(egui::Shape::convex_polygon(
                vec![
                    c + vec2(-3.0, -1.5 * dir),
                    c + vec2(3.0, -1.5 * dir),
                    c + vec2(0.0, 1.5 * dir),
                ],
                color,
                Stroke::NONE,
            ));
            if enabled && response.clicked() {
                *value = (*value + delta).clamp(min, max);
            }
            if enabled && response.hovered() {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
        }
    });
    *value != before
}

/// `DateTimePicker` in time mode: `HH : MM : SS`, each part its own spinner
/// field. `seconds_of_day` is kept within one day.
pub fn time_picker(
    ui: &mut Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    seconds_of_day: &mut i64,
) -> bool {
    let p = theme::palette();
    let before = *seconds_of_day;
    let total = seconds_of_day.rem_euclid(86_400);
    let (mut h, mut m, mut s) = (total / 3600, total / 60 % 60, total % 60);
    ui.push_id(id, |ui| {
        egui::Frame::NONE
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border_strong))
            .corner_radius(theme::CORNER_RADIUS)
            .inner_margin(Margin::symmetric(3, 0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.spacing_mut().button_padding = vec2(2.0, 1.0);
                // One box, not three: the parts are frameless until hovered.
                let w = &mut ui.visuals_mut().widgets;
                w.inactive.bg_stroke = Stroke::NONE;
                w.inactive.weak_bg_fill = p.surface;
                w.inactive.bg_fill = p.surface;
                w.hovered.bg_stroke = Stroke::NONE;
                w.active.bg_stroke = Stroke::NONE;
                let part = |ui: &mut Ui, v: &mut i64, max: i64| {
                    ui.add_sized(
                        vec2(22.0, FIELD_HEIGHT - 2.0),
                        egui::DragValue::new(v)
                            .range(0..=max)
                            .speed(0.2)
                            .custom_formatter(|n, _| format!("{:02}", n as i64)),
                    );
                };
                part(ui, &mut h, 23);
                ui.label(RichText::new(":").color(p.text_muted));
                part(ui, &mut m, 59);
                ui.label(RichText::new(":").color(p.text_muted));
                part(ui, &mut s, 59);
            });
    });
    *seconds_of_day = h * 3600 + m * 60 + s;
    *seconds_of_day != before
}

/// A muted "QUEUES"-style section caption.
pub fn section_caption(ui: &mut Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(theme::small_strong())
            .color(theme::palette().text_muted),
    );
}

/// Esc pressed in this window (and not consumed by an open popup).
pub fn escape_pressed(ui: &Ui) -> bool {
    !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
}

/// Enter pressed with no multi-line box focused.
pub fn enter_pressed(ui: &Ui) -> bool {
    !egui::Popup::is_any_open(ui.ctx())
        && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter))
}

/// A list header cell as the main table draws it: header colour, hairlines,
/// the title in the strong face.
pub fn header_cell(ui: &mut Ui, title: &str) {
    let p = theme::palette();
    let rect = ui.max_rect();
    let painter = ui.painter();
    painter.rect_filled(rect, 0.0, p.header_back);
    painter.hline(
        rect.x_range(),
        rect.bottom() - 0.5,
        Stroke::new(1.0, p.border),
    );
    painter.vline(
        rect.right() - 0.5,
        rect.y_range().shrink(4.0),
        Stroke::new(1.0, p.border),
    );
    painter.text(
        rect.left_center() + vec2(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        title,
        theme::body_strong(),
        p.text,
    );
}

/// A bordered box around a list (`BorderStyle.FixedSingle`).
pub fn list_frame<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette();
    egui::Frame::NONE
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border_strong))
        .inner_margin(Margin::same(1))
        .show(ui, add)
        .inner
}

/// The list colours the main table uses: banding, selection, hover.
pub fn list_visuals(ui: &mut Ui) {
    let p = theme::palette();
    let focused = ui.ctx().input(|i| i.focused);
    let v = ui.visuals_mut();
    v.faint_bg_color = p.row_alt;
    v.selection.bg_fill = if focused {
        p.row_selected
    } else {
        p.row_selected_inactive
    };
    v.selection.stroke.color = if focused { p.text_on_accent } else { p.text };
    v.widgets.hovered.bg_fill = p.accent_soft;
    ui.spacing_mut().item_spacing = vec2(0.0, 0.0);
}

/// The body of a dialog below its header: surface fill with a margin.
pub fn body<R>(ui: &mut Ui, margin: Margin, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette();
    egui::CentralPanel::no_frame()
        .frame(egui::Frame::NONE.fill(p.surface).inner_margin(margin))
        .show(ui, add)
        .inner
}
