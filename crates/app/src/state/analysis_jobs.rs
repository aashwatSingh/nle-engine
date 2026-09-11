//! Background execution for the media-analysis actions — scene cuts,
//! silence, beats, loudness, stabilization, captions and transcription.
//!
//! These used to run inside their button's click handler, on the UI thread:
//! a whole-clip decode plus analysis with no feedback, which froze the
//! editor for as long as it took — over twenty minutes on real footage in
//! August. Now a click snapshots what the analysis needs, runs it on a
//! worker thread, and `EditorState::poll_analysis` applies the result on the
//! UI thread when it lands.
//!
//! The one hard question is what happens when the user edits while a job
//! runs. The answer is `Placement`: a result is only applied if its clip is
//! still where it was, cut the same way, at the same speed.

use super::*;
use std::collections::HashSet;
use std::sync::mpsc;

/// Which analysis a job is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnalysisKind {
    SceneCuts,
    Silence,
    Beats,
    Loudness,
    Stabilize,
    Captions,
    Transcribe,
}

impl AnalysisKind {
    /// Shown in place of the button while the job runs.
    pub fn running_label(self) -> &'static str {
        match self {
            AnalysisKind::SceneCuts => "Detecting scene cuts…",
            AnalysisKind::Silence => "Finding silence…",
            AnalysisKind::Beats => "Detecting beats…",
            AnalysisKind::Loudness => "Measuring loudness…",
            AnalysisKind::Stabilize => "Stabilizing…",
            AnalysisKind::Captions | AnalysisKind::Transcribe => "Transcribing…",
        }
    }
}

/// What a finished analysis computed. Plain data — nothing is applied to
/// the project until `EditorState::apply_finished` does it.
pub(super) enum Outcome {
    /// Source ticks where a hard cut was found.
    SceneCuts(Vec<i64>),
    /// Gaps to remove, in seconds relative to the clip's `source_in`.
    Silence(Vec<(f64, f64)>),
    /// Onsets in absolute source seconds.
    Beats(Vec<f64>),
    Loudness { gain_db: f64, target_lufs: f64 },
    /// `(timeline_tick, position offset)` keyframes.
    Stabilize(super::analysis::StabilizationKeyframes),
    Captions(Vec<speech::Segment>),
    Transcribe(Vec<speech::Segment>),
}

/// Why an analysis produced nothing to apply.
#[derive(Debug)]
pub(super) enum AnalysisError {
    NoMedia,
    NoAudio,
    Failed(String),
}

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalysisError::NoMedia => write!(f, "the clip's media file can't be found"),
            AnalysisError::NoAudio => write!(f, "this clip has no audio to analyse"),
            AnalysisError::Failed(message) => write!(f, "{message}"),
        }
    }
}

/// The parts of a clip every analysis result is computed against: which
/// track it's on, what media and which slice of it, where it sits, and how
/// fast it plays.
///
/// Compared whole, deliberately. Some results would survive some of these
/// changing — scene cuts are source-relative, so a pure move doesn't break
/// them — but stabilization writes timeline positions and silence ranges are
/// measured from the old trim point. One rule for all seven actions is the
/// rule that can never cut in the wrong place; the cost is re-running an
/// analysis after a nudge that happened to be harmless. Edits that don't
/// touch these fields — effects, gain, other clips — leave a result valid.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Placement {
    track: TrackId,
    source: ClipSource,
    source_in: TimeTick,
    source_out: TimeTick,
    timeline_in: TimeTick,
    timeline_out: TimeTick,
    speed: SpeedCurve,
}

impl Placement {
    pub(super) fn of(track: TrackId, clip: &ClipInstance) -> Self {
        Placement {
            track,
            source: clip.source.clone(),
            source_in: clip.source_in,
            source_out: clip.source_out,
            timeline_in: clip.timeline_in,
            timeline_out: clip.timeline_out,
            speed: clip.speed.clone(),
        }
    }
}

