//! Render graph, per spec 4.5: source frames -> per-clip effect chains ->
//! transform -> track blending -> sequence-level effects -> output.
//!
//! This module is the *pure* half of compositing: given a `Project` and a
//! tick, it produces a fully-resolved `CompiledFrameGraph` describing what
//! to draw, with every keyframed parameter already evaluated to a concrete
//! value. It touches no GPU state and has no wgpu dependency, so all of it
//! is testable headlessly. `compositor.rs` executes what this produces.
//!
//! Two conventions this module fixes, both of which are real decisions
//! rather than incidental:
//!
//! 1. **Track order is bottom-to-top.** `Sequence::tracks[0]` is the
//!    bottom-most layer; later tracks composite over earlier ones. The
//!    emitted `track_plans` preserve that order.
//! 2. **Effect keyframes are clip-relative.** A `ParamTrack` on a clip's
//!    effect is evaluated at `tick - clip.timeline_in`, not at the absolute
//!    sequence tick. This is what makes a clip carry its own animation when
//!    it's moved or rippled — the alternative (absolute ticks) would mean
//!    every ripple edit silently re-times every animation downstream of it.

use crate::effect::{mask, transform, EffectRegistry};
use std::collections::HashMap;
use std::sync::Arc;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, EffectInstanceId, ParamValue, Project, Sequence,
    SequenceId, TimeTick, TrackId, TrackKind, TransitionKind,
};

/// A resolved 2D transform, folded from a clip's `transform` effect
/// instances (or defaulted when it has none).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform2D {
    /// Pixels, relative to the sequence centre.
    pub position: (f32, f32),
    pub scale: (f32, f32),
    /// Degrees, clockwise.
    pub rotation_degrees: f32,
    /// Normalised 0..1 within the source frame.
    pub anchor: (f32, f32),
    pub opacity: f32,
}

impl Default for Transform2D {
    fn default() -> Self {
        Transform2D {
            position: (0.0, 0.0),
            scale: (1.0, 1.0),
            rotation_degrees: 0.0,
            anchor: (0.5, 0.5),
            opacity: 1.0,
        }
    }
}

/// One effect to execute, with its keyframes already resolved for this tick.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectPass {
    pub effect_instance: EffectInstanceId,
    pub type_id: String,
    pub resolved_params: HashMap<String, ParamValue>,
}

