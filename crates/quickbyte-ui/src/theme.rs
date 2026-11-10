//! The single source of truth for the app's palette, fonts and spacing — the
//! port of `Controls/Theme.cs`.
//!
//! Every window pulls its colours from here instead of egui's defaults, which is
//! what keeps the windows looking like one flat, deliberate product.
//!
//! There are two palettes, light and dark, carried value for value from the C#
//! `Palette` record. One improvement over WinForms: egui restyles every frame, so
//! a theme change applies the moment it is made, with no restart.
//!
//! Type scale and spacing follow MIGRATION.md §7.2 — the user's explicit
//! requirement: **12 px text everywhere**, small compact controls.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::epaint::Shadow;
use egui::style::{ScrollStyle, Selection, WidgetVisuals, Widgets};
use egui::{
    Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, Style,
    TextStyle, Vec2, Visuals,
};
use quickbyte_core::enums::AppTheme;

// --------------------------------------------------------------------------
// Palette
// --------------------------------------------------------------------------

/// The colours of one theme. Light is the palette QuickByte has always had;
/// dark keeps the same roles and the same accent hue, lifted so blue text and
/// bars stay readable on a dark surface. The tests hold both to readable contrast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub window: Color32,
    pub surface: Color32,
    pub surface_alt: Color32,
    pub chrome: Color32,
    pub sidebar: Color32,
    pub header_back: Color32,
    pub track: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    pub text_on_accent: Color32,
    pub accent: Color32,
    pub accent_dark: Color32,
    pub accent_light: Color32,
    pub accent_soft: Color32,
    /// A toolbar button held down — a step deeper than `accent_soft`.
    pub accent_pressed: Color32,
    pub success: Color32,
    pub success_light: Color32,
    pub warning: Color32,
    pub danger: Color32,
    pub row_selected: Color32,
    pub row_selected_inactive: Color32,
    pub row_alt: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        window: rgb(243, 246, 249),
        surface: rgb(255, 255, 255),
        surface_alt: rgb(250, 251, 253),
        chrome: rgb(252, 253, 254),
        sidebar: rgb(248, 250, 252),
        header_back: rgb(249, 250, 252),
        track: rgb(232, 237, 242),
        border: rgb(223, 229, 236),
        border_strong: rgb(200, 209, 219),
        text: rgb(30, 37, 46),
        text_muted: rgb(112, 123, 136),
        text_disabled: rgb(163, 172, 182),
        text_on_accent: rgb(255, 255, 255),
        accent: rgb(0, 114, 198),
        accent_dark: rgb(0, 84, 152),
        accent_light: rgb(74, 163, 231),
        accent_soft: rgb(232, 242, 252),
        accent_pressed: rgb(212, 231, 249),
        success: rgb(32, 152, 86),
        success_light: rgb(72, 190, 122),
        warning: rgb(216, 152, 26),
        danger: rgb(205, 68, 68),
        row_selected: rgb(0, 114, 198),
        row_selected_inactive: rgb(224, 233, 242),
        row_alt: rgb(250, 251, 253),
    };

    pub const DARK: Palette = Palette {
        window: rgb(27, 29, 33),
        surface: rgb(34, 36, 41),
        surface_alt: rgb(39, 42, 47),
        chrome: rgb(30, 32, 37),
        sidebar: rgb(31, 33, 38),
        header_back: rgb(40, 43, 49),
        track: rgb(52, 56, 63),
        border: rgb(55, 59, 66),
        border_strong: rgb(78, 83, 92),
        text: rgb(230, 233, 237),
        text_muted: rgb(158, 166, 176),
        text_disabled: rgb(108, 115, 124),
        text_on_accent: rgb(255, 255, 255),
        // Balanced between its two jobs: text on a dark surface (4.7:1) and the
        // background of a primary button under white text (3.3:1).
        accent: rgb(56, 146, 220),
        accent_dark: rgb(24, 104, 170),
        accent_light: rgb(104, 180, 238),
        accent_soft: rgb(37, 56, 77),
        accent_pressed: rgb(45, 72, 101),
        success: rgb(76, 187, 118),
        success_light: rgb(108, 208, 146),
        warning: rgb(232, 176, 64),
        danger: rgb(236, 104, 104),
        row_selected: rgb(24, 104, 170),
        row_selected_inactive: rgb(52, 60, 71),
        row_alt: rgb(38, 40, 45),
    };
}

// --------------------------------------------------------------------------
// The palette in use
// --------------------------------------------------------------------------

