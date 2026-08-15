//! AAC encoder for the export muxer.
//!
//! Takes interleaved stereo f32 (what `audio::mix_range` produces) and deals
//! with the two mismatches between that and what AAC wants:
//!
//! 1. **Fixed frame size.** AAC encodes in blocks of exactly
//!    `frame_size` samples (1024 for the native encoder). The mixer produces
//!    one video-frame's worth at a time — 1600 samples at 48kHz/30fps — which
//!    divides into neither. So samples are accumulated here and drained in
//!    whole encoder frames, with the remainder carried to the next push.
//! 2. **Planar layout.** The encoder wants FLTP (each channel in its own
//!    plane); the mixer produces interleaved. De-interleaving happens on the
//!    way in.
//!
//! PTS is a running sample count in a 1/sample_rate timebase, so it can't
//! drift from the actual number of samples written.

use crate::ExportError;
use audio::CHANNELS;

pub struct AudioEncoder {
    encoder: ffmpeg_next::encoder::audio::Encoder,
    /// Leftover interleaved samples that didn't fill a whole encoder frame.
    pending: Vec<f32>,
    frame_size: usize,
    sample_rate: u32,
    layout: ffmpeg_next::channel_layout::ChannelLayout,
    /// Next frame's PTS, counted in samples.
    next_pts: i64,
    encoder_time_base: ffmpeg_next::Rational,
    stream_time_base: ffmpeg_next::Rational,
    stream_index: usize,
}

impl AudioEncoder {
    /// Adds an AAC stream to `octx` and opens the encoder. Must be called
    /// before `octx.write_header()`; `set_stream_time_base` must be called
    /// after it.
    pub fn add_stream(
        octx: &mut ffmpeg_next::format::context::Output,
        sample_rate: u32,
    ) -> Result<Self, ExportError> {
        let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::AAC)
            .ok_or(ExportError::NoAudioEncoder)?;
        let stream_index = octx.nb_streams() as usize;
        let mut ost = octx.add_stream(codec)?;

        let layout = ffmpeg_next::channel_layout::ChannelLayout::default(CHANNELS as i32);
        let encoder_time_base = ffmpeg_next::Rational::new(1, sample_rate as i32);
        let mut encoder =
            ffmpeg_next::codec::context::Context::new_with_codec(codec).encoder().audio()?;
        encoder.set_rate(sample_rate as i32);
        // On FFmpeg 7 the channel count is derived from the layout — there's
        // no separate `set_channels`, and setting the layout is what defines
        // both.
        encoder.set_channel_layout(layout);
        // FLTP is what FFmpeg's native AAC encoder accepts.
        encoder.set_format(ffmpeg_next::format::Sample::F32(
            ffmpeg_next::format::sample::Type::Planar,
        ));
        encoder.set_bit_rate(192_000);
        encoder.set_time_base(encoder_time_base);

        let encoder = encoder.open()?;
        ost.set_parameters(&encoder);
        ost.set_time_base(encoder_time_base);

        // Some encoders report 0 (meaning "any size"); pick a sane block so
        // the accumulate-and-drain logic below always has a real target.
        let frame_size = match encoder.frame_size() {
            0 => 1024,
            n => n as usize,
        };

        Ok(AudioEncoder {
            encoder,
            pending: Vec::new(),
            frame_size,
            sample_rate,
            layout,
            next_pts: 0,
            encoder_time_base,
            // Placeholder until the muxer has chosen; see the note on
            // `Encoder::stream_time_base` for why this must be read back
            // after `write_header`.
            stream_time_base: encoder_time_base,
            stream_index,
        })
    }

    pub fn stream_index(&self) -> usize {
        self.stream_index
    }

    pub fn set_stream_time_base(&mut self, tb: ffmpeg_next::Rational) {
        self.stream_time_base = tb;
    }

    /// Queues interleaved stereo samples and encodes as many whole frames as
    /// they complete.
    pub fn push(
        &mut self,
        octx: &mut ffmpeg_next::format::context::Output,
        interleaved: &[f32],
    ) -> Result<(), ExportError> {
        self.pending.extend_from_slice(interleaved);
        let per_frame = self.frame_size * CHANNELS;
        while self.pending.len() >= per_frame {
            let block: Vec<f32> = self.pending.drain(..per_frame).collect();
            self.encode_block(octx, &block, self.frame_size)?;
        }
        Ok(())
    }

    /// Encodes any remaining partial block, flushes the encoder, and drains
    /// its packets. The tail block is zero-padded to `frame_size` because AAC
    /// can't encode a short frame; that adds at most ~21ms of silence at the
    /// very end, which is inaudible and preferable to dropping real samples.
    pub fn finish(
        mut self,
        octx: &mut ffmpeg_next::format::context::Output,
    ) -> Result<(), ExportError> {
        if !self.pending.is_empty() {
            let real_frames = self.pending.len() / CHANNELS;
            let mut block = std::mem::take(&mut self.pending);
            block.resize(self.frame_size * CHANNELS, 0.0);
            self.encode_block(octx, &block, real_frames)?;
        }
        self.encoder.send_eof()?;
        self.drain(octx)?;
        Ok(())
    }

    fn encode_block(
        &mut self,
        octx: &mut ffmpeg_next::format::context::Output,
        interleaved: &[f32],
        real_frames: usize,
    ) -> Result<(), ExportError> {
        let mut frame = ffmpeg_next::frame::Audio::new(
            ffmpeg_next::format::Sample::F32(ffmpeg_next::format::sample::Type::Planar),
            self.frame_size,
            self.layout,
        );
        // `plane_mut::<f32>` rather than `data_mut` + byte writes: for audio,
        // FFmpeg only fills `linesize[0]`, and `data_mut` derives its slice
        // length from `linesize[index]` — so `data_mut(1)` hands back an
        // empty slice and writing the right channel panics. `plane_mut` sizes
        // from `samples()` instead, which is correct for every plane.
        for ch in 0..CHANNELS {
            let plane = frame.plane_mut::<f32>(ch);
            for i in 0..self.frame_size {
                plane[i] = interleaved[i * CHANNELS + ch];
            }
        }
        frame.set_pts(Some(self.next_pts));
        frame.set_rate(self.sample_rate);
        // Advance by the samples actually supplied, not the padded frame
        // size, so a zero-padded tail can't stretch the reported duration.
        self.next_pts += real_frames as i64;
        self.encoder.send_frame(&frame)?;
        self.drain(octx)
    }

    fn drain(
        &mut self,
        octx: &mut ffmpeg_next::format::context::Output,
    ) -> Result<(), ExportError> {
        let mut packet = ffmpeg_next::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            packet.set_stream(self.stream_index);
            packet.rescale_ts(self.encoder_time_base, self.stream_time_base);
            packet.write_interleaved(octx)?;
        }
        Ok(())
    }
}
