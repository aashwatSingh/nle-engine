//! Onset (beat) detection via spectral flux — the classic technique for
//! "find the transients": an STFT magnitude spectrum computed per hop, the
//! **half-wave-rectified** frame-to-frame increase in that spectrum summed
//! into one novelty value per hop (a rising note or a drum hit shows up as a
//! sudden increase in energy across many frequency bins at once; a
//! decaying tail doesn't, which is exactly what half-wave rectification is
//! for), and adaptive peak-picking on the resulting novelty curve.
//!
//! Tested with synthetic click trains at known positions rather than real
//! music — a click is a broadband impulse, which is the cleanest possible
//! onset a detector can be asked to find, and it lets every assertion here
//! check against an exact, known ground truth instead of "sounds about
//! right".

use audio::beat::{detect_onsets, OnsetConfig};

const SAMPLE_RATE: u32 = 48_000;

fn silence(seconds: f64) -> Vec<f32> {
    vec![0.0; (seconds * SAMPLE_RATE as f64) as usize]
}

/// A single-sample-wide impulse at `at_seconds`, in an otherwise-silent
/// buffer of length `total_seconds`. A true impulse has energy spread evenly
/// across every frequency bin, which is what makes it register as a clean,
/// unambiguous onset regardless of window/hop size.
fn click_train(total_seconds: f64, click_times: &[f64]) -> Vec<f32> {
    let mut samples = silence(total_seconds);
    for &t in click_times {
        let i = (t * SAMPLE_RATE as f64) as usize;
        if i < samples.len() {
            samples[i] = 1.0;
        }
    }
    samples
}

fn default_config() -> OnsetConfig {
    OnsetConfig::default()
}

#[test]
fn silence_has_no_onsets() {
    let samples = silence(3.0);
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &default_config());
    assert!(onsets.is_empty(), "silence should have no onsets, got {onsets:?}");
}

#[test]
fn a_constant_tone_has_no_onsets() {
    // A steady sine has an unchanging spectrum after the first window, so
    // its flux is ~zero throughout — the negative case that proves flux is
    // measuring *change*, not just the presence of energy.
    //
    // The frequency is chosen to land exactly on an FFT bin centre for the
    // default 1024-sample window at 48kHz (bin width 46.875Hz; bin 10 is
    // 468.75Hz) rather than a "nicer" number like 440Hz. A tone that doesn't
    // land on an exact bin leaks energy across neighbouring bins by an
    // amount that depends on the window's exact sample-phase offset — real,
    // well-understood STFT behaviour, not a detector bug — so consecutive
    // hops of a non-bin-aligned tone genuinely do show small magnitude
    // differences. Testing "no change" needs a signal that truly doesn't
    // change under this analysis, which a bin-aligned tone is and 440Hz isn't.
    let freq = SAMPLE_RATE as f64 / 1024.0 * 10.0;
    let mut samples = Vec::with_capacity((2.0 * SAMPLE_RATE as f64) as usize);
    for n in 0..(2.0 * SAMPLE_RATE as f64) as usize {
        let t = n as f64 / SAMPLE_RATE as f64;
        samples.push((2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5);
    }
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &default_config());
    assert!(onsets.is_empty(), "a steady, bin-aligned tone should have no onsets, got {onsets:?}");
}

#[test]
fn a_single_click_is_found_near_its_true_position() {
    let samples = click_train(2.0, &[1.0]);
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(onsets.len(), 1, "expected exactly one onset, got {onsets:?}");
    assert!((onsets[0] - 1.0).abs() < 0.03, "onset should land within 30ms of the true click at 1.0s, got {onsets:?}");
}

#[test]
fn evenly_spaced_clicks_are_all_found_in_order() {
    let times = [0.5, 1.0, 1.5, 2.0, 2.5];
    let samples = click_train(3.0, &times);
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(onsets.len(), times.len(), "expected {} onsets, got {onsets:?}", times.len());
    for (found, &expected) in onsets.iter().zip(times.iter()) {
        assert!((found - expected).abs() < 0.03, "onset {found} should be near {expected}");
    }
}

