//! Everyday timeline editing — marks, snapping, finding and moving clips,
//! and adding or removing effects.

use super::*;

impl EditorState {
    /// Snaps `tick` to a nearby point of interest, or returns it unchanged.
    ///
    /// Candidates are every other clip's edges, the playhead, the in/out marks,
    /// and zero. `exclude` skips the clip being dragged so it can't snap to
    /// itself. `tolerance_ticks` should come from a pixel distance converted at
    /// the current zoom — snapping must feel like a fixed *screen* distance,
    /// not a fixed amount of time, or it becomes unusable when zoomed out.
    pub fn snap_tick(
        &self,
        tick: i64,
        exclude: Option<ClipInstanceId>,
        tolerance_ticks: i64,
    ) -> i64 {
        if !self.snapping || tolerance_ticks <= 0 {
            return tick;
        }
        let mut best: Option<(i64, i64)> = None; // (distance, candidate)
        let mut consider = |candidate: i64| {
            let d = (candidate - tick).abs();
            if d <= tolerance_ticks && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, candidate));
            }
        };

        consider(0);
        consider(self.playhead);
        if let Some(i) = self.in_point {
            consider(i);
        }
        if let Some(o) = self.out_point {
            consider(o);
        }
        for track in &self.sequence().tracks {
            for clip in &track.clips {
                if Some(clip.id) == exclude {
                    continue;
                }
                consider(clip.timeline_in.0);
                consider(clip.timeline_out.0);
            }
        }
        best.map(|(_, c)| c).unwrap_or(tick)
    }

    // ---- In/out marks -------------------------------------------------


    /// Sets the in point at the playhead. An in point at or after the out
    /// point clears the out point rather than leaving an inverted range that
    /// every range operation would then have to special-case.
    pub fn mark_in(&mut self) {
        self.in_point = Some(self.playhead);
        if self.out_point.is_some_and(|o| o <= self.playhead) {
            self.out_point = None;
        }
    }


    pub fn mark_out(&mut self) {
        self.out_point = Some(self.playhead);
        if self.in_point.is_some_and(|i| i >= self.playhead) {
            self.in_point = None;
        }
    }


    pub fn clear_marks(&mut self) {
        self.in_point = None;
        self.out_point = None;
    }


    /// The marked range, if both marks are set and ordered.
    pub fn marked_range(&self) -> Option<(i64, i64)> {
        match (self.in_point, self.out_point) {
            (Some(i), Some(o)) if o > i => Some((i, o)),
            _ => None,
        }
    }


    /// Locates a clip by ID anywhere in the active sequence, returning the
    /// track that holds it and a clone of the clip itself.
    pub fn find_clip(&self, clip_id: ClipInstanceId) -> Option<(TrackId, ClipInstance)> {
        self.sequence().tracks.iter().find_map(|t| {
            t.clips
                .iter()
                .find(|c| c.id == clip_id)
                .map(|c| (t.id, c.clone()))
        })
    }


    /// Moves `clip` to start at `new_timeline_in` on `new_track`, composing
    /// `Lift` (remove without rippling) + `Overwrite` (place, trimming
    /// anything already there) against the *current* project and
    /// coalescing the pair into the drag in progress. This is a plain
    /// overwrite-style move (matches Premiere's default, non-ripple drag) —
    /// dropping a clip onto another one overwrites it rather than being
    /// rejected.
    pub fn move_clip(&mut self, clip: ClipInstanceId, new_track: TrackId, new_timeline_in: i64) {
        let Some((_, original)) = self.find_clip(clip) else {
            return;
        };
        let new_timeline_in = new_timeline_in.max(0);
        let duration = original.timeline_out.0 - original.timeline_in.0;

        let Ok(after_lift) = timeline::edit_ops::apply(self.project(), &EditOp::Lift { clip })
        else {
            return;
        };
        let mut moved = original;
        moved.timeline_in = TimeTick(0);
        moved.timeline_out = TimeTick(duration);
        match timeline::edit_ops::apply(
            &after_lift,
            &EditOp::Overwrite {
                track: new_track,
                at: TimeTick(new_timeline_in),
                clip: moved,
            },
        ) {
            Ok(after_overwrite) => {
                self.undo
                    .update_coalescing(std::sync::Arc::new(after_overwrite));
                self.status.clear();
            }
            Err(e) => self.status = format!("move failed: {e:?}"),
        }
    }


    /// Clones the project and hands `f` a mutable reference to `clip_id`
    /// (found by searching every track in the active sequence), returning
    /// the mutated project. Shared by every effect-editing method below —
    /// none of them go through `timeline::edit_ops` since effect stacks and
    /// param values aren't part of the invariants that module enforces.
    pub(super) fn with_clip_mut(
        &self,
        clip_id: ClipInstanceId,
        f: impl FnOnce(&mut ClipInstance),
    ) -> Option<Project> {
        let mut project = (**self.project()).clone();
        let seq = project.sequences.iter_mut().find(|s| s.id == self.seq_id)?;
        for track in &mut seq.tracks {
            if let Some(clip) = track.clips.iter_mut().find(|c| c.id == clip_id) {
                f(clip);
                return Some(project);
            }
        }
        None
    }


    pub fn add_effect(&mut self, clip_id: ClipInstanceId, effect: timeline::EffectInstance) {
        if let Some(project) = self.with_clip_mut(clip_id, |clip| clip.effects.push(effect)) {
            self.undo.push("add effect", std::sync::Arc::new(project));
        }
    }


    pub fn remove_effect(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
    ) {
        if let Some(project) =
            self.with_clip_mut(clip_id, |clip| clip.effects.retain(|e| e.id != effect_id))
        {
            self.undo
                .push("remove effect", std::sync::Arc::new(project));
        }
    }


    /// The tick the preview should actually render, which is not always the
    /// playhead.
    ///
    /// A sequence's duration is an *exclusive* out point, so parking exactly on
    /// it — which is where playback stops, and where End takes you — resolves
    /// to no active clip and renders black. Every NLE shows the last frame
    /// there instead, so the render tick is clamped one tick inside the
    /// sequence. Only affects on-demand rendering; during playback the decoder
    /// supplies frames for ticks it chose itself and never reaches the out
    /// point.
    pub fn display_tick(&self) -> TimeTick {
        let last = self.sequence_duration_ticks().saturating_sub(1);
        TimeTick(self.playhead.clamp(0, last.max(0)))
    }


    /// The playhead expressed in the clip's own timebase, which is what
    /// keyframe times are stored in.
    ///
    /// Keyframes are clip-relative on purpose (see `render::graph`'s
    /// `local` — it evaluates params at `at - clip.timeline_in`): that's what
    /// makes an animation travel with its clip when the clip is moved or
    /// rippled instead of staying pinned to a sequence time it no longer
    /// occupies. `None` when the playhead isn't over the clip at all, where
    /// "add a keyframe here" has no sensible meaning.
    pub fn playhead_local_to_clip(&self, clip_id: ClipInstanceId) -> Option<TimeTick> {
        let (_, clip) = self.find_clip(clip_id)?;
        if self.playhead < clip.timeline_in.0 || self.playhead >= clip.timeline_out.0 {
            return None;
        }
        Some(TimeTick(self.playhead - clip.timeline_in.0))
    }

}
