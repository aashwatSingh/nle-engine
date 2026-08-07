//! Persistent, sequential audio decoder producing interleaved f32 samples
//! already resampled to a target rate/channel-count — designed so its
//! output can be pushed straight into an audio device's ring buffer with no
//! further conversion. Parallel to `VideoDecoderStream`; same M2-scoped
//! caveat (single-stream sequential reader, not the real pooled
//! `media::DecoderPool`).

use crate::ProbeError;
use std::path::Path;

pub struct AudioDecoderStream {
    input: ffmpeg_next::format::context::Input,
    stream_index: usize,
    decoder: ffmpeg_next::codec::decoder::Audio,
    resampler: ffmpeg_next::software::resampling::Context,
    eof_sent: bool,
}

impl AudioDecoderStream {
    pub fn open(path: &Path, target_rate: u32, target_channels: u16) -> Result<Self, ProbeError> {
        let input = ffmpeg_next::format::input(path)?;
        let stream_index = input
            .streams()
            .best(ffmpeg_next::media::Type::Audio)
            .map(|s| s.index())
            .ok_or(ProbeError::NoDecodableStreams)?;
        let stream = input.stream(stream_index).unwrap();
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
        Ok(AudioDecoderStream { input, stream_index, decoder, resampler, eof_sent: false })
    }

    pub fn seek(&mut self, target_ticks: i64) -> Result<(), ProbeError> {
        let target_us = (target_ticks as i128 * 1_000_000 / crate::timeline_timebase() as i128) as i64;
        self.input.seek(target_us, ..target_us)?;
        self.decoder.flush();
        self.eof_sent = false;
        Ok(())
    }

    /// Returns the next chunk of interleaved f32 samples (already at the
    /// target rate/channel count), or `Ok(None)` at end of stream.
    pub fn next_samples(&mut self) -> Result<Option<Vec<f32>>, ProbeError> {
        let mut decoded = ffmpeg_next::frame::Audio::empty();
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                let mut resampled = ffmpeg_next::frame::Audio::empty();
                self.resampler.run(&decoded, &mut resampled)?;
                let n = resampled.samples() * resampled.channels() as usize;
                let data = resampled.data(0);
                let samples: Vec<f32> = data[..n * 4]
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                    .collect();
                if !samples.is_empty() {
                    return Ok(Some(samples));
                }
                continue;
            }
            if self.eof_sent {
                return Ok(None);
            }
            match self.input.packets().find(|(s, _)| s.index() == self.stream_index) {
                Some((_, packet)) => self.decoder.send_packet(&packet)?,
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
    fn seek_repositions_audio_stream() {
        crate::init().unwrap();
        let mut stream = AudioDecoderStream::open(&fixture("test_h264.mp4"), 48000, 2).unwrap();
        stream.seek(crate::timeline_timebase() * 2).unwrap();
        let chunk = stream.next_samples().unwrap();
        assert!(chunk.is_some(), "should still produce samples after seeking near the end of a 3s clip");
    }
}
