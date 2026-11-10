//! Every small icon the UI needs — toolbar buttons, sidebar tree nodes,
//! file-type badges in the downloads list, dialog glyphs — and the QuickByte
//! mark itself, drawn with egui's painter. Port of `Controls/IconFactory.cs`
//! and `Controls/BrandIcon.cs`.
//!
//! Nothing is an image file: each icon is a few vector shapes expressed as
//! fractions of its size (exactly as the GDI+ code was), so it stays crisp at
//! 16 px and at any DPI. Colours come from [`crate::theme`], so the icons follow
//! a live theme switch with the rest of the chrome.
//!
//! Fills go through epaint's anti-aliased tessellator. GDI+'s linear gradients
//! are reproduced with a gradient mesh laid over an anti-aliased base shape, so
//! the edge stays smooth and the face keeps its shading.

use std::cell::RefCell;

use egui::epaint::{Mesh, PathShape, PathStroke};
use egui::{Color32, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};
use quickbyte_core::helpers::file_category::FileCategory;

use crate::theme;

/// Every icon the app draws.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Icon {
    // Toolbar and menus
    AddUrl,
    Resume,
    Pause,
    Stop,
    StopAll,
    Delete,
    DeleteCompleted,
    Settings,
    Retry,
    OpenFolder,
    OpenFile,
    CheckCircle,
    Properties,
    Queue,
    Scheduler,
    SignIn,
    BatchDownload,
    RefreshAddress,
    Exit,
    Update,
    // Sidebar tree and rows
    Folder(Option<Color32>),
    Category(FileCategory),
    StatusDot(Color32),
    /// The product mark (`BrandIcon`).
    Brand,
}

impl Icon {
    /// Every icon that takes nothing but a size — the toolbar and menu set.
    pub const SIMPLE: [Icon; 20] = [
        Icon::AddUrl,
        Icon::Resume,
        Icon::Pause,
        Icon::Stop,
        Icon::StopAll,
        Icon::Delete,
        Icon::DeleteCompleted,
        Icon::Settings,
        Icon::Retry,
        Icon::OpenFolder,
        Icon::OpenFile,
        Icon::CheckCircle,
        Icon::Properties,
        Icon::Queue,
        Icon::Scheduler,
        Icon::SignIn,
        Icon::BatchDownload,
        Icon::RefreshAddress,
        Icon::Exit,
        Icon::Update,
    ];
}

/// The badge colour of a file category (`IconFactory.CategoryColor`).
pub fn category_color(category: FileCategory) -> Color32 {
    match category {
        FileCategory::Compressed => Color32::from_rgb(146, 110, 206),
        FileCategory::Documents => Color32::from_rgb(64, 122, 196),
        FileCategory::Music => Color32::from_rgb(210, 88, 148),
        FileCategory::Programs => Color32::from_rgb(58, 166, 118),
        FileCategory::Video => Color32::from_rgb(224, 104, 62),
        FileCategory::Pictures => Color32::from_rgb(46, 168, 190),
        FileCategory::Other => theme::palette().text_muted,
    }
}

/// Allocates a `size`-square and draws `icon` in it.
pub fn show(ui: &mut Ui, icon: Icon, size: f32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    paint(ui.painter(), rect, icon);
    response
}

/// Draws `icon` into `rect` (its shorter side is the icon's size, centred).
pub fn paint(painter: &Painter, rect: Rect, icon: Icon) {
    paint_ex(painter, rect, icon, false);
}

/// Draws `icon`, greyed and faded when `disabled` — what a WinForms ToolStrip
/// did to a disabled button's image.
pub fn paint_ex(painter: &Painter, rect: Rect, icon: Icon, disabled: bool) {
    painter.extend(shapes(rect, icon, disabled));
}

/// The shapes that draw `icon` into `rect`.
pub fn shapes(rect: Rect, icon: Icon, disabled: bool) -> Vec<Shape> {
    let size = rect.width().min(rect.height());
    let origin = rect.center() - Vec2::splat(size / 2.0);
    let g = Canvas {
        shapes: RefCell::new(Vec::new()),
        origin,
        size,
        disabled,
    };
    draw(&g, icon);
    g.shapes.into_inner()
}

fn draw(g: &Canvas, icon: Icon) {
    match icon {
        Icon::AddUrl => add_url(g),
        Icon::Resume => resume(g),
        Icon::Pause => pause(g),
        Icon::Stop => stop(g),
        Icon::StopAll => stop_all(g),
        Icon::Delete => delete(g),
        Icon::DeleteCompleted => delete_completed(g),
        Icon::Settings => settings(g),
        Icon::Retry => retry(g),
        Icon::OpenFolder => open_folder(g),
        Icon::OpenFile => open_file(g),
        Icon::CheckCircle => check_circle(g),
        Icon::Properties => properties(g),
        Icon::Queue => queue(g),
        Icon::Scheduler => scheduler(g),
        Icon::SignIn => sign_in(g),
        Icon::BatchDownload => batch_download(g),
        Icon::RefreshAddress => refresh_address(g),
        Icon::Exit => exit(g),
        Icon::Update => update(g),
        Icon::Folder(tint) => folder(g, tint),
        Icon::Category(category) => category_icon(g, category),
        Icon::StatusDot(color) => status_dot(g, color),
        Icon::Brand => brand(g),
    }
}

