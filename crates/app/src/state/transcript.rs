//! Speech: transcription, caption generation, and transcript-based editing.

use super::*;

impl EditorState {
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
    pub(super) fn caption_ops(&mut self, track: TrackId, clip: &ClipInstance, segments: &[speech::Segment]) -> Vec<EditOp> {
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
    pub(super) fn timeline_words_from_transcript(&mut self, clip: &ClipInstance, segments: &[speech::Segment]) -> Vec<TimelineWord> {
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
    pub(super) fn timeline_range_removal_ops(&mut self, track: TrackId, start_tick: i64, end_tick: i64) -> Vec<EditOp> {
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

}
