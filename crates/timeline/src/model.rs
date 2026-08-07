//! `Project -> Sequence[] -> Track[] -> ClipInstance[]`, per spec 4.2.
//!
//! Types only in M0: no edit operations are implemented here (see
//! `edit_ops.rs`), but the shape has been checked on paper against every
//! required edit (ripple/roll/slip/slide, nesting, linked A/V groups,
//! sync-locks) so M3 doesn't discover a structural gap.

use crate::keyframe::ParamTrack;
use crate::time::{FrameRate, TimeTick};
use media::{ColorPrimaries, MediaAssetId};
use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);
    };
}

id_type!(SequenceId);
id_type!(TrackId);
id_type!(ClipInstanceId);
id_type!(MarkerId);
id_type!(LinkedGroupId);
id_type!(EffectInstanceId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
}

/// What a `ClipInstance` actually plays back. `NestedSequence` is spec 4.2's
/// "a sequence used as a clip inside another sequence" — the seam is here
/// from M0 even though multicam (which also nests) is deferred to v1.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipSource {
    Media(MediaAssetId),
    NestedSequence(SequenceId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SpeedCurve {
    /// e.g. 200% speed = Constant { numerator: 2, denominator: 1 }.
    Constant { numerator: i64, denominator: i64 },
    /// Keyframed rate; resolving timeline-tick -> source-tick under a
    /// non-constant curve is M3/M7 (time remapping) work.
    Keyframed(ParamTrack),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EffectInstance {
    pub id: EffectInstanceId,
    /// Matches an `EffectDescriptor::type_id` in the render crate's registry.
    /// Stored as a string (not a render-crate enum) so `timeline` never has
    /// to depend on `render`.
    pub effect_type: String,
    pub enabled: bool,
    pub params: std::collections::BTreeMap<String, ParamTrack>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClipInstance {
    pub id: ClipInstanceId,
    pub source: ClipSource,
    pub source_in: TimeTick,
    pub source_out: TimeTick,
    pub timeline_in: TimeTick,
    pub timeline_out: TimeTick,
    pub speed: SpeedCurve,
    pub effects: Vec<EffectInstance>,
    pub audio_gain_db: ParamTrack,
    pub audio_pan: ParamTrack,
    pub linked_group: Option<LinkedGroupId>,
}

impl ClipInstance {
    pub fn timeline_duration(&self) -> TimeTick {
        TimeTick(self.timeline_out.0 - self.timeline_in.0)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: TrackId,
    pub kind: TrackKind,
    pub name: String,
    /// Invariant: sorted by `timeline_in`, non-overlapping. Enforced by edit
    /// ops (M3), checked by `invariants::check_no_overlaps` and the
    /// property-based test suite.
    pub clips: Vec<ClipInstance>,
    pub locked: bool,
    pub sync_locked: bool,
    pub muted: bool,
    pub solo: bool,
    pub height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: MarkerId,
    pub position: TimeTick,
    pub duration: TimeTick,
    pub name: String,
    pub comment: String,
    pub color: [f32; 4],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SequenceSettings {
    pub frame_rate: FrameRate,
    pub width: u32,
    pub height: u32,
    pub sample_rate: u32,
    pub working_color_primaries: ColorPrimaries,
    pub drop_frame_timecode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sequence {
    pub id: SequenceId,
    pub name: String,
    pub settings: SequenceSettings,
    pub tracks: Vec<Track>,
    pub markers: Vec<Marker>,
}

impl Sequence {
    /// Derived, not stored: the end of the last clip across all tracks.
    /// Never cache this on the struct — it would need updating on every
    /// edit and could drift; recomputing is O(clips) and cheap.
    pub fn duration(&self) -> TimeTick {
        self.tracks
            .iter()
            .flat_map(|t| t.clips.iter())
            .map(|c| c.timeline_out)
            .max()
            .unwrap_or(TimeTick::ZERO)
    }
}

/// The whole document. Every edit produces a new `Project` value; nothing
/// mutates a `Project` in place. `Arc<Project>` is what actually flows
/// through the app as a cheap-to-clone version snapshot — see
/// `command::UndoStack`.
///
/// M0 note: this uses ordinary `Vec`/`BTreeMap`, so producing a "new version"
/// today means cloning whatever subtree changed, not true structural sharing.
/// If profiling on 500+-clip sequences (the M0 risk list item) shows clone
/// cost is a problem, the fix is swapping in a persistent-vector crate (e.g.
/// `im`) behind these same types — that's a decision to make with real
/// numbers, not now.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub sequences: Vec<Sequence>,
    pub assets: Vec<media::MediaAsset>,
}

pub mod invariants {
    use super::Track;

    #[derive(Debug, PartialEq, Eq)]
    pub enum InvariantViolation {
        Overlap { first: usize, second: usize },
        NotSortedByTimelineIn { index: usize },
    }

    /// Real, tested logic (not a stub) — this is the primitive the M3
    /// property-based test suite (spec 4.2 "Gap and collision invariants")
    /// is built on top of.
    pub fn check_no_overlaps(track: &Track) -> Result<(), InvariantViolation> {
        for i in 1..track.clips.len() {
            if track.clips[i].timeline_in < track.clips[i - 1].timeline_in {
                return Err(InvariantViolation::NotSortedByTimelineIn { index: i });
            }
            if track.clips[i].timeline_in < track.clips[i - 1].timeline_out {
                return Err(InvariantViolation::Overlap { first: i - 1, second: i });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::invariants::*;
    use super::*;

    fn clip(id: u64, tin: i64, tout: i64) -> ClipInstance {
        ClipInstance {
            id: ClipInstanceId(id),
            source: ClipSource::Media(MediaAssetId(0)),
            source_in: TimeTick(0),
            source_out: TimeTick(tout - tin),
            timeline_in: TimeTick(tin),
            timeline_out: TimeTick(tout),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
            audio_pan: ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
            linked_group: None,
        }
    }

    fn track_with(clips: Vec<ClipInstance>) -> Track {
        Track {
            id: TrackId(0),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        }
    }

    #[test]
    fn adjacent_non_overlapping_clips_pass() {
        let track = track_with(vec![clip(1, 0, 100), clip(2, 100, 200)]);
        assert_eq!(check_no_overlaps(&track), Ok(()));
    }

    #[test]
    fn overlapping_clips_are_rejected() {
        let track = track_with(vec![clip(1, 0, 100), clip(2, 50, 200)]);
        assert_eq!(
            check_no_overlaps(&track),
            Err(InvariantViolation::Overlap { first: 0, second: 1 })
        );
    }

    #[test]
    fn out_of_order_clips_are_rejected() {
        let track = track_with(vec![clip(1, 100, 200), clip(2, 0, 50)]);
        assert_eq!(
            check_no_overlaps(&track),
            Err(InvariantViolation::NotSortedByTimelineIn { index: 1 })
        );
    }
}
