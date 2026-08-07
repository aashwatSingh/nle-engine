//! Render graph shape, per spec 4.5: source frames -> per-clip effect
//! chains -> transform -> track blend -> sequence effects -> output.
//! Compiled once per timeline version and cached; only the per-instant
//! evaluation (resolving keyframes to concrete values) changes per frame.
//!
//! M0 note: this models the graph for a single instant (one output frame).
//! Building it for a rolling playback window, and caching/invalidating it
//! correctly on timeline-version change, is M4 work.

use std::collections::BTreeMap;
use timeline::{ClipInstanceId, EffectInstanceId, ParamValue, SequenceId, TrackId};

#[derive(Debug, Clone)]
pub struct EffectPass {
    pub effect_instance: EffectInstanceId,
    pub type_id: String,
    /// Keyframes already resolved to a concrete value for this tick.
    pub resolved_params: BTreeMap<String, ParamValue>,
}

#[derive(Debug, Clone)]
pub struct ActiveClipPlan {
    pub clip: ClipInstanceId,
    pub source_pts_ticks: i64,
    pub effect_passes: Vec<EffectPass>,
}

#[derive(Debug, Clone)]
pub struct TrackRenderPlan {
    pub track: TrackId,
    /// `None` when no clip on this track covers the requested tick.
    pub active_clip: Option<ActiveClipPlan>,
}

#[derive(Debug, Clone)]
pub struct CompiledFrameGraph {
    pub sequence: SequenceId,
    /// Bottom-to-top track order, matching composite order.
    pub track_plans: Vec<TrackRenderPlan>,
    pub sequence_level_effects: Vec<EffectPass>,
}

/// TODO(M4): implement. Must memoize per timeline-version (spec 4.5
/// "compile and cache this graph; only rebuild when the timeline version
/// changes") — recompiling per frame would defeat the playback performance
/// budget.
pub trait RenderGraphCompiler {
    fn compile(
        &self,
        project: &timeline::Project,
        sequence: SequenceId,
        at: timeline::TimeTick,
    ) -> CompiledFrameGraph;
}
