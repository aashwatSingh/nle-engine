//! EBU R128 / ITU-R BS.1770-4 loudness and true-peak, checked against the
//! standard's own published reference points rather than against whatever this
//! implementation happens to produce.
//!
//! These are *compliance* numbers — a broadcaster or a streaming platform
//! rejects a deliverable on them — so "looks about right" is not good enough,
//! and a plausible approximation is worse than an absent measurement. Every
//! assertion below is either a value the standard fixes (a 1 kHz sine at
//! -23 dBFS is -23 LUFS; a sine whose peaks fall between samples reads
//! 3 dB higher on a true-peak meter than on a sample meter) or an exact
//! algebraic relationship (halving amplitude is -6 LU), never a number read
//! off this code's own output.

use audio::{LoudnessAnalyzer, LoudnessMeasurement, SILENCE_DBFS};

const FS: u32 = 48_000;

/// Mono sine at `freq` Hz whose **RMS** is `rms_dbfs`. RMS, not peak: the
/// standard's calibration points are stated in RMS, and a sine's peak sits
/// 3.01 dB above its RMS — mixing the two up is exactly the error these tests
/// exist to catch.
fn sine(freq: f32, rms_dbfs: f32, seconds: f32) -> Vec<f32> {
    let amplitude = 10f32.powf(rms_dbfs / 20.0) * std::f32::consts::SQRT_2;
    let n = (FS as f32 * seconds) as usize;
    (0..n)
        .map(|i| amplitude * (std::f32::consts::TAU * freq * i as f32 / FS as f32).sin())
        .collect()
}

fn measure_mono(samples: &[f32]) -> LoudnessMeasurement {
    let mut a = LoudnessAnalyzer::new(FS, 1);
    a.push(samples);
    a.finish()
}

fn measure_stereo(interleaved: &[f32]) -> LoudnessMeasurement {
    let mut a = LoudnessAnalyzer::new(FS, 2);
    a.push(interleaved);
    a.finish()
}

#[test]
fn a_one_kilohertz_sine_at_minus_twenty_three_dbfs_measures_minus_twenty_three_lufs() {
    // BS.1770's calibration point, and the reason the algorithm carries its
    // -0.691 dB offset at all: the K-weighting shelf contributes about
    // +0.691 dB at 1 kHz, and the constant is chosen to cancel it so this
    // exact signal reads its own RMS level. Getting the offset, the shelf, or
    // the channel weighting wrong all show up right here.
    let m = measure_mono(&sine(1000.0, -23.0, 4.0));
    assert!(
        (m.integrated_lufs - -23.0).abs() < 0.2,
        "expected -23.0 LUFS for the standard calibration tone, got {}",
        m.integrated_lufs
    );
}

#[test]
fn a_one_kilohertz_sine_at_minus_ten_dbfs_measures_minus_ten_lufs() {
    // The same calibration at a different level, so the test above can't pass
    // by an offset that only happens to work at -23.
    let m = measure_mono(&sine(1000.0, -10.0, 4.0));
    assert!(
        (m.integrated_lufs - -10.0).abs() < 0.2,
        "expected -10.0 LUFS, got {}",
        m.integrated_lufs
    );
}

#[test]
fn halving_the_amplitude_lowers_integrated_loudness_by_exactly_six_lu() {
    // Pure algebra, independent of any filter coefficient: loudness is a log
    // of a mean square, so a factor of 2 in amplitude is 6.02 LU. A weighting
    // filter applied in the wrong place (per-block instead of per-sample, say)
    // would still pass the calibration tests above but break this one.
    let loud = measure_mono(&sine(1000.0, -20.0, 4.0)).integrated_lufs;
    let quiet = measure_mono(&sine(1000.0, -26.02, 4.0)).integrated_lufs;
    assert!(
        (loud - quiet - 6.02).abs() < 0.1,
        "expected exactly 6.02 LU apart, got {loud} and {quiet}"
    );
}