// --------------------------------------------------------------------------
// Canvas: GDI+-style drawing in fractions of the icon size
// --------------------------------------------------------------------------

/// Records shapes rather than painting them, so an icon is a pure function of
/// its rectangle (and testable without a GPU).
struct Canvas {
    shapes: RefCell<Vec<Shape>>,
    origin: Pos2,
    size: f32,
    disabled: bool,
}

const WHITE: Color32 = Color32::WHITE;

fn with_alpha(c: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), alpha)
}

/// `ControlPaint.Light`: towards white.
pub fn lighten(c: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    let mix = |a: u8| (a as f32 + (255.0 - a as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(mix(c.r()), mix(c.g()), mix(c.b()), c.a())
}

/// `ControlPaint.Dark`: towards black.
pub fn darken(c: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    let mix = |a: u8| (a as f32 * (1.0 - t)).round() as u8;
    Color32::from_rgba_unmultiplied(mix(c.r()), mix(c.g()), mix(c.b()), c.a())
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(
        m(a.r(), b.r()),
        m(a.g(), b.g()),
        m(a.b(), b.b()),
        m(a.a(), b.a()),
    )
}

impl Canvas {
    fn add(&self, shape: impl Into<Shape>) {
        self.shapes.borrow_mut().push(shape.into());
    }
    fn add_line_segment(&self, points: [Pos2; 2], stroke: Stroke) {
        self.add(Shape::line_segment(points, stroke));
    }
    fn add_circle(&self, center: Pos2, radius: f32, color: Color32) {
        self.add(Shape::circle_filled(center, radius, color));
    }
    fn s(&self, fraction: f32) -> f32 {
        self.size * fraction
    }
    fn p(&self, x: f32, y: f32) -> Pos2 {
        self.origin + vec2(self.size * x, self.size * y)
    }
    /// A rectangle in fractions: x, y, width, height.
    fn r(&self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_size(self.p(x, y), vec2(self.size * w, self.size * h))
    }

    /// Greys a colour out for a disabled button.
    fn c(&self, color: Color32) -> Color32 {
        if !self.disabled {
            return color;
        }
        let [r, g, b, a] = color.to_srgba_unmultiplied();
        let grey = (0.30 * r as f32 + 0.59 * g as f32 + 0.11 * b as f32) as u8;
        let p = theme::palette();
        // Pulled towards the chrome so it reads as inert, not just colourless.
        let base = Color32::from_rgba_unmultiplied(grey, grey, grey, a);
        let faded = lerp(base, p.chrome, 0.45);
        Color32::from_rgba_unmultiplied(faded.r(), faded.g(), faded.b(), a)
    }

    fn stroke(&self, width: f32, color: Color32) -> Stroke {
        Stroke::new(width, self.c(color))
    }

    fn ellipse(&self, rect: Rect, color: Color32) {
        self.add(Shape::ellipse_filled(
            rect.center(),
            rect.size() / 2.0,
            self.c(color),
        ));
    }

    fn ellipse_stroke(&self, rect: Rect, stroke: Stroke) {
        self.add(Shape::ellipse_stroke(
            rect.center(),
            rect.size() / 2.0,
            stroke,
        ));
    }

    /// An anti-aliased ellipse with a vertical gradient face (`LinearGradientMode.Vertical`).
    fn gradient_ellipse(&self, rect: Rect, top: Color32, bottom: Color32) {
        let (top, bottom) = (self.c(top), self.c(bottom));
        self.add(Shape::ellipse_filled(
            rect.center(),
            rect.size() / 2.0,
            lerp(top, bottom, 0.6),
        ));
        let inset = (rect.width() / 2.0 - 0.6).max(0.0) / (rect.width() / 2.0).max(0.001);
        let points = ellipse_points(rect.center(), rect.size() / 2.0 * inset, 40);
        self.gradient_fan(&points, rect, top, bottom, vec2(0.0, 1.0));
    }

    /// A convex polygon with a gradient face along `direction` across `bounds`.
    fn gradient_fan(
        &self,
        points: &[Pos2],
        bounds: Rect,
        from: Color32,
        to: Color32,
        direction: Vec2,
    ) {
        if points.len() < 3 {
            return;
        }
        let dir = direction.normalized();
        let corners = [
            bounds.left_top(),
            bounds.right_top(),
            bounds.left_bottom(),
            bounds.right_bottom(),
        ];
        let (lo, hi) = corners.iter().fold((f32::MAX, f32::MIN), |(lo, hi), c| {
            let d = c.to_vec2().dot(dir);
            (lo.min(d), hi.max(d))
        });
        let span = (hi - lo).max(0.001);
        let color_at = |p: Pos2| lerp(from, to, (p.to_vec2().dot(dir) - lo) / span);

        let mut mesh = Mesh::default();
        let centroid =
            points.iter().fold(Vec2::ZERO, |acc, p| acc + p.to_vec2()) / points.len() as f32;
        let centroid = centroid.to_pos2();
        mesh.colored_vertex(centroid, color_at(centroid));
        for p in points {
            mesh.colored_vertex(*p, color_at(*p));
        }
        let n = points.len() as u32;
        for i in 0..n {
            mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
        }
        self.add(Shape::mesh(mesh));
    }

    fn polygon(&self, points: Vec<Pos2>, color: Color32) {
        self.add(Shape::convex_polygon(points, self.c(color), Stroke::NONE));
    }

    fn round_rect(&self, rect: Rect, radius: f32, color: Color32) {
        self.add(Shape::convex_polygon(
            rounded_rect_points(rect, radius),
            self.c(color),
            Stroke::NONE,
        ));
    }

    fn round_rect_stroke(&self, points: Vec<Pos2>, stroke: Stroke) {
        self.add(PathShape::closed_line(points, PathStroke::from(stroke)));
    }

    fn rect(&self, rect: Rect, color: Color32) {
        self.add(Shape::rect_filled(rect, 0.0, self.c(color)));
    }

    /// A line with round caps (`LineCap.Round`).
    fn line(&self, a: Pos2, b: Pos2, stroke: Stroke) {
        self.add_line_segment([a, b], stroke);
        self.caps(&[a, b], stroke);
    }

    /// A polyline with round caps and joins.
    fn lines(&self, points: Vec<Pos2>, stroke: Stroke) {
        self.caps(&points, stroke);
        for p in points.iter().skip(1).take(points.len().saturating_sub(2)) {
            self.add_circle(*p, stroke.width / 2.0, stroke.color);
        }
        self.add(Shape::line(points, stroke));
    }

    fn caps(&self, points: &[Pos2], stroke: Stroke) {
        if let (Some(first), Some(last)) = (points.first(), points.last()) {
            self.add_circle(*first, stroke.width / 2.0, stroke.color);
            self.add_circle(*last, stroke.width / 2.0, stroke.color);
        }
    }

    /// `Graphics.DrawArc`: angles in degrees, clockwise from +x (y points down).
    fn arc(&self, rect: Rect, start_deg: f32, sweep_deg: f32, stroke: Stroke, round_caps: bool) {
        let points = arc_points(rect, start_deg, sweep_deg);
        if round_caps {
            self.caps(&points, stroke);
        }
        self.add(Shape::line(points, stroke));
    }

    /// The small round badge in the bottom-right corner (`DrawBadge`).
    fn badge(&self, color: Color32, kind: Badge) {
        let badge = self.r(0.50, 0.50, 0.46, 0.46);
        self.ellipse(badge.expand(1.0), Color32::from_black_alpha(40));
        self.ellipse(badge, color);

        let c = badge.center();
        let (w, h) = (badge.width(), badge.height());
        let pen = self.stroke((self.s(0.075)).max(1.4), WHITE);
        match kind {
            Badge::Plus => {
                self.line(pos2(c.x - w * 0.22, c.y), pos2(c.x + w * 0.22, c.y), pen);
                self.line(pos2(c.x, c.y - h * 0.22), pos2(c.x, c.y + h * 0.22), pen);
            }
            Badge::Arrow => {
                self.line(
                    pos2(c.x - w * 0.20, c.y + h * 0.16),
                    pos2(c.x + w * 0.18, c.y - h * 0.18),
                    pen,
                );
                self.lines(
                    vec![
                        pos2(c.x - w * 0.02, c.y - h * 0.20),
                        pos2(c.x + w * 0.20, c.y - h * 0.20),
                        pos2(c.x + w * 0.20, c.y + h * 0.02),
                    ],
                    pen,
                );
            }
            Badge::Check => {
                self.lines(
                    vec![
                        pos2(c.x - w * 0.22, c.y),
                        pos2(c.x - w * 0.05, c.y + h * 0.18),
                        pos2(c.x + w * 0.24, c.y - h * 0.20),
                    ],
                    pen,
                );
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Badge {
    Plus,
    Arrow,
    Check,
}

fn ellipse_points(center: Pos2, radius: Vec2, segments: usize) -> Vec<Pos2> {
    (0..segments)
        .map(|i| {
            let a = i as f32 / segments as f32 * std::f32::consts::TAU;
            center + vec2(radius.x * a.cos(), radius.y * a.sin())
        })
        .collect()
}

fn arc_points(rect: Rect, start_deg: f32, sweep_deg: f32) -> Vec<Pos2> {
    let segments = ((sweep_deg.abs() / 8.0).ceil() as usize).max(4);
    let (c, rx, ry) = (rect.center(), rect.width() / 2.0, rect.height() / 2.0);
    (0..=segments)
        .map(|i| {
            let a = (start_deg + sweep_deg * i as f32 / segments as f32).to_radians();
            pos2(c.x + rx * a.cos(), c.y + ry * a.sin())
        })
        .collect()
}

/// `Theme.RoundedRect`: a rectangle with circular corners of `radius`, as a
/// convex outline (float radius — epaint's own rounded rect rounds to whole points,
/// which is visible at 16 px).
pub fn rounded_rect_points(rect: Rect, radius: f32) -> Vec<Pos2> {
    let r = radius
        .min(rect.width() / 2.0)
        .min(rect.height() / 2.0)
        .max(0.0);
    if r <= 0.01 {
        return vec![
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ];
    }
    let mut points = Vec::with_capacity(4 * 7);
    let corners = [
        (pos2(rect.right() - r, rect.top() + r), -90.0_f32),
        (pos2(rect.right() - r, rect.bottom() - r), 0.0),
        (pos2(rect.left() + r, rect.bottom() - r), 90.0),
        (pos2(rect.left() + r, rect.top() + r), 180.0),
    ];
    for (center, start) in corners {
        for i in 0..=6 {
            let a = (start + 90.0 * i as f32 / 6.0).to_radians();
            points.push(center + vec2(r * a.cos(), r * a.sin()));
        }
    }
    points
}

// --------------------------------------------------------------------------
// Toolbar
// --------------------------------------------------------------------------

fn add_url(g: &Canvas) {
    let p = theme::palette();
    let inset = 0.08;
    let globe = g.r(inset, inset, 0.7, 0.7);
    g.add(Shape::ellipse_filled(
        globe.center(),
        globe.size() / 2.0,
        g.c(p.accent),
    ));
    let inner = ellipse_points(globe.center(), globe.size() / 2.0 - Vec2::splat(0.6), 40);
    g.gradient_fan(
        &inner,
        globe,
        g.c(p.accent_light),
        g.c(p.accent),
        vec2(1.0, 1.0),
    );

    let pen = g.stroke(g.s(0.045).max(1.0), with_alpha(WHITE, 140));
    g.add_line_segment(
        [
            pos2(globe.left() + globe.width() * 0.06, globe.center().y),
            pos2(globe.right() - globe.width() * 0.06, globe.center().y),
        ],
        pen,
    );
    g.ellipse_stroke(
        Rect::from_min_size(
            pos2(globe.left() + globe.width() * 0.28, globe.top()),
            vec2(globe.width() * 0.44, globe.height()),
        ),
        pen,
    );
    g.badge(p.success, Badge::Plus);
}

fn resume(g: &Canvas) {
    let p = theme::palette();
    g.gradient_ellipse(g.r(0.06, 0.06, 0.88, 0.88), p.success_light, p.success);
    g.polygon(
        vec![g.p(0.40, 0.29), g.p(0.73, 0.50), g.p(0.40, 0.71)],
        WHITE,
    );
}

fn pause(g: &Canvas) {
    let p = theme::palette();
    g.gradient_ellipse(
        g.r(0.06, 0.06, 0.88, 0.88),
        lighten(p.warning, 0.3),
        p.warning,
    );
    g.rect(g.r(0.37, 0.30, 0.11, 0.40), WHITE);
    g.rect(g.r(0.53, 0.30, 0.11, 0.40), WHITE);
}

fn stop(g: &Canvas) {
    let p = theme::palette();
    g.gradient_ellipse(
        g.r(0.06, 0.06, 0.88, 0.88),
        lighten(p.danger, 0.3),
        p.danger,
    );
    g.round_rect(g.r(0.34, 0.34, 0.32, 0.32), g.s(0.05), WHITE);
}

fn stop_all(g: &Canvas) {
    let p = theme::palette();
    g.round_rect(
        g.r(0.14, 0.22, 0.34, 0.56),
        g.s(0.06),
        lighten(p.danger, 0.45),
    );
    g.round_rect(g.r(0.52, 0.22, 0.34, 0.56), g.s(0.06), p.danger);
}

fn delete(g: &Canvas) {
    let body = Color32::from_rgb(224, 92, 92);
    let lid = Color32::from_rgb(196, 70, 70);
    g.round_rect(g.r(0.16, 0.20, 0.68, 0.11), g.s(0.045), lid);
    g.rect(g.r(0.40, 0.12, 0.20, 0.08), lid);
    g.round_rect(g.r(0.24, 0.32, 0.52, 0.54), g.s(0.07), body);
    let pen = g.stroke(g.s(0.05).max(1.0), with_alpha(WHITE, 200));
    for i in 0..3 {
        let x = 0.36 + i as f32 * 0.14;
        g.add_line_segment([g.p(x, 0.44), g.p(x, 0.74)], pen);
    }
}

fn delete_completed(g: &Canvas) {
    let body = Color32::from_rgb(224, 92, 92);
    let lid = Color32::from_rgb(196, 70, 70);
    g.round_rect(g.r(0.08, 0.20, 0.60, 0.10), g.s(0.04), lid);
    g.round_rect(g.r(0.15, 0.31, 0.46, 0.50), g.s(0.06), body);
    g.badge(theme::palette().success, Badge::Check);
}

fn settings(g: &Canvas) {
    let center = g.p(0.5, 0.5);
    let (outer_r, mid_r, inner_r) = (g.s(0.42), g.s(0.28), g.s(0.12));
    let dark = theme::is_dark();
    let teeth = if dark {
        Color32::from_rgb(150, 162, 176)
    } else {
        Color32::from_rgb(108, 122, 137)
    };
    let ring = if dark {
        Color32::from_rgb(128, 140, 156)
    } else {
        Color32::from_rgb(88, 102, 118)
    };
    for i in 0..8 {
        let angle = i as f32 * std::f32::consts::FRAC_PI_4;
        let tc = center + vec2(angle.cos(), angle.sin()) * (outer_r - g.s(0.06));
        g.add_circle(tc, g.s(0.075), g.c(teeth));
    }
    g.add_circle(center, mid_r, g.c(ring));
    // The hole shows what is behind the gear, as the white fill did on a white toolbar.
    g.add_circle(center, inner_r, g.c(theme::palette().chrome));
}

fn retry(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.11).max(1.6), p.accent);
    g.arc(g.r(0.16, 0.16, 0.68, 0.68), 40.0, 280.0, pen, true);
    g.polygon(
        vec![g.p(0.80, 0.10), g.p(0.86, 0.44), g.p(0.52, 0.36)],
        p.accent,
    );
}

fn open_folder(g: &Canvas) {
    let back = Color32::from_rgb(238, 186, 78);
    let front = Color32::from_rgb(252, 208, 108);
    g.rect(g.r(0.10, 0.22, 0.42, 0.14), back);
    g.round_rect(g.r(0.10, 0.28, 0.80, 0.50), g.s(0.06), back);
    g.round_rect(g.r(0.16, 0.40, 0.78, 0.38), g.s(0.06), front);
}

fn open_file(g: &Canvas) {
    let p = theme::palette();
    let body = g.r(0.22, 0.10, 0.50, 0.68);
    let outline = rounded_rect_points(body, g.s(0.05));
    g.add(Shape::convex_polygon(
        outline.clone(),
        g.c(WHITE),
        Stroke::NONE,
    ));
    g.round_rect_stroke(outline, g.stroke(g.s(0.055).max(1.0), p.border_strong));
    let pen = g.stroke(g.s(0.05).max(1.0), p.accent);
    for i in 0..3 {
        let y = body.top() + body.height() * (0.28 + i as f32 * 0.20);
        g.add_line_segment(
            [
                pos2(body.left() + body.width() * 0.18, y),
                pos2(body.right() - body.width() * 0.18, y),
            ],
            pen,
        );
    }
    g.badge(p.accent, Badge::Arrow);
}

fn check_circle(g: &Canvas) {
    let p = theme::palette();
    g.gradient_ellipse(g.r(0.06, 0.06, 0.88, 0.88), p.success_light, p.success);
    let pen = g.stroke(g.s(0.10).max(1.6), WHITE);
    g.lines(vec![g.p(0.29, 0.52), g.p(0.44, 0.67), g.p(0.72, 0.34)], pen);
}

fn properties(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.09).max(1.4), p.accent);
    g.ellipse_stroke(g.r(0.12, 0.12, 0.76, 0.76), pen);
    g.ellipse(g.r(0.44, 0.24, 0.13, 0.13), p.accent);
    g.rect(g.r(0.44, 0.44, 0.13, 0.32), p.accent);
}

fn queue(g: &Canvas) {
    let p = theme::palette();
    let dim = with_alpha(p.accent, 120);
    for i in 0..3 {
        let y = 0.24 + i as f32 * 0.24;
        g.ellipse(g.r(0.16, y, 0.12, 0.12), p.accent);
        g.round_rect(
            g.r(0.36, y + 0.015, 0.48, 0.09),
            g.s(0.045),
            if i == 0 { p.accent } else { dim },
        );
    }
}

fn scheduler(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.09).max(1.4), p.accent);
    let clock = g.r(0.14, 0.14, 0.72, 0.72);
    g.ellipse_stroke(clock, pen);
    let c = clock.center();
    g.line(c, pos2(c.x, c.y - clock.height() * 0.28), pen);
    g.line(c, pos2(c.x + clock.width() * 0.22, c.y), pen);
}

