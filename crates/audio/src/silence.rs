//! Silence detection over raw PCM samples: find gaps worth ripple-deleting.
//!
//! Two-stage, mirroring `render::scene_cut`'s detect/apply split: `detect_
//! silence` measures RMS in fixed windows and reports the **raw** silent
//! spans in seconds (nothing shrunk yet — that's a separate, explicit
//! decision). `ranges_to_remove` then pads each span inward so an edit built
//! from it doesn't clip the tail or head of adjacent speech. Keeping these
//! separate means a caller that only wants to *see* where the pauses are
//! (a UI overlay, say) doesn't have to reason about padding at all.
//!
//! Windowed RMS, not a sample-by-sample threshold: a single near-zero sample
//! in the middle of loud speech (there are plenty, since audio crosses zero
//! constantly) must not fragment one pause into a hundred one-sample
//! "silences". The window size is fixed at 10ms — short enough to place a
//! cut boundary precisely, long enough that its own RMS is a meaningful
//! loudness estimate rather than an artifact of exactly where the window
//! happened to land on the waveform.

const WINDOW_SECONDS: f64 = 0.01;

pub struct SilenceConfig {
    /// RMS level, in dBFS, below which a window counts as silent.
    pub threshold_dbfs: f64,
    /// A silent stretch shorter than this is an ordinary pause between
    /// words, not dead air — not flagged at all.
    pub min_silence_seconds: f64,
    /// How much of each flagged range `ranges_to_remove` shrinks inward from
    /// both ends, so the actual cut doesn't clip adjacent speech.
    pub padding_seconds: f64,
}

fn rms_dbfs(window: &[f32]) -> f64 {
    if window.is_empty() {
        return f64::NEG_INFINITY;
    }
    let mean_square: f64 = window.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>() / window.len() as f64;
    if mean_square <= 0.0 {
        return f64::NEG_INFINITY;
    }
    10.0 * mean_square.log10()
}

/// Raw silent ranges, as `(start_seconds, end_seconds)`, in order. `samples`
/// is mono (or one interleaved channel — silence detection doesn't need
/// stereo separation the way loudness metering does).
pub fn detect_silence(samples: &[f32], sample_rate: u32, config: &SilenceConfig) -> Vec<(f64, f64)> {
    let window_len = ((WINDOW_SECONDS * sample_rate as f64) as usize).max(1);
    if samples.len() < window_len {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut silence_start: Option<usize> = None;
    let window_count = samples.len() / window_len;

    for w in 0..window_count {
        let window = &samples[w * window_len..(w + 1) * window_len];
        let is_silent = rms_dbfs(window) < config.threshold_dbfs;
        match (is_silent, silence_start) {
            (true, None) => silence_start = Some(w),
            (false, Some(start)) => {
                push_if_long_enough(&mut ranges, start, w, window_len, sample_rate, config);
                silence_start = None;
            }
            _ => {}
        }
    }
    if let Some(start) = silence_start {
        push_if_long_enough(&mut ranges, start, window_count, window_len, sample_rate, config);
    }
    ranges
}

fn push_if_long_enough(
    ranges: &mut Vec<(f64, f64)>,
    start_window: usize,
    end_window: usize,
    window_len: usize,
    sample_rate: u32,
    config: &SilenceConfig,
) {
    let start_s = (start_window * window_len) as f64 / sample_rate as f64;
    let end_s = (end_window * window_len) as f64 / sample_rate as f64;
    if end_s - start_s >= config.min_silence_seconds {
        ranges.push((start_s, end_s));
    }
}

/// Shrinks each of `silent_ranges` inward by `config.padding_seconds` on both
/// ends. A range too short for its own padding (would invert) is dropped
/// rather than emitted as a negative-length span.
pub fn ranges_to_remove(silent_ranges: &[(f64, f64)], config: &SilenceConfig) -> Vec<(f64, f64)> {
    silent_ranges
        .iter()
        .filter_map(|&(start, end)| {
            let padded = (start + config.padding_seconds, end - config.padding_seconds);
            (padded.1 > padded.0).then_some(padded)
        })
        .collect()
}
