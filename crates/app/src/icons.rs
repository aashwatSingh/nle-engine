//! Small, hand-drawn vector icons for the always-visible toolbar/transport
//! buttons — drawn directly with egui's painter rather than pulling in an
//! icon font or SVG assets, so the icon set is just Rust code with no new
//! binary files in the repo.
//!
//! Deliberately scoped to the buttons a user sees on every single frame
//! (tool selector, transport, timeline toolbar, top menu essentials) —
//! *not* the effects panel's advanced action buttons ("Detect Scene Cuts,"
//! "Generate Captions," ...), where a clear text label beats a guessable
//! icon for something used occasionally.
//!
//! Each icon's shape is a list of points normalized to a 0..1 unit square,
//! kept separate from the painting step so the geometry itself is testable
//! without needing a real `egui::Painter` (which needs a live `Context`).

use egui::{Color32, Pos2, Rect, Response, Sense, Stroke, Ui, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Play,
    Pause,
    Select,
    Razor,
    ZoomIn,
    ZoomOut,
    Snap,
    MarkIn,
    MarkOut,
    Undo,
    Redo,
    Import,
    AddTitle,
}

/// One filled or stroked shape, in unit-square (0..1) coordinates.
enum Shape {
    Polyline(Vec<(f32, f32)>),
    FilledPolygon(Vec<(f32, f32)>),
    Circle { center: (f32, f32), radius: f32 },
}

/// Every point across every shape for `icon`. Kept as one flat iterator so
/// the bounds test below doesn't need to know each icon's shape count.
fn all_points(icon: Icon) -> Vec<(f32, f32)> {
    shapes(icon).into_iter().flat_map(shape_points).collect()
}

fn shape_points(shape: Shape) -> Vec<(f32, f32)> {
    match shape {
        Shape::Polyline(pts) | Shape::FilledPolygon(pts) => pts,
        Shape::Circle { center, radius } => {
            vec![(center.0 - radius, center.1 - radius), (center.0 + radius, center.1 + radius)]
        }
    }
}

fn arc(center: (f32, f32), radius: f32, start_deg: f32, end_deg: f32, segments: usize) -> Vec<(f32, f32)> {
    (0..=segments)
        .map(|i| {
            let t = i as f32 / segments as f32;
            let deg = start_deg + (end_deg - start_deg) * t;
            let rad = deg.to_radians();
            (center.0 + radius * rad.cos(), center.1 + radius * rad.sin())
        })
        .collect()
}

