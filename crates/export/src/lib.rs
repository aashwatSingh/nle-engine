//! Export: render a sequence to a real H.264 mp4.
//!
//! The frame loop deliberately reuses the *same* `GraphCompiler` +
//! `Compositor` the preview panel uses, via `render_to_rgba` — M4e's
//! acceptance test proved `render_to_view` (preview) and `render_to_rgba`
//! (export) produce identical pixels from the same graph, so "what you see
//! is what you get" is a structural property here, not something kept in
//! sync by hand.
//!
//! Audio comes from `audio::mix_range` (per-clip gain and pan, track
//! mute/solo, master gain) fed by a decoder-backed sample source, muxed as
//! an AAC stream. A sequence with no audio clips gets no audio stream at
//! all rather than a silent one.
//!
//! Scope, stated honestly:
//! - **H.264 / yuv420p + AAC / mp4 only**, at a caller-chosen CRF. No
//!   ProRes, no hardware encoders, no multi-pass, no per-codec option
//!   surface.
//! - **No track faders, submixes, or audio insert effects** — see
//!   `audio::timeline_mix`'s module doc for why those wait on a persisted
//!   mixer graph.
//! - **Clip speed doesn't retime audio.** Reported via
//!   `ExportStats::audio_clips_with_unsupported_speed` rather than silently
//!   desyncing.
//! - **Single-threaded, synchronous.** One frame decoded, composited, and
//!   encoded at a time. The caller runs it on a background thread (see the
//!   editor's File > Export) and gets progress via the callback.

mod audio_encoder;

use media_ffmpeg::SourceReader;
use render::{BuiltinRegistry, Compositor, DeliverySpace, GraphCompiler, SourceFrames};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use timeline::{Project, SequenceId, TimeTick, TrackKind};

/// A named quality/speed point, so callers pick an intent rather than guessing
/// at a CRF number and an x264 preset string.
///
/// The two knobs trade differently and are easy to get backwards: **CRF** sets
/// quality (lower = better and bigger), while **preset** sets how hard the
/// encoder works to hit that quality (slower = smaller file, same quality).
/// Exposing them raw invited combinations that make no sense together, like
/// CRF 28 at `veryslow`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityPreset {
    /// Fast, larger file. For checking an edit, not for delivery.
    Draft,
    /// Visually transparent for most material. The default.
    High,
    /// Near-lossless, much slower and much bigger. For an intermediate that
    /// will be re-encoded downstream.
    Master,
}

impl QualityPreset {
    pub const ALL: [QualityPreset; 3] =
        [QualityPreset::Draft, QualityPreset::High, QualityPreset::Master];

    pub fn label(self) -> &'static str {
        match self {
            QualityPreset::Draft => "Draft (fast, larger)",
            QualityPreset::High => "High (recommended)",
            QualityPreset::Master => "Master (near-lossless, slow)",
        }
    }

    /// `(crf, x264 preset)`.
    pub fn encoder_settings(self) -> (u32, &'static str) {
        match self {
            QualityPreset::Draft => (26, "veryfast"),
            QualityPreset::High => (18, "medium"),
            QualityPreset::Master => (12, "slow"),
        }
    }
}

/// Export resolution relative to the sequence's native size. A closed
/// enum rather than a raw percentage or explicit width/height, for the
/// same reason `QualityPreset` is a closed enum rather than a raw CRF
/// number: a caller can't construct a nonsense value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputScale {
    /// The sequence's own resolution — the default, and the only value
    /// that must produce byte-identical behavior to code that predates
    /// this enum.
    Native,
    /// A percentage of the sequence's native size. Only the values in
    /// `ALL` (25/50/75) are reachable today — nothing constructs an
    /// arbitrary percentage, and custom resolution entry is deliberately
    /// out of scope.
    ///
    /// `scaled_dimensions` truncates through `as u32`, which is only
    /// lossy above roughly a 223-million-percent scale, so it is not
    /// guarded here. **If custom entry is ever added, bound the input at
    /// the UI** rather than relying on that: values far below the
    /// truncation point (a few thousand percent) already produce
    /// resolutions no encoder will accept.
    Percent(u32),
}

impl OutputScale {
    pub const ALL: [OutputScale; 4] =
        [OutputScale::Native, OutputScale::Percent(75), OutputScale::Percent(50), OutputScale::Percent(25)];

