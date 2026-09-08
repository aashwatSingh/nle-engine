//! Editor session state: the mutable "what is the user looking at and doing
//! right now" layer on top of the pure, immutable `timeline` data model.
//! Every actual timeline mutation still goes through `timeline::edit_ops` —
//! this module never edits a `Project` by hand, only by constructing an
//! `EditOp` and pushing the result through `command::UndoStack`. That's what
//! keeps undo/redo correct without this file having to know how each edit
//! works.

use std::path::PathBuf;
use std::time::Instant;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, EditOp, ParamValue, Project, Sequence, SequenceId,
    SequenceSettings, SpeedCurve, TimeTick, Track, TrackId, TrackKind, TIMEBASE,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Select,
    Razor,
}

/// An in-progress drag gesture, tracked so mouse-move deltas can be applied
/// against the state at drag *start* rather than accumulating rounding error
/// frame to frame, and so the whole drag collapses into one undo step.
#[derive(Debug, Clone)]
pub enum Drag {
    /// `grab_offset_ticks` is fixed at drag start (pointer tick minus the
    /// clip's `timeline_in` at that moment) so the clip doesn't jump to
    /// re-center under the pointer on the first move event. Every other
    /// field the move needs (current track, position, duration, effects)
    /// is re-read from the live project by clip ID on each move — since
    /// `Lift` and `TrimRipple` both locate a clip by ID regardless of which
    /// track/position it's currently at, this self-corrects across
    /// multi-step coalesced drags without needing its own snapshot.
    MoveClip {
        clip: ClipInstanceId,
        grab_offset_ticks: i64,
    },
    /// `base` is the project snapshot from the instant this drag started,
    /// captured once and reused for every move event — unlike `MoveClip`
    /// and `TrimOut`, a head trim is **not** idempotent when recomputed
    /// against the live (already-trimmed) project: `TrimRipple`'s `new_in`
    /// branch derives the result from the clip's *current* `timeline_out`
    /// (see its doc comment — the head trim keeps `timeline_in` anchored
    /// and absorbs the change into `timeline_out` instead), so calling it
    /// again against an already-shrunk clip compounds the shrink on every
    /// mouse-move frame instead of converging on the pointer position.
    /// Re-deriving from the same fixed `base` each frame is what makes the
    /// final result depend only on where the pointer ended up, not on how
    /// many intermediate frames fired along the way.
    TrimIn {
        clip: ClipInstanceId,
        base: std::sync::Arc<Project>,
    },
    TrimOut {
        clip: ClipInstanceId,
    },
}

pub struct EditorState {
    pub undo: command::UndoStack,
    pub seq_id: SequenceId,
    pub playhead: i64,
    /// Selected clips. A `Vec`, not a set, because order carries meaning: the
    /// last one clicked is the *primary* selection, which is what the effects
    /// panel edits. Empty means nothing selected.
    pub selected_clips: Vec<ClipInstanceId>,
    /// Sequence in/out marks (I and O). Used to scope range operations —
    /// extract-range, and what "export" means once it takes a range.
    pub in_point: Option<i64>,
    pub out_point: Option<i64>,
    /// Copied clips, ready to paste.
    ///
    /// Stores whole `ClipInstance`s — effects, keyframes, gain and all — rather
    /// than IDs, so a paste still works after the source clip has been deleted
    /// or the sequence changed underneath it.
    pub clipboard: Vec<ClipInstance>,
    /// Snapping toggle (S). On by default, as in every NLE.
    pub snapping: bool,
    /// Master bus gain, in dB. Session state rather than project state: it's a
    /// monitoring control, and baking it into the saved project would mean a
    /// quiet monitoring choice silently followed the file into an export.
    pub master_gain_db: f64,
    /// Shuttle rate from JKL. `1.0` is normal forward play, `0.0` stopped,
    /// negative reverse. Only `1.0` uses the full A/V pipeline — see
    /// `main.rs`'s `shuttle` handling for why other rates are silent.
    pub shuttle_rate: f64,
    /// Selected item in the Project panel — an asset or a sequence, since
    /// bins hold both (Premiere lists sequences in the Project panel too).
    pub selected_item: Option<timeline::BinItem>,
    pub tool: Tool,
    /// Timeline ticks represented by one pixel — the zoom level.
    pub ticks_per_px: f64,
    /// Ticks scrolled off the left edge of the timeline view.
    pub scroll_ticks: i64,
    pub playing: bool,
    pub play_anchor: Option<(Instant, i64)>,
    pub drag: Option<Drag>,
    /// True while a coalescing group is open, regardless of which gesture
    /// opened it (a timeline drag or an effects-panel slider). Checked once
    /// per frame in `main.rs` so a group whose `drag_stopped`/mouse-up event
    /// was missed (e.g. focus lost mid-drag) can't wedge `UndoStack` —
    /// `begin_coalescing` asserts no group is already open.
    pub coalescing_open: bool,
    pub status: String,
    /// Live playback-health readout, e.g. "dropped 4". Empty when playback is
    /// keeping up or stopped.
    ///
    /// Worth surfacing rather than logging: "the picture looks choppy" is
    /// otherwise indistinguishable from "the footage is choppy", and the
    /// difference decides whether the answer is proxies or a re-export.
    pub playback_health: String,
    /// Path to each imported asset's source file, since playback/preview
    /// needs to re-open it on demand — `MediaAsset` itself already carries
    /// this (`original_absolute_path`), this just avoids re-parsing that
    /// back into a `PathBuf` on every frame.
    pub asset_paths: std::collections::HashMap<media::MediaAssetId, PathBuf>,
    /// Where this project was last saved to / opened from. `None` means
    /// "never saved", so Save falls through to Save As.
    pub project_path: Option<PathBuf>,
    /// Per-clip transcripts, for `title_panel`'s transcript view — words
    /// already mapped to timeline ticks (not source milliseconds), so the
    /// panel can click-to-seek and range-select without re-doing the speed/
    /// trim arithmetic every frame. Populated by `transcribe_clip` and by
    /// `generate_captions` (which transcribes anyway, so it stores the same
    /// result rather than throwing it away). Not persisted in the project
    /// file — regenerating on demand is cheap enough, and a session-only
    /// cache avoids growing the save format for derived data.
    pub transcripts: std::collections::HashMap<ClipInstanceId, Vec<TimelineWord>>,
    next_id: u64,
}

/// One transcribed word, already mapped to timeline ticks.
#[derive(Debug, Clone, PartialEq)]
pub struct TimelineWord {
    pub text: String,
    pub start_tick: i64,
    pub end_tick: i64,
}

const DEFAULT_SEQ_WIDTH: u32 = 1920;
const DEFAULT_SEQ_HEIGHT: u32 = 1080;

impl EditorState {
    pub fn new() -> Self {
        let seq_id = SequenceId(1);
        let project = Project {
            sequences: vec![Sequence {
                id: seq_id,
                name: "Sequence 01".into(),
                settings: SequenceSettings {
                    frame_rate: timeline::FrameRate::Fps30,
                    width: DEFAULT_SEQ_WIDTH,
                    height: DEFAULT_SEQ_HEIGHT,
                    sample_rate: 48_000,
                    working_color_primaries: media::ColorPrimaries::Rec709,
                    drop_frame_timecode: false,
                },
                tracks: vec![],
                markers: vec![],
            }],
            assets: vec![],
            bins: vec![],
        };
        EditorState {
            undo: command::UndoStack::new(
                std::sync::Arc::new(project),
                command::UndoStack::DEFAULT_MAX_HISTORY,
            ),
            seq_id,
            playhead: 0,
            selected_clips: Vec::new(),
            in_point: None,
            out_point: None,
            clipboard: Vec::new(),
            snapping: true,
            master_gain_db: 0.0,
            shuttle_rate: 0.0,
            selected_item: None,
            tool: Tool::Select,
            ticks_per_px: TIMEBASE as f64 / 60.0, // ~60px per second at 1x
            scroll_ticks: 0,
            playing: false,
            play_anchor: None,
            drag: None,
            coalescing_open: false,
            status: String::new(),
            playback_health: String::new(),
            asset_paths: std::collections::HashMap::new(),
            project_path: None,
            transcripts: std::collections::HashMap::new(),
            next_id: 1,
        }
    }

    pub fn begin_drag_edit(&mut self, label: &str) {
        self.undo.begin_coalescing(label);
        self.coalescing_open = true;
    }

    pub fn end_drag_edit(&mut self) {
        self.undo.end_coalescing();
        self.coalescing_open = false;
        self.drag = None;
    }

    /// Safety net for a coalescing group whose end never got observed by
    /// the widget that opened it (see `coalescing_open`'s doc comment).
    /// Call once per frame after building the UI.
    pub fn force_close_stale_drag(&mut self, pointer_down: bool) {
        if self.coalescing_open && !pointer_down {
            self.end_drag_edit();
        }
    }

    pub fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn project(&self) -> &std::sync::Arc<Project> {
        self.undo.current()
    }

    pub fn sequence(&self) -> &Sequence {
        self.project()
            .sequences
            .iter()
            .find(|s| s.id == self.seq_id)
            .expect("active sequence exists")
    }

    pub fn sequence_duration_ticks(&self) -> i64 {
        self.sequence().duration().0
    }

    /// Applies `op`, pushing the result as one undo step labeled `label`. On
    /// failure (e.g. an edit that would overlap another clip), leaves state
    /// untouched and records the reason in `status` instead of panicking —
    /// gestures driven by mouse drags routinely produce positions that
    /// briefly violate invariants, and that must be a no-op, not a crash.
    pub fn apply_op(&mut self, label: &str, op: EditOp) -> bool {
        match timeline::edit_ops::apply(self.project(), &op) {
            Ok(new_project) => {
                self.undo.push(label, std::sync::Arc::new(new_project));
                self.status.clear();
                true
            }
            Err(e) => {
                self.status = format!("{label} failed: {e:?}");
                false
            }
        }
    }

    /// Applies several ops as **one** undo step.
    ///
    /// Deleting a multi-clip selection has to be atomic: one `Ctrl+Z` should
    /// bring back everything the delete removed, not undo it one clip at a
    /// time. Ops are applied in sequence against the accumulating project, so
    /// each sees the result of the last — which matters for rippling ops, where
    /// earlier extracts shift the positions later ones operate near. Locating
    /// clips by ID (never by position) is what keeps that correct.
    ///
    /// All-or-nothing: if any op fails, nothing is pushed.
    pub fn apply_ops(&mut self, label: &str, ops: &[EditOp]) -> bool {
        if ops.is_empty() {
            return false;
        }
        let mut project = (**self.project()).clone();
        for op in ops {
            match timeline::edit_ops::apply(&project, op) {
                Ok(next) => project = next,
                Err(e) => {
                    self.status = format!("{label} failed: {e:?}");
                    return false;
                }
            }
        }
        self.undo.push(label, std::sync::Arc::new(project));
        self.status.clear();
        true
    }

    /// Same as `apply_op`, but coalesces into the drag currently in
    /// progress rather than pushing a new undo step per mouse-move event —
    /// see `command::UndoStack::update_coalescing`.
    pub fn apply_op_coalescing(&mut self, op: EditOp) -> bool {
        self.apply_op_coalescing_from(&self.project().clone(), op)
    }

    /// Same as `apply_op_coalescing`, but applies `op` against a caller-
    /// supplied `base` instead of the live project. Needed for edits whose
    /// result depends on more than just "this clip, found by ID" — see
    /// `Drag::TrimIn`'s doc comment for why a head trim specifically can't
    /// use the live project the way a move or tail trim safely can.
    pub fn apply_op_coalescing_from(&mut self, base: &Project, op: EditOp) -> bool {
        match timeline::edit_ops::apply(base, &op) {
            Ok(new_project) => {
                self.undo
                    .update_coalescing(std::sync::Arc::new(new_project));
                self.status.clear();
                true
            }
            Err(e) => {
                self.status = format!("edit failed: {e:?}");
                false
            }
        }
    }

    // ---- Selection ----------------------------------------------------

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

    fn clip_kind(&self, clip: &ClipInstance) -> TrackKind {
        self.track_of_clip(clip.id)
            .and_then(|tid| self.sequence().tracks.iter().find(|t| t.id == tid))
            .map(|t| t.kind)
            // A clip whose original track is gone: assume video, the common
            // case, rather than refusing to paste.
            .unwrap_or(TrackKind::Video)
    }

    fn track_of_clip(&self, clip: ClipInstanceId) -> Option<TrackId> {
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

    /// Samples decoded frames across `clip_id`'s source range, finds hard
    /// cuts via `render::scene_cut`, and splits the clip on the timeline at
    /// each one — all as a single undo step. Returns how many cuts were
    /// found and applied (0 if none, or if the clip can't be analysed —
    /// no media source, no known file path, or a keyframed speed curve; see
    /// `scene_cut_ops`).
    ///
    /// Sampling, not full-frame-rate decoding: `media_ffmpeg::decode_frame_at`
    /// reopens and seeks the file per call (the same trade `Preview::render`
    /// makes for scrubbing — see its doc), so decoding every source frame of
    /// a long clip would be slow enough to freeze the UI for seconds. Eight
    /// samples per second of source is enough to find any cut a human would
    /// call a hard cut, capped at 600 samples so an hour-long clip doesn't
    /// turn one click into a multi-minute wait.
    pub fn detect_scene_cuts(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((track_id, clip)) = self.find_clip(clip_id) else { return 0 };
        let ClipSource::Media(asset_id) = clip.source else { return 0 };
        let Some(path) = self.asset_paths.get(&asset_id).cloned() else { return 0 };

        let source_span = clip.source_out.0 - clip.source_in.0;
        if source_span <= 0 {
            return 0;
        }
        const SAMPLES_PER_SECOND: i64 = 8;
        let sample_count =
            ((source_span / (TIMEBASE / SAMPLES_PER_SECOND)).clamp(2, 600)) as usize;

        let mut histograms = Vec::with_capacity(sample_count);
        let mut sample_ticks = Vec::with_capacity(sample_count);
        for i in 0..sample_count {
            let t = clip.source_in.0 + (source_span * i as i64) / (sample_count as i64 - 1).max(1);
            let Ok(frame) = media_ffmpeg::decode_frame_at(&path, t) else { continue };
            histograms.push(render::scopes::Histogram::from_rgba(&frame.rgba));
            sample_ticks.push(t);
        }
        if histograms.len() < 2 {
            return 0;
        }

        let cut_indices =
            render::scene_cut::detect_cuts(&histograms, &render::scene_cut::CutDetectorConfig::default());
        let source_cut_ticks: Vec<i64> = cut_indices.iter().map(|&i| sample_ticks[i]).collect();

        let ops = self.scene_cut_ops(track_id, &clip, &source_cut_ticks);
        if ops.is_empty() {
            return 0;
        }
        // `apply_ops` is all-or-nothing: if any op in the batch fails, it
        // pushes nothing and leaves the project untouched. Reporting
        // `ops.len()` regardless of that return value would tell the caller
        // "found and split at N cuts" when the split never actually
        // happened — caught during development via a test-fixture id
        // collision that made `apply_ops` fail and this code claim success
        // anyway (see `silence_removal_ops`'s equivalent fix for the fuller
        // story — it's the same bug in the sibling feature).
        if self.apply_ops("detect scene cuts", &ops) { ops.len() } else { 0 }
    }

    /// The pure, testable half of `detect_scene_cuts`: turns already-known
    /// source ticks into `Razor` ops on `clip`'s track. Split out specifically
    /// so the tick-mapping arithmetic (timeline placement + source trim +
    /// speed inversion) can be tested without decoding real video — see
    /// `crates/app/src/state.rs`'s test module and
    /// `crates/render/tests/scene_cut.rs` for where the two halves of this
    /// feature are actually verified.
    ///
    /// Refuses a `SpeedCurve::Keyframed` clip outright (empty result, not a
    /// wrong mapping): recovering a timeline tick from a source tick needs
    /// inverting the speed curve, which is a cheap closed-form division for
    /// constant speed and not cheap in general for a keyframed one — the same
    /// scope line clip-speed audio retiming draws.
    fn scene_cut_ops(&mut self, track: TrackId, clip: &ClipInstance, source_cut_ticks: &[i64]) -> Vec<EditOp> {
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return Vec::new() };
        source_cut_ticks
            .iter()
            .filter(|&&t| t > clip.source_in.0 && t < clip.source_out.0)
            .map(|&source_tick| {
                let timeline_tick =
                    clip.timeline_in.0 + (source_tick - clip.source_in.0) * denominator / numerator;
                EditOp::Razor {
                    track,
                    at: TimeTick(timeline_tick),
                    new_clip_id: ClipInstanceId(self.next_id()),
                }
            })
            .collect()
    }

    // ---- Silence-based auto-cut ----------------------------------------

    /// Decodes `clip_id`'s audio, finds pauses via `audio::silence`, and
    /// ripple-deletes each one — all as a single undo step. Returns how many
    /// gaps were removed.
    ///
    /// Scoped to `SpeedCurve::Constant` clips, same reasoning as
    /// `detect_scene_cuts`. Uses the real decoder-backed `audio_source::
    /// DecodedSampleSource` — the same sample source `audio::timeline_mix`
    /// itself uses for playback and export — so "what gets analysed" is
    /// exactly "what would actually play", not a separate approximation.
    /// Decodes `clip`'s whole source range to **interleaved stereo** `f32`
    /// samples, via the same real decoder-backed `audio_source::
    /// DecodedSampleSource` `audio::timeline_mix` itself uses — so "what
    /// gets analysed" is always exactly "what would actually play". `None`
    /// when the clip isn't decodable media, or has no source range.
    ///
    /// Interleaved, not mixed to mono, because loudness measurement is
    /// channel-aware (EBU R128 weighs a stereo pair differently from a mono
    /// signal — see `audio::loudness`) and collapsing channels here would
    /// silently answer a different, wrong question. `decode_clip_mono_audio`
    /// is a thin wrapper around this for the callers that *do* want mono
    /// (silence and onset detection, where channel summation isn't part of
    /// the measurement being made).
    fn decode_clip_interleaved_audio(&self, clip: &ClipInstance) -> Option<(Vec<f32>, u32)> {
        let ClipSource::Media(asset_id) = clip.source else { return None };
        if !self.asset_paths.contains_key(&asset_id) {
            return None;
        }
        let sample_rate = self.sequence().settings.sample_rate;
        let ticks_to_frames =
            |ticks: i64| -> i64 { (ticks as i128 * sample_rate as i128 / TIMEBASE as i128) as i64 };
        let source_frames = ticks_to_frames(clip.source_out.0 - clip.source_in.0);
        if source_frames <= 0 {
            return None;
        }

        let mut source = audio_source::DecodedSampleSource::new(self.asset_paths.clone());
        let interleaved = {
            use audio::SampleSource;
            source.samples_at(asset_id, ticks_to_frames(clip.source_in.0), source_frames as usize, sample_rate)
        };
        if interleaved.is_empty() {
            return None;
        }
        Some((interleaved, sample_rate))
    }

    /// Decodes `clip`'s whole source range to mono `f32` samples. Shared by
    /// every audio-analysis action that doesn't need channel-aware
    /// measurement (`detect_silence_and_ripple_delete`,
    /// `detect_beats_and_add_markers`) — see `decode_clip_interleaved_audio`
    /// for why loudness matching needs the un-mixed version instead.
    fn decode_clip_mono_audio(&self, clip: &ClipInstance) -> Option<(Vec<f32>, u32)> {
        let (interleaved, sample_rate) = self.decode_clip_interleaved_audio(clip)?;
        let mono: Vec<f32> = interleaved
            .chunks_exact(audio::CHANNELS)
            .map(|frame| frame.iter().sum::<f32>() / audio::CHANNELS as f32)
            .collect();
        Some((mono, sample_rate))
    }

    pub fn detect_silence_and_ripple_delete(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((track_id, clip)) = self.find_clip(clip_id) else { return 0 };
        let Some((mono, sample_rate)) = self.decode_clip_mono_audio(&clip) else { return 0 };

        let config = audio::silence::SilenceConfig {
            threshold_dbfs: -40.0,
            min_silence_seconds: 0.3,
            padding_seconds: 0.1,
        };
        let silent = audio::silence::detect_silence(&mono, sample_rate, &config);
        let removed = audio::silence::ranges_to_remove(&silent, &config);
        if removed.is_empty() {
            return 0;
        }

        let ops = self.silence_removal_ops(track_id, &clip, &removed);
        if ops.is_empty() {
            return 0;
        }
        // Same reasoning as `detect_scene_cuts`: `apply_ops` is all-or-
        // nothing, and reporting success without checking its return value
        // is a real bug, not a hypothetical one — this is exactly what a
        // real-footage run first caught (a test-fixture id collision made
        // `apply_ops` fail with `ClipNotFound`, and this code claimed 8
        // gaps removed while the project was actually untouched).
        if self.apply_ops("remove silence", &ops) { ops.len() / 3 } else { 0 }
    }

    /// The pure, testable half of `detect_silence_and_ripple_delete`: turns
    /// already-known gaps (`(start_seconds, end_seconds)`, relative to
    /// `clip.source_in`) into `Razor`+`Razor`+`Extract` triples.
    ///
    /// Each gap becomes two razors (isolating it as its own clip) and one
    /// extract (ripple-deleting that clip) — no new `EditOp` variant needed,
    /// this composes entirely from existing primitives. Multiple gaps are
    /// processed **rightmost first**: an `Extract` ripples every clip to its
    /// right leftward, and processing right-to-left guarantees every gap
    /// still queued sits entirely left of anything already removed, so its
    /// precomputed timeline tick is never invalidated by an earlier step in
    /// this same batch.
    fn silence_removal_ops(
        &mut self,
        track: TrackId,
        clip: &ClipInstance,
        removed_source_seconds: &[(f64, f64)],
    ) -> Vec<EditOp> {
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return Vec::new() };
        let to_timeline_tick = |source_seconds: f64| -> i64 {
            let source_offset = (source_seconds * TIMEBASE as f64).round() as i64;
            clip.timeline_in.0 + source_offset * denominator / numerator
        };