/// Padlock for the sign-in dialog. A closed shackle: the window it heads is the
/// door still being refused; amber for "something is wanted from you".
fn sign_in(g: &Canvas) {
    let p = theme::palette();
    let body = g.r(0.20, 0.44, 0.60, 0.42);
    let pen = g.stroke(g.s(0.10).max(1.4), p.warning);
    let shackle = g.r(0.31, 0.14, 0.38, 0.38);
    g.arc(shackle, 180.0, 180.0, pen, false);
    g.add_line_segment(
        [
            pos2(shackle.left(), shackle.center().y),
            pos2(shackle.left(), body.top()),
        ],
        pen,
    );
    g.add_line_segment(
        [
            pos2(shackle.right(), shackle.center().y),
            pos2(shackle.right(), body.top()),
        ],
        pen,
    );

    let outline = rounded_rect_points(body, g.s(0.10));
    g.add(Shape::convex_polygon(
        outline.clone(),
        g.c(p.warning),
        Stroke::NONE,
    ));
    g.gradient_fan(
        &outline,
        body,
        g.c(Color32::from_rgb(240, 190, 80)),
        g.c(p.warning),
        vec2(0.0, 1.0),
    );

    let hole = g.s(0.13);
    g.ellipse(
        Rect::from_min_size(
            pos2(
                body.left() + (body.width() - hole) / 2.0,
                body.top() + body.height() * 0.22,
            ),
            Vec2::splat(hole),
        ),
        WHITE,
    );
    g.rect(
        Rect::from_min_size(
            pos2(
                body.left() + (body.width() - hole * 0.42) / 2.0,
                body.top() + body.height() * 0.42,
            ),
            vec2(hole * 0.42, body.height() * 0.30),
        ),
        WHITE,
    );
}

