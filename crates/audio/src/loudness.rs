//! ITU-R BS.1770-4 loudness and true-peak, and EBU Tech 3342 loudness range.
//!
//! These are compliance numbers — a broadcaster or streaming platform accepts
//! or rejects a deliverable on them — so this implements the actual specified
//! algorithm rather than an approximation of it. Where the standard fixes a
//! constant (the K-weighting filter's centre frequencies and Q values, the
//! -0.691 dB offset, the -70 LUFS absolute gate, the -10 LU relative gate,
//! the 400 ms block with 75% overlap), that constant appears here verbatim
//! with a note on what it does.
//!
//! **Streaming, not batch.** [`LoudnessAnalyzer`] takes audio in whatever
//! block sizes the caller has and keeps only the running state each stage
//! needs, because export hands frames over as it encodes them and holding a
//! whole programme in memory to measure it afterwards would defeat that.
//! Gating is what forces the one unavoidable retention: the relative gate's
//! threshold isn't known until the last block has been seen, so per-block mean
//! squares are kept (8 bytes per 100 ms per channel — about 3 KB per hour).

use crate::meter::{to_dbfs, SILENCE_DBFS};

/// The gain, in dB, that would bring a clip measured at `measured_lufs` to
/// `target_lufs` — loudness auto-match's whole arithmetic, in one place so it
/// can be reasoned about (and tested) independent of decoding or applying it.
///
/// **Clamped to ±24dB.** Two real failure modes this guards against: a
/// clip measured at or near the analyzer's `SILENCE_DBFS` floor (true
/// silence, or audio too short to gate any blocks in) would otherwise call
/// for a gain in the hundreds of dB to reach an ordinary target — applying
/// that would amplify noise-floor hiss into a deafening blast, not "match"
/// anything. And a clip that's already far louder than the target (an air-
/// horn sample, badly clipped audio) calling for an equally extreme cut is
/// the same problem in the other direction. ±24dB comfortably covers every
/// real-world "this track is quieter/louder than the others" case while
/// refusing to manufacture nonsense from a degenerate measurement.
pub fn gain_to_reach_target(measured_lufs: f64, target_lufs: f64) -> f64 {
    (target_lufs - measured_lufs).clamp(-24.0, 24.0)
}

/// EBU R128 loudness measurement over some window (per-clip or whole
/// sequence, depending on caller).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeasurement {
    pub integrated_lufs: f32,
    pub true_peak_dbtp: f32,
    pub loudness_range_lu: f32,
}

impl Default for LoudnessMeasurement {
    fn default() -> Self {
        LoudnessMeasurement {
            integrated_lufs: SILENCE_DBFS,
            true_peak_dbtp: SILENCE_DBFS,
            loudness_range_lu: 0.0,
        }
    }
}

/// The offset in BS.1770's loudness equation, in dB. Not arbitrary: the
/// K-weighting shelf contributes about +0.691 dB at 1 kHz, and this cancels it
/// so a 1 kHz sine reads its own RMS level in LUFS. That identity is what
/// `a_one_kilohertz_sine_at_minus_twenty_three_dbfs_measures_minus_twenty_three_lufs`
/// pins.
const LOUDNESS_OFFSET_DB: f64 = -0.691;

/// Blocks quieter than this are dropped outright, before any relative
/// threshold is computed. Stops a long silence from dragging the answer down.
const ABSOLUTE_GATE_LUFS: f64 = -70.0;

/// Blocks more than this far below the (absolutely-gated) mean are dropped in
/// the second pass. This is what makes integrated loudness describe the
/// programme rather than the gaps between its parts.
const RELATIVE_GATE_LU: f64 = -10.0;

/// R128's analysis block. 400 ms with a 100 ms hop (75% overlap).
const BLOCK_MS: f64 = 400.0;
const HOP_MS: f64 = 100.0;

/// EBU Tech 3342's loudness-range block: 3 s with a 1 s hop, and its own
/// relative gate, both wider than the integrated measurement's.
const LRA_BLOCK_MS: f64 = 3000.0;
const LRA_HOP_MS: f64 = 1000.0;
const LRA_RELATIVE_GATE_LU: f64 = -20.0;
const LRA_LOW_PERCENTILE: f64 = 10.0;
const LRA_HIGH_PERCENTILE: f64 = 95.0;

/// A two-pole IIR section in direct form I.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f64,
    b1: f64,
    b2: f64,
    a1: f64,
    a2: f64,
}

