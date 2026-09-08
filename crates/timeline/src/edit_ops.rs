//! `(Project, EditOp) -> Project` as pure functions, per spec 4.2.
//!
//! Design rule: these functions never mint their own new IDs (new clips,
//! new sequences, new tracks). A pure function synthesizing "fresh" IDs
//! without external input either isn't deterministic or risks collisions
//! across a long edit history — so operations that create something new
//! (`Razor`, `NestSequence`) carry the caller-allocated ID as a field. The
//! caller (eventually the app/session layer, today the property-based test
//! generator) owns a monotonic counter.
//!
//! Scope note: `TrimRipple` supports trimming *one* edge at a time
//! (`new_in` XOR `new_out`). Supporting both edges in one call has a
//! genuinely ambiguous general semantics (which edge is "anchored"?) that
//! isn't worth guessing at without a concrete UI gesture driving it —
//! rejected with `EditError::InvalidRange` for now.

use crate::model::{
    ClipInstance, ClipInstanceId, LinkedGroupId, Project, Sequence, SequenceId, SpeedCurve, Track,
    TrackId, TrackKind,
};
use crate::time::TimeTick;

#[derive(Debug, Clone, PartialEq)]
pub enum EditOp {
    /// If `at` falls inside an existing clip on `track`, that clip must be
    /// split first (like `Razor`) before the ripple — `split_clip_id` is
    /// the caller-allocated ID for the resulting right-hand remainder.
    /// `None` if the caller expects `at` to land in a gap or exactly on a
    /// clip boundary; if it turns out not to, `apply` returns
    /// `EditError::InvalidRange` rather than silently minting an ID.
    Insert { track: TrackId, at: TimeTick, clip: ClipInstance, split_clip_id: Option<ClipInstanceId> },
    Overwrite { track: TrackId, at: TimeTick, clip: ClipInstance },
    Extract { clip: ClipInstanceId },
    Lift { clip: ClipInstanceId },
    TrimRipple { clip: ClipInstanceId, new_in: Option<TimeTick>, new_out: Option<TimeTick> },
    TrimRoll { left_clip: ClipInstanceId, right_clip: ClipInstanceId, new_cut_point: TimeTick },
    TrimSlip { clip: ClipInstanceId, source_delta: TimeTick },
    TrimSlide { clip: ClipInstanceId, timeline_delta: TimeTick },
    RateStretch { clip: ClipInstanceId, new_duration: TimeTick },
    Razor { track: TrackId, at: TimeTick, new_clip_id: ClipInstanceId },
    JoinThroughCut { left_clip: ClipInstanceId, right_clip: ClipInstanceId },
    SetTrackLock { track: TrackId, locked: bool },
    SetSyncLock { track: TrackId, sync_locked: bool },
    SetTrackMute { track: TrackId, muted: bool },
    SetTrackSolo { track: TrackId, solo: bool },
    Group { clips: Vec<ClipInstanceId>, group: LinkedGroupId },
    Ungroup { group: LinkedGroupId },
    NestSequence {
        clips: Vec<ClipInstanceId>,
        new_sequence_id: SequenceId,
        new_video_track_id: TrackId,
        new_audio_track_id: TrackId,
        new_clip_id: ClipInstanceId,
        new_sequence_name: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum EditError {
    ClipNotFound(ClipInstanceId),
    TrackNotFound(TrackId),
    #[allow(dead_code)]
    SequenceNotFound(SequenceId),
    TrackLocked(TrackId),
    WouldOverlap,
    InvalidRange,
}

pub fn apply(project: &Project, op: &EditOp) -> Result<Project, EditError> {
    let mut project = project.clone();
    match op {
        EditOp::Insert { track, at, clip, split_clip_id } => {
            apply_insert(&mut project, *track, *at, clip, *split_clip_id)?
        }
        EditOp::Overwrite { track, at, clip } => apply_overwrite(&mut project, *track, *at, clip)?,
        EditOp::Extract { clip } => apply_extract(&mut project, *clip)?,
        EditOp::Lift { clip } => apply_lift(&mut project, *clip)?,
        EditOp::TrimRipple { clip, new_in, new_out } => {
            apply_trim_ripple(&mut project, *clip, *new_in, *new_out)?
        }
        EditOp::TrimRoll { left_clip, right_clip, new_cut_point } => {
            apply_trim_roll(&mut project, *left_clip, *right_clip, *new_cut_point)?
        }
        EditOp::TrimSlip { clip, source_delta } => apply_trim_slip(&mut project, *clip, *source_delta)?,
        EditOp::TrimSlide { clip, timeline_delta } => {
            apply_trim_slide(&mut project, *clip, *timeline_delta)?
        }
        EditOp::RateStretch { clip, new_duration } => {
            apply_rate_stretch(&mut project, *clip, *new_duration)?
        }
        EditOp::Razor { track, at, new_clip_id } => apply_razor(&mut project, *track, *at, *new_clip_id)?,
        EditOp::JoinThroughCut { left_clip, right_clip } => {
            apply_join(&mut project, *left_clip, *right_clip)?
        }
        EditOp::SetTrackLock { track, locked } => {
            apply_set_flag(&mut project, *track, |t| t.locked = *locked)?
        }
        EditOp::SetSyncLock { track, sync_locked } => {
            apply_set_flag(&mut project, *track, |t| t.sync_locked = *sync_locked)?
        }
        EditOp::SetTrackMute { track, muted } => {
            apply_set_flag(&mut project, *track, |t| t.muted = *muted)?
        }
        EditOp::SetTrackSolo { track, solo } => apply_set_flag(&mut project, *track, |t| t.solo = *solo)?,
        EditOp::Group { clips, group } => apply_group(&mut project, clips, *group)?,
        EditOp::Ungroup { group } => apply_ungroup(&mut project, *group),
        EditOp::NestSequence {
            clips,
            new_sequence_id,
            new_video_track_id,
            new_audio_track_id,
            new_clip_id,
            new_sequence_name,
        } => apply_nest(
            &mut project,
            clips,
            *new_sequence_id,
            *new_video_track_id,
            *new_audio_track_id,
            *new_clip_id,
            new_sequence_name.clone(),
        )?,
    }

    // Final safety net, not a substitute for getting each operation right:
    // the property-based test suite found real bugs (see
    // docs/decisions-log.md) where an individual operation's own logic
    // looked locally correct but produced a corrupt result through an
    // interaction its author didn't anticipate. Every `apply` call
    // validates the *entire* result before returning it — an operation
    // that would corrupt state is rejected outright rather than silently
    // handed back, converting "undiscovered corruption" into "clean,
    // testable failure" even for interactions this file's tests don't
    // cover yet.
    //
    // Sorted first: operations like `TrimSlide` mutate a clip's position in
    // place without re-sorting the storage vec, so checking storage order
    // directly would flag harmless reordering as a fake "not sorted"
    // violation instead of only flagging genuine time overlaps.
    for seq in &mut project.sequences {
        for track in &mut seq.tracks {
            track.clips.sort_by_key(|c| c.timeline_in.0);
            if crate::model::invariants::check_no_overlaps(track).is_err() {
                return Err(EditError::WouldOverlap);
            }
        }
    }
    Ok(project)
}

// --- lookup helpers ---

fn find_track_seq_idx(project: &Project, track_id: TrackId) -> Option<usize> {
    project.sequences.iter().position(|seq| seq.tracks.iter().any(|t| t.id == track_id))
}

fn find_clip_seq_idx(project: &Project, clip_id: ClipInstanceId) -> Option<usize> {
    project
        .sequences
        .iter()
        .position(|seq| seq.tracks.iter().any(|t| t.clips.iter().any(|c| c.id == clip_id)))
}

fn find_track(seq: &Sequence, track_id: TrackId) -> Option<&Track> {
    seq.tracks.iter().find(|t| t.id == track_id)
}

fn find_track_mut(seq: &mut Sequence, track_id: TrackId) -> Option<&mut Track> {
    seq.tracks.iter_mut().find(|t| t.id == track_id)
}

fn find_clip_location(seq: &Sequence, clip_id: ClipInstanceId) -> Option<(usize, usize)> {
    for (ti, t) in seq.tracks.iter().enumerate() {
        if let Some(ci) = t.clips.iter().position(|c| c.id == clip_id) {
            return Some((ti, ci));
        }
    }
    None
}

// --- shared primitives ---

/// Thin alias for `SpeedCurve::source_delta` — the shared implementation
/// lives on the type itself so `render`'s graph compiler (which needs the
/// same timeline-tick -> source-tick mapping to pick a source frame) uses
/// exactly this logic rather than a second copy that could drift.
fn source_delta_for(speed: &SpeedCurve, timeline_delta: i64) -> i64 {
    speed.source_delta(timeline_delta)
}

/// Shifts every clip at or after `at_or_after` on `source_track` by `delta`
/// ticks. If sync-lock is active on `source_track`, the same shift is
/// applied to every other sync-locked track in the sequence too, per spec
/// 4.2: "ripple operations preserve relative offsets on sync-locked
/// tracks."
///
/// Found by the property-based test suite, not designed in up front: when
/// `delta` is negative (a ripple *closing* a gap, e.g. from `Extract`) and
/// a sync-locked sibling track's clip boundaries have diverged from the
/// track that triggered the ripple, a clip can *span* `at_or_after` — its
/// own timeline_in is before the point, so the naive rule ("shift clips
/// with timeline_in >= at_or_after") leaves it in place, while clips after
/// it *do* shift backward, straight into it. Real NLEs handle this by
/// trimming the spanning clip's overlap with the removed range instead of
/// leaving it untouched — see `trim_spanning_clip_for_negative_ripple`.
///
/// That trim can itself land the (now-shorter) clip on top of *other*,
/// completely unrelated content already sitting on the same track — also
/// found by the property test, on a track dense enough with prior edits
/// that the compressed landing spot wasn't actually empty. Rather than
/// silently produce a self-consistent-looking result that's still
/// corrupt, this re-validates every touched track afterward and rejects
/// the whole ripple (`WouldOverlap`) if any of them still collide —
/// consistent with every other operation in this module validating before
/// committing, not after.
fn ripple_shift(seq: &mut Sequence, source_track: TrackId, at_or_after: TimeTick, delta: i64) -> Result<(), EditError> {
    if delta == 0 {
        return Ok(());
    }
    let sync_lock_active = find_track(seq, source_track).map(|t| t.sync_locked).unwrap_or(false);
    for track in seq.tracks.iter() {
        if (track.id == source_track || (sync_lock_active && track.sync_locked)) && track.locked {
            return Err(EditError::TrackLocked(track.id));
        }
    }
    for track in seq.tracks.iter_mut() {
        if track.id == source_track || (sync_lock_active && track.sync_locked) {
            if delta < 0 {
                for clip in track.clips.iter_mut() {
                    if clip.timeline_in < at_or_after && clip.timeline_out > at_or_after {
                        trim_spanning_clip_for_negative_ripple(clip, at_or_after, delta);
                    }
                }
            }
            for clip in track.clips.iter_mut() {
                if clip.timeline_in >= at_or_after {
                    clip.timeline_in = TimeTick(clip.timeline_in.0 + delta);
                    clip.timeline_out = TimeTick(clip.timeline_out.0 + delta);
                }
            }
            track.clips.sort_by_key(|c| c.timeline_in.0);
            if crate::model::invariants::check_no_overlaps(track).is_err() {
                return Err(EditError::WouldOverlap);
            }
        }
    }
    Ok(())
}

/// A clip spans the point where a ripple of `delta` (< 0) ticks is about to
/// remove the range `[at_or_after + delta, at_or_after)`. Keeps whatever
/// part of the clip was *before* that removed range untouched, and shifts
/// whatever part was *at or after* `at_or_after` back by `delta`, exactly
/// like every other clip being rippled — the difference is this clip's
/// span bridges both sides, so it shrinks instead of moving wholesale.
///
/// Known limitation: if the clip's own start is also before the removed
/// range (it bridges "untouched prefix — deleted middle — surviving
/// suffix"), this collapses the prefix and suffix into one continuous
/// clip rather than splitting it — correct duration and no overlap, but
/// the source content has an invisible jump at the seam. A real split
/// would need a caller-supplied fresh ID, which this ripple primitive
/// doesn't have (see the module doc comment's ID-minting rule). Rare
/// enough in practice (it requires a sync-locked sibling track clip to
/// bridge an edit point by a wide margin) that documenting it beats
/// threading an optional ID through every ripple call site for it.
fn trim_spanning_clip_for_negative_ripple(clip: &mut ClipInstance, at_or_after: TimeTick, delta: i64) {
    let removed_start = TimeTick(at_or_after.0 + delta);
    let new_timeline_in = clip.timeline_in.min(removed_start);
    let new_timeline_out = TimeTick(clip.timeline_out.0 + delta);
    let removed_ticks = (clip.timeline_out.0 - clip.timeline_in.0) - (new_timeline_out.0 - new_timeline_in.0);
    clip.source_in = TimeTick(clip.source_in.0 + source_delta_for(&clip.speed, removed_ticks));
    clip.timeline_in = new_timeline_in;
    clip.timeline_out = new_timeline_out;
}

fn insert_sorted(track: &mut Track, clip: ClipInstance) {
    let pos = track.clips.iter().position(|c| c.timeline_in >= clip.timeline_in).unwrap_or(track.clips.len());
    track.clips.insert(pos, clip);
}

// --- operations ---

fn apply_insert(
    project: &mut Project,
    track_id: TrackId,
    at: TimeTick,
    clip: &ClipInstance,
    split_clip_id: Option<ClipInstanceId>,
) -> Result<(), EditError> {
    let seq_idx = find_track_seq_idx(project, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    let seq = &mut project.sequences[seq_idx];
    if find_track(seq, track_id).ok_or(EditError::TrackNotFound(track_id))?.locked {
        return Err(EditError::TrackLocked(track_id));
    }
    let duration = clip.timeline_out.0 - clip.timeline_in.0;
    if duration <= 0 {
        return Err(EditError::InvalidRange);
    }

    // If `at` lands inside an existing clip, it must be split first — a
    // plain ripple (shift everything with timeline_in >= at) would
    // otherwise leave that clip's tail overlapping the newly inserted one.
    let lands_mid_clip = find_track(seq, track_id)
        .unwrap()
        .clips
        .iter()
        .any(|c| c.timeline_in < at && c.timeline_out > at);
    if lands_mid_clip {
        let new_id = split_clip_id.ok_or(EditError::InvalidRange)?;
        split_clip_on_track(seq, track_id, at, new_id)?;
    }

    ripple_shift(seq, track_id, at, duration)?;
    let mut new_clip = clip.clone();
    new_clip.timeline_in = at;
    new_clip.timeline_out = TimeTick(at.0 + duration);
    insert_sorted(find_track_mut(seq, track_id).unwrap(), new_clip);
    Ok(())
}

fn apply_overwrite(project: &mut Project, track_id: TrackId, at: TimeTick, clip: &ClipInstance) -> Result<(), EditError> {
    let seq_idx = find_track_seq_idx(project, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    let seq = &mut project.sequences[seq_idx];
    let track = find_track_mut(seq, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    if track.locked {
        return Err(EditError::TrackLocked(track_id));
    }
    let duration = clip.timeline_out.0 - clip.timeline_in.0;
    if duration <= 0 {
        return Err(EditError::InvalidRange);
    }
    let new_in = at;
    let new_out = TimeTick(at.0 + duration);

    let mut result = Vec::with_capacity(track.clips.len() + 1);
    for c in track.clips.drain(..) {
        if c.timeline_out <= new_in || c.timeline_in >= new_out {
            result.push(c);
        } else if c.timeline_in < new_in && c.timeline_out > new_out {
            // The new clip lands entirely inside this one: split into a
            // left and right remainder.
            let mut left = c.clone();
            left.timeline_out = new_in;
            left.source_out = TimeTick(c.source_out.0 - source_delta_for(&c.speed, c.timeline_out.0 - new_in.0));
            let mut right = c.clone();
            right.timeline_in = new_out;
            right.source_in = TimeTick(c.source_in.0 + source_delta_for(&c.speed, new_out.0 - c.timeline_in.0));
            result.push(left);
            result.push(right);
        } else if c.timeline_in < new_in {
            // Trim this clip's tail.
            let mut left = c.clone();
            let cut = c.timeline_out.0 - new_in.0;
            left.timeline_out = new_in;
            left.source_out = TimeTick(c.source_out.0 - source_delta_for(&c.speed, cut));
            result.push(left);
        } else if c.timeline_out > new_out {
            // Trim this clip's head.
            let mut right = c.clone();
            let cut = new_out.0 - c.timeline_in.0;
            right.timeline_in = new_out;
            right.source_in = TimeTick(c.source_in.0 + source_delta_for(&c.speed, cut));
            result.push(right);
        }
        // else: fully covered by the new clip — dropped.
    }
    let mut new_clip = clip.clone();
    new_clip.timeline_in = new_in;
    new_clip.timeline_out = new_out;
    result.push(new_clip);
    result.sort_by_key(|c| c.timeline_in.0);
    track.clips = result;
    Ok(())
}

fn apply_extract(project: &mut Project, clip_id: ClipInstanceId) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let track_id = seq.tracks[track_idx].id;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(track_id));
    }
    let removed = seq.tracks[track_idx].clips.remove(clip_idx);
    let duration = removed.timeline_out.0 - removed.timeline_in.0;
    ripple_shift(seq, track_id, removed.timeline_out, -duration)?;
    Ok(())
}

fn apply_lift(project: &mut Project, clip_id: ClipInstanceId) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(seq.tracks[track_idx].id));
    }
    seq.tracks[track_idx].clips.remove(clip_idx);
    Ok(())
}