static DARK: AtomicBool = AtomicBool::new(false);

/// Whether the dark palette is in use.
pub fn is_dark() -> bool {
    DARK.load(Ordering::Relaxed)
}

/// The palette in use. Every painter reads its colours through this, so a live
/// theme switch reaches icons and owner-drawn cells as well as egui's widgets.
pub fn palette() -> &'static Palette {
    if is_dark() {
        &Palette::DARK
    } else {
        &Palette::LIGHT
    }
}

/// Light or dark. "System" follows Windows' app mode, and a machine that does
/// not say — older Windows, a locked-down profile — gets the light palette the
/// app always had.
pub fn resolve(preference: AppTheme, windows_prefers_dark: Option<bool>) -> bool {
    match preference {
        AppTheme::Light => false,
        AppTheme::Dark => true,
        AppTheme::System => windows_prefers_dark == Some(true),
    }
}

/// Switches every viewport to the light or dark palette. Cheap to call when
/// nothing changed (it returns false and does nothing).
pub fn apply(ctx: &egui::Context, dark: bool) -> bool {
    let changed = DARK.swap(dark, Ordering::Relaxed) != dark;
    let style = Arc::new(style_for(
        if dark {
            &Palette::DARK
        } else {
            &Palette::LIGHT
        },
        dark,
    ));
    ctx.set_style_of(egui::Theme::Light, (*style).clone());
    ctx.set_style_of(egui::Theme::Dark, (*style).clone());
    ctx.set_theme(if dark {
        egui::ThemePreference::Dark
    } else {
        egui::ThemePreference::Light
    });
    changed
}

/// The native title bar's theme for a viewport (winit's `set_theme` asks DWM for
/// the immersive dark caption, which is what `DarkMode.UseDarkTitleBar` did).
pub fn system_theme() -> egui::SystemTheme {
    if is_dark() {
        egui::SystemTheme::Dark
    } else {
        egui::SystemTheme::Light
    }
}

// --------------------------------------------------------------------------
// Type scale
// --------------------------------------------------------------------------

/// Body / Button / Monospace (MIGRATION.md §7.2).
pub const BODY: f32 = 12.0;
/// Status bar, hints, captions.
pub const SMALL: f32 = 11.0;
/// Dialog titles inside a window.
pub const HEADING: f32 = 13.0;

/// Toolbar buttons, table rows and the like.
pub const ROW_HEIGHT: f32 = 20.0;
pub const INTERACT_HEIGHT: f32 = 20.0;
pub const CORNER_RADIUS: u8 = 3;

/// The family strong text is set in: Segoe UI Semibold when Windows has it.
pub fn semibold_family() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn body() -> FontId {
    FontId::proportional(BODY)
}
pub fn body_strong() -> FontId {
    FontId::new(BODY, semibold_family())
}
pub fn small() -> FontId {
    FontId::proportional(SMALL)
}
pub fn small_strong() -> FontId {
    FontId::new(SMALL, semibold_family())
}
pub fn heading() -> FontId {
    FontId::new(HEADING, semibold_family())
}
/// The title of a dialog-style window ("Download complete").
pub fn title() -> FontId {
    FontId::new(15.0, semibold_family())
}

/// Text in the strong (semibold) face at the body size.
pub fn strong(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).font(body_strong())
}

// --------------------------------------------------------------------------
// Fonts
// --------------------------------------------------------------------------

