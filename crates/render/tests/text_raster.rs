//! Title rasterisation, measured rather than eyeballed.
//!
//! Every assertion here is a pixel statistic — ink coverage, ink centroid,
//! channel values — chosen because they survive a font version bump. Asserting
//! exact glyph shapes would pin this to one build of one font file and break on
//! any machine with a different Arial.

use render::text::{rasterize, FontFace, RasterizedText};
use timeline::{TextAlign, TitleSpec};

const W: u32 = 320;
const H: u32 = 180;
/// Anything at least this opaque counts as ink. Well above anti-aliasing
/// noise, well below a solid glyph interior.
const INK: u8 = 128;

fn font() -> FontFace {
    let path = r"C:\Windows\Fonts\arial.ttf";
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("these tests need a real font at {path}: {e}"));
    FontFace::from_bytes(bytes).expect("arial.ttf should parse as a font")
}

fn spec(text: &str) -> TitleSpec {
    TitleSpec { text: text.into(), size_px: 48.0, ..Default::default() }
}

fn draw(spec: &TitleSpec) -> RasterizedText {
    rasterize(spec, &font(), W, H)
}

#[test]
fn text_puts_ink_on_the_frame_and_leaves_the_rest_transparent() {
    let out = draw(&spec("Hello"));

    let inked = out.inked_pixels(INK);
    assert!(inked > 0, "nothing was drawn at all");
    // Five 48px glyphs cover a small fraction of a 320x180 frame. The upper
    // bound is the real assertion: it catches a rasteriser that floods the
    // frame instead of drawing glyphs.
    let total = W * H;
    assert!(
        inked < total / 4,
        "text should cover a small part of the frame, got {inked}/{total} pixels"
    );

    // The corners are far outside a centred line of text.
    for (x, y) in [(0, 0), (W - 1, 0), (0, H - 1), (W - 1, H - 1)] {
        assert_eq!(out.pixel(x, y)[3], 0, "corner ({x},{y}) should be transparent");
    }
}

#[test]
fn empty_text_draws_nothing() {
    let out = draw(&spec(""));
    assert_eq!(out.inked_pixels(1), 0, "empty text must not put any ink down");
}

#[test]
fn glyph_pixels_carry_the_requested_colour_at_full_alpha_in_their_interior() {
    // A big glyph so there's a solid interior to sample, not just antialiased
    // edges. RGB must be the requested colour *everywhere*, including where
    // alpha is 0 — that's what straight (non-premultiplied) alpha means, and
    // getting it wrong shows up as dark fringing once the compositor
    // linearises and blends.
    let mut s = spec("H");
    s.size_px = 120.0;
    s.color = [1.0, 0.0, 0.0, 1.0];
    let out = draw(&s);

    let solid = (0..H)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .find(|&(x, y)| out.pixel(x, y)[3] == 255)
        .expect("a 120px glyph should have at least one fully opaque pixel");
    assert_eq!(
        out.pixel(solid.0, solid.1),
        [255, 0, 0, 255],
        "a solid glyph pixel should be exactly the requested colour"
    );

    let corner = out.pixel(0, 0);
    assert_eq!(corner[3], 0, "the corner is outside the glyph");
    assert_eq!(
        [corner[0], corner[1], corner[2]],
        [255, 0, 0],
        "straight alpha: RGB stays the title colour even where coverage is zero"
    );
}

#[test]
fn a_larger_size_puts_down_more_ink() {
    let mut small = spec("Wide");
    small.size_px = 24.0;
    let mut large = spec("Wide");
    large.size_px = 72.0;

    let small_ink = draw(&small).inked_pixels(INK);
    let large_ink = draw(&large).inked_pixels(INK);
    // Glyph area scales with the square of the size, so 3x the size is far
    // more than 3x the ink — asserting only ">" would pass on a rasteriser
    // that ignored size and drew one extra pixel.
    assert!(
        large_ink > small_ink * 4,
        "3x the point size should be roughly 9x the ink, got {small_ink} -> {large_ink}"
    );
}

