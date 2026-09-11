//! Media analysis that turns a clip into edits: scene-cut detection,
//! silence removal, beat markers, loudness matching and stabilization.
//!
//! Each action is split in two. The *measure* half is a free function over
//! plain data — the clip, the asset paths, the sample rate — so it can run on
//! a worker thread (see `analysis_jobs`); it's the slow part, a whole-clip
//! decode. The *apply* half is an `EditorState` method that turns the
//! measurement into `EditOp`s through the normal undo path rather than
//! mutating the project directly.
//!
//! The editor's buttons go through `EditorState::start_analysis`. The
//! `detect_*`/`match_clip_loudness`/`stabilize_clip` methods run both halves
//! inline and block until done, which is only acceptable in a test — they
//! exist for the real-footage tests and are compiled only there.

use super::analysis_jobs::AnalysisError;
use super::*;
use std::collections::HashMap;

pub(super) type AssetPaths = HashMap<media::MediaAssetId, PathBuf>;

/// `(timeline_tick, position offset)` pairs — see `stabilization_keyframes`.
pub(super) type StabilizationKeyframes = Vec<(i64, (f64, f64))>;

/// The file behind `clip`, or `NoMedia` for a title or an asset whose file
/// wasn't found when the project was opened.
fn media_path<'a>(asset_paths: &'a AssetPaths, clip: &ClipInstance) -> Result<&'a PathBuf, AnalysisError> {
    match clip.source {
        ClipSource::Media(asset_id) => asset_paths.get(&asset_id).ok_or(AnalysisError::NoMedia),
        _ => Err(AnalysisError::NoMedia),
    }
}

/// Samples decoded frames across `clip`'s source range and returns the source
/// ticks where `render::scene_cut` finds a hard cut. Empty when there's too
/// little footage to compare.
///
/// Sampling, not full-frame-rate decoding: `media_ffmpeg::decode_frame_at`
/// reopens and seeks the file per call (the same trade `Preview::render`
/// makes for scrubbing — see its doc), so decoding every source frame of a
/// long clip would take far longer than it's worth. Eight samples per second
/// of source is enough to find any cut a human would call a hard cut, capped
/// at 600 samples so an hour-long clip stays a bounded job.
pub(super) fn scene_cut_ticks(asset_paths: &AssetPaths, clip: &ClipInstance) -> Result<Vec<i64>, AnalysisError> {
    let path = media_path(asset_paths, clip)?;

    let source_span = clip.source_out.0 - clip.source_in.0;
    if source_span <= 0 {
        return Ok(Vec::new());
    }
    const SAMPLES_PER_SECOND: i64 = 8;
    let sample_count = ((source_span / (TIMEBASE / SAMPLES_PER_SECOND)).clamp(2, 600)) as usize;

    let mut histograms = Vec::with_capacity(sample_count);
    let mut sample_ticks = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let t = clip.source_in.0 + (source_span * i as i64) / (sample_count as i64 - 1).max(1);
        let Ok(frame) = media_ffmpeg::decode_frame_at(path, t) else { continue };
        histograms.push(render::scopes::Histogram::from_rgba(&frame.rgba));
        sample_ticks.push(t);
    }
    if histograms.len() < 2 {
        return Ok(Vec::new());
    }

    let cut_indices =
        render::scene_cut::detect_cuts(&histograms, &render::scene_cut::CutDetectorConfig::default());
    Ok(cut_indices.iter().map(|&i| sample_ticks[i]).collect())
}

/// Decodes `clip`'s whole source range to **interleaved stereo** `f32`
/// samples, via the same real decoder-backed `audio_source::
/// DecodedSampleSource` `audio::timeline_mix` itself uses — so "what gets
/// analysed" is always exactly "what would actually play".
///
/// Interleaved, not mixed to mono, because loudness measurement is
/// channel-aware (EBU R128 weighs a stereo pair differently from a mono
/// signal — see `audio::loudness`) and collapsing channels here would
/// silently answer a different, wrong question. `decode_mono` is a thin
/// wrapper around this for the callers that *do* want mono (silence, onset
/// and speech detection, where channel summation isn't part of the
/// measurement being made).
pub(super) fn decode_interleaved(
    asset_paths: &AssetPaths,
    sample_rate: u32,
    clip: &ClipInstance,
) -> Result<Vec<f32>, AnalysisError> {
    media_path(asset_paths, clip)?;
    let ClipSource::Media(asset_id) = clip.source else { return Err(AnalysisError::NoMedia) };
    let ticks_to_frames =
        |ticks: i64| -> i64 { (ticks as i128 * sample_rate as i128 / TIMEBASE as i128) as i64 };
    let source_frames = ticks_to_frames(clip.source_out.0 - clip.source_in.0);
    if source_frames <= 0 {
        return Err(AnalysisError::NoAudio);
    }

    let mut source = audio_source::DecodedSampleSource::new(asset_paths.clone());
    let interleaved = {
        use audio::SampleSource;
        source.samples_at(asset_id, ticks_to_frames(clip.source_in.0), source_frames as usize, sample_rate)
    };
    if interleaved.is_empty() {
        return Err(AnalysisError::NoAudio);
    }
    Ok(interleaved)
}