/// Trims one edge (never both — see module doc comment), anchoring the
/// clip's *other* edge and rippling everything after the clip's original
/// end by however much its total duration changed.
fn apply_trim_ripple(
    project: &mut Project,
    clip_id: ClipInstanceId,
    new_in: Option<TimeTick>,
    new_out: Option<TimeTick>,
) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let track_id = seq.tracks[track_idx].id;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(track_id));
    }

    let (old_in, old_out, old_duration, ripple_delta);
    {
        let clip = &mut seq.tracks[track_idx].clips[clip_idx];
        old_in = clip.timeline_in;
        old_out = clip.timeline_out;
        old_duration = old_out.0 - old_in.0;

        match (new_in, new_out) {
            (Some(_), Some(_)) => return Err(EditError::InvalidRange),
            (Some(nin), None) => {
                // Head trim: this clip's own start stays anchored at
                // old_in; the shown range shrinks/grows from the front,
                // which shows up as the OUT point moving.
                let new_duration = old_out.0 - nin.0;
                if new_duration <= 0 {
                    return Err(EditError::InvalidRange);
                }
                clip.source_in = TimeTick(clip.source_in.0 + source_delta_for(&clip.speed, nin.0 - old_in.0));
                clip.timeline_out = TimeTick(old_in.0 + new_duration);
                ripple_delta = new_duration - old_duration;
            }
            (None, Some(nout)) => {
                let new_duration = nout.0 - old_in.0;
                if new_duration <= 0 {
                    return Err(EditError::InvalidRange);
                }
                clip.source_out = TimeTick(clip.source_out.0 + source_delta_for(&clip.speed, nout.0 - old_out.0));
                clip.timeline_out = nout;
                ripple_delta = new_duration - old_duration;
            }
            (None, None) => return Err(EditError::InvalidRange),
        }
    }
    ripple_shift(seq, track_id, old_out, ripple_delta)?;
    Ok(())
}

