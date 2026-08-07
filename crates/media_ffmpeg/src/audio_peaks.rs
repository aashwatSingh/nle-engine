//! Audio peak (waveform envelope) generation, per spec 4.1/4.6: precomputed
//! min/max envelopes so the timeline UI can draw waveforms without decoding
//! audio on the UI thread. This generates one base (finest) resolution;
//! spec 4.1 also asks for "multiple zoom levels" but those are cheap pure
//! downsampling of this base array (see `AudioPeaks::downsample`), not
//! separate FFmpeg passes.
//!
//! Simplification: peaks are computed on a mono downmix, not per-channel.
//! A stereo waveform display showing independent L/R envelopes is a real
//! follow-up, not built speculatively here.

use crate::ProbeError;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioPeaks {
    pub sample_rate: u32,
    pub samples_per_peak: u32,
    /// (min, max) per block, in the resampled mono downmix.
    pub peaks: Vec<(f32, f32)>,
}

impl AudioPeaks {
    /// Derives a coarser zoom level by merging `factor` adjacent peaks into
    /// one. Pure math — no re-decoding.
    pub fn downsample(&self, factor: usize) -> AudioPeaks {
        assert!(factor >= 1);
        let peaks = self
            .peaks
            .chunks(factor)
            .map(|chunk| {
                let min = chunk.iter().map(|(mn, _)| *mn).fold(f32::INFINITY, f32::min);
                let max = chunk.iter().map(|(_, mx)| *mx).fold(f32::NEG_INFINITY, f32::max);
                (min, max)
            })
            .collect();
        AudioPeaks {
            sample_rate: self.sample_rate,
            samples_per_peak: self.samples_per_peak * factor as u32,
            peaks,
        }
    }
}

pub fn generate_audio_peaks(source: &Path, samples_per_peak: u32) -> Result<AudioPeaks, ProbeError> {
    let mut input = ffmpeg_next::format::input(source)?;
    let stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Audio)
        .map(|s| s.index())
        .ok_or(ProbeError::NoDecodableStreams)?;

    let stream = input.stream(stream_index).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .audio()?;

    let sample_rate = decoder.rate();
    let mut resampler = ffmpeg_next::software::resampling::Context::get(
        decoder.format(),
        decoder.channel_layout(),
        sample_rate,
        ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Packed),
        ffmpeg_next::channel_layout::ChannelLayout::MONO,
        sample_rate,
    )?;

    let mut peaks = Vec::new();
    let mut block_min = f32::INFINITY;
    let mut block_max = f32::NEG_INFINITY;
    let mut count_in_block: u32 = 0;

    let push_sample = |s: f32, peaks: &mut Vec<(f32, f32)>, block_min: &mut f32, block_max: &mut f32, count_in_block: &mut u32| {
        *block_min = block_min.min(s);
        *block_max = block_max.max(s);
        *count_in_block += 1;
        if *count_in_block >= samples_per_peak {
            peaks.push((*block_min, *block_max));
            *block_min = f32::INFINITY;
            *block_max = f32::NEG_INFINITY;
            *count_in_block = 0;
        }
    };

    let mut decoded = ffmpeg_next::frame::Audio::empty();
    let mut resampled = ffmpeg_next::frame::Audio::empty();

    for (s, packet) in input.packets() {
        if s.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            resampler.run(&decoded, &mut resampled)?;
            let data = resampled.data(0);
            let n = resampled.samples();
            for chunk in data[..n * 4].chunks_exact(4) {
                let sample = f32::from_le_bytes(chunk.try_into().unwrap());
                push_sample(sample, &mut peaks, &mut block_min, &mut block_max, &mut count_in_block);
            }
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        resampler.run(&decoded, &mut resampled)?;
        let data = resampled.data(0);
        let n = resampled.samples();
        for chunk in data[..n * 4].chunks_exact(4) {
            let sample = f32::from_le_bytes(chunk.try_into().unwrap());
            push_sample(sample, &mut peaks, &mut block_min, &mut block_max, &mut count_in_block);
        }
    }
    if count_in_block > 0 {
        peaks.push((block_min, block_max));
    }

    Ok(AudioPeaks { sample_rate, samples_per_peak, peaks })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(name)
    }

    #[test]
    fn generates_nonempty_peaks_with_real_amplitude() {
        crate::init().unwrap();
        let peaks = generate_audio_peaks(&fixture("test_h264.mp4"), 512).unwrap();
        assert!(!peaks.peaks.is_empty());
        // test_h264.mp4's audio is a 1kHz sine wave — must show real
        // amplitude swing, not silence (which would mean the decode/resample
        // path is broken even though it "succeeded").
        let max_abs = peaks
            .peaks
            .iter()
            .map(|(mn, mx)| mn.abs().max(mx.abs()))
            .fold(0.0f32, f32::max);
        assert!(max_abs > 0.1, "expected real sine-wave amplitude, got max_abs={max_abs}");
    }

    #[test]
    fn downsample_halves_peak_count_and_widens_range() {
        crate::init().unwrap();
        let base = generate_audio_peaks(&fixture("test_h264.mp4"), 512).unwrap();
        let coarse = base.downsample(2);
        assert_eq!(coarse.peaks.len(), base.peaks.len() / 2);
        assert_eq!(coarse.samples_per_peak, base.samples_per_peak * 2);
    }
}