        let mut ranges: Vec<(i64, i64)> = removed_source_seconds
            .iter()
            .map(|&(s, e)| (to_timeline_tick(s), to_timeline_tick(e)))
            // A gap touching either edge of the clip isn't a middle section
            // to isolate and extract — trimming an edge is a different
            // operation (TrimRipple), out of scope for this action.
            .filter(|&(s, e)| s > clip.timeline_in.0 && e < clip.timeline_out.0 && e > s)
            .collect();
        ranges.sort_by_key(|r| std::cmp::Reverse(r.0));

        let mut ops = Vec::with_capacity(ranges.len() * 3);
        for (start, end) in ranges {
            // `new_clip_id` from the first razor becomes the *right*-hand
            // piece of that split, which is then the *left*-hand piece of
            // the second split (so it keeps the same id) — see the test
            // module for the worked-through reasoning.
            let gap_clip_id = ClipInstanceId(self.next_id());
            let after_gap_id = ClipInstanceId(self.next_id());
            ops.push(EditOp::Razor { track, at: TimeTick(start), new_clip_id: gap_clip_id });
            ops.push(EditOp::Razor { track, at: TimeTick(end), new_clip_id: after_gap_id });
            ops.push(EditOp::Extract { clip: gap_clip_id });
        }
        ops
    }

    // ---- Beat-sync -------------------------------------------------------

    /// Decodes `clip_id`'s audio, finds onsets via `audio::beat`, and adds a
    /// sequence marker at each one — one undo step. Returns how many markers
    /// were added.
    ///
    /// Markers, not razor cuts: a detected beat is a *snap target*, not an
    /// instruction to restructure the timeline the way a scene cut or a
    /// silence gap is. `EditorState::snap_tick` already treats every marker
    /// as a snap candidate, so this makes beats magnetic for free.
    pub fn detect_beats_and_add_markers(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        let Some((mono, sample_rate)) = self.decode_clip_mono_audio(&clip) else { return 0 };

        let onset_seconds = audio::beat::detect_onsets(&mono, sample_rate, &audio::beat::OnsetConfig::default());
        if onset_seconds.is_empty() {
            return 0;
        }
        // Onsets are relative to the decoded buffer's start, i.e. relative to
        // clip.source_in — `beat_markers` expects absolute source seconds.
        let source_in_seconds = clip.source_in.0 as f64 / TIMEBASE as f64;
        let absolute: Vec<f64> = onset_seconds.iter().map(|&t| source_in_seconds + t).collect();

        let markers = self.beat_markers(&clip, &absolute);
        if markers.is_empty() {
            return 0;
        }
        let added = markers.len();
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else { return 0 };
        seq.markers.extend(markers);
        self.undo.push("detect beats", std::sync::Arc::new(project));
        added
    }

    /// The pure, testable half: turns already-known onset times (absolute
    /// source seconds) into `Marker`s at the corresponding timeline ticks.
    /// Same tick-mapping and speed-inversion reasoning as `scene_cut_ops`;
    /// see that method's doc for the fuller explanation. Onsets at or beyond
    /// the clip's own source bounds are dropped rather than placing a marker
    /// outside the material it's supposed to mark.
    fn beat_markers(&mut self, clip: &ClipInstance, onset_source_seconds: &[f64]) -> Vec<timeline::Marker> {
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return Vec::new() };
        onset_source_seconds
            .iter()
            .filter_map(|&onset_s| {
                let source_tick = (onset_s * TIMEBASE as f64).round() as i64;
                if source_tick <= clip.source_in.0 || source_tick >= clip.source_out.0 {
                    return None;
                }
                let timeline_tick =
                    clip.timeline_in.0 + (source_tick - clip.source_in.0) * denominator / numerator;
                Some(timeline::Marker {
                    id: timeline::MarkerId(self.next_id()),
                    position: TimeTick(timeline_tick),
                    duration: TimeTick(0),
                    name: "Beat".into(),
                    comment: String::new(),
                    color: [1.0, 0.8, 0.0, 1.0],
                })
            })
            .collect()
    }

    // ---- Loudness auto-match -------------------------------------------

    /// Measures `clip_id`'s integrated loudness (real EBU R128, via
    /// `audio::loudness::LoudnessAnalyzer` — the same measurement export
    /// reports) and sets its gain to reach `target_lufs`. Returns the gain
    /// applied, or `None` if the clip couldn't be measured or already has
    /// keyframed gain automation (see `apply_loudness_match_gain`).
    pub fn match_clip_loudness(&mut self, clip_id: ClipInstanceId, target_lufs: f64) -> Option<f64> {
        let (_, clip) = self.find_clip(clip_id)?;
        let (interleaved, sample_rate) = self.decode_clip_interleaved_audio(&clip)?;
        let measurement =
            audio::loudness::LoudnessMeasurement::analyze(&interleaved, audio::CHANNELS, sample_rate);
        let gain = audio::loudness::gain_to_reach_target(measurement.integrated_lufs as f64, target_lufs);
        self.apply_loudness_match_gain(clip_id, gain).then_some(gain)
    }

    /// The pure, testable half: writes an already-computed gain to
    /// `clip_id`'s `audio_gain_db`, replacing whatever was there. Refuses
    /// (returns `false`, no undo entry) if the clip is missing or its gain
    /// is already keyframed — loudness matching sets one flat value for the
    /// whole clip, and silently overwriting real fader automation with that
    /// would be destroying user work, not matching loudness.
    fn apply_loudness_match_gain(&mut self, clip_id: ClipInstanceId, gain_db: f64) -> bool {
        let Some((_, clip)) = self.find_clip(clip_id) else { return false };
        if clip.audio_gain_db.is_animated() {
            return false;
        }
        let mut project = (**self.project()).clone();
        let Some(clip_mut) = project
            .sequences
            .iter_mut()
            .flat_map(|s| &mut s.tracks)
            .flat_map(|t| &mut t.clips)
            .find(|c| c.id == clip_id)
        else {
            return false;
        };
        clip_mut.audio_gain_db = timeline::ParamTrack::constant(ParamValue::Number(gain_db));
        self.undo.push("match loudness", std::sync::Arc::new(project));
        true
    }

    // ---- Warp Stabilizer -------------------------------------------------

    /// Samples decoded frames across `clip_id`'s source range, estimates
    /// frame-to-frame motion via `render::stabilize` (block matching on a
    /// downsampled greyscale copy), smooths the resulting camera path, and
    /// writes the correction as `Transform::POSITION` keyframes — one undo
    /// step. Returns how many keyframes were written.
    ///
    /// Translation-only: see `render::stabilize`'s module doc for what this
    /// does and doesn't correct (no rotation/scale/perspective, no border
    /// crop or fill). Scoped to `SpeedCurve::Constant` clips, same reasoning
    /// as every other analysis action in this file.
    pub fn stabilize_clip(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        let ClipSource::Media(asset_id) = clip.source else { return 0 };
        let Some(path) = self.asset_paths.get(&asset_id).cloned() else { return 0 };
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return 0 };

        let source_span = clip.source_out.0 - clip.source_in.0;
        if source_span <= 0 {
            return 0;
        }
        const SAMPLES_PER_SECOND: i64 = 6;
        let sample_count = (source_span / (TIMEBASE / SAMPLES_PER_SECOND)).clamp(3, 300) as usize;
        const DOWNSAMPLE: usize = 8;

        let mut luma_frames: Vec<Vec<f32>> = Vec::with_capacity(sample_count);
        let mut sample_ticks: Vec<i64> = Vec::with_capacity(sample_count);
        let mut dims: Option<(usize, usize)> = None;
        for i in 0..sample_count {
            let t = clip.source_in.0 + (source_span * i as i64) / (sample_count as i64 - 1).max(1);
            let Ok(frame) = media_ffmpeg::decode_frame_at(&path, t) else { continue };
            let (w, h) = (frame.width as usize, frame.height as usize);
            if dims.is_some() && dims != Some((w.div_ceil(DOWNSAMPLE), h.div_ceil(DOWNSAMPLE))) {
                continue; // a resolution change mid-source shouldn't happen, but never mix frame sizes if it does
            }
            luma_frames.push(downsample_luma(&frame.rgba, w, h, DOWNSAMPLE));
            sample_ticks.push(t);
            dims = Some((w.div_ceil(DOWNSAMPLE), h.div_ceil(DOWNSAMPLE)));
        }
        let Some((dw, dh)) = dims else { return 0 };
        if luma_frames.len() < 3 {
            return 0;
        }

        let config = render::stabilize::MotionConfig { max_offset: 24, downsample: 1 };
        let mut cumulative = vec![(0.0, 0.0)];
        for pair in luma_frames.windows(2) {
            let (dx, dy) = render::stabilize::estimate_translation(&pair[0], &pair[1], dw, dh, &config);
            let (px, py) = *cumulative.last().unwrap();
            // Scale back up from downsampled-image pixels to source pixels.
            cumulative.push((px + dx * DOWNSAMPLE as f64, py + dy * DOWNSAMPLE as f64));
        }
        let offsets = render::stabilize::stabilization_offsets(&cumulative, 5);

        let ticks_and_offsets: Vec<(i64, (f64, f64))> = sample_ticks
            .iter()
            .zip(&offsets)
            .map(|(&source_tick, &offset)| {
                let timeline_tick =
                    clip.timeline_in.0 + (source_tick - clip.source_in.0) * denominator / numerator;
                // `offset` is exactly the shift that makes `cumulative +
                // offset == smoothed` (see `stabilization_offsets`'s doc and
                // its own tests) — since the decoded frame's content already
                // sits at `cumulative[i]`'s natural drift, applying `offset`
                // as the Transform position directly moves the *displayed*
                // content from `cumulative[i]` to `smoothed[i]`. No sign
                // flip: adding is exactly what's needed, not its inverse.
                (timeline_tick, offset)
            })
            .collect();

        self.apply_stabilization_keyframes(clip_id, &ticks_and_offsets)
    }


    /// The pure, testable half: writes already-computed `(timeline_tick,
    /// offset)` pairs as `Transform::POSITION` keyframes, adding each offset
    /// to whatever constant position the clip already had (so a deliberate
    /// framing choice survives stabilization layered on top of it) and
    /// creating the Transform effect if the clip has none yet. Refuses
    /// (returns 0, no undo entry) if `Transform::POSITION` is already
    /// keyframed — this writes one full curve, not a patch onto an existing
    /// one, and overwriting real keyframes the user placed by hand would be
    /// destroying their work, not stabilizing their shot.
    fn apply_stabilization_keyframes(&mut self, clip_id: ClipInstanceId, ticks_and_offsets: &[(i64, (f64, f64))]) -> usize {
        if ticks_and_offsets.is_empty() {
            return 0;
        }
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        let existing = clip.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID).cloned();
        if let Some(t) = &existing {
            if let Some(pos) = t.params.get(render::transform::POSITION) {
                if pos.is_animated() {
                    return 0;
                }
            }
        }
        let base = existing
            .as_ref()
            .and_then(|t| t.params.get(render::transform::POSITION))
            .map(|track| match track.default {
                ParamValue::Vec2(x, y) => (x, y),
                _ => (0.0, 0.0),
            })
            .unwrap_or((0.0, 0.0));

        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else { return 0 };
        let Some(clip_mut) = seq.tracks.iter_mut().flat_map(|t| &mut t.clips).find(|c| c.id == clip_id) else {
            return 0;
        };
        let effect_id = match clip_mut.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID) {
            Some(e) => e.id,
            None => {
                let id = timeline::EffectInstanceId(self.next_id());
                let mut params = std::collections::BTreeMap::new();
                for p in render::transform::descriptor().params {
                    params.insert(p.name.to_string(), timeline::ParamTrack::constant(p.default));
                }
                clip_mut.effects.push(timeline::EffectInstance {
                    id,
                    effect_type: render::transform::TYPE_ID.to_string(),
                    enabled: true,
                    params,
                });
                id
            }
        };
        let effect = clip_mut.effects.iter_mut().find(|e| e.id == effect_id).unwrap();
        let pos_track = effect.params.get_mut(render::transform::POSITION).unwrap();
        for &(tick, (ox, oy)) in ticks_and_offsets {
            pos_track.upsert_keyframe(TimeTick(tick), ParamValue::Vec2(base.0 + ox, base.1 + oy), timeline::InterpolationMode::Linear);
        }

        self.undo.push("stabilize", std::sync::Arc::new(project));
        ticks_and_offsets.len()
    }

    // ---- Auto-captions ---------------------------------------------------

    /// Transcribes `clip_id`'s audio (local Whisper, via the `speech` crate)
    /// and places one title clip per segment on the topmost video track —
    /// creating a new one above if the top track is occupied, same "never
    /// overwrite existing material" rule `add_title_at_playhead` follows.
    /// All segments land as a single undo step. Returns how many captions
    /// were placed.
    pub fn generate_captions(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        // Checked before transcribing (not just before placing captions):
        // this method also caches the transcript into `self.transcripts`
        // for the transcript panel, and doing that ahead of this guard used
        // to let a refused (keyframed-speed) attempt silently overwrite a
        // real, previously-stored transcript for the same clip with an
        // empty one — see
        // `generate_captions_on_a_keyframed_clip_does_not_clobber_an_existing_transcript`.
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return 0 };
        let Some((mono, sample_rate)) = self.decode_clip_mono_audio(&clip) else { return 0 };

        let transcript = match speech::transcribe(&mono, 1, sample_rate, &speech::WhisperConfig::default()) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("transcription failed: {e}");
                return 0;
            }
        };
        if transcript.segments.is_empty() {
            return 0;
        }
        // Stored for the transcript panel too — it already did the work of
        // transcribing, so `transcribe_clip` doesn't need to run Whisper
        // again over the same audio just to get word-level ticks.
        let words = self.timeline_words_from_transcript(&clip, &transcript.segments);
        self.transcripts.insert(clip_id, words);

        // Reuses `add_title_at_playhead`'s track-selection rule (top track if
        // free, else a new one above) by targeting the first segment's own
        // span, then placing every other segment on whatever track that
        // resolved to — captions from one transcription run always belong
        // together on one track, not scattered across several.
        let source_span_start = transcript.segments[0].start_ms as f64 / 1000.0;
        let source_span_end = transcript.segments.last().unwrap().end_ms as f64 / 1000.0;
        let to_timeline_tick = |source_seconds: f64| -> i64 {
            let source_offset = (source_seconds * TIMEBASE as f64).round() as i64;
            clip.timeline_in.0 + source_offset * denominator / numerator
        };
        let whole_span_start = TimeTick(to_timeline_tick(source_span_start));
        let whole_span_end = TimeTick(to_timeline_tick(source_span_end));

        let top_video = self.sequence().tracks.iter().rfind(|t| t.kind == TrackKind::Video);
        let free = top_video.map(|t| {
            !t.clips
                .iter()
                .any(|c| c.timeline_in.0 < whole_span_end.0 && c.timeline_out.0 > whole_span_start.0)
        });
        let track = match (top_video.map(|t| t.id), free) {
            (Some(track), Some(true)) => track,
            _ => {
                let track_id = TrackId(self.next_id());
                let mut project = (**self.project()).clone();
                let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else { return 0 };
                seq.tracks.push(Track {
                    id: track_id,
                    kind: TrackKind::Video,
                    name: format!("V{}", track_id.0),
                    clips: vec![],
                    transitions: vec![],
                    gain_db: timeline::unity_gain(),
                    pan: 0.0,
                    locked: false,
                    sync_locked: true,
                    muted: false,
                    solo: false,
                    height_px: 60,
                });
                self.undo.push("add caption track", std::sync::Arc::new(project));
                track_id
            }
        };

        let ops = self.caption_ops(track, &clip, &transcript.segments);
        if ops.is_empty() {
            return 0;
        }
        if self.apply_ops("generate captions", &ops) {
            ops.len()
        } else {
            0
        }
    }

    /// The pure, testable half: turns already-transcribed segments into
    /// `Overwrite` ops placing one title clip per segment. Zero-duration
    /// segments are dropped rather than failing the whole batch. Default
    /// caption styling: centred, near the bottom of frame, white — a plain,
    /// legible default with no background box (`TitleSpec` has none to give
    /// it — see `render::text`'s known limits).
    fn caption_ops(&mut self, track: TrackId, clip: &ClipInstance, segments: &[speech::Segment]) -> Vec<EditOp> {
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return Vec::new() };
        let to_timeline_tick = |ms: u32| -> i64 {
            let source_offset = ((ms as f64 / 1000.0) * TIMEBASE as f64).round() as i64;
            clip.timeline_in.0 + source_offset * denominator / numerator
        };

        segments
            .iter()
            .filter(|s| s.end_ms > s.start_ms && !s.text.trim().is_empty())
            .map(|s| {
                let start = to_timeline_tick(s.start_ms);
                let end = to_timeline_tick(s.end_ms);
                let clip_id = ClipInstanceId(self.next_id());
                let spec = timeline::TitleSpec {
                    text: s.text.clone(),
                    size_px: 42.0,
                    position: (0.5, 0.88),
                    align: timeline::TextAlign::Center,
                    ..Default::default()
                };
                EditOp::Overwrite {
                    track,
                    at: TimeTick(start),
                    clip: ClipInstance {
                        id: clip_id,
                        source: ClipSource::Title(spec),
                        source_in: TimeTick(0),
                        source_out: TimeTick(end - start),
                        timeline_in: TimeTick(start),
                        timeline_out: TimeTick(end),
                        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                        effects: vec![],
                        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                        linked_group: None,
                    },
                }
            })
            .collect()
    }

    // ---- Transcript panel -------------------------------------------------

    /// Transcribes `clip_id`'s audio and stores the result in
    /// `self.transcripts` for `title_panel`'s transcript view — no timeline
    /// clips created, unlike `generate_captions`. Returns whether a
    /// transcript with at least one word was stored.
    pub fn transcribe_clip(&mut self, clip_id: ClipInstanceId) -> bool {
        let Some((_, clip)) = self.find_clip(clip_id) else { return false };
        let Some((mono, sample_rate)) = self.decode_clip_mono_audio(&clip) else { return false };
        let transcript = match speech::transcribe(&mono, 1, sample_rate, &speech::WhisperConfig::default()) {
            Ok(t) => t,
            Err(e) => {
                self.status = format!("transcription failed: {e}");
                return false;
            }
        };
        let words = self.timeline_words_from_transcript(&clip, &transcript.segments);
        let found = !words.is_empty();
        self.transcripts.insert(clip_id, words);
        found
    }

    /// Maps transcribed segments' words onto timeline ticks — the pure half,
    /// same speed-inversion and source-bounds reasoning as `caption_ops` and
    /// every other analysis action in this file. Words outside the clip's
    /// own source bounds are dropped rather than producing a tick outside
    /// the clip.
    fn timeline_words_from_transcript(&mut self, clip: &ClipInstance, segments: &[speech::Segment]) -> Vec<TimelineWord> {
        let SpeedCurve::Constant { numerator, denominator } = clip.speed else { return Vec::new() };
        let to_timeline_tick = |ms: u32| -> i64 {
            let source_offset = ((ms as f64 / 1000.0) * TIMEBASE as f64).round() as i64;
            clip.timeline_in.0 + source_offset * denominator / numerator
        };
        segments
            .iter()
            .flat_map(|s| &s.words)
            .filter(|w| w.end_ms > w.start_ms)
            .map(|w| TimelineWord { text: w.text.clone(), start_tick: to_timeline_tick(w.start_ms), end_tick: to_timeline_tick(w.end_ms) })
            .collect()
    }

    /// Ripple-deletes the timeline span covered by words
    /// `self.transcripts[clip_id][first_word..=last_word]` — the payoff of
    /// having a transcript at all: select a run of words, delete them, the
    /// video ripples to match. One undo step. Returns whether it happened.
    ///
    /// Scoped to *interior* selections, same reasoning as
    /// `silence_removal_ops`: a selection touching either edge of the clip
    /// is a trim (shorten the clip), not an isolate-and-extract — handling
    /// both shapes in one action is real, separate follow-up work.
    pub fn delete_word_range(&mut self, track: TrackId, clip_id: ClipInstanceId, first_word: usize, last_word: usize) -> bool {
        let Some(words) = self.transcripts.get(&clip_id) else { return false };
        if first_word > last_word || last_word >= words.len() {
            return false;
        }
        let (start_tick, end_tick) = (words[first_word].start_tick, words[last_word].end_tick);
        let Some((_, clip)) = self.find_clip(clip_id) else { return false };
        if start_tick <= clip.timeline_in.0 || end_tick >= clip.timeline_out.0 {
            return false;
        }

        let ops = self.timeline_range_removal_ops(track, start_tick, end_tick);
        if ops.is_empty() {
            return false;
        }
        self.apply_ops("delete transcript range", &ops)
    }

    /// Builds the `Razor`+`Razor`+`Extract` triple that isolates and
    /// ripple-deletes `[start_tick, end_tick)` on `track`. No clip or speed
    /// parameter needed — unlike `silence_removal_ops`/`caption_ops`, the
    /// caller already has timeline ticks, not source milliseconds, so
    /// there's no trim/speed arithmetic left to do here.
    fn timeline_range_removal_ops(&mut self, track: TrackId, start_tick: i64, end_tick: i64) -> Vec<EditOp> {
        if end_tick <= start_tick {
            return Vec::new();
        }
        let gap_clip_id = ClipInstanceId(self.next_id());
        let after_gap_id = ClipInstanceId(self.next_id());
        vec![
            EditOp::Razor { track, at: TimeTick(start_tick), new_clip_id: gap_clip_id },
            EditOp::Razor { track, at: TimeTick(end_tick), new_clip_id: after_gap_id },
            EditOp::Extract { clip: gap_clip_id },
        ]
    }

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
    fn with_clip_mut(
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

    /// Runs `f` against one parameter's track and pushes the result.
    fn with_param_mut(
        &mut self,
        label: &str,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        f: impl FnOnce(&mut timeline::ParamTrack),
    ) {
        if let Some(project) = self.with_clip_mut(clip_id, |clip| {
            if let Some(effect) = clip.effects.iter_mut().find(|e| e.id == effect_id) {
                if let Some(track) = effect.params.get_mut(param_name) {
                    f(track);
                }
            }
        }) {
            self.undo.push(label, std::sync::Arc::new(project));
        }
    }

    /// Turns animation on or off for a parameter, Premiere's stopwatch.
    ///
    /// Both directions deliberately preserve the value visible at `local`:
    /// enabling seeds a keyframe from the current constant, and disabling
    /// collapses to whatever the curve evaluated to right there. Otherwise
    /// toggling the stopwatch would make the picture jump — losing work in one
    /// direction and silently changing the frame in the other.
    pub fn toggle_param_animation(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
    ) {
        self.with_param_mut("toggle animation", clip_id, effect_id, param_name, |track| {
            if track.is_animated() {
                let held = track.evaluate_at(local);
                track.keyframes.clear();
                track.default = held;
            } else {
                let seed = track.default;
                track.upsert_keyframe(local, seed, timeline::InterpolationMode::Linear);
            }
        });
    }

    /// Adds a keyframe at `local` holding the parameter's current value there,
    /// or removes the one already at that exact time.
    pub fn toggle_keyframe(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
    ) {
        self.with_param_mut("toggle keyframe", clip_id, effect_id, param_name, |track| {
            if track.keyframe_index_at(local).is_some() {
                track.remove_keyframe_at(local);
            } else {
                let value = track.evaluate_at(local);
                track.upsert_keyframe(local, value, timeline::InterpolationMode::Linear);
            }
        });
    }

    pub fn set_keyframe_interpolation(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
        mode: timeline::InterpolationMode,
    ) {
        self.with_param_mut("set interpolation", clip_id, effect_id, param_name, |track| {
            track.set_interpolation_at(local, mode);
        });
    }

    /// Sets a keyframe's explicit bezier tangent handles, in (time-ticks-delta,
    /// value-delta) space as `Keyframe::tangents` stores them.
    pub fn set_keyframe_tangents(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        local: TimeTick,
        tangents: ((f64, f64), (f64, f64)),
    ) {
        self.with_param_mut("edit tangent", clip_id, effect_id, param_name, |track| {
            track.set_tangents_at(local, tangents);
        });
    }

    pub fn move_keyframe(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        from: TimeTick,
        to: TimeTick,
    ) {
        self.with_param_mut("move keyframe", clip_id, effect_id, param_name, |track| {
            track.move_keyframe(from, to);
        });
    }

    /// Writes a parameter value the way an NLE does: to the keyframe at the
    /// playhead when the parameter is animated, to the constant otherwise.
    ///
    /// This is the whole point of the keyframe UI — without the animated
    /// branch, editing a slider on an animated parameter would change
    /// `default`, which `evaluate_at` ignores entirely whenever any keyframe
    /// exists. The edit would appear to do nothing.
    pub fn set_param_value_at(
        &mut self,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
        param_name: &str,
        value: ParamValue,
        local: Option<TimeTick>,
        coalescing: bool,
    ) {
        let project = self.with_clip_mut(clip_id, |clip| {
            if let Some(effect) = clip.effects.iter_mut().find(|e| e.id == effect_id) {
                if let Some(track) = effect.params.get_mut(param_name) {
                    match (track.is_animated(), local) {
                        (true, Some(local)) => track.upsert_keyframe(
                            local,
                            value,
                            timeline::InterpolationMode::Linear,
                        ),
                        // Animated but the playhead is off the clip: there's no
                        // keyframe time to write to, so leave the curve alone
                        // rather than silently editing an arbitrary one.
                        (true, None) => {}
                        (false, _) => track.default = value,
                    }
                }
            }
        });
        let Some(project) = project else { return };
        if coalescing {
            self.undo.update_coalescing(std::sync::Arc::new(project));
        } else {
            self.undo.push("edit effect", std::sync::Arc::new(project));
        }
    }

    pub fn import_assets(&mut self, paths: Vec<PathBuf>) {
        let mut project = (**self.project()).clone();
        let mut imported = 0;
        for path in paths {
            match media_ffmpeg::probe(&path) {
                Ok(asset) => {
                    self.asset_paths.insert(asset.id, path);
                    project.assets.push(asset);
                    imported += 1;
                }
                Err(e) => {
                    self.status = format!("failed to import {path:?}: {e:?}");
                }
            }
        }
        if imported > 0 {
            self.selected_item = project
                .assets
                .last()
                .map(|a| timeline::BinItem::Asset(a.id));
            self.undo.push("import media", std::sync::Arc::new(project));
        }
    }

    /// Finds (or lazily creates, via `apply_op`) the first track of `kind`,
    /// returning its id. Real NLEs let you manage tracks explicitly; this
    /// is the v1 stand-in — one V track and one A track, created on first
    /// use, rather than a full "insert track" UI.
    fn ensure_track(&mut self, kind: TrackKind) -> TrackId {
        if let Some(t) = self.sequence().tracks.iter().find(|t| t.kind == kind) {
            return t.id;
        }
        let track_id = TrackId(self.next_id());
        let mut project = (**self.project()).clone();
        let seq = project
            .sequences
            .iter_mut()
            .find(|s| s.id == self.seq_id)
            .unwrap();
        let name = match kind {
            TrackKind::Video => format!(
                "V{}",
                seq.tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Video)
                    .count()
                    + 1
            ),
            TrackKind::Audio => format!(
                "A{}",
                seq.tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Audio)
                    .count()
                    + 1
            ),
        };
        seq.tracks.push(Track {
            id: track_id,
            kind,
            name,
            clips: vec![], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
        self.undo.push("add track", std::sync::Arc::new(project));
        track_id
    }

    /// Appends `asset` to the end of its kind's track (video assets with an
    /// audio stream get a linked audio clip too, mirroring how a real NLE
    /// treats a camera file as one linked A/V unit).
    pub fn append_asset_to_timeline(&mut self, asset_id: media::MediaAssetId) {
        let Some(asset) = self
            .project()
            .assets
            .iter()
            .find(|a| a.id == asset_id)
            .cloned()
        else {
            return;
        };
        let duration = TimeTick(asset.duration_ticks);
        self.adopt_settings_from_first_clip(&asset);

        if asset.video.is_some() {
            let track = self.ensure_track(TrackKind::Video);
            let at = self
                .sequence()
                .tracks
                .iter()
                .find(|t| t.id == track)
                .unwrap()
                .duration_end();
            let clip_id = ClipInstanceId(self.next_id());
            let clip = new_clip(clip_id, asset.id, duration);
            self.apply_op("append clip", EditOp::Overwrite { track, at, clip });
        }
        if asset.audio.is_some() {
            let track = self.ensure_track(TrackKind::Audio);
            let at = self
                .sequence()
                .tracks
                .iter()
                .find(|t| t.id == track)
                .unwrap()
                .duration_end();
            let clip_id = ClipInstanceId(self.next_id());
            let clip = new_clip(clip_id, asset.id, duration);
            self.apply_op("append audio clip", EditOp::Overwrite { track, at, clip });
        }
    }

    // --- Project panel: bins ---------------------------------------------
    //
    // Bin edits go through the undo stack like every other change, so
    // organising media is undoable and a mis-drop is one Ctrl+Z away. They
    // clone-mutate-push directly rather than going through
    // `timeline::edit_ops`, for the same reason effect edits do: the
    // invariants that op set enforces are about clip positions on tracks,
    // and none of them apply to folder membership.

    /// Creates a bin under `parent` and returns its id.
    pub fn create_bin(&mut self, name: &str, parent: Option<timeline::BinId>) -> timeline::BinId {
        let id = timeline::BinId(self.next_id());
        let mut project = (**self.project()).clone();
        project.bins.push(timeline::Bin {
            id,
            name: name.to_string(),
            parent,
            items: Vec::new(),
        });
        self.undo.push("new bin", std::sync::Arc::new(project));
        id
    }

    pub fn rename_bin(&mut self, id: timeline::BinId, name: &str) {
        let mut project = (**self.project()).clone();
        if let Some(bin) = project.bins.iter_mut().find(|b| b.id == id) {
            if bin.name == name {
                return; // no-op; don't add an undo step for it
            }
            bin.name = name.to_string();
            self.undo.push("rename bin", std::sync::Arc::new(project));
        }
    }

    /// Moves `item` into `target` (or to the root when `None`).
    pub fn move_item_to_bin(&mut self, item: timeline::BinItem, target: Option<timeline::BinId>) {
        if self.project().bin_of(item) == target {
            return;
        }
        let mut project = (**self.project()).clone();
        // Remove from wherever it currently is first — an item belongs to
        // exactly one bin, and letting it appear in two would make
        // `root_items` and `bin_of` disagree about where it lives.
        for bin in &mut project.bins {
            bin.items.retain(|i| *i != item);
        }
        if let Some(target) = target {
            if let Some(bin) = project.bins.iter_mut().find(|b| b.id == target) {
                bin.items.push(item);
            }
        }
        self.undo.push("move to bin", std::sync::Arc::new(project));
    }

    /// Reparents a bin, refusing moves that would create a cycle.
    pub fn move_bin(&mut self, bin: timeline::BinId, new_parent: Option<timeline::BinId>) {
        if let Some(parent) = new_parent {
            if self.project().is_descendant_of(parent, bin) {
                self.status = "can't move a bin into itself".into();
                return;
            }
        }
        let mut project = (**self.project()).clone();
        if let Some(b) = project.bins.iter_mut().find(|b| b.id == bin) {
            if b.parent == new_parent {
                return;
            }
            b.parent = new_parent;
            self.undo.push("move bin", std::sync::Arc::new(project));
        }
    }

    /// Deletes a bin, promoting its contents to the bin's own parent rather
    /// than deleting them. Removing media from the project is a separate,
    /// much more destructive action — a folder delete that silently took the
    /// footage with it would be a data-loss trap.
    pub fn delete_bin(&mut self, id: timeline::BinId) {
        let mut project = (**self.project()).clone();
        let Some(index) = project.bins.iter().position(|b| b.id == id) else {
            return;
        };
        let removed = project.bins.remove(index);
        let grandparent = removed.parent;
        for child in project.bins.iter_mut().filter(|b| b.parent == Some(id)) {
            child.parent = grandparent;
        }
        // A `None` grandparent means the items are at the root by
        // definition, so dropping them from the tree is all that's needed.
        if let Some(parent_id) = grandparent {
            if let Some(parent) = project.bins.iter_mut().find(|b| b.id == parent_id) {
                parent.items.extend(removed.items);
            }
        }
        self.undo.push("delete bin", std::sync::Arc::new(project));
    }

    /// Display name for a Project-panel item.
    pub fn item_name(&self, item: timeline::BinItem) -> String {
        match item {
            timeline::BinItem::Asset(id) => self
                .project()
                .assets
                .iter()
                .find(|a| a.id == id)
                .and_then(|a| std::path::Path::new(&a.original_absolute_path).file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "(missing asset)".into()),
            timeline::BinItem::Sequence(id) => self
                .project()
                .sequences
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.name.clone())
                .unwrap_or_else(|| "(missing sequence)".into()),
        }
    }

    /// When the very first clip lands on an empty timeline, resize the
    /// sequence to match it — what Premiere and Resolve both offer on the
    /// first drop, and for the same reason: the default 1920x1080 sequence
    /// otherwise renders a 640x360 phone clip as a small island in a large
    /// black frame, both in the preview and in the exported file. Adopting
    /// the source's dimensions makes the common case (one camera format)
    /// right by default, and only applies while nothing is on the timeline
    /// yet, so it can never silently re-frame an edit in progress.
    fn adopt_settings_from_first_clip(&mut self, asset: &media::MediaAsset) {
        let already_has_clips = self.sequence().tracks.iter().any(|t| !t.clips.is_empty());
        if already_has_clips {
            return;
        }
        let Some(video) = &asset.video else { return };
        let rate = standard_frame_rate(&video.frame_rate);
        let mut project = (**self.project()).clone();
        let Some(seq) = project.sequences.iter_mut().find(|s| s.id == self.seq_id) else {
            return;
        };
        if seq.settings.width == video.width
            && seq.settings.height == video.height
            && rate.is_none_or(|r| r == seq.settings.frame_rate)
        {
            return; // nothing to change; don't push a no-op undo step
        }
        seq.settings.width = video.width;
        seq.settings.height = video.height;
        if let Some(rate) = rate {
            seq.settings.frame_rate = rate;
            seq.settings.drop_frame_timecode = rate.is_drop_frame_by_default();
        }
        self.undo
            .push("match sequence to clip", std::sync::Arc::new(project));
    }

    /// Saves to `path` via `project::save` (atomic write-temp-fsync-rename,
    /// already implemented and tested in the `project` crate).
    ///
    /// Media references carry the content hash and both a relative and the
    /// original absolute path, per spec 4.7, so a moved project folder can
    /// still relink by hash. `relative_path` is computed against the project
    /// file's own directory when the media lives at or below it, which is
    /// the case that actually benefits from relative paths (a
    /// self-contained project folder you can move or copy wholesale);
    /// media elsewhere on disk falls back to its absolute path, since
    /// inventing a `../../..` chain across volumes wouldn't survive a move
    /// either and would just obscure where the file really is.
    /// Writes the project to `path` **without** claiming it as the project's
    /// own location or touching the status line.
    ///
    /// Autosave uses this: adopting the recovery file as `project_path` would
    /// mean the next Ctrl+S silently saved over the recovery file instead of the
    /// user's project, and a status line churning every 20 seconds would bury
    /// whatever the user was actually being told.
    pub fn write_snapshot_to(&self, path: &std::path::Path) -> Result<(), String> {
        // No undo history in a recovery write. `docs/decisions-log.md` bounds
        // history specifically to keep autosave size predictable; excluding it
        // outright is stricter and cheaper still, because serialising N project
        // snapshots every 20 seconds would make the cost of autosave scale with
        // how long you'd been working — the opposite of what a background safety
        // net should do. A crash therefore loses undo history but not work.
        let doc = self.build_document(path, false);
        project::save(&doc, path).map_err(|e| format!("{e:?}"))
    }

    /// Builds the on-disk document, with media references made relative to
    /// `path`'s directory.
    ///
    /// `include_history` persists the undo stack (spec 4.3 /
    /// `docs/decisions-log.md` 2026-08-07: undo history is persisted from v1.0,
    /// bounded by `UndoStack::max_history`).
    fn build_document(
        &self,
        path: &std::path::Path,
        include_history: bool,
    ) -> project::ProjectDocument {
        let project = (**self.project()).clone();
        let base_dir = path.parent();
        let media_references = project
            .assets
            .iter()
            .map(|a| {
                let abs = std::path::Path::new(&a.original_absolute_path);
                let relative_path = base_dir
                    .and_then(|dir| abs.strip_prefix(dir).ok())
                    .map(|rel| rel.to_string_lossy().into_owned())
                    .unwrap_or_else(|| a.original_absolute_path.clone());
                project::MediaReference {
                    asset_id: a.id,
                    relative_path,
                    content_hash: a.content_hash,
                    original_absolute_path: a.original_absolute_path.clone(),
                }
            })
            .collect();
        let mut doc = project::ProjectDocument::new(project, media_references);
        if include_history {
            // Only the most recent entries, and only each one's `before` — the
            // `after` is recoverable from the next entry (see
            // `project::PersistedCommand`). Together these took a 40-clip
            // project's file from 199x its bare size down to a few times it.
            let history = self.undo.history();
            let keep = history.len().saturating_sub(project::MAX_PERSISTED_UNDO_ENTRIES);
            doc.undo_history = history[keep..]
                .iter()
                .map(|e| project::PersistedCommand {
                    label: e.label.clone(),
                    before: (*e.before).clone(),
                })
                .collect();
        }
        doc
    }

    pub fn save_to(&mut self, path: &std::path::Path) {
        let doc = self.build_document(path, true);
        match project::save(&doc, path) {
            Ok(()) => {
                self.project_path = Some(path.to_path_buf());
                self.status = format!("saved to {}", path.display());
            }
            Err(e) => self.status = format!("save failed: {e:?}"),
        }
    }

    /// Loads a project, replacing all current state. Returns false (and
    /// leaves the editor untouched) if the file can't be read — an
    /// unreadable file must not destroy whatever the user already has open.
    pub fn open_from(&mut self, path: &std::path::Path) -> bool {
        let doc = match project::load(path) {
            Ok(doc) => doc,
            Err(e) => {
                self.status = format!("open failed: {e:?}");
                return false;
            }
        };

        // Resolve each asset back to a real file so the preview can decode
        // it: try the saved relative path against this file's directory
        // first (so a moved-but-self-contained project folder just works),
        // then the recorded absolute path. Missing media isn't fatal —
        // the project still opens, those clips just render as gaps, which
        // is the honest outcome and matches what "relink media" exists to
        // fix. Hash-based relinking (searching a directory for a matching
        // content_hash) is the real fix and isn't built yet.
        let base_dir = path.parent();
        let mut missing = 0;
        self.asset_paths.clear();
        // A freshly loaded project's clip ids restart from a low number
        // (see `next_id` below), so a transcript cached against the
        // previous project's clip ids could otherwise silently attach
        // itself to an unrelated clip that happens to reuse the same id.
        self.transcripts.clear();
        for r in &doc.media_references {
            let candidate = base_dir
                .map(|d| d.join(&r.relative_path))
                .filter(|p| p.exists())
                .or_else(|| {
                    let abs = PathBuf::from(&r.original_absolute_path);
                    abs.exists().then_some(abs)
                });
            match candidate {
                Some(p) => {
                    self.asset_paths.insert(r.asset_id, p);
                }
                None => missing += 1,
            }
        }

        // Every ID in the loaded project must be below `next_id`, or newly
        // created clips/tracks would collide with existing ones — a
        // collision would make `find_clip`-style lookups match the wrong
        // object and corrupt edits in ways that are painful to trace back
        // to their cause.
        self.next_id = max_id_in(&doc.project) + 1;

        self.seq_id = doc
            .project
            .sequences
            .first()
            .map(|s| s.id)
            .unwrap_or(SequenceId(1));
        // Restore the undo stack, so reopening a project doesn't silently lose
        // the ability to undo the work in it (spec 4.3 /
        // `docs/decisions-log.md` 2026-08-07).
        //
        // The chain is *reconstructed* rather than read verbatim: each entry's
        // `after` is the next entry's `before`, and the last one's is the
        // project itself. Schema v3 stored `after` too and this code validated
        // the two against each other, dropping the history when they disagreed.
        // Not storing it is better than checking it — the inconsistency it
        // guarded against is now unrepresentable, and the file is half the size.
        let current = std::sync::Arc::new(doc.project);
        let befores: Vec<std::sync::Arc<Project>> = doc
            .undo_history
            .iter()
            .map(|c| std::sync::Arc::new(c.before.clone()))
            .collect();
        let history: Vec<command::CommandEntry> = doc
            .undo_history
            .iter()
            .enumerate()
            .map(|(i, c)| command::CommandEntry {
                label: c.label.clone(),
                before: befores[i].clone(),
                after: befores.get(i + 1).cloned().unwrap_or_else(|| current.clone()),
            })
            .collect();
        self.undo = command::UndoStack::restore(
            current,
            history,
            command::UndoStack::DEFAULT_MAX_HISTORY,
        );
        self.playhead = 0;
        self.selected_clips.clear();
        self.selected_item = None;
        self.playing = false;
        self.play_anchor = None;
        self.drag = None;
        self.coalescing_open = false;
        self.scroll_ticks = 0;
        self.project_path = Some(path.to_path_buf());
        // The "undo history was discarded as inconsistent" branches are gone
        // with schema v4: a reconstructed chain cannot disagree with its
        // project, so there is no such outcome to report.
        self.status = match missing {
            0 => format!("opened {}", path.display()),
            n => format!("opened {} ({n} media file(s) not found)", path.display()),
        };
        true
    }
}