fn apply_trim_roll(
    project: &mut Project,
    left_id: ClipInstanceId,
    right_id: ClipInstanceId,
    new_cut_point: TimeTick,
) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, left_id).ok_or(EditError::ClipNotFound(left_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (l_track, l_idx) = find_clip_location(seq, left_id).ok_or(EditError::ClipNotFound(left_id))?;
    let (r_track, r_idx) = find_clip_location(seq, right_id).ok_or(EditError::ClipNotFound(right_id))?;
    if l_track != r_track {
        return Err(EditError::InvalidRange);
    }
    if seq.tracks[l_track].locked {
        return Err(EditError::TrackLocked(seq.tracks[l_track].id));
    }
    let (left_in, right_out, left_speed, right_speed, old_cut) = {
        let clips = &seq.tracks[l_track].clips;
        (
            clips[l_idx].timeline_in,
            clips[r_idx].timeline_out,
            clips[l_idx].speed.clone(),
            clips[r_idx].speed.clone(),
            clips[l_idx].timeline_out,
        )
    };
    if seq.tracks[l_track].clips[l_idx].timeline_out != seq.tracks[l_track].clips[r_idx].timeline_in {
        return Err(EditError::InvalidRange); // not actually adjacent
    }
    if new_cut_point <= left_in || new_cut_point >= right_out {
        return Err(EditError::InvalidRange);
    }
    let delta = new_cut_point.0 - old_cut.0;
    let clips = &mut seq.tracks[l_track].clips;
    clips[l_idx].timeline_out = new_cut_point;
    clips[l_idx].source_out = TimeTick(clips[l_idx].source_out.0 + source_delta_for(&left_speed, delta));
    clips[r_idx].timeline_in = new_cut_point;
    clips[r_idx].source_in = TimeTick(clips[r_idx].source_in.0 + source_delta_for(&right_speed, delta));
    Ok(())
}

fn apply_trim_slip(project: &mut Project, clip_id: ClipInstanceId, source_delta: TimeTick) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(seq.tracks[track_idx].id));
    }
    let clip = &mut seq.tracks[track_idx].clips[clip_idx];
    clip.source_in = TimeTick(clip.source_in.0 + source_delta.0);
    clip.source_out = TimeTick(clip.source_out.0 + source_delta.0);
    Ok(())
}