/// A job that has finished running, waiting to be applied.
pub(super) struct Finished {
    pub(super) clip_id: ClipInstanceId,
    pub(super) kind: AnalysisKind,
    pub(super) placement: Placement,
    pub(super) result: Result<Outcome, AnalysisError>,
}

pub struct AnalysisJobs {
    in_flight: HashSet<(ClipInstanceId, AnalysisKind)>,
    tx: mpsc::Sender<Finished>,
    rx: mpsc::Receiver<Finished>,
}

impl Default for AnalysisJobs {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        AnalysisJobs { in_flight: HashSet::new(), tx, rx }
    }
}

impl AnalysisJobs {
    pub fn is_running(&self, clip: ClipInstanceId, kind: AnalysisKind) -> bool {
        self.in_flight.contains(&(clip, kind))
    }

    /// Runs `work` on a worker thread. Refuses — returns `false` — if the
    /// same action is already running on the same clip; a different action,
    /// or the same action on another clip, runs independently.
    ///
    /// A panic inside `work` comes back as `AnalysisError::Failed` rather
    /// than never reporting at all. Export and proxy jobs don't do this, and
    /// a panicking decode there leaves the job marked running forever — the
    /// August audit's "worker panic strands jobs" finding. New code shouldn't
    /// repeat it.
    pub(super) fn spawn(
        &mut self,
        clip: ClipInstanceId,
        kind: AnalysisKind,
        placement: Placement,
        work: impl FnOnce() -> Result<Outcome, AnalysisError> + Send + 'static,
    ) -> bool {
        if !self.in_flight.insert((clip, kind)) {
            return false;
        }
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
                .unwrap_or_else(|_| Err(AnalysisError::Failed("the analysis crashed partway through".into())));
            // The receiver is gone if the project was closed or reopened while
            // this ran; the result belongs to a project that no longer exists.
            let _ = tx.send(Finished { clip_id: clip, kind, placement, result });
        });
        true
    }

    /// Everything that finished since the last call.
    pub(super) fn drain(&mut self) -> Vec<Finished> {
        let mut done = Vec::new();
        while let Ok(finished) = self.rx.try_recv() {
            self.in_flight.remove(&(finished.clip_id, finished.kind));
            done.push(finished);
        }
        done
    }

    /// Queues an already-finished job, as if a worker had just reported it.
    #[cfg(test)]
    pub(super) fn inject_for_test(&mut self, finished: Finished) {
        self.in_flight.insert((finished.clip_id, finished.kind));
        self.tx.send(finished).expect("our own receiver is alive");
    }
}

type Work = Box<dyn FnOnce() -> Result<Outcome, AnalysisError> + Send>;

impl AnalysisKind {
    /// For "couldn't …" messages.
    fn verb(self) -> &'static str {
        match self {
            AnalysisKind::SceneCuts => "detect scene cuts",
            AnalysisKind::Silence => "remove silence",
            AnalysisKind::Beats => "detect beats",
            AnalysisKind::Loudness => "match loudness",
            AnalysisKind::Stabilize => "stabilize",
            AnalysisKind::Captions => "generate captions",
            AnalysisKind::Transcribe => "transcribe",
        }
    }

    /// For "… discarded" messages.
    fn noun(self) -> &'static str {
        match self {
            AnalysisKind::SceneCuts => "Scene cut detection",
            AnalysisKind::Silence => "Silence removal",
            AnalysisKind::Beats => "Beat detection",
            AnalysisKind::Loudness => "Loudness matching",
            AnalysisKind::Stabilize => "Stabilization",
            AnalysisKind::Captions => "Captioning",
            AnalysisKind::Transcribe => "Transcription",
        }
    }

    /// Everything but loudness maps source time onto the timeline, which is
    /// only a closed-form division at constant speed — see `scene_cut_ops`.
    fn needs_constant_speed(self) -> bool {
        self != AnalysisKind::Loudness
    }
}

