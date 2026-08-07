//! Decoder pool interface (spec 4.1). No implementation in M0 — this is the
//! seam M1 fills in with the real FFmpeg-FFI-backed pool.

use crate::asset::MediaAssetId;
use crate::frame::Frame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeRequest {
    pub asset: MediaAssetId,
    pub pts_ticks: i64,
}

#[derive(Debug)]
pub enum DecodeError {
    AssetNotFound(MediaAssetId),
    SeekFailed { asset: MediaAssetId, pts_ticks: i64 },
    /// The underlying decoder (FFmpeg) rejected the data. Must never panic
    /// or crash the process — this is the error path the fuzzing suite (spec
    /// section 8) is required to exercise.
    Corrupt { asset: MediaAssetId, detail: String },
}

/// A bounded pool of decoder instances keyed by asset, reused across
/// requests, evicted least-recently-used under a memory ceiling. The trait
/// boundary here is deliberately narrow so FFmpeg can be swapped for a
/// platform hardware decoder per spec 4.1 "Decoder pool".
pub trait DecoderPool: Send + Sync {
    fn request(&self, req: DecodeRequest) -> Result<(), DecodeError>;
    fn poll_ready(&self) -> Vec<Frame>;
    /// Bumped on every seek; in-flight results tagged with a stale
    /// generation are discarded rather than delivered. See the "seek during
    /// playback" failure mode in docs/architecture.md.
    fn generation(&self) -> u64;
    fn cancel_all(&self);
}