/// `decode_interleaved`, mixed down to mono.
pub(super) fn decode_mono(asset_paths: &AssetPaths, sample_rate: u32, clip: &ClipInstance) -> Result<Vec<f32>, AnalysisError> {
    let interleaved = decode_interleaved(asset_paths, sample_rate, clip)?;
    Ok(interleaved
        .chunks_exact(audio::CHANNELS)
        .map(|frame| frame.iter().sum::<f32>() / audio::CHANNELS as f32)
        .collect())
}

/// Pauses worth cutting in `clip`'s audio, as `(start, end)` seconds relative
/// to `clip.source_in`: below -40 dBFS for at least 0.3s, keeping 0.1s of
/// padding either side so speech isn't clipped.
pub(super) fn silence_ranges(
    asset_paths: &AssetPaths,
    sample_rate: u32,
    clip: &ClipInstance,
) -> Result<Vec<(f64, f64)>, AnalysisError> {
    let mono = decode_mono(asset_paths, sample_rate, clip)?;
    let config = audio::silence::SilenceConfig {
        threshold_dbfs: -40.0,
        min_silence_seconds: 0.3,
        padding_seconds: 0.1,
    };
    let silent = audio::silence::detect_silence(&mono, sample_rate, &config);
    Ok(audio::silence::ranges_to_remove(&silent, &config))
}

/// Onsets in `clip`'s audio, in **absolute** source seconds — what
/// `beat_markers` expects.
pub(super) fn beat_onsets(asset_paths: &AssetPaths, sample_rate: u32, clip: &ClipInstance) -> Result<Vec<f64>, AnalysisError> {
    let mono = decode_mono(asset_paths, sample_rate, clip)?;
    let onset_seconds = audio::beat::detect_onsets(&mono, sample_rate, &audio::beat::OnsetConfig::default());
    // Onsets are relative to the decoded buffer's start, i.e. to source_in.
    let source_in_seconds = clip.source_in.0 as f64 / TIMEBASE as f64;
    Ok(onset_seconds.iter().map(|&t| source_in_seconds + t).collect())
}

/// The gain, in dB, that brings `clip`'s integrated loudness (real EBU R128,
/// via `audio::loudness` — the same measurement export reports) to
/// `target_lufs`.
pub(super) fn loudness_gain(
    asset_paths: &AssetPaths,
    sample_rate: u32,
    clip: &ClipInstance,
    target_lufs: f64,
) -> Result<f64, AnalysisError> {
    let interleaved = decode_interleaved(asset_paths, sample_rate, clip)?;
    let measurement = audio::loudness::LoudnessMeasurement::analyze(&interleaved, audio::CHANNELS, sample_rate);
    Ok(audio::loudness::gain_to_reach_target(measurement.integrated_lufs as f64, target_lufs))
}