/// Three link rows with an add badge: the Add URL globe means one address, and
/// this is many of them at once.
fn batch_download(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.06).max(1.0), with_alpha(WHITE, 200));
    for i in 0..3 {
        let row = g.r(0.06, 0.10 + i as f32 * 0.24, 0.62, 0.17);
        g.round_rect(
            row,
            g.s(0.05),
            if i == 0 { p.accent } else { p.accent_light },
        );
        g.add_line_segment(
            [
                pos2(row.left() + row.width() * 0.18, row.center().y),
                pos2(row.right() - row.width() * 0.18, row.center().y),
            ],
            pen,
        );
    }
    g.badge(p.success, Badge::Plus);
}

/// Two chain links with a circular-arrow badge, for Refresh Download Address.
fn refresh_address(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.10).max(1.5), p.accent);
    let pivot = g.p(0.40, 0.40);
    let rot = |q: Pos2| {
        let (s, c) = (-45.0_f32).to_radians().sin_cos();
        let d = q.to_vec2();
        pivot + vec2(d.x * c - d.y * s, d.x * s + d.y * c)
    };
    for x in [-0.36, -0.04] {
        let local = Rect::from_min_size(pos2(g.s(x), -g.s(0.13)), vec2(g.s(0.40), g.s(0.26)));
        let points = rounded_rect_points(local, g.s(0.12))
            .into_iter()
            .map(rot)
            .collect();
        g.round_rect_stroke(points, pen);
    }
    g.badge(p.success, Badge::Arrow);
}