#[test]
fn onsets_closer_than_the_minimum_interval_are_debounced() {
    // Two clicks 20ms apart are well inside a single drum hit's own
    // transient ringing in any real recording — reporting both as separate
    // beats would be nonsense for anything this feature is actually for
    // (snapping cuts to music). Only the first should survive.
    let config = OnsetConfig { min_interval_seconds: 0.1, ..OnsetConfig::default() };
    let samples = click_train(2.0, &[1.0, 1.02]);
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &config);
    assert_eq!(onsets.len(), 1, "two clicks 20ms apart should debounce to one onset, got {onsets:?}");
}

#[test]
fn onsets_farther_than_the_minimum_interval_are_both_kept() {
    let config = OnsetConfig { min_interval_seconds: 0.1, ..OnsetConfig::default() };
    let samples = click_train(2.0, &[1.0, 1.5]);
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &config);
    assert_eq!(onsets.len(), 2, "two clicks 0.5s apart should both survive debouncing, got {onsets:?}");
}

#[test]
fn a_slow_swell_longer_than_the_debounce_window_is_not_reported_as_many_onsets() {
    // Debouncing alone (collapsing anything within `min_interval_seconds`)
    // isn't the same thing as peak-picking, and this is the case that tells
    // them apart: a smooth rise in energy spanning *longer* than the
    // debounce window has many consecutive above-threshold hops (each hop
    // louder than the last, so each has positive flux) that debounce cannot
    // collapse on its own — only requiring each reported onset to be a
    // genuine local maximum keeps this from being reported as a dozen
    // separate "beats" instead of essentially one swell.
    let config = OnsetConfig { min_interval_seconds: 0.1, ..OnsetConfig::default() };
    let ramp_seconds = 0.4; // longer than the 0.1s debounce window
    let mut samples = silence(0.5);
    let mut state: u32 = 999;
    for i in 0..(ramp_seconds * SAMPLE_RATE as f64) as usize {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (state >> 16) as f32 / 65535.0 - 0.5;
        // Rises smoothly to full amplitude and back down — one broad hump,
        // not a train of distinct hits.
        let progress = i as f64 / (ramp_seconds * SAMPLE_RATE as f64);
        let envelope = (std::f64::consts::PI * progress).sin(); // 0 -> 1 -> 0
        samples.push(noise * envelope as f32);
    }
    samples.extend(silence(0.5));

    let onsets = detect_onsets(&samples, SAMPLE_RATE, &config);
    assert!(
        onsets.len() <= 2,
        "a single smooth swell should register as roughly one event, not a burst of them: {onsets:?}"
    );
}

#[test]
fn empty_or_short_audio_produces_no_onsets_and_does_not_panic() {
    assert!(detect_onsets(&[], SAMPLE_RATE, &default_config()).is_empty());
    assert!(detect_onsets(&silence(0.001), SAMPLE_RATE, &default_config()).is_empty());
}

#[test]
fn a_loud_click_among_quiet_background_hiss_is_still_found() {
    // Real audio is never true digital silence between beats — there's
    // always some noise floor. A detector tuned only against perfect
    // silence would be useless on anything real.
    let mut samples = silence(2.0);
    // Deterministic pseudo-noise (not `rand`, to keep this test dependency-free
    // and perfectly reproducible): a cheap linear congruential sequence is
    // plenty for "quiet broadband hiss", not for anything cryptographic.
    let mut state: u32 = 12345;
    for s in samples.iter_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *s = ((state >> 16) as f32 / 65535.0 - 0.5) * 0.01; // small amplitude hiss
    }
    let click_at = (1.0 * SAMPLE_RATE as f64) as usize;
    samples[click_at] = 1.0;
    let onsets = detect_onsets(&samples, SAMPLE_RATE, &default_config());
    assert_eq!(onsets.len(), 1, "the click should still stand out against quiet hiss, got {onsets:?}");
    assert!((onsets[0] - 1.0).abs() < 0.03);
}