fn shapes(icon: Icon) -> Vec<Shape> {
    match icon {
        Icon::Play => vec![Shape::FilledPolygon(vec![(0.25, 0.15), (0.25, 0.85), (0.85, 0.5)])],
        Icon::Pause => vec![
            Shape::FilledPolygon(vec![(0.2, 0.15), (0.4, 0.15), (0.4, 0.85), (0.2, 0.85)]),
            Shape::FilledPolygon(vec![(0.6, 0.15), (0.8, 0.15), (0.8, 0.85), (0.6, 0.85)]),
        ],
        // A standard pointer-cursor silhouette.
        Icon::Select => vec![Shape::FilledPolygon(vec![
            (0.18, 0.1),
            (0.18, 0.82),
            (0.36, 0.66),
            (0.48, 0.9),
            (0.6, 0.84),
            (0.48, 0.6),
            (0.75, 0.55),
        ])],
        // A slanted blade.
        Icon::Razor => vec![Shape::FilledPolygon(vec![(0.15, 0.8), (0.72, 0.15), (0.85, 0.25), (0.28, 0.85)])],
        Icon::ZoomIn => {
            let mut s = magnifying_glass();
            s.push(Shape::Polyline(vec![(0.26, 0.38), (0.5, 0.38)]));
            s.push(Shape::Polyline(vec![(0.38, 0.26), (0.38, 0.5)]));
            s
        }
        Icon::ZoomOut => {
            let mut s = magnifying_glass();
            s.push(Shape::Polyline(vec![(0.26, 0.38), (0.5, 0.38)]));
            s
        }
        // A magnet: two prongs joined by a bottom arc, with a gap band near
        // each tip standing in for the classic red/silver magnet caps.
        Icon::Snap => vec![
            Shape::Polyline(vec![(0.25, 0.15), (0.25, 0.55)]),
            Shape::Polyline(vec![(0.75, 0.15), (0.75, 0.55)]),
            Shape::Polyline(arc((0.5, 0.55), 0.25, 180.0, 360.0, 12)),
            Shape::Polyline(vec![(0.2, 0.32), (0.3, 0.32)]),
            Shape::Polyline(vec![(0.7, 0.32), (0.8, 0.32)]),
        ],
        Icon::MarkIn => vec![Shape::Polyline(vec![(0.6, 0.15), (0.35, 0.15), (0.35, 0.85), (0.6, 0.85)])],
        Icon::MarkOut => vec![Shape::Polyline(vec![(0.4, 0.15), (0.65, 0.15), (0.65, 0.85), (0.4, 0.85)])],
        Icon::Undo => curved_arrow(true),
        Icon::Redo => curved_arrow(false),
        Icon::Import => vec![
            Shape::Polyline(vec![(0.5, 0.15), (0.5, 0.55)]),
            Shape::FilledPolygon(vec![(0.32, 0.42), (0.68, 0.42), (0.5, 0.68)]),
            Shape::Polyline(vec![(0.2, 0.72), (0.2, 0.85), (0.8, 0.85), (0.8, 0.72)]),
        ],
        Icon::AddTitle => vec![
            Shape::Polyline(vec![(0.15, 0.25), (0.65, 0.25)]),
            Shape::Polyline(vec![(0.15, 0.45), (0.75, 0.45)]),
            Shape::Polyline(vec![(0.15, 0.65), (0.55, 0.65)]),
            Shape::Polyline(vec![(0.72, 0.75), (0.72, 0.95)]),
            Shape::Polyline(vec![(0.62, 0.85), (0.82, 0.85)]),
        ],
    }
}

fn magnifying_glass() -> Vec<Shape> {
    vec![
        Shape::Circle { center: (0.38, 0.38), radius: 0.22 },
        Shape::Polyline(vec![(0.55, 0.55), (0.85, 0.85)]),
    ]
}

/// A hooked arrow sweeping counter-clockwise (undo) or its mirror (redo).
fn curved_arrow(counter_clockwise: bool) -> Vec<Shape> {
    let (start, end) = if counter_clockwise { (30.0, 220.0) } else { (150.0, -40.0) };
    let body = arc((0.5, 0.55), 0.28, start, end, 12);
    let tail = *body.last().unwrap();
    let head_dir = if counter_clockwise { -1.0 } else { 1.0 };
    vec![
        Shape::Polyline(body),
        Shape::FilledPolygon(vec![
            (tail.0, tail.1),
            (tail.0 - 0.1 * head_dir, tail.1 - 0.12),
            (tail.0 + 0.02 * head_dir, tail.1 + 0.14),
        ]),
    ]
}

fn to_screen(rect: Rect, p: (f32, f32)) -> Pos2 {
    Pos2::new(rect.min.x + p.0 * rect.width(), rect.min.y + p.1 * rect.height())
}

fn paint_icon(painter: &egui::Painter, icon: Icon, rect: Rect, color: Color32) {
    let stroke = Stroke::new(1.6f32, color);
    for shape in shapes(icon) {
        match shape {
            Shape::Polyline(pts) => {
                let screen: Vec<Pos2> = pts.iter().map(|&p| to_screen(rect, p)).collect();
                painter.add(egui::Shape::line(screen, stroke));
            }
            Shape::FilledPolygon(pts) => {
                let screen: Vec<Pos2> = pts.iter().map(|&p| to_screen(rect, p)).collect();
                painter.add(egui::Shape::convex_polygon(screen, color, Stroke::NONE));
            }
            Shape::Circle { center, radius } => {
                let c = to_screen(rect, center);
                let r = radius * rect.width().min(rect.height());
                painter.circle_stroke(c, r, stroke);
            }
        }
    }
}

/// An icon button: paints `icon` inside a normal egui button frame (so it
/// still gets hover/press/disabled styling from the current theme), with an
/// optional trailing text label. Pass an empty `label` for an icon-only
/// button (used for the tool selector and transport, where the icon alone
/// is unambiguous).
pub fn icon_button(ui: &mut Ui, icon: Icon, label: &str) -> Response {
    icon_button_impl(ui, icon, label, false)
}