    pub fn label(self) -> &'static str {
        match self {
            OutputScale::Native => "Native",
            OutputScale::Percent(75) => "75%",
            OutputScale::Percent(50) => "50%",
            OutputScale::Percent(25) => "25%",
            OutputScale::Percent(_) => "Custom",
        }
    }

    /// Applies this scale to `(width, height)`, rounding up to even
    /// dimensions — required for yuv420p 4:2:0 chroma subsampling, same
    /// convention `export_sequence` already applies to the native size.
    pub fn scaled_dimensions(self, width: u32, height: u32) -> (u32, u32) {
        let pct = match self {
            OutputScale::Native => return (width, height),
            OutputScale::Percent(p) => p,
        };
        let even = |n: u32| if n.is_multiple_of(2) { n.max(2) } else { n + 1 };
        let scaled_w = (width as u64 * pct as u64 / 100) as u32;
        let scaled_h = (height as u64 * pct as u64 / 100) as u32;
        (even(scaled_w), even(scaled_h))
    }
}

#[derive(Debug)]
pub struct ExportOptions {
    pub quality: QualityPreset,
    /// Output audio sample rate. The mixer resamples every source to this.
    pub sample_rate: u32,
    /// Master bus gain applied after summing, in dB.
    pub master_gain_db: f64,
    /// Sequence range to render, in ticks. `None` exports the whole sequence.
    ///
    /// Output timestamps always start at zero regardless — a range export is a
    /// standalone file, not a clip that begins several seconds in with nothing
    /// on screen.
    pub range_ticks: Option<(i64, i64)>,
    /// Output resolution relative to the sequence's native size.
    pub output_scale: OutputScale,
}

impl Default for ExportOptions {
    fn default() -> Self {
        ExportOptions {
            quality: QualityPreset::High,
            sample_rate: 48_000,
            master_gain_db: 0.0,
            range_ticks: None,
            output_scale: OutputScale::Native,
        }
    }
}

#[derive(Debug)]
pub enum ExportError {
    SequenceNotFound(SequenceId),
    /// The sequence has no clips, so there is nothing to render. Caught up
    /// front because the alternative is writing a valid-but-empty mp4, which
    /// looks like a successful export of a broken result.
    EmptySequence,
    /// `range_ticks` described nothing renderable (out at or before in, or a
    /// range entirely outside the sequence).
    EmptyRange,
    Ffmpeg(ffmpeg_next::Error),
    Media(media_ffmpeg::ProbeError),
    /// No GPU adapter available for the compositor.
    NoGpu,
    /// This FFmpeg build has no AAC encoder, so audio can't be written.
    NoAudioEncoder,
    /// The progress callback returned `false`. The partial output file is
    /// removed before returning, so a cancelled export never leaves a
    /// truncated video behind looking like a finished one.
    Cancelled,
}

impl From<ffmpeg_next::Error> for ExportError {
    fn from(e: ffmpeg_next::Error) -> Self {
        ExportError::Ffmpeg(e)
    }
}

impl From<media_ffmpeg::ProbeError> for ExportError {
    fn from(e: media_ffmpeg::ProbeError) -> Self {
        ExportError::Media(e)
    }
}

/// Not `Eq`: `loudness` carries `f32` measurements, and float equality is the
/// wrong relation for them anyway — callers compare against a delivery target
/// with a tolerance, never for exact equality.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExportStats {
    pub frames_written: u32,
    pub width: u32,
    pub height: u32,
    /// Frames where at least one track had an active clip but no decodable
    /// source frame. Surfaced rather than swallowed: a nonzero count means
    /// the output has gaps the user should know about (missing/unreadable
    /// media), which is exactly the failure that's invisible in the file
    /// itself.
    pub frames_with_missing_sources: u32,
    /// True when an AAC stream was written. False means the sequence had no
    /// audio clips at all — no silent track is added in that case.
    pub audio_written: bool,
    /// Audio clips whose source produced nothing (missing/unreadable media),
    /// counted across the whole export.
    pub audio_clips_with_no_source: u32,
    /// Audio clips whose speed can't be honoured — a keyframed (time-remapped)
    /// curve or a reverse speed. Their audio is mixed at 1x, so it will not
    /// line up with the retimed video; surfaced rather than silently desynced.
    /// A constant non-1x speed *is* retimed and doesn't appear here. See
    /// `audio::timeline_mix`'s module doc.
    pub audio_clips_with_unsupported_speed: u32,
    /// EBU R128 loudness and true peak of the audio actually written, or
    /// `None` when the export had no audio stream at all.
    ///
    /// Measured over the mixed output — the very samples handed to the
    /// encoder — rather than estimated from the source clips, because clip
    /// gain, pan, track faders and the master bus all sit between the two.
    /// `None` rather than a silence reading for a video-only export: a
    /// delivery check reading `Some(-120 LUFS)` would flag a silent-audio
    /// failure on a file that correctly has no audio.
    pub loudness: Option<audio::LoudnessMeasurement>,
}