/// "1 caption", "3 captions".
fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

impl EditorState {
    pub fn analysis_running(&self, clip: ClipInstanceId, kind: AnalysisKind) -> bool {
        self.analysis.is_running(clip, kind)
    }

    /// Starts `kind` on `clip_id` on a worker thread and returns at once;
    /// `poll_analysis` applies the result when it's ready. `target_lufs` is
    /// only read by `AnalysisKind::Loudness`.
    ///
    /// Returns whether a job started. A missing clip or a speed curve the
    /// action can't map is refused here, with the reason in `status`, rather
    /// than discovered after minutes of decoding. A repeat of an action
    /// already running on this clip is refused quietly — its spinner is
    /// already showing.
    pub fn start_analysis(&mut self, clip_id: ClipInstanceId, kind: AnalysisKind, target_lufs: f64) -> bool {
        let Some((track, clip)) = self.find_clip(clip_id) else {
            self.status = format!("couldn't {} — the clip no longer exists", kind.verb());
            return false;
        };
        if kind.needs_constant_speed() && constant_speed(&clip.speed).is_none() {
            self.status = format!("couldn't {} — this only works on clips playing at a constant speed", kind.verb());
            return false;
        }

        let placement = Placement::of(track, &clip);
        // Snapshots, so the worker never touches `self`. The path map is a
        // handful of `PathBuf`s; cloning it is nothing next to the decode.
        let paths = self.asset_paths.clone();
        let rate = self.sequence().settings.sample_rate;
        let work: Work = match kind {
            AnalysisKind::SceneCuts => {
                Box::new(move || super::analysis::scene_cut_ticks(&paths, &clip).map(Outcome::SceneCuts))
            }
            AnalysisKind::Silence => {
                Box::new(move || super::analysis::silence_ranges(&paths, rate, &clip).map(Outcome::Silence))
            }
            AnalysisKind::Beats => {
                Box::new(move || super::analysis::beat_onsets(&paths, rate, &clip).map(Outcome::Beats))
            }
            AnalysisKind::Loudness => Box::new(move || {
                super::analysis::loudness_gain(&paths, rate, &clip, target_lufs)
                    .map(|gain_db| Outcome::Loudness { gain_db, target_lufs })
            }),
            AnalysisKind::Stabilize => {
                Box::new(move || super::analysis::stabilization_keyframes(&paths, &clip).map(Outcome::Stabilize))
            }
            AnalysisKind::Captions => {
                Box::new(move || super::transcript::transcribe_segments(&paths, rate, &clip).map(Outcome::Captions))
            }
            AnalysisKind::Transcribe => {
                Box::new(move || super::transcript::transcribe_segments(&paths, rate, &clip).map(Outcome::Transcribe))
            }
        };

        if !self.analysis.spawn(clip_id, kind, placement, work) {
            return false;
        }
        self.status = kind.running_label().into();
        true
    }

    /// Applies every analysis that has finished since the last call. Call
    /// once per frame.
    pub fn poll_analysis(&mut self) {
        for finished in self.analysis.drain() {
            self.apply_finished(finished);
        }
    }

