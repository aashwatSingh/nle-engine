//! Persistent, sequential audio decoder producing interleaved f32 samples
//! already resampled to a target rate/channel-count — designed so its
//! output can be pushed straight into an audio device's ring buffer with no
//! further conversion. Parallel to `VideoDecoderStream`; same M2-scoped
//! caveat (single-stream sequential reader, not the real pooled
//! `media::DecoderPool`).

use crate::ProbeError;
use std::path::Path;

/// One decoded, resampled block of interleaved samples plus where it sits in
/// the source.
pub struct AudioChunk {
    /// Presentation time of the chunk's *first* sample, in TIMEBASE ticks.
    pub pts_ticks: i64,
    /// Interleaved f32, already at the requested rate and channel count.
    pub samples: Vec<f32>,
}

pub struct AudioDecoderStream {
    input: ffmpeg_next::format::context::Input,
    stream_index: usize,
    time_base: ffmpeg_next::Rational,
    decoder: ffmpeg_next::codec::decoder::Audio,
    resampler: ffmpeg_next::software::resampling::Context,
    eof_sent: bool,
    /// Set by `seek`: discard whole chunks before the target (see the
    /// module-level note on this being coarser-grained than the video
    /// seek's per-frame precision) — this is `Some` once we've decoded far
    /// enough forward, `None` while still discarding pre-target chunks.
    discard_before_ticks: Option<i64>,
}

