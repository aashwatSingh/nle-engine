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
    /// trim arithmetic every frame. Populated by the Transcribe and Generate
    /// Captions analyses (captioning transcribes anyway, so it stores the
    /// same result rather than throwing it away). Not persisted in the project
    /// file — regenerating on demand is cheap enough, and a session-only
    /// cache avoids growing the save format for derived data.
    ///
    /// Private, and read through `EditorState::transcript`: those ticks are
    /// only meaningful while the clip still sits where it did when they were
    /// computed, and the accessor is what enforces that.
    transcripts: std::collections::HashMap<ClipInstanceId, transcript::StoredTranscript>,
    /// Scene-cut, silence, beat, loudness, stabilize, caption and transcribe
    /// jobs running in the background — see `analysis_jobs`.
    analysis: analysis_jobs::AnalysisJobs,
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

mod selection;
mod tracks;
mod titles;
mod analysis;
mod analysis_jobs;
mod transcript;

pub use analysis_jobs::AnalysisKind;
pub use transcript::Transcript;
mod editing;
mod keyframes;
mod project_io;

#[cfg(test)]
mod tests;

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
            analysis: analysis_jobs::AnalysisJobs::default(),
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

}


/// The `(numerator, denominator)` behind a `SpeedCurve::Constant`, if it's one
/// every analysis action in `analysis.rs`/`transcript.rs` can safely map
/// timeline ticks through — `None` for `Keyframed` (see `scene_cut_ops`'s doc
/// for why a keyframed curve can't use this closed-form mapping) and for a
/// zero or negative numerator or denominator, which nothing in the UI can
/// create but a hand-edited or corrupted project file can.
///
/// A project file is untrusted input, and until this existed nothing checked
/// a clip's speed before dividing by it: `* denominator / numerator` reached
/// a zero numerator directly, panicking with a divide-by-zero on the UI
/// thread the moment a scene-cut, silence, beat or caption result was applied
/// against such a clip. Every one of those call sites should go through this
/// rather than destructuring `SpeedCurve::Constant` directly.
fn constant_speed(speed: &SpeedCurve) -> Option<(i64, i64)> {
    match *speed {
        SpeedCurve::Constant { numerator, denominator } if numerator > 0 && denominator > 0 => {
            Some((numerator, denominator))
        }
        _ => None,
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