/// Converts an RGBA frame to a box-downsampled Rec.709 luma buffer, `factor`
/// pixels averaged into one, for `EditorState::stabilize_clip`'s motion
/// search. Downsampling isn't just a speed optimisation there: block
/// matching's cost is `O(search_window^2 * pixel_count)`, so running it at
/// full 1080p/4K resolution would make one stabilization pass minutes long;
/// shake big enough to need correcting is still visible at 1/8 resolution.
fn downsample_luma(rgba: &[u8], width: usize, height: usize, factor: usize) -> Vec<f32> {
    let (dw, dh) = (width.div_ceil(factor), height.div_ceil(factor));
    let mut out = vec![0f32; dw * dh];
    for by in 0..dh {
        for bx in 0..dw {
            let mut sum = 0f32;
            let mut count = 0u32;
            for yy in 0..factor {
                let sy = by * factor + yy;
                if sy >= height {
                    break;
                }
                for xx in 0..factor {
                    let sx = bx * factor + xx;
                    if sx >= width {
                        break;
                    }
                    let i = (sy * width + sx) * 4;
                    let (r, g, b) = (rgba[i] as f32 / 255.0, rgba[i + 1] as f32 / 255.0, rgba[i + 2] as f32 / 255.0);
                    sum += render::scopes::luma_rec709(r, g, b);
                    count += 1;
                }
            }
            out[by * dw + bx] = sum / count.max(1) as f32;
        }
    }
    out
}

