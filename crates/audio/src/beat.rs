//! Onset (beat) detection via spectral flux, for "snap this cut to the
//! music" — CapCut's beat-sync, minus the closed-source part.
//!
//! **Spectral flux, not a time-domain energy threshold.** A transient (a
//! drum hit, a clap, a plucked string) shows up as a sudden increase in
//! energy spread across *many frequency bins at once*, not just as a louder
//! sample — a time-domain RMS/peak threshold would just be another silence
//! detector and would miss a hit that isn't much louder than the music
//! already playing under it. Comparing successive magnitude spectra directly
//! measures "how much did the timbre just change", which is the actual
//! signature of a hit.
//!
//! **Half-wave rectified.** Only *increases* in a bin's magnitude count
//! toward the flux sum; a bin's energy decaying after a hit (which happens
//! on every hit, constantly) must not itself look like a second onset.
//!
//! **Adaptive peak-picking**, same reasoning as `render::scene_cut`'s
//! adaptive cut threshold: a fixed flux threshold tuned for one track's
//! loudness and density is wrong for a quieter or busier one. A flux value
//! only counts as an onset if it's both a local maximum *and* above the
//! novelty curve's own mean + k·stddev.

use rustfft::num_complex::Complex32;
use rustfft::FftPlanner;

pub struct OnsetConfig {
    /// FFT window size, in samples. 1024 at 48kHz is ~21ms — short enough to
    /// place an onset precisely, long enough for a meaningful spectrum.
    pub window_size: usize,
    /// Hop between windows. Half the window (50% overlap) is the standard
    /// STFT trade between time resolution and redundancy.
    pub hop_size: usize,
    /// Peaks below `mean + threshold_multiplier * stddev` of the novelty
    /// curve are not onsets.
    pub threshold_multiplier: f64,
    /// A second, independent floor: a peak must also reach at least this
    /// fraction of the clip's single loudest flux value. Needed because
    /// `mean + k*stddev` alone is a *relative* threshold, and relative
    /// thresholds break down when the whole clip is one dynamic level (loud
    /// throughout, or quiet throughout) — the tallest bump in an otherwise
    /// flat sequence is *always* several stddevs above the mean of that same
    /// flat sequence, onset or not.
    pub min_flux_fraction_of_peak: f64,
    /// A third, genuinely absolute floor, independent of anything measured
    /// from this clip. Exists for the degenerate case the other two can't
    /// cover: a signal with **no real onsets at all** (silence, or a steady
    /// tone whose spectrum truly doesn't change). There, every flux value —
    /// including the "peak" — is floating-point rounding noise from the FFT
    /// itself, on the order of 1e-10 for normalised -1..1 audio. A relative
    /// floor computed from that same noise is still noise-scale and provides
    /// no real protection; a real onset's flux for -1..1 audio is orders of
    /// magnitude above the default here, so this floor costs real onsets
    /// nothing while reliably silencing a clip that has none.
    pub min_absolute_flux: f64,
    /// Two onsets closer together than this collapse to the first — a real
    /// hit's own transient ringing must not register as multiple beats.
    pub min_interval_seconds: f64,
}

impl Default for OnsetConfig {
    fn default() -> Self {
        OnsetConfig {
            window_size: 1024,
            hop_size: 512,
            threshold_multiplier: 1.5,
            min_flux_fraction_of_peak: 0.1,
            min_absolute_flux: 0.01,
            min_interval_seconds: 0.1,
        }
    }
}

/// One novelty value per hop: the half-wave-rectified sum of magnitude
/// increases between consecutive windows' spectra. `samples` is mono.
pub fn spectral_flux(samples: &[f32], config: &OnsetConfig) -> Vec<f64> {
    if samples.len() < config.window_size {
        return Vec::new();
    }
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(config.window_size);

    // Hann window: tapers each frame to zero at its edges so the FFT sees a
    // periodic-looking signal instead of the sharp edge discontinuities a
    // rectangular window would introduce as spurious high-frequency energy.
    //
    // **Periodic**, not symmetric — divides by `window_size`, not
    // `window_size - 1`. That extra `-1` is the textbook Hann formula for
    // *display/FIR-filter* use, but it makes the window itself asymmetric
    // under the DFT's implicit periodicity assumption, so a perfectly
    // steady, bin-aligned tone would then show a spurious, shift-dependent
    // magnitude wobble between hops — a detector artifact, not a real onset,
    // and exactly what spectral flux must not be fooled by. The periodic
    // form is the standard choice for STFT analysis for this reason.
    let hann: Vec<f32> = (0..config.window_size)
        .map(|i| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / config.window_size as f32).cos()))
        .collect();

    let hop_count = (samples.len() - config.window_size) / config.hop_size + 1;
    let mut prev_mags: Option<Vec<f32>> = None;
    let mut flux = Vec::with_capacity(hop_count);

    for h in 0..hop_count {
        let start = h * config.hop_size;
        let mut buf: Vec<Complex32> = samples[start..start + config.window_size]
            .iter()
            .zip(&hann)
            .map(|(&s, &w)| Complex32::new(s * w, 0.0))
            .collect();
        fft.process(&mut buf);

        // Only the first half is independent information for a real input
        // (the second half mirrors it) — no benefit to summing it twice.
        let mags: Vec<f32> = buf[..config.window_size / 2].iter().map(|c| c.norm()).collect();

        let this_flux = match &prev_mags {
            None => 0.0,
            Some(prev) => mags
                .iter()
                .zip(prev)
                .map(|(&m, &p)| (m - p).max(0.0) as f64)
                .sum(),
        };
        flux.push(this_flux);
        prev_mags = Some(mags);
    }
    flux
}

/// Onset times, in seconds, found by adaptively peak-picking `samples`'
/// spectral flux novelty curve.
pub fn detect_onsets(samples: &[f32], sample_rate: u32, config: &OnsetConfig) -> Vec<f64> {
    let flux = spectral_flux(samples, config);
    if flux.len() < 3 {
        return Vec::new();
    }

    let mean = flux.iter().sum::<f64>() / flux.len() as f64;
    let variance = flux.iter().map(|f| (f - mean).powi(2)).sum::<f64>() / flux.len() as f64;
    let peak = flux.iter().cloned().fold(0.0f64, f64::max);
    let threshold = (mean + config.threshold_multiplier * variance.sqrt())
        .max(peak * config.min_flux_fraction_of_peak)
        .max(config.min_absolute_flux);

    let hop_seconds = config.hop_size as f64 / sample_rate as f64;
    let mut onsets: Vec<f64> = Vec::new();
    for i in 1..flux.len() - 1 {
        let is_local_peak = flux[i] > flux[i - 1] && flux[i] >= flux[i + 1];
        if is_local_peak && flux[i] > threshold {
            let t = i as f64 * hop_seconds;
            match onsets.last() {
                Some(&last) if t - last < config.min_interval_seconds => {}
                _ => onsets.push(t),
            }
        }
    }
    onsets
}