/// Power glyph for Exit — the one command that really closes the app.
fn exit(g: &Canvas) {
    let p = theme::palette();
    let pen = g.stroke(g.s(0.11).max(1.4), p.danger);
    g.arc(g.r(0.18, 0.20, 0.64, 0.64), -60.0, 300.0, pen, true);
    g.line(g.p(0.50, 0.14), g.p(0.50, 0.46), pen);
}

/// An arrow pointing *up* out of a disc: every download arrow points down, and
/// an update has to read as a different kind of transfer at 16 px.
fn update(g: &Canvas) {
    let p = theme::palette();
    g.gradient_ellipse(g.r(0.06, 0.06, 0.88, 0.88), p.accent_light, p.accent);
    let pen = g.stroke(g.s(0.10).max(1.5), WHITE);
    g.line(g.p(0.50, 0.72), g.p(0.50, 0.32), pen);
    g.lines(vec![g.p(0.30, 0.51), g.p(0.50, 0.28), g.p(0.70, 0.51)], pen);
}

// --------------------------------------------------------------------------
// Sidebar tree and rows
// --------------------------------------------------------------------------

fn folder(g: &Canvas, tint: Option<Color32>) {
    let color = tint.unwrap_or(Color32::from_rgb(246, 196, 84));
    g.rect(g.r(0.06, 0.16, 0.44, 0.16), darken(color, 0.05));
    g.round_rect(g.r(0.06, 0.25, 0.88, 0.56), g.s(0.09), color);
}

