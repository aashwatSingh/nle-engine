//! What's selected, and the clipboard operations that act on it.

use super::*;

impl EditorState {
    /// The clip the effects panel and single-clip operations act on: the most
    /// recently added to the selection.
    pub fn primary_selection(&self) -> Option<ClipInstanceId> {
        self.selected_clips.last().copied()
    }


    pub fn is_selected(&self, clip: ClipInstanceId) -> bool {
        self.selected_clips.contains(&clip)
    }


    pub fn select_only(&mut self, clip: ClipInstanceId) {
        self.selected_clips.clear();
        self.selected_clips.push(clip);
    }


    /// Ctrl/Cmd-click behaviour: add if absent, remove if present. Re-adding
    /// moves the clip to the end, making it primary — clicking a clip should
    /// always bring it into focus in the effects panel.
    pub fn toggle_in_selection(&mut self, clip: ClipInstanceId) {
        if let Some(i) = self.selected_clips.iter().position(|c| *c == clip) {
            self.selected_clips.remove(i);
        } else {
            self.selected_clips.push(clip);
        }
    }


    pub fn clear_selection(&mut self) {
        self.selected_clips.clear();
    }


    /// Selection in timeline order, so range operations and paste see clips
    /// left to right regardless of the order they were clicked in.
    fn selection_in_time_order(&self) -> Vec<ClipInstance> {
        let mut clips: Vec<ClipInstance> = self
            .selected_clips
            .iter()
            .filter_map(|id| self.find_clip(*id).map(|(_, c)| c))
            .collect();
        clips.sort_by_key(|c| c.timeline_in.0);
        clips
    }

    // ---- Delete / copy / paste ----------------------------------------


    /// Delete leaving a gap (Premiere's plain Delete).
    pub fn lift_selection(&mut self) {
        let ops: Vec<EditOp> = self
            .selected_clips
            .iter()
            .map(|clip| EditOp::Lift { clip: *clip })
            .collect();
        if self.apply_ops("delete", &ops) {
            self.clear_selection();
        }
    }


    /// Delete and close the gap (Premiere's Shift+Delete, "ripple delete").
    pub fn ripple_delete_selection(&mut self) {
        // Right to left: each extract shifts everything after it earlier, and
        // working backwards means no already-computed target moves under us.
        // Ops locate clips by ID so this is belt-and-braces, but it also keeps
        // the intermediate states sane if an op ever fails partway.
        let mut clips = self.selection_in_time_order();
        clips.reverse();
        let ops: Vec<EditOp> = clips.iter().map(|c| EditOp::Extract { clip: c.id }).collect();
        if self.apply_ops("ripple delete", &ops) {
            self.clear_selection();
        }
    }


    pub fn copy_selection(&mut self) {
        self.clipboard = self.selection_in_time_order();
        self.status = if self.clipboard.is_empty() {
            "nothing selected to copy".into()
        } else {
            format!("copied {} clip(s)", self.clipboard.len())
        };
    }


    /// Pastes the clipboard at the playhead, preserving the clips' relative
    /// spacing, onto the track each came from when it still exists.
    ///
    /// Overwrite rather than insert, matching a plain Premiere paste: it drops
    /// onto the timeline where the playhead is and trims whatever it lands on,
    /// instead of rippling everything downstream.
    pub fn paste_at_playhead(&mut self) {
        if self.clipboard.is_empty() {
            self.status = "clipboard is empty".into();
            return;
        }
        // Offsets are relative to the earliest clip, so a multi-clip paste
        // keeps its internal timing instead of stacking everything at the
        // playhead.
        let base = self.clipboard[0].timeline_in.0;
        let existing_tracks: Vec<(TrackId, TrackKind)> = self
            .sequence()
            .tracks
            .iter()
            .map(|t| (t.id, t.kind))
            .collect();

        let mut ops = Vec::new();
        // Cloned up front: `next_id` needs `&mut self`, and the loop also
        // reads the project.
        let clipboard = self.clipboard.clone();
        for clip in &clipboard {
            let offset = clip.timeline_in.0 - base;
            let at = (self.playhead + offset).max(0);
            let duration = clip.timeline_out.0 - clip.timeline_in.0;

            // The source track may be gone (project reopened, track deleted),
            // so fall back to any track of a matching kind rather than
            // dropping the clip silently.
            let kind = self.clip_kind(clip);
            let track = existing_tracks
                .iter()
                .find(|(id, _)| Some(*id) == self.track_of_clip(clip.id))
                .or_else(|| existing_tracks.iter().find(|(_, k)| *k == kind))
                .map(|(id, _)| *id);
            let track = match track {
                Some(t) => t,
                None => self.ensure_track(kind),
            };

            let mut pasted = clip.clone();
            pasted.id = ClipInstanceId(self.next_id());
            pasted.timeline_in = TimeTick(0);
            pasted.timeline_out = TimeTick(duration);
            // A pasted clip is a new instance, not a member of the original's
            // link group — otherwise it would move in lockstep with clips it
            // has nothing to do with.
            pasted.linked_group = None;
            ops.push(EditOp::Overwrite { track, at: TimeTick(at), clip: pasted });
        }
        if self.apply_ops("paste", &ops) {
            self.status = format!("pasted {} clip(s)", clipboard.len());
        }
    }

}
