//! Persistent, sequential video decoder — unlike `decode_frame_at` (which
//! opens and closes the file per call, fine for a one-off frame grab), this
//! keeps the decoder open across many `next_frame()` calls, which is what
//! continuous playback (M2) actually needs. Still not the real
//! `media::DecoderPool` (no pooling across multiple assets, no
//! cancellation/generation semantics for concurrent seeks) — this is a
//! single-stream sequential reader, sized for M2's "single clip" scope.

use crate::{DecodedRgbaFrame, ProbeError};
use std::path::Path;

pub struct VideoDecoderStream {
    input: ffmpeg_next::format::context::Input,
    stream_index: usize,
    time_base: ffmpeg_next::Rational,
    decoder: ffmpeg_next::codec::decoder::Video,
    scaler: ffmpeg_next::software::scaling::Context,
    eof_sent: bool,
    /// Set by `seek`: the first frame at or after the seek target, found by
    /// decoding forward past whatever the container-level seek landed on.
    /// `next_frame` returns this before decoding anything further.
    pending_frame: Option<DecodedRgbaFrame>,
}

impl VideoDecoderStream {
    pub fn open(path: &Path) -> Result<Self, ProbeError> {
        let input = crate::open_input(path)?;
        let stream_index = input
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .map(|s| s.index())
            .ok_or(ProbeError::NoDecodableStreams)?;
        let stream = input.stream(stream_index).unwrap();
        let time_base = stream.time_base();
        let decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()?;
        // Refused here rather than per frame: this stream decodes every
        // frame of the clip, so the first allocation is the one to stop.
        crate::rgba_buffer_len(decoder.width(), decoder.height())?;
        let scaler = ffmpeg_next::software::scaling::Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            ffmpeg_next::format::Pixel::RGBA,
            decoder.width(),
            decoder.height(),
            ffmpeg_next::software::scaling::Flags::BILINEAR,
        )?;
        Ok(VideoDecoderStream {
            input,
            stream_index,
            time_base,
            decoder,
            scaler,
            eof_sent: false,
            pending_frame: None,
        })
    }

    /// Seeks to `target_ticks` (our TIMEBASE-based tick), per spec 4.1's
    /// long-GOP seek algorithm: "seek to nearest preceding keyframe, decode
    /// forward, present target frame." The container-level seek below only
    /// does the first half — for a sparse-keyframe encode (confirmed
    /// empirically: a test fixture with a single keyframe for its entire 8s
    /// duration), that can land far before the target, so every subsequent
    /// `next_frame()` would silently replay from the keyframe instead of
    /// the requested position unless we decode-and-discard forward here.
    pub fn seek(&mut self, target_ticks: i64) -> Result<(), ProbeError> {
        let target_us = (target_ticks as i128 * 1_000_000 / crate::timeline_timebase() as i128) as i64;
        self.input.seek(target_us, ..target_us)?;
        self.decoder.flush();
        self.eof_sent = false;
        self.pending_frame = None;

        while let Some(frame) = self.decode_next_raw()? {
            if frame.pts_ticks >= target_ticks {
                self.pending_frame = Some(frame);
                break;
            }
        }
        Ok(())
    }

    /// Returns the next frame in decode order, or `Ok(None)` at end of
    /// stream. Call `seek` first to jump; without it this just continues
    /// from wherever the previous call left off.
    pub fn next_frame(&mut self) -> Result<Option<DecodedRgbaFrame>, ProbeError> {
        if let Some(frame) = self.pending_frame.take() {
            return Ok(Some(frame));
        }
        self.decode_next_raw()
    }

    fn decode_next_raw(&mut self) -> Result<Option<DecodedRgbaFrame>, ProbeError> {
        let mut decoded = ffmpeg_next::frame::Video::empty();
        loop {
            if self.decoder.receive_frame(&mut decoded).is_ok() {
                return Ok(Some(self.convert_to_rgba(&decoded)?));
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

    fn convert_to_rgba(&mut self, decoded: &ffmpeg_next::frame::Video) -> Result<DecodedRgbaFrame, ProbeError> {
        let pts_ticks = crate::pts_to_ticks(decoded.pts(), self.time_base);
        let mut rgba_frame = ffmpeg_next::frame::Video::empty();
        self.scaler.run(decoded, &mut rgba_frame)?;
        let width = rgba_frame.width();
        let height = rgba_frame.height();
        let stride = rgba_frame.stride(0);
        let data = rgba_frame.data(0);
        let mut rgba = vec![0u8; crate::rgba_buffer_len(width, height)?];
        for row in 0..height as usize {
            let src = &data[row * stride..row * stride + (width as usize * 4)];
            let dst_start = row * width as usize * 4;
            rgba[dst_start..dst_start + width as usize * 4].copy_from_slice(src);
        }
        Ok(DecodedRgbaFrame { width, height, rgba, pts_ticks })
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
    fn yields_frames_in_increasing_pts_order() {
        crate::init().unwrap();
        let mut stream = VideoDecoderStream::open(&fixture("test_h264.mp4")).unwrap();
        let mut last_pts = -1i64;
        let mut count = 0;
        while let Some(frame) = stream.next_frame().unwrap() {
            assert!(frame.pts_ticks > last_pts, "frames must be strictly increasing in pts");
            last_pts = frame.pts_ticks;
            count += 1;
        }
        assert!(count >= 80, "expected ~90 frames for a 3s@30fps clip, got {count}");
    }

    #[test]
    fn seek_jumps_forward_and_continues_decoding() {
        crate::init().unwrap();
        let mut stream = VideoDecoderStream::open(&fixture("test_prores.mov")).unwrap();
        stream.seek(crate::timeline_timebase() * 2).unwrap();
        let frame = stream.next_frame().unwrap().expect("frame after seek");
        // ProRes is all-intra, so seeking near tick 2s should land close to
        // it, not snap back to a keyframe far earlier the way a long-GOP
        // codec's seek would.
        let two_sec = crate::timeline_timebase() * 2;
        assert!(
            (frame.pts_ticks - two_sec).abs() < crate::timeline_timebase() / 2,
            "expected to land within ~0.5s of the seek target, got pts_ticks={}",
            frame.pts_ticks
        );
    }

    #[test]
    fn exhausted_stream_keeps_returning_none() {
        crate::init().unwrap();
        let mut stream = VideoDecoderStream::open(&fixture("test_h264.mp4")).unwrap();
        while stream.next_frame().unwrap().is_some() {}
        assert!(stream.next_frame().unwrap().is_none());
        assert!(stream.next_frame().unwrap().is_none());
    }
}
