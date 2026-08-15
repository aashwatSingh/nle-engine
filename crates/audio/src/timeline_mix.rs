//! Renders a sequence's audio tracks to interleaved stereo samples — the
//! thing that turns "clips on audio tracks" into something you can hear or
//! mux into a file.
//!
//! Gain staging follows Premiere's model, in this order:
//!   1. **Clip gain** (`ClipInstance::audio_gain_db`) and **clip pan**
//!      (`audio_pan`) — per-clip, keyframeable, applied to that clip's
//!      samples alone.
//!   2. **Track state** — `Track::muted` / `Track::solo`. Solo is
//!      exclusive: if *any* track is soloed, tracks that aren't are silent.
//!      Mute wins over solo on the same track, matching every NLE.
//!   3. **Track fader and pan** (`Track::gain_db`, `Track::pan`), applied to
//!      the track's summed clips — a fader rides the track, not each clip.
//!      Each track is mixed into its own buffer so it can be metered here,
//!      post-fader, before it reaches the bus.
//!   4. **Summing** — all audible tracks added together. Sums are *not*
//!      normalised (adding two full-scale tracks can clip); that's the
//!      correct, expected behaviour, and it's what a limiter on the master
//!      is for.
//!   5. **Master gain** (`MixOptions::master_gain_db`), then the master meter.
//!
//! Deliberately **not** here, and why:
//! - **Submixes and insert effects** (EQ, compressor, limiter). These need a
//!   real routing graph — arbitrary track -> submix -> master topology — rather
//!   than the fixed track-to-master path here. That's a `MixerGraph` in
//!   `timeline::Project` and another schema step, so it's a deliberate
//!   follow-up rather than something bolted onto this function.
//! - **Fader automation.** `Track::gain_db` is a plain number, not a
//!   `ParamTrack`: a keyframed field with no automation lane in the UI to edit
//!   it would be a worse lie than a static fader.
//! - **Clip speed.** A clip with a constant non-1x `SpeedCurve` is resampled
//!   (see `resample_linear`). What remains unsupported is a *keyframed* speed
//!   curve, which needs integrating a rate curve rather than scaling by a
//!   constant, and a *reverse* speed, which is refused deliberately rather
//!   than half-implemented: the video pipeline only walks forward, so
//!   reversing the audio alone would desync it against picture that isn't
//!   reversed. Both are reported in `MixStats::clips_with_unsupported_speed`.
//!
//!   Retiming is **varispeed**: pitch rises and falls with speed, the way a
//!   tape machine does, which is also what Premiere does with "Maintain Audio
//!   Pitch" off. Pitch-preserving time-stretch needs a phase vocoder and is a
//!   separate feature, not a refinement of this one.
//!

use crate::{PeakRms, SampleSource};
use timeline::{ClipSource, Project, SequenceId, TimeTick, TrackId, TrackKind, TIMEBASE};

/// Stereo is the only output layout for now: it's what the delivery formats
/// this project targets use, and what `pan` is defined against.
pub const CHANNELS: usize = 2;

#[derive(Debug, Clone, Copy)]
pub struct MixOptions {
    pub sample_rate: u32,
    pub master_gain_db: f64,
}