fn apply_trim_slide(project: &mut Project, clip_id: ClipInstanceId, timeline_delta: TimeTick) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(seq.tracks[track_idx].id));
    }
    let delta = timeline_delta.0;
    if delta == 0 {
        return Ok(());
    }
    let clips = &mut seq.tracks[track_idx].clips;
    let (old_in, old_out) = (clips[clip_idx].timeline_in, clips[clip_idx].timeline_out);
    let new_in = TimeTick(old_in.0 + delta);
    let new_out = TimeTick(old_out.0 + delta);

    let left_neighbor = clips[..clip_idx].iter().position(|c| c.timeline_out == old_in);
    let right_neighbor = clips[clip_idx + 1..].iter().position(|c| c.timeline_in == old_out).map(|i| clip_idx + 1 + i);

    if let Some(li) = left_neighbor {
        let new_left_out = new_in;
        if new_left_out <= clips[li].timeline_in {
            return Err(EditError::WouldOverlap);
        }
        let cut = new_left_out.0 - clips[li].timeline_out.0;
        clips[li].source_out = TimeTick(clips[li].source_out.0 + source_delta_for(&clips[li].speed, cut));
        clips[li].timeline_out = new_left_out;
    } else if new_in < TimeTick::ZERO {
        return Err(EditError::InvalidRange);
    }
    if let Some(ri) = right_neighbor {
        let new_right_in = new_out;
        if new_right_in >= clips[ri].timeline_out {
            return Err(EditError::WouldOverlap);
        }
        let cut = new_right_in.0 - clips[ri].timeline_in.0;
        clips[ri].source_in = TimeTick(clips[ri].source_in.0 + source_delta_for(&clips[ri].speed, cut));
        clips[ri].timeline_in = new_right_in;
    }
    clips[clip_idx].timeline_in = new_in;
    clips[clip_idx].timeline_out = new_out;
    Ok(())
}

