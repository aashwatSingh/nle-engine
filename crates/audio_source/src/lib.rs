//! The real, decoder-backed `audio::SampleSource`.
//!
//! Sample-accurate placement is the whole job here. `AudioDecoderStream::seek`
//! is chunk-granular (it discards whole decoded chunks, so it can overshoot
//! by tens of milliseconds — see its doc comment), and tens of milliseconds
//! of slip at every clip boundary is audible as a flam on cuts. So this
//! seeks *before* the target, then uses each chunk's PTS to work out exactly
//! which sample within it the caller actually asked for and trims to it.

use audio::SampleSource;
use media_ffmpeg::{AudioChunk, AudioDecoderStream};
use std::collections::HashMap;
use std::path::PathBuf;
use timeline::TIMEBASE;

/// How far before the requested position to seek, to guarantee the target
/// lands *inside* a decoded chunk rather than before the first one we get.
/// 0.5s comfortably exceeds any codec's frame size while staying cheap.
const SEEK_BACKOFF_TICKS: i64 = TIMEBASE / 2;

/// Forward gap beyond which re-seeking beats decoding-and-discarding — same
/// tradeoff as the video `SourceReader`.
const RESEEK_THRESHOLD_FRAMES: i64 = 48_000; // ~1s at 48kHz

struct AssetReader {
    stream: AudioDecoderStream,
    /// Source frame index of `buffer`'s first frame.
    buffer_start: i64,
    /// Interleaved stereo samples decoded but not yet consumed.
    buffer: Vec<f32>,
    ended: bool,
}

pub struct DecodedSampleSource {
    paths: HashMap<media::MediaAssetId, PathBuf>,
    /// `None` marks an asset we already failed to open, so an unreadable
    /// file isn't retried (and re-erroring) on every block.
    readers: HashMap<media::MediaAssetId, Option<AssetReader>>,
    channels: u16,
}

impl DecodedSampleSource {
    pub fn new(paths: HashMap<media::MediaAssetId, PathBuf>) -> Self {
        DecodedSampleSource {
            paths,
            readers: HashMap::new(),
            channels: audio::CHANNELS as u16,
        }
    }

    fn frames_in(buffer: &[f32]) -> i64 {
        (buffer.len() / audio::CHANNELS) as i64
    }

    /// Decodes one more chunk into `reader.buffer`. Returns false at EOF.
    fn pull_chunk(reader: &mut AssetReader, sample_rate: u32) -> bool {
        match reader.stream.next_chunk() {
            Ok(Some(AudioChunk { pts_ticks, samples })) => {
                if reader.buffer.is_empty() {
                    // Anchor the buffer to where this chunk actually starts.
                    reader.buffer_start =
                        (pts_ticks as i128 * sample_rate as i128 / TIMEBASE as i128) as i64;
                }
                reader.buffer.extend_from_slice(&samples);
                true
            }
            Ok(None) => {
                reader.ended = true;
                false
            }
            Err(_) => {
                reader.ended = true;
                false
            }
        }
    }
}

impl SampleSource for DecodedSampleSource {
    fn samples_at(
        &mut self,
        asset: media::MediaAssetId,
        start_frame: i64,
        frames: usize,
        sample_rate: u32,
    ) -> Vec<f32> {
        let channels = self.channels;
        let paths = &self.paths;
        let entry = self.readers.entry(asset).or_insert_with(|| {
            let path = paths.get(&asset)?;
            let stream = AudioDecoderStream::open(path, sample_rate, channels).ok()?;
            Some(AssetReader { stream, buffer_start: 0, buffer: Vec::new(), ended: false })
        });
        let Some(reader) = entry.as_mut() else { return Vec::new() };

        let want_end = start_frame + frames as i64;
        let buffered_end = reader.buffer_start + Self::frames_in(&reader.buffer);
        let needs_seek = reader.buffer.is_empty()
            || start_frame < reader.buffer_start
            || start_frame > buffered_end + RESEEK_THRESHOLD_FRAMES;

        if needs_seek {
            let target_ticks = (start_frame as i128 * TIMEBASE as i128 / sample_rate as i128) as i64;
            let seek_to = (target_ticks - SEEK_BACKOFF_TICKS).max(0);
            if reader.stream.seek(seek_to).is_err() {
                return Vec::new();
            }
            reader.buffer.clear();
            reader.buffer_start = 0;
            reader.ended = false;
            // The first chunk after the seek establishes buffer_start, so
            // everything below can work in absolute source-frame terms.
            if !Self::pull_chunk(reader, sample_rate) {
                return Vec::new();
            }
        }

        // Decode until the buffer covers the whole request (or the file ends).
        while reader.buffer_start + Self::frames_in(&reader.buffer) < want_end && !reader.ended {
            if !Self::pull_chunk(reader, sample_rate) {
                break;
            }
        }

        // Copy by absolute source-frame index rather than by "drain then take
        // from the front". The drain-first approach silently desyncs when the
        // skip-forward is larger than what's currently buffered: the drain
        // clamps to the buffer length, `buffer_start` advances by less than
        // asked, and every subsequent frame is then read from the wrong
        // offset. Indexing absolutely can't drift, and it makes both edge
        // cases fall out for free — an under- or over-shooting buffer just
        // leaves those frames as the silence they were initialised to.
        let mut out = vec![0.0f32; frames * audio::CHANNELS];
        let buf_start = reader.buffer_start;
        let buf_end = buf_start + Self::frames_in(&reader.buffer);
        let copy_from = start_frame.max(buf_start);
        let copy_to = want_end.min(buf_end);
        if copy_to <= copy_from {
            // Nothing usable. Past end of file is normal for a clip whose
            // source ran out, and returning silence keeps it out of the
            // caller's missing-source count; anything else means the asset
            // genuinely yielded nothing, which the caller should report.
            return if reader.ended { out } else { Vec::new() };
        }
        for f in copy_from..copy_to {
            let dst = ((f - start_frame) as usize) * audio::CHANNELS;
            let src = ((f - buf_start) as usize) * audio::CHANNELS;
            out[dst] = reader.buffer[src];
            out[dst + 1] = reader.buffer[src + 1];
        }

        // Drop what can never be asked for again (requests advance
        // monotonically within a clip), keeping the buffer bounded on long
        // clips while leaving `buffer_start` consistent with its contents.
        if copy_to > buf_start {
            let drop_frames = (copy_to - buf_start) as usize;
            let drop_samples = (drop_frames * audio::CHANNELS).min(reader.buffer.len());
            reader.buffer.drain(..drop_samples);
            reader.buffer_start += (drop_samples / audio::CHANNELS) as i64;
        }
        out
    }
}
