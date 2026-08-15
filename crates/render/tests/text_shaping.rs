//! Real text *shaping* — glyph substitution and repositioning driven by the
//! font's own GSUB/GPOS rules — as opposed to the character-to-glyph lookup
//! `render::text` had before this. A straight lookup can only ever emit
//! exactly one glyph per input character; it cannot form a ligature, cannot
//! merge a consonant cluster into one conjunct glyph, and cannot reorder a
//! vowel sign that is encoded after its consonant but drawn before it. All
//! three are ordinary, everyday rendering for the scripts that need them.
//!
//! Proving shaping is real needs a font with real Indic OpenType tables, and
//! a Latin font can't demonstrate it — Arial has no ligature or reordering
//! rules that fire on ordinary text. Windows ships `Nirmala.ttc`, the default
//! UI font for Devanagari (and several other Indic scripts) since Windows 8,
//! so this machine can run the real case. Skipped, not failed, where it's
//! missing — the same real-environment-dependent shape as
//! `real_footage_smoke.rs`.

use render::text::{rasterize, shaped_glyph_count, FontFace};
use timeline::TitleSpec;

fn nirmala() -> Option<FontFace> {
    let bytes = std::fs::read(r"C:\Windows\Fonts\Nirmala.ttc").ok()?;
    FontFace::from_bytes(bytes)
}

/// KA + VIRAMA + SSA + VOWEL SIGN I. Four codepoints. A real shaping engine
/// forms KA+VIRAMA+SSA into a single conjunct glyph via the font's GSUB
/// rules — this is ordinary Devanagari, not a stress case — so it comes out
/// as *two* glyphs, never four. A naive one-glyph-per-character mapping can
/// only ever produce four.
const CONJUNCT: &str = "\u{0915}\u{094D}\u{0937}\u{093F}";

#[test]
fn a_devanagari_consonant_cluster_shapes_to_fewer_glyphs_than_codepoints() {
    let Some(font) = nirmala() else {
        eprintln!("skipping: Nirmala.ttc not present on this machine");
        return;
    };
    let count = shaped_glyph_count(&font, CONJUNCT, 48.0).expect("Nirmala should shape Devanagari");
    assert!(
        count < CONJUNCT.chars().count(),
        "expected the conjunct to merge into fewer glyphs than the {} input \
         codepoints, got {count} — looks like one glyph was emitted per \
         character instead of real GSUB substitution",
        CONJUNCT.chars().count()
    );
}

#[test]
fn plain_latin_text_still_shapes_to_one_glyph_per_character() {
    // The negative case for the same property: shaping must not invent
    // substitutions where the font has none. Arial has no GSUB rules that
    // fire on plain ASCII, so this should come back exactly as naive lookup
    // would — proving the assertion above is about real substitution, not
    // some off-by-one in how glyphs get counted.
    let bytes = std::fs::read(r"C:\Windows\Fonts\arial.ttf").expect("arial.ttf should exist");
    let font = FontFace::from_bytes(bytes).expect("arial.ttf should parse");
    let count = shaped_glyph_count(&font, "Hello", 48.0).expect("arial should shape ASCII");
    assert_eq!(count, 5, "five plain ASCII letters should still be five glyphs");
}

#[test]
fn a_devanagari_title_still_rasterises_to_a_frame_sized_straight_alpha_buffer() {
    // Shaping must not break the contract every other title honours.
    let Some(font) = nirmala() else {
        eprintln!("skipping: Nirmala.ttc not present on this machine");
        return;
    };
    let spec = TitleSpec { text: CONJUNCT.into(), size_px: 48.0, ..Default::default() };
    let out = rasterize(&spec, &font, 320, 180);
    assert_eq!(out.rgba.len(), 320 * 180 * 4, "buffer stays exactly frame-sized");
    assert!(out.inked_pixels(128) > 0, "the shaped conjunct should still draw ink");
}
