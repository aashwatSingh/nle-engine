//! Track-level state: which track a clip is on, and per-track
//! gain, pan, mute and solo.

use super::*;

impl EditorState {
    pub(super) fn clip_kind(&self, clip: &ClipInstance) -> TrackKind {
        self.track_of_clip(clip.id)
            .and_then(|tid| self.sequence().tracks.iter().find(|t| t.id == tid))
            .map(|t| t.kind)
            // A clip whose original track is gone: assume video, the common
            // case, rather than refusing to paste.
            .unwrap_or(TrackKind::Video)
    }


    pub(super) fn track_of_clip(&self, clip: ClipInstanceId) -> Option<TrackId> {
        self.find_clip(clip).map(|(t, _)| t)
    }

    // ---- Mixer --------------------------------------------------------


    /// Runs `f` on one track and pushes the result.
    ///
    /// Track mixer state isn't subject to the clip-position invariants
    /// `timeline::edit_ops` enforces, so — like effect params — this
    /// clone-mutate-pushes directly rather than inventing an `EditOp` for it.
    fn with_track_mut(&mut self, label: &str, track: TrackId, f: impl FnOnce(&mut Track)) {
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return;
        };
        f(t);
        self.undo.push(label, std::sync::Arc::new(project));
    }


    /// The fader value a track's automation reads at the playhead. This is
    /// what the fader widget shows, so an automated track's fader tracks its
    /// curve as the playhead moves rather than sitting wherever it was last
    /// dragged.
    pub fn track_gain_at_playhead(&self, track: TrackId) -> f64 {
        self.sequence()
            .tracks
            .iter()
            .find(|t| t.id == track)
            .and_then(|t| t.gain_db.evaluate_at(TimeTick(self.playhead)).as_scalar())
            .unwrap_or(0.0)
    }


    /// Sets a track's fader. `coalescing` folds a whole drag into one undo step.
    ///
    /// On an **automated** track this writes a keyframe at the playhead rather
    /// than replacing the curve — dragging the fader of an automated track and
    /// having the whole automation vanish is the behaviour this avoids. On a
    /// static track it replaces the constant value, so an ordinary fader move
    /// stays an ordinary fader move and doesn't accidentally start automating.
    pub fn set_track_gain(&mut self, track: TrackId, gain_db: f64, coalescing: bool) {
        let playhead = TimeTick(self.playhead);
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return;
        };
        if t.gain_db.is_animated() {
            t.gain_db.upsert_keyframe(
                playhead,
                ParamValue::Number(gain_db),
                timeline::InterpolationMode::Linear,
            );
        } else {
            t.gain_db = timeline::ParamTrack::constant(ParamValue::Number(gain_db));
        }
        let project = std::sync::Arc::new(project);
        if coalescing {
            self.undo.update_coalescing(project);
        } else {
            self.undo.push("fader", project);
        }
    }


    /// Whether this track's fader has a keyframe exactly at the playhead.
    /// Decides whether the automation button adds or removes one, so it has to
    /// be answerable before the click, not after.
    pub fn track_gain_keyframe_at_playhead(&self, track: TrackId) -> bool {
        self.sequence()
            .tracks
            .iter()
            .find(|t| t.id == track)
            .map(|t| t.gain_db.keyframe_index_at(TimeTick(self.playhead)).is_some())
            .unwrap_or(false)
    }


    /// Adds a fader keyframe at the playhead, holding whatever the fader reads
    /// there. This is what turns a static fader into an automated one; from
    /// then on `set_track_gain` writes keyframes instead of replacing the
    /// value.
    pub fn add_track_gain_keyframe(&mut self, track: TrackId) {
        let playhead = TimeTick(self.playhead);
        let current = self.track_gain_at_playhead(track);
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return;
        };
        t.gain_db.upsert_keyframe(
            playhead,
            ParamValue::Number(current),
            timeline::InterpolationMode::Linear,
        );
        self.undo.push("fader keyframe", std::sync::Arc::new(project));
    }


    /// Removes the fader keyframe at the playhead, if there is one. Returns
    /// whether anything was removed.
    pub fn remove_track_gain_keyframe(&mut self, track: TrackId) -> bool {
        let playhead = TimeTick(self.playhead);
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return false;
        };
        let Some(t) = seq.tracks.iter_mut().find(|t| t.id == track) else {
            return false;
        };
        if !t.gain_db.remove_keyframe_at(playhead) {
            return false;
        }
        self.undo.push("remove fader keyframe", std::sync::Arc::new(project));
        true
    }


    pub fn set_track_pan(&mut self, track: TrackId, pan: f64) {
        self.with_track_mut("pan", track, |t| t.pan = pan.clamp(-1.0, 1.0));
    }


    pub fn set_track_mute(&mut self, track: TrackId, muted: bool) {
        self.apply_op("mute track", EditOp::SetTrackMute { track, muted });
    }


    pub fn set_track_solo(&mut self, track: TrackId, solo: bool) {
        self.apply_op("solo track", EditOp::SetTrackSolo { track, solo });
    }

    // ---- Transitions --------------------------------------------------

    /// Default transition length: 1 second, Premiere's own default.
    pub const DEFAULT_TRANSITION_TICKS: i64 = TIMEBASE;

}