fn category_icon(g: &Canvas, category: FileCategory) {
    let color = category_color(category);
    let rect = g.r(0.12, 0.06, 0.76, 0.88);
    g.round_rect(rect, g.s(0.16), color);

    let pen = g.stroke(g.s(0.075).max(1.0), WHITE);
    let at = |x: f32, y: f32| {
        pos2(
            rect.left() + rect.width() * x,
            rect.top() + rect.height() * y,
        )
    };
    let sub = |x: f32, y: f32, w: f32, h: f32| {
        Rect::from_min_size(at(x, y), vec2(rect.width() * w, rect.height() * h))
    };

    match category {
        FileCategory::Music => {
            g.ellipse_stroke(sub(0.22, 0.56, 0.24, 0.22), pen);
            g.line(at(0.46, 0.67), at(0.46, 0.24), pen);
            g.line(at(0.46, 0.24), at(0.72, 0.32), pen);
        }
        FileCategory::Video => {
            g.polygon(vec![at(0.36, 0.28), at(0.72, 0.50), at(0.36, 0.72)], WHITE);
        }
        FileCategory::Compressed => {
            for i in 0..4 {
                let y = 0.20 + i as f32 * 0.16;
                let offset = if i % 2 == 0 { -0.07 } else { 0.07 };
                g.rect(sub(0.44 + offset, y, 0.14, 0.10), WHITE);
            }
        }
        FileCategory::Documents => {
            for i in 0..3 {
                let y = 0.32 + i as f32 * 0.19;
                let right = if i == 2 { 0.58 } else { 0.72 };
                g.line(at(0.28, y), at(right, y), pen);
            }
        }
        FileCategory::Programs => {
            g.rect(sub(0.28, 0.30, 0.44, 0.40), WHITE);
            g.rect(sub(0.40, 0.42, 0.20, 0.16), color);
        }
        FileCategory::Pictures => {
            g.ellipse(sub(0.28, 0.26, 0.18, 0.18), WHITE);
            g.polygon(vec![at(0.22, 0.76), at(0.48, 0.46), at(0.78, 0.76)], WHITE);
        }
        FileCategory::Other => {
            g.line(at(0.30, 0.38), at(0.70, 0.38), pen);
            g.line(at(0.30, 0.60), at(0.58, 0.60), pen);
        }
    }
}

fn status_dot(g: &Canvas, color: Color32) {
    g.ellipse(g.r(0.14, 0.14, 0.72, 0.72), with_alpha(color, 60));
    g.ellipse(g.r(0.29, 0.29, 0.42, 0.42), color);
}

// --------------------------------------------------------------------------
// BrandIcon
// --------------------------------------------------------------------------

