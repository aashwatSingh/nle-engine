//! Mixer graph, per spec 4.6: clips -> track strips -> submixes -> master.
//! Reuses `timeline::EffectInstance` for insert chains rather than a
//! separate audio effect type — same generic (effect_type, params) shape
//! spec 4.6's effect list (gain, EQ, compressor, limiter, high/low-pass,
//! reverb) all fit. De-noise is deliberately not in that list for v1.0 (see
//! docs/decisions-log.md) — real spectral/ML noise reduction is its own
//! project, not a mixer insert.

use serde::{Deserialize, Serialize};
use timeline::EffectInstance;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackStripId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubmixId(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackStrip {
    pub id: TrackStripId,
    pub gain_db: timeline::ParamTrack,
    pub pan: timeline::ParamTrack,
    pub muted: bool,
    pub solo: bool,
    pub insert_effects: Vec<EffectInstance>,
    pub output: SubmixId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submix {
    pub id: SubmixId,
    pub gain_db: timeline::ParamTrack,
    pub insert_effects: Vec<EffectInstance>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MixerGraph {
    pub track_strips: Vec<TrackStrip>,
    pub submixes: Vec<Submix>,
    pub master_gain_db: timeline::ParamTrack,
    pub master_insert_effects: Vec<EffectInstance>,
}

/// Hard real-time rule (spec 4.6): the actual audio callback that reads from
/// a lock-free ring buffer to hand samples to CPAL must allocate nothing,
/// lock nothing, and block on nothing. This trait describes the *mixing*
/// step that runs ahead of real time on its own thread and fills that ring
/// buffer — not the callback itself. The callback is just a memcpy out of
/// whatever this produces; implementing it is M2/M6 work.
pub trait MixerEngine {
    fn mix_block(&mut self, graph: &MixerGraph, start_sample: u64, frame_count: u32) -> Vec<f32>;
}
