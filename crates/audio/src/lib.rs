pub mod beat;
pub mod loudness;
pub mod meter;
pub mod mixer;
pub mod silence;
pub mod timeline_mix;

pub use loudness::{LoudnessAnalyzer, LoudnessMeasurement};
pub use meter::{to_dbfs, PeakRms, SILENCE_DBFS};
pub use mixer::{MixerEngine, MixerGraph, Submix, SubmixId, TrackStrip, TrackStripId};
pub use timeline_mix::{
    db_to_linear, frames_to_ticks, mix_range, pan_gains, total_frames, MixOptions, MixStats,
    CHANNELS,
};

/// Where the mixer gets a clip's samples from.
///
/// A trait rather than a concrete decoder so the mixing maths stays testable
/// with synthetic input (a constant, a ramp, a sine) and no media files at
/// all — which is what let every gain-staging, pan-law, solo/mute and
/// source-offset rule in `timeline_mix` be pinned down by a unit test rather
/// than inferred from listening to an export.
///
/// `audio` deliberately does not depend on `media_ffmpeg`; the real
/// implementation lives next to the code that already owns decoders.
pub trait SampleSource {
    /// Interleaved stereo f32 for `frames` frames starting at `start_frame`
    /// (a frame offset into the *source*, at `sample_rate`).
    ///
    /// Return an empty vec when the asset can't be read — the caller counts
    /// that as a missing source rather than treating it as silence. A short
    /// (but non-empty) return is fine and means end of stream; the mixer
    /// only consumes what's there.
    fn samples_at(
        &mut self,
        asset: media::MediaAssetId,
        start_frame: i64,
        frames: usize,
        sample_rate: u32,
    ) -> Vec<f32>;
}