    fn apply_finished(&mut self, finished: Finished) {
        let Finished { clip_id, kind, placement, result } = finished;
        let Some((track, clip)) = self.find_clip(clip_id) else {
            self.status = format!("{} discarded — the clip was deleted while it ran", kind.noun());
            return;
        };
        if Placement::of(track, &clip) != placement {
            self.status = format!(
                "{} discarded — the clip was moved, trimmed or re-timed while it ran; run it again",
                kind.noun()
            );
            return;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(e) => {
                self.status = format!("couldn't {} — {e}", kind.verb());
                return;
            }
        };

        self.status.clear();
        let message = match outcome {
            Outcome::SceneCuts(ticks) => match self.apply_scene_cuts(track, &clip, &ticks) {
                0 => "no scene cuts found".to_string(),
                n => format!("found and split at {}", count(n, "scene cut")),
            },
            Outcome::Silence(ranges) => match self.apply_silence_removal(track, &clip, &ranges) {
                0 => "no silence found to remove".to_string(),
                n => format!("removed {}", count(n, "silent gap")),
            },
            Outcome::Beats(onsets) => match self.apply_beat_markers(&clip, &onsets) {
                0 => "no beats found".to_string(),
                n => format!("added {}", count(n, "beat marker")),
            },
            Outcome::Loudness { gain_db, target_lufs } => {
                if self.apply_loudness_match_gain(clip_id, gain_db) {
                    format!("applied {gain_db:+.1}dB to reach {target_lufs:.0} LUFS")
                } else {
                    "couldn't match loudness — the clip's gain is keyframed".to_string()
                }
            }
            Outcome::Stabilize(keyframes) => match self.apply_stabilization_keyframes(clip_id, &keyframes) {
                0 => "couldn't stabilize — too short, or position is already keyframed".to_string(),
                n => format!("wrote {}", count(n, "stabilization keyframe")),
            },
            Outcome::Captions(segments) => match self.apply_captions(clip_id, &clip, &segments) {
                0 => "no speech found to caption".to_string(),
                n => format!("generated {}", count(n, "caption")),
            },
            Outcome::Transcribe(segments) => match self.store_transcript(clip_id, &clip, &segments) {
                0 => "no speech found".to_string(),
                n => format!("transcribed {}", count(n, "word")),
            },
        };
        // An edit that failed validation has already put the rejected op in
        // `status` (`apply_ops` does); "none found" would hide that.
        if self.status.is_empty() {
            self.status = message;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn placement() -> Placement {
        Placement {
            track: TrackId(1),
            source: ClipSource::Media(media::MediaAssetId(1)),
            source_in: TimeTick(0),
            source_out: TimeTick(TIMEBASE),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(TIMEBASE),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        }
    }

    /// Drains until `count` jobs have reported, or five seconds pass.
    fn wait_for(jobs: &mut AnalysisJobs, count: usize) -> Vec<Finished> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut done = Vec::new();
        while done.len() < count && Instant::now() < deadline {
            done.extend(jobs.drain());
            std::thread::sleep(Duration::from_millis(5));
        }
        done
    }

    #[test]
    fn a_job_that_panics_is_reported_as_failed_instead_of_running_forever() {
        let mut jobs = AnalysisJobs::default();
        let clip = ClipInstanceId(1);

        assert!(jobs.spawn(clip, AnalysisKind::SceneCuts, placement(), || panic!("decoder blew up")));
        let done = wait_for(&mut jobs, 1);

        assert_eq!(done.len(), 1, "a panicking job must still report back");
        assert!(matches!(done[0].result, Err(AnalysisError::Failed(_))));
        assert!(!jobs.is_running(clip, AnalysisKind::SceneCuts), "and must not stay marked as running");
    }

    #[test]
    fn the_same_action_cannot_run_twice_on_one_clip_at_once() {
        let mut jobs = AnalysisJobs::default();
        let clip = ClipInstanceId(1);
        let (release, gate) = mpsc::channel::<()>();

        assert!(jobs.spawn(clip, AnalysisKind::Beats, placement(), move || {
            let _ = gate.recv();
            Ok(Outcome::Beats(Vec::new()))
        }));
        assert!(
            !jobs.spawn(clip, AnalysisKind::Beats, placement(), || Ok(Outcome::Beats(Vec::new()))),
            "a second click while it runs must not start a duplicate"
        );
        assert!(
            jobs.spawn(clip, AnalysisKind::Silence, placement(), || Ok(Outcome::Silence(Vec::new()))),
            "a different action on the same clip is independent"
        );

        release.send(()).unwrap();
        assert_eq!(wait_for(&mut jobs, 2).len(), 2);
        assert!(!jobs.is_running(clip, AnalysisKind::Beats));
    }
}