#[derive(Clone, Copy, Default)]
struct BiquadState {
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Biquad {
    fn process(&self, s: &mut BiquadState, x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * s.x1 + self.b2 * s.x2 - self.a1 * s.y1 - self.a2 * s.y2;
        s.x2 = s.x1;
        s.x1 = x;
        s.y2 = s.y1;
        s.y1 = y;
        y
    }

    /// BS.1770's stage 1: a high-frequency shelf approximating the acoustic
    /// effect of a head in a diffuse field. The magic numbers are the
    /// standard's own, given there for 48 kHz; deriving the coefficients from
    /// `fs` (rather than hardcoding the 48 kHz table) is what lets this
    /// measure a 44.1 or 96 kHz programme correctly instead of silently
    /// mis-weighting it.
    fn k_weighting_shelf(fs: f64) -> Biquad {
        let f0 = 1681.974450955533;
        let gain_db = 3.999843853973347;
        let q = 0.7071752369554196;

        let k = (std::f64::consts::PI * f0 / fs).tan();
        let vh = 10f64.powf(gain_db / 20.0);
        let vb = vh.powf(0.4996667741545416);
        let denom = 1.0 + k / q + k * k;

        Biquad {
            b0: (vh + vb * k / q + k * k) / denom,
            b1: 2.0 * (k * k - vh) / denom,
            b2: (vh - vb * k / q + k * k) / denom,
            a1: 2.0 * (k * k - 1.0) / denom,
            a2: (1.0 - k / q + k * k) / denom,
        }
    }

    /// BS.1770's stage 2: a high-pass that discards subsonic energy the ear
    /// doesn't register as loudness but which would otherwise dominate the
    /// mean square of anything with rumble or DC offset.
    fn k_weighting_highpass(fs: f64) -> Biquad {
        let f0 = 38.13547087602444;
        let q = 0.5003270373238773;
        let k = (std::f64::consts::PI * f0 / fs).tan();
        let denom = 1.0 + k / q + k * k;

        Biquad {
            b0: 1.0,
            b1: -2.0,
            b2: 1.0,
            a1: 2.0 * (k * k - 1.0) / denom,
            a2: (1.0 - k / q + k * k) / denom,
        }
    }
}

/// Per-channel weights from BS.1770 Table 3. Left/right/centre count fully;
/// the surround channels are weighted up because sound arriving from the
/// sides is perceived as louder than the same power from the front. LFE is
/// excluded entirely, which is why a 5.1 layout weights only 5 channels.
fn channel_weight(index: usize, channels: usize) -> f64 {
    match (channels, index) {
        // Mono and stereo: every channel counts fully.
        (1, _) | (2, _) => 1.0,
        // 5.1 in the usual L R C LFE Ls Rs order.
        (6, 3) => 0.0,
        (6, 4) | (6, 5) => 1.41,
        // Anything else (or an unrecognised layout): count everything fully
        // rather than guess at an ordering. Wrong-but-predictable beats
        // silently applying a surround weight to a channel that isn't one.
        _ => 1.0,
    }
}

/// 4x oversampling reconstruction filter for true-peak, as a polyphase bank.
///
/// BS.1770 Annex 2 specifies a 4x oversampled peak with a 48-tap filter; this
/// uses a windowed-sinc of the same order derived at build time rather than a
/// transcribed coefficient table, so it can't be mistyped. `TAPS_PER_PHASE`
/// sets the accuracy: too short and the reconstruction under-reads a genuine
/// inter-sample peak, which is the failure that matters (a true-peak meter
/// that reads low tells you there's headroom when there isn't).
const OVERSAMPLE: usize = 4;
const TAPS_PER_PHASE: usize = 12;

struct PolyphaseUpsampler {
    /// `phases[p][k]` multiplies `history[k]` to produce output phase `p`.
    phases: [[f64; TAPS_PER_PHASE]; OVERSAMPLE],
    history: Vec<f64>,
}

impl PolyphaseUpsampler {
    fn new(channels: usize) -> Self {
        let total = OVERSAMPLE * TAPS_PER_PHASE;
        let mut proto = vec![0.0f64; total];
        let centre = (total - 1) as f64 / 2.0;
        for (i, tap) in proto.iter_mut().enumerate() {
            let x = i as f64 - centre;
            // sinc with cutoff at the original Nyquist, i.e. 1/OVERSAMPLE of
            // the upsampled rate.
            let sinc = if x.abs() < 1e-12 {
                1.0 / OVERSAMPLE as f64
            } else {
                (std::f64::consts::PI * x / OVERSAMPLE as f64).sin()
                    / (std::f64::consts::PI * x)
            };
            // Blackman-Harris: its deep stopband is what keeps the filter from
            // ringing a peak into existence on a signal that has none —
            // checked by
            // `true_peak_of_a_signal_with_no_inter_sample_content_matches_its_sample_peak`.
            let t = std::f64::consts::TAU * i as f64 / (total - 1) as f64;
            let w = 0.35875 - 0.48829 * t.cos() + 0.14128 * (2.0 * t).cos()
                - 0.01168 * (3.0 * t).cos();
            *tap = sinc * w;
        }
        // Normalise so a constant input comes back at its own level rather
        // than scaled by whatever the window happened to sum to.
        for phase in 0..OVERSAMPLE {
            let sum: f64 = (0..TAPS_PER_PHASE).map(|k| proto[k * OVERSAMPLE + phase]).sum();
            if sum.abs() > 1e-12 {
                for k in 0..TAPS_PER_PHASE {
                    proto[k * OVERSAMPLE + phase] /= sum;
                }
            }
        }
        let mut phases = [[0.0f64; TAPS_PER_PHASE]; OVERSAMPLE];
        for (phase, row) in phases.iter_mut().enumerate() {
            for (k, coeff) in row.iter_mut().enumerate() {
                *coeff = proto[k * OVERSAMPLE + phase];
            }
        }
        PolyphaseUpsampler { phases, history: vec![0.0; TAPS_PER_PHASE * channels] }
    }