/// Segoe UI from `%WINDIR%\Fonts` (nothing is bundled, so the binary does not
/// grow), Segoe UI Semibold for strong text, Consolas for monospace — each with
/// egui's bundled fonts behind it as the fallback.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map(|w| std::path::PathBuf::from(w).join("Fonts"))
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Windows\Fonts"));

    let load = |file: &str| std::fs::read(dir.join(file)).ok();

    let regular = load("segoeui.ttf");
    let semibold = load("seguisb.ttf");
    let mono = load("consola.ttf");
    // ✓ ✗ ▲ ▼ and friends: Segoe UI itself has none of them.
    let symbols = load("seguisym.ttf");

    let defaults_prop = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let defaults_mono = fonts
        .families
        .get(&FontFamily::Monospace)
        .cloned()
        .unwrap_or_default();

    // ⏱ and the other pictographs Segoe UI Symbol lacks.
    let emoji = load("seguiemj.ttf");

    // The chain, front to back: Segoe UI → Segoe UI Symbol → Segoe UI Emoji →
    // egui's bundled fonts. Inserted back to front.
    if let Some(bytes) = emoji {
        fonts
            .font_data
            .insert("seguiemj".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "seguiemj".into());
    }
    if let Some(bytes) = symbols {
        fonts
            .font_data
            .insert("seguisym".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "seguisym".into());
    }
    if let Some(bytes) = regular {
        fonts
            .font_data
            .insert("segoeui".into(), Arc::new(FontData::from_owned(bytes)));
        fonts
            .families
            .entry(FontFamily::Proportional)
            .or_default()
            .insert(0, "segoeui".into());
    }
    let mut strong_chain = Vec::new();
    if let Some(bytes) = semibold {
        fonts
            .font_data
            .insert("seguisb".into(), Arc::new(FontData::from_owned(bytes)));
        strong_chain.push("seguisb".to_string());
    }
    // Without the semibold face, strong text falls back to the regular chain.
    strong_chain.extend(
        fonts
            .families
            .get(&FontFamily::Proportional)
            .cloned()
            .unwrap_or(defaults_prop),
    );
    fonts.families.insert(semibold_family(), strong_chain);

    if let Some(bytes) = mono {
        fonts
            .font_data
            .insert("consolas".into(), Arc::new(FontData::from_owned(bytes)));
        let mut chain = vec!["consolas".to_string()];
        chain.extend(defaults_mono);
        fonts.families.insert(FontFamily::Monospace, chain);
    }

    ctx.set_fonts(fonts);
}

// --------------------------------------------------------------------------
// egui Style from a palette
// --------------------------------------------------------------------------

fn widget(bg: Color32, weak_bg: Color32, stroke: Color32, fg: Color32) -> WidgetVisuals {
    WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: weak_bg,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: CornerRadius::same(CORNER_RADIUS),
        fg_stroke: Stroke::new(1.0, fg),
        expansion: 0.0,
    }
}

/// The whole egui style for one palette: colours, the 12 px type scale and the
/// compact spacing.
pub fn style_for(p: &Palette, dark: bool) -> Style {
    let mut style = Style {
        text_styles: [
            (TextStyle::Small, FontId::proportional(SMALL)),
            (TextStyle::Body, FontId::proportional(BODY)),
            (TextStyle::Button, FontId::proportional(BODY)),
            (TextStyle::Monospace, FontId::monospace(BODY)),
            (TextStyle::Heading, FontId::new(HEADING, semibold_family())),
        ]
        .into(),
        ..Style::default()
    };

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(6.0, 4.0);
    s.button_padding = Vec2::new(6.0, 2.0);
    s.interact_size = Vec2::new(40.0, INTERACT_HEIGHT);
    s.menu_margin = Margin::symmetric(4, 4);
    s.window_margin = Margin::same(8);
    s.indent = 14.0;
    s.icon_width = 13.0;
    s.icon_width_inner = 7.0;
    s.icon_spacing = 5.0;
    s.combo_width = 120.0;
    s.text_edit_width = 160.0;
    s.tooltip_width = 360.0;
    s.menu_width = 200.0;
    s.combo_height = 240.0;
    s.scroll = ScrollStyle {
        bar_width: 8.0,
        handle_min_length: 16.0,
        bar_inner_margin: 2.0,
        bar_outer_margin: 0.0,
        floating: false,
        ..ScrollStyle::solid()
    };

    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    v.dark_mode = dark;
    v.override_text_color = None;
    v.hyperlink_color = p.accent;
    v.faint_bg_color = p.row_alt;
    v.extreme_bg_color = p.surface;
    v.text_edit_bg_color = Some(p.surface);
    v.code_bg_color = p.surface_alt;
    v.warn_fg_color = p.warning;
    v.error_fg_color = p.danger;
    v.window_fill = p.surface;
    v.panel_fill = p.window;
    v.window_stroke = Stroke::new(1.0, p.border);
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = CornerRadius::same(CORNER_RADIUS);
    v.window_shadow = Shadow {
        offset: [0, 2],
        blur: 8,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 90 } else { 40 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 2],
        blur: 6,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 80 } else { 32 }),
    };
    v.selection = Selection {
        bg_fill: p.accent_pressed,
        stroke: Stroke::new(1.0, p.accent),
    };
    v.widgets = Widgets {
        noninteractive: widget(p.surface, p.surface, p.border, p.text),
        inactive: widget(p.surface, p.surface, p.border_strong, p.text),
        hovered: widget(p.accent_soft, p.accent_soft, p.accent_light, p.text),
        active: widget(p.accent_pressed, p.accent_pressed, p.accent, p.text),
        open: widget(p.accent_soft, p.accent_soft, p.accent_light, p.text),
    };
    v.striped = false;
    v.slider_trailing_fill = true;
    v.handle_shape = egui::style::HandleShape::Circle;
    v.button_frame = true;
    v.collapsing_header_frame = false;
    v.indent_has_left_vline = false;
    v.text_cursor.stroke = Stroke::new(1.0, p.text);
    v.weak_text_color = Some(p.text_muted);
    v.disabled_alpha = 0.55;

    style.visuals = v;
    style.interaction.tooltip_delay = 0.45;
    style.interaction.selectable_labels = false;
    style.animation_time = 0.08;
    style
}