/// Maps a probed source rate onto one of `timeline::FrameRate`'s named
/// standard rates, or `None` if it isn't one.
///
/// Sequences are deliberately restricted to standard rates (see
/// `FrameRate`'s own doc comment) — a screen recorder's 47.113fps average is
/// a fact about the *source*, not a sensible sequence rate, and adopting it
/// would make every timecode and tick calculation in the sequence odd. VFR
/// sources are rejected outright for the same reason: their nominal rate is
/// explicitly display-only.
fn standard_frame_rate(kind: &media::FrameRateKind) -> Option<timeline::FrameRate> {
    let media::FrameRateKind::Constant(r) = kind else {
        return None;
    };
    [
        timeline::FrameRate::Fps23_976,
        timeline::FrameRate::Fps24,
        timeline::FrameRate::Fps25,
        timeline::FrameRate::Fps29_97,
        timeline::FrameRate::Fps30,
        timeline::FrameRate::Fps50,
        timeline::FrameRate::Fps59_94,
        timeline::FrameRate::Fps60,
    ]
    .into_iter()
    // Compare as a cross-multiplied ratio rather than by exact
    // numerator/denominator: a 30fps source can legitimately be probed as
    // 30/1, 60000/2000, or 15360/512, and all three mean the same rate.
    .find(|std| {
        let (n, d) = std.as_rational();
        n as u64 * r.den as u64 == d as u64 * r.num as u64
    })
}

/// Highest ID used anywhere in `project`, across every kind of ID that
/// `EditorState::next_id` hands out (sequences, tracks, clips, effects).
/// They share one counter, so the ceiling has to consider all of them.
fn max_id_in(project: &Project) -> u64 {
    let mut max = 0;
    for seq in &project.sequences {
        max = max.max(seq.id.0);
        for track in &seq.tracks {
            max = max.max(track.id.0);
            for clip in &track.clips {
                max = max.max(clip.id.0);
                for effect in &clip.effects {
                    max = max.max(effect.id.0);
                }
            }
        }
        for marker in &seq.markers {
            max = max.max(marker.id.0);
        }
    }
    max
}

fn new_clip(id: ClipInstanceId, asset: media::MediaAssetId, duration: TimeTick) -> ClipInstance {
    ClipInstance {
        id,
        source: ClipSource::Media(asset),
        source_in: TimeTick(0),
        source_out: duration,
        timeline_in: TimeTick(0), // overwritten positionally by the caller via EditOp::Overwrite::at
        timeline_out: duration,
        speed: SpeedCurve::Constant {
            numerator: 1,
            denominator: 1,
        },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    }
}

trait TrackExt {
    fn duration_end(&self) -> TimeTick;
}

impl TrackExt for Track {
    fn duration_end(&self) -> TimeTick {
        self.clips
            .last()
            .map(|c| c.timeline_out)
            .unwrap_or(TimeTick(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_asset(id: u128, path: &str) -> media::MediaAsset {
        media::MediaAsset {
            id: media::MediaAssetId(id),
            original_absolute_path: path.into(),
            content_hash: [7u8; 32],
            container_format: "mp4".into(),
            video: None,
            audio: None,
            duration_ticks: TIMEBASE,
        }
    }

    /// Builds a state whose project has deliberately high IDs, so a loader
    /// that resets `next_id` to 1 would immediately collide.
    fn state_with_high_ids() -> EditorState {
        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        let seq = &mut project.sequences[0];
        seq.tracks.push(Track {
            id: TrackId(500),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(900),
                source: ClipSource::Media(media::MediaAssetId(1)),
                source_in: TimeTick(0),
                source_out: TimeTick(TIMEBASE),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(TIMEBASE),
                speed: SpeedCurve::Constant {
                    numerator: 1,
                    denominator: 1,
                },
                effects: vec![timeline::EffectInstance {
                    id: timeline::EffectInstanceId(1234),
                    effect_type: "gaussian_blur".into(),
                    enabled: true,
                    params: Default::default(),
                }],
                audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                linked_group: None,
            }], transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
        project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
        state.undo.push("setup", std::sync::Arc::new(project));
        state
    }

    fn video_asset(id: u128, w: u32, h: u32, fps: (u32, u32)) -> media::MediaAsset {
        let mut a = fake_asset(id, "C:/media/clip.mp4");
        a.video = Some(media::VideoStreamInfo {
            width: w,
            height: h,
            pixel_format: media::PixelFormat::Yuv420p8,
            color: media::ColorMetadata {
                primaries: media::ColorPrimaries::Rec709,
                transfer: media::TransferFunction::Bt709,
                matrix: media::MatrixCoefficients::Bt709,
                full_range: false,
            },
            frame_rate: media::FrameRateKind::Constant(media::Rational {
                num: fps.0,
                den: fps.1,
            }),
            start_timecode: None,
            keyframe_index: vec![],
            pts_index: vec![],
        });
        a
    }

    #[test]
    fn first_clip_resizes_the_sequence_to_match_it() {
        // Otherwise a 640x360 clip renders as a small island inside the
        // default 1920x1080 frame, in both the preview and the export.
        let mut state = EditorState::new();
        assert_eq!(
            (
                state.sequence().settings.width,
                state.sequence().settings.height
            ),
            (1920, 1080)
        );

        let asset = video_asset(1, 640, 360, (25, 1));
        let mut project = (**state.project()).clone();
        project.assets.push(asset.clone());
        state.undo.push("import", std::sync::Arc::new(project));
        state.append_asset_to_timeline(asset.id);

        let settings = &state.sequence().settings;
        assert_eq!((settings.width, settings.height), (640, 360));
        assert_eq!(settings.frame_rate, timeline::FrameRate::Fps25);
    }

    #[test]
    fn a_second_clip_does_not_re_frame_an_edit_in_progress() {
        let mut state = EditorState::new();
        let first = video_asset(1, 640, 360, (30, 1));
        let second = video_asset(2, 1920, 1080, (60, 1));
        let mut project = (**state.project()).clone();
        project.assets.push(first.clone());
        project.assets.push(second.clone());
        state.undo.push("import", std::sync::Arc::new(project));

        state.append_asset_to_timeline(first.id);
        state.append_asset_to_timeline(second.id);

        let settings = &state.sequence().settings;
        assert_eq!(
            (settings.width, settings.height),
            (640, 360),
            "the sequence must keep the format it adopted from the first clip"
        );
        assert_eq!(settings.frame_rate, timeline::FrameRate::Fps30);
    }

    #[test]
    fn non_standard_and_variable_source_rates_do_not_become_sequence_rates() {
        use media::{FrameRateKind, Rational};
        // A 47.113fps screen recording is a fact about the source, not a
        // sensible sequence rate — adopting it would make every timecode in
        // the sequence non-standard.
        assert_eq!(
            standard_frame_rate(&FrameRateKind::Constant(Rational {
                num: 47113,
                den: 1000
            })),
            None
        );
        // VFR's nominal rate is explicitly display-only.
        assert_eq!(
            standard_frame_rate(&FrameRateKind::Variable {
                nominal: Rational { num: 30, den: 1 }
            }),
            None
        );
        // Equivalent-ratio spellings of a standard rate must still match.
        assert_eq!(
            standard_frame_rate(&FrameRateKind::Constant(Rational {
                num: 60000,
                den: 2000
            })),
            Some(timeline::FrameRate::Fps30)
        );
        assert_eq!(
            standard_frame_rate(&FrameRateKind::Constant(Rational {
                num: 30000,
                den: 1001
            })),
            Some(timeline::FrameRate::Fps29_97)
        );
    }

    /// End-to-end over the path a user actually walks: import a real file,
    /// add it to the timeline, export, and check the result fills the frame
    /// at the source's native size.
    ///
    /// Lives here rather than in `tests/` because `app` is a binary crate,
    /// so integration tests can't reach its modules. It's the combination
    /// that matters: before the sequence adopted the first clip's format,
    /// both halves were individually correct and still produced a 640x360
    /// picture stranded inside a 1920x1080 black frame.
    /// Guards the invariant the drag-and-drop batching in `main.rs` relies
    /// on: one call, one undo step, however many files came in. winit fires a
    /// separate `DroppedFile` event per file, so the event loop accumulates
    /// them and flushes once — if this ever became one undo entry per asset,
    /// a 10-file drop would silently need 10 undos to back out.
    #[test]
    fn importing_several_files_at_once_is_a_single_undo_step() {
        media_ffmpeg::init().unwrap();
        let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("test_fixtures");
        let mut state = EditorState::new();
        let before = state.undo.history().len();

        state.import_assets(vec![
            fixtures.join("test_playback_demo.mp4"),
            fixtures.join("test_h264.mp4"),
        ]);

        assert_eq!(state.project().assets.len(), 2, "both files should import");
        assert_eq!(
            state.undo.history().len() - before,
            1,
            "a multi-file import must be one undoable action, not one per file"
        );
    }

    #[test]
    fn import_add_export_fills_the_frame_at_native_resolution() {
        media_ffmpeg::init().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("test_fixtures")
            .join("test_playback_demo.mp4"); // 640x360
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out.mp4");

        let mut state = EditorState::new();
        state.import_assets(vec![source.clone()]);
        let asset_id = state.project().assets[0].id;
        state.append_asset_to_timeline(asset_id);

        let settings = &state.sequence().settings;
        assert_eq!(
            (settings.width, settings.height),
            (640, 360),
            "sequence should have adopted the imported clip's format"
        );

        let stats = export::export_sequence(
            state.project(),
            state.seq_id,
            &state.asset_paths,
            &out,
            &export::ExportOptions {
                quality: export::QualityPreset::Draft,
                ..Default::default()
            },
            |_, _| true,
        )
        .expect("export should succeed");
        assert_eq!(stats.frames_with_missing_sources, 0);
        assert_eq!((stats.width, stats.height), (640, 360));

        // Fills the frame: compare the exported frame's left/right edge
        // columns against the source's. Letterboxing shows up as black
        // borders the source doesn't have — the exact regression this
        // guards, and one a dimensions-only assertion would miss.
        let mid = timeline::TIMEBASE / 2;
        let exported = media_ffmpeg::decode_frame_at(&out, mid).unwrap();
        let src = media_ffmpeg::decode_frame_at(&source, mid).unwrap();
        let edge_mean = |f: &media_ffmpeg::DecodedRgbaFrame, left: bool| -> f64 {
            let w = f.width as usize;
            let (mut total, mut count) = (0u64, 0u64);
            for y in 0..f.height as usize {
                let p = (y * w + if left { 0 } else { w - 1 }) * 4;
                total += f.rgba[p] as u64 + f.rgba[p + 1] as u64 + f.rgba[p + 2] as u64;
                count += 3;
            }
            total as f64 / count as f64
        };
        for left in [true, false] {
            let got = edge_mean(&exported, left);
            let want = edge_mean(&src, left);
            assert!(
                (got - want).abs() < 40.0,
                "exported {} edge (mean {got:.1}) should match the source's ({want:.1}) — a large \
                 gap means the picture was letterboxed instead of filling the frame",
                if left { "left" } else { "right" }
            );
        }
    }

    // --- bins ------------------------------------------------------------

    /// A state with two imported assets, ready to be filed into bins.
    fn state_with_two_assets() -> EditorState {
        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        project.assets.push(fake_asset(1, "C:/media/a.mp4"));
        project.assets.push(fake_asset(2, "C:/media/b.mp4"));
        state.undo.push("import", std::sync::Arc::new(project));
        state
    }

    #[test]
    fn a_new_bin_starts_empty_and_everything_stays_at_the_root() {
        let mut state = state_with_two_assets();
        let bin = state.create_bin("Footage", None);

        assert_eq!(state.project().child_bins(None).len(), 1);
        assert_eq!(
            state.project().root_items().len(),
            3,
            "2 assets + 1 sequence still at root"
        );
        assert!(state
            .project()
            .bins
            .iter()
            .find(|b| b.id == bin)
            .unwrap()
            .items
            .is_empty());
    }

    #[test]
    fn moving_an_item_into_a_bin_removes_it_from_the_root() {
        let mut state = state_with_two_assets();
        let bin = state.create_bin("Footage", None);
        let item = timeline::BinItem::Asset(media::MediaAssetId(1));

        state.move_item_to_bin(item, Some(bin));

        assert_eq!(state.project().bin_of(item), Some(bin));
        assert!(!state.project().root_items().contains(&item));
        // And back out again.
        state.move_item_to_bin(item, None);
        assert_eq!(state.project().bin_of(item), None);
        assert!(state.project().root_items().contains(&item));
    }

    #[test]
    fn an_item_can_only_be_in_one_bin_at_a_time() {
        // Otherwise `bin_of` and `root_items` would disagree about where an
        // item lives, and the panel would draw it twice.
        let mut state = state_with_two_assets();
        let a = state.create_bin("A", None);
        let b = state.create_bin("B", None);
        let item = timeline::BinItem::Asset(media::MediaAssetId(1));

        state.move_item_to_bin(item, Some(a));
        state.move_item_to_bin(item, Some(b));

        let holding: Vec<u64> = state
            .project()
            .bins
            .iter()
            .filter(|bin| bin.items.contains(&item))
            .map(|bin| bin.id.0)
            .collect();
        assert_eq!(
            holding,
            vec![b.0],
            "only the destination bin should hold it"
        );
    }

    #[test]
    fn deleting_a_bin_keeps_its_contents_and_promotes_child_bins() {
        // The data-loss guard: a folder delete must never take the footage
        // with it.
        let mut state = state_with_two_assets();
        let parent = state.create_bin("Parent", None);
        let child = state.create_bin("Child", Some(parent));
        let item = timeline::BinItem::Asset(media::MediaAssetId(1));
        state.move_item_to_bin(item, Some(child));

        state.delete_bin(child);

        assert!(
            state.project().bins.iter().all(|b| b.id != child),
            "bin is gone"
        );
        assert_eq!(
            state.project().bin_of(item),
            Some(parent),
            "its contents should move up to the parent, not vanish"
        );
        assert!(
            state
                .project()
                .assets
                .iter()
                .any(|a| a.id == media::MediaAssetId(1)),
            "the asset itself must still be in the project"
        );
    }

    #[test]
    fn deleting_a_top_level_bin_returns_its_items_to_the_root() {
        let mut state = state_with_two_assets();
        let bin = state.create_bin("Temp", None);
        let item = timeline::BinItem::Asset(media::MediaAssetId(1));
        state.move_item_to_bin(item, Some(bin));

        state.delete_bin(bin);

        assert_eq!(state.project().bin_of(item), None);
        assert!(state.project().root_items().contains(&item));
    }

    #[test]
    fn a_bin_cannot_be_moved_into_its_own_subtree() {
        // Would make the panel's recursive tree walk infinite.
        let mut state = state_with_two_assets();
        let parent = state.create_bin("Parent", None);
        let child = state.create_bin("Child", Some(parent));

        state.move_bin(parent, Some(child));

        assert_eq!(
            state
                .project()
                .bins
                .iter()
                .find(|b| b.id == parent)
                .unwrap()
                .parent,
            None,
            "the illegal move should have been refused"
        );
        assert!(
            !state.status.is_empty(),
            "and the refusal should be visible to the user"
        );
    }

    #[test]
    fn bin_edits_are_undoable() {
        let mut state = state_with_two_assets();
        let bin = state.create_bin("Footage", None);
        let item = timeline::BinItem::Asset(media::MediaAssetId(1));
        state.move_item_to_bin(item, Some(bin));
        assert_eq!(state.project().bin_of(item), Some(bin));

        state.undo.undo(); // un-move
        assert_eq!(state.project().bin_of(item), None);
        state.undo.undo(); // un-create
        assert!(state.project().bins.is_empty());
        state.undo.redo();
        assert_eq!(state.project().bins.len(), 1);
    }

    #[test]
    fn renaming_a_bin_to_the_same_name_adds_no_undo_step() {
        let mut state = state_with_two_assets();
        let bin = state.create_bin("Footage", None);
        state.rename_bin(bin, "Footage");

        // One undo returns to "no bins" — proving the no-op rename didn't
        // push a step of its own that would need undoing first.
        state.undo.undo();
        assert!(state.project().bins.is_empty());
    }

    #[test]
    fn save_load_round_trips_the_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.nleproj");
        let mut state = state_with_high_ids();
        let before = (**state.project()).clone();

        state.save_to(&path);
        assert_eq!(state.project_path.as_deref(), Some(path.as_path()));

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        assert_eq!(**fresh.project(), before);
    }

    #[test]
    fn open_from_clears_stale_transcripts_from_the_previous_project() {
        // A freshly loaded project's clip ids restart from a low number
        // (see `loading_advances_next_id_past_every_existing_id`'s sibling
        // guarantee below), so they can easily collide with ids left over
        // from whatever project was open before. A leftover transcript
        // entry surviving that switch would silently attach itself to an
        // unrelated clip in the new project — wrong words shown, and
        // `delete_word_range` ripple-deleting footage based on stale ticks.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.nleproj");
        state_with_high_ids().save_to(&path);

        let mut fresh = EditorState::new();
        fresh.transcripts.insert(
            ClipInstanceId(900),
            vec![TimelineWord { text: "stale".into(), start_tick: 0, end_tick: TIMEBASE }],
        );
        assert!(fresh.open_from(&path));

        assert!(fresh.transcripts.is_empty(), "transcripts from the previous project must not survive open_from");
    }

    #[test]
    fn loading_advances_next_id_past_every_existing_id() {
        // Regression guard: a loader that left `next_id` at 1 would hand out
        // IDs that already exist, so lookups by ID would match the wrong
        // clip and edits would corrupt unrelated parts of the timeline.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.nleproj");
        state_with_high_ids().save_to(&path);

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));

        // 1234 is the highest ID in the fixture (an effect instance) —
        // proof this considers effect IDs, not just clips/tracks.
        assert_eq!(fresh.next_id(), 1235);
    }

    #[test]
    fn opening_a_bad_file_leaves_the_current_project_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.nleproj");
        std::fs::write(&path, b"not a cbor document").unwrap();