#[test]
fn the_same_signal_in_stereo_reads_about_three_lu_louder_than_in_mono() {
    // BS.1770 sums channel powers with weight 1.0 for L and R, so two
    // identical channels are 10*log10(2) = 3.01 LU louder than one. This is
    // the property that breaks if channels are *averaged* instead of summed —
    // a natural-seeming mistake that makes every stereo deliverable measure
    // 3 dB quiet.
    let mono = sine(1000.0, -23.0, 4.0);
    let stereo: Vec<f32> = mono.iter().flat_map(|&s| [s, s]).collect();

    let m = measure_mono(&mono).integrated_lufs;
    let s = measure_stereo(&stereo).integrated_lufs;
    assert!(
        (s - m - 3.01).abs() < 0.15,
        "stereo should be 3.01 LU above mono, got {s} vs {m}"
    );
}

#[test]
fn silence_measures_at_the_floor_rather_than_negative_infinity() {
    let m = measure_mono(&vec![0.0; FS as usize * 2]);
    assert_eq!(m.integrated_lufs, SILENCE_DBFS);
    assert!(m.integrated_lufs.is_finite(), "-inf breaks every comparison and every meter");
}

#[test]
fn a_signal_shorter_than_one_analysis_block_reports_silence_not_a_wrong_number() {
    // A 400 ms block is the smallest thing R128 can measure. Anything shorter
    // has no gated block to average, and inventing a number from a partial
    // block would be a compliance figure with nothing behind it.
    let m = measure_mono(&sine(1000.0, -23.0, 0.1));
    assert_eq!(m.integrated_lufs, SILENCE_DBFS);
}

#[test]
fn the_relative_gate_stops_a_long_silence_from_diluting_the_measurement() {
    // The whole point of R128's gating: integrated loudness describes the
    // programme, not the gaps in it. One second of tone followed by nine
    // seconds of silence must measure close to the tone's own loudness. An
    // ungated mean would report roughly 10 dB lower and be useless for
    // matching programme levels.
    let mut signal = sine(1000.0, -23.0, 1.0);
    signal.extend(std::iter::repeat(0.0).take(FS as usize * 9));

    let m = measure_mono(&signal);
    assert!(
        (m.integrated_lufs - -23.0).abs() < 1.0,
        "gating should keep this near the tone's -23 LUFS, got {}",
        m.integrated_lufs
    );
}

#[test]
fn pushing_in_many_small_chunks_matches_one_big_push() {
    // Export streams audio through in whatever block size the encoder wants,
    // so a measurement that depends on how the signal was chunked would be
    // reporting a property of the buffer size rather than of the audio.
    let signal = sine(1000.0, -20.0, 3.0);
    let whole = measure_mono(&signal).integrated_lufs;

    let mut chunked = LoudnessAnalyzer::new(FS, 1);
    for chunk in signal.chunks(577) {
        // A deliberately awkward size: coprime with both the 400 ms block and
        // the 100 ms hop, so no chunk boundary lines up with either.
        chunked.push(chunk);
    }
    let chunked = chunked.finish().integrated_lufs;

    assert!(
        (whole - chunked).abs() < 0.01,
        "chunking changed the answer: {whole} vs {chunked}"
    );
}

#[test]
fn true_peak_catches_an_inter_sample_peak_that_a_sample_meter_misses() {
    // The canonical case, and the reason true-peak exists at all: a sine at
    // exactly a quarter of the sample rate, offset by an eighth of a cycle,
    // lands every single sample at +/-0.7071 while the waveform between them
    // reaches full scale. A sample-peak meter reads -3.01 dBFS and says
    // there's 3 dB of headroom; the signal actually clips a converter.
    // The pattern repeats every 4 samples, so it's written from `i % 4`
    // rather than from a running phase: computing `PI * i / 2` in f32 for
    // i in the tens of thousands loses enough precision that the samples
    // drift measurably off +/-0.7071, which broke this test's own
    // precondition before it ever reached the assertion that matters.
    let n = FS as usize * 2;
    let samples: Vec<f32> = (0..n)
        .map(|i| {
            (std::f32::consts::PI * (i % 4) as f32 / 2.0 + std::f32::consts::FRAC_PI_4).sin()
        })
        .collect();

    let sample_peak = samples.iter().fold(0f32, |a, s| a.max(s.abs()));
    assert!(
        (audio::to_dbfs(sample_peak) - -3.01).abs() < 0.05,
        "precondition: every sample should sit at -3.01 dBFS, got {}",
        audio::to_dbfs(sample_peak)
    );

    let m = measure_mono(&samples);
    assert!(
        m.true_peak_dbtp > -0.5,
        "true peak should approach 0 dBTP where the waveform actually peaks, got {}",
        m.true_peak_dbtp
    );
    assert!(
        m.true_peak_dbtp < 0.5,
        "and must not overshoot far past it, got {}",
        m.true_peak_dbtp
    );
}