    /// Feeds one sample of `channel` and returns the largest absolute value
    /// among the `OVERSAMPLE` reconstructed samples it produces.
    fn push_max_abs(&mut self, channel: usize, channels: usize, sample: f64) -> f64 {
        let base = channel * TAPS_PER_PHASE;
        let hist = &mut self.history[base..base + TAPS_PER_PHASE];
        hist.rotate_right(1);
        hist[0] = sample;
        let _ = channels;

        let mut peak = 0.0f64;
        for phase in &self.phases {
            let mut acc = 0.0f64;
            for (k, coeff) in phase.iter().enumerate() {
                acc += coeff * hist[k];
            }
            peak = peak.max(acc.abs());
        }
        peak
    }
}

/// Streaming BS.1770 loudness and true-peak analyser.
///
/// Feed it interleaved audio with [`push`](Self::push) in any block sizes and
/// call [`finish`](Self::finish) once. See the module doc on why this is
/// streaming and what it must nonetheless retain.
pub struct LoudnessAnalyzer {
    channels: usize,
    /// Samples per channel in one 100 ms hop.
    hop_frames: usize,
    /// How many hops make up one 400 ms analysis block.
    hops_per_block: usize,
    /// Same, for the 3 s loudness-range block.
    hops_per_lra_block: usize,

    shelf: Biquad,
    highpass: Biquad,
    shelf_state: Vec<BiquadState>,
    highpass_state: Vec<BiquadState>,

    /// Sum of squared K-weighted samples in the hop being filled, per channel.
    hop_sum_sq: Vec<f64>,
    /// Frames accumulated into the current hop so far.
    hop_frames_filled: usize,
    /// Position within the interleaved frame, so a `push` can split a frame
    /// across calls without losing channel alignment.
    partial_channel: usize,

    /// Per-hop, per-channel mean squares. The one thing that must be retained
    /// — see the module doc.
    hops: Vec<Vec<f64>>,

    upsampler: PolyphaseUpsampler,
    true_peak: f64,
}

impl LoudnessAnalyzer {
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let fs = sample_rate.max(1) as f64;
        let hop_frames = ((fs * HOP_MS / 1000.0).round() as usize).max(1);
        LoudnessAnalyzer {
            channels,
            hop_frames,
            hops_per_block: (BLOCK_MS / HOP_MS).round() as usize,
            hops_per_lra_block: (LRA_BLOCK_MS / LRA_HOP_MS).round() as usize
                * (LRA_HOP_MS / HOP_MS).round() as usize,
            shelf: Biquad::k_weighting_shelf(fs),
            highpass: Biquad::k_weighting_highpass(fs),
            shelf_state: vec![BiquadState::default(); channels],
            highpass_state: vec![BiquadState::default(); channels],
            hop_sum_sq: vec![0.0; channels],
            hop_frames_filled: 0,
            partial_channel: 0,
            hops: Vec::new(),
            upsampler: PolyphaseUpsampler::new(channels),
            true_peak: 0.0,
        }
    }

    /// Feeds interleaved samples. Any block size; frames may be split across
    /// calls.
    pub fn push(&mut self, interleaved: &[f32]) {
        for &sample in interleaved {
            let ch = self.partial_channel;
            let x = sample as f64;

            // True peak runs on the *unweighted* signal: it's about what a
            // converter has to reproduce, not about what the ear hears.
            self.true_peak = self.true_peak.max(self.upsampler.push_max_abs(ch, self.channels, x));

            let shelved = self.shelf.process(&mut self.shelf_state[ch], x);
            let weighted = self.highpass.process(&mut self.highpass_state[ch], shelved);
            self.hop_sum_sq[ch] += weighted * weighted;

            self.partial_channel += 1;
            if self.partial_channel == self.channels {
                self.partial_channel = 0;
                self.hop_frames_filled += 1;
                if self.hop_frames_filled == self.hop_frames {
                    self.close_hop();
                }
            }
        }
    }