/// The QuickByte product mark: a rounded accent tile carrying a download arrow
/// over a three-segment tray — the segments being the one visual cue that this
/// is a *multi-connection* downloader rather than a generic one.
fn brand(g: &Canvas) {
    let p = theme::palette();
    // Below ~24 px the shadow and the tray gaps land on sub-pixel boundaries
    // and turn into grey mush, so the small variant drops both.
    let detailed = g.size >= 24.0;

    let inset = 0.045;
    let tile = g.r(inset, inset, 1.0 - inset * 2.0, 1.0 - inset * 2.0);
    let outline = rounded_rect_points(tile, g.s(0.225));
    g.add(Shape::convex_polygon(
        outline.clone(),
        g.c(lerp(p.accent_light, p.accent_dark, 0.5)),
        Stroke::NONE,
    ));
    let shrink =
        |q: &Pos2| tile.center() + (*q - tile.center()) * (1.0 - 0.8 / tile.width().max(1.0));
    let inner: Vec<Pos2> = outline.iter().map(shrink).collect();
    // ForwardDiagonal, as the GDI+ brush ran.
    let (light, dark) = (Palette::fixed().accent_light, Palette::fixed().accent_dark);
    g.gradient_fan(&inner, tile, g.c(light), g.c(dark), vec2(1.0, 1.0));

    // The gloss over the top half keeps the tile from reading as a flat block.
    let gloss_top = with_alpha(WHITE, 58);
    let gloss_points: Vec<Pos2> = inner
        .iter()
        .map(|q| pos2(q.x, q.y.min(tile.top() + tile.height() * 0.60)))
        .collect();
    g.gradient_fan(
        &gloss_points,
        Rect::from_min_size(tile.min, vec2(tile.width(), tile.height() * 0.60)),
        gloss_top,
        Color32::TRANSPARENT,
        vec2(0.0, 1.0),
    );

    g.round_rect_stroke(
        outline,
        Stroke::new(
            g.s(0.02).max(1.0),
            g.c(Color32::from_rgba_unmultiplied(0, 30, 60, 64)),
        ),
    );

    if detailed {
        brand_mark(
            g,
            Color32::from_rgba_unmultiplied(0, 24, 48, 48),
            g.s(0.022),
            detailed,
        );
    }
    brand_mark(g, WHITE, 0.0, detailed);
}

/// The arrow and its tray, painted in one colour so the drop shadow is the same
/// call with an offset rather than a second, drifting definition.
fn brand_mark(g: &Canvas, color: Color32, offset: f32, detailed: bool) {
    let f = |x: f32, y: f32| g.p(x, y) + Vec2::splat(offset);
    let size = g.size;
    g.round_rect(
        Rect::from_min_size(f(0.422, 0.210), vec2(size * 0.156, size * 0.290)),
        size * 0.055,
        color,
    );
    g.polygon(
        vec![f(0.275, 0.435), f(0.725, 0.435), f(0.500, 0.705)],
        color,
    );

    let tray_height = size * 0.085;
    if !detailed {
        g.round_rect(
            Rect::from_min_size(f(0.250, 0.775), vec2(size * 0.5, tray_height)),
            tray_height / 2.0,
            color,
        );
        return;
    }
    let gap = 0.028;
    let segment = (0.500 - gap * 2.0) / 3.0;
    for i in 0..3 {
        let x = 0.250 + i as f32 * (segment + gap);
        g.round_rect(
            Rect::from_min_size(f(x, 0.775), vec2(size * segment, tray_height)),
            tray_height / 2.0,
            color,
        );
    }
}

/// The brand is the brand: its blue does not shift with the dark palette's lifted accent.
struct Palette;
impl Palette {
    fn fixed() -> &'static theme::Palette {
        &theme::Palette::LIGHT
    }
}