#[cfg(test)]
mod tests {
    //! The light and dark palettes, and choosing between them — port of `ThemeTests.cs`.
    //!
    //! Nobody notices a palette that is subtly unreadable until they squint at it, so
    //! the contrast of the pairs that carry text is held to WCAG ratios here — a colour
    //! tweaked later cannot quietly make the dark theme's muted text illegible.

    use super::*;

    fn luminance(c: Color32) -> f64 {
        fn channel(v: u8) -> f64 {
            let s = v as f64 / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn the_preference_decides_unless_it_defers_to_windows() {
        assert!(!resolve(AppTheme::Light, Some(true)));
        assert!(resolve(AppTheme::Dark, Some(false)));
        assert!(resolve(AppTheme::System, Some(true)));
        assert!(!resolve(AppTheme::System, Some(false)));
    }

    #[test]
    fn a_windows_that_does_not_say_gets_the_light_palette_the_app_always_had() {
        assert!(!resolve(AppTheme::System, None));
    }

    #[test]
    fn body_text_is_comfortably_readable_on_every_background_it_sits_on() {
        for (name, p) in [("light", Palette::LIGHT), ("dark", Palette::DARK)] {
            assert!(contrast(p.text, p.surface) >= 7.0, "{name} text/surface");
            assert!(contrast(p.text, p.window) >= 7.0, "{name} text/window");
            assert!(contrast(p.text, p.header_back) >= 7.0, "{name} text/header");
            assert!(
                contrast(p.text_on_accent, p.row_selected) >= 4.5,
                "{name} selected row"
            );
        }
    }

    #[test]
    fn the_dark_palette_keeps_its_secondary_text_and_status_colours_readable() {
        let p = Palette::DARK;
        assert!(contrast(p.text_muted, p.surface) >= 4.5, "muted");
        assert!(contrast(p.accent, p.surface) >= 4.5, "accent text");
        assert!(contrast(p.success, p.surface) >= 4.5, "success");
        assert!(contrast(p.warning, p.surface) >= 4.5, "warning");
        assert!(contrast(p.danger, p.surface) >= 4.5, "danger");
        assert!(
            contrast(p.text_on_accent, p.accent) >= 3.0,
            "primary button"
        );
    }

    #[test]
    fn the_dark_palette_is_actually_dark() {
        assert!(luminance(Palette::DARK.surface) < 0.05);
        assert!(luminance(Palette::DARK.text) > 0.7);
    }

    #[test]
    fn the_type_scale_is_twelve_pixels_everywhere_that_matters() {
        let style = style_for(&Palette::LIGHT, false);
        for text_style in [TextStyle::Body, TextStyle::Button, TextStyle::Monospace] {
            assert_eq!(style.text_styles[&text_style].size, 12.0, "{text_style:?}");
        }
        assert_eq!(style.text_styles[&TextStyle::Small].size, 11.0);
        assert_eq!(style.text_styles[&TextStyle::Heading].size, 13.0);
        assert_eq!(style.spacing.button_padding, Vec2::new(6.0, 2.0));
        assert_eq!(style.spacing.interact_size.y, 20.0);
        assert_eq!(style.spacing.item_spacing, Vec2::new(6.0, 4.0));
        assert_eq!(
            style.visuals.widgets.inactive.corner_radius,
            CornerRadius::same(3)
        );
    }

    #[test]
    fn every_colour_follows_the_palette_in_use() {
        // Both styles derive from their own palette, so a switch is a full restyle.
        let light = style_for(&Palette::LIGHT, false);
        let dark = style_for(&Palette::DARK, true);
        assert_eq!(light.visuals.panel_fill, Palette::LIGHT.window);
        assert_eq!(dark.visuals.panel_fill, Palette::DARK.window);
        assert!(dark.visuals.dark_mode && !light.visuals.dark_mode);
    }
}