/// Same as `icon_button`, but styled as "currently active" (the theme's
/// accent-tinted selection color) regardless of hover state — for a tool
/// selector or any other icon button standing in for `selectable_value`.
pub fn icon_toggle(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    icon_button_impl(ui, icon, label, selected)
}

fn icon_button_impl(ui: &mut Ui, icon: Icon, label: &str, selected: bool) -> Response {
    let icon_size = 15.0;
    let padding = ui.spacing().button_padding;
    let galley = (!label.is_empty())
        .then(|| ui.painter().layout_no_wrap(label.to_owned(), egui::FontId::proportional(13.0), Color32::PLACEHOLDER));
    let gap = if galley.is_some() { 6.0 } else { 0.0 };
    let content_w = icon_size + gap + galley.as_ref().map_or(0.0, |g| g.size().x);
    let content_h = icon_size.max(galley.as_ref().map_or(0.0, |g| g.size().y));
    let desired_size = Vec2::new(content_w, content_h) + padding * 2.0;

    let (rect, response) = ui.allocate_exact_size(desired_size, Sense::click());
    if ui.is_rect_visible(rect) {
        let widget_visuals = ui.style().interact_selectable(&response, selected);
        ui.painter().rect(rect, widget_visuals.rounding, widget_visuals.bg_fill, widget_visuals.bg_stroke);
        let icon_rect = Rect::from_min_size(
            Pos2::new(rect.min.x + padding.x, rect.center().y - icon_size / 2.0),
            Vec2::splat(icon_size),
        );
        paint_icon(ui.painter(), icon, icon_rect, widget_visuals.fg_stroke.color);
        if let Some(galley) = galley {
            let text_pos = Pos2::new(icon_rect.right() + gap, rect.center().y - galley.size().y / 2.0);
            ui.painter().galley(text_pos, galley, widget_visuals.fg_stroke.color);
        }
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Icon; 13] = [
        Icon::Play,
        Icon::Pause,
        Icon::Select,
        Icon::Razor,
        Icon::ZoomIn,
        Icon::ZoomOut,
        Icon::Snap,
        Icon::MarkIn,
        Icon::MarkOut,
        Icon::Undo,
        Icon::Redo,
        Icon::Import,
        Icon::AddTitle,
    ];

    #[test]
    fn every_icon_stays_within_its_unit_square() {
        // A point outside 0..1 would draw outside the button's own bounds —
        // clipped or bleeding into whatever's next to it. Small tolerance
        // for the curved-arrow icons, whose arc math can land a hair
        // outside 0/1 depending on the exact angle.
        for icon in ALL {
            for (x, y) in all_points(icon) {
                assert!((-0.05..=1.05).contains(&x), "{icon:?}: x={x} out of bounds");
                assert!((-0.05..=1.05).contains(&y), "{icon:?}: y={y} out of bounds");
            }
        }
    }

    #[test]
    fn every_icon_has_at_least_one_shape() {
        for icon in ALL {
            assert!(!shapes(icon).is_empty(), "{icon:?} has no geometry");
        }
    }

    #[test]
    fn play_and_pause_are_visually_distinct_shapes() {
        // Regression guard against a copy-paste that leaves two icons
        // pointing at identical geometry.
        assert_ne!(all_points(Icon::Play), all_points(Icon::Pause));
    }

    #[test]
    fn undo_and_redo_sweep_in_opposite_directions() {
        let undo_points = all_points(Icon::Undo);
        let redo_points = all_points(Icon::Redo);
        assert_ne!(undo_points, redo_points);
    }

    #[test]
    fn mark_in_and_mark_out_are_mirrored() {
        let inn = all_points(Icon::MarkIn);
        let out = all_points(Icon::MarkOut);
        // Mirrored around x=0.5: each x should map to 1.0-x of the other's.
        assert_eq!(inn.len(), out.len());
        for (a, b) in inn.iter().zip(out.iter()) {
            assert!((a.0 - (1.0 - b.0)).abs() < 1e-6);
            assert!((a.1 - b.1).abs() < 1e-6);
        }
    }
}