impl Default for MixOptions {
    fn default() -> Self {
        MixOptions { sample_rate: 48_000, master_gain_db: 0.0 }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MixStats {
    /// Clips that contributed at least some samples.
    pub clips_mixed: u32,
    /// Clips skipped because their source produced no samples (missing or
    /// unreadable media). Surfaced so a silent export isn't mistaken for a
    /// successful one.
    pub clips_with_no_source: u32,
    /// Title clips, which are picture-only and contribute silence by nature.
    /// Tracked separately from `clips_with_no_source` so "this project has
    /// titles on an audio track" never reads as "media failed to load".
    pub title_clips_without_audio: u32,
    /// Clips mixed at a constant non-1x speed. Their audio *is* retimed to
    /// match the picture — this is a diagnostic, not a warning.
    pub clips_retimed: u32,
    /// Clips whose speed can't be honoured: a keyframed (time-remapped) curve,
    /// or a reverse speed. Their audio is mixed at 1x, so it will not line up
    /// with the retimed video — see the module note.
    pub clips_with_unsupported_speed: u32,
    /// Post-fader level of each audible audio track over this block, in track
    /// order. Muted and solo-excluded tracks are absent rather than reported at
    /// silence, so a UI can tell "this track is off" from "this track is quiet".
    pub track_meters: Vec<(TrackId, PeakRms)>,
    /// Level of the master bus after `master_gain_db`. This is what the output
    /// device actually receives, so it's the number that says whether the mix
    /// is clipping.
    pub master_meter: PeakRms,
}

/// Resamples interleaved `src` to exactly `want_frames` output frames, reading
/// the source at `ratio` source frames per output frame.
///
/// Linear interpolation between neighbouring source frames. That is the right
/// level of effort here and the reason is worth stating: this is *varispeed*,
/// where the whole point is that the signal is being played at a different
/// rate, and the artefacts that matter (aliasing when speeding up) are
/// addressed by the box average below rather than by a fancier interpolator.
/// A windowed-sinc resampler would be sharper but would not change what the
/// user hears in a way they could pick out, and a phase vocoder — the thing
/// that would genuinely sound different — solves a different problem
/// (pitch-preserving stretch), not this one.
///
/// `ratio > 1` means decimation, which aliases: source content above the new
/// Nyquist folds back as tones that were never in the recording. Averaging the
/// whole source span each output frame covers is a box low-pass, crude but
/// enormously better than point-sampling, and it costs one pass.
fn resample_linear(src: &[f32], want_frames: usize, ratio: f64) -> Vec<f32> {
    let src_frames = src.len() / CHANNELS;
    if src_frames == 0 || want_frames == 0 {
        return Vec::new();
    }
    // Exactly 1x with an integer read position is the common case by far, and
    // must come back untouched rather than round-tripped through the
    // interpolator — otherwise every ordinary clip would pick up a fractional
    // sample of smear it didn't have before.
    if (ratio - 1.0).abs() <= f64::EPSILON {
        let n = want_frames.min(src_frames) * CHANNELS;
        let mut out = src[..n].to_vec();
        out.resize(want_frames * CHANNELS, 0.0);
        return out;
    }

    // Number of source frames each output frame spans, for the anti-alias
    // average. Below 1 (slowing down) there is nothing to average away.
    let span = ratio.max(1.0);
    let taps = (span.round() as usize).max(1);

    let mut out = vec![0.0f32; want_frames * CHANNELS];
    for i in 0..want_frames {
        let base = i as f64 * ratio;
        for ch in 0..CHANNELS {
            let mut acc = 0.0f64;
            for t in 0..taps {
                let pos = base + t as f64;
                let lo = pos.floor() as usize;
                let frac = pos - lo as f64;
                let a = src.get(lo * CHANNELS + ch).copied().unwrap_or(0.0) as f64;
                let b = src.get((lo + 1) * CHANNELS + ch).copied().unwrap_or(a as f32) as f64;
                acc += a + (b - a) * frac;
            }
            out[i * CHANNELS + ch] = (acc / taps as f64) as f32;
        }
    }
    out
}

/// dB to a linear amplitude multiplier. `-inf`-ish values floor at silence:
/// below -100 dB the multiplier is small enough to be inaudible and exact
/// zero avoids denormals in the sum.
pub fn db_to_linear(db: f64) -> f32 {
    if db <= -100.0 {
        0.0
    } else {
        10f64.powf(db / 20.0) as f32
    }
}

/// Constant-power stereo pan for `pan` in -1.0 (hard left) ..= 1.0 (hard
/// right). Returns per-channel multipliers.
///
/// Constant *power* (sin/cos law) rather than linear: with a linear law a
/// centred signal is 6 dB quieter than the same signal panned hard to one
/// side, so panning a clip around audibly changes its loudness. The sin/cos
/// law keeps perceived level constant, which is what every DAW and NLE does
/// and what makes a pan automation sweep sound like movement instead of a
/// volume dip.
pub fn pan_gains(pan: f64) -> (f32, f32) {
    let p = pan.clamp(-1.0, 1.0);
    // Map -1..1 onto 0..PI/2 so left = (1,0), centre = (√½,√½), right = (0,1).
    let theta = (p + 1.0) * std::f64::consts::FRAC_PI_4;
    (theta.cos() as f32, theta.sin() as f32)
}

/// Constant-power pan **normalised so centre is unity gain**.
///
/// `pan_gains` puts centre at (√½, √½) — 3 dB down — which is the correct
/// constant-power law but means every pan stage costs 3 dB at centre. That's
/// tolerable for the clip stage, where it's long-established behaviour every
/// existing project was mixed against. It is *not* tolerable for the track
/// fader stage: using `pan_gains` there would make every project written before
/// faders existed play back 3 dB quieter than it did, purely because a
/// defaulted `pan: 0.0` field appeared. Backward compatibility for a
/// `serde(default)` field means the default has to be a genuine no-op.
///
/// Hard left/right therefore read as +3 dB on the surviving channel, which is
/// the standard "unity at centre" console convention.
fn track_pan_gains(pan: f64) -> (f32, f32) {
    let (l, r) = pan_gains(pan);
    // pan_gains(0.0) == (FRAC_1_SQRT_2, FRAC_1_SQRT_2), so dividing by it puts
    // centre at exactly (1.0, 1.0).
    let centre = std::f32::consts::FRAC_1_SQRT_2;
    (l / centre, r / centre)
}

fn ticks_to_samples(ticks: i64, sample_rate: u32) -> i64 {
    (ticks as i128 * sample_rate as i128 / TIMEBASE as i128) as i64
}

fn samples_to_ticks(samples: i64, sample_rate: u32) -> i64 {
    (samples as i128 * TIMEBASE as i128 / sample_rate as i128) as i64
}

/// Mixes `[start_tick, start_tick + duration_ticks)` of `sequence` into
/// interleaved stereo f32.
///
/// The returned buffer is always exactly `frames * CHANNELS` long (silence
/// where nothing plays), so callers can treat it as a fixed-size block and
/// don't have to special-case gaps.
pub fn mix_range(
    project: &Project,
    sequence_id: SequenceId,
    start_tick: i64,
    duration_ticks: i64,
    options: &MixOptions,
    source: &mut dyn SampleSource,
) -> (Vec<f32>, MixStats) {
    let mut stats = MixStats::default();
    let mut ancestors = vec![sequence_id];
    let mut out = mix_tracks(
        project,
        sequence_id,
        start_tick,
        duration_ticks,
        options.sample_rate,
        source,
        &mut ancestors,
        true,
        &mut stats,
    );

    // Master gain last. Mathematically identical to folding it into each clip
    // (which is what this used to do), but it belongs here: it's one gain stage
    // on the bus, and metering the bus post-gain is what tells the user whether
    // what leaves the mixer is clipping.
    let master = db_to_linear(options.master_gain_db);
    if master != 1.0 {
        for s in out.iter_mut() {
            *s *= master;
        }
    }
    stats.master_meter = PeakRms::measure(&out);

    (out, stats)
}

/// Guard against a sequence that (directly or transitively) nests itself.
/// Mirrors `render::graph`'s `MAX_NEST_DEPTH` — kept as its own constant here
/// rather than shared, since that one is private to the `render` crate and
/// `audio` must not depend on it (see this module's dependency direction).
const MAX_NEST_DEPTH: usize = 16;

/// Sums `sequence_id`'s audio tracks into one interleaved stereo buffer,
/// recursing into `ClipSource::NestedSequence` clips.
///
/// This is the shared core behind both the top-level `mix_range` call and
/// every nested-sequence clip's own contribution — a nested clip is mixed by
/// calling this again for its inner sequence and treating the result exactly
/// like a decoded media clip's samples. No master gain and no metering happen
/// in here; those apply exactly once, at the outermost `mix_range` call, via
/// `top_level`. Without that flag a nested sequence's own tracks would push
/// entries into `stats.track_meters` under `TrackId`s the top-level mixer
/// panel has no idea belong to a different sequence.
#[allow(clippy::too_many_arguments)]
fn mix_tracks(
    project: &Project,
    sequence_id: SequenceId,
    start_tick: i64,
    duration_ticks: i64,
    sample_rate: u32,
    source: &mut dyn SampleSource,
    ancestors: &mut Vec<SequenceId>,
    top_level: bool,
    stats: &mut MixStats,
) -> Vec<f32> {
    let frames = ticks_to_samples(duration_ticks, sample_rate).max(0) as usize;
    let mut out = vec![0.0f32; frames * CHANNELS];
    if frames == 0 {
        return out;
    }
    let Some(sequence) = project.sequences.iter().find(|s| s.id == sequence_id) else {
        return out;
    };

    let any_solo = sequence.tracks.iter().any(|t| t.kind == TrackKind::Audio && t.solo);
    let range_end = start_tick + duration_ticks;

    // Each track is mixed into its own buffer, metered post-fader, then summed
    // into the master bus — the topology a console has, and the only way a
    // per-track meter can mean anything. Summing straight into `out` (the
    // previous shape) makes each track's own level unrecoverable.
    let mut track_buf = vec![0.0f32; frames * CHANNELS];

    for track in sequence.tracks.iter().filter(|t| t.kind == TrackKind::Audio) {
        if track.muted || (any_solo && !track.solo) {
            continue;
        }
        track_buf.fill(0.0);
        for clip in &track.clips {
            // Clip must overlap the requested window at all.
            if clip.timeline_out.0 <= start_tick || clip.timeline_in.0 >= range_end {
                continue;
            }
            // How fast this clip reads its source, as source frames per
            // output frame. `None` means the speed can't be honoured — a
            // keyframed curve or a reverse — in which case it is mixed at 1x
            // and reported, exactly as before.
            let speed_ratio = match clip.speed {
                timeline::SpeedCurve::Constant { numerator, denominator }
                    if denominator != 0 && numerator > 0 =>
                {
                    Some(numerator as f64 / denominator as f64)
                }
                _ => None,
            };
            match speed_ratio {
                Some(r) if (r - 1.0).abs() > f64::EPSILON => stats.clips_retimed += 1,
                Some(_) => {}
                None => stats.clips_with_unsupported_speed += 1,
            }
            let ratio = speed_ratio.unwrap_or(1.0);

            // The overlap, in output frames relative to the block start.
            let overlap_start_tick = clip.timeline_in.0.max(start_tick);
            let overlap_end_tick = clip.timeline_out.0.min(range_end);
            let dst_first = ticks_to_samples(overlap_start_tick - start_tick, sample_rate);
            let dst_last = ticks_to_samples(overlap_end_tick - start_tick, sample_rate);
            let want_frames = (dst_last - dst_first).max(0) as usize;
            if want_frames == 0 {
                continue;
            }

            // Where in the source those frames come from. Source position is
            // derived from the clip's own in-point plus how far into the clip
            // we are, so a trimmed or moved clip reads the right audio. Shared
            // by both branches below: a nested sequence is addressed by inner
            // *tick*, a media asset by sample offset, but both start from the
            // same "how far into this clip are we" arithmetic.
            let into_clip_ticks = overlap_start_tick - clip.timeline_in.0;

            // Counting is per-branch rather than "increment after the match"
            // because a nested sequence's recursive call already tallies its
            // own leaf clips into `stats`. Bumping `clips_mixed` again for the
            // wrapper clip on top of that would count one contribution twice —
            // a project with one nesting clip around one real audio clip would
            // report 2 clips mixed for what is, from the timeline the user is
            // looking at, 1 clip. `clips_mixed` isn't consumed by any caller
            // today; it exists purely as a diagnostic, so it should say
            // something a human reading it would expect.
            let samples: Vec<f32> = match &clip.source {
                // A title is picture only — there is genuinely no audio to
                // mix, so contributing silence is correct rather than a drop.
                // Counted anyway, under its own name: `clips_with_no_source`
                // would wrongly imply unreadable media, and reporting nothing
                // at all is how the nested-sequence bug hid for as long as it
                // did.
                ClipSource::Title(_) => {
                    stats.title_clips_without_audio += 1;
                    Vec::new()
                }
                &ClipSource::Media(asset) => {
                    // Start position uses `source_delta` so it matches how the
                    // video graph advances into the same clip — speed scales
                    // how fast the read head moves, never where it starts, or
                    // trimming a retimed clip would shift its audio.
                    let src_first = ticks_to_samples(
                        clip.source_in.0 + clip.speed.source_delta(into_clip_ticks),
                        sample_rate,
                    );
                    // At ratio r, `want_frames` of output spans r*want_frames
                    // of source, plus interpolation headroom so the last
                    // output frame still has a right-hand neighbour to read.
                    // At exactly 1x there is no interpolation, so the request
                    // stays exactly `want_frames` — over-reading there would
                    // make every ordinary clip pull two frames it never uses.
                    let src_frames = if (ratio - 1.0).abs() <= f64::EPSILON {
                        want_frames
                    } else {
                        ((want_frames as f64 * ratio).ceil() as usize) + 2
                    };
                    let s = source.samples_at(asset, src_first, src_frames, sample_rate);
                    if s.is_empty() {
                        stats.clips_with_no_source += 1;
                        s
                    } else {
                        stats.clips_mixed += 1;
                        resample_linear(&s, want_frames, ratio)
                    }
                }
                &ClipSource::NestedSequence(inner_id) => {
                    if ancestors.contains(&inner_id) || ancestors.len() >= MAX_NEST_DEPTH {
                        // A sequence nesting itself: skip rather than recurse
                        // forever, mirroring the video graph's guard. Not
                        // counted as missing source — a cycle isn't a decode
                        // failure, it's a project the model doesn't prevent
                        // constructing.
                        Vec::new()
                    } else {
                        let inner_start_tick =
                            clip.source_in.0 + clip.speed.source_delta(into_clip_ticks);
                        let inner_duration_ticks = overlap_end_tick - overlap_start_tick;
                        ancestors.push(inner_id);
                        // `stats` is threaded straight through: the recursive
                        // call attributes its own clips_mixed /
                        // clips_with_no_source / clips_with_unsupported_speed
                        // directly, so this branch adds nothing further.
                        let nested = mix_tracks(
                            project,
                            inner_id,
                            inner_start_tick,
                            inner_duration_ticks,
                            sample_rate,
                            source,
                            ancestors,
                            false,
                            stats,
                        );
                        ancestors.pop();
                        nested
                    }
                }
            };
            if samples.is_empty() {
                continue;
            }

            // Clip gain/pan are keyframeable, evaluated once per block at the
            // block's midpoint rather than per sample: at the block sizes
            // callers use (a video frame, or an audio device buffer) that's
            // a step every few milliseconds, inaudible for level automation,
            // and it keeps the inner loop to a multiply-add. Per-sample
            // interpolation is what a click-free fade at very short block
            // sizes would need — noted, not needed yet.
            let eval_tick = timeline::TimeTick(
                into_clip_ticks + (overlap_end_tick - overlap_start_tick) / 2,
            );
            let gain_db = clip.audio_gain_db.evaluate_at(eval_tick).as_scalar().unwrap_or(0.0);
            let pan = clip.audio_pan.evaluate_at(eval_tick).as_scalar().unwrap_or(0.0);
            let clip_gain = db_to_linear(gain_db);
            let (l, r) = pan_gains(pan);
            let (gl, gr) = (clip_gain * l, clip_gain * r);

            let available = samples.len() / CHANNELS;
            for f in 0..want_frames.min(available) {
                let dst = (dst_first as usize + f) * CHANNELS;
                if dst + 1 >= track_buf.len() {
                    break;
                }
                track_buf[dst] += samples[f * CHANNELS] * gl;
                track_buf[dst + 1] += samples[f * CHANNELS + 1] * gr;
            }
        }

        // Track fader and pan, applied after every clip on the track has been
        // summed — the whole point of a fader is that it rides the track, not
        // each clip. Clip pan and track pan compose multiplicatively, which is
        // what two successive constant-power pan stages give.
        let (tl, tr) = track_pan_gains(track.pan);
        if track.gain_db.is_animated() {
            // Automated: evaluated per output frame, at the frame's own
            // position on the sequence timeline. Per *frame* rather than once
            // per block because a block-rate fader steps at every block
            // boundary, which is audible as zipper noise; and in sequence time
            // rather than block time because a block mixed from the middle of
            // the timeline must read the middle of the curve, not restart it.
            for f in 0..frames {
                let tick = TimeTick(start_tick + frames_to_ticks(f as u64, sample_rate));
                let db = track.gain_db.evaluate_at(tick).as_scalar().unwrap_or(0.0);
                let g = db_to_linear(db);
                track_buf[f * CHANNELS] *= g * tl;
                track_buf[f * CHANNELS + 1] *= g * tr;
            }
        } else {
            // The overwhelmingly common case: one constant value, hoisted out
            // of the loop so an un-automated fader costs exactly what the
            // plain `f64` did.
            let track_gain =
                db_to_linear(track.gain_db.evaluate_at(TimeTick(0)).as_scalar().unwrap_or(0.0));
            if track_gain != 1.0 || track.pan != 0.0 {
                for f in 0..frames {
                    track_buf[f * CHANNELS] *= track_gain * tl;
                    track_buf[f * CHANNELS + 1] *= track_gain * tr;
                }
            }
        }
        if top_level {
            stats.track_meters.push((track.id, PeakRms::measure(&track_buf)));
        }

        for (o, t) in out.iter_mut().zip(track_buf.iter()) {
            *o += *t;
        }
    }

    out
}

/// Total audio frames a sequence produces at `sample_rate`.
pub fn total_frames(project: &Project, sequence_id: SequenceId, sample_rate: u32) -> u64 {
    let Some(sequence) = project.sequences.iter().find(|s| s.id == sequence_id) else {
        return 0;
    };
    ticks_to_samples(sequence.duration().0, sample_rate).max(0) as u64
}

/// Ticks spanned by `frames` audio frames — the inverse of the internal
/// tick->frame conversion, exposed so callers stepping the timeline in audio
/// blocks stay on exactly the same grid the mixer uses.
pub fn frames_to_ticks(frames: u64, sample_rate: u32) -> i64 {
    samples_to_ticks(frames as i64, sample_rate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use timeline::{
        ClipInstance, ClipInstanceId, FrameRate, InterpolationMode, ParamValue, Sequence, SequenceSettings,
        SpeedCurve, TimeTick, Track, TrackId,
    };

    const SEQ: SequenceId = SequenceId(1);
    const RATE: u32 = 48_000;

    /// A source that returns a constant value per asset, so mixing maths is
    /// checkable by reading one output sample. No media, no ffmpeg.
    struct ConstSource {
        values: HashMap<u128, f32>,
        /// Records what was asked for, to assert on source-offset mapping.
        requests: Vec<(u128, i64, usize)>,
    }

    impl ConstSource {
        fn new(pairs: &[(u128, f32)]) -> Self {
            ConstSource {
                values: pairs.iter().copied().collect(),
                requests: Vec::new(),
            }
        }
    }

    impl SampleSource for ConstSource {
        fn samples_at(
            &mut self,
            asset: media::MediaAssetId,
            start_frame: i64,
            frames: usize,
            _sample_rate: u32,
        ) -> Vec<f32> {
            self.requests.push((asset.0, start_frame, frames));
            match self.values.get(&asset.0) {
                Some(v) => vec![*v; frames * CHANNELS],
                None => Vec::new(),
            }
        }
    }

    fn audio_clip(id: u64, asset: u128, tl_in: i64, tl_out: i64, src_in: i64) -> ClipInstance {
        ClipInstance {
            id: ClipInstanceId(id),
            source: ClipSource::Media(media::MediaAssetId(asset)),
            source_in: TimeTick(src_in),
            source_out: TimeTick(src_in + (tl_out - tl_in)),
            timeline_in: TimeTick(tl_in),
            timeline_out: TimeTick(tl_out),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        }
    }

    fn project_with(tracks: Vec<Track>) -> Project {
        Project {
            sequences: vec![Sequence {
                id: SEQ,
                name: "S".into(),
                settings: SequenceSettings {
                    frame_rate: FrameRate::Fps30,
                    width: 640,
                    height: 360,
                    sample_rate: RATE,
                    working_color_primaries: media::ColorPrimaries::Rec709,
                    drop_frame_timecode: false,
                },
                tracks,
                markers: vec![],
            }],
            assets: vec![],
            bins: vec![],
        }
    }

    fn audio_track(id: u64, clips: Vec<ClipInstance>) -> Track {
        Track {
            id: TrackId(id),
            kind: TrackKind::Audio,
            name: format!("A{id}"),
            clips,
            transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
            locked: false,
            sync_locked: true,
            muted: false,
            solo: false,
            height_px: 60,
        }
    }

    #[test]
    fn db_and_pan_conversions_match_the_standard_laws() {
        assert!((db_to_linear(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_linear(-6.0206) - 0.5).abs() < 1e-4, "-6dB is half amplitude");
        assert_eq!(db_to_linear(-120.0), 0.0, "very low dB floors to exact silence");

        let (l, r) = pan_gains(0.0);
        assert!((l - r).abs() < 1e-6, "centre is equal in both channels");
        // Constant power: the two gains square-sum to 1 at every position,
        // which is exactly the property that keeps loudness steady while panning.
        for p in [-1.0, -0.5, 0.0, 0.37, 1.0] {
            let (l, r) = pan_gains(p);
            assert!((l * l + r * r - 1.0).abs() < 1e-5, "constant power at pan {p}");
        }
        let (l, r) = pan_gains(-1.0);
        assert!(l > 0.99 && r < 0.01, "hard left");
        let (l, r) = pan_gains(1.0);
        assert!(r > 0.99 && l < 0.01, "hard right");
    }

    #[test]
    fn a_single_clip_lands_at_its_timeline_position_with_silence_around_it() {
        // Clip occupies the middle second of a 3s window.
        let clip = audio_clip(1, 1, TIMEBASE, TIMEBASE * 2, 0);
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 0.5)]);

        let (out, stats) =
            mix_range(&project, SEQ, 0, TIMEBASE * 3, &MixOptions::default(), &mut src);

        assert_eq!(out.len(), RATE as usize * 3 * CHANNELS);
        assert_eq!(stats.clips_mixed, 1);
        let frame = |f: usize| out[f * CHANNELS];
        assert_eq!(frame(0), 0.0, "silence before the clip");
        assert_eq!(frame(RATE as usize * 3 - 1), 0.0, "silence after the clip");
        // Inside the clip: 0.5 through unity gain, centre pan (~0.707).
        let mid = frame(RATE as usize + 100);
        assert!((mid - 0.5 * std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5, "got {mid}");
    }

    #[test]
    fn a_trimmed_clip_reads_from_its_source_in_point() {
        // Regression guard for the mapping that makes trimming work at all:
        // a clip starting 2s into its source, placed at 1s on the timeline,
        // must request source samples from 2s — not 0s, and not 3s.
        let clip = audio_clip(1, 1, TIMEBASE, TIMEBASE * 2, TIMEBASE * 2);
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        mix_range(&project, SEQ, 0, TIMEBASE * 3, &MixOptions::default(), &mut src);

        assert_eq!(src.requests.len(), 1);
        let (asset, start_frame, frames) = src.requests[0];
        assert_eq!(asset, 1);
        assert_eq!(start_frame, RATE as i64 * 2, "should read from 2s into the source");
        assert_eq!(frames, RATE as usize, "one second of frames");
    }

    #[test]
    fn overlapping_clips_sum_rather_than_replace() {
        let project = project_with(vec![
            audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]),
            audio_track(2, vec![audio_clip(2, 2, 0, TIMEBASE, 0)]),
        ]);
        let mut src = ConstSource::new(&[(1, 0.25), (2, 0.25)]);

        let (out, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_mixed, 2);
        let expected = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[200] - expected).abs() < 1e-5, "two 0.25 clips should sum to 0.5, got {}", out[200]);
    }

    #[test]
    fn mute_silences_a_track_and_solo_silences_every_other_track() {
        let mut tracks = vec![
            audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]),
            audio_track(2, vec![audio_clip(2, 2, 0, TIMEBASE, 0)]),
        ];
        // Muted track contributes nothing.
        tracks[0].muted = true;
        let project = project_with(tracks.clone());
        let mut src = ConstSource::new(&[(1, 1.0), (2, 0.25)]);
        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);
        assert_eq!(stats.clips_mixed, 1, "only the unmuted track should mix");

        // Solo on track 2 excludes track 1 even though track 1 isn't muted.
        let mut tracks2 = vec![
            audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]),
            audio_track(2, vec![audio_clip(2, 2, 0, TIMEBASE, 0)]),
        ];
        tracks2[1].solo = true;
        let project = project_with(tracks2);
        let mut src = ConstSource::new(&[(1, 1.0), (2, 0.25)]);
        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);
        assert_eq!(stats.clips_mixed, 1);
        assert_eq!(src.requests[0].0, 2, "the soloed track is the one that played");

        // Mute wins over solo on the same track.
        let mut tracks3 = vec![audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)])];
        tracks3[0].solo = true;
        tracks3[0].muted = true;
        let project = project_with(tracks3);
        let mut src = ConstSource::new(&[(1, 1.0)]);
        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);
        assert_eq!(stats.clips_mixed, 0, "mute should win over solo");
    }

    #[test]
    fn clip_gain_pan_and_master_gain_all_apply() {
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        // -6dB clip gain, panned hard right.
        clip.audio_gain_db = timeline::ParamTrack::constant(ParamValue::Number(-6.0206));
        clip.audio_pan = timeline::ParamTrack::constant(ParamValue::Number(1.0));
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);
        let options = MixOptions { sample_rate: RATE, master_gain_db: -6.0206 };

        let (out, _) = mix_range(&project, SEQ, 0, TIMEBASE, &options, &mut src);

        // 1.0 * 0.5 (clip) * 0.5 (master) = 0.25, all in the right channel.
        assert!(out[0].abs() < 1e-4, "hard right should leave the left channel silent, got {}", out[0]);
        assert!((out[1] - 0.25).abs() < 1e-3, "right channel should be 0.25, got {}", out[1]);
    }

    #[test]
    fn keyframed_clip_gain_changes_over_time() {
        // A fade-in: -inf..0dB across the clip. Early blocks must be quieter
        // than late ones — proves gain is evaluated per block against the
        // clip's own timeline, not read once as a constant.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.audio_gain_db = timeline::ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![
                timeline::Keyframe {
                    at: TimeTick(0),
                    value: ParamValue::Number(-60.0),
                    interpolation: timeline::InterpolationMode::Linear,
                    tangents: None,
                },
                timeline::Keyframe {
                    at: TimeTick(TIMEBASE),
                    value: ParamValue::Number(0.0),
                    interpolation: timeline::InterpolationMode::Linear,
                    tangents: None,
                },
            ],
        };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);
        let opts = MixOptions::default();

        // Mix the first and last tenth of a second separately.
        let tenth = TIMEBASE / 10;
        let (early, _) = mix_range(&project, SEQ, 0, tenth, &opts, &mut src);
        let (late, _) = mix_range(&project, SEQ, TIMEBASE - tenth, tenth, &opts, &mut src);

        assert!(
            early[0].abs() < late[0].abs() * 0.5,
            "fade-in should make the start much quieter than the end (early {}, late {})",
            early[0],
            late[0]
        );
    }

    #[test]
    fn a_clip_with_no_source_is_counted_not_silently_dropped() {
        let project = project_with(vec![audio_track(1, vec![audio_clip(1, 99, 0, TIMEBASE, 0)])]);
        let mut src = ConstSource::new(&[]); // asset 99 unknown

        let (out, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_with_no_source, 1);
        assert_eq!(stats.clips_mixed, 0);
        assert!(out.iter().all(|s| *s == 0.0), "output should be silence, not garbage");
    }

    #[test]
    fn video_tracks_are_ignored() {
        let mut track = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
        track.kind = TrackKind::Video;
        let project = project_with(vec![track]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_mixed, 0, "a video track must not contribute to the audio mix");
    }

    /// A source whose sample value *is* its frame index, so a caller can read
    /// back exactly which source frames a retimed clip consumed. A constant
    /// source can prove a clip asked for the right *number* of frames but not
    /// that it read the right *ones*, which is the whole question for retiming.
    /// Every clip goes through centre pan, whose constant-power law puts a
    /// factor of 1/sqrt(2) on both channels (see `pan_gains`). The ramp tests
    /// read mixer *output*, so they divide it back out to recover the source
    /// frame index they're actually asserting about.
    const CENTRE_PAN: f32 = std::f32::consts::FRAC_1_SQRT_2;

    struct RampSource;

    impl SampleSource for RampSource {
        fn samples_at(
            &mut self,
            _asset: media::MediaAssetId,
            start_frame: i64,
            frames: usize,
            _sample_rate: u32,
        ) -> Vec<f32> {
            (0..frames)
                .flat_map(|i| {
                    let v = (start_frame + i as i64) as f32;
                    [v, v]
                })
                .collect()
        }
    }

    #[test]
    fn a_double_speed_clip_advances_through_its_source_twice_as_fast() {
        // The defining property of 2x: consecutive output frames come from
        // source frames two apart. Reading the ramp back says exactly which
        // source frames were used, so this can't pass by reading the right
        // count of the wrong samples.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = RampSource;

        let (samples, _) =
            mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        // Sample well inside the block so start/end edge handling isn't what's
        // under test. Left channel only (stride CHANNELS).
        let at = |frame: usize| samples[frame * CHANNELS] / CENTRE_PAN;
        let a = at(1000);
        let b = at(2000);
        assert!(
            ((b - a) - 2000.0).abs() < 4.0,
            "1000 output frames at 2x should advance 2000 source frames, got {}",
            b - a
        );
    }

    #[test]
    fn a_half_speed_clip_advances_through_its_source_half_as_fast() {
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.speed = SpeedCurve::Constant { numerator: 1, denominator: 2 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = RampSource;

        let (samples, _) =
            mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        let at = |frame: usize| samples[frame * CHANNELS] / CENTRE_PAN;
        let advance = at(2000) - at(1000);
        assert!(
            (advance - 500.0).abs() < 4.0,
            "1000 output frames at 0.5x should advance 500 source frames, got {advance}"
        );
    }

    #[test]
    fn a_retimed_clip_starts_from_its_own_source_in_point() {
        // Speed scales how fast the read position advances; it must not also
        // scale where that position starts, or trimming a retimed clip would
        // move its audio.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, TIMEBASE);
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = RampSource;

        let (samples, _) =
            mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        // source_in is one second in, so the first output frame must come from
        // source frame ~RATE regardless of the speed.
        let first = samples[0] / CENTRE_PAN;
        assert!(
            (first - RATE as f32).abs() < 4.0,
            "expected to start at source frame {}, got {first}",
            RATE
        );
    }

    #[test]
    fn a_static_track_fader_is_unchanged_by_the_automation_path() {
        // Regression guard: every track now goes through the automation-aware
        // fader, so an un-keyframed one must behave exactly as the plain
        // number did. `a_default_fader_and_pan_are_an_exact_no_op` already
        // pins unity; this pins a non-unity static value.
        let project = project_with(vec![{
            let mut t = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
            t.gain_db = timeline::ParamTrack::constant(ParamValue::Number(-6.02));
            t
        }]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (samples, _) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);
        let expected = CENTRE_PAN * 0.5; // -6.02 dB is exactly half amplitude.
        assert!(
            (samples[0] - expected).abs() < 1e-3,
            "a static -6.02 dB fader should halve the signal, got {} vs {expected}",
            samples[0]
        );
    }

    #[test]
    fn an_automated_track_fader_ramps_across_the_block() {
        // The feature itself: two keyframes a second apart, silence to unity,
        // must produce a level that actually climbs *within* one mixed block.
        // Evaluating the fader once per block instead of per sample would give
        // a flat block here and step at block boundaries — audible as zipper
        // noise, which is the whole reason automation is evaluated per frame.
        let mut gain = timeline::ParamTrack::constant(ParamValue::Number(-60.0));
        gain.upsert_keyframe(TimeTick(0), ParamValue::Number(-60.0), InterpolationMode::Linear);
        gain.upsert_keyframe(
            TimeTick(TIMEBASE),
            ParamValue::Number(0.0),
            InterpolationMode::Linear,
        );
        let project = project_with(vec![{
            let mut t = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
            t.gain_db = gain;
            t
        }]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (samples, _) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        let at = |f: usize| samples[f * CHANNELS].abs();
        let early = at(100);
        let middle = at(RATE as usize / 2);
        let late = at(RATE as usize - 200);
        assert!(
            early < middle && middle < late,
            "the fader should ramp up across the block, got {early} -> {middle} -> {late}"
        );
        // And land near unity at the end, not merely somewhere higher.
        assert!(
            (late - CENTRE_PAN).abs() < 0.05,
            "should reach about unity by the last keyframe, got {late}"
        );
    }

    #[test]
    fn track_fader_automation_is_evaluated_in_sequence_time_not_block_time() {
        // A block mixed from the middle of the timeline must read the fader at
        // *that* point on the curve. Evaluating from the block's own start
        // would restart the automation on every block — the fader would sound
        // right only for a mix that happened to begin at tick 0.
        let mut gain = timeline::ParamTrack::constant(ParamValue::Number(-60.0));
        gain.upsert_keyframe(TimeTick(0), ParamValue::Number(-60.0), InterpolationMode::Linear);
        gain.upsert_keyframe(
            TimeTick(TIMEBASE * 2),
            ParamValue::Number(0.0),
            InterpolationMode::Linear,
        );
        let project = project_with(vec![{
            let mut t = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE * 2, 0)]);
            t.gain_db = gain;
            t
        }]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        // Mix only the second half. At its start the curve is halfway, so the
        // level must already be well above where it began.
        let (samples, _) = mix_range(
            &project,
            SEQ,
            TIMEBASE,
            TIMEBASE,
            &MixOptions::default(),
            &mut src,
        );
        let first = samples[0].abs();
        let unity_at_minus_30 = CENTRE_PAN * db_to_linear(-30.0);
        assert!(
            (first - unity_at_minus_30).abs() < 0.02,
            "halfway along a -60..0 dB ramp should be about -30 dB ({unity_at_minus_30}), got {first}"
        );
    }

    #[test]
    fn a_block_starting_partway_into_a_retimed_clip_reads_the_scaled_source_position() {
        // The case every other retiming test here misses: they all mix from
        // tick 0, where "how far into the clip" is zero and so scaling it by
        // the speed changes nothing. Playback and export both request blocks
        // from the middle of a clip constantly, and getting this wrong makes a
        // retimed clip jump to the wrong audio the moment you scrub into it
        // rather than play it from the top.
        //
        // Half a second into a 2x clip, the read head is a *full* second into
        // the source.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE * 4, 0);
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = RampSource;

        let (samples, _) = mix_range(
            &project,
            SEQ,
            TIMEBASE / 2,
            TIMEBASE / 2,
            &MixOptions::default(),
            &mut src,
        );

        let first = samples[0] / CENTRE_PAN;
        assert!(
            (first - RATE as f32).abs() < 4.0,
            "half a second into a 2x clip should read source frame {} (one second in), got {first}",
            RATE
        );
    }

    #[test]
    fn a_constant_non_unity_speed_is_retimed_rather_than_reported_unsupported() {
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.speed = SpeedCurve::Constant { numerator: 2, denominator: 1 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_retimed, 1, "a constant speed is supported now");
        assert_eq!(
            stats.clips_with_unsupported_speed, 0,
            "and must no longer be reported as desynced"
        );
        assert_eq!(stats.clips_mixed, 1);
    }

    #[test]
    fn a_keyframed_speed_curve_is_still_reported_unsupported() {
        // Time remapping needs integrating a rate curve, which `source_delta`
        // explicitly doesn't do. Still mixed at 1x, still reported — the
        // reason that stat exists.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.speed = SpeedCurve::Keyframed(timeline::ParamTrack::constant(ParamValue::Number(2.0)));
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_with_unsupported_speed, 1);
        assert_eq!(stats.clips_retimed, 0);
    }

    #[test]
    fn a_reverse_speed_is_reported_unsupported_rather_than_played_forwards() {
        // The video pipeline only walks forward, so reversing just the audio
        // would desync it against picture that isn't reversed. Refused
        // consistently with video rather than half-implemented.
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.speed = SpeedCurve::Constant { numerator: -1, denominator: 1 };
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = ConstSource::new(&[(1, 1.0)]);

        let (_, stats) = mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_with_unsupported_speed, 1);
        assert_eq!(stats.clips_retimed, 0);
    }

    #[test]
    fn a_unity_speed_clip_is_unchanged_by_the_retiming_path() {
        // Regression guard: every ordinary clip goes through the same code as
        // a retimed one now, so 1x must remain exactly what it was.
        let clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        let project = project_with(vec![audio_track(1, vec![clip])]);
        let mut src = RampSource;

        let (samples, stats) =
            mix_range(&project, SEQ, 0, TIMEBASE, &MixOptions::default(), &mut src);

        assert_eq!(stats.clips_retimed, 0, "1x is not a retime");
        for frame in [0usize, 1, 500, 4321] {
            let got = samples[frame * CHANNELS] / CENTRE_PAN;
            assert!(
                (got - frame as f32).abs() < 0.01,
                "at 1x output frame {frame} must be source frame {frame}, got {got}"
            );
        }
    }

    /// Mixes one 0.1s block from tick 0 and returns (samples, stats).
    /// Builds a project with two sequences: `inner` (a plain audio clip) and
    /// `outer`, which places `inner` as a `NestedSequence` clip on one track.
    fn project_with_nested_sequence(inner_clip: ClipInstance) -> (Project, SequenceId, SequenceId) {
        let inner_id = SequenceId(2);
        let outer_id = SequenceId(1);
        let settings = SequenceSettings {
            frame_rate: FrameRate::Fps30,
            width: 640,
            height: 360,
            sample_rate: RATE,
            working_color_primaries: media::ColorPrimaries::Rec709,
            drop_frame_timecode: false,
        };
        let inner_seq = Sequence {
            id: inner_id,
            name: "inner".into(),
            settings: settings.clone(),
            tracks: vec![audio_track(1, vec![inner_clip])],
            markers: vec![],
        };
        let nesting_clip = ClipInstance {
            id: ClipInstanceId(900),
            source: ClipSource::NestedSequence(inner_id),
            source_in: TimeTick(0),
            source_out: TimeTick(TIMEBASE),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(TIMEBASE),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        };
        let outer_seq = Sequence {
            id: outer_id,
            name: "outer".into(),
            settings,
            tracks: vec![audio_track(2, vec![nesting_clip])],
            markers: vec![],
        };
        (
            Project { sequences: vec![outer_seq, inner_seq], assets: vec![], bins: vec![] },
            outer_id,
            inner_id,
        )
    }

    #[test]
    fn a_nested_sequence_clip_contributes_its_inner_audio() {
        // Today this clip falls through `let ClipSource::Media(asset) = ... else
        // { continue }` and is dropped with no trace at all — despite a comment
        // claiming otherwise. Mixing the outer sequence should sound like
        // mixing the inner one directly.
        let (project, outer_id, _inner_id) = project_with_nested_sequence(audio_clip(1, 7, 0, TIMEBASE, 0));
        let mut src = ConstSource::new(&[(7, 0.5)]);
        let (out, stats) = mix_range(
            &project,
            outer_id,
            0,
            TIMEBASE / 10,
            &MixOptions { sample_rate: RATE, master_gain_db: 0.0 },
            &mut src,
        );

        assert!(
            out.iter().any(|&s| s.abs() > 1e-6),
            "the nested sequence's audio should reach the outer mix, got silence"
        );
        assert_eq!(
            stats.clips_mixed, 1,
            "the nested clip should be counted as mixed, not silently skipped"
        );
    }

    #[test]
    fn a_self_nesting_sequence_does_not_recurse_forever() {
        // A sequence referencing itself (directly or through a cycle) must be
        // survivable, mirroring render::graph's cycle guard for video.
        let id = SequenceId(1);
        let settings = SequenceSettings {
            frame_rate: FrameRate::Fps30,
            width: 640,
            height: 360,
            sample_rate: RATE,
            working_color_primaries: media::ColorPrimaries::Rec709,
            drop_frame_timecode: false,
        };
        let self_clip = ClipInstance {
            id: ClipInstanceId(1),
            source: ClipSource::NestedSequence(id),
            source_in: TimeTick(0),
            source_out: TimeTick(TIMEBASE),
            timeline_in: TimeTick(0),
            timeline_out: TimeTick(TIMEBASE),
            speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
            effects: vec![],
            audio_gain_db: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            audio_pan: timeline::ParamTrack::constant(ParamValue::Number(0.0)),
            linked_group: None,
        };
        let project = Project {
            sequences: vec![Sequence {
                id,
                name: "S".into(),
                settings,
                tracks: vec![audio_track(1, vec![self_clip])],
                markers: vec![],
            }],
            assets: vec![],
            bins: vec![],
        };
        let mut src = ConstSource::new(&[]);
        // Must return, not hang or overflow the stack.
        let (out, _stats) = mix_range(
            &project,
            id,
            0,
            TIMEBASE / 10,
            &MixOptions { sample_rate: RATE, master_gain_db: 0.0 },
            &mut src,
        );
        assert!(out.iter().all(|&s| s == 0.0), "a self-nesting sequence should mix as silence");
    }

    #[test]
    fn nested_sequence_audio_respects_the_nesting_clips_own_gain() {
        // The nested clip is a clip like any other on the outer timeline — its
        // own gain/pan must still apply to whatever the inner sequence produces.
        let inner = audio_clip(1, 7, 0, TIMEBASE, 0);
        let (mut project, outer_id, _inner_id) = project_with_nested_sequence(inner);
        project.sequences[0].tracks[0].clips[0].audio_gain_db =
            timeline::ParamTrack::constant(ParamValue::Number(-6.0206)); // half amplitude

        let mut full = ConstSource::new(&[(7, 0.5)]);
        let (loud, _) = mix_range(
            &project,
            outer_id,
            0,
            TIMEBASE / 10,
            &MixOptions { sample_rate: RATE, master_gain_db: 0.0 },
            &mut full,
        );

        let mut unity_project = project.clone();
        unity_project.sequences[0].tracks[0].clips[0].audio_gain_db =
            timeline::ParamTrack::constant(ParamValue::Number(0.0));
        let mut unity_src = ConstSource::new(&[(7, 0.5)]);
        let (unity, _) = mix_range(
            &unity_project,
            outer_id,
            0,
            TIMEBASE / 10,
            &MixOptions { sample_rate: RATE, master_gain_db: 0.0 },
            &mut unity_src,
        );

        assert!(loud[0].abs() > 1e-6 && unity[0].abs() > 1e-6, "both should have signal");
        assert!(
            (loud[0] / unity[0] - 0.5).abs() < 1e-3,
            "the nesting clip's own -6dB gain should halve the inner sequence's output, got ratio {}",
            loud[0] / unity[0]
        );
    }

    fn mix_block(project: &Project, source: &mut ConstSource, master_db: f64) -> (Vec<f32>, MixStats) {
        mix_range(
            project,
            SEQ,
            0,
            TIMEBASE / 10,
            &MixOptions { sample_rate: RATE, master_gain_db: master_db },
            source,
        )
    }

    /// Peak of the left channel for a single-clip project, so tests can state
    /// ratios rather than baking in the clip stage's constant-power constant.
    fn left_level(track_gain_db: f64, track_pan: f64, clip_gain_db: f64, master_db: f64) -> f32 {
        let mut clip = audio_clip(1, 1, 0, TIMEBASE, 0);
        clip.audio_gain_db = timeline::ParamTrack::constant(ParamValue::Number(clip_gain_db));
        let mut track = audio_track(1, vec![clip]);
        track.gain_db = timeline::ParamTrack::constant(ParamValue::Number(track_gain_db));
        track.pan = track_pan;
        let p = project_with(vec![track]);
        let mut src = ConstSource::new(&[(1, 0.5)]);
        mix_block(&p, &mut src, master_db).0[0]
    }

    #[test]
    fn a_default_fader_and_pan_are_an_exact_no_op() {
        // The backward-compatibility guard, and the test that catches the real
        // trap in adding a pan stage: `pan_gains` puts centre 3 dB down, so
        // reusing it for the track stage makes every project saved before
        // faders existed play back 3 dB quieter for no reason the user can see.
        // 0.5 source through the clip's centre pan is the long-standing value.
        let expected = 0.5 * std::f32::consts::FRAC_1_SQRT_2;
        let actual = left_level(0.0, 0.0, 0.0, 0.0);
        assert!(
            (actual - expected).abs() < 1e-6,
            "a defaulted track fader/pan must change nothing: expected {expected}, got {actual}"
        );
    }

    #[test]
    fn a_track_fader_scales_the_whole_track() {
        let unity = left_level(0.0, 0.0, 0.0, 0.0);
        let cut = left_level(-6.020_6, 0.0, 0.0, 0.0);
        assert!((cut / unity - 0.5).abs() < 1e-3, "-6 dB should halve amplitude, got {}", cut / unity);
    }

    #[test]
    fn track_fader_and_clip_gain_compose() {
        // Both stages must apply. If one replaced the other this would be 0.5.
        let unity = left_level(0.0, 0.0, 0.0, 0.0);
        let both = left_level(-6.020_6, 0.0, -6.020_6, 0.0);
        assert!(
            (both / unity - 0.25).abs() < 1e-3,
            "clip -6 dB then track -6 dB should be a quarter, got {}",
            both / unity
        );
    }

    #[test]
    fn a_track_pan_hard_left_silences_the_right_channel_and_boosts_the_left() {
        let mut track = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
        track.pan = -1.0;
        let p = project_with(vec![track]);
        let mut src = ConstSource::new(&[(1, 0.5)]);
        let (out, _) = mix_block(&p, &mut src, 0.0);

        let centre = left_level(0.0, 0.0, 0.0, 0.0);
        assert!(out[1].abs() < 1e-6, "right should be silent, got {}", out[1]);
        assert!(
            (out[0] / centre - std::f32::consts::SQRT_2).abs() < 1e-3,
            "constant power means hard-left is +3 dB on the surviving channel, got ratio {}",
            out[0] / centre
        );
    }

    #[test]
    fn master_gain_applies_once_after_summing() {
        // Master moved from a per-clip multiply to a single bus stage during the
        // mixer rework. Distributive, so the result must not have changed — two
        // tracks make a double-application visible.
        let p = project_with(vec![
            audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]),
            audio_track(2, vec![audio_clip(2, 1, 0, TIMEBASE, 0)]),
        ]);
        let mut src = ConstSource::new(&[(1, 0.25)]);
        let (unity, _) = mix_block(&p, &mut src, 0.0);
        let mut src = ConstSource::new(&[(1, 0.25)]);
        let (halved, _) = mix_block(&p, &mut src, -6.020_6);
        assert!(
            (halved[0] / unity[0] - 0.5).abs() < 1e-3,
            "master -6 dB should halve the sum exactly once, got {}",
            halved[0] / unity[0]
        );
    }

    #[test]
    fn per_track_meters_report_each_tracks_own_post_fader_level() {
        // The measurement a mixer panel needs, and the reason tracks are mixed
        // into separate buffers: with everything summed into one bus first,
        // an individual track's level is unrecoverable.
        let mut loud = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
        loud.gain_db = timeline::ParamTrack::constant(ParamValue::Number(0.0));
        let mut quiet = audio_track(2, vec![audio_clip(2, 2, 0, TIMEBASE, 0)]);
        quiet.gain_db = timeline::ParamTrack::constant(ParamValue::Number(-20.0));
        let p = project_with(vec![loud, quiet]);
        let mut src = ConstSource::new(&[(1, 0.5), (2, 0.5)]);
        let (_, stats) = mix_block(&p, &mut src, 0.0);

        assert_eq!(stats.track_meters.len(), 2);
        let m1 = stats.track_meters.iter().find(|(id, _)| *id == TrackId(1)).unwrap().1;
        let m2 = stats.track_meters.iter().find(|(id, _)| *id == TrackId(2)).unwrap().1;
        assert!(
            (m1.peak_dbfs - m2.peak_dbfs - 20.0).abs() < 0.2,
            "the -20 dB track should meter 20 dB lower: {} vs {}",
            m1.peak_dbfs,
            m2.peak_dbfs
        );
    }

    #[test]
    fn a_muted_track_is_absent_from_the_meters_rather_than_reported_silent() {
        // "Off" and "quiet" are different states, and a mixer that draws them
        // identically hides why a track can't be heard.
        let mut muted = audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)]);
        muted.muted = true;
        let p = project_with(vec![muted, audio_track(2, vec![audio_clip(2, 1, 0, TIMEBASE, 0)])]);
        let mut src = ConstSource::new(&[(1, 0.5)]);
        let (_, stats) = mix_block(&p, &mut src, 0.0);

        assert_eq!(stats.track_meters.len(), 1);
        assert_eq!(stats.track_meters[0].0, TrackId(2));
    }

    #[test]
    fn the_master_meter_reflects_the_master_fader() {
        let p = project_with(vec![audio_track(1, vec![audio_clip(1, 1, 0, TIMEBASE, 0)])]);
        let mut src = ConstSource::new(&[(1, 0.5)]);
        let (_, unity) = mix_block(&p, &mut src, 0.0);
        let mut src = ConstSource::new(&[(1, 0.5)]);
        let (_, cut) = mix_block(&p, &mut src, -12.0);
        assert!(
            (unity.master_meter.peak_dbfs - cut.master_meter.peak_dbfs - 12.0).abs() < 0.2,
            "master meter should follow the master fader: {} vs {}",
            unity.master_meter.peak_dbfs,
            cut.master_meter.peak_dbfs
        );
        // And the track meter is *pre*-master, so it should not have moved.
        assert!(
            (unity.track_meters[0].1.peak_dbfs - cut.track_meters[0].1.peak_dbfs).abs() < 0.01,
            "a track meter is post-fader but pre-master and must not follow the master"
        );
    }

    #[test]
    fn silence_meters_at_the_floor_not_as_a_missing_reading() {
        let p = project_with(vec![audio_track(1, vec![])]);
        let mut src = ConstSource::new(&[]);
        let (_, stats) = mix_block(&p, &mut src, 0.0);
        assert_eq!(stats.track_meters.len(), 1, "an empty but audible track still reports");
        assert_eq!(stats.track_meters[0].1.peak_dbfs, crate::SILENCE_DBFS);
        assert_eq!(stats.master_meter.peak_dbfs, crate::SILENCE_DBFS);
    }
}