    fn close_hop(&mut self) {
        let frames = self.hop_frames as f64;
        self.hops.push(self.hop_sum_sq.iter().map(|s| s / frames).collect());
        self.hop_sum_sq.iter_mut().for_each(|s| *s = 0.0);
        self.hop_frames_filled = 0;
    }

    /// Loudness of every overlapping block of `hops_per_block` hops, paired
    /// with the block's summed weighted power (needed to average blocks in the
    /// power domain, which is what the standard specifies — averaging decibels
    /// instead would be a different and wrong number).
    fn blocks(&self, hops_per_block: usize) -> Vec<(f64, f64)> {
        if self.hops.len() < hops_per_block {
            return Vec::new();
        }
        (0..=self.hops.len() - hops_per_block)
            .filter_map(|start| {
                let window = &self.hops[start..start + hops_per_block];
                let mut power = 0.0;
                for ch in 0..self.channels {
                    let mean_sq: f64 =
                        window.iter().map(|h| h[ch]).sum::<f64>() / window.len() as f64;
                    power += channel_weight(ch, self.channels) * mean_sq;
                }
                (power > 0.0).then(|| (LOUDNESS_OFFSET_DB + 10.0 * power.log10(), power))
            })
            .collect()
    }

    fn integrated(&self) -> f64 {
        let blocks = self.blocks(self.hops_per_block);
        // First pass: absolute gate only.
        let above_absolute: Vec<(f64, f64)> =
            blocks.into_iter().filter(|(l, _)| *l > ABSOLUTE_GATE_LUFS).collect();
        if above_absolute.is_empty() {
            return SILENCE_DBFS as f64;
        }
        let mean_power =
            above_absolute.iter().map(|(_, p)| p).sum::<f64>() / above_absolute.len() as f64;
        let relative_threshold = LOUDNESS_OFFSET_DB + 10.0 * mean_power.log10() + RELATIVE_GATE_LU;

        // Second pass: drop everything below the relative threshold, then
        // average what's left — in the power domain.
        let kept: Vec<f64> = above_absolute
            .iter()
            .filter(|(l, _)| *l > relative_threshold)
            .map(|(_, p)| *p)
            .collect();
        if kept.is_empty() {
            return SILENCE_DBFS as f64;
        }
        let power = kept.iter().sum::<f64>() / kept.len() as f64;
        LOUDNESS_OFFSET_DB + 10.0 * power.log10()
    }

    fn loudness_range(&self) -> f64 {
        let blocks = self.blocks(self.hops_per_lra_block);
        let above_absolute: Vec<(f64, f64)> =
            blocks.into_iter().filter(|(l, _)| *l > ABSOLUTE_GATE_LUFS).collect();
        if above_absolute.len() < 2 {
            return 0.0;
        }
        let mean_power =
            above_absolute.iter().map(|(_, p)| p).sum::<f64>() / above_absolute.len() as f64;
        let threshold =
            LOUDNESS_OFFSET_DB + 10.0 * mean_power.log10() + LRA_RELATIVE_GATE_LU;

        let mut kept: Vec<f64> =
            above_absolute.iter().map(|(l, _)| *l).filter(|l| *l > threshold).collect();
        if kept.len() < 2 {
            return 0.0;
        }
        kept.sort_by(|a, b| a.partial_cmp(b).unwrap());
        percentile(&kept, LRA_HIGH_PERCENTILE) - percentile(&kept, LRA_LOW_PERCENTILE)
    }

    /// Finishes the measurement. Any partially-filled hop is discarded rather
    /// than scaled up to a full one: a short tail measured as if it were a
    /// whole 100 ms would report energy the programme doesn't contain.
    pub fn finish(&self) -> LoudnessMeasurement {
        LoudnessMeasurement {
            integrated_lufs: self.integrated() as f32,
            true_peak_dbtp: to_dbfs(self.true_peak as f32),
            loudness_range_lu: self.loudness_range() as f32,
        }
    }
}

/// Linear-interpolated percentile of an already-sorted slice.
fn percentile(sorted: &[f64], pct: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let rank = (pct / 100.0) * (sorted.len() - 1) as f64;
    let lo = rank.floor() as usize;
    let hi = rank.ceil() as usize;
    if lo == hi {
        return sorted[lo];
    }
    let frac = rank - lo as f64;
    sorted[lo] * (1.0 - frac) + sorted[hi] * frac
}

impl LoudnessMeasurement {
    /// Measures a whole interleaved buffer in one call. The batch convenience
    /// over [`LoudnessAnalyzer`]; export uses the streaming form.
    pub fn analyze(interleaved: &[f32], channels: usize, sample_rate: u32) -> Self {
        let mut a = LoudnessAnalyzer::new(sample_rate, channels);
        a.push(interleaved);
        a.finish()
    }
}