#[test]
fn true_peak_of_a_well_sampled_low_frequency_signal_matches_its_sample_peak() {
    // The negative case for the same property: oversampling must not *invent*
    // a peak where the waveform has none, or every mix would read as clipping.
    //
    // Deliberately a low-frequency sine rather than held DC. A constant that
    // starts abruptly at sample zero is a step, and a step genuinely does
    // contain inter-sample content — a correct true-peak meter overshoots on
    // one, so asserting otherwise would be asserting the reconstruction
    // filter is wrong. At 100 Hz there are 480 samples per cycle, so the
    // sampled peak lands within a hair of the real one and there is nothing
    // legitimate for the filter to find.
    let samples = sine(100.0, -20.0, 1.0);
    let sample_peak = samples.iter().fold(0f32, |a, s| a.max(s.abs()));
    let m = measure_mono(&samples);
    assert!(
        (m.true_peak_dbtp - audio::to_dbfs(sample_peak)).abs() < 0.2,
        "true peak {} should sit essentially on the sample peak {}",
        m.true_peak_dbtp,
        audio::to_dbfs(sample_peak)
    );
}

#[test]
fn loudness_range_is_wider_for_a_signal_that_changes_level_than_one_that_does_not() {
    // LRA describes how much the level moves over the programme. A steady tone
    // has essentially none; alternating loud and quiet sections has a lot.
    let steady = measure_mono(&sine(1000.0, -23.0, 12.0)).loudness_range_lu;

    // Segments must be longer than the 3 s analysis block, or every block
    // straddles a level change and averages the two levels back together —
    // the first version of this test used 2 s segments and measured a 3 LU
    // range on a signal spanning 20 dB, which said more about the block
    // length than about the audio.
    let mut dynamic = Vec::new();
    for i in 0..4 {
        let level = if i % 2 == 0 { -33.0 } else { -13.0 };
        dynamic.extend(sine(1000.0, level, 6.0));
    }
    let dynamic = measure_mono(&dynamic).loudness_range_lu;

    assert!(steady < 1.0, "a constant tone should have almost no range, got {steady} LU");
    assert!(
        dynamic > 10.0,
        "a signal alternating over a 20 dB span should show a wide range, got {dynamic} LU"
    );
}

// ---- loudness auto-match: the gain arithmetic ----

use audio::loudness::gain_to_reach_target;

#[test]
fn a_quiet_clip_gets_a_positive_gain_to_reach_a_louder_target() {
    assert!((gain_to_reach_target(-23.0, -14.0) - 9.0).abs() < 1e-9);
}

#[test]
fn a_loud_clip_gets_a_negative_gain_to_reach_a_quieter_target() {
    assert!((gain_to_reach_target(-10.0, -23.0) - -13.0).abs() < 1e-9);
}

#[test]
fn a_clip_already_at_the_target_gets_zero_gain() {
    assert_eq!(gain_to_reach_target(-16.0, -16.0), 0.0);
}

#[test]
fn a_clip_at_the_silence_floor_is_clamped_rather_than_wildly_boosted() {
    // Measured at the analyzer's -120 LUFS silence floor, a naive
    // `target - measured` for a -14 LUFS target would be +106dB — enough to
    // turn noise-floor hiss (or true silence) into ear damage. The clamp
    // exists precisely for this case.
    let gain = gain_to_reach_target(-120.0, -14.0);
    assert_eq!(gain, 24.0, "should clamp to the maximum boost, not chase an absurd target");
}

#[test]
fn an_extremely_loud_clip_is_clamped_rather_than_wildly_cut() {
    let gain = gain_to_reach_target(10.0, -50.0);
    assert_eq!(gain, -24.0, "should clamp to the maximum cut");
}

#[test]
fn gain_within_the_clamp_range_is_returned_exactly_unclamped() {
    // The clamp must only bite at the edges — an ordinary correction well
    // inside ±24dB should come back exact, not rounded toward the bound.
    assert!((gain_to_reach_target(-30.0, -14.0) - 16.0).abs() < 1e-9);
}