/// Number of output frames a sequence of `duration_ticks` produces at `rate`.
/// Rounds up so a sequence whose length isn't a whole number of frames still
/// includes its final partial frame rather than truncating it away.
fn frame_count(duration_ticks: i64, ticks_per_frame: i64) -> u32 {
    if duration_ticks <= 0 || ticks_per_frame <= 0 {
        return 0;
    }
    ((duration_ticks + ticks_per_frame - 1) / ticks_per_frame) as u32
}

/// Audio frame index at `tick`. Truncating (not rounding) so the sequence of
/// per-video-frame targets is monotonic and never overshoots the timeline.
fn audio_frame_index(tick: i64, sample_rate: u32) -> u64 {
    (tick.max(0) as i128 * sample_rate as i128 / timeline::TIMEBASE as i128) as u64
}

/// Renders `sequence` of `project` to an H.264 mp4 at `output`.
///
/// `asset_paths` maps each asset to a readable file (the editor's
/// `EditorState::asset_paths`, already resolved through project-relative
/// relinking). Assets missing from the map render as gaps and bump
/// `frames_with_missing_sources`.
///
/// `progress` is called once per frame with `(frame_index, total_frames)`;
/// returning `false` cancels the export and deletes the partial file.
pub fn export_sequence(
    project: &Project,
    sequence_id: SequenceId,
    asset_paths: &HashMap<media::MediaAssetId, PathBuf>,
    output: &Path,
    options: &ExportOptions,
    mut progress: impl FnMut(u32, u32) -> bool,
) -> Result<ExportStats, ExportError> {
    let sequence = project
        .sequences
        .iter()
        .find(|s| s.id == sequence_id)
        .ok_or(ExportError::SequenceNotFound(sequence_id))?;

    let ticks_per_frame = sequence.settings.frame_rate.ticks_per_frame();
    let sequence_duration = sequence.duration().0;

    // Range export. Snapped to the frame grid so the first output frame is a
    // real frame boundary rather than a fractional offset, and clamped to the
    // sequence so an in/out pair left over from a longer edit can't ask for
    // frames that don't exist.
    let (range_start, range_end) = match options.range_ticks {
        Some((a, b)) => {
            let start = (a.clamp(0, sequence_duration) / ticks_per_frame) * ticks_per_frame;
            let end = b.clamp(0, sequence_duration);
            if end <= start {
                return Err(ExportError::EmptyRange);
            }
            (start, end)
        }
        None => (0, sequence_duration),
    };
    let duration = range_end - range_start;
    let total_frames = frame_count(duration, ticks_per_frame);
    if total_frames == 0 {
        return Err(ExportError::EmptySequence);
    }

    // Even dimensions are required by yuv420p chroma subsampling; a sequence
    // configured at an odd size would otherwise fail deep inside the encoder
    // with a much less obvious error.
    let width = sequence.settings.width + (sequence.settings.width % 2);
    let height = sequence.settings.height + (sequence.settings.height % 2);
    // The compositor still renders at the sequence's native size (below) —
    // only the encoder's output size changes. This is what keeps
    // export/preview pixel-identical at `OutputScale::Native` and avoids
    // any risk of effects behaving differently at a scaled render target.
    let (out_width, out_height) = options.output_scale.scaled_dimensions(width, height);

    let (device, queue) = render::headless_context().ok_or(ExportError::NoGpu)?;
    let compositor = Compositor::new(device, queue);
    let compiler = GraphCompiler::new(BuiltinRegistry::default());

    // No audio clips means no audio stream at all. A silent AAC track would
    // be indistinguishable, in the file, from "the mix failed" — better to
    // let the absence be the signal.
    let has_audio = sequence
        .tracks
        .iter()
        .any(|t| t.kind == TrackKind::Audio && !t.clips.is_empty());

    let mut encoder = Encoder::open(
        output,
        width,
        height,
        out_width,
        out_height,
        sequence.settings.frame_rate,
        options,
        has_audio,
    )?;
    let mut readers: HashMap<(media::MediaAssetId, timeline::ClipInstanceId), Option<SourceReader>> =
        HashMap::new();
    let mut audio_source = audio_source::DecodedSampleSource::new(asset_paths.clone());
    let mix_options = audio::MixOptions {
        sample_rate: options.sample_rate,
        master_gain_db: options.master_gain_db,
    };
    // Audio is emitted in step with video frames, but the boundary is tracked
    // in *samples* rather than recomputed from each frame's tick: rounding
    // tick->sample independently per frame would drop or duplicate a sample
    // whenever the frame duration isn't a whole number of samples (1601.6
    // samples per frame at 48kHz/29.97fps), and those add up to audible drift
    // over a long export. Advancing a sample counter makes the stream
    // continuous by construction.
    // Absolute sequence frame index, not a count of frames emitted: it doubles
    // as the read position for `mix_range`, so on a range export it has to start
    // at the range's own start or the audio would come from the wrong place
    // while the video came from the right one. `Encoder::write_audio` keeps its
    // own output-side PTS counter, so the file still starts at zero.
    // Built even when there's no audio (it costs a few hundred bytes and no
    // work until something is pushed), so the frame loop below doesn't need an
    // `Option` dance around every push.
    let mut loudness = audio::LoudnessAnalyzer::new(options.sample_rate, audio::CHANNELS);
    let mut audio_frames_written: u64 = if has_audio {
        audio_frame_index(range_start, options.sample_rate)
    } else {
        0
    };
    let total_audio_frames = if has_audio {
        audio_frame_index(range_end, options.sample_rate)
            .min(audio::total_frames(project, sequence_id, options.sample_rate))
    } else {
        0
    };
    let mut stats = ExportStats {
        frames_written: 0,
        width: out_width,
        height: out_height,
        frames_with_missing_sources: 0,
        audio_written: has_audio,
        loudness: None,
        audio_clips_with_no_source: 0,
        audio_clips_with_unsupported_speed: 0,
    };

    for frame_index in 0..total_frames {
        if !progress(frame_index, total_frames) {
            encoder.abort(output);
            return Err(ExportError::Cancelled);
        }

        // Sequence tick to read, offset by the range's start; output PTS stays
        // frame_index-based so the file always begins at zero.
        let tick = TimeTick(range_start + frame_index as i64 * ticks_per_frame);
        let Some(graph) = compiler.compile(project, sequence_id, tick) else {
            // The sequence existed above, so a `None` here would mean the
            // compiler rejected it mid-render (e.g. nesting cycle). Nothing
            // sensible to draw — skip rather than abort, and let the missing
            // -source counter reflect it.
            stats.frames_with_missing_sources += 1;
            continue;
        };

        // One frame per asset per output frame, cleared each time: the
        // compositor's lookup falls back to the earliest held frame, so a
        // single insert always satisfies the request regardless of how the
        // decoded PTS lines up with the graph's computed source tick.
        let mut sources = SourceFrames::default();
        let mut missing = false;
        // `media_requests` covers transitions' outgoing layers and nested
        // sequences too, so export can't render a dissolve with one input
        // missing while the preview shows it correctly.
        for req in graph.media_requests() {
            let color = project
                .assets
                .iter()
                .find(|a| a.id == req.asset)
                .and_then(|a| a.video.as_ref())
                .map(|v| v.color);
            let (Some(color), Some(path)) = (color, asset_paths.get(&req.asset)) else {
                missing = true;
                continue;
            };

            // Cache the open/failed-to-open decision per (asset, clip) so an
            // unreadable file isn't retried (and re-erroring) on every frame,
            // and so two clips off one file each keep their own read position.
            let reader = readers
                .entry((req.asset, req.clip))
                .or_insert_with(|| SourceReader::open(path).ok());
            let Some(reader) = reader.as_mut() else {
                missing = true;
                continue;
            };
            match reader.frame_at(req.source_pts_ticks) {
                Some(frame) => {
                    let tex = compositor.upload_rgba(&frame.rgba, frame.width, frame.height, color);
                    sources.insert(req.asset, frame.pts_ticks, tex);
                }
                None => missing = true,
            }
        }
        if missing {
            stats.frames_with_missing_sources += 1;
        }

        let (rendered, _) = compositor.render_to_rgba(&graph, &sources, DeliverySpace::Rec709);
        encoder.write_frame(&rendered.rgba, frame_index as i64)?;
        stats.frames_written += 1;

        if has_audio {
            // Mix from where the audio stream currently ends up to where this
            // video frame ends, so the two streams stay locked together
            // without either one rounding independently.
            let next_frame_end_tick = range_start + (frame_index as i64 + 1) * ticks_per_frame;
            let target_audio_frames =
                audio_frame_index(next_frame_end_tick, options.sample_rate).min(total_audio_frames);
            if target_audio_frames > audio_frames_written {
                let block_frames = target_audio_frames - audio_frames_written;
                let block_start_tick =
                    audio::frames_to_ticks(audio_frames_written, options.sample_rate);
                let block_ticks = audio::frames_to_ticks(
                    audio_frames_written + block_frames,
                    options.sample_rate,
                ) - block_start_tick;
                let (samples, mix_stats) = audio::mix_range(
                    project,
                    sequence_id,
                    block_start_tick,
                    block_ticks,
                    &mix_options,
                    &mut audio_source,
                );
                stats.audio_clips_with_no_source += mix_stats.clips_with_no_source;
                stats.audio_clips_with_unsupported_speed += mix_stats.clips_with_unsupported_speed;
                // Fed the same slice the encoder gets, so the reported figure
                // describes the delivered file rather than an intermediate.
                loudness.push(&samples);
                encoder.write_audio(&samples)?;
                audio_frames_written += (samples.len() / audio::CHANNELS) as u64;
            }
        }
    }

    encoder.finish()?;
    if has_audio {
        stats.loudness = Some(loudness.finish());
    }
    Ok(stats)
}