impl AudioDecoderStream {
    pub fn open(path: &Path, target_rate: u32, target_channels: u16) -> Result<Self, ProbeError> {
        let input = crate::open_input(path)?;
        let stream_index = input
            .streams()
            .best(ffmpeg_next::media::Type::Audio)
            .map(|s| s.index())
            .ok_or(ProbeError::NoDecodableStreams)?;
        let stream = input.stream(stream_index).unwrap();
        let time_base = stream.time_base();
        let decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .audio()?;
        let out_layout = ffmpeg_next::channel_layout::ChannelLayout::default(target_channels as i32);
        let resampler = ffmpeg_next::software::resampling::Context::get(
            decoder.format(),
            decoder.channel_layout(),
            decoder.rate(),
            ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Packed),
            out_layout,
            target_rate,
        )?;
        Ok(AudioDecoderStream {
            input,
            stream_index,
            time_base,
            decoder,
            resampler,
            eof_sent: false,
            discard_before_ticks: None,
        })
    }

    /// Seeks to `target_ticks`, same long-GOP-aware discipline as
    /// `VideoDecoderStream::seek` (see its doc comment for why the
    /// container-level seek alone isn't enough). Coarser-grained than the
    /// video seek: whole resampled chunks are discarded rather than
    /// individual samples, so this can overshoot by up to one chunk's
    /// duration (typically 10-40ms depending on codec frame size) — audio
    /// scrubbing precision finer than that is a real follow-up, not built
    /// speculatively here.
    pub fn seek(&mut self, target_ticks: i64) -> Result<(), ProbeError> {
        let target_us = (target_ticks as i128 * 1_000_000 / crate::timeline_timebase() as i128) as i64;
        self.input.seek(target_us, ..target_us)?;
        self.decoder.flush();
        self.eof_sent = false;
        self.discard_before_ticks = Some(target_ticks);
        Ok(())
    }

    /// Returns the next chunk of interleaved f32 samples (already at the
    /// target rate/channel count), or `Ok(None)` at end of stream.
    ///
    /// Drops the chunk's timestamp. Use `next_chunk` when the caller needs
    /// to know *where* in the source the samples came from — anything doing
    /// sample-accurate placement (the timeline audio mixer) does.
    pub fn next_samples(&mut self) -> Result<Option<Vec<f32>>, ProbeError> {
        Ok(self.next_chunk()?.map(|c| c.samples))
    }

    /// Same as `next_samples` but keeps the chunk's presentation time, which
    /// the decoder computes anyway. Without it a caller can only count
    /// samples from wherever the stream happens to be, and since `seek` is
    /// chunk-granular (see its doc comment) that means it can't tell how far
    /// past the requested point it actually landed — making sample-accurate
    /// trimming impossible.
    pub fn next_chunk(&mut self) -> Result<Option<AudioChunk>, ProbeError> {
        let mut decoded = ffmpeg_next::frame::Audio::empty();
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                let pts_ticks = crate::pts_to_ticks(decoded.pts(), self.time_base);
                if let Some(target) = self.discard_before_ticks {
                    if pts_ticks < target {
                        continue;
                    }
                    self.discard_before_ticks = None;
                }
                let mut resampled = ffmpeg_next::frame::Audio::empty();
                self.resampler.run(&decoded, &mut resampled)?;
                let n = resampled.samples() * resampled.channels() as usize;
                let data = resampled.data(0);
                let samples: Vec<f32> = data[..n * 4]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                if !samples.is_empty() {
                    return Ok(Some(AudioChunk { pts_ticks, samples }));
                }
                continue;
            }
            if self.eof_sent {
                return Ok(None);
            }
            match crate::read_next_packet_for_stream(&mut self.input, self.stream_index) {
                Some(packet) => self.decoder.send_packet(&packet)?,
                None => {
                    self.decoder.send_eof()?;
                    self.eof_sent = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(name)
    }

    #[test]
    fn resamples_to_requested_rate_and_channels() {
        crate::init().unwrap();
        // test_h264.mp4's audio is mono 44100 Hz; ask for stereo 48000 Hz.
        let mut stream = AudioDecoderStream::open(&fixture("test_h264.mp4"), 48000, 2).unwrap();
        let mut total_samples = 0usize;
        while let Some(chunk) = stream.next_samples().unwrap() {
            assert_eq!(chunk.len() % 2, 0, "stereo output must have an even sample count");
            total_samples += chunk.len();
        }
        let total_frames = total_samples / 2;
        // ~3s at 48000Hz = ~144000 frames; allow generous tolerance for
        // resampler edge effects.
        assert!(
            (120_000..170_000).contains(&total_frames),
            "expected ~144000 frames at 48kHz for a 3s clip, got {total_frames}"
        );
    }

    #[test]
    fn seek_works_after_reading_to_genuine_eof() {
        // AudioEngine's decode-ahead thread routinely reads all the way to
        // real EOF (it races far ahead of real time, filling its ring
        // buffer) before a later seek arrives — this exercises exactly
        // that: read to genuine EOF, poll past it a few times the way the
        // engine's retry loop does, then seek backward and confirm it
        // resumes producing real samples.
        crate::init().unwrap();
        let mut stream = AudioDecoderStream::open(&fixture("test_h264.mp4"), 48000, 2).unwrap();
        let mut chunks_before_eof = 0;
        while stream.next_samples().unwrap().is_some() {
            chunks_before_eof += 1;
        }
        assert!(chunks_before_eof > 0, "should have decoded real content before EOF");
        for _ in 0..5 {
            assert!(stream.next_samples().unwrap().is_none(), "should keep reporting EOF until seeked");
        }

        stream.seek(crate::timeline_timebase()).unwrap(); // 1 second into a 3-second file
        let after_seek = stream.next_samples().unwrap();
        assert!(after_seek.is_some(), "seeking backward after reaching real EOF should resume producing samples");
    }

    #[test]
    fn seek_repositions_audio_stream() {
        crate::init().unwrap();
        let mut stream = AudioDecoderStream::open(&fixture("test_h264.mp4"), 48000, 2).unwrap();
        stream.seek(crate::timeline_timebase() * 2).unwrap();
        let chunk = stream.next_samples().unwrap();
        assert!(chunk.is_some(), "should still produce samples after seeking near the end of a 3s clip");
    }
}