impl EffectPass {
    pub fn number(&self, name: &str) -> Option<f64> {
        match self.resolved_params.get(name) {
            Some(ParamValue::Number(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn vec2(&self, name: &str) -> Option<(f64, f64)> {
        match self.resolved_params.get(name) {
            Some(ParamValue::Vec2(x, y)) => Some((*x, *y)),
            _ => None,
        }
    }

    pub fn bool_param(&self, name: &str) -> Option<bool> {
        match self.resolved_params.get(name) {
            Some(ParamValue::Bool(v)) => Some(*v),
            _ => None,
        }
    }

    pub fn color(&self, name: &str) -> Option<[f32; 4]> {
        match self.resolved_params.get(name) {
            Some(ParamValue::Color(v)) => Some(*v),
            _ => None,
        }
    }
}

/// A resolved mask shape, folded from a clip's `mask` effect instance (at
/// most one is meaningful — see `mask` module doc comment). `enabled` is
/// `false` when the clip has no mask, which the compositor treats as "no
/// masking, alpha unmodified."
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MaskShape {
    pub enabled: bool,
    pub is_rectangle: bool,
    /// Normalised 0..1 within the source frame.
    pub center: (f32, f32),
    /// Normalised 0..1 half-extents (rectangle) or radii (ellipse).
    pub size: (f32, f32),
    /// Normalised 0..1 fraction of the frame diagonal.
    pub feather: f32,
    pub invert: bool,
}

impl Default for MaskShape {
    fn default() -> Self {
        MaskShape {
            enabled: false,
            is_rectangle: false,
            center: (0.5, 0.5),
            size: (0.25, 0.25),
            feather: 0.0,
            invert: false,
        }
    }
}

/// Where a track's pixels come from for this frame.
#[derive(Debug, Clone, PartialEq)]
pub enum FrameSource {
    /// A decoded media frame. The compositor's caller is responsible for
    /// supplying a texture for this (asset, pts) pair — the graph names
    /// what it needs but never decodes anything itself.
    Media { asset: media::MediaAssetId, source_pts_ticks: i64 },
    /// A nested sequence, compiled at the corresponding inner tick. The
    /// compositor renders this to an intermediate texture and then treats
    /// it as this clip's source.
    Nested(Box<CompiledFrameGraph>),
    /// A text title, rasterised by the compositor at sequence resolution.
    ///
    /// The spec travels in the graph by value rather than as a reference into
    /// the project because a `CompiledFrameGraph` outlives the borrow it was
    /// compiled from — the playback thread holds one while the UI thread is
    /// free to keep editing. Titles are small; cloning one per frame is not
    /// the cost worth optimising here.
    Title(timeline::TitleSpec),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ActiveClipPlan {
    pub clip: ClipInstanceId,
    pub source: FrameSource,
    /// Folded from this clip's `transform` effect instances.
    pub transform: Transform2D,
    /// Every effect on the clip, resolved — including the `transform`
    /// instances already folded into `transform` above, so a future
    /// compositor that wants to run transform as a real shader pass has
    /// what it needs without re-resolving. Effects whose `type_id` isn't in
    /// the registry are dropped here (with a count in
    /// `CompiledFrameGraph::unknown_effects`) rather than silently ignored
    /// deeper down.
    ///
    /// Includes `transform` and `mask` instances (already folded into
    /// `transform`/`mask` below) as well as pixel effects (blur, color
    /// correction, crop) in stack order — the compositor iterates this list,
    /// skipping the ones it applies by folding, to build the effect chain.
    pub effect_passes: Vec<EffectPass>,
    /// Folded from this clip's `mask` effect instance, if any.
    pub mask: MaskShape,
}

/// A transition in progress on a track at the compiled tick.
///
/// The compositor draws `outgoing` first at full opacity, then the track's
/// `active_clip` at `progress` opacity over it. That ordering is what makes a
/// cross dissolve come out right with no dedicated shader: alpha-over gives
/// `B*p + A*(1-p)` with alpha 1. Drawing *both* layers at partial opacity —
/// the obvious-looking alternative — yields `B*p + A*(1-p)^2` and visibly dips
/// in the middle of every dissolve.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRender {
    pub kind: TransitionKind,
    /// 0.0 = entirely `outgoing`, 1.0 = entirely `active_clip`.
    pub progress: f32,
    /// The clip being transitioned away from, sampled past its own out point
    /// (into its handles). `None` when the cut has nothing before it, which
    /// makes the transition a fade in.
    pub outgoing: Option<ActiveClipPlan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrackRenderPlan {
    pub track: TrackId,
    /// The incoming/primary layer. `None` when no clip on this track covers the
    /// requested tick — the track contributes nothing and the compositor skips
    /// it. During a transition's first half this is the clip *after* the cut,
    /// sampled before its own in point.
    pub active_clip: Option<ActiveClipPlan>,
    /// Set only while a transition covers this tick.
    pub transition: Option<TransitionRender>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompiledFrameGraph {
    pub sequence: SequenceId,
    pub width: u32,
    pub height: u32,
    /// Bottom-to-top composite order.
    pub track_plans: Vec<TrackRenderPlan>,
    pub sequence_level_effects: Vec<EffectPass>,
    /// Count of effect instances whose `type_id` the registry didn't
    /// recognise. Surfaced rather than swallowed so a UI can warn "this
    /// project uses effects this build doesn't have" instead of rendering
    /// subtly wrong output and saying nothing.
    pub unknown_effects: usize,
}

/// One decoded frame a graph needs before it can be composited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRequest {
    pub asset: media::MediaAssetId,
    pub source_pts_ticks: i64,
    /// Which clip wants it.
    ///
    /// Carried because two clips can reference the **same asset at different
    /// source times** — a dissolve between two moments of one interview, say.
    /// A decoder cache keyed only by asset would then be yanked back and forth
    /// between the two positions on every single frame, turning a cheap
    /// forward walk into two seeks per frame. Keying by clip gives each layer
    /// its own decoder, which is the whole reason `SourceReader` is fast.
    pub clip: ClipInstanceId,
}

impl CompiledFrameGraph {
    /// Every frame this graph needs decoded, including transitions' outgoing
    /// layers, recursing into nested sequences.
    ///
    /// Exists so the three places that feed the compositor — the editor's
    /// scrub path, the playback decode thread, and export — can't disagree
    /// about what a frame requires. Each of them previously walked
    /// `track_plans` and read `active_clip` directly, which meant adding the
    /// transition layer would have silently rendered a dissolve with a missing
    /// input in whichever call site was overlooked.
    pub fn media_requests(&self) -> Vec<MediaRequest> {
        let mut out = Vec::new();
        self.collect_media_requests(&mut out);
        out
    }

    fn collect_media_requests(&self, out: &mut Vec<MediaRequest>) {
        for plan in &self.track_plans {
            let layers = plan
                .active_clip
                .iter()
                .chain(plan.transition.as_ref().and_then(|t| t.outgoing.as_ref()));
            for layer in layers {
                match &layer.source {
                    FrameSource::Media { asset, source_pts_ticks } => out.push(MediaRequest {
                        asset: *asset,
                        source_pts_ticks: *source_pts_ticks,
                        clip: layer.clip,
                    }),
                    FrameSource::Nested(inner) => inner.collect_media_requests(out),
                    // Titles need no decode — the compositor rasterises them
                    // from the spec already in the graph — so they contribute
                    // no request. A title is therefore never counted as a
                    // missing source, which is the point: it isn't missing.
                    FrameSource::Title(_) => {}
                }
            }
        }
    }
}

/// Guard against a sequence that (directly or transitively) nests itself.
/// Cycle detection below makes this unreachable for true cycles; the depth
/// cap is a second, cheaper backstop against pathological-but-acyclic
/// nesting depth.
const MAX_NEST_DEPTH: usize = 16;

pub struct GraphCompiler<R: EffectRegistry> {
    registry: R,
}

impl<R: EffectRegistry> GraphCompiler<R> {
    pub fn new(registry: R) -> Self {
        GraphCompiler { registry }
    }

    /// Compiles the frame graph for `sequence` at `at`. Returns `None` if
    /// the sequence doesn't exist.
    pub fn compile(
        &self,
        project: &Project,
        sequence: SequenceId,
        at: TimeTick,
    ) -> Option<CompiledFrameGraph> {
        // Seeded with the root: `ancestors` is the chain of sequences
        // currently being compiled *including* the current one, so a clip
        // referencing the very sequence it lives in is caught immediately
        // rather than one level down (where it would otherwise produce a
        // pointless empty nested graph instead of nothing).
        let mut ancestors = vec![sequence];
        self.compile_inner(project, sequence, at, &mut ancestors)
    }

    /// Plans one clip as it would look at sequence tick `at`.
    ///
    /// `at` is deliberately **not** required to fall inside the clip: a
    /// transition samples both of its clips outside their own trim points, so
    /// `source_pts` can land before `source_in` or after `source_out`. Both are
    /// legitimate reads into the clip's handles, and the decoder clamps to the
    /// file's real extent if the handles aren't there.
    fn plan_clip(
        &self,
        project: &Project,
        clip: &ClipInstance,
        at: TimeTick,
        ancestors: &mut Vec<SequenceId>,
        unknown_effects: &mut usize,
    ) -> Option<ActiveClipPlan> {
        let into_clip = at.0 - clip.timeline_in.0;
        let source = match &clip.source {
            &ClipSource::Media(asset) => {
                let source_pts = clip.source_in.0 + clip.speed.source_delta(into_clip);
                Some(FrameSource::Media { asset, source_pts_ticks: source_pts })
            }
            // A title has no source media and no time axis of its own — it
            // looks the same at every tick of the clip — so `into_clip` and
            // the speed curve simply don't apply to it. Animating a title is
            // what keyframed effects on the clip are for.
            ClipSource::Title(spec) => Some(FrameSource::Title(spec.clone())),
            &ClipSource::NestedSequence(inner_id) => {
                if ancestors.contains(&inner_id) || ancestors.len() >= MAX_NEST_DEPTH {
                    // A sequence nesting itself: skip rather than recurse
                    // forever. The timeline model doesn't currently prevent
                    // constructing this, so the renderer has to survive it.
                    None
                } else {
                    let inner_tick =
                        TimeTick(clip.source_in.0 + clip.speed.source_delta(into_clip));
                    ancestors.push(inner_id);
                    let nested = self
                        .compile_inner(project, inner_id, inner_tick, ancestors)
                        .map(|g| FrameSource::Nested(Box::new(g)));
                    ancestors.pop();
                    nested
                }
            }
        };

        source.map(|source| {
            let (transform, mask, passes, unknown) = self.resolve_effects(clip, at);
            *unknown_effects += unknown;
            ActiveClipPlan { clip: clip.id, source, transform, effect_passes: passes, mask }
        })
    }

    fn compile_inner(
        &self,
        project: &Project,
        sequence_id: SequenceId,
        at: TimeTick,
        ancestors: &mut Vec<SequenceId>,
    ) -> Option<CompiledFrameGraph> {
        let seq: &Sequence = project.sequences.iter().find(|s| s.id == sequence_id)?;
        let mut unknown_effects = 0usize;

        // Solo, if any video track is soloed, restricts rendering to the
        // soloed tracks — matching how solo behaves on an audio mixer.
        let any_solo = seq.tracks.iter().any(|t| t.kind == TrackKind::Video && t.solo);

        let mut track_plans = Vec::new();
        for track in seq.tracks.iter().filter(|t| t.kind == TrackKind::Video) {
            let audible = if any_solo { track.solo } else { !track.muted };
            if !audible {
                track_plans.push(TrackRenderPlan { track: track.id, active_clip: None, transition: None });
                continue;
            }

            // A transition replaces the normal "one clip covers this tick"
            // lookup with the explicit pair its cut joins. During the first
            // half the incoming clip doesn't contain `at` at all — sampling it
            // early, into its handles, is exactly what a transition is.
            let (plan, transition) = match track.transition_at(at) {
                Some(tr) => {
                    let (left, right) = track.clips_at_cut(tr.at);
                    let incoming = right
                        .and_then(|c| self.plan_clip(project, c, at, ancestors, &mut unknown_effects));
                    let outgoing = left
                        .and_then(|c| self.plan_clip(project, c, at, ancestors, &mut unknown_effects));
                    (
                        incoming,
                        Some(TransitionRender {
                            kind: tr.kind,
                            progress: tr.progress_at(at),
                            outgoing,
                        }),
                    )
                }
                None => {
                    let active = track
                        .clips
                        .iter()
                        .find(|c| c.timeline_in <= at && at < c.timeline_out);
                    let plan = active
                        .and_then(|c| self.plan_clip(project, c, at, ancestors, &mut unknown_effects));
                    (plan, None)
                }
            };
            track_plans.push(TrackRenderPlan { track: track.id, active_clip: plan, transition });
        }

        Some(CompiledFrameGraph {
            sequence: sequence_id,
            width: seq.settings.width,
            height: seq.settings.height,
            track_plans,
            // Sequence-level (adjustment-layer style) effects aren't part of
            // the timeline model yet — `Sequence` has no effect stack field.
            // Kept in the graph shape because spec 4.5 calls for them, so
            // adding them later is a model change plus a fold here, not a
            // graph-shape change.
            sequence_level_effects: Vec::new(),
            unknown_effects,
        })
    }

    /// Resolves a clip's effect stack at `at`, folding every `transform`
    /// instance into a single `Transform2D`. Multiple transform instances
    /// compose: positions add, scales multiply, rotations add. Anchor takes
    /// the last one set (averaging pivots would be meaningless).
    fn resolve_effects(
        &self,
        clip: &ClipInstance,
        at: TimeTick,
    ) -> (Transform2D, MaskShape, Vec<EffectPass>, usize) {
        // Clip-relative — see the module doc comment.
        let local = TimeTick(at.0 - clip.timeline_in.0);
        let mut transform = Transform2D::default();
        let mut mask_shape = MaskShape::default();
        let mut passes = Vec::new();
        let mut unknown = 0usize;

        for instance in clip.effects.iter().filter(|e| e.enabled) {
            if self.registry.lookup(&instance.effect_type).is_none() {
                unknown += 1;
                continue;
            }
            let resolved: HashMap<String, ParamValue> = instance
                .params
                .iter()
                .map(|(name, track)| (name.clone(), track.evaluate_at(local)))
                .collect();
            let pass = EffectPass {
                effect_instance: instance.id,
                type_id: instance.effect_type.clone(),
                resolved_params: resolved,
            };

            if pass.type_id == transform::TYPE_ID {
                if let Some((x, y)) = pass.vec2(transform::POSITION) {
                    transform.position.0 += x as f32;
                    transform.position.1 += y as f32;
                }
                if let Some((x, y)) = pass.vec2(transform::SCALE) {
                    transform.scale.0 *= x as f32;
                    transform.scale.1 *= y as f32;
                }
                if let Some(r) = pass.number(transform::ROTATION) {
                    transform.rotation_degrees += r as f32;
                }
                if let Some((x, y)) = pass.vec2(transform::ANCHOR) {
                    transform.anchor = (x as f32, y as f32);
                }
                if let Some(o) = pass.number(transform::OPACITY) {
                    transform.opacity *= o as f32;
                }
            } else if pass.type_id == mask::TYPE_ID {
                mask_shape.enabled = true;
                if let Some(v) = pass.bool_param(mask::IS_RECTANGLE) {
                    mask_shape.is_rectangle = v;
                }
                if let Some((x, y)) = pass.vec2(mask::CENTER) {
                    mask_shape.center = (x as f32, y as f32);
                }
                if let Some((x, y)) = pass.vec2(mask::SIZE) {
                    mask_shape.size = (x as f32, y as f32);
                }
                if let Some(f) = pass.number(mask::FEATHER) {
                    mask_shape.feather = f as f32;
                }
                if let Some(v) = pass.bool_param(mask::INVERT) {
                    mask_shape.invert = v;
                }
            }
            passes.push(pass);
        }
        (transform, mask_shape, passes, unknown)
    }
}

/// Memoises compiled graphs, keyed by `(sequence, tick)`, and invalidated
/// wholesale whenever the `Arc<Project>` identity changes — which is
/// exactly the right version signal given the persistent-timeline design
/// (every edit produces a new `Arc`, so pointer identity *is* the version).
///
/// Known limitation, stated rather than hidden: because the graph bakes
/// resolved parameter values, entries are per-tick, so sustained playback
/// grows the cache one entry per frame until `max_entries` clears it. The
/// better design splits "structure" (per version) from "resolved params"
/// (per tick) and only memoises the former. That's a worthwhile
/// optimisation to make against a profile, not on speculation — spec 4.5
/// only requires that the graph not be rebuilt from scratch when the
/// timeline hasn't changed, which this satisfies.
pub struct GraphCache {
    project: Option<Arc<Project>>,
    entries: HashMap<(SequenceId, i64), Arc<CompiledFrameGraph>>,
    max_entries: usize,
}

impl GraphCache {
    pub const DEFAULT_MAX_ENTRIES: usize = 512;

    pub fn new(max_entries: usize) -> Self {
        GraphCache { project: None, entries: HashMap::new(), max_entries }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get_or_compile<R: EffectRegistry>(
        &mut self,
        compiler: &GraphCompiler<R>,
        project: &Arc<Project>,
        sequence: SequenceId,
        at: TimeTick,
    ) -> Option<Arc<CompiledFrameGraph>> {
        let same_version = self
            .project
            .as_ref()
            .map(|p| Arc::ptr_eq(p, project))
            .unwrap_or(false);
        if !same_version {
            self.entries.clear();
            self.project = Some(project.clone());
        }
        let key = (sequence, at.0);
        if let Some(hit) = self.entries.get(&key) {
            return Some(hit.clone());
        }
        let compiled = Arc::new(compiler.compile(project, sequence, at)?);
        if self.entries.len() >= self.max_entries {
            self.entries.clear();
        }
        self.entries.insert(key, compiled.clone());
        Some(compiled)
    }
}

impl Default for GraphCache {
    fn default() -> Self {
        GraphCache::new(GraphCache::DEFAULT_MAX_ENTRIES)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::effect::BuiltinRegistry;
    use media::{ColorPrimaries, MediaAssetId};
    use std::collections::BTreeMap;
    use timeline::{
        Transition,
        EffectInstance, FrameRate, InterpolationMode, Keyframe, ParamTrack, SequenceSettings,
        SpeedCurve, Track,
    };

    fn compiler() -> GraphCompiler<BuiltinRegistry> {
        GraphCompiler::new(BuiltinRegistry::default())
    }

    fn clip(id: u64, tin: i64, tout: i64) -> ClipInstance {
        ClipInstance {
            id: ClipInstanceId(id),
            source: ClipSource::Media(MediaAssetId(7)),
            source_in: TimeTick(0),
            source_out: TimeTick(tout - tin),
            timeline_in: TimeTick(tin),
            timeline_out: TimeTick(tout),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }
    }

    fn video_track(id: u64, clips: Vec<ClipInstance>) -> Track {
        Track {
            id: TrackId(id),
            kind: TrackKind::Video,
            name: format!("V{id}"),
            clips,
            transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        }
    }

    /// Two abutting clips cut at `cut`, with a transition of `duration`
    /// centred on it. Both clips have handles: `source_out` extends past what
    /// the clip uses, so the transition can read into them.
    fn track_with_transition(kind: TransitionKind, cut: i64, duration: i64) -> Track {
        let mut left = clip(1, 0, cut);
        left.source_out = TimeTick(cut * 2); // plenty of tail handle
        let mut right = clip(2, cut, cut * 2);
        right.source_in = TimeTick(cut); // head handle available before its in point
        right.source_out = TimeTick(cut * 2);
        let mut track = video_track(1, vec![left, right]);
        track.transitions = vec![Transition {
            id: timeline::TransitionId(1),
            kind,
            at: TimeTick(cut),
            duration: TimeTick(duration),
        }];
        track
    }

    #[test]
    fn a_transition_makes_both_clips_active_at_once() {
        // The defining property: outside a transition a track has one layer,
        // inside it has two. Everything else about dissolves depends on this.
        let track = track_with_transition(TransitionKind::CrossDissolve, 1000, 400);
        let p = Project {
            sequences: vec![sequence(1, vec![track])],
            assets: vec![],
            bins: vec![],
        };

        // Well before the transition: one layer, no transition.
        let g = compiler().compile(&p, SequenceId(1), TimeTick(500)).unwrap();
        assert!(g.track_plans[0].transition.is_none());
        assert_eq!(g.media_requests().len(), 1);

        // Inside it (region is 800..1200): two layers.
        let g = compiler().compile(&p, SequenceId(1), TimeTick(1000)).unwrap();
        let tr = g.track_plans[0].transition.as_ref().expect("transition should be active");
        assert!(tr.outgoing.is_some(), "the outgoing clip must be planned too");
        assert_eq!(
            g.media_requests().len(),
            2,
            "a dissolve needs two decoded frames, not one"
        );
    }

    #[test]
    fn transition_progress_runs_zero_to_one_across_its_region() {
        let track = track_with_transition(TransitionKind::CrossDissolve, 1000, 400);
        let p = Project { sequences: vec![sequence(1, vec![track])], assets: vec![], bins: vec![] };
        let progress_at = |t: i64| {
            compiler()
                .compile(&p, SequenceId(1), TimeTick(t))
                .unwrap()
                .track_plans[0]
                .transition
                .as_ref()
                .map(|tr| tr.progress)
        };
        // Region 800..1200, centred on the cut at 1000.
        assert_eq!(progress_at(800), Some(0.0), "starts fully on the outgoing clip");
        assert_eq!(progress_at(1000), Some(0.5), "half way at the cut itself");
        assert!(progress_at(1199).unwrap() > 0.99, "ends fully on the incoming clip");
        assert_eq!(progress_at(1200), None, "and is over at the region's end");
    }

    #[test]
    fn clips_are_sampled_into_their_handles_during_a_transition() {
        // What makes a transition possible at all: the outgoing clip is read
        // *past* its out point and the incoming one *before* its in point. If
        // the compiler clamped either to the clip's own bounds, both layers
        // would show a frozen frame for half the transition.
        let track = track_with_transition(TransitionKind::CrossDissolve, 1000, 400);
        let p = Project { sequences: vec![sequence(1, vec![track])], assets: vec![], bins: vec![] };

        // At tick 900: 100 before the cut, so the incoming clip (in at 1000,
        // source_in 1000) must be read at source 900 — before its in point.
        let g = compiler().compile(&p, SequenceId(1), TimeTick(900)).unwrap();
        let plan = &g.track_plans[0];
        let incoming = plan.active_clip.as_ref().unwrap();
        assert_eq!(
            incoming.source,
            FrameSource::Media { asset: MediaAssetId(7), source_pts_ticks: 900 },
            "incoming clip should be read 100 ticks before its in point"
        );

        // At tick 1100: 100 past the cut, so the outgoing clip (out at 1000)
        // must be read at source 1100 — past its out point.
        let g = compiler().compile(&p, SequenceId(1), TimeTick(1100)).unwrap();
        let outgoing = g.track_plans[0].transition.as_ref().unwrap().outgoing.as_ref().unwrap();
        assert_eq!(
            outgoing.source,
            FrameSource::Media { asset: MediaAssetId(7), source_pts_ticks: 1100 },
            "outgoing clip should be read 100 ticks past its out point"
        );
    }

    #[test]
    fn media_requests_distinguish_two_clips_sharing_one_asset() {
        // Two clips off one file, taken from moments far apart — the ordinary
        // "dissolve between two takes in the same interview" case. The requests
        // must carry their clip ids, or a decoder cache keyed by asset alone
        // would be yanked between the two positions every frame.
        let mut track = track_with_transition(TransitionKind::CrossDissolve, 1000, 400);
        track.clips[1].source_in = TimeTick(50_000);
        track.clips[1].source_out = TimeTick(51_000);
        let p = Project { sequences: vec![sequence(1, vec![track])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(900)).unwrap();

        let reqs = g.media_requests();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|r| r.asset == MediaAssetId(7)));
        assert_ne!(reqs[0].clip, reqs[1].clip, "requests must be distinguishable by clip");
        assert_ne!(
            reqs[0].source_pts_ticks, reqs[1].source_pts_ticks,
            "the two layers are at different source times — that's the whole point"
        );
    }

    #[test]
    fn a_transition_at_a_clips_head_has_no_outgoing_layer() {
        // A dissolve on the very first cut of a sequence is a fade in. It must
        // compile, with nothing to dissolve *from*, rather than being rejected.
        let mut track = video_track(1, vec![clip(1, 0, 1000)]);
        track.transitions = vec![Transition {
            id: timeline::TransitionId(1),
            kind: TransitionKind::CrossDissolve,
            at: TimeTick(0),
            duration: TimeTick(200),
        }];
        let p = Project { sequences: vec![sequence(1, vec![track])], assets: vec![], bins: vec![] };

        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let tr = g.track_plans[0].transition.as_ref().unwrap();
        assert!(tr.outgoing.is_none(), "nothing precedes the sequence start");
        assert!(g.track_plans[0].active_clip.is_some(), "but the incoming clip is there");
    }

    fn sequence(id: u64, tracks: Vec<Track>) -> Sequence {
        Sequence {
            id: SequenceId(id),
            name: format!("S{id}"),
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: 640,
                height: 360,
                sample_rate: 48000,
                working_color_primaries: ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }
    }

    fn transform_effect(params: Vec<(&str, ParamTrack)>) -> EffectInstance {
        let mut map = BTreeMap::new();
        for (name, track) in params {
            map.insert(name.to_string(), track);
        }
        EffectInstance {
            id: EffectInstanceId(1),
            effect_type: transform::TYPE_ID.to_string(),
            enabled: true,
            params: map,
        }
    }

    fn generic_effect(type_id: &str, params: Vec<(&str, ParamTrack)>) -> EffectInstance {
        let mut map = BTreeMap::new();
        for (name, track) in params {
            map.insert(name.to_string(), track);
        }
        EffectInstance { id: EffectInstanceId(2), effect_type: type_id.to_string(), enabled: true, params: map }
    }

    #[test]
    fn clip_with_no_mask_effect_has_mask_disabled() {
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 100)])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert!(!g.track_plans[0].active_clip.as_ref().unwrap().mask.enabled);
    }

    #[test]
    fn mask_effect_params_are_resolved() {
        use crate::effect::mask;
        let mut c = clip(1, 0, 100);
        c.effects = vec![generic_effect(
            mask::TYPE_ID,
            vec![
                (mask::IS_RECTANGLE, ParamTrack::constant(ParamValue::Bool(true))),
                (mask::CENTER, ParamTrack::constant(ParamValue::Vec2(0.3, 0.7))),
                (mask::SIZE, ParamTrack::constant(ParamValue::Vec2(0.1, 0.2))),
                (mask::FEATHER, ParamTrack::constant(ParamValue::Number(0.05))),
                (mask::INVERT, ParamTrack::constant(ParamValue::Bool(true))),
            ],
        )];
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let m = g.track_plans[0].active_clip.as_ref().unwrap().mask;
        assert!(m.enabled);
        assert!(m.is_rectangle);
        assert_eq!(m.center, (0.3, 0.7));
        assert_eq!(m.size, (0.1, 0.2));
        assert_eq!(m.feather, 0.05);
        assert!(m.invert);
    }

    #[test]
    fn mask_and_transform_fold_independently() {
        use crate::effect::mask;
        let mut c = clip(1, 0, 100);
        c.effects = vec![
            transform_effect(vec![(transform::OPACITY, ParamTrack::constant(ParamValue::Number(0.4)))]),
            generic_effect(mask::TYPE_ID, vec![(mask::IS_RECTANGLE, ParamTrack::constant(ParamValue::Bool(true)))]),
        ];
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let active = g.track_plans[0].active_clip.as_ref().unwrap();
        assert_eq!(active.transform.opacity, 0.4, "transform folding must be unaffected by a mask being present");
        assert!(active.mask.enabled);
        assert!(active.mask.is_rectangle);
        assert_eq!(active.effect_passes.len(), 2, "both effects should still appear in effect_passes");
    }

    #[test]
    fn empty_track_yields_no_active_clip() {
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(0)).unwrap();
        assert_eq!(g.track_plans.len(), 1);
        assert!(g.track_plans[0].active_clip.is_none());
    }

    #[test]
    fn picks_the_clip_covering_the_tick_and_maps_source_pts() {
        let mut c = clip(1, 100, 200);
        c.source_in = TimeTick(1000);
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(150)).unwrap();
        let active = g.track_plans[0].active_clip.as_ref().unwrap();
        assert_eq!(
            active.source,
            FrameSource::Media { asset: MediaAssetId(7), source_pts_ticks: 1050 },
            "50 ticks into the clip at 1x should read source_in + 50"
        );
    }

    #[test]
    fn clip_boundaries_are_half_open() {
        let p = Project {
            sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 100), clip(2, 100, 200)])])],
            assets: vec![],
            bins: vec![],
        };
        let c = compiler();
        let at_99 = c.compile(&p, SequenceId(1), TimeTick(99)).unwrap();
        let at_100 = c.compile(&p, SequenceId(1), TimeTick(100)).unwrap();
        assert_eq!(at_99.track_plans[0].active_clip.as_ref().unwrap().clip, ClipInstanceId(1));
        assert_eq!(at_100.track_plans[0].active_clip.as_ref().unwrap().clip, ClipInstanceId(2));
    }

    #[test]
    fn speed_scales_the_source_pts_mapping() {
        let mut c = clip(1, 0, 100);
        c.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 }; // 2x
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        match g.track_plans[0].active_clip.as_ref().unwrap().source {
            FrameSource::Media { source_pts_ticks, .. } => {
                assert_eq!(source_pts_ticks, 100, "at 2x, 50 timeline ticks = 100 source ticks");
            }
            ref other => panic!("expected Media, got {other:?}"),
        }
    }

    #[test]
    fn tracks_are_reported_bottom_to_top() {
        let p = Project {
            sequences: vec![sequence(
                1,
                vec![video_track(1, vec![clip(1, 0, 100)]), video_track(2, vec![clip(2, 0, 100)])],
            )],
            assets: vec![],
            bins: vec![],
        };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert_eq!(g.track_plans[0].track, TrackId(1), "tracks[0] is the bottom layer");
        assert_eq!(g.track_plans[1].track, TrackId(2));
    }

    #[test]
    fn audio_tracks_are_excluded_from_the_video_graph() {
        let mut audio = video_track(2, vec![clip(2, 0, 100)]);
        audio.kind = TrackKind::Audio;
        let p = Project {
            sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 100)]), audio])],
            assets: vec![],
            bins: vec![],
        };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert_eq!(g.track_plans.len(), 1);
        assert_eq!(g.track_plans[0].track, TrackId(1));
    }

    #[test]
    fn muted_track_contributes_nothing() {
        let mut t = video_track(1, vec![clip(1, 0, 100)]);
        t.muted = true;
        let p = Project { sequences: vec![sequence(1, vec![t])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert!(g.track_plans[0].active_clip.is_none());
    }

    #[test]
    fn solo_restricts_rendering_to_soloed_tracks() {
        let mut bottom = video_track(1, vec![clip(1, 0, 100)]);
        let mut top = video_track(2, vec![clip(2, 0, 100)]);
        top.solo = true;
        bottom.solo = false;
        let p = Project { sequences: vec![sequence(1, vec![bottom, top])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert!(g.track_plans[0].active_clip.is_none(), "non-soloed track suppressed");
        assert!(g.track_plans[1].active_clip.is_some(), "soloed track renders");
    }

    #[test]
    fn transform_defaults_when_the_clip_has_no_effects() {
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 100)])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let t = g.track_plans[0].active_clip.as_ref().unwrap().transform;
        assert_eq!(t, Transform2D::default());
    }

    #[test]
    fn transform_effect_params_are_resolved() {
        let mut c = clip(1, 0, 100);
        c.effects = vec![transform_effect(vec![
            (transform::POSITION, ParamTrack::constant(ParamValue::Vec2(10.0, -20.0))),
            (transform::SCALE, ParamTrack::constant(ParamValue::Vec2(2.0, 3.0))),
            (transform::ROTATION, ParamTrack::constant(ParamValue::Number(45.0))),
            (transform::OPACITY, ParamTrack::constant(ParamValue::Number(0.5))),
        ])];
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let t = g.track_plans[0].active_clip.as_ref().unwrap().transform;
        assert_eq!(t.position, (10.0, -20.0));
        assert_eq!(t.scale, (2.0, 3.0));
        assert_eq!(t.rotation_degrees, 45.0);
        assert_eq!(t.opacity, 0.5);
    }

    #[test]
    fn disabled_effects_are_skipped() {
        let mut c = clip(1, 0, 100);
        let mut fx = transform_effect(vec![(transform::OPACITY, ParamTrack::constant(ParamValue::Number(0.0)))]);
        fx.enabled = false;
        c.effects = vec![fx];
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        let active = g.track_plans[0].active_clip.as_ref().unwrap();
        assert_eq!(active.transform.opacity, 1.0);
        assert!(active.effect_passes.is_empty());
    }

    #[test]
    fn unknown_effect_types_are_counted_not_silently_dropped() {
        let mut c = clip(1, 0, 100);
        c.effects = vec![EffectInstance {
            id: EffectInstanceId(9),
            effect_type: "some_future_plugin_effect".into(),
            enabled: true,
            params: BTreeMap::new(),
        }];
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![c])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert_eq!(g.unknown_effects, 1);
        assert!(g.track_plans[0].active_clip.as_ref().unwrap().effect_passes.is_empty());
    }

    #[test]
    fn effect_keyframes_are_clip_relative_so_moving_a_clip_carries_its_animation() {
        // Opacity ramps 0 -> 1 over the clip's first 100 ticks.
        let ramp = ParamTrack {
            default: ParamValue::Number(1.0),
            keyframes: vec![
                Keyframe { at: TimeTick(0), value: ParamValue::Number(0.0), interpolation: InterpolationMode::Linear, tangents: None },
                Keyframe { at: TimeTick(100), value: ParamValue::Number(1.0), interpolation: InterpolationMode::Linear, tangents: None },
            ],
        };

        let mut at_origin = clip(1, 0, 100);
        at_origin.effects = vec![transform_effect(vec![(transform::OPACITY, ramp.clone())])];
        let p1 = Project { sequences: vec![sequence(1, vec![video_track(1, vec![at_origin])])], assets: vec![], bins: vec![] };
        let g1 = compiler().compile(&p1, SequenceId(1), TimeTick(50)).unwrap();
        let mid_at_origin = g1.track_plans[0].active_clip.as_ref().unwrap().transform.opacity;

        // Same clip, same animation, moved 1000 ticks later.
        let mut moved = clip(1, 1000, 1100);
        moved.effects = vec![transform_effect(vec![(transform::OPACITY, ramp)])];
        let p2 = Project { sequences: vec![sequence(1, vec![video_track(1, vec![moved])])], assets: vec![], bins: vec![] };
        let g2 = compiler().compile(&p2, SequenceId(1), TimeTick(1050)).unwrap();
        let mid_after_move = g2.track_plans[0].active_clip.as_ref().unwrap().transform.opacity;

        assert!((mid_at_origin - 0.5).abs() < 1e-6, "expected midpoint 0.5, got {mid_at_origin}");
        assert!(
            (mid_at_origin - mid_after_move).abs() < 1e-6,
            "moving a clip must not re-time its animation: {mid_at_origin} vs {mid_after_move}"
        );
    }

    #[test]
    fn nested_sequence_compiles_recursively() {
        let inner = sequence(2, vec![video_track(10, vec![clip(100, 0, 100)])]);
        let mut host_clip = clip(1, 0, 100);
        host_clip.source = ClipSource::NestedSequence(SequenceId(2));
        let host = sequence(1, vec![video_track(1, vec![host_clip])]);
        let p = Project { sequences: vec![host, inner], assets: vec![], bins: vec![] };

        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        match &g.track_plans[0].active_clip.as_ref().unwrap().source {
            FrameSource::Nested(inner_graph) => {
                assert_eq!(inner_graph.sequence, SequenceId(2));
                assert_eq!(
                    inner_graph.track_plans[0].active_clip.as_ref().unwrap().clip,
                    ClipInstanceId(100)
                );
            }
            other => panic!("expected Nested, got {other:?}"),
        }
    }

    #[test]
    fn self_nesting_sequence_does_not_recurse_forever() {
        // A sequence containing a clip whose source is that same sequence.
        // The timeline model doesn't prevent building this, so the compiler
        // must survive it rather than blowing the stack.
        let mut looping = clip(1, 0, 100);
        looping.source = ClipSource::NestedSequence(SequenceId(1));
        let p = Project { sequences: vec![sequence(1, vec![video_track(1, vec![looping])])], assets: vec![], bins: vec![] };
        let g = compiler().compile(&p, SequenceId(1), TimeTick(50)).unwrap();
        assert!(
            g.track_plans[0].active_clip.is_none(),
            "a self-nesting clip should contribute nothing rather than recursing"
        );
    }

    #[test]
    fn missing_sequence_returns_none() {
        let p = Project { sequences: vec![], assets: vec![], bins: vec![] };
        assert!(compiler().compile(&p, SequenceId(42), TimeTick(0)).is_none());
    }

    #[test]
    fn cache_reuses_entries_until_the_project_version_changes() {
        let p = Arc::new(Project {
            sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 100)])])],
            assets: vec![],
            bins: vec![],
        });
        let c = compiler();
        let mut cache = GraphCache::default();

        let a = cache.get_or_compile(&c, &p, SequenceId(1), TimeTick(50)).unwrap();
        let b = cache.get_or_compile(&c, &p, SequenceId(1), TimeTick(50)).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same version + same tick should hit the cache");
        assert_eq!(cache.len(), 1);

        // A different tick is a different entry, not a replacement.
        cache.get_or_compile(&c, &p, SequenceId(1), TimeTick(60)).unwrap();
        assert_eq!(cache.len(), 2);

        // A new project version invalidates everything.
        let p2 = Arc::new((*p).clone());
        let after_edit = cache.get_or_compile(&c, &p2, SequenceId(1), TimeTick(50)).unwrap();
        assert!(!Arc::ptr_eq(&a, &after_edit), "a new version must not serve stale graphs");
        assert_eq!(cache.len(), 1, "cache is cleared on version change");
    }

    #[test]
    fn cache_is_bounded() {
        let p = Arc::new(Project {
            sequences: vec![sequence(1, vec![video_track(1, vec![clip(1, 0, 10_000)])])],
            assets: vec![],
            bins: vec![],
        });
        let c = compiler();
        let mut cache = GraphCache::new(4);
        for tick in 0..20 {
            cache.get_or_compile(&c, &p, SequenceId(1), TimeTick(tick)).unwrap();
        }
        assert!(cache.len() <= 4, "cache grew past max_entries: {}", cache.len());
    }
}
