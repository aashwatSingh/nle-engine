//! Immutable ingest-time record of a media file. Per spec section 4.1: probed
//! once on import, never re-probed during playback. Nothing in this module
//! decodes anything — that's the decoder pool (M1), not this type layer.

use serde::{Deserialize, Serialize};

/// Opaque, content-derived identifier. Computed from a content hash at
/// ingest (M1), not a random UUID — this is what lets project persistence
/// "relink media" by hash when a path moves. See docs/project-schema.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MediaAssetId(pub u128);

/// A plain numerator/denominator rate, as probed. Deliberately not the
/// timeline crate's `FrameRate` enum: probing can report anything (e.g. a
/// screen recorder's 47.113 fps average), and classifying that into a named
/// standard rate is a timeline/sequence-level decision, not a media one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rational {
    pub num: u32,
    pub den: u32,
}

/// Per spec 4.1: VFR must be detected explicitly, never silently treated as
/// CFR. `Variable` sources carry a PTS index instead of a single rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FrameRateKind {
    Constant(Rational),
    /// `nominal` is for display only (e.g. "~30fps VFR"); real timing for a
    /// VFR source must come from `VideoStreamInfo::pts_index`, never this.
    Variable { nominal: Rational },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorPrimaries {
    Rec709,
    Rec2020,
    P3D65,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TransferFunction {
    Srgb,
    Bt709,
    Pq,
    Hlg,
    Linear,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatrixCoefficients {
    Bt709,
    Bt2020Ncl,
    Rgb,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorMetadata {
    pub primaries: ColorPrimaries,
    pub transfer: TransferFunction,
    pub matrix: MatrixCoefficients,
    /// Full vs. limited (studio) range.
    pub full_range: bool,
}

/// Pixel format as decoded, before any working-space conversion. Kept small
/// deliberately for M0 — extended as real sources force it in M1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    Yuv420p8,
    Yuv422p10le,
    Yuv420p10le,
    Rgba8,
    Rgba16Float,
}

/// One entry in the ingest-time keyframe index (spec 4.1). Lets a seek be a
/// bounded operation: find the nearest preceding entry, byte-seek there,
/// decode forward to the target PTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyframeIndexEntry {
    pub pts: i64,
    pub byte_offset: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoStreamInfo {
    pub width: u32,
    pub height: u32,
    pub pixel_format: PixelFormat,
    pub color: ColorMetadata,
    pub frame_rate: FrameRateKind,
    /// Source timecode at frame 0, if embedded in the container.
    pub start_timecode: Option<Rational>,
    pub keyframe_index: Vec<KeyframeIndexEntry>,
    /// Non-empty only for `FrameRateKind::Variable` sources: exact PTS per
    /// decoded frame, built at ingest so playback never re-probes.
    pub pts_index: Vec<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub channel_count: u16,
    pub duration_samples: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaAsset {
    pub id: MediaAssetId,
    pub original_absolute_path: String,
    pub content_hash: [u8; 32],
    pub container_format: String,
    pub video: Option<VideoStreamInfo>,
    pub audio: Option<AudioStreamInfo>,
    pub duration_ticks: i64,
}