/// Thin RAII-ish wrapper over the ffmpeg output context + H.264 encoder +
/// RGBA->YUV420P scaler. Split out so the frame loop above reads as the
/// render pipeline it is, rather than being half muxer bookkeeping.
struct Encoder {
    octx: ffmpeg_next::format::context::Output,
    /// The *opened* encoder — a distinct type from the unopened
    /// `encoder::video::Video` that `open_with` consumes.
    encoder: ffmpeg_next::encoder::video::Encoder,
    scaler: ffmpeg_next::software::scaling::Context,
    width: u32,
    height: u32,
    /// The timebase packets come out of the encoder in (1/fps).
    encoder_time_base: ffmpeg_next::Rational,
    /// The timebase the *muxer* actually chose, read back after
    /// `write_header`. mp4 overrides whatever the caller asked for (it uses
    /// its own tick rate), so every packet's PTS/DTS has to be rescaled from
    /// `encoder_time_base` into this or the container reports a duration off
    /// by the ratio between them — a 2s export showed up as 0.004s before
    /// this was added.
    stream_time_base: ffmpeg_next::Rational,
    /// `None` when the sequence has no audio clips.
    audio: Option<audio_encoder::AudioEncoder>,
}

impl Encoder {
    fn open(
        output: &Path,
        in_width: u32,
        in_height: u32,
        out_width: u32,
        out_height: u32,
        rate: timeline::FrameRate,
        options: &ExportOptions,
        with_audio: bool,
    ) -> Result<Self, ExportError> {
        let (num, den) = rate.as_rational();
        // FFmpeg time_base is seconds-per-tick, so it's the reciprocal of the
        // frame rate: 30000/1001 fps -> 1001/30000.
        let time_base = ffmpeg_next::Rational::new(den as i32, num as i32);

        let mut octx = ffmpeg_next::format::output(&output.to_path_buf())?;
        let codec =
            ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::H264).ok_or(ExportError::NoGpu)?;
        let mut ost = octx.add_stream(codec)?;
        let mut encoder =
            ffmpeg_next::codec::context::Context::new_with_codec(codec).encoder().video()?;
        encoder.set_width(out_width);
        encoder.set_height(out_height);
        encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
        encoder.set_time_base(time_base);
        encoder.set_frame_rate(Some(ffmpeg_next::Rational::new(num as i32, den as i32)));

