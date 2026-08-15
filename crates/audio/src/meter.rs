//! Metering, per spec 4.6: per-track/master peak+RMS, true-peak limiting on
//! export, and integrated loudness (LUFS, EBU R128) since deliverables require
//! it.
//!
//! Peak/RMS lives here (see [`PeakRms::measure`]) and feeds the mixer panel's
//! moving meters. True-peak and EBU R128 integrated loudness are real
//! measurements too, but they need oversampling, K-weighting and gated blocks
//! rather than a single pass over a buffer, so they live in
//! [`crate::loudness`] — see that module for why they can't share this one's
//! per-block shape.

/// Level floor in dBFS. Digital silence has no logarithm, so it reports as this
/// rather than `-inf` — meters, comparisons, and serialisation all behave, and
/// -120 dB is far below anything audible.
pub const SILENCE_DBFS: f32 = -120.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakRms {
    pub peak_dbfs: f32,
    pub rms_dbfs: f32,
}

impl Default for PeakRms {
    fn default() -> Self {
        PeakRms { peak_dbfs: SILENCE_DBFS, rms_dbfs: SILENCE_DBFS }
    }
}

impl PeakRms {
    /// Measures interleaved (or mono — it doesn't care) samples.
    ///
    /// Peak is the largest absolute sample; RMS is the root mean square over
    /// the whole slice. Both in dBFS relative to 1.0 full scale.
    ///
    /// Note RMS here is *unweighted and ungated*, over exactly the block it's
    /// given. That's the right thing for a moving level meter, and explicitly
    /// not a loudness measurement — RMS of a block is not LUFS, and reporting
    /// one as the other is how mixes end up delivered at the wrong level.
    pub fn measure(samples: &[f32]) -> Self {
        if samples.is_empty() {
            return Self::default();
        }
        let mut peak = 0.0f32;
        // Accumulated in f64: a block of 48k samples summing squares in f32
        // loses precision fast, and a level meter that drifts with block size
        // is worse than no meter.
        let mut sum_sq = 0.0f64;
        for &s in samples {
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            sum_sq += (s as f64) * (s as f64);
        }
        let rms = (sum_sq / samples.len() as f64).sqrt() as f32;
        PeakRms { peak_dbfs: to_dbfs(peak), rms_dbfs: to_dbfs(rms) }
    }

    /// The louder of two measurements, per field. Used to fold several blocks
    /// into one meter reading without losing a transient in the earlier one.
    pub fn max(self, other: Self) -> Self {
        PeakRms {
            peak_dbfs: self.peak_dbfs.max(other.peak_dbfs),
            rms_dbfs: self.rms_dbfs.max(other.rms_dbfs),
        }
    }
}

/// Linear amplitude to dBFS, floored at [`SILENCE_DBFS`].
pub fn to_dbfs(amplitude: f32) -> f32 {
    if amplitude <= 0.0 {
        return SILENCE_DBFS;
    }
    (20.0 * amplitude.log10()).max(SILENCE_DBFS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_reads_at_the_floor_not_negative_infinity() {
        // -inf propagates through every comparison and draws as a NaN-width
        // meter bar; the floor is what makes the value usable.
        let m = PeakRms::measure(&[0.0; 128]);
        assert_eq!(m.peak_dbfs, SILENCE_DBFS);
        assert_eq!(m.rms_dbfs, SILENCE_DBFS);
        assert!(m.peak_dbfs.is_finite());
    }

    #[test]
    fn an_empty_block_is_silence_rather_than_a_panic() {
        let m = PeakRms::measure(&[]);
        assert_eq!(m, PeakRms::default());
    }

    #[test]
    fn full_scale_sine_peaks_at_zero_and_reads_minus_three_rms() {
        // The textbook relationship: a sine's RMS is its peak / sqrt(2), i.e.
        // 3.01 dB below peak. If RMS and peak were computed the same way (a
        // common slip) this would read 0 for both.
        let n = 4800;
        let sine: Vec<f32> = (0..n)
            .map(|i| (i as f32 / n as f32 * 100.0 * std::f32::consts::TAU).sin())
            .collect();
        let m = PeakRms::measure(&sine);
        assert!((m.peak_dbfs - 0.0).abs() < 0.1, "peak should be ~0 dBFS, got {}", m.peak_dbfs);
        assert!(
            (m.rms_dbfs + 3.01).abs() < 0.1,
            "a sine's RMS should be 3.01 dB below its peak, got {}",
            m.rms_dbfs
        );
    }

    #[test]
    fn half_amplitude_reads_about_minus_six_dbfs() {
        let m = PeakRms::measure(&[0.5, -0.5, 0.5, -0.5]);
        assert!((m.peak_dbfs + 6.02).abs() < 0.05, "got {}", m.peak_dbfs);
        // A square wave's RMS equals its peak.
        assert!((m.rms_dbfs + 6.02).abs() < 0.05, "got {}", m.rms_dbfs);
    }

    #[test]
    fn rms_does_not_drift_with_block_length() {
        // Guards the f64 accumulator: summing squares in f32 over a long block
        // loses precision, and a meter whose reading depends on buffer size is
        // untrustworthy.
        let short = PeakRms::measure(&vec![0.25f32; 512]);
        let long = PeakRms::measure(&vec![0.25f32; 480_000]);
        assert!(
            (short.rms_dbfs - long.rms_dbfs).abs() < 0.01,
            "same signal, different block lengths: {} vs {}",
            short.rms_dbfs,
            long.rms_dbfs
        );
    }

    #[test]
    fn max_folds_blocks_without_losing_a_transient() {
        let quiet = PeakRms::measure(&[0.01; 64]);
        let loud = PeakRms::measure(&[0.9; 64]);
        assert_eq!(quiet.max(loud).peak_dbfs, loud.peak_dbfs);
        assert_eq!(loud.max(quiet).peak_dbfs, loud.peak_dbfs);
    }
}
