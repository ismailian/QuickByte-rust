//! The "start positions and download progress by connections" bar — port of
//! `Controls/ConnectionSegmentsBar.cs`: the file's width is divided into one
//! segment per connection (proportional to its byte range), each segment fills
//! with a colour for that connection's status, and a thin marker separates
//! adjacent connections' start positions.
//!
//! Fills are interpolated between the engine's ~100 ms snapshots by an
//! [`Animator`] of fractions, so the segments creep forward continuously and
//! never move backwards (a restarted connection snaps).

use std::time::Duration;

use egui::emath::GuiRounding as _;
use egui::epaint::Shape;
use egui::{Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2};
use quickbyte_core::models::ConnectionInfo;

use super::animator::Animator;
use super::progress_bar::color_for_connection;
use crate::icons::lighten;
use crate::theme;

#[derive(Clone, Debug, Default)]
pub struct SegmentsBar {
    fractions: Animator<i32>,
}

fn target_fraction(c: &ConnectionInfo) -> f64 {
    let total = c.total_bytes();
    if total <= 0 {
        0.0
    } else {
        (c.bytes_downloaded as f64 / total as f64).clamp(0.0, 1.0)
    }
}

impl SegmentsBar {
    pub fn new() -> Self {
        Self {
            fractions: Animator::fractions(),
        }
    }

    /// Takes a new snapshot of the connections.
    pub fn update(&mut self, connections: &[ConnectionInfo]) {
        for c in connections {
            self.fractions
                .set_target(c.connection_id, target_fraction(c), false);
        }
        self.fractions
            .retain(|id| connections.iter().any(|c| c.connection_id == *id));
    }

    pub fn advance(&mut self, elapsed: Duration) -> bool {
        self.fractions.advance_by(elapsed)
    }

    pub fn is_animating(&self) -> bool {
        self.fractions.is_animating()
    }

    pub fn show(
        &self,
        ui: &mut Ui,
        height: f32,
        connections: &[ConnectionInfo],
        total_bytes: i64,
    ) -> Response {
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let rect = rect.round_to_pixels(ui.pixels_per_point());
        let p = theme::palette();
        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, p.track);

        if !connections.is_empty() && total_bytes > 0 {
            let inner = rect.shrink(1.0);
            let clipped = painter.with_clip_rect(painter.clip_rect().intersect(inner));
            let mut ordered: Vec<&ConnectionInfo> = connections.iter().collect();
            ordered.sort_by_key(|c| c.range_start);
            let mut x = inner.left();
            let marker = Stroke::new(1.0, p.border_strong.gamma_multiply(0.6));
            for c in ordered {
                let width = c.total_bytes() as f32 / total_bytes as f32 * inner.width();
                let segment = Rect::from_min_size(
                    pos2(x, inner.top()),
                    Vec2::new(width.max(1.0), inner.height()),
                );
                let fraction = self
                    .fractions
                    .displayed_or(&c.connection_id, target_fraction(c));
                let fill_width = segment.width() * fraction as f32;
                if fill_width > 0.5 {
                    let color = color_for_connection(c.status);
                    let fill =
                        Rect::from_min_size(segment.min, Vec2::new(fill_width, segment.height()));
                    clipped.add(Shape::gradient_rect(
                        fill,
                        egui::epaint::Direction::TopDown,
                        [lighten(color, 0.2), color],
                    ));
                }
                x += width;
                if x < inner.right() - 0.5 {
                    clipped.vline(x.round() + 0.5, inner.y_range(), marker);
                }
            }
        }
        painter.rect_stroke(rect, 0.0, Stroke::new(1.0, p.border), StrokeKind::Inside);
        response
    }
}