#[test]
fn alignment_moves_the_text_relative_to_its_anchor() {
    // Same anchor, three alignments. Left-aligned text starts at the anchor
    // and extends right; right-aligned ends at it; centred straddles it.
    let anchor_x = 0.5;
    let mut s = spec("ALIGNMENT");
    s.position = (anchor_x, 0.5);

    let centroid = |align| {
        let mut s = s.clone();
        s.align = align;
        draw(&s).ink_centroid_x(INK).expect("text was drawn")
    };

    let left = centroid(TextAlign::Left);
    let center = centroid(TextAlign::Center);
    let right = centroid(TextAlign::Right);

    let anchor_px = anchor_x * W as f64;
    assert!(left > anchor_px, "left-aligned text sits right of its anchor, got {left}");
    assert!(right < anchor_px, "right-aligned text sits left of its anchor, got {right}");
    assert!(
        (center - anchor_px).abs() < 12.0,
        "centred text should straddle its anchor at {anchor_px}, got {center}"
    );
    // And the three are ordered, which a per-alignment sign error would break
    // even if each individual bound happened to hold.
    assert!(right < center && center < left, "got right={right} center={center} left={left}");
}

#[test]
fn the_position_anchor_moves_the_text() {
    let mut high = spec("Y");
    high.position = (0.5, 0.2);
    let mut low = spec("Y");
    low.position = (0.5, 0.8);

    let high_y = draw(&high).ink_centroid_y(INK).expect("drawn");
    let low_y = draw(&low).ink_centroid_y(INK).expect("drawn");

    assert!(high_y < low_y, "y=0.2 should sit above y=0.8, got {high_y} and {low_y}");
    // Near the requested fractions, not merely in the right order.
    assert!((high_y - 0.2 * H as f64).abs() < 15.0, "y=0.2 should land near 36px, got {high_y}");
    assert!((low_y - 0.8 * H as f64).abs() < 15.0, "y=0.8 should land near 144px, got {low_y}");
}

#[test]
fn multiple_lines_stack_vertically_around_the_anchor() {
    // A title box with a line break is ordinary. The block as a whole should
    // stay centred on the anchor, so adding a second line doesn't shove the
    // first one off its mark.
    let one = draw(&spec("One"));
    let two = draw(&spec("One\nTwo"));

    let (_, one_top, _, one_bottom) = one.ink_bounds(INK).expect("drawn");
    let (_, two_top, _, two_bottom) = two.ink_bounds(INK).expect("drawn");
    let one_h = one_bottom - one_top;
    let two_h = two_bottom - two_top;

    // The real assertion, and the one that catches a rasteriser that ignores
    // `\n` outright: two lines must be visibly *taller*, not merely wider.
    // Dropping the newline renders "OneTwo" on one line, which lays down more
    // ink at the same centroid and would sail past a coverage check.
    assert!(
        two_h > one_h * 3 / 2,
        "two lines should be much taller than one: {one_h}px -> {two_h}px"
    );

    // And the block stays centred on its anchor, so adding a line grows it
    // symmetrically instead of shoving the first line upward.
    let one_y = one.ink_centroid_y(INK).expect("drawn");
    let two_y = two.ink_centroid_y(INK).expect("drawn");
    assert!(
        (one_y - two_y).abs() < 12.0,
        "the block stays centred on its anchor: one line at {one_y}, two at {two_y}"
    );
}

#[test]
fn text_wider_than_the_frame_is_clipped_not_wrapped_or_panicking() {
    // Nothing stops a user typing a long title at a huge size. Glyphs falling
    // outside the frame must be dropped silently — an out-of-bounds write here
    // would be a buffer overrun, and a panic would take the whole render down.
    let mut s = spec("THIS TITLE IS FAR TOO LONG TO FIT IN THE FRAME AT ALL");
    s.size_px = 96.0;
    let out = draw(&s);
    assert!(out.inked_pixels(INK) > 0, "the middle of the line should still be visible");
    assert_eq!(out.rgba.len(), (W * H * 4) as usize, "the buffer must stay exactly frame-sized");
}
