//! The one frame type everything downstream of decode speaks (spec 4.1).
//! Compositor, effects, and export all consume `Frame` — never a raw decoder
//! output directly.

use crate::asset::{ColorMetadata, MediaAssetId};

/// TODO(M1/M2): placeholder for a real GPU texture handle. Once the render
/// crate's wgpu integration lands, this becomes a thin wrapper around
/// `wgpu::Texture` (or an index into a texture pool) instead of a bare u64.
/// Kept here rather than in `render` so `media` doesn't depend on `render` —
/// see docs/architecture.md "crate dependency direction".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuTextureHandle(pub u64);

#[derive(Debug, Clone, Copy)]
pub struct Frame {
    pub texture: GpuTextureHandle,
    pub color: ColorMetadata,
    pub pts_ticks: i64,
    pub source: MediaAssetId,
}