        let mut dict = ffmpeg_next::Dictionary::new();
        let (crf, x264_preset) = options.quality.encoder_settings();
        dict.set("preset", x264_preset);
        dict.set("crf", &crf.to_string());
        let encoder = encoder.open_with(dict)?;
        ost.set_parameters(&encoder);
        ost.set_time_base(time_base);

        // Source dims match what the compositor actually rendered
        // (`in_width`/`in_height`, always the sequence's native size);
        // destination dims are the (possibly scaled) encoder output. This
        // scaler already existed purely for RGBA->YUV420P pixel-format
        // conversion — giving it different src/dst sizes makes it do the
        // resize in the same pass, so no second scaling step is needed.
        let scaler = ffmpeg_next::software::scaling::Context::get(
            ffmpeg_next::format::Pixel::RGBA,
            in_width,
            in_height,
            ffmpeg_next::format::Pixel::YUV420P,
            out_width,
            out_height,
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )?;

        // Both streams must exist before write_header, so the audio stream is
        // added here rather than lazily on the first audio block.
        let mut audio =
            if with_audio { Some(audio_encoder::AudioEncoder::add_stream(&mut octx, options.sample_rate)?) } else { None };

        octx.write_header()?;
        // Read back what the muxer settled on — must be after write_header.
        let stream_time_base = octx.stream(0).expect("stream 0 was just added").time_base();
        if let Some(audio) = audio.as_mut() {
            let tb = octx
                .stream(audio.stream_index())
                .expect("audio stream was just added")
                .time_base();
            audio.set_stream_time_base(tb);
        }
        Ok(Encoder {
            octx,
            encoder,
            scaler,
            width: in_width,
            height: in_height,
            encoder_time_base: time_base,
            stream_time_base,
            audio,
        })
    }

    fn write_audio(&mut self, interleaved: &[f32]) -> Result<(), ExportError> {
        if let Some(audio) = self.audio.as_mut() {
            audio.push(&mut self.octx, interleaved)?;
        }
        Ok(())
    }

    fn write_frame(&mut self, rgba: &[u8], pts: i64) -> Result<(), ExportError> {
        let mut src = ffmpeg_next::frame::Video::new(
            ffmpeg_next::format::Pixel::RGBA,
            self.width,
            self.height,
        );
        // The compositor hands back tightly packed rows; an ffmpeg frame's
        // rows are stride-aligned, so this copies row by row rather than in
        // one memcpy (which would skew the image whenever stride != width*4).
        let row_bytes = self.width as usize * 4;
        let stride = src.stride(0);
        let dst = src.data_mut(0);
        for row in 0..self.height as usize {
            dst[row * stride..row * stride + row_bytes]
                .copy_from_slice(&rgba[row * row_bytes..(row + 1) * row_bytes]);
        }

        let mut yuv = ffmpeg_next::frame::Video::empty();
        self.scaler.run(&src, &mut yuv)?;
        yuv.set_pts(Some(pts));
        self.encoder.send_frame(&yuv)?;
        self.drain()
    }

    fn drain(&mut self) -> Result<(), ExportError> {
        let mut packet = ffmpeg_next::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(0);
            packet.rescale_ts(self.encoder_time_base, self.stream_time_base);
            packet.write_interleaved(&mut self.octx)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<(), ExportError> {
        self.encoder.send_eof()?;
        self.drain()?;
        // Audio flushes after video so its trailing packets are interleaved
        // into an already-complete video timeline.
        if let Some(audio) = self.audio.take() {
            audio.finish(&mut self.octx)?;
        }
        self.octx.write_trailer()?;
        Ok(())
    }

    /// Close the muxer and delete the partial file. Used on cancellation so
    /// no half-written video is left looking like a completed export.
    fn abort(self, output: &Path) {
        drop(self);
        let _ = std::fs::remove_file(output);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_count_rounds_up_partial_final_frame() {
        let tpf = timeline::FrameRate::Fps30.ticks_per_frame();
        assert_eq!(frame_count(tpf * 10, tpf), 10);
        // Half a frame past 10 must still produce an 11th frame, not drop it.
        assert_eq!(frame_count(tpf * 10 + tpf / 2, tpf), 11);
        assert_eq!(frame_count(0, tpf), 0);
    }

    #[test]
    fn scaled_dimensions_are_even_and_proportional() {
        assert_eq!(OutputScale::Native.scaled_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(OutputScale::Percent(50).scaled_dimensions(1920, 1080), (960, 540));
        assert_eq!(OutputScale::Percent(75).scaled_dimensions(1920, 1080), (1440, 810));
        // 103*50/100 truncates to 51 (odd) -> must round up to 52.
        assert_eq!(OutputScale::Percent(50).scaled_dimensions(103, 100), (52, 50));
    }
}
