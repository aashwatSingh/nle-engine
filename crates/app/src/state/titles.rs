//! Title clips and transitions — the two things you add to a timeline
//! that aren't imported media.

use super::*;

impl EditorState {
    /// Adds (or replaces) a transition on `track`'s cut at `cut`.
    ///
    /// `desired` is clamped so the transition can't extend past either
    /// neighbouring clip. It's centred on the cut, so each half must fit inside
    /// the clip on that side — a 1s dissolve between two 0.2s clips would
    /// otherwise reach into material that isn't in the sequence at all, and the
    /// renderer would show two frozen frames instead of a dissolve.
    ///
    /// Note the clamp bounds the transition by *clip* lengths, not by how much
    /// spare footage each clip has beyond its trim points. A clip trimmed to
    /// the very end of its file has no tail handle, and the decoder holds its
    /// last frame — so that half of the dissolve freezes. Bounding by handles
    /// would need the source duration for every clip, which is knowable but not
    /// worth threading through until it's a complaint.
    pub fn add_transition(
        &mut self,
        track: TrackId,
        cut: TimeTick,
        kind: timeline::TransitionKind,
        desired: TimeTick,
    ) -> bool {
        let Some(t) = self.sequence().tracks.iter().find(|t| t.id == track) else {
            return false;
        };
        let (left, right) = t.clips_at_cut(cut);
        if left.is_none() && right.is_none() {
            self.status = "no cut there to put a transition on".into();
            return false;
        }
        // Each side limits its own half; a missing side (start or end of the
        // track) doesn't limit anything, since there's no clip to overrun.
        let half_limit = [left, right]
            .iter()
            .flatten()
            .map(|c| c.timeline_duration().0)
            .min()
            .unwrap_or(desired.0);
        let duration = desired.0.min(half_limit * 2).max(1);

        let id = timeline::TransitionId(self.next_id());
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return false;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return false;
        };
        // One transition per cut: adding again replaces rather than stacking two
        // that would both claim the same ticks.
        t.transitions.retain(|tr| tr.at != cut);
        t.transitions.push(timeline::Transition { id, kind, at: cut, duration: TimeTick(duration) });
        self.undo.push("add transition", std::sync::Arc::new(project));
        self.status = if duration < desired.0 {
            format!(
                "transition shortened to {:.2}s to fit the clips",
                duration as f64 / TIMEBASE as f64
            )
        } else {
            String::new()
        };
        true
    }

    // ---- Titles -------------------------------------------------------


    /// Inserts a default title clip on `track`, starting at the playhead and
    /// running for `duration`. Returns the new clip's id, already selected so
    /// the properties panel opens on it and the user can start typing.
    ///
    /// Goes through `EditOp::Overwrite` rather than pushing onto `clips`
    /// directly, so a title dropped on top of existing material behaves like
    /// every other insert — the alternative would be the one edit in the app
    /// that can silently produce overlapping clips.
    pub fn add_title(&mut self, track: TrackId, duration: TimeTick) -> Option<ClipInstanceId> {
        let id = ClipInstanceId(self.next_id());
        let start = self.playhead.max(0);
        let clip = ClipInstance {
            id,
            source: ClipSource::Title(timeline::TitleSpec::default()),
            // A title has no source media to trim into, so its source range is
            // just its own length; `render::graph` ignores both for titles.
            source_in: TimeTick(0),
            source_out: duration,
            timeline_in: TimeTick(start),
            timeline_out: TimeTick(start + duration.0),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        };
        if !self.apply_op("add title", EditOp::Overwrite { track, at: TimeTick(start), clip }) {
            return None;
        }
        self.selected_clips = vec![id];
        Some(id)
    }


    /// Inserts a title at the playhead on the topmost video track, adding a
    /// new track above when that one already has material there.
    ///
    /// The track choice is the whole point. `EditOp::Overwrite` does what it
    /// says, so dropping a title onto an occupied track would silently eat a
    /// second of footage — invisible until the user scrubs back to it. "Add
    /// Title" is a create gesture and must never destroy, so an occupied track
    /// gets a new one above it instead. Both halves land as one undo step,
    /// because creating the track and placing the clip are one action to
    /// whoever pressed the button.
    pub fn add_title_at_playhead(&mut self, duration: TimeTick) -> Option<ClipInstanceId> {
        let start = self.playhead.max(0);
        let end = start + duration.0;
        let top_video = self
            .sequence()
            .tracks
            .iter().rfind(|t| t.kind == TrackKind::Video);
        let free = top_video.map(|t| {
            !t.clips.iter().any(|c| c.timeline_in.0 < end && c.timeline_out.0 > start)
        });

        match (top_video.map(|t| t.id), free) {
            (Some(track), Some(true)) => self.add_title(track, duration),
            _ => {
                // Either there is no video track at all, or the top one is
                // busy here. Same answer: make one.
                let track_id = TrackId(self.next_id());
                let clip_id = ClipInstanceId(self.next_id());
                let mut project = (**self.project()).clone();
                let seq = project.sequences.iter_mut().find(|s| s.id == self.seq_id)?;
                // Pushed onto the end because the model stores video tracks
                // bottom-to-top, so last is topmost — a title belongs over the
                // picture, not under it.
                seq.tracks.push(Track {
                    id: track_id,
                    kind: TrackKind::Video,
                    name: format!("V{}", track_id.0),
                    clips: vec![ClipInstance {
                        id: clip_id,
                        source: ClipSource::Title(timeline::TitleSpec::default()),
                        source_in: TimeTick(0),
                        source_out: duration,
                        timeline_in: TimeTick(start),
                        timeline_out: TimeTick(end),
                        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                        effects: vec![],
                        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                        linked_group: None,
                    }],
                    transitions: vec![],
                    gain_db: timeline::unity_gain(),
                    pan: 0.0,
                    locked: false,
                    sync_locked: true,
                    muted: false,
                    solo: false,
                    height_px: 60,
                });
                self.undo.push("add title", std::sync::Arc::new(project));
                self.selected_clips = vec![clip_id];
                Some(clip_id)
            }
        }
    }


    /// Replaces the spec of the title clip `id`. Returns whether anything
    /// actually changed.
    ///
    /// Coalesces into a single undo entry, because the caller is a properties
    /// panel that re-submits on every keystroke and every frame of a drag —
    /// one undo step per character typed would make Ctrl+Z useless on titles.
    /// An unchanged spec is rejected outright rather than coalesced, so simply
    /// *looking* at a title never touches the history.
    pub fn set_title_spec(&mut self, id: ClipInstanceId, spec: timeline::TitleSpec) -> bool {
        let current = self.sequence().tracks.iter().flat_map(|t| &t.clips).find(|c| c.id == id);
        match current.map(|c| &c.source) {
            Some(ClipSource::Title(existing)) if *existing == spec => return false,
            Some(ClipSource::Title(_)) => {}
            // Not a title (or gone): nothing to set. Silently false rather
            // than a panic — a stale selection is an ordinary UI race.
            _ => return false,
        }

        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return false;
        };
        let Some(clip) = seq
            .tracks
            .iter_mut()
            .flat_map(|t| &mut t.clips)
            .find(|c| c.id == id)
        else {
            return false;
        };
        clip.source = ClipSource::Title(spec);

        // The label carries the clip id so consecutive edits to *this* title
        // fold together while an edit to a different one starts its own entry.
        // `push_or_amend` rather than a coalescing group because there is no
        // reliable "done typing" event to close a group with — see its doc.
        self.undo.push_or_amend(format!("edit title {}", id.0), std::sync::Arc::new(project));
        true
    }

    // ---- Scene-cut detection -------------------------------------------


    pub fn remove_transition(&mut self, track: TrackId, at: TimeTick) {
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return;
        };
        let before = t.transitions.len();
        t.transitions.retain(|tr| tr.at != at);
        if t.transitions.len() != before {
            self.undo.push("remove transition", std::sync::Arc::new(project));
        }
    }

    // ---- Snapping -----------------------------------------------------

}
