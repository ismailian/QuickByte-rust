//! The small shared controls every window uses: the flat dialog button
//! (`Theme.StyleButton`), the toolbar button, menu entries with icons, the
//! caption/value pair, the hairline and the footer strip. Kept here so the
//! windows cannot drift apart in how a button or a caption looks.

use egui::{
    Align2, Atom, Button, Color32, CursorIcon, FontId, IdSalt, Rect, Response, RichText, Sense,
    Stroke, StrokeKind, Ui, Vec2, pos2, vec2,
};

use crate::icons::{self, Icon};
use crate::theme;

/// Dialog button height — compact (the C# buttons were 30 px; §7.2 asks for 20–22).
pub const BUTTON_HEIGHT: f32 = 22.0;
pub const BUTTON_MIN_WIDTH: f32 = 72.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// Accent fill, white text: the window's main action.
    Primary,
    /// Surface fill, strong border.
    Secondary,
}

/// The flat dialog button. Flat buttons do not grey themselves out the way
/// system-drawn ones do, so the disabled palette is applied explicitly.
pub fn button(ui: &mut Ui, text: &str, kind: ButtonKind, enabled: bool) -> Response {
    button_sized(ui, text, kind, enabled, BUTTON_MIN_WIDTH)
}

/// [`button`], at least `min_width` wide.
pub fn button_sized(
    ui: &mut Ui,
    text: &str,
    kind: ButtonKind,
    enabled: bool,
    min_width: f32,
) -> Response {
    let p = theme::palette();
    let font = theme::body();
    let galley = ui
        .painter()
        .layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER);
    let size = vec2((galley.size().x + 24.0).max(min_width), BUTTON_HEIGHT);
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    if !ui.is_rect_visible(rect) {
        return response;
    }
    let pressed = response.is_pointer_button_down_on();
    let hovered = response.hovered();
    let (fill, stroke, color) = match (enabled, kind) {
        (false, _) => (p.track, p.border, p.text_disabled),
        (true, ButtonKind::Primary) => {
            let fill = if pressed {
                p.accent_dark
            } else if hovered {
                p.accent_light
            } else {
                p.accent
            };
            (fill, p.accent_dark, p.text_on_accent)
        }
        (true, ButtonKind::Secondary) => {
            let fill = if pressed {
                p.track
            } else if hovered {
                p.accent_soft
            } else {
                p.surface
            };
            (
                fill,
                if hovered {
                    p.accent_light
                } else {
                    p.border_strong
                },
                p.text,
            )
        }
    };
    let painter = ui.painter();
    painter.rect(
        rect,
        theme::CORNER_RADIUS,
        fill,
        Stroke::new(1.0, stroke),
        StrokeKind::Inside,
    );
    if response.has_focus() {
        painter.rect_stroke(
            rect.shrink(2.0),
            2.0,
            Stroke::new(1.0, p.accent_light),
            StrokeKind::Inside,
        );
    }
    let pos = Align2::CENTER_CENTER
        .anchor_size(rect.center(), galley.size())
        .min;
    painter.galley_with_override_text_color(pos, galley, color);
    if enabled && hovered {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

/// A toolbar button: a 16 px icon and its label on one 24 px line, tinted on
/// hover and pressed, greyed when it cannot act. The tooltip names the shortcut.
pub fn toolbar_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    tooltip: &str,
    enabled: bool,
) -> Response {
    let p = theme::palette();
    let font = theme::body();
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let size = vec2(6.0 + 16.0 + 5.0 + galley.size().x + 8.0, 24.0);
    let (rect, response) = ui.allocate_exact_size(
        size,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let pressed = enabled && response.is_pointer_button_down_on();
    let hovered = enabled && response.hovered();
    let painter = ui.painter();
    if pressed {
        painter.rect(
            rect,
            theme::CORNER_RADIUS,
            p.accent_pressed,
            Stroke::new(1.0, p.accent),
            StrokeKind::Inside,
        );
    } else if hovered {
        painter.rect(
            rect,
            theme::CORNER_RADIUS,
            p.accent_soft,
            Stroke::new(1.0, p.accent_light),
            StrokeKind::Inside,
        );
    }
    let icon_rect = Rect::from_min_size(
        pos2(rect.left() + 6.0, rect.center().y - 8.0),
        Vec2::splat(16.0),
    );
    icons::paint_ex(painter, icon_rect, icon, !enabled);
    let text_pos = pos2(
        icon_rect.right() + 5.0,
        rect.center().y - galley.size().y / 2.0,
    );
    painter.galley_with_override_text_color(
        text_pos,
        galley,
        if enabled { p.text } else { p.text_disabled },
    );
    response.on_hover_text(tooltip)
}

/// A vertical hairline between toolbar groups.
pub fn toolbar_separator(ui: &mut Ui) {
    let p = theme::palette();
    let (rect, _) = ui.allocate_exact_size(vec2(9.0, 22.0), Sense::hover());
    ui.painter().vline(
        rect.center().x.round() + 0.5,
        rect.shrink2(vec2(0.0, 3.0)).y_range(),
        Stroke::new(1.0, p.border),
    );
}

/// A menu entry with a 16 px icon in the margin, an optional shortcut on the
/// right, greyed when `enabled` is false. Returns true when clicked.
pub fn menu_item(
    ui: &mut Ui,
    icon: Option<Icon>,
    text: &str,
    shortcut: Option<&str>,
    enabled: bool,
) -> bool {
    let id = IdSalt::new(("menu_icon", text));
    let mut button = Button::new((Atom::custom(id, Vec2::splat(16.0)), RichText::new(text)))
        .frame_when_inactive(false)
        .wrap_mode(egui::TextWrapMode::Extend)
        .min_size(vec2(0.0, 22.0));
    if let Some(shortcut) = shortcut {
        button = button.shortcut_text(RichText::new(shortcut).color(theme::palette().text_muted));
    }
    let response = ui.add_enabled_ui(enabled, |ui| button.atom_ui(ui)).inner;
    if let (Some(icon), Some(rect)) = (icon, response.rect(id)) {
        icons::paint_ex(ui.painter(), rect, icon, !enabled);
    }
    enabled && response.response.clicked()
}

/// A check-mark menu entry (View > Toolbar).
pub fn menu_check(ui: &mut Ui, text: &str, checked: &mut bool) -> bool {
    let id = IdSalt::new(("menu_check", text));
    let response = Button::new((Atom::custom(id, Vec2::splat(16.0)), RichText::new(text)))
        .frame_when_inactive(false)
        .wrap_mode(egui::TextWrapMode::Extend)
        .min_size(vec2(0.0, 22.0))
        .atom_ui(ui);
    if *checked && let Some(rect) = response.rect(id) {
        let p = theme::palette();
        let c = rect.center();
        let stroke = Stroke::new(1.6, p.accent);
        ui.painter().line(
            vec![
                pos2(c.x - 4.0, c.y),
                pos2(c.x - 1.0, c.y + 3.0),
                pos2(c.x + 4.5, c.y - 3.5),
            ],
            stroke,
        );
    }
    if response.response.clicked() {
        *checked = !*checked;
        true
    } else {
        false
    }
}

/// A muted caption ("Status:") in the small face.
pub fn caption(ui: &mut Ui, text: &str) -> Response {
    ui.label(
        RichText::new(text)
            .font(theme::small())
            .color(theme::palette().text_muted),
    )
}

/// A left-aligned, fixed-width cell of a field grid that ellipsises its text.
pub fn field(ui: &mut Ui, width: f32, text: impl Into<RichText>) -> Response {
    ui.allocate_ui_with_layout(
        vec2(width, 16.0),
        egui::Layout::left_to_right(egui::Align::Center),
        |ui| {
            ui.set_min_width(width);
            ui.add(egui::Label::new(text.into()).truncate())
        },
    )
    .inner
}

/// A label that ellipsises instead of wrapping.
pub fn truncated(ui: &mut Ui, text: impl Into<RichText>) -> Response {
    ui.add(egui::Label::new(text.into()).truncate())
}

/// A clickable accent text ("Verify against the publisher's checksum…").
pub fn link(ui: &mut Ui, text: &str, font: FontId) -> Response {
    let p = theme::palette();
    let response = ui.add(
        egui::Label::new(RichText::new(text).font(font).color(p.accent))
            .sense(Sense::click())
            .truncate(),
    );
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    response
}

/// A full-width hairline.
pub fn hairline(ui: &mut Ui) {
    let p = theme::palette();
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, p.border);
}

/// The button strip along the bottom of a dialog: `header_back`, a hairline on
/// top (the panel separator, drawn in the border colour), buttons laid out by `add`.
pub fn footer<R>(ui: &mut Ui, id: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette();
    egui::Panel::bottom(id)
        .exact_size(38.0)
        .resizable(false)
        .frame(
            egui::Frame::NONE
                .fill(p.header_back)
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ui, add)
        .inner
}

/// The body of a dialog: the surface colour with the standard margin.
pub fn body<R>(ui: &mut Ui, margin: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    let p = theme::palette();
    egui::CentralPanel::no_frame()
        .frame(
            egui::Frame::NONE
                .fill(p.surface)
                .inner_margin(egui::Margin::same(margin)),
        )
        .show(ui, add)
        .inner
}

/// A frame for a panel with no margin and the given fill.
pub fn panel_frame(fill: Color32, margin: egui::Margin) -> egui::Frame {
    egui::Frame::NONE.fill(fill).inner_margin(margin)
}
