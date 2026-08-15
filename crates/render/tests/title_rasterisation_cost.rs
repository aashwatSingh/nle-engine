//! What a title actually costs to rasterise, per frame, at delivery
//! resolution.
//!
//! This is what justified `compositor::TitleCache`, and it is measured rather
//! than assumed — this project has already once built an optimisation on a
//! number that turned out to be measurement noise (see
//! `docs/decisions-log.md`, GPU-resident decode).
//!
//! **What it found, and it was not the obvious answer.** An *empty* title —
//! no glyphs at all — costs almost as much as a real one: the work is
//! dominated by filling a frame-sized (1920x1080x4 = 8.3MB) RGBA buffer, not
//! by drawing letters. Two release runs on the same machine measured
//! 3.07/3.48/5.69 ms and 6.73/7.73/11.76 ms for empty/lower-third/full-screen
//! — the absolute numbers move a lot between runs, but the *shape* is stable
//! and is the part worth acting on: caching the whole raster saves the
//! dominant cost, whereas optimising glyph rasterisation would have chased the
//! smaller half.
//!
//! The bounds below are deliberately loose and release-only. They exist to
//! catch an order-of-magnitude regression — a rasteriser that starts scaling
//! with glyph count squared, or one that stops short-circuiting the empty
//! case — not to pin a number that visibly varies run to run.

use render::text::{rasterize, FontFace};
use std::time::Instant;
use timeline::TitleSpec;

const W: u32 = 1920;
const H: u32 = 1080;
/// One frame's budget at 30fps. A title costing a whole frame period would be
/// unusable; the assertions below are fractions of this.
const FRAME_BUDGET_MS: f64 = 33.3;

/// Asserts a timing bound, but **only in a release build**.
///
/// The same rasterisation measures ~3.5ms optimised and ~23ms unoptimised —
/// nearly 7x — so a bound tight enough to mean anything in release fails
/// every `cargo test`, and one loose enough for debug asserts nothing. Rather
/// than pick a number that is wrong in both profiles, the measurement always
/// prints and the bound applies where it's meaningful. Run with:
///
/// ```text
/// cargo test --release -p render --test title_rasterisation_cost -- --nocapture
/// ```
fn assert_within_budget(ms: f64, limit_ms: f64, what: &str) {
    if cfg!(debug_assertions) {
        println!("  (debug build — timing bound not enforced; run with --release)");
        return;
    }
    assert!(ms < limit_ms, "{what}: expected under {limit_ms:.1} ms, got {ms:.2} ms");
}

fn font() -> FontFace {
    let path = r"C:\Windows\Fonts\arial.ttf";
    let bytes = std::fs::read(path)
        .unwrap_or_else(|e| panic!("this measurement needs a real font at {path}: {e}"));
    FontFace::from_bytes(bytes).expect("arial.ttf should parse")
}

/// Median of `runs` rasterisations, in milliseconds. Median rather than mean
/// so one scheduling hiccup doesn't set the number — the lesson from the
/// starved-frame measurement that didn't reproduce.
fn median_ms(spec: &TitleSpec, runs: usize) -> f64 {
    let font = font();
    let mut samples: Vec<f64> = (0..runs)
        .map(|_| {
            let t = Instant::now();
            let out = rasterize(spec, &font, W, H);
            // Touch the result so the optimiser can't discard the work.
            std::hint::black_box(out.rgba.len());
            t.elapsed().as_secs_f64() * 1000.0
        })
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

#[test]
fn a_typical_lower_third_rasterises_well_inside_one_frame() {
    let spec = TitleSpec {
        text: "Dr. Someone Somebody\nHead of Something".into(),
        size_px: 64.0,
        ..Default::default()
    };
    let ms = median_ms(&spec, 21);
    println!("typical two-line title at {W}x{H}: {ms:.2} ms/frame (budget {FRAME_BUDGET_MS} ms)");
    assert_within_budget(ms, FRAME_BUDGET_MS / 4.0, "a two-line title");
}

#[test]
fn a_full_screen_of_large_text_still_fits_in_a_frame() {
    // The pathological-but-real case: a full title card. Glyph rasterisation
    // scales with inked area, so this is far more work than a lower third.
    let spec = TitleSpec {
        text: (0..8).map(|_| "ABCDEFGHIJKLMNOP").collect::<Vec<_>>().join("\n"),
        size_px: 96.0,
        ..Default::default()
    };
    let ms = median_ms(&spec, 11);
    println!("full-screen title card at {W}x{H}: {ms:.2} ms/frame");
    assert_within_budget(ms, FRAME_BUDGET_MS, "a full screen of text");
}

#[test]
fn an_empty_title_costs_about_what_clearing_the_buffer_costs() {
    // With no glyphs the only work left is filling a frame-sized buffer with
    // the title colour. Pinning that separately says how much of the numbers
    // above is glyph work and how much is the unavoidable allocation — which
    // is exactly what a cache would and wouldn't save.
    let spec = TitleSpec { text: String::new(), ..Default::default() };
    let ms = median_ms(&spec, 21);
    println!("empty title (buffer fill only) at {W}x{H}: {ms:.2} ms/frame");
    assert_within_budget(ms, FRAME_BUDGET_MS / 4.0, "filling the buffer alone");
}
