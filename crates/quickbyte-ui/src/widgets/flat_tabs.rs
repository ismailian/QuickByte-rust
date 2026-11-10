//! A tab strip that looks like the rest of the app — port of
//! `Controls/FlatTabView.cs`: flat labels on the header colour, an accent
//! underline and accent text on the active tab, a tint on hover, and a hairline
//! under the strip.
//!
//! egui is immediate mode, so this is only the strip: the caller shows whichever
//! page `selected` names below it.

use egui::{Align2, Rect, Response, Sense, Stroke, Ui, Vec2, pos2};

use crate::theme;

/// Draws the strip across the full width at `height`; returns true when the
/// selection changed.
pub fn show(ui: &mut Ui, titles: &[&str], selected: &mut usize, height: f32) -> bool {
    let p = theme::palette();
    let (strip, _) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(strip, 0.0, p.header_back);

    let mut changed = false;
    let mut x = strip.left();
    for (index, title) in titles.iter().enumerate() {
        let is_selected = *selected == index;
        let font = if is_selected {
            theme::body_strong()
        } else {
            theme::body()
        };
        // Sized for the bold face so a tab does not change width when picked.
        let width = painter
            .layout_no_wrap(title.to_string(), theme::body_strong(), p.text)
            .size()
            .x
            + 28.0;
        let rect = Rect::from_min_size(pos2(x, strip.top()), Vec2::new(width, height));
        let response: Response = ui.interact(
            rect,
            ui.scope_id().with(("flat_tab", index)),
            Sense::click(),
        );
        if response.clicked() && !is_selected {
            *selected = index;
            changed = true;
        }
        let back = if is_selected {
            p.surface
        } else if response.hovered() {
            p.accent_soft
        } else {
            p.header_back
        };
        painter.rect_filled(rect, 0.0, back);
        let color = if is_selected { p.accent } else { p.text_muted };
        painter.text(
            rect.center() - Vec2::new(0.0, 1.0),
            Align2::CENTER_CENTER,
            *title,
            font,
            color,
        );
        if is_selected {
            painter.rect_filled(
                Rect::from_min_max(pos2(rect.left(), rect.bottom() - 2.0), rect.right_bottom()),
                0.0,
                p.accent,
            );
        }
        x += width;
    }
    painter.hline(
        strip.x_range(),
        strip.bottom() - 0.5,
        Stroke::new(1.0, p.border),
    );
    changed
}
