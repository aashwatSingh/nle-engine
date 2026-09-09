//! Media analysis that turns a clip into edits: scene-cut detection,
//! silence removal, beat markers, loudness matching and stabilization.
//!
//! Each of these decodes the clip, computes something, and emits `EditOp`s
//! through the normal undo path rather than mutating the project directly.

use super::*;

impl EditorState {
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
    pub(super) fn scene_cut_ops(&mut self, track: TrackId, clip: &ClipInstance, source_cut_ticks: &[i64]) -> Vec<EditOp> {
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
    pub(super) fn decode_clip_interleaved_audio(&self, clip: &ClipInstance) -> Option<(Vec<f32>, u32)> {
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
    pub(super) fn decode_clip_mono_audio(&self, clip: &ClipInstance) -> Option<(Vec<f32>, u32)> {
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
    pub(super) fn silence_removal_ops(
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
    pub(super) fn beat_markers(&mut self, clip: &ClipInstance, onset_source_seconds: &[f64]) -> Vec<timeline::Marker> {
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
    pub(super) fn apply_loudness_match_gain(&mut self, clip_id: ClipInstanceId, gain_db: f64) -> bool {
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
    pub(super) fn apply_stabilization_keyframes(&mut self, clip_id: ClipInstanceId, ticks_and_offsets: &[(i64, (f64, f64))]) -> usize {
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

}