fn apply_rate_stretch(project: &mut Project, clip_id: ClipInstanceId, new_duration: TimeTick) -> Result<(), EditError> {
    if new_duration.0 <= 0 {
        return Err(EditError::InvalidRange);
    }
    let seq_idx = find_clip_seq_idx(project, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (track_idx, clip_idx) = find_clip_location(seq, clip_id).ok_or(EditError::ClipNotFound(clip_id))?;
    if seq.tracks[track_idx].locked {
        return Err(EditError::TrackLocked(seq.tracks[track_idx].id));
    }
    let clips = &mut seq.tracks[track_idx].clips;
    if !matches!(clips[clip_idx].speed, SpeedCurve::Constant { .. }) {
        return Err(EditError::InvalidRange); // keyframed speed: M7 territory
    }
    let new_out = TimeTick(clips[clip_idx].timeline_in.0 + new_duration.0);
    if clip_idx + 1 < clips.len() && new_out > clips[clip_idx + 1].timeline_in {
        return Err(EditError::WouldOverlap);
    }
    let source_span = clips[clip_idx].source_out.0 - clips[clip_idx].source_in.0;
    clips[clip_idx].timeline_out = new_out;
    clips[clip_idx].speed = SpeedCurve::Constant { numerator: source_span, denominator: new_duration.0 };
    Ok(())
}

fn apply_razor(project: &mut Project, track_id: TrackId, at: TimeTick, new_clip_id: ClipInstanceId) -> Result<(), EditError> {
    let seq_idx = find_track_seq_idx(project, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    let seq = &mut project.sequences[seq_idx];
    let group = split_clip_on_track(seq, track_id, at, new_clip_id)?;
    // Note: propagating the split to other clips in the same linked group
    // (e.g. a linked A/V pair) needs a *second* caller-supplied clip ID for
    // the other track's new right-hand clip, which this single-ID EditOp
    // variant doesn't carry. Left as a follow-up rather than guessing an ID.
    let _ = group;
    Ok(())
}

fn split_clip_on_track(
    seq: &mut Sequence,
    track_id: TrackId,
    at: TimeTick,
    new_clip_id: ClipInstanceId,
) -> Result<Option<LinkedGroupId>, EditError> {
    let track = find_track_mut(seq, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    if track.locked {
        return Err(EditError::TrackLocked(track_id));
    }
    let idx = track.clips.iter().position(|c| c.timeline_in < at && c.timeline_out > at);
    let Some(idx) = idx else {
        return Ok(None);
    };
    let original = track.clips[idx].clone();
    let cut = source_delta_for(&original.speed, at.0 - original.timeline_in.0);

    let mut left = original.clone();
    left.timeline_out = at;
    left.source_out = TimeTick(original.source_in.0 + cut);

    let mut right = original.clone();
    right.id = new_clip_id;
    right.timeline_in = at;
    right.source_in = TimeTick(original.source_in.0 + cut);

    track.clips[idx] = left;
    track.clips.insert(idx + 1, right);
    Ok(original.linked_group)
}

fn apply_join(project: &mut Project, left_id: ClipInstanceId, right_id: ClipInstanceId) -> Result<(), EditError> {
    let seq_idx = find_clip_seq_idx(project, left_id).ok_or(EditError::ClipNotFound(left_id))?;
    let seq = &mut project.sequences[seq_idx];
    let (l_track, l_idx) = find_clip_location(seq, left_id).ok_or(EditError::ClipNotFound(left_id))?;
    let (r_track, r_idx) = find_clip_location(seq, right_id).ok_or(EditError::ClipNotFound(right_id))?;
    if l_track != r_track || r_idx != l_idx + 1 {
        return Err(EditError::InvalidRange); // must be immediately adjacent, same track
    }
    if seq.tracks[l_track].locked {
        return Err(EditError::TrackLocked(seq.tracks[l_track].id));
    }
    let clips = &seq.tracks[l_track].clips;
    let (left, right) = (clips[l_idx].clone(), clips[r_idx].clone());
    if left.timeline_out != right.timeline_in || left.source != right.source || left.source_out != right.source_in {
        return Err(EditError::InvalidRange); // not actually a contiguous cut
    }
    let mut merged = left;
    merged.timeline_out = right.timeline_out;
    merged.source_out = right.source_out;
    let clips = &mut seq.tracks[l_track].clips;
    clips.remove(r_idx);
    clips[l_idx] = merged;
    Ok(())
}

fn apply_set_flag(project: &mut Project, track_id: TrackId, setter: impl FnOnce(&mut Track)) -> Result<(), EditError> {
    let seq_idx = find_track_seq_idx(project, track_id).ok_or(EditError::TrackNotFound(track_id))?;
    let track = find_track_mut(&mut project.sequences[seq_idx], track_id).ok_or(EditError::TrackNotFound(track_id))?;
    setter(track);
    Ok(())
}

fn apply_group(project: &mut Project, clip_ids: &[ClipInstanceId], group: LinkedGroupId) -> Result<(), EditError> {
    for &id in clip_ids {
        let seq_idx = find_clip_seq_idx(project, id).ok_or(EditError::ClipNotFound(id))?;
        let seq = &mut project.sequences[seq_idx];
        let (t, c) = find_clip_location(seq, id).ok_or(EditError::ClipNotFound(id))?;
        seq.tracks[t].clips[c].linked_group = Some(group);
    }
    Ok(())
}

fn apply_ungroup(project: &mut Project, group: LinkedGroupId) {
    for seq in project.sequences.iter_mut() {
        for track in seq.tracks.iter_mut() {
            for clip in track.clips.iter_mut() {
                if clip.linked_group == Some(group) {
                    clip.linked_group = None;
                }
            }
        }
    }
}

/// Basic nested-sequence creation (spec 4.2's "a sequence used as a clip
/// inside another sequence"): extracts the given clips into a brand-new
/// two-track (video + audio) sequence and replaces them with a single
/// `ClipSource::NestedSequence` clip spanning their combined range, placed
/// on the first clip's original track. Multicam (which also nests) is not
/// built on top of this — see docs/decisions-log.md.
fn apply_nest(
    project: &mut Project,
    clip_ids: &[ClipInstanceId],
    new_sequence_id: SequenceId,
    new_video_track_id: TrackId,
    new_audio_track_id: TrackId,
    new_clip_id: ClipInstanceId,
    new_sequence_name: String,
) -> Result<(), EditError> {
    if clip_ids.is_empty() {
        return Err(EditError::InvalidRange);
    }
    let seq_idx = find_clip_seq_idx(project, clip_ids[0]).ok_or(EditError::ClipNotFound(clip_ids[0]))?;
    let (host_track_idx, _) =
        find_clip_location(&project.sequences[seq_idx], clip_ids[0]).ok_or(EditError::ClipNotFound(clip_ids[0]))?;
    let host_track_id = project.sequences[seq_idx].tracks[host_track_idx].id;
    let source_settings = project.sequences[seq_idx].settings.clone();

    let mut extracted = Vec::with_capacity(clip_ids.len());
    {
        let seq = &mut project.sequences[seq_idx];
        for &id in clip_ids {
            let (t, c) = find_clip_location(seq, id).ok_or(EditError::ClipNotFound(id))?;
            if seq.tracks[t].locked {
                return Err(EditError::TrackLocked(seq.tracks[t].id));
            }
            let kind = seq.tracks[t].kind;
            extracted.push((kind, seq.tracks[t].clips.remove(c)));
        }
    }
    let min_in = extracted.iter().map(|(_, c)| c.timeline_in.0).min().unwrap();
    let max_out = extracted.iter().map(|(_, c)| c.timeline_out.0).max().unwrap();

    let mut video_track =
        Track { id: new_video_track_id, kind: TrackKind::Video, name: "V1".into(), clips: vec![], transitions: vec![], gain_db: crate::model::unity_gain(), pan: 0.0, locked: false, sync_locked: true, muted: false, solo: false, height_px: 60 };
    let mut audio_track =
        Track { id: new_audio_track_id, kind: TrackKind::Audio, name: "A1".into(), clips: vec![], transitions: vec![], gain_db: crate::model::unity_gain(), pan: 0.0, locked: false, sync_locked: true, muted: false, solo: false, height_px: 60 };
    for (kind, mut clip) in extracted {
        clip.timeline_in = TimeTick(clip.timeline_in.0 - min_in);
        clip.timeline_out = TimeTick(clip.timeline_out.0 - min_in);
        match kind {
            TrackKind::Video => video_track.clips.push(clip),
            TrackKind::Audio => audio_track.clips.push(clip),
        }
    }
    video_track.clips.sort_by_key(|c| c.timeline_in.0);
    audio_track.clips.sort_by_key(|c| c.timeline_in.0);

    project.sequences.push(Sequence {
        id: new_sequence_id,
        name: new_sequence_name,
        settings: source_settings,
        tracks: vec![video_track, audio_track],
        markers: vec![],
    });

    let nested_clip = ClipInstance {
        id: new_clip_id,
        source: crate::model::ClipSource::NestedSequence(new_sequence_id),
        source_in: TimeTick::ZERO,
        source_out: TimeTick(max_out - min_in),
        timeline_in: TimeTick(min_in),
        timeline_out: TimeTick(max_out),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: crate::keyframe::ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
        audio_pan: crate::keyframe::ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
        linked_group: None,
    };
    insert_sorted(&mut project.sequences[seq_idx].tracks[host_track_idx], nested_clip);
    let _ = host_track_id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::invariants::check_no_overlaps;
    use crate::model::ClipSource;
    use crate::{FrameRate, SequenceSettings};
    use media::{ColorPrimaries, MediaAssetId};

    fn clip(id: u64, tin: i64, tout: i64) -> ClipInstance {
        ClipInstance {
            id: ClipInstanceId(id),
            source: ClipSource::Media(MediaAssetId(1)),
            source_in: TimeTick(0),
            source_out: TimeTick(tout - tin),
            timeline_in: TimeTick(tin),
            timeline_out: TimeTick(tout),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: crate::keyframe::ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
            audio_pan: crate::keyframe::ParamTrack::constant(crate::keyframe::ParamValue::Number(0.0)),
            linked_group: None,
        }
    }

    fn track(id: u64, kind: TrackKind, clips: Vec<ClipInstance>) -> Track {
        Track { id: TrackId(id), kind, name: format!("T{id}"), clips, transitions: vec![], gain_db: crate::model::unity_gain(), pan: 0.0, locked: false, sync_locked: true, muted: false, solo: false, height_px: 60 }
    }

    fn project_with(tracks: Vec<Track>) -> Project {
        Project {
            sequences: vec![Sequence {
                id: SequenceId(1),
                name: "S1".into(),
                settings: SequenceSettings {
                    frame_rate: FrameRate::Fps30,
                    width: 1920,
                    height: 1080,
                    sample_rate: 48000,
                    working_color_primaries: ColorPrimaries::Rec709,
                    drop_frame_timecode: false,
                },
                tracks,
                markers: vec![],
            }],
            assets: vec![],
            bins: vec![],
        }
    }

    fn assert_no_overlaps(project: &Project) {
        for seq in &project.sequences {
            for t in &seq.tracks {
                assert_eq!(check_no_overlaps(t), Ok(()), "overlap on track {:?}: {:?}", t.id, t.clips);
            }
        }
    }

    #[test]
    fn insert_ripples_downstream_clips() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 50), clip(2, 50, 150)])]);
        let new_clip = clip(3, 0, 30); // 30-tick clip; position fields overwritten by `at`
        let p2 = apply(&p, &EditOp::Insert { track: TrackId(1), at: TimeTick(50), clip: new_clip, split_clip_id: None }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 3);
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 50));
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (50, 80)); // inserted
        assert_eq!((clips[2].timeline_in.0, clips[2].timeline_out.0), (80, 180)); // rippled +30
        assert_no_overlaps(&p2);
    }

    #[test]
    fn insert_ripples_sync_locked_sibling_track() {
        let p = project_with(vec![
            track(1, TrackKind::Video, vec![clip(1, 0, 50)]),
            track(2, TrackKind::Audio, vec![clip(2, 0, 100)]),
        ]);
        let p2 = apply(&p, &EditOp::Insert { track: TrackId(1), at: TimeTick(50), clip: clip(3, 0, 20), split_clip_id: None }).unwrap();
        assert_eq!(p2.sequences[0].tracks[0].clips.len(), 2);
        // audio track (sync-locked sibling): its clip covers [0,100), whose
        // timeline_in (0) is before the insert point, so per our ripple
        // rule (only clips with timeline_in >= at shift) it's untouched.
        // Sync-lock ripples don't retroactively split unrelated clips on
        // other tracks — documented, not a bug.
        assert_eq!(p2.sequences[0].tracks[1].clips[0].timeline_in.0, 0);
        assert_no_overlaps(&p2);
    }

    #[test]
    fn insert_refuses_to_ripple_a_locked_sync_locked_sibling_track() {
        let p = project_with(vec![
            track(1, TrackKind::Video, vec![clip(1, 0, 50)]),
            Track {
                id: TrackId(2),
                kind: TrackKind::Audio,
                name: "A1".into(),
                clips: vec![clip(2, 60, 160)],
                transitions: vec![],
                gain_db: crate::model::unity_gain(),
                pan: 0.0,
                locked: true,
                sync_locked: true,
                muted: false,
                solo: false,
                height_px: 60,
            },
        ]);
        // Track 1 (the insert's own target) is unlocked, so this isn't
        // caught by the ordinary "is the target track locked" check every
        // other op already has. Track 2 is sync-locked (so its clip at
        // [60,160) would ordinarily ripple to [80,180)) but is *also*
        // individually locked — locking it must mean its clips genuinely
        // don't move, not just that direct edits to it are refused.
        let result = apply(&p, &EditOp::Insert { track: TrackId(1), at: TimeTick(50), clip: clip(3, 0, 20), split_clip_id: None });
        assert_eq!(result, Err(EditError::TrackLocked(TrackId(2))));
    }

    #[test]
    fn insert_splits_a_clip_when_landing_mid_clip() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 100), clip(2, 100, 200)])]);
        let p2 = apply(
            &p,
            &EditOp::Insert { track: TrackId(1), at: TimeTick(50), clip: clip(3, 0, 20), split_clip_id: Some(ClipInstanceId(99)) },
        )
        .unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 4);
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 50)); // left half of split clip 1
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (50, 70)); // inserted
        assert_eq!((clips[2].timeline_in.0, clips[2].timeline_out.0), (70, 120)); // right half of split clip 1, rippled
        assert_eq!(clips[2].id, ClipInstanceId(99));
        assert_eq!((clips[3].timeline_in.0, clips[3].timeline_out.0), (120, 220)); // clip 2, rippled
        assert_no_overlaps(&p2);
    }

    #[test]
    fn insert_mid_clip_without_split_id_is_rejected() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 100)])]);
        let result = apply(&p, &EditOp::Insert { track: TrackId(1), at: TimeTick(50), clip: clip(3, 0, 20), split_clip_id: None });
        assert_eq!(result, Err(EditError::InvalidRange));
    }

    #[test]
    fn extract_ripples_downstream_closed() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 100), clip(2, 100, 150), clip(3, 150, 250)])]);
        let p2 = apply(&p, &EditOp::Extract { clip: ClipInstanceId(2) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 100));
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (100, 200)); // rippled -50
        assert_no_overlaps(&p2);
    }

    #[test]
    fn lift_leaves_a_gap() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 100), clip(2, 100, 150), clip(3, 150, 250)])]);
        let p2 = apply(&p, &EditOp::Lift { clip: ClipInstanceId(2) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[1].timeline_in.0, 150); // unchanged: no ripple
        assert_no_overlaps(&p2);
    }

    #[test]
    fn trim_ripple_head_shrinks_and_ripples() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20), clip(3, 20, 30)])]);
        let p2 = apply(&p, &EditOp::TrimRipple { clip: ClipInstanceId(2), new_in: Some(TimeTick(12)), new_out: None }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (10, 18)); // anchored start, shrunk
        assert_eq!(clips[1].source_in.0, 2); // trimmed 2 off the head of source
        assert_eq!((clips[2].timeline_in.0, clips[2].timeline_out.0), (18, 28)); // rippled -2
        assert_no_overlaps(&p2);
    }

    #[test]
    fn trim_ripple_out_extends_and_ripples() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20), clip(3, 20, 30)])]);
        let p2 = apply(&p, &EditOp::TrimRipple { clip: ClipInstanceId(2), new_in: None, new_out: Some(TimeTick(25)) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (10, 25));
        assert_eq!((clips[2].timeline_in.0, clips[2].timeline_out.0), (25, 35)); // rippled +5
        assert_no_overlaps(&p2);
    }

    #[test]
    fn trim_ripple_rejects_both_edges_at_once() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10)])]);
        let result = apply(&p, &EditOp::TrimRipple { clip: ClipInstanceId(1), new_in: Some(TimeTick(1)), new_out: Some(TimeTick(9)) });
        assert_eq!(result, Err(EditError::InvalidRange));
    }

    #[test]
    fn trim_roll_moves_shared_cut_point_without_rippling() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20), clip(3, 20, 30)])]);
        let p2 = apply(&p, &EditOp::TrimRoll { left_clip: ClipInstanceId(1), right_clip: ClipInstanceId(2), new_cut_point: TimeTick(8) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips[0].timeline_out.0, 8);
        assert_eq!(clips[1].timeline_in.0, 8);
        assert_eq!(clips[2].timeline_in.0, 20); // untouched — no ripple
        assert_no_overlaps(&p2);
    }

    #[test]
    fn trim_slip_changes_source_only() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 10, 20)])]);
        let p2 = apply(&p, &EditOp::TrimSlip { clip: ClipInstanceId(1), source_delta: TimeTick(5) }).unwrap();
        let c = &p2.sequences[0].tracks[0].clips[0];
        assert_eq!((c.timeline_in.0, c.timeline_out.0), (10, 20)); // unchanged
        assert_eq!((c.source_in.0, c.source_out.0), (5, 15)); // shifted
    }

    #[test]
    fn trim_slide_moves_clip_and_resizes_neighbors() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20), clip(3, 20, 30)])]);
        let p2 = apply(&p, &EditOp::TrimSlide { clip: ClipInstanceId(2), timeline_delta: TimeTick(3) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 13)); // left neighbor extended
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (13, 23)); // slid clip
        assert_eq!((clips[2].timeline_in.0, clips[2].timeline_out.0), (23, 30)); // right neighbor shrunk
        assert_no_overlaps(&p2);
    }

    #[test]
    fn rate_stretch_changes_duration_and_speed() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10)])]);
        let p2 = apply(&p, &EditOp::RateStretch { clip: ClipInstanceId(1), new_duration: TimeTick(5) }).unwrap();
        let c = &p2.sequences[0].tracks[0].clips[0];
        assert_eq!(c.timeline_out.0, 5);
        assert_eq!(c.speed, SpeedCurve::Constant { numerator: 10, denominator: 5 }); // 2x
    }

    #[test]
    fn rate_stretch_rejects_overlap_with_next_clip() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20)])]);
        let result = apply(&p, &EditOp::RateStretch { clip: ClipInstanceId(1), new_duration: TimeTick(15) });
        assert_eq!(result, Err(EditError::WouldOverlap));
    }

    #[test]
    fn razor_splits_clip_at_position() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10)])]);
        let p2 = apply(&p, &EditOp::Razor { track: TrackId(1), at: TimeTick(4), new_clip_id: ClipInstanceId(99) }).unwrap();
        let clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 2);
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 4));
        assert_eq!((clips[0].source_in.0, clips[0].source_out.0), (0, 4));
        assert_eq!((clips[1].timeline_in.0, clips[1].timeline_out.0), (4, 10));
        assert_eq!((clips[1].source_in.0, clips[1].source_out.0), (4, 10));
        assert_eq!(clips[1].id, ClipInstanceId(99));
        assert_no_overlaps(&p2);
    }

    #[test]
    fn razor_at_a_position_with_no_covering_clip_is_a_noop() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 20, 30)])]);
        let p2 = apply(&p, &EditOp::Razor { track: TrackId(1), at: TimeTick(15), new_clip_id: ClipInstanceId(99) }).unwrap();
        assert_eq!(p2.sequences[0].tracks[0].clips.len(), 2);
    }

    #[test]
    fn join_reverses_a_razor_split() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10)])]);
        let split = apply(&p, &EditOp::Razor { track: TrackId(1), at: TimeTick(4), new_clip_id: ClipInstanceId(2) }).unwrap();
        let joined = apply(&split, &EditOp::JoinThroughCut { left_clip: ClipInstanceId(1), right_clip: ClipInstanceId(2) }).unwrap();
        let clips = &joined.sequences[0].tracks[0].clips;
        assert_eq!(clips.len(), 1);
        assert_eq!((clips[0].timeline_in.0, clips[0].timeline_out.0), (0, 10));
        assert_eq!((clips[0].source_in.0, clips[0].source_out.0), (0, 10));
    }

    #[test]
    fn join_rejects_non_contiguous_clips() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20)])]);
        // clip 2's source doesn't continue clip 1's — not a real cut.
        let result = apply(&p, &EditOp::JoinThroughCut { left_clip: ClipInstanceId(1), right_clip: ClipInstanceId(2) });
        assert_eq!(result, Err(EditError::InvalidRange));
    }

    #[test]
    fn track_lock_prevents_edits() {
        let mut p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10)])]);
        p.sequences[0].tracks[0].locked = true;
        let result = apply(&p, &EditOp::Extract { clip: ClipInstanceId(1) });
        assert_eq!(result, Err(EditError::TrackLocked(TrackId(1))));
    }

    #[test]
    fn group_and_ungroup_round_trip() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 0, 10), clip(2, 10, 20)])]);
        let grouped = apply(&p, &EditOp::Group { clips: vec![ClipInstanceId(1), ClipInstanceId(2)], group: crate::model::LinkedGroupId(1) }).unwrap();
        assert_eq!(grouped.sequences[0].tracks[0].clips[0].linked_group, Some(crate::model::LinkedGroupId(1)));
        assert_eq!(grouped.sequences[0].tracks[0].clips[1].linked_group, Some(crate::model::LinkedGroupId(1)));
        let ungrouped = apply(&grouped, &EditOp::Ungroup { group: crate::model::LinkedGroupId(1) }).unwrap();
        assert!(ungrouped.sequences[0].tracks[0].clips.iter().all(|c| c.linked_group.is_none()));
    }

    #[test]
    fn nest_sequence_creates_new_sequence_and_replaces_clips() {
        let p = project_with(vec![track(1, TrackKind::Video, vec![clip(1, 10, 30), clip(2, 30, 50)])]);
        let p2 = apply(
            &p,
            &EditOp::NestSequence {
                clips: vec![ClipInstanceId(1), ClipInstanceId(2)],
                new_sequence_id: SequenceId(99),
                new_video_track_id: TrackId(100),
                new_audio_track_id: TrackId(101),
                new_clip_id: ClipInstanceId(200),
                new_sequence_name: "Nested".into(),
            },
        )
        .unwrap();
        assert_eq!(p2.sequences.len(), 2);
        let nested = &p2.sequences[1];
        assert_eq!(nested.tracks[0].clips.len(), 2);
        assert_eq!(nested.tracks[0].clips[0].timeline_in.0, 0); // repositioned relative to min_in
        let host_clips = &p2.sequences[0].tracks[0].clips;
        assert_eq!(host_clips.len(), 1);
        assert_eq!((host_clips[0].timeline_in.0, host_clips[0].timeline_out.0), (10, 50));
        assert_eq!(host_clips[0].source, ClipSource::NestedSequence(SequenceId(99)));
        assert_no_overlaps(&p2);
    }
}
