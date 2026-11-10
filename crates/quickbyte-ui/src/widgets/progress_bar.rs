//! The flat progress bar — port of `SmoothProgressBar` and
//! `ListViewProgressPainter`, which were the same component at two sizes: a
//! square track, a fill with a slight vertical sheen, a hairline border, and a
//! centred label drawn twice — clipped to each side of the fill edge — so it
//! stays readable whether it sits on the fill or on the empty track.
//!
//! The animation itself lives in [`super::animator::Animator`]; this only paints
//! whatever value it is handed.

use egui::emath::GuiRounding as _;
use egui::epaint::Shape;
use egui::{Align2, Color32, FontId, Painter, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};
use quickbyte_core::enums::{ConnectionStatus, DownloadStatus};
use quickbyte_core::helpers::byte_formatter::format_percentage;

use crate::icons::lighten;
use crate::theme;

/// The fill colour for a download's status (`ListViewProgressPainter.ColorForStatus`).
pub fn color_for_status(status: DownloadStatus) -> Color32 {
    let p = theme::palette();
    match status {
        DownloadStatus::Failed | DownloadStatus::Cancelled => p.danger,
        DownloadStatus::Paused => p.warning,
        DownloadStatus::Completed => p.success,
        _ => p.accent,
    }
}

/// The fill colour for one connection (`ConnectionSegmentsBar.ColorFor`).
pub fn color_for_connection(status: ConnectionStatus) -> Color32 {
    let p = theme::palette();
    match status {
        ConnectionStatus::Finished => p.success,
        ConnectionStatus::Failed => p.danger,
        ConnectionStatus::Paused => p.warning,
        ConnectionStatus::Idle => p.border_strong,
        _ => p.accent,
    }
}

/// Paints a bar into `rect`. `percentage` is 0–100; `overlay` replaces the
/// percentage label (the merge text); `text_color` is the label's colour over the
/// empty track (it is always `text_on_accent` over the fill).
pub fn paint(
    painter: &Painter,
    rect: Rect,
    percentage: f64,
    fill: Color32,
    overlay: Option<&str>,
    font: FontId,
    text_color: Color32,
) {
    let p = theme::palette();
    let pct = if percentage.is_nan() {
        0.0
    } else {
        percentage.clamp(0.0, 100.0)
    };
    let rect = rect.round_to_pixels(painter.pixels_per_point());
    if rect.width() <= 2.0 || rect.height() <= 2.0 {
        return;
    }

    // Square corners: no curves to smooth, and anti-aliasing would only blur the
    // 1px border and the fill edge.
    painter.rect_filled(rect, 0.0, p.track);
    let fill_width = (rect.width() as f64 * pct / 100.0) as f32;
    let fill_rect = Rect::from_min_size(rect.min, Vec2::new(fill_width, rect.height()));
    if fill_width > 0.5 {
        painter.add(Shape::gradient_rect(
            fill_rect,
            egui::epaint::Direction::TopDown,
            [lighten(fill, 0.15), fill],
        ));
    }
    painter.rect_stroke(rect, 0.0, Stroke::new(1.0, p.border), StrokeKind::Inside);

    let text = match overlay {
        Some(t) => t.to_string(),
        None => format_percentage(pct),
    };
    if text.is_empty() {
        return;
    }
    let galley = painter.layout_no_wrap(text, font, Color32::PLACEHOLDER);
    let pos = Align2::CENTER_CENTER
        .anchor_size(rect.center(), galley.size())
        .min;

    let on_fill = painter.with_clip_rect(painter.clip_rect().intersect(fill_rect));
    on_fill.galley_with_override_text_color(pos, galley.clone(), p.text_on_accent);
    let track_rect = Rect::from_min_max(egui::pos2(rect.min.x + fill_width, rect.min.y), rect.max);
    let on_track = painter.with_clip_rect(painter.clip_rect().intersect(track_rect));
    on_track.galley_with_override_text_color(pos, galley, text_color);
}

/// A full-width bar laid out in `ui` at `height` (the details window's overall bar).
pub fn show(
    ui: &mut Ui,
    height: f32,
    percentage: f64,
    fill: Color32,
    overlay: Option<&str>,
) -> Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    paint(
        ui.painter(),
        rect,
        percentage,
        fill,
        overlay,
        theme::small_strong(),
        theme::palette().text,
    );
    response
}