/// Samples decoded frames across `clip`'s source range, estimates
/// frame-to-frame motion via `render::stabilize` (block matching on a
/// downsampled greyscale copy), smooths the resulting camera path, and
/// returns the correction as `(timeline_tick, position offset)` pairs.
/// Empty for a keyframed speed curve, or footage too short to smooth.
///
/// Translation-only: see `render::stabilize`'s module doc for what this does
/// and doesn't correct (no rotation/scale/perspective, no border crop or
/// fill).
pub(super) fn stabilization_keyframes(
    asset_paths: &AssetPaths,
    clip: &ClipInstance,
) -> Result<StabilizationKeyframes, AnalysisError> {
    let path = media_path(asset_paths, clip)?;
    let Some((numerator, denominator)) = constant_speed(&clip.speed) else { return Ok(Vec::new()) };

    let source_span = clip.source_out.0 - clip.source_in.0;
    if source_span <= 0 {
        return Ok(Vec::new());
    }
    const SAMPLES_PER_SECOND: i64 = 6;
    let sample_count = (source_span / (TIMEBASE / SAMPLES_PER_SECOND)).clamp(3, 300) as usize;
    const DOWNSAMPLE: usize = 8;

    let mut luma_frames: Vec<Vec<f32>> = Vec::with_capacity(sample_count);
    let mut sample_ticks: Vec<i64> = Vec::with_capacity(sample_count);
    let mut dims: Option<(usize, usize)> = None;
    for i in 0..sample_count {
        let t = clip.source_in.0 + (source_span * i as i64) / (sample_count as i64 - 1).max(1);
        let Ok(frame) = media_ffmpeg::decode_frame_at(path, t) else { continue };
        let (w, h) = (frame.width as usize, frame.height as usize);
        if dims.is_some() && dims != Some((w.div_ceil(DOWNSAMPLE), h.div_ceil(DOWNSAMPLE))) {
            continue; // a resolution change mid-source shouldn't happen, but never mix frame sizes if it does
        }
        luma_frames.push(downsample_luma(&frame.rgba, w, h, DOWNSAMPLE));
        sample_ticks.push(t);
        dims = Some((w.div_ceil(DOWNSAMPLE), h.div_ceil(DOWNSAMPLE)));
    }
    let Some((dw, dh)) = dims else { return Ok(Vec::new()) };
    if luma_frames.len() < 3 {
        return Ok(Vec::new());
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

    Ok(sample_ticks
        .iter()
        .zip(&offsets)
        .map(|(&source_tick, &offset)| {
            let timeline_tick = clip.timeline_in.0 + (source_tick - clip.source_in.0) * denominator / numerator;
            // `offset` is exactly the shift that makes `cumulative + offset ==
            // smoothed` (see `stabilization_offsets`'s doc and its own tests)
            // — since the decoded frame's content already sits at
            // `cumulative[i]`'s natural drift, applying `offset` as the
            // Transform position directly moves the *displayed* content from
            // `cumulative[i]` to `smoothed[i]`. No sign flip: adding is
            // exactly what's needed, not its inverse.
            (timeline_tick, offset)
        })
        .collect())
}

impl EditorState {
    /// Finds hard cuts in `clip_id` and splits the clip on the timeline at
    /// each one — all as a single undo step. Blocking; see the module doc.
    /// Returns how many cuts were found and applied (0 if none, or if the
    /// clip can't be analysed — no media source, no known file path, or a
    /// keyframed speed curve; see `scene_cut_ops`).
    #[cfg(test)]
    pub fn detect_scene_cuts(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((track_id, clip)) = self.find_clip(clip_id) else { return 0 };
        let ticks = scene_cut_ticks(&self.asset_paths, &clip).unwrap_or_default();
        self.apply_scene_cuts(track_id, &clip, &ticks)
    }

    /// The apply half of scene-cut detection. Returns how many cuts were made.
    pub(super) fn apply_scene_cuts(&mut self, track: TrackId, clip: &ClipInstance, source_cut_ticks: &[i64]) -> usize {
        let ops = self.scene_cut_ops(track, clip, source_cut_ticks);
        if ops.is_empty() {
            return 0;
        }
        // `apply_ops` is all-or-nothing: if any op in the batch fails, it
        // pushes nothing and leaves the project untouched. Reporting
        // `ops.len()` regardless of that return value would tell the caller
        // "found and split at N cuts" when the split never actually
        // happened — caught during development via a test-fixture id
        // collision that made `apply_ops` fail and this code claim success
        // anyway (see `apply_silence_removal`'s equivalent fix for the fuller
        // story — it's the same bug in the sibling feature).
        if self.apply_ops("detect scene cuts", &ops) { ops.len() } else { 0 }
    }


    /// The pure, testable half of `detect_scene_cuts`: turns already-known
    /// source ticks into `Razor` ops on `clip`'s track. Split out specifically
    /// so the tick-mapping arithmetic (timeline placement + source trim +
    /// speed inversion) can be tested without decoding real video — see
    /// `crates/app/src/state/tests.rs` and
    /// `crates/render/tests/scene_cut.rs` for where the two halves of this
    /// feature are actually verified.
    ///
    /// Refuses a `SpeedCurve::Keyframed` clip outright (empty result, not a
    /// wrong mapping): recovering a timeline tick from a source tick needs
    /// inverting the speed curve, which is a cheap closed-form division for
    /// constant speed and not cheap in general for a keyframed one — the same
    /// scope line clip-speed audio retiming draws.
    pub(super) fn scene_cut_ops(&mut self, track: TrackId, clip: &ClipInstance, source_cut_ticks: &[i64]) -> Vec<EditOp> {
        let Some((numerator, denominator)) = constant_speed(&clip.speed) else { return Vec::new() };
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


    /// Finds pauses in `clip_id`'s audio and ripple-deletes each one — all as
    /// a single undo step. Blocking; see the module doc. Returns how many gaps
    /// were removed.
    ///
    /// Scoped to `SpeedCurve::Constant` clips, same reasoning as
    /// `scene_cut_ops`.
    #[cfg(test)]
    pub fn detect_silence_and_ripple_delete(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((track_id, clip)) = self.find_clip(clip_id) else { return 0 };
        let sample_rate = self.sequence().settings.sample_rate;
        let Ok(removed) = silence_ranges(&self.asset_paths, sample_rate, &clip) else { return 0 };
        self.apply_silence_removal(track_id, &clip, &removed)
    }

    /// The apply half of silence removal. Returns how many gaps were removed.
    pub(super) fn apply_silence_removal(&mut self, track: TrackId, clip: &ClipInstance, removed_source_seconds: &[(f64, f64)]) -> usize {
        let ops = self.silence_removal_ops(track, clip, removed_source_seconds);
        if ops.is_empty() {
            return 0;
        }
        // Same reasoning as `apply_scene_cuts`: `apply_ops` is all-or-
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
        let Some((numerator, denominator)) = constant_speed(&clip.speed) else { return Vec::new() };
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


    /// Finds onsets in `clip_id`'s audio and adds a sequence marker at each
    /// one — one undo step. Blocking; see the module doc. Returns how many
    /// markers were added.
    ///
    /// Markers, not razor cuts: a detected beat is a *snap target*, not an
    /// instruction to restructure the timeline the way a scene cut or a
    /// silence gap is. `EditorState::snap_tick` already treats every marker
    /// as a snap candidate, so this makes beats magnetic for free.
    #[cfg(test)]
    pub fn detect_beats_and_add_markers(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        let sample_rate = self.sequence().settings.sample_rate;
        let Ok(onsets) = beat_onsets(&self.asset_paths, sample_rate, &clip) else { return 0 };
        self.apply_beat_markers(&clip, &onsets)
    }

    /// The apply half of beat detection, from absolute source seconds.
    /// Returns how many markers were added.
    pub(super) fn apply_beat_markers(&mut self, clip: &ClipInstance, onset_source_seconds: &[f64]) -> usize {
        let markers = self.beat_markers(clip, onset_source_seconds);
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
        let Some((numerator, denominator)) = constant_speed(&clip.speed) else { return Vec::new() };
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


    /// Measures `clip_id`'s integrated loudness and sets its gain to reach
    /// `target_lufs`. Blocking; see the module doc. Returns the gain applied,
    /// or `None` if the clip couldn't be measured or already has keyframed
    /// gain automation (see `apply_loudness_match_gain`).
    #[cfg(test)]
    pub fn match_clip_loudness(&mut self, clip_id: ClipInstanceId, target_lufs: f64) -> Option<f64> {
        let (_, clip) = self.find_clip(clip_id)?;
        let sample_rate = self.sequence().settings.sample_rate;
        let gain = loudness_gain(&self.asset_paths, sample_rate, &clip, target_lufs).ok()?;
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


    /// Estimates camera shake in `clip_id` and writes the correction as
    /// `Transform::POSITION` keyframes — one undo step. Blocking; see the
    /// module doc and `stabilization_keyframes`. Returns how many keyframes
    /// were written.
    #[cfg(test)]
    pub fn stabilize_clip(&mut self, clip_id: ClipInstanceId) -> usize {
        let Some((_, clip)) = self.find_clip(clip_id) else { return 0 };
        let Ok(keyframes) = stabilization_keyframes(&self.asset_paths, &clip) else { return 0 };
        self.apply_stabilization_keyframes(clip_id, &keyframes)
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
}