/// Rasterises the mark into RGBA pixels (for the window icon) — a tiny scanline
/// renderer over the same geometry, supersampled 4×4 per pixel.
pub fn brand_rgba(size: u32) -> Vec<u8> {
    let n = size as f32;
    let ss = 4;
    let mut out = vec![0u8; (size * size * 4) as usize];
    let light = theme::Palette::LIGHT.accent_light;
    let dark = theme::Palette::LIGHT.accent_dark;
    let inset = n * 0.045;
    let tile = Rect::from_min_max(pos2(inset, inset), pos2(n - inset, n - inset));
    let radius = n * 0.225;
    let in_round = |p: Pos2, r: Rect, rad: f32| {
        if !r.contains(p) {
            return false;
        }
        let rad = rad.min(r.width() / 2.0).min(r.height() / 2.0);
        // max/min rather than clamp: at a half-height radius the bounds can cross by an ulp.
        let cx = p.x.max(r.left() + rad).min(r.right() - rad);
        let cy = p.y.max(r.top() + rad).min(r.bottom() - rad);
        (p.x - cx).powi(2) + (p.y - cy).powi(2) <= rad * rad
    };
    let tray_h = n * 0.085;
    let stem = Rect::from_min_size(pos2(n * 0.422, n * 0.210), vec2(n * 0.156, n * 0.290));
    let tri = [
        pos2(n * 0.275, n * 0.435),
        pos2(n * 0.725, n * 0.435),
        pos2(n * 0.5, n * 0.705),
    ];
    let in_tri = |p: Pos2| {
        let s = |a: Pos2, b: Pos2| (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
        let (d1, d2, d3) = (s(tri[0], tri[1]), s(tri[1], tri[2]), s(tri[2], tri[0]));
        (d1 >= 0.0 && d2 >= 0.0 && d3 >= 0.0) || (d1 <= 0.0 && d2 <= 0.0 && d3 <= 0.0)
    };
    let detailed = size >= 24;
    let gap = 0.028;
    let seg = (0.5 - gap * 2.0) / 3.0;
    let in_mark = |p: Pos2| {
        if in_round(p, stem, n * 0.055) || in_tri(p) {
            return true;
        }
        if detailed {
            (0..3).any(|i| {
                let x = 0.25 + i as f32 * (seg + gap);
                in_round(
                    p,
                    Rect::from_min_size(pos2(n * x, n * 0.775), vec2(n * seg, tray_h)),
                    tray_h / 2.0,
                )
            })
        } else {
            in_round(
                p,
                Rect::from_min_size(pos2(n * 0.25, n * 0.775), vec2(n * 0.5, tray_h)),
                tray_h / 2.0,
            )
        }
    };
    for y in 0..size {
        for x in 0..size {
            let (mut r, mut gg, mut b, mut a) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for sy in 0..ss {
                for sx in 0..ss {
                    let p = pos2(
                        x as f32 + (sx as f32 + 0.5) / ss as f32,
                        y as f32 + (sy as f32 + 0.5) / ss as f32,
                    );
                    if !in_round(p, tile, radius) {
                        continue;
                    }
                    let c = if in_mark(p) {
                        WHITE
                    } else {
                        let t = ((p.x - tile.left()) + (p.y - tile.top()))
                            / (tile.width() + tile.height());
                        lerp(light, dark, t)
                    };
                    r += c.r() as f32;
                    gg += c.g() as f32;
                    b += c.b() as f32;
                    a += 255.0;
                }
            }
            let count = (ss * ss) as f32;
            let i = ((y * size + x) * 4) as usize;
            if a > 0.0 {
                let cover = a / 255.0;
                out[i] = (r / cover) as u8;
                out[i + 1] = (gg / cover) as u8;
                out[i + 2] = (b / cover) as u8;
                out[i + 3] = (a / count) as u8;
            }
        }
    }
    out
}

/// The window icon: the brand mark at 32 px.
pub fn window_icon() -> egui::IconData {
    let size = 32;
    egui::IconData {
        rgba: brand_rgba(size),
        width: size,
        height: size,
    }
}

#[cfg(test)]
mod tests {
    //! Every icon draws at the sizes the app asks for, and puts something on the
    //! canvas — port of `IconFactoryTests.cs` / `BrandIconTests.cs`. Nothing here
    //! judges how they look.

    use super::*;

    /// How many shapes the icon drew that actually cover something.
    fn shapes_for(icon: Icon, size: f32) -> usize {
        let rect = Rect::from_min_size(pos2(10.0, 10.0), Vec2::splat(size));
        let drawn = shapes(rect, icon, false);
        // Everything an icon draws stays inside its square (a hard-coded inset
        // that went negative at 16 px would show up here).
        for shape in &drawn {
            let bounds = shape.visual_bounding_rect();
            assert!(
                rect.expand(2.0).contains_rect(bounds),
                "{icon:?}({size}) drew outside its square: {bounds:?}"
            );
        }
        drawn
            .iter()
            .filter(|s| s.visual_bounding_rect().area() > 0.0)
            .count()
    }

    #[test]
    fn an_icon_draws_at_every_size_the_app_asks_for() {
        for icon in Icon::SIMPLE {
            for size in [16.0, 24.0, 48.0] {
                assert!(shapes_for(icon, size) > 0, "{icon:?}({size}) drew nothing");
            }
        }
    }

    #[test]
    fn every_file_category_has_a_badge_and_a_colour() {
        for category in FileCategory::ALL {
            assert!(
                shapes_for(Icon::Category(category), 16.0) > 0,
                "{category} drew nothing"
            );
            assert_ne!(category_color(category), Color32::TRANSPARENT);
        }
    }

    #[test]
    fn a_status_dot_and_a_folder_draw() {
        assert!(shapes_for(Icon::StatusDot(Color32::RED), 16.0) > 0);
        assert!(shapes_for(Icon::Folder(None), 16.0) > 0);
        assert!(shapes_for(Icon::Folder(Some(Color32::RED)), 16.0) > 0);
        assert!(shapes_for(Icon::Brand, 16.0) > 0);
        assert!(shapes_for(Icon::Brand, 48.0) > 0);
    }

    #[test]
    fn the_rasterised_mark_has_ink_and_transparent_corners() {
        let px = brand_rgba(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        assert_eq!(px[3], 0, "the corner is outside the rounded tile");
        let centre = ((16 * 32 + 16) * 4) as usize;
        assert_eq!(px[centre + 3], 255);
    }

    #[test]
    fn rounded_rect_outlines_degrade_to_a_plain_rectangle() {
        let r = Rect::from_min_size(Pos2::ZERO, vec2(10.0, 4.0));
        assert_eq!(rounded_rect_points(r, 0.0).len(), 4);
        assert!(
            rounded_rect_points(r, 2.0)
                .iter()
                .all(|p| r.expand(0.01).contains(*p))
        );
    }
}
