//! `(Project, EditOp) -> Project` as pure functions, per spec 4.2. The enum
//! shape is fixed at M0; `apply` itself is M3 work — implementing it now
//! would mean guessing at ripple/sync-lock semantics before the property
//! test suite exists to hold them accountable.

use crate::model::{ClipInstanceId, LinkedGroupId, Project, SequenceId, TrackId};
use crate::time::TimeTick;

#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    Insert { track: TrackId, at: TimeTick, clip: crate::model::ClipInstance },
    Overwrite { track: TrackId, at: TimeTick, clip: crate::model::ClipInstance },
    Extract { clip: ClipInstanceId },
    Lift { clip: ClipInstanceId },
    TrimRipple { clip: ClipInstanceId, new_in: Option<TimeTick>, new_out: Option<TimeTick> },
    TrimRoll { left_clip: ClipInstanceId, right_clip: ClipInstanceId, new_cut_point: TimeTick },
    TrimSlip { clip: ClipInstanceId, source_delta: TimeTick },
    TrimSlide { clip: ClipInstanceId, timeline_delta: TimeTick },
    RateStretch { clip: ClipInstanceId, new_duration: TimeTick },
    Razor { track: TrackId, at: TimeTick },
    JoinThroughCut { left_clip: ClipInstanceId, right_clip: ClipInstanceId },
    SetTrackLock { track: TrackId, locked: bool },
    SetSyncLock { track: TrackId, sync_locked: bool },
    SetTrackMute { track: TrackId, muted: bool },
    SetTrackSolo { track: TrackId, solo: bool },
    Group { clips: Vec<ClipInstanceId>, group: LinkedGroupId },
    Ungroup { group: LinkedGroupId },
    NestSequence { clips: Vec<ClipInstanceId>, new_sequence_name: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    ClipNotFound(ClipInstanceId),
    TrackNotFound(TrackId),
    SequenceNotFound(SequenceId),
    TrackLocked(TrackId),
    WouldOverlap,
    InvalidRange,
}

/// TODO(M3): implement. Every variant must preserve
/// `invariants::check_no_overlaps` on every track and respect
/// `sync_locked`/`locked` flags; ripple variants must preserve relative
/// offsets on sync-locked tracks per spec 4.2.
pub fn apply(_project: &Project, _op: &EditOp) -> Result<Project, EditError> {
    todo!("M3: edit operation semantics, driven by the property-based test suite")
}
