//! The editor's visual theme: a real dark palette in place of egui's flat
//! defaults, with one deliberate accent color used consistently for
//! selection, the active tool, and focus states — everything else in the
//! app (panel colors, clip fills, etc.) should read *against* this theme,
//! not invent its own.
//!
//! Colors are picked, not defaulted: a warm amber accent rather than the
//! generic blue/purple most tools reach for, because it reads as
//! "scrubber/playhead" energy — a real association for a video editor —
//! and stays legible on the charcoal ground in both a bright edit suite and
//! a dim one.

use egui::{Color32, Context, Rounding, Stroke, Visuals};

/// Deep charcoal, not pure black — pure black against bright preview footage
/// is harsher on the eyes over a long edit session.
pub const BACKGROUND: Color32 = Color32::from_rgb(0x15, 0x17, 0x1B);
/// Panel/widget surface, one step up from the background so panels read as
/// distinct layers without a hard border everywhere.
pub const SURFACE: Color32 = Color32::from_rgb(0x1D, 0x20, 0x24);
/// A second surface step, for panels nested inside panels (e.g. a group box
/// inside the effects panel).
pub const SURFACE_RAISED: Color32 = Color32::from_rgb(0x25, 0x29, 0x2E);
/// One step lighter than `SURFACE_RAISED`, for a hovered widget's fill.
pub const SURFACE_HOVER: Color32 = Color32::from_rgb(0x2E, 0x33, 0x3A);
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xE9, 0xEA);
pub const TEXT_MUTED: Color32 = Color32::from_rgb(0x90, 0x96, 0xA0);
/// The one accent color in the app. Used for: the active tool, a selected
/// clip's outline, focus rings, and anywhere else that means "this is the
/// thing that's currently active" — never for decoration.
pub const ACCENT: Color32 = Color32::from_rgb(0xF2, 0xA9, 0x3B);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x8A, 0x63, 0x28);

const ROUNDING: f32 = 5.0;

/// Applies the theme to `ctx`. Call once at startup, before the first frame.
pub fn apply(ctx: &Context) {
    let mut visuals = Visuals::dark();
    visuals.override_text_color = Some(TEXT);
    visuals.panel_fill = SURFACE;
    visuals.window_fill = SURFACE;
    visuals.faint_bg_color = BACKGROUND;
    visuals.extreme_bg_color = BACKGROUND;
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.selection.stroke = Stroke::new(1.0f32, ACCENT);
    visuals.hyperlink_color = ACCENT;

    visuals.widgets.noninteractive.bg_fill = SURFACE;
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0f32, TEXT_MUTED);
    visuals.widgets.noninteractive.rounding = Rounding::same(ROUNDING);

    visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0f32, TEXT);
    visuals.widgets.inactive.rounding = Rounding::same(ROUNDING);

    visuals.widgets.hovered.bg_fill = SURFACE_HOVER;
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.2f32, TEXT);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0f32, ACCENT_DIM);
    visuals.widgets.hovered.rounding = Rounding::same(ROUNDING);

    visuals.widgets.active.bg_fill = ACCENT_DIM;
    visuals.widgets.active.fg_stroke = Stroke::new(1.2f32, TEXT);
    visuals.widgets.active.bg_stroke = Stroke::new(1.2f32, ACCENT);
    visuals.widgets.active.rounding = Rounding::same(ROUNDING);

    // The "currently selected/toggled on" state — a selectable tool button,
    // a checked checkbox — is exactly where the accent should show up most
    // clearly, since it answers "what's active right now."
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.widgets.open.bg_fill = ACCENT_DIM;
    visuals.widgets.open.fg_stroke = Stroke::new(1.2f32, TEXT);
    visuals.widgets.open.bg_stroke = Stroke::new(1.2f32, ACCENT);
    visuals.widgets.open.rounding = Rounding::same(ROUNDING);

    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = egui::vec2(6.0, 6.0);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
    ctx.set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_is_distinct_from_background_and_surface() {
        // The whole point of an accent color is that it stands out — a
        // theme bug that quietly set ACCENT equal to (or very close to) the
        // background would make every "this is active" cue invisible.
        let dist = |a: Color32, b: Color32| {
            let dr = a.r() as i32 - b.r() as i32;
            let dg = a.g() as i32 - b.g() as i32;
            let db = a.b() as i32 - b.b() as i32;
            ((dr * dr + dg * dg + db * db) as f64).sqrt()
        };
        assert!(dist(ACCENT, BACKGROUND) > 80.0);
        assert!(dist(ACCENT, SURFACE) > 80.0);
    }

    #[test]
    fn text_is_readable_against_the_background() {
        // Cheap luminance check rather than a full contrast-ratio
        // calculation — this only needs to catch a gross regression (e.g.
        // TEXT accidentally set to a dark color), not certify WCAG
        // compliance.
        let luminance = |c: Color32| 0.299 * c.r() as f64 + 0.587 * c.g() as f64 + 0.114 * c.b() as f64;
        assert!(luminance(TEXT) - luminance(BACKGROUND) > 100.0);
    }

    #[test]
    fn apply_does_not_panic_without_a_running_app() {
        // `Context::default()` is enough to exercise `set_visuals`/
        // `set_style` outside a real render loop.
        let ctx = Context::default();
        apply(&ctx);
        assert_eq!(ctx.style().visuals.panel_fill, SURFACE);
    }
}