        let mut state = state_with_high_ids();
        let before = (**state.project()).clone();
        assert!(!state.open_from(&path));
        assert_eq!(
            **state.project(),
            before,
            "a failed open must not destroy open work"
        );
        assert!(state.project_path.is_none());
    }

    #[test]
    fn media_saved_beside_the_project_gets_a_relative_reference() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("footage").join("a.mp4");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(&media, b"x").unwrap();

        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        project.assets.push(fake_asset(1, &media.to_string_lossy()));
        state.undo.push("setup", std::sync::Arc::new(project));

        let path = dir.path().join("p.nleproj");
        state.save_to(&path);
        let doc = project::load(&path).unwrap();

        let rel = &doc.media_references[0].relative_path;
        assert!(
            !std::path::Path::new(rel).is_absolute(),
            "media under the project dir should be stored relative, got {rel:?}"
        );
        // And it must resolve back to the real file from the project's dir.
        assert!(dir.path().join(rel).exists());
    }

    // ---- Scene-cut detection: the pure tick-mapping half ---------------
    //
    // `scene_cut_ops` is tested directly with hand-supplied "detected" source
    // ticks rather than through `detect_scene_cuts`'s real decode+histogram
    // pipeline — that pipeline is exercised by a real-footage smoke test
    // instead (env-gated, matching the rest of this project's real-footage
    // coverage), since decoding is slow and this half is the actual risk in
    // the wiring: source-tick -> timeline-tick mapping, speed inversion,
    // boundary filtering. The detection *algorithm* itself is already fully
    // covered, with hand-derived expected values, in
    // `crates/render/tests/scene_cut.rs`.

    #[test]
    fn scene_cut_ops_maps_a_source_tick_straight_through_at_1x_with_no_trim() {
        let (mut state, _) = state_with_three_clips();
        let clip = state.sequence().tracks[0].clips[0].clone();
        let track = state.sequence().tracks[0].id;
        assert_eq!(clip.timeline_in, TimeTick(0));
        assert_eq!(clip.source_in, TimeTick(0));

        let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE / 2]);
        assert_eq!(ops.len(), 1);
        let EditOp::Razor { track: op_track, at, .. } = ops[0] else { panic!("expected a Razor op") };
        assert_eq!(op_track, track);
        assert_eq!(at, TimeTick(TIMEBASE / 2));
    }

    #[test]
    fn scene_cut_ops_accounts_for_both_timeline_placement_and_source_trim() {
        let (mut state, _) = state_with_three_clips();
        // The middle clip: starts at 1s on the timeline, but (per the
        // fixture) also starts at 0 in its own source — trim it in so the
        // two offsets are genuinely different and a bug that mixed them up
        // would be caught.
        let track = state.sequence().tracks[0].id;
        let mut clip = state.sequence().tracks[0].clips[1].clone();
        clip.source_in = TimeTick(TIMEBASE / 4); // trimmed a quarter-second into its source
        // timeline_in stays at 1s (from the fixture).

        // A cut detected 0.3s into the (already-trimmed) source should land
        // at timeline 1s + 0.3s, independent of the 0.25s trim offset itself
        // — trim moves *where in the source* ticks 0 corresponds to, not how
        // far a detected cut is from that point.
        let source_cut = clip.source_in.0 + TIMEBASE * 3 / 10;
        let ops = state.scene_cut_ops(track, &clip, &[source_cut]);
        assert_eq!(ops.len(), 1);
        let EditOp::Razor { at, .. } = ops[0] else { panic!("expected a Razor op") };
        assert_eq!(at, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
    }

    #[test]
    fn scene_cut_ops_inverts_speed_for_a_retimed_clip() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.sequence().tracks[0].clips[0].clone();
        // 2x speed: twice as much source passes per unit of timeline time, so
        // a cut 1s into the source lands at 0.5s on the timeline. Widen
        // source_out to match — a real 2x clip covering 1s of timeline
        // consumes 2s of source, and leaving it at the fixture's 1x value
        // would put this test's own cut right on (or past) the boundary
        // `scene_cut_ops` correctly rejects.
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);

        let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE]);
        assert_eq!(ops.len(), 1);
        let EditOp::Razor { at, .. } = ops[0] else { panic!("expected a Razor op") };
        assert_eq!(at, TimeTick(TIMEBASE / 2), "2x speed should halve the timeline offset");
    }

    #[test]
    fn scene_cut_ops_refuses_a_keyframed_speed_clip() {
        // Same limitation as clip-speed audio retiming, and for the same
        // reason: inverting a keyframed curve to recover a timeline tick from
        // a source tick isn't a cheap closed-form operation the way constant
        // speed is.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.sequence().tracks[0].clips[0].clone();
        clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));

        let ops = state.scene_cut_ops(track, &clip, &[TIMEBASE / 2]);
        assert!(ops.is_empty(), "a keyframed-speed clip should produce no ops, not a wrong mapping");
    }

    #[test]
    fn scene_cut_ops_drops_a_cut_at_or_beyond_the_clips_own_source_bounds() {
        // A "cut" detected exactly at source_in or at/after source_out isn't
        // a split inside the clip's own material — applying it would ask
        // Razor to cut at the clip's own edge (or beyond it), which is not
        // what "detect the cuts inside this clip" means.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[0].clone();
        assert_eq!(clip.source_in, TimeTick(0));
        assert_eq!(clip.source_out, TimeTick(TIMEBASE));

        let ops = state.scene_cut_ops(track, &clip, &[0, TIMEBASE, TIMEBASE * 2]);
        assert!(ops.is_empty(), "cuts at/outside the clip's own bounds must be dropped, got {ops:?}");
    }

    #[test]
    fn scene_cut_ops_produces_one_op_with_a_distinct_id_per_cut() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[2].clone(); // the 2s-3s clip
        let base = clip.source_in.0;

        let ops = state.scene_cut_ops(track, &clip, &[base + TIMEBASE / 4, base + TIMEBASE / 2]);
        assert_eq!(ops.len(), 2);
        let ids: Vec<ClipInstanceId> = ops
            .iter()
            .map(|op| match op {
                EditOp::Razor { new_clip_id, .. } => *new_clip_id,
                _ => panic!("expected a Razor op"),
            })
            .collect();
        assert_ne!(ids[0], ids[1], "each cut needs its own new clip id");
    }

    // ---- Silence-based auto-cut: the pure ripple-delete-planning half --
    //
    // `silence_removal_ops` is tested with hand-supplied "detected" gaps,
    // same reasoning as `scene_cut_ops` above: real decoding is covered by
    // a real-footage smoke test, and this half is where the actual wiring
    // risk lives — tick mapping, right-to-left ordering so ripple deletes
    // don't invalidate not-yet-processed ranges, and which of the two
    // Razor-created pieces is the one to Extract.

    #[test]
    fn silence_removal_ops_produces_two_razors_and_one_extract_per_gap() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s clip, source_in=0
        let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
        assert_eq!(ops.len(), 3, "one gap should be two Razor ops plus one Extract, got {ops:?}");
        let razor_ticks: Vec<i64> = ops
            .iter()
            .filter_map(|op| match op {
                EditOp::Razor { at, .. } => Some(at.0),
                _ => None,
            })
            .collect();
        assert_eq!(razor_ticks, vec![TIMEBASE / 5, TIMEBASE * 2 / 5], "expected razors at 0.2s and 0.4s");
        assert!(matches!(ops[2], EditOp::Extract { .. }), "the gap piece should be extracted last");
    }

    #[test]
    fn silence_removal_ops_extracts_the_clip_between_the_two_razors() {
        // The subtle part: `Razor`'s `new_clip_id` becomes the *right-hand*
        // piece each time, so isolating [start, end) as its own clip means
        // extracting the id assigned to the *first* razor (at `start`) — by
        // the second razor (at `end`), that same clip is the *left*-hand
        // piece of *that* split, which is why it keeps that id rather than
        // getting a third one.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[0].clone();
        let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
        let EditOp::Razor { new_clip_id: first_razor_id, .. } = ops[0] else { panic!() };
        let EditOp::Extract { clip: extracted } = ops[2] else { panic!() };
        assert_eq!(extracted, first_razor_id);
    }

    #[test]
    fn silence_removal_ops_processes_multiple_gaps_rightmost_first() {
        // Rightmost first: extracting a gap ripples everything to its right
        // leftward. Processing right-to-left means every range still queued
        // is entirely to the *left* of whatever was just ripple-deleted, so
        // its precomputed tick positions are never invalidated by an earlier
        // step in this same batch.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[0].clone();
        let ops = state.silence_removal_ops(track, &clip, &[(0.1, 0.2), (0.6, 0.7)]);
        assert_eq!(ops.len(), 6, "two gaps should be six ops, got {ops:?}");
        let EditOp::Razor { at: first_at, .. } = ops[0] else { panic!() };
        assert_eq!(first_at, TimeTick(TIMEBASE * 6 / 10), "the rightmost gap (0.6s) must be processed first");
    }

    #[test]
    fn silence_removal_ops_drops_a_gap_that_reaches_either_edge_of_the_clip() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s
        let ops = state.silence_removal_ops(track, &clip, &[(0.0, 0.1), (0.9, 1.0)]);
        assert!(ops.is_empty(), "gaps touching either edge aren't a razor-and-extract cut, got {ops:?}");
    }

    #[test]
    fn silence_removal_ops_refuses_a_keyframed_speed_clip() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.sequence().tracks[0].clips[0].clone();
        clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
        let ops = state.silence_removal_ops(track, &clip, &[(0.2, 0.4)]);
        assert!(ops.is_empty());
    }

    /// Runs `detect_silence_and_ripple_delete` against a real file, decoding
    /// real audio through the real `audio_source` path — same convention and
    /// same reasoning as the scene-cut real-footage test just below: env-gated,
    /// `#[ignore]`d, loose assertion (real footage's true silence count isn't
    /// known ahead of time), proving the decode+detect+ripple-delete pipeline
    /// runs end to end rather than pinning an exact number.
    #[test]
    #[ignore]
    fn detect_silence_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        assert!(asset.audio.is_some(), "this test needs footage with an audio track");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 20);

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        let track_id = TrackId(1);
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: track_id,
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        // The fixture above hand-picks ClipInstanceId(1) directly rather than
        // going through `next_id()` (which a real import/add path always
        // does) — so `next_id` must be advanced past it here, or the method
        // under test would allocate a colliding id the moment it calls
        // `self.next_id()` itself, corrupting the very project it's editing.
        state.next_id = 1000;

        let total_duration_before: i64 = state.sequence().tracks[0].clips.iter().map(|c| c.timeline_out.0 - c.timeline_in.0).sum();
        let removed = state.detect_silence_and_ripple_delete(ClipInstanceId(1));
        println!("real footage silence detection: {removed} gap(s) removed over {:.1}s of source, status: {:?}", duration as f64 / TIMEBASE as f64, state.status);

        let clips = &state.sequence().tracks[0].clips;
        assert_eq!(clips.len(), removed + 1, "expected one more clip than gaps removed");
        let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
        sorted.sort();
        for w in sorted.windows(2) {
            assert_eq!(w[0].1, w[1].0, "remaining pieces must abut with no gap or overlap: {sorted:?}");
        }
        let mut ids: Vec<_> = clips.iter().map(|c| c.id).collect();
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        assert_eq!(ids.len(), clips.len(), "every resulting piece must have a distinct id");
        let total_duration_after: i64 = clips.iter().map(|c| c.timeline_out.0 - c.timeline_in.0).sum();
        assert!(
            total_duration_after <= total_duration_before,
            "ripple-deleting silence must never make the sequence longer"
        );
    }

    // ---- Beat-sync: the pure onset-to-marker mapping half --------------
    //
    // Same split as scene-cut and silence detection: `beat_markers` is
    // tested with hand-supplied onset times, real decode+FFT is exercised by
    // a real-footage smoke test. Markers, not razor cuts — a beat is
    // something to *snap to* (the timeline's existing `snap_tick` already
    // treats every marker as a snap candidate, so adding beat markers makes
    // them magnetic for free, no new snapping code needed), not something
    // that should silently restructure the timeline the way a scene cut or
    // a silence removal does.

    #[test]
    fn beat_markers_maps_source_seconds_to_timeline_ticks() {
        let (mut state, _) = state_with_three_clips();
        let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s, source_in=0, 1x
        let markers = state.beat_markers(&clip, &[0.25, 0.5, 0.75]);
        assert_eq!(markers.len(), 3);
        let positions: Vec<i64> = markers.iter().map(|m| m.position.0).collect();
        assert_eq!(positions, vec![TIMEBASE / 4, TIMEBASE / 2, TIMEBASE * 3 / 4]);
    }

    #[test]
    fn beat_markers_accounts_for_timeline_placement_and_source_trim() {
        let (mut state, _) = state_with_three_clips();
        let mut clip = state.sequence().tracks[0].clips[1].clone(); // starts at 1s on the timeline
        clip.source_in = TimeTick(TIMEBASE / 4);
        let markers = state.beat_markers(&clip, &[clip.source_in.0 as f64 / TIMEBASE as f64 + 0.3]);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].position, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
    }

    #[test]
    fn beat_markers_inverts_speed_for_a_retimed_clip() {
        let (mut state, _) = state_with_three_clips();
        let mut clip = state.sequence().tracks[0].clips[0].clone();
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
        let markers = state.beat_markers(&clip, &[1.0]);
        assert_eq!(markers.len(), 1);
        assert_eq!(markers[0].position, TimeTick(TIMEBASE / 2), "2x speed should halve the timeline offset");
    }

    #[test]
    fn beat_markers_drops_onsets_outside_the_clips_own_bounds() {
        let (mut state, _) = state_with_three_clips();
        let clip = state.sequence().tracks[0].clips[0].clone(); // 0-1s
        let markers = state.beat_markers(&clip, &[-0.1, 0.0, 0.5, 1.0, 1.5]);
        assert_eq!(markers.len(), 1, "only the 0.5s onset falls strictly inside the clip");
    }

    #[test]
    fn beat_markers_refuses_a_keyframed_speed_clip() {
        let (mut state, _) = state_with_three_clips();
        let mut clip = state.sequence().tracks[0].clips[0].clone();
        clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
        assert!(state.beat_markers(&clip, &[0.5]).is_empty());
    }

    #[test]
    fn beat_markers_each_get_a_distinct_id() {
        let (mut state, _) = state_with_three_clips();
        let clip = state.sequence().tracks[0].clips[2].clone();
        let base = clip.source_in.0 as f64 / TIMEBASE as f64;
        let markers = state.beat_markers(&clip, &[base + 0.2, base + 0.5, base + 0.8]);
        assert_eq!(markers.len(), 3);
        let mut ids: Vec<_> = markers.iter().map(|m| m.id).collect();
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        assert_eq!(ids.len(), 3, "each beat marker needs its own id");
    }

    /// Runs `detect_beats_and_add_markers` against a real file, exercising
    /// the real decode+FFT path. Same conventions as the other real-footage
    /// tests in this module: env-gated, `#[ignore]`d, loose assertion (this
    /// screen recording has no music, so its true "beat" count — probably
    /// driven by keyboard/mouse clicks rather than a musical pulse — isn't
    /// known ahead of time). What this proves is that decoding, FFT-based
    /// flux analysis, and adding real markers to a real project works end to
    /// end on a real file, not that the detected events are musically
    /// meaningful for this particular recording.
    #[test]
    #[ignore]
    fn detect_beats_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        assert!(asset.audio.is_some(), "this test needs footage with an audio track");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 20);

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        let track_id = TrackId(1);
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: track_id,
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        state.next_id = 1000;

        let added = state.detect_beats_and_add_markers(ClipInstanceId(1));
        println!("real footage beat detection: {added} marker(s) added over {:.1}s of source", duration as f64 / TIMEBASE as f64);

        let markers = &state.sequence().markers;
        assert_eq!(markers.len(), added, "the sequence should have exactly the reported number of markers");
        for m in markers {
            assert!(m.position.0 > 0 && m.position.0 < duration, "every marker must fall inside the analysed span, got {:?}", m.position);
        }
        let mut ids: Vec<_> = markers.iter().map(|m| m.id).collect();
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        assert_eq!(ids.len(), markers.len(), "every marker must have a distinct id");
    }

    // ---- Loudness auto-match: the pure gain-application half -----------
    //
    // `apply_loudness_match_gain` takes an already-*computed* gain (from
    // `audio::loudness::gain_to_reach_target`, itself tested with hand
    // values in `crates/audio/tests/loudness.rs`) and writes it to the
    // clip. Real decode + measurement is exercised by a real-footage smoke
    // test — this half is where the actual wiring risk is: refusing to
    // clobber existing fader automation, writing to the right clip.

    #[test]
    fn apply_loudness_match_gain_sets_the_clips_audio_gain() {
        let (mut state, ids) = state_with_three_clips();
        assert!(state.apply_loudness_match_gain(ids[0], 6.5));
        let clip = state.find_clip(ids[0]).unwrap().1;
        assert_eq!(clip.audio_gain_db.default, ParamValue::Number(6.5));
    }

    #[test]
    fn apply_loudness_match_gain_is_undoable() {
        let (mut state, ids) = state_with_three_clips();
        state.apply_loudness_match_gain(ids[0], 6.5);
        state.undo.undo();
        let clip = state.find_clip(ids[0]).unwrap().1;
        assert_eq!(clip.audio_gain_db.default, ParamValue::Number(0.0), "undo should restore the original gain");
    }

    #[test]
    fn apply_loudness_match_gain_refuses_a_clip_with_animated_gain() {
        // Loudness matching sets one constant value for the whole clip —
        // applying it to a clip whose gain is already keyframed would
        // silently discard that automation, which is real user work.
        let (mut state, ids) = state_with_three_clips();
        {
            let mut project = (**state.project()).clone();
            let clip = project.sequences[0].tracks[0].clips.iter_mut().find(|c| c.id == ids[0]).unwrap();
            clip.audio_gain_db.upsert_keyframe(TimeTick(0), ParamValue::Number(0.0), timeline::InterpolationMode::Linear);
            clip.audio_gain_db.upsert_keyframe(TimeTick(TIMEBASE), ParamValue::Number(3.0), timeline::InterpolationMode::Linear);
            state.undo.push("animate gain (test setup)", std::sync::Arc::new(project));
        }
        assert!(!state.apply_loudness_match_gain(ids[0], 6.5), "must refuse rather than overwrite automation");
    }

    #[test]
    fn apply_loudness_match_gain_on_a_missing_clip_does_nothing() {
        let (mut state, _) = state_with_three_clips();
        assert!(!state.apply_loudness_match_gain(ClipInstanceId(99999), 6.5));
    }

    /// Runs `match_clip_loudness` against a real file — real decode, real
    /// EBU R128 measurement, real gain applied. Unlike the other real-
    /// footage tests here, this one's expected outcome *is* fully known
    /// ahead of time (loudness matching is deterministic arithmetic on a
    /// measured value), so the assertion checks the actual number rather
    /// than just "ran without crashing".
    #[test]
    #[ignore]
    fn match_clip_loudness_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        assert!(asset.audio.is_some(), "this test needs footage with an audio track");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 20);

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        state.next_id = 1000;

        let target = -14.0;
        let gain = state.match_clip_loudness(ClipInstanceId(1), target).expect("real footage should measure");
        println!("real footage loudness match: applied {gain:.2}dB toward {target} LUFS");

        let clip = state.find_clip(ClipInstanceId(1)).unwrap().1;
        assert_eq!(clip.audio_gain_db.default, ParamValue::Number(gain), "the clip must carry the reported gain");
        assert!((-24.0..=24.0).contains(&gain), "gain must stay within the documented clamp, got {gain}");

        // Re-measuring after applying the gain (by adding it to the raw
        // signal in dB, which is what a linear gain stage does) should land
        // at the target — the honest way to confirm this isn't just
        // "produced *a* number" but produced the *right* number.
        let (interleaved, sample_rate) = state.decode_clip_interleaved_audio(&clip).unwrap();
        let raw = audio::loudness::LoudnessMeasurement::analyze(&interleaved, audio::CHANNELS, sample_rate);
        let achieved = raw.integrated_lufs as f64 + gain;
        if gain.abs() < 23.9 {
            // Only meaningful when the clamp didn't bite — a clamped gain by
            // definition won't reach the target.
            assert!((achieved - target).abs() < 0.5, "applying the computed gain should land near {target} LUFS, got {achieved:.2}");
        }
    }

    // ---- Warp Stabilizer: the pure keyframe-writing half ----------------
    //
    // `apply_stabilization_keyframes` takes already-computed `(timeline_tick,
    // (offset_x, offset_y))` pairs and writes them as `Transform::POSITION`
    // keyframes — real motion estimation and path smoothing are exercised by
    // a real-footage smoke test and are already covered with synthetic
    // ground truth in `crates/render/tests/stabilize.rs`. This half is where
    // the wiring risk is: finding vs. creating the Transform effect, adding
    // to (not replacing) any existing constant framing offset, and refusing
    // to clobber existing position keyframes.

    #[test]
    fn apply_stabilization_keyframes_creates_a_transform_effect_when_none_exists() {
        let (mut state, ids) = state_with_three_clips();
        assert!(state.find_clip(ids[0]).unwrap().1.effects.is_empty());
        let applied = state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0)), (TIMEBASE / 2, (2.0, 1.0))]);
        assert_eq!(applied, 2);
        let clip = state.find_clip(ids[0]).unwrap().1;
        let transform = clip.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID).unwrap();
        let pos = transform.params.get(render::transform::POSITION).unwrap();
        assert_eq!(pos.evaluate_at(TimeTick(0)), ParamValue::Vec2(5.0, -3.0));
        assert_eq!(pos.evaluate_at(TimeTick(TIMEBASE / 2)), ParamValue::Vec2(2.0, 1.0));
    }

    #[test]
    fn apply_stabilization_keyframes_adds_to_an_existing_constant_position() {
        // A user-set constant framing offset (e.g. "shift the shot left a
        // bit") must survive stabilization layered on top of it, not get
        // silently discarded.
        let (mut state, ids) = state_with_three_clips();
        add_transform_effect(&mut state, ids[0], (100.0, 50.0));
        state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0))]);
        let clip = state.find_clip(ids[0]).unwrap().1;
        let pos = clip.effects[0].params.get(render::transform::POSITION).unwrap();
        assert_eq!(pos.evaluate_at(TimeTick(0)), ParamValue::Vec2(105.0, 47.0));
    }

    #[test]
    fn apply_stabilization_keyframes_refuses_an_already_animated_position() {
        let (mut state, ids) = state_with_three_clips();
        add_transform_effect(&mut state, ids[0], (0.0, 0.0));
        // `set_param_value_at` only writes a keyframe when the track is
        // *already* animated (otherwise it just overwrites `default`) — so
        // building the animated case needs the same direct project-mutation
        // approach `state_with_three_clips`'s siblings use elsewhere, not a
        // couple of ordinary param-value calls.
        {
            let mut project = (**state.project()).clone();
            let clip = project.sequences[0].tracks[0].clips.iter_mut().find(|c| c.id == ids[0]).unwrap();
            let pos = clip.effects[0].params.get_mut(render::transform::POSITION).unwrap();
            pos.upsert_keyframe(TimeTick(0), ParamValue::Vec2(1.0, 0.0), timeline::InterpolationMode::Linear);
            pos.upsert_keyframe(TimeTick(TIMEBASE / 2), ParamValue::Vec2(2.0, 0.0), timeline::InterpolationMode::Linear);
            state.undo.push("animate position (test setup)", std::sync::Arc::new(project));
        }
        let applied = state.apply_stabilization_keyframes(ids[0], &[(0, (5.0, -3.0))]);
        assert_eq!(applied, 0, "must refuse rather than overwrite existing position keyframes");
    }

    #[test]
    fn apply_stabilization_keyframes_is_one_undo_step_regardless_of_sample_count() {
        let (mut state, ids) = state_with_three_clips();
        let depth = state.undo.history().len();
        state.apply_stabilization_keyframes(ids[0], &[(0, (1.0, 0.0)), (TIMEBASE / 3, (2.0, 0.0)), (TIMEBASE * 2 / 3, (3.0, 0.0))]);
        assert_eq!(state.undo.history().len(), depth + 1);
    }

    #[test]
    fn apply_stabilization_keyframes_on_an_empty_input_does_nothing() {
        let (mut state, ids) = state_with_three_clips();
        let depth = state.undo.history().len();
        assert_eq!(state.apply_stabilization_keyframes(ids[0], &[]), 0);
        assert_eq!(state.undo.history().len(), depth);
    }

    fn add_transform_effect(state: &mut EditorState, clip_id: ClipInstanceId, position: (f64, f64)) {
        let mut params = std::collections::BTreeMap::new();
        params.insert(
            render::transform::POSITION.to_string(),
            timeline::ParamTrack::constant(ParamValue::Vec2(position.0, position.1)),
        );
        for p in render::transform::descriptor().params {
            params.entry(p.name.to_string()).or_insert_with(|| timeline::ParamTrack::constant(p.default));
        }
        let id = timeline::EffectInstanceId(state.next_id());
        state.add_effect(
            clip_id,
            timeline::EffectInstance {
                id,
                effect_type: render::transform::TYPE_ID.to_string(),
                enabled: true,
                params,
            },
        );
    }

    /// Runs `stabilize_clip` against a real file — real frame decode, real
    /// downsampled block-matching motion estimation, real keyframes written.
    /// Env-gated, `#[ignore]`d, same conventions as the rest of this module's
    /// real-footage tests. The assertion checks that *something* measurable
    /// happened (keyframes landed at the expected ticks, with the expected
    /// count) rather than that the correction is visually good — motion
    /// estimation quality on an arbitrary real clip isn't something a single
    /// automated assertion can honestly judge; this is the same trade the
    /// scene-cut and beat-detection real-footage tests make.
    #[test]
    #[ignore]
    fn stabilize_clip_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 10); // stabilization's per-sample decode+search is the slowest of these actions; keep the smoke test itself short

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        state.next_id = 1000;

        let applied = state.stabilize_clip(ClipInstanceId(1));
        println!("real footage stabilization: {applied} keyframe(s) written over {:.1}s of source", duration as f64 / TIMEBASE as f64);
        assert!(applied >= 3, "at least the sampled frames should produce keyframes, got {applied}");

        let clip = state.find_clip(ClipInstanceId(1)).unwrap().1;
        let transform = clip.effects.iter().find(|e| e.effect_type == render::transform::TYPE_ID).expect("Transform effect should exist");
        let pos = transform.params.get(render::transform::POSITION).unwrap();
        assert!(pos.is_animated(), "position should be keyframed after stabilization");
        assert_eq!(pos.keyframes.len(), applied);
        for kf in &pos.keyframes {
            assert!(kf.at.0 >= 0 && kf.at.0 <= duration, "every keyframe must land inside the clip's own span, got {:?}", kf.at);
        }
    }

    // ---- Auto-captions: the pure clip-building half ---------------------
    //
    // `caption_ops` turns already-transcribed segments (hand-supplied here,
    // real transcription exercised by a real-footage smoke test) into
    // `Overwrite` ops placing a title clip per segment — same
    // detect/apply split as every other analysis action in this file.

    fn dummy_segment(text: &str, start_ms: u32, end_ms: u32) -> speech::Segment {
        speech::Segment { text: text.into(), start_ms, end_ms, words: vec![] }
    }

    #[test]
    fn caption_ops_places_one_title_clip_per_segment_at_the_right_ticks() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.find_clip(ids[0]).unwrap().1; // 0-1s clip
        let segments = vec![dummy_segment("Hello there", 0, 300), dummy_segment("General Kenobi", 300, 900)];
        let ops = state.caption_ops(track, &clip, &segments);
        assert_eq!(ops.len(), 2);
        let EditOp::Overwrite { at: at0, clip: clip0, .. } = &ops[0] else { panic!() };
        assert_eq!(*at0, TimeTick(0));
        assert_eq!(clip0.timeline_out.0, TIMEBASE * 3 / 10);
        let ClipSource::Title(spec0) = &clip0.source else { panic!("expected a title clip") };
        assert_eq!(spec0.text, "Hello there");

        let EditOp::Overwrite { at: at1, clip: clip1, .. } = &ops[1] else { panic!() };
        assert_eq!(*at1, TimeTick(TIMEBASE * 3 / 10));
        assert_eq!(clip1.timeline_out.0, TIMEBASE * 9 / 10);
    }

    #[test]
    fn caption_ops_accounts_for_timeline_placement_and_source_trim() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.find_clip(ids[1]).unwrap().1; // starts at 1s on the timeline
        clip.source_in = TimeTick(TIMEBASE / 4);
        let start_ms = 300; // 0.3s into the (trimmed) source
        let end_ms = 600;
        let ops = state.caption_ops(track, &clip, &[dummy_segment("hi", start_ms, end_ms)]);
        assert_eq!(ops.len(), 1);
        let EditOp::Overwrite { at, clip: caption_clip, .. } = &ops[0] else { panic!() };
        assert_eq!(*at, TimeTick(clip.timeline_in.0 + TIMEBASE * 3 / 10));
        assert_eq!(caption_clip.timeline_out.0, clip.timeline_in.0 + TIMEBASE * 6 / 10);
    }

    #[test]
    fn caption_ops_inverts_speed_for_a_retimed_clip() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.find_clip(ids[0]).unwrap().1;
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
        let ops = state.caption_ops(track, &clip, &[dummy_segment("hi", 0, 1000)]);
        assert_eq!(ops.len(), 1);
        let EditOp::Overwrite { clip: caption_clip, .. } = &ops[0] else { panic!() };
        assert_eq!(caption_clip.timeline_out.0, TIMEBASE / 2, "2x speed should halve the caption's timeline duration");
    }

    #[test]
    fn caption_ops_refuses_a_keyframed_speed_clip() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let mut clip = state.find_clip(ids[0]).unwrap().1;
        clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0)));
        assert!(state.caption_ops(track, &clip, &[dummy_segment("hi", 0, 500)]).is_empty());
    }

    #[test]
    fn caption_ops_drops_a_zero_length_segment() {
        // A malformed or degenerate segment (start == end) would place a
        // zero-duration clip — Overwrite would reject it anyway, but
        // filtering it here means the whole caption batch doesn't fail for
        // one bad segment.
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.find_clip(ids[0]).unwrap().1;
        let ops = state.caption_ops(track, &clip, &[dummy_segment("", 100, 100)]);
        assert!(ops.is_empty());
    }

    #[test]
    fn caption_ops_each_get_a_distinct_clip_id() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let clip = state.find_clip(ids[2]).unwrap().1;
        let ops = state.caption_ops(track, &clip, &[dummy_segment("a", 0, 300), dummy_segment("b", 300, 600)]);
        assert_eq!(ops.len(), 2);
        let mut clip_ids = Vec::new();
        for op in &ops {
            let EditOp::Overwrite { clip, .. } = op else { panic!() };
            clip_ids.push(clip.id);
        }
        assert_ne!(clip_ids[0], clip_ids[1]);
    }

    /// Runs `generate_captions` against a real file — real audio decode,
    /// real local Whisper transcription, real title clips placed on the
    /// timeline. Env-gated, `#[ignore]`d, same conventions as the rest of
    /// this module. Requires the prebuilt whisper-cli + model to be
    /// installed (see `speech::WhisperConfig::default`'s doc).
    #[test]
    #[ignore]
    fn generate_captions_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        let config = speech::WhisperConfig::default();
        if !config.cli_path.is_file() || !config.model_path.is_file() {
            panic!("whisper-cli or the model isn't installed — see docs/decisions-log.md's speech entry");
        }
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        assert!(asset.audio.is_some(), "this test needs footage with an audio track");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 15);

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        state.next_id = 1000;

        let added = state.generate_captions(ClipInstanceId(1));
        println!("real footage captions: {added} caption(s) generated over {:.1}s of source", duration as f64 / TIMEBASE as f64);
        assert!(added > 0, "expected at least one caption from real speech");

        // Captions land on a *different* track than the original video, per
        // `generate_captions`'s "never overwrite existing material" rule.
        assert_eq!(state.sequence().tracks.len(), 2, "expected a new caption track above the video");
        let caption_track = &state.sequence().tracks[1];
        assert_eq!(caption_track.clips.len(), added);
        for c in &caption_track.clips {
            assert!(matches!(c.source, ClipSource::Title(_)), "every caption clip should be a title");
            assert!(c.timeline_out.0 > c.timeline_in.0, "every caption should have real duration");
        }
        let mut sorted: Vec<_> = caption_track.clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
        sorted.sort();
        for w in sorted.windows(2) {
            assert!(w[0].1 <= w[1].0, "captions must not overlap: {sorted:?}");
        }
    }

    /// Regression guard for a real bug: `generate_captions` used to write
    /// into `self.transcripts` *before* checking the clip's speed curve.
    /// Run it on a keyframed-speed clip that already had a real transcript
    /// (from an earlier `transcribe_clip` call via the transcript panel),
    /// and the correct refusal (0 captions, keyframed speed isn't
    /// supported) had the side effect of overwriting that real transcript
    /// with an empty one — silent data loss while reporting nothing
    /// happened. Real footage + real Whisper, so the bug is only reachable
    /// once transcription actually succeeds and reaches the speed check.
    #[test]
    #[ignore]
    fn generate_captions_on_a_keyframed_clip_does_not_clobber_an_existing_transcript() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        let config = speech::WhisperConfig::default();
        if !config.cli_path.is_file() || !config.model_path.is_file() {
            panic!("whisper-cli or the model isn't installed — see docs/decisions-log.md's speech entry");
        }
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        assert!(asset.audio.is_some(), "this test needs footage with an audio track");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 15);

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
                speed: SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(1.0))),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        state.next_id = 1000;

        let real_transcript = vec![TimelineWord { text: "hello".into(), start_tick: 0, end_tick: TIMEBASE / 2 }];
        state.transcripts.insert(ClipInstanceId(1), real_transcript.clone());

        let added = state.generate_captions(ClipInstanceId(1));

        assert_eq!(added, 0, "keyframed-speed clips aren't supported yet, so no captions should be created");
        assert_eq!(
            state.transcripts.get(&ClipInstanceId(1)),
            Some(&real_transcript),
            "the real transcript from the earlier transcribe_clip call must survive the refused caption attempt"
        );
    }

    // ---- Transcript panel: word mapping and range deletion --------------

    #[test]
    fn timeline_words_from_transcript_maps_real_words() {
        let (mut state, ids) = state_with_three_clips();
        let clip = state.find_clip(ids[0]).unwrap().1; // 0-1s, source_in=0, 1x
        let segments = vec![speech::Segment {
            text: "hi there".into(),
            start_ms: 0,
            end_ms: 900,
            words: vec![
                speech::Word { text: "hi".into(), start_ms: 100, end_ms: 300 },
                speech::Word { text: "there".into(), start_ms: 300, end_ms: 900 },
            ],
        }];
        let words = state.timeline_words_from_transcript(&clip, &segments);
        assert_eq!(words.len(), 2);
        assert_eq!(words[0], TimelineWord { text: "hi".into(), start_tick: TIMEBASE / 10, end_tick: TIMEBASE * 3 / 10 });
        assert_eq!(words[1].text, "there");
        assert_eq!(words[1].start_tick, TIMEBASE * 3 / 10);
    }

    #[test]
    fn timeline_words_from_transcript_accounts_for_trim_speed_and_bounds() {
        let (mut state, ids) = state_with_three_clips();
        let mut clip = state.find_clip(ids[0]).unwrap().1;
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        clip.source_out = TimeTick(clip.source_in.0 + TIMEBASE * 2);
        let segments = vec![speech::Segment {
            text: "a".into(),
            start_ms: 0,
            end_ms: 2000,
            words: vec![speech::Word { text: "a".into(), start_ms: 1000, end_ms: 2000 }],
        }];
        let words = state.timeline_words_from_transcript(&clip, &segments);
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].start_tick, TIMEBASE / 2, "2x speed should halve the timeline offset");
        assert_eq!(words[0].end_tick, TIMEBASE);
    }

    #[test]
    fn timeline_range_removal_ops_produces_two_razors_and_one_extract() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let ops = state.timeline_range_removal_ops(track, TIMEBASE / 4, TIMEBASE / 2);
        assert_eq!(ops.len(), 3);
        assert!(matches!(ops[2], EditOp::Extract { .. }));
        let EditOp::Razor { at: at0, .. } = ops[0] else { panic!() };
        let EditOp::Razor { at: at1, .. } = ops[1] else { panic!() };
        assert_eq!(at0, TimeTick(TIMEBASE / 4));
        assert_eq!(at1, TimeTick(TIMEBASE / 2));
    }

    #[test]
    fn timeline_range_removal_ops_rejects_an_inverted_or_empty_range() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        assert!(state.timeline_range_removal_ops(track, TIMEBASE / 2, TIMEBASE / 2).is_empty());
        assert!(state.timeline_range_removal_ops(track, TIMEBASE / 2, TIMEBASE / 4).is_empty());
    }

    #[test]
    fn delete_word_range_ripple_deletes_the_spanned_words_timeline_range() {
        // An *interior* selection deliberately — "two" alone, not touching
        // either edge of the clip. `delete_word_range` is scoped to interior
        // ranges only, same as `silence_removal_ops`: a range touching an
        // edge is a trim (shorten the clip), not an isolate-and-extract, and
        // handling both shapes in one action is real, separate follow-up
        // work rather than something to half-solve here.
        let (mut state, ids) = state_with_three_clips();
        let clip_id = ids[0]; // spans [0, TIMEBASE)
        state.transcripts.insert(
            clip_id,
            vec![
                TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 5 },
                TimelineWord { text: "two".into(), start_tick: TIMEBASE / 5, end_tick: TIMEBASE * 2 / 5 },
                TimelineWord { text: "three".into(), start_tick: TIMEBASE * 2 / 5, end_tick: TIMEBASE },
            ],
        );
        let track = state.sequence().tracks[0].id;
        let removed = state.delete_word_range(track, clip_id, 1, 1); // "two" only
        assert!(removed, "should successfully ripple-delete the selected word");
        // `state_with_three_clips` already has two more clips on this same
        // track (originally at 1-2s and 2-3s), untouched apart from
        // rippling left. Clip 0 itself nets to *two* remaining pieces — cut
        // a chunk out of the middle of one clip and what's left is a head
        // and a tail, not three — so four clips total, not the fixture's
        // original three.
        let clips = &state.sequence().tracks[0].clips;
        assert_eq!(clips.len(), 4, "clip 0's head + tail, plus the two untouched (rippled) clips after it");
        let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
        sorted.sort();
        assert_eq!(sorted[0], (0, TIMEBASE / 5), "\"one\" keeps its start and shrinks to just before \"two\"");
        // "three" started at 2/5 and the deletion removed 1/5 of duration, so
        // the tail piece now starts right where "one" ends.
        assert_eq!(sorted[1].0, TIMEBASE / 5, "\"three\" ripples left to close the gap \"two\" left");
        for w in sorted.windows(2) {
            assert_eq!(w[0].1, w[1].0, "every piece must still abut with no gap or overlap: {sorted:?}");
        }
    }

    #[test]
    fn delete_word_range_returns_false_for_an_edge_touching_selection() {
        let (mut state, ids) = state_with_three_clips();
        let clip_id = ids[0];
        state.transcripts.insert(
            clip_id,
            vec![TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 2 }],
        );
        let track = state.sequence().tracks[0].id;
        assert!(!state.delete_word_range(track, clip_id, 0, 0), "a selection touching the clip's own start must be refused");
    }

    #[test]
    fn delete_word_range_never_reaches_into_the_next_clip_on_the_track() {
        // The case that actually needs the bounds guard, not just a "touches
        // the edge exactly" formality: a word whose end tick has drifted
        // past this clip's own end (e.g. a stale transcript after the clip
        // was trimmed) must never let the deletion razor into — and
        // ripple-delete part of — a completely different, unrelated clip
        // sitting right after it on the same track.
        let (mut state, ids) = state_with_three_clips(); // clip0 [0,1s), clip1 [1s,2s)
        let clip_id = ids[0];
        state.transcripts.insert(
            clip_id,
            vec![TimelineWord { text: "one".into(), start_tick: TIMEBASE / 10, end_tick: TIMEBASE + TIMEBASE / 10 }],
        );
        let track = state.sequence().tracks[0].id;
        let before = state.project().clone();
        assert!(!state.delete_word_range(track, clip_id, 0, 0), "a word extending past this clip's own end must be refused");
        assert_eq!(**state.project(), *before, "a refused deletion must leave the project completely untouched");
    }

    #[test]
    fn delete_word_range_returns_false_for_an_out_of_range_selection() {
        let (mut state, ids) = state_with_three_clips();
        let clip_id = ids[0];
        state.transcripts.insert(clip_id, vec![TimelineWord { text: "one".into(), start_tick: 0, end_tick: TIMEBASE / 2 }]);
        let track = state.sequence().tracks[0].id;
        assert!(!state.delete_word_range(track, clip_id, 5, 9));
    }

    /// Runs `detect_scene_cuts` against a real file, decoding real frames
    /// through the real `media_ffmpeg` path — the integration risk the pure
    /// `scene_cut_ops` tests above can't reach. Env-gated and `#[ignore]`d,
    /// matching `crates/playback/tests/real_footage_smoke.rs`'s convention:
    /// real footage is machine-specific and not something to commit or run
    /// in ordinary `cargo test`.
    ///
    /// The assertion is deliberately loose (runs to completion, returns a
    /// plausible count) rather than pinned to an exact number of cuts: unlike
    /// the synthetic tests above, this file's true cut count isn't known
    /// ahead of time, and a screen recording may have few or none at all.
    /// What this test actually proves is that decoding, histogramming, and
    /// applying real `Razor` ops against a real H.264 file works end to end
    /// without silently breaking — the pure tests already prove the
    /// detection math itself is correct.
    #[test]
    #[ignore]
    fn detect_scene_cuts_runs_end_to_end_on_real_footage() {
        let path = match std::env::var("NLE_REAL_FOOTAGE_PATH") {
            Ok(p) => std::path::PathBuf::from(p),
            Err(_) => panic!(
                "set NLE_REAL_FOOTAGE_PATH to a real video file and run with --ignored to use this test"
            ),
        };
        media_ffmpeg::init().unwrap();
        let asset = media_ffmpeg::probe(&path).expect("real footage failed to probe");
        let video = asset.video.as_ref().expect("expected a video stream");
        let duration = asset.duration_ticks.min(TIMEBASE * 20); // cap the sample at 20s of source

        let mut state = EditorState::new();
        state.asset_paths.insert(asset.id, path);
        let mut project = (**state.project()).clone();
        let track_id = TrackId(1);
        project.sequences[0].settings.width = video.width;
        project.sequences[0].settings.height = video.height;
        project.assets.push(asset.clone());
        project.sequences[0].tracks.push(Track {
            id: track_id,
            kind: TrackKind::Video,
            name: "V1".into(),
            clips: vec![ClipInstance {
                id: ClipInstanceId(1),
                source: ClipSource::Media(asset.id),
                source_in: TimeTick(0),
                source_out: TimeTick(duration),
                timeline_in: TimeTick(0),
                timeline_out: TimeTick(duration),
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
        state.undo = command::UndoStack::new(std::sync::Arc::new(project), command::UndoStack::DEFAULT_MAX_HISTORY);
        // The fixture above hand-picks ClipInstanceId(1) directly rather than
        // going through `next_id()` (which a real import/add path always
        // does) — so `next_id` must be advanced past it here, or the method
        // under test would allocate a colliding id the moment it calls
        // `self.next_id()` itself, corrupting the very project it's editing.
        state.next_id = 1000;

        let found = state.detect_scene_cuts(ClipInstanceId(1));
        println!(
            "real footage scene-cut detection: {found} cut(s) found over {:.1}s of source",
            duration as f64 / TIMEBASE as f64
        );

        // The clip should have been split into `found + 1` pieces on the
        // track, and every resulting clip must still be a real, non-empty,
        // non-overlapping span — `check_no_overlaps`-style sanity, proving
        // the applied `Razor` ops actually left a valid project rather than
        // merely not panicking.
        let clips = &state.sequence().tracks[0].clips;
        assert_eq!(clips.len(), found + 1, "expected one more clip than cuts found");
        let mut sorted: Vec<_> = clips.iter().map(|c| (c.timeline_in.0, c.timeline_out.0)).collect();
        sorted.sort();
        for w in sorted.windows(2) {
            assert_eq!(w[0].1, w[1].0, "split pieces must abut with no gap or overlap: {sorted:?}");
        }
        let mut ids: Vec<_> = clips.iter().map(|c| c.id).collect();
        ids.sort_by_key(|id| id.0);
        ids.dedup();
        assert_eq!(ids.len(), clips.len(), "every resulting piece must have a distinct id");
        assert!(found < 160, "8 samples/sec over 20s caps at 160 samples — can't exceed that many cuts");
    }

    /// Three abutting 1s clips on one video track: 0-1s, 1-2s, 2-3s. Abutting
    /// on purpose — it's the layout where lift vs ripple delete actually differ.
    fn state_with_three_clips() -> (EditorState, Vec<ClipInstanceId>) {
        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        let ids: Vec<ClipInstanceId> = (1..=3).map(|i| ClipInstanceId(i * 10)).collect();
        let clips: Vec<ClipInstance> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| {
                let start = i as i64 * TIMEBASE;
                ClipInstance {
                    id: *id,
                    source: ClipSource::Media(media::MediaAssetId(1)),
                    source_in: TimeTick(0),
                    source_out: TimeTick(TIMEBASE),
                    timeline_in: TimeTick(start),
                    timeline_out: TimeTick(start + TIMEBASE),
                    speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                    effects: vec![],
                    audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                    audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                    linked_group: None,
                }
            })
            .collect();
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips,
            transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
        project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
        state.undo.push("setup", std::sync::Arc::new(project));
        state.selected_clips.clear();
        (state, ids)
    }

    fn clip_starts(state: &EditorState) -> Vec<i64> {
        let mut v: Vec<i64> = state.sequence().tracks[0]
            .clips
            .iter()
            .map(|c| c.timeline_in.0)
            .collect();
        v.sort_unstable();
        v
    }

    fn transitions_of(state: &EditorState) -> Vec<timeline::Transition> {
        state.sequence().tracks[0].transitions.clone()
    }

    #[test]
    fn a_transition_lands_on_the_cut_and_is_undoable() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let cut = TimeTick(TIMEBASE); // between clip 1 and clip 2

        assert!(state.add_transition(
            track,
            cut,
            timeline::TransitionKind::CrossDissolve,
            TimeTick(TIMEBASE / 2),
        ));
        let trs = transitions_of(&state);
        assert_eq!(trs.len(), 1);
        assert_eq!(trs[0].at, cut);
        assert_eq!(trs[0].duration, TimeTick(TIMEBASE / 2));
        // Centred on the cut.
        assert_eq!(trs[0].region(), (TimeTick(TIMEBASE * 3 / 4), TimeTick(TIMEBASE * 5 / 4)));

        state.undo.undo();
        assert!(transitions_of(&state).is_empty());
    }

    #[test]
    fn a_transition_is_clamped_to_fit_its_neighbouring_clips() {
        // Unclamped, a transition wider than its clips reaches into material
        // that isn't in the sequence, and both halves show frozen frames
        // instead of a dissolve.
        let (mut state, _) = state_with_three_clips(); // 1s clips
        let track = state.sequence().tracks[0].id;

        state.add_transition(
            track,
            TimeTick(TIMEBASE),
            timeline::TransitionKind::CrossDissolve,
            TimeTick(TIMEBASE * 10), // absurdly long
        );
        let tr = &transitions_of(&state)[0];
        assert_eq!(
            tr.duration,
            TimeTick(TIMEBASE * 2),
            "each half must fit inside its 1s clip, so 2s total is the maximum"
        );
        assert!(state.status.contains("shortened"), "the clamp should be reported, not silent");
    }

    #[test]
    fn adding_a_transition_twice_on_one_cut_replaces_it() {
        // Two transitions claiming the same ticks would make `transition_at`'s
        // "first match wins" arbitrary.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let cut = TimeTick(TIMEBASE);

        state.add_transition(track, cut, timeline::TransitionKind::CrossDissolve, TimeTick(1000));
        state.add_transition(track, cut, timeline::TransitionKind::DipToBlack, TimeTick(2000));

        let trs = transitions_of(&state);
        assert_eq!(trs.len(), 1, "the second add must replace, not stack");
        assert_eq!(trs[0].kind, timeline::TransitionKind::DipToBlack);
        assert_eq!(trs[0].duration, TimeTick(2000));
    }

    // ---- Titles -------------------------------------------------------

    fn only_title(state: &EditorState) -> timeline::TitleSpec {
        let clip = state
            .sequence()
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .find(|c| matches!(c.source, ClipSource::Title(_)))
            .expect("expected a title clip");
        let ClipSource::Title(spec) = &clip.source else { unreachable!() };
        spec.clone()
    }

    #[test]
    fn adding_a_title_puts_a_title_clip_on_the_track_at_the_playhead() {
        let (mut state, _) = state_with_three_clips();
        // Past the end of the three media clips (which span 0..3s), so the
        // title lands on empty track rather than overwriting them.
        let track = state.sequence().tracks[0].id;
        state.playhead = TIMEBASE * 5;

        let id = state.add_title(track, TimeTick(TIMEBASE * 2)).expect("title added");

        let clip = state
            .sequence()
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .find(|c| c.id == id)
            .expect("the new clip exists");
        assert_eq!(clip.timeline_in, TimeTick(TIMEBASE * 5), "starts at the playhead");
        assert_eq!(clip.timeline_out, TimeTick(TIMEBASE * 7), "and runs for the given duration");
        assert!(matches!(clip.source, ClipSource::Title(_)));
    }

    #[test]
    fn a_new_title_is_selected_so_it_can_be_typed_into_immediately() {
        // Adding a title and then having to hunt for it before typing would be
        // a poor enough gesture that the feature wouldn't get used.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.playhead = TIMEBASE * 5;
        let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
        assert_eq!(state.selected_clips, vec![id]);
    }

    #[test]
    fn editing_a_titles_text_is_undoable() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.playhead = TIMEBASE * 5;
        let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");

        let mut spec = only_title(&state);
        spec.text = "Rewritten".into();
        assert!(state.set_title_spec(id, spec));
        assert_eq!(only_title(&state).text, "Rewritten");

        state.undo.undo();
        assert_eq!(
            only_title(&state).text,
            timeline::TitleSpec::default().text,
            "undo should restore the title's previous text, not remove the clip"
        );
    }

    #[test]
    fn consecutive_keystrokes_in_one_title_coalesce_into_one_undo_step() {
        // Typing a word is one edit to a human. Without coalescing, undoing a
        // title would walk back one character at a time — the same reason
        // dragging a clip is one undo step and not 400.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.playhead = TIMEBASE * 5;
        let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
        let depth_before = state.undo.history().len();

        for text in ["H", "He", "Hel", "Hell", "Hello"] {
            let mut spec = only_title(&state);
            spec.text = text.into();
            state.set_title_spec(id, spec);
        }

        assert_eq!(
            state.undo.history().len(),
            depth_before + 1,
            "five keystrokes should be one undo step, not five"
        );
        state.undo.undo();
        assert_eq!(only_title(&state).text, timeline::TitleSpec::default().text);
    }

    #[test]
    fn setting_a_title_spec_that_changes_nothing_records_no_undo_step() {
        // The properties panel re-submits the spec every frame it's shown.
        // Pushing an undo entry for each would fill the whole history with
        // no-ops within seconds of selecting a title.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.playhead = TIMEBASE * 5;
        let id = state.add_title(track, TimeTick(TIMEBASE)).expect("title added");
        let depth = state.undo.history().len();

        let unchanged = only_title(&state);
        assert!(!state.set_title_spec(id, unchanged), "an identical spec is not an edit");
        assert_eq!(state.undo.history().len(), depth);
    }

    #[test]
    fn add_title_at_playhead_uses_the_top_video_track_when_it_is_free_there() {
        let (mut state, _) = state_with_three_clips();
        let existing_tracks = state.sequence().tracks.len();
        // Past the three clips, so the top video track is empty here.
        state.playhead = TIMEBASE * 10;

        state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");

        assert_eq!(
            state.sequence().tracks.len(),
            existing_tracks,
            "no need for a new track when the top one is free"
        );
        assert_eq!(only_title(&state), timeline::TitleSpec::default());
    }

    #[test]
    fn add_title_at_playhead_makes_a_new_track_rather_than_overwriting_footage() {
        // The important one. A title inserted over existing video must not
        // destroy it — "Add Title" is a create gesture, and silently eating a
        // second of footage would be the worst kind of data loss: invisible
        // until you scrub back to it.
        let (mut state, ids) = state_with_three_clips();
        let existing_tracks = state.sequence().tracks.len();
        // Squarely on top of the middle clip.
        state.playhead = TIMEBASE + TIMEBASE / 2;

        state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");

        assert_eq!(
            state.sequence().tracks.len(),
            existing_tracks + 1,
            "an occupied top track should get a new one above it"
        );
        let media_clips: Vec<ClipInstanceId> = state
            .sequence()
            .tracks
            .iter()
            .flat_map(|t| &t.clips)
            .filter(|c| matches!(c.source, ClipSource::Media(_)))
            .map(|c| c.id)
            .collect();
        assert_eq!(media_clips, ids, "every original clip must survive untouched");
    }

    #[test]
    fn adding_a_title_over_footage_is_a_single_undo_step() {
        // Creating the track and placing the clip are one gesture to the user,
        // so one Ctrl+Z must put things back exactly as they were.
        let (mut state, _) = state_with_three_clips();
        let before = state.project().clone();
        state.playhead = TIMEBASE + TIMEBASE / 2;

        state.add_title_at_playhead(TimeTick(TIMEBASE)).expect("title added");
        state.undo.undo();

        assert_eq!(**state.project(), *before, "one undo restores the whole gesture");
    }

    #[test]
    fn set_title_spec_on_a_clip_that_is_not_a_title_does_nothing() {
        let (mut state, ids) = state_with_three_clips();
        let depth = state.undo.history().len();
        assert!(!state.set_title_spec(ids[0], timeline::TitleSpec::default()));
        assert_eq!(state.undo.history().len(), depth);
    }

    #[test]
    fn a_transition_needs_an_actual_cut() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        // Mid-clip: no clip starts or ends here.
        assert!(!state.add_transition(
            track,
            TimeTick(TIMEBASE / 2),
            timeline::TransitionKind::CrossDissolve,
            TimeTick(1000),
        ));
        assert!(transitions_of(&state).is_empty());
        assert!(!state.status.is_empty(), "should say why nothing happened");
    }

    #[test]
    fn removing_a_transition_leaves_the_clips_alone() {
        let (mut state, ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let cut = TimeTick(TIMEBASE);
        state.add_transition(track, cut, timeline::TransitionKind::CrossDissolve, TimeTick(1000));

        state.remove_transition(track, cut);

        assert!(transitions_of(&state).is_empty());
        assert_eq!(state.sequence().tracks[0].clips.len(), 3);
        assert_eq!(clip_starts(&state), vec![0, TIMEBASE, TIMEBASE * 2]);
        assert!(state.find_clip(ids[0]).is_some(), "clips must be untouched");
    }

    #[test]
    fn transitions_survive_a_save_load_round_trip() {
        // The schema v3 field has to actually persist — a transition that
        // vanishes on reopen is worse than one that never existed.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.add_transition(
            track,
            TimeTick(TIMEBASE),
            timeline::TransitionKind::DipToBlack,
            TimeTick(TIMEBASE / 2),
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.nleproj");
        state.save_to(&path);

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        let trs = transitions_of(&fresh);
        assert_eq!(trs.len(), 1);
        assert_eq!(trs[0].kind, timeline::TransitionKind::DipToBlack);
        assert_eq!(trs[0].at, TimeTick(TIMEBASE));
    }

    #[test]
    fn a_track_fader_and_pan_are_stored_and_undoable() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;

        state.set_track_gain(track, -9.0, false);
        state.set_track_pan(track, -0.5);
        let t = &state.sequence().tracks[0];
        assert_eq!(t.gain_db.evaluate_at(TimeTick(0)).as_scalar(), Some(-9.0));
        assert_eq!(t.pan, -0.5);

        state.undo.undo(); // pan
        assert_eq!(state.sequence().tracks[0].pan, 0.0);
        state.undo.undo(); // fader
        assert_eq!(
            state.sequence().tracks[0].gain_db.evaluate_at(TimeTick(0)).as_scalar(),
            Some(0.0)
        );
    }

    #[test]
    fn track_pan_is_clamped_to_the_stereo_field() {
        // The mixer's pan law is defined on -1..1; a value outside it would be
        // clamped silently inside the mixer, so the stored value would disagree
        // with what you hear.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.set_track_pan(track, 5.0);
        assert_eq!(state.sequence().tracks[0].pan, 1.0);
        state.set_track_pan(track, -5.0);
        assert_eq!(state.sequence().tracks[0].pan, -1.0);
    }

    #[test]
    fn mixer_settings_survive_a_save_load_round_trip() {
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.set_track_gain(track, -4.5, false);
        state.set_track_pan(track, 0.75);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mix.nleproj");
        state.save_to(&path);
        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));

        let t = &fresh.sequence().tracks[0];
        assert_eq!(t.gain_db.evaluate_at(TimeTick(0)).as_scalar(), Some(-4.5));
        assert_eq!(t.pan, 0.75);
    }

    #[test]
    fn the_master_fader_is_session_state_not_project_state() {
        // A monitoring choice must not follow the file: opening a project saved
        // by someone who had pulled the master down should not silently export
        // quiet.
        let (mut state, _) = state_with_three_clips();
        state.master_gain_db = -20.0;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.nleproj");
        state.save_to(&path);

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        assert_eq!(fresh.master_gain_db, 0.0, "master gain must not persist into the project");
    }

    #[test]
    fn an_autosave_snapshot_round_trips_but_does_not_claim_the_project_path() {
        // The trap this guards: if a recovery write adopted its own path as the
        // project's location, the next Ctrl+S would silently overwrite the
        // recovery file instead of the user's project — and "Save" would stop
        // producing a file the user could find.
        let (state, _) = state_with_three_clips();
        let dir = tempfile::tempdir().unwrap();
        let recovery = dir.path().join(".recover-x.nleproj");

        state.write_snapshot_to(&recovery).expect("snapshot should write");
        assert!(recovery.exists());
        assert_eq!(state.project_path, None, "autosave must not claim the project path");
        assert!(
            !state.status.contains("saved"),
            "autosave must not overwrite the status line, found {:?}",
            state.status
        );

        // And it's a real, loadable project, not a truncated one.
        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&recovery));
        assert_eq!(fresh.sequence().tracks[0].clips.len(), 3);
    }

    #[test]
    fn a_transition_added_in_the_editor_reaches_the_compiled_graph() {
        // Bridges the two halves that are each tested on their own: the state
        // layer writing a transition, and the compositor blending one. If the
        // preview shows no dissolve, this says which side is at fault.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.add_transition(
            track,
            TimeTick(TIMEBASE),
            timeline::TransitionKind::CrossDissolve,
            TimeTick(TIMEBASE / 2),
        );

        let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
        // Region is 0.75s..1.25s, so the cut at 1s is the midpoint.
        let graph = compiler
            .compile(state.project(), state.seq_id, TimeTick(TIMEBASE))
            .expect("sequence compiles");
        let plan = &graph.track_plans[0];
        let tr = plan
            .transition
            .as_ref()
            .expect("the transition the editor added must appear in the graph");
        assert!((tr.progress - 0.5).abs() < 1e-4, "progress at the cut should be 0.5, got {}", tr.progress);
        assert!(tr.outgoing.is_some(), "the clip before the cut should be the outgoing layer");
        assert_eq!(graph.media_requests().len(), 2, "both layers need a frame");
    }

    #[test]
    fn a_transition_at_a_clips_head_dissolves_up_from_nothing() {
        // The exact case exercised by hand in the editor: a dissolve on the
        // first clip's head, with the playhead at 0. The region straddles tick
        // 0, so progress there is 0.5 and the clip must be drawn half-strength.
        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        state.add_transition(
            track,
            TimeTick(0),
            timeline::TransitionKind::CrossDissolve,
            TimeTick(TIMEBASE),
        );

        let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
        let graph = compiler
            .compile(state.project(), state.seq_id, state.display_tick())
            .expect("compiles");
        let tr = graph.track_plans[0]
            .transition
            .as_ref()
            .expect("a head transition should be active at tick 0");
        assert!(tr.outgoing.is_none(), "nothing precedes the sequence start");
        assert!(
            (tr.progress - 0.5).abs() < 1e-4,
            "a 1s transition centred on tick 0 is half done at tick 0, got {}",
            tr.progress
        );
    }

    #[test]
    fn the_preview_actually_dims_a_clip_under_a_head_dissolve() {
        // Closes the last gap in the transition chain by *measuring* it. The
        // graph side and the compositor side are each tested on their own, but
        // nothing rendered a real `EditorState` and checked the resulting pixel
        // — and a saturated-red preview at 180/255 versus 255/255 is exactly
        // the difference an eye can't be trusted on in a screenshot.
        let Some((device, queue)) = render::headless_context() else {
            panic!("no GPU adapter — this project cannot run without one, so this is a failure");
        };
        let compositor = render::Compositor::new(device, queue);
        let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());

        let (mut state, _) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        let asset = media::MediaAssetId(1);

        // A solid opaque red source for the clip to show.
        let (w, h) = (16u32, 16u32);
        let red: Vec<u8> = [255u8, 0, 0, 255].iter().copied().cycle().take((w * h * 4) as usize).collect();
        let colour = media::ColorMetadata {
            primaries: media::ColorPrimaries::Rec709,
            transfer: media::TransferFunction::Bt709,
            matrix: media::MatrixCoefficients::Bt709,
            full_range: true,
        };

        let render_at_playhead = |state: &EditorState| -> [u8; 4] {
            let mut sources = render::SourceFrames::default();
            sources.insert(asset, 0, compositor.upload_rgba(&red, w, h, colour));
            let graph = compiler
                .compile(state.project(), state.seq_id, state.display_tick())
                .expect("compiles");
            let (frame, _) = compositor.render_to_rgba(&graph, &sources, render::DeliverySpace::Rec709);
            frame.pixel(frame.width / 2, frame.height / 2)
        };

        // Playhead 0, no transition: the clip at full strength.
        state.playhead = 0;
        let plain = render_at_playhead(&state);
        assert!(plain[0] > 250, "baseline should be full red, got {plain:?}");

        // A 1s dissolve on the clip's head is half done at tick 0, so the clip
        // draws at 50% over nothing.
        state.add_transition(
            track,
            TimeTick(0),
            timeline::TransitionKind::CrossDissolve,
            TimeTick(TIMEBASE),
        );
        let dissolving = render_at_playhead(&state);

        let expected = (render::color::linear_to_rec709(0.5) * 255.0).round() as u8;
        assert!(
            dissolving[0].abs_diff(expected) <= 4,
            "under a half-done dissolve the clip should read ~{expected}/255, got {}",
            dissolving[0]
        );
        assert!(
            plain[0] - dissolving[0] > 40,
            "the dissolve must visibly darken the frame: {} -> {}",
            plain[0],
            dissolving[0]
        );

        // And undoing it restores the full-strength frame, which is what the
        // editor's undo has to mean for a transition.
        state.undo.undo();
        assert_eq!(render_at_playhead(&state), plain, "undo should restore the plain frame");
    }

    #[test]
    fn a_reconstructed_history_cannot_disagree_with_its_project_even_hand_edited() {
        // Schema v3 stored `after` per entry and validated it against the
        // project, dropping the whole history on a mismatch (a hand-edited or
        // buggy-build file could otherwise send the first Ctrl+Z to an
        // unrelated state). Schema v4 removes the redundant copy instead of
        // checking it: `after` is now always derived as "the next entry's
        // `before`, or the project itself for the last one" — so there is
        // nothing left that could disagree. This proves that by construction:
        // even a hand-edited `before` still reconstructs to a chain ending
        // exactly at the saved project.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[2]);
        state.ripple_delete_selection();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edited.nleproj");
        state.save_to(&path);

        let mut doc = project::load(&path).unwrap();
        assert!(!doc.undo_history.is_empty(), "precondition: the file has history");
        // Hand-edit the stored `before` to something arbitrary — the kind of
        // edit that used to produce a stored `after` disagreeing with reality.
        doc.undo_history.last_mut().unwrap().before.sequences[0].tracks[0].clips.clear();
        project::save(&doc, &path).unwrap();

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path), "the project must still open");
        let expected = state.sequence().tracks[0].clips.len();
        assert_eq!(
            fresh.sequence().tracks[0].clips.len(),
            expected,
            "the loaded project is unaffected by a hand-edited history entry"
        );
        assert!(fresh.undo.undo(), "undo must still be offered — there is no disagreement to detect");
        assert!(!fresh.status.contains("discard"), "there is no longer a discard outcome to report");
    }

    #[test]
    fn saved_history_is_capped_to_the_most_recent_entries() {
        // The other half of the size fix: even an in-memory stack at its full
        // 100-entry cap must not write more than
        // `project::MAX_PERSISTED_UNDO_ENTRIES` of them to disk.
        let (mut state, _ids) = state_with_three_clips();
        let track = state.sequence().tracks[0].id;
        for i in 0..(command::UndoStack::DEFAULT_MAX_HISTORY + 10) {
            // A track-gain change is a real, distinct project version and
            // always pushes — unlike marks or selection, which are session
            // state and never touch the undo stack.
            state.set_track_gain(track, (i % 5) as f64, false);
        }
        assert_eq!(
            state.undo.history().len(),
            command::UndoStack::DEFAULT_MAX_HISTORY,
            "precondition: the in-memory stack is at its cap"
        );

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("many.nleproj");
        state.save_to(&path);

        let doc = project::load(&path).unwrap();
        assert_eq!(
            doc.undo_history.len(),
            project::MAX_PERSISTED_UNDO_ENTRIES,
            "the file must carry only the persisted cap, not the full in-memory history"
        );

        // And what's kept is still a correct, undoable chain ending at the
        // saved project — the most recent entries, not an arbitrary slice.
        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        assert!(fresh.undo.undo(), "the kept history must still be usable");
    }

    #[test]
    fn undo_still_works_after_a_save_and_reopen() {
        // The whole promise, and what was missing: `undo_history` was declared
        // in schema v1 and written as an empty list every single time, so
        // reopening a project silently threw away the ability to undo the work
        // in it. Everything else here is bookkeeping around this assertion.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[1]);
        state.ripple_delete_selection();
        assert_eq!(state.sequence().tracks[0].clips.len(), 2);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.nleproj");
        state.save_to(&path);

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        assert_eq!(fresh.sequence().tracks[0].clips.len(), 2, "loads the saved state");

        assert!(fresh.undo.undo(), "there should be history to undo into");
        assert_eq!(
            fresh.sequence().tracks[0].clips.len(),
            3,
            "undo after reopen must bring the deleted clip back"
        );
        assert_eq!(clip_starts(&fresh), vec![0, TIMEBASE, TIMEBASE * 2]);
    }

    #[test]
    fn a_reopened_project_can_redo_what_it_just_undid() {
        // Redo isn't *persisted*, but undoing after a reopen has to create a
        // redo entry like any other undo — otherwise restored history is a
        // one-way trip.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[0]);
        state.ripple_delete_selection();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.nleproj");
        state.save_to(&path);

        let mut fresh = EditorState::new();
        fresh.open_from(&path);
        assert!(!fresh.undo.redo(), "nothing to redo before undoing");
        fresh.undo.undo();
        assert!(fresh.undo.redo(), "undo after reopen must be redoable");
        assert_eq!(fresh.sequence().tracks[0].clips.len(), 2);
    }

    #[test]
    fn an_autosave_snapshot_carries_no_history_so_its_cost_stays_flat() {
        // Autosave runs every 20 seconds; if it serialised the undo stack its
        // cost would grow with session length, which is the opposite of what a
        // safety net should do.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[0]);
        state.ripple_delete_selection();
        assert!(!state.undo.history().is_empty());

        let dir = tempfile::tempdir().unwrap();
        let recovery = dir.path().join(".recover-x.nleproj");
        state.write_snapshot_to(&recovery).unwrap();

        let doc = project::load(&recovery).unwrap();
        assert!(doc.undo_history.is_empty(), "recovery writes must skip history");
        // But the work itself is still there — that's the part that matters.
        assert_eq!(doc.project.sequences[0].tracks[0].clips.len(), 2);
    }

    #[test]
    fn a_project_saved_without_history_still_opens() {
        // Every project written before this change has `undo_history: []`.
        let (state, _) = state_with_three_clips();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nohist.nleproj");
        let doc = project::ProjectDocument::new((**state.project()).clone(), vec![]);
        assert!(doc.undo_history.is_empty());
        project::save(&doc, &path).unwrap();

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&path));
        assert!(!fresh.undo.undo(), "no history to undo, and that's fine");
        assert_eq!(fresh.sequence().tracks[0].clips.len(), 3);
    }

    #[test]
    fn persisted_undo_history_size_is_measured_not_assumed() {
        // `docs/decisions-log.md` bounds history "to keep autosave file size
        // predictable" — but nobody ever measured what it actually costs, and
        // each `PersistedCommand` stores TWO full project snapshots. This puts
        // a number on it and fails if that number becomes unreasonable.
        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        let mut clips = Vec::new();
        for i in 0..40i64 {
            let start = i * TIMEBASE;
            let mut c = ClipInstance {
                id: ClipInstanceId(1000 + i as u64),
                source: ClipSource::Media(media::MediaAssetId(1)),
                source_in: TimeTick(0),
                source_out: TimeTick(TIMEBASE),
                timeline_in: TimeTick(start),
                timeline_out: TimeTick(start + TIMEBASE),
                speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
                effects: vec![],
                audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
                linked_group: None,
            };
            // A keyframed effect, so clips aren't unrealistically tiny.
            let mut params = std::collections::BTreeMap::new();
            let mut track = timeline::ParamTrack::constant(ParamValue::Number(1.0));
            for k in 0..8 {
                track.upsert_keyframe(
                    TimeTick(k * TIMEBASE / 8),
                    ParamValue::Number(k as f64),
                    timeline::InterpolationMode::Linear,
                );
            }
            params.insert("sigma".to_string(), track);
            c.effects.push(timeline::EffectInstance {
                id: timeline::EffectInstanceId(2000 + i as u64),
                effect_type: "gaussian_blur".into(),
                enabled: true,
                params,
            });
            clips.push(c);
        }
        project.sequences[0].tracks.push(Track {
            id: TrackId(1),
            kind: TrackKind::Video,
            name: "V1".into(),
            clips,
            transitions: vec![],
            gain_db: timeline::unity_gain(),
            pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        });
        project.assets.push(fake_asset(1, "C:/media/clip.mp4"));
        state.undo.push("setup", std::sync::Arc::new(project));

        let dir = tempfile::tempdir().unwrap();
        let no_history = dir.path().join("plain.nleproj");
        state.write_snapshot_to(&no_history).unwrap();
        let baseline = std::fs::metadata(&no_history).unwrap().len();

        // Fill the bounded history right up to its cap.
        let cap = command::UndoStack::DEFAULT_MAX_HISTORY;
        for i in 0..cap {
            let mut p = (**state.project()).clone();
            p.sequences[0].tracks[0].clips[i % 40].timeline_in = TimeTick(i as i64);
            state.undo.push("edit", std::sync::Arc::new(p));
        }
        assert_eq!(state.undo.history().len(), cap, "history should be at its cap");

        let with_history = dir.path().join("full.nleproj");
        state.save_to(&with_history);
        let full = std::fs::metadata(&with_history).unwrap().len();

        let multiplier = full as f64 / baseline as f64;
        println!(
            "project {:.1} KB alone, {:.1} KB with {cap} undo entries ({multiplier:.0}x)",
            baseline as f64 / 1024.0,
            full as f64 / 1024.0
        );

        // Two things make the naive encoding wasteful, and both are fixed:
        //  - every entry stored `before` AND `after`, but `history[i].after` is
        //    always `history[i+1].before`, so half the bytes were duplicates;
        //  - all 100 in-memory entries were written, when far fewer are useful
        //    after reopening a project.
        // Measured at 199x the bare project before those fixes.
        assert!(
            multiplier < 40.0,
            "undo history inflated the project file {multiplier:.0}x ({:.1} MB) —              the before/after duplication or the persisted cap has regressed",
            full as f64 / (1024.0 * 1024.0)
        );
    }

    #[test]
    fn parking_at_the_sequence_end_still_renders_the_last_frame() {
        // Found by watching real playback: it stops with the playhead exactly on
        // the duration, which is an exclusive out point, so the graph compiled
        // there had no active clip and the preview went black the moment
        // playback finished.
        let (mut state, _) = state_with_three_clips();
        let end = state.sequence_duration_ticks();
        assert_eq!(end, TIMEBASE * 3);

        state.playhead = end;
        assert_eq!(
            state.display_tick(),
            TimeTick(end - 1),
            "at the out point the preview must fall back inside the last frame"
        );

        // Everywhere else it's the playhead untouched.
        state.playhead = TIMEBASE;
        assert_eq!(state.display_tick(), TimeTick(TIMEBASE));
        state.playhead = -50;
        assert_eq!(state.display_tick(), TimeTick(0), "never negative");
    }

    #[test]
    fn display_tick_is_zero_for_an_empty_sequence() {
        // An empty sequence has duration 0, so the clamp's "one tick inside"
        // would go negative.
        let state = EditorState::new();
        assert_eq!(state.sequence_duration_ticks(), 0);
        assert_eq!(state.display_tick(), TimeTick(0));
    }

    #[test]
    fn lift_leaves_a_gap_and_ripple_delete_closes_it() {
        // The distinction that makes both shortcuts worth having, and the one
        // that's silently wrong if Delete and Shift+Delete are wired to the
        // same op.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[1]); // the middle clip
        state.lift_selection();
        assert_eq!(
            clip_starts(&state),
            vec![0, TIMEBASE * 2],
            "lift must leave the third clip where it was"
        );

        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[1]);
        state.ripple_delete_selection();
        assert_eq!(
            clip_starts(&state),
            vec![0, TIMEBASE],
            "ripple delete must pull the third clip back to close the gap"
        );
    }

    #[test]
    fn deleting_a_multi_clip_selection_is_a_single_undo_step() {
        // One Ctrl+Z should restore everything the delete removed. Pushing one
        // undo entry per clip would make the user press undo N times and see
        // half-deleted intermediate states.
        let (mut state, ids) = state_with_three_clips();
        state.selected_clips = vec![ids[0], ids[1]];
        state.ripple_delete_selection();
        assert_eq!(state.sequence().tracks[0].clips.len(), 1);

        state.undo.undo();
        assert_eq!(
            state.sequence().tracks[0].clips.len(),
            3,
            "a single undo must bring back both deleted clips"
        );
    }

    #[test]
    fn a_failed_multi_op_leaves_the_project_untouched() {
        // All-or-nothing: a selection containing something undeletable must not
        // half-apply, or the user is left with a project neither state.
        let (mut state, ids) = state_with_three_clips();
        let before = state.project().clone();
        let ops = vec![
            EditOp::Extract { clip: ids[0] },
            EditOp::Extract { clip: ClipInstanceId(99999) }, // does not exist
        ];
        assert!(!state.apply_ops("delete", &ops));
        assert!(
            std::sync::Arc::ptr_eq(state.project(), &before),
            "a failed batch must not push a new project version at all"
        );
    }

    #[test]
    fn paste_gives_the_copy_a_fresh_id_and_keeps_relative_spacing() {
        // Reusing the source ID would put two clips with the same identity in
        // one sequence, and every op locates clips by ID — the duplicate would
        // shadow the original and edits would hit the wrong one.
        let (mut state, ids) = state_with_three_clips();
        state.selected_clips = vec![ids[0], ids[1]];
        state.copy_selection();
        assert_eq!(state.clipboard.len(), 2);

        state.playhead = TIMEBASE * 10;
        state.paste_at_playhead();

        let clips = &state.sequence().tracks[0].clips;
        let pasted: Vec<&ClipInstance> =
            clips.iter().filter(|c| c.timeline_in.0 >= TIMEBASE * 10).collect();
        assert_eq!(pasted.len(), 2, "both copied clips should land");
        let mut starts: Vec<i64> = pasted.iter().map(|c| c.timeline_in.0).collect();
        starts.sort_unstable();
        assert_eq!(
            starts,
            vec![TIMEBASE * 10, TIMEBASE * 11],
            "the 1s gap between the originals must be preserved"
        );
        for p in &pasted {
            assert!(!ids.contains(&p.id), "pasted clip reused a source ID: {:?}", p.id);
        }
        // And no duplicate IDs anywhere in the sequence.
        let mut all: Vec<u64> = clips.iter().map(|c| c.id.0).collect();
        all.sort_unstable();
        let count = all.len();
        all.dedup();
        assert_eq!(all.len(), count, "sequence has duplicate clip IDs after paste");
    }

    #[test]
    fn paste_survives_deleting_the_clip_it_was_copied_from() {
        // The clipboard stores whole clips, not references — copy, delete,
        // paste is an ordinary move-by-cut-and-paste and must work.
        let (mut state, ids) = state_with_three_clips();
        state.select_only(ids[0]);
        state.copy_selection();
        state.ripple_delete_selection();
        state.playhead = TIMEBASE * 5;
        state.paste_at_playhead();
        assert!(
            state
                .sequence()
                .tracks[0]
                .clips
                .iter()
                .any(|c| c.timeline_in.0 == TIMEBASE * 5),
            "paste should still work after the source clip is gone"
        );
    }

    #[test]
    fn snapping_takes_the_nearest_candidate_and_ignores_distant_ones() {
        let (mut state, _) = state_with_three_clips();
        state.playhead = 0;
        let tol = TIMEBASE / 10;

        // Just shy of the 1s clip boundary — should snap onto it.
        assert_eq!(state.snap_tick(TIMEBASE - 1000, None, tol), TIMEBASE);
        // Far from everything — must be left exactly alone, or dragging becomes
        // impossible anywhere near a busy timeline.
        let free = TIMEBASE / 2;
        assert_eq!(state.snap_tick(free, None, tol), free);
    }

    #[test]
    fn a_dragged_clip_cannot_snap_to_its_own_edges() {
        // Without the exclusion, a clip snaps to where it already is and can
        // never be nudged off that spot.
        let (mut state, ids) = state_with_three_clips();
        state.playhead = -1; // keep the playhead out of the candidate set
        let tol = TIMEBASE / 10;
        // Clip 3's *end* at 3s, which is the only edge in this layout belonging
        // to exactly one clip. Its start at 2s is also clip 2's end, so
        // excluding clip 3 wouldn't remove that candidate — abutting clips
        // share a boundary, and snapping there is correct, not a bug.
        let near_own_end = TIMEBASE * 3 + 1000;

        assert_eq!(
            state.snap_tick(near_own_end, None, tol),
            TIMEBASE * 3,
            "without exclusion it snaps to the clip's own end"
        );
        assert_eq!(
            state.snap_tick(near_own_end, Some(ids[2]), tol),
            near_own_end,
            "excluding the dragged clip must leave the tick alone"
        );
    }

    #[test]
    fn snapping_disabled_is_a_no_op() {
        let (mut state, _) = state_with_three_clips();
        state.snapping = false;
        let t = TIMEBASE - 1000;
        assert_eq!(state.snap_tick(t, None, TIMEBASE / 10), t);
    }

    #[test]
    fn marks_never_end_up_inverted() {
        // An out point before the in point would make every range operation
        // need its own "is this backwards" check.
        let mut state = EditorState::new();
        state.playhead = TIMEBASE * 5;
        state.mark_out();
        state.playhead = TIMEBASE * 8;
        state.mark_in(); // in after out
        assert_eq!(state.in_point, Some(TIMEBASE * 8));
        assert_eq!(state.out_point, None, "the now-invalid out point must be dropped");
        assert!(state.marked_range().is_none());

        state.playhead = TIMEBASE * 9;
        state.mark_out();
        assert_eq!(state.marked_range(), Some((TIMEBASE * 8, TIMEBASE * 9)));
    }

    #[test]
    fn ctrl_click_toggles_membership_and_re_adding_promotes_to_primary() {
        // The effects panel edits the primary selection, so a Ctrl+click that
        // brings a clip *into* the selection must also bring it into focus.
        // Ctrl+clicking one that's already selected removes it — that's the
        // point of a toggle, not a bug to work around.
        let (mut state, ids) = state_with_three_clips();
        state.toggle_in_selection(ids[0]);
        state.toggle_in_selection(ids[1]);
        assert_eq!(state.selected_clips, vec![ids[0], ids[1]]);
        assert_eq!(state.primary_selection(), Some(ids[1]));

        state.toggle_in_selection(ids[0]); // deselect
        assert_eq!(state.selected_clips, vec![ids[1]]);

        state.toggle_in_selection(ids[0]); // re-select, now last = primary
        assert_eq!(state.selected_clips, vec![ids[1], ids[0]]);
        assert_eq!(
            state.primary_selection(),
            Some(ids[0]),
            "a clip brought back into the selection must become the primary one"
        );
    }

    /// A clip at 1s..2s on the timeline carrying one keyframeable param, so
    /// the clip-relative conversion is actually exercised (a clip starting at
    /// 0 would make local and sequence time identical and hide the bug).
    fn state_with_keyframeable_clip() -> (EditorState, ClipInstanceId, timeline::EffectInstanceId) {
        let mut state = state_with_high_ids();
        let clip_id = ClipInstanceId(900);
        let effect_id = timeline::EffectInstanceId(1234);
        let mut project = (**state.project()).clone();
        let clip = &mut project.sequences[0].tracks[0].clips[0];
        clip.timeline_in = TimeTick(TIMEBASE);
        clip.timeline_out = TimeTick(TIMEBASE * 2);
        clip.effects[0].params.insert(
            "sigma".to_string(),
            timeline::ParamTrack::constant(ParamValue::Number(1.0)),
        );
        state.undo.push("setup kf", std::sync::Arc::new(project));
        (state, clip_id, effect_id)
    }

    fn track_of(
        state: &EditorState,
        clip_id: ClipInstanceId,
        effect_id: timeline::EffectInstanceId,
    ) -> timeline::ParamTrack {
        let (_, clip) = state.find_clip(clip_id).unwrap();
        clip.effects
            .iter()
            .find(|e| e.id == effect_id)
            .unwrap()
            .params
            .get("sigma")
            .unwrap()
            .clone()
    }

    #[test]
    fn keyframe_times_are_clip_relative_not_sequence_relative() {
        // The bug this guards: writing the sequence tick as the keyframe time.
        // `render::graph` evaluates params at `playhead - clip.timeline_in`, so
        // a keyframe stored at sequence time would animate at the wrong moment
        // — and would break entirely as soon as the clip was moved.
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        // Playhead 1.5s: half a second into a clip that starts at 1s.
        state.playhead = TIMEBASE + TIMEBASE / 2;
        let local = state.playhead_local_to_clip(clip_id).unwrap();
        assert_eq!(local, TimeTick(TIMEBASE / 2), "local time must subtract timeline_in");

        state.toggle_param_animation(clip_id, effect_id, "sigma", local);
        let track = track_of(&state, clip_id, effect_id);
        assert_eq!(track.keyframes.len(), 1);
        assert_eq!(
            track.keyframes[0].at,
            TimeTick(TIMEBASE / 2),
            "keyframe must be stored at clip-relative time"
        );
    }

    #[test]
    fn playhead_outside_the_clip_has_no_local_time() {
        let (mut state, clip_id, _) = state_with_keyframeable_clip();
        state.playhead = 0; // before the clip, which starts at 1s
        assert!(state.playhead_local_to_clip(clip_id).is_none());
        state.playhead = TIMEBASE * 3; // after it ends at 2s
        assert!(state.playhead_local_to_clip(clip_id).is_none());
        // The out point is exclusive — the frame at timeline_out belongs to
        // whatever comes next, so keyframing there would target the wrong clip.
        state.playhead = TIMEBASE * 2;
        assert!(state.playhead_local_to_clip(clip_id).is_none());
    }

    #[test]
    fn editing_an_animated_param_writes_a_keyframe_instead_of_the_constant() {
        // Without this, the slider would edit `default` — which `evaluate_at`
        // ignores completely once any keyframe exists — so dragging it would
        // appear to do nothing at all.
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE; // clip start, local 0
        let local = state.playhead_local_to_clip(clip_id).unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", local);

        // Move to 0.5s into the clip and set a different value.
        state.playhead = TIMEBASE + TIMEBASE / 2;
        let later = state.playhead_local_to_clip(clip_id).unwrap();
        state.set_param_value_at(
            clip_id,
            effect_id,
            "sigma",
            ParamValue::Number(9.0),
            Some(later),
            false,
        );

        let track = track_of(&state, clip_id, effect_id);
        assert_eq!(track.keyframes.len(), 2, "should have added a second keyframe");
        assert_eq!(
            track.evaluate_at(later).as_scalar(),
            Some(9.0),
            "the new value must be what the renderer reads at that tick"
        );
        assert_eq!(
            track.evaluate_at(TimeTick(0)).as_scalar(),
            Some(1.0),
            "the original keyframe must keep its value — this is an animation, not a constant"
        );
    }

    #[test]
    fn a_non_animated_param_still_edits_its_constant() {
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE;
        let local = state.playhead_local_to_clip(clip_id);
        state.set_param_value_at(
            clip_id,
            effect_id,
            "sigma",
            ParamValue::Number(5.0),
            local,
            false,
        );
        let track = track_of(&state, clip_id, effect_id);
        assert!(!track.is_animated(), "editing a constant must not create keyframes");
        assert_eq!(track.default.as_scalar(), Some(5.0));
    }

    #[test]
    fn toggling_animation_off_holds_the_value_at_the_playhead() {
        // Premiere's behaviour, and the non-surprising one: turning the
        // stopwatch off must not change the frame you're looking at. Collapsing
        // to the old `default` instead would make the picture jump.
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE;
        let start = state.playhead_local_to_clip(clip_id).unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", start);
        state.playhead = TIMEBASE + TIMEBASE / 2;
        let mid = state.playhead_local_to_clip(clip_id).unwrap();
        state.set_param_value_at(clip_id, effect_id, "sigma", ParamValue::Number(8.0), Some(mid), false);

        // Park a quarter into the clip, where the curve reads ~4.5 (halfway
        // between 1.0 at local 0 and 8.0 at local 0.5s), then de-animate.
        state.playhead = TIMEBASE + TIMEBASE / 4;
        let quarter = state.playhead_local_to_clip(clip_id).unwrap();
        let before = track_of(&state, clip_id, effect_id)
            .evaluate_at(quarter)
            .as_scalar()
            .unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", quarter);

        let track = track_of(&state, clip_id, effect_id);
        assert!(!track.is_animated());
        let after = track.evaluate_at(quarter).as_scalar().unwrap();
        assert!(
            (after - before).abs() < 1e-9,
            "de-animating changed the rendered value from {before} to {after}"
        );
        assert!(
            (before - 4.5).abs() < 0.01,
            "sanity: the linear curve should read ~4.5 a quarter in, got {before}"
        );
    }

    #[test]
    fn set_keyframe_tangents_writes_the_handles_at_the_playhead_and_is_undoable() {
        // The engine has evaluated `Keyframe::tangents` since M4a and the file
        // format has always stored them, but nothing could write one — this is
        // the state-layer half of the tangent-handle drag UI.
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE;
        let local = state.playhead_local_to_clip(clip_id).unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", local);
        state.set_keyframe_interpolation(
            clip_id,
            effect_id,
            "sigma",
            local,
            timeline::InterpolationMode::Bezier,
        );

        let tangents = ((-30.0, -0.4), (30.0, 0.4));
        state.set_keyframe_tangents(clip_id, effect_id, "sigma", local, tangents);

        let track = track_of(&state, clip_id, effect_id);
        assert_eq!(track.keyframes[0].tangents, Some(tangents));

        state.undo.undo();
        assert_eq!(
            track_of(&state, clip_id, effect_id).keyframes[0].tangents,
            None,
            "undo should remove the tangent edit like any other keyframe change"
        );
    }

    #[test]
    fn animating_an_off_clip_param_leaves_the_curve_alone() {
        // With the playhead off the clip there's no keyframe time to write to.
        // Falling back to editing `default` would silently corrupt the
        // animation with a value the renderer never shows.
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE;
        let local = state.playhead_local_to_clip(clip_id).unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", local);
        let before = track_of(&state, clip_id, effect_id);

        state.playhead = 0; // off the clip
        state.set_param_value_at(clip_id, effect_id, "sigma", ParamValue::Number(99.0), None, false);

        assert_eq!(track_of(&state, clip_id, effect_id), before, "curve must be untouched");
    }

    #[test]
    fn keyframe_edits_are_undoable() {
        let (mut state, clip_id, effect_id) = state_with_keyframeable_clip();
        state.playhead = TIMEBASE;
        let local = state.playhead_local_to_clip(clip_id).unwrap();
        state.toggle_param_animation(clip_id, effect_id, "sigma", local);
        assert!(track_of(&state, clip_id, effect_id).is_animated());
        state.undo.undo();
        assert!(
            !track_of(&state, clip_id, effect_id).is_animated(),
            "undo must remove the keyframe the stopwatch added"
        );
    }

    #[test]
    fn open_resolves_media_that_moved_with_the_project_folder() {
        // The whole point of storing a relative path: copy the project
        // folder somewhere else and the media still resolves, even though
        // every recorded absolute path is now wrong.
        let original = tempfile::tempdir().unwrap();
        let media = original.path().join("footage").join("a.mp4");
        std::fs::create_dir_all(media.parent().unwrap()).unwrap();
        std::fs::write(&media, b"x").unwrap();

        let mut state = EditorState::new();
        let mut project = (**state.project()).clone();
        project.assets.push(fake_asset(1, &media.to_string_lossy()));
        state.undo.push("setup", std::sync::Arc::new(project));
        let proj_path = original.path().join("p.nleproj");
        state.save_to(&proj_path);

        // Simulate the move: same layout, different parent directory.
        let moved = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(moved.path().join("footage")).unwrap();
        std::fs::copy(&media, moved.path().join("footage").join("a.mp4")).unwrap();
        let moved_proj = moved.path().join("p.nleproj");
        std::fs::copy(&proj_path, &moved_proj).unwrap();

        let mut fresh = EditorState::new();
        assert!(fresh.open_from(&moved_proj));
        assert_eq!(
            fresh.asset_paths.get(&media::MediaAssetId(1)),
            Some(&moved.path().join("footage").join("a.mp4")),
            "should resolve via the relative path, not the stale absolute one"
        );
    }
}
