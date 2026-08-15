//! Silence detection: find gaps in a clip's audio worth ripple-deleting —
//! "auto-cut the pauses out of a talking-head recording", CapCut's most-used
//! single feature.
//!
//! Every assertion here works in **seconds**, not samples or windows — the
//! caller (an editor action) thinks in timeline time, and window-count
//! off-by-ones are exactly the kind of bug that survives testing in window
//! units and only shows up once someone asks "why is the cut 40ms early".

use audio::silence::{detect_silence, ranges_to_remove, SilenceConfig};

const SAMPLE_RATE: u32 = 48_000;

/// `seconds` of mono audio at a constant amplitude (0 = true digital
/// silence). `amplitude` is linear (0..1), not dB.
fn tone(seconds: f64, amplitude: f32) -> Vec<f32> {
    vec![amplitude; (seconds * SAMPLE_RATE as f64) as usize]
}

fn concat(parts: Vec<Vec<f32>>) -> Vec<f32> {
    parts.into_iter().flatten().collect()
}

fn default_config() -> SilenceConfig {
    SilenceConfig { threshold_dbfs: -40.0, min_silence_seconds: 0.3, padding_seconds: 0.1 }
}

#[test]
fn a_constantly_loud_clip_has_no_silence() {
    let samples = tone(5.0, 0.5);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert!(ranges.is_empty(), "loud audio throughout should have no silent ranges, got {ranges:?}");
}

#[test]
fn a_completely_silent_clip_is_one_range_covering_it() {
    let samples = tone(3.0, 0.0);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(ranges.len(), 1);
    assert!((ranges[0].0 - 0.0).abs() < 0.02, "should start at the very beginning, got {ranges:?}");
    assert!((ranges[0].1 - 3.0).abs() < 0.02, "should end at the very end, got {ranges:?}");
}

#[test]
fn a_pause_in_the_middle_is_found_and_bounded_correctly() {
    // 1s loud, 1s silent, 1s loud — the silent range should land at [1, 2).
    let samples = concat(vec![tone(1.0, 0.5), tone(1.0, 0.0), tone(1.0, 0.5)]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(ranges.len(), 1, "expected exactly one silent range, got {ranges:?}");
    assert!((ranges[0].0 - 1.0).abs() < 0.05, "should start around 1.0s, got {:?}", ranges[0]);
    assert!((ranges[0].1 - 2.0).abs() < 0.05, "should end around 2.0s, got {:?}", ranges[0]);
}

#[test]
fn a_brief_dip_shorter_than_the_minimum_is_not_flagged() {
    // A 0.1s gap is a natural pause between words, not dead air worth
    // cutting — `min_silence_seconds` (0.3s in the default config) exists
    // specifically so this doesn't turn into a machine-gun of tiny cuts.
    let samples = concat(vec![tone(1.0, 0.5), tone(0.1, 0.0), tone(1.0, 0.5)]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert!(ranges.is_empty(), "a sub-minimum dip must not be flagged, got {ranges:?}");
}

#[test]
fn a_dip_at_or_above_the_minimum_is_flagged() {
    let samples = concat(vec![tone(1.0, 0.5), tone(0.35, 0.0), tone(1.0, 0.5)]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(ranges.len(), 1, "a 0.35s gap should clear the 0.3s minimum: {ranges:?}");
}

#[test]
fn multiple_pauses_are_all_found_in_order() {
    let samples = concat(vec![
        tone(0.5, 0.5),
        tone(0.5, 0.0), // silence #1: [0.5, 1.0)
        tone(0.5, 0.5),
        tone(0.5, 0.0), // silence #2: [1.5, 2.0)
        tone(0.5, 0.5),
    ]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(ranges.len(), 2, "expected two silent ranges, got {ranges:?}");
    assert!((ranges[0].0 - 0.5).abs() < 0.05);
    assert!((ranges[1].0 - 1.5).abs() < 0.05);
}

#[test]
fn low_but_not_silent_audio_is_measured_by_the_threshold_not_by_zero() {
    // A quiet room-tone track (well above digital zero, well below -40dBFS)
    // should still register as "silence" for editing purposes — that's the
    // whole point of a configurable dB threshold rather than checking for
    // exact zero.
    let quiet = 10f32.powf(-50.0 / 20.0); // -50 dBFS, below the -40 dBFS default threshold
    let samples = concat(vec![tone(1.0, 0.5), tone(0.5, quiet), tone(1.0, 0.5)]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(ranges.len(), 1, "quiet-but-nonzero room tone below the threshold should count as silence");
}

#[test]
fn audio_just_above_the_threshold_is_not_silence() {
    let just_above = 10f32.powf(-35.0 / 20.0); // -35 dBFS, above the -40 dBFS default threshold
    let samples = concat(vec![tone(1.0, 0.5), tone(1.0, just_above), tone(1.0, 0.5)]);
    let ranges = detect_silence(&samples, SAMPLE_RATE, &default_config());
    assert!(ranges.is_empty(), "audio above the threshold must not be flagged as silence");
}

#[test]
fn empty_or_short_audio_produces_no_ranges_and_does_not_panic() {
    assert!(detect_silence(&[], SAMPLE_RATE, &default_config()).is_empty());
    assert!(detect_silence(&tone(0.01, 0.0), SAMPLE_RATE, &default_config()).is_empty());
}

// ---- ranges_to_remove: padding, applied to already-detected silence ----

#[test]
fn padding_shrinks_the_removed_range_so_words_are_not_clipped() {
    // Raw silence detected as [1.0, 2.0); with 0.1s padding, only [1.1, 1.9)
    // should actually be removed, leaving a little breathing room on each
    // side so the edit doesn't clip the tail/head of adjacent speech.
    let config = SilenceConfig { threshold_dbfs: -40.0, min_silence_seconds: 0.3, padding_seconds: 0.1 };
    let removed = ranges_to_remove(&[(1.0, 2.0)], &config);
    assert_eq!(removed.len(), 1);
    assert!((removed[0].0 - 1.1).abs() < 1e-6);
    assert!((removed[0].1 - 1.9).abs() < 1e-6);
}

#[test]
fn padding_that_would_invert_a_short_range_drops_it_instead_of_going_negative() {
    // A raw silent range only slightly longer than 2x padding would invert
    // (end before start) if shrunk naively — it must be dropped, not turned
    // into a nonsensical negative-length range.
    let config = SilenceConfig { threshold_dbfs: -40.0, min_silence_seconds: 0.3, padding_seconds: 0.5 };
    let removed = ranges_to_remove(&[(1.0, 1.4)], &config); // only 0.4s wide, padding is 0.5s each side
    assert!(removed.is_empty(), "a range too short for its own padding must be dropped, got {removed:?}");
}

#[test]
fn zero_padding_removes_the_full_detected_range() {
    let config = SilenceConfig { threshold_dbfs: -40.0, min_silence_seconds: 0.3, padding_seconds: 0.0 };
    let removed = ranges_to_remove(&[(1.0, 2.0)], &config);
    assert_eq!(removed, vec![(1.0, 2.0)]);
}
