//! Proxy generation, per spec 4.1: a lower-resolution, all-intra re-encode
//! of a source, used for smoother scrubbing/playback on heavy footage. This
//! is the transcode capability itself — the background job queue with
//! priorities, cancellation, and UI progress reporting (also spec 4.1) is a
//! separate, later concern; wiring this into that queue is straightforward
//! once the queue exists, so it isn't built speculatively here.

use crate::ProbeError;
use std::path::Path;

#[derive(Debug)]
pub struct ProxyOptions {
    /// Target size for the longer edge; aspect ratio is preserved and the
    /// result is rounded to even dimensions (required for 4:2:0 chroma
    /// subsampling).
    pub max_dimension: u32,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        ProxyOptions { max_dimension: 960 }
    }
}

fn even(n: u32) -> u32 {
    if n % 2 == 0 {
        n
    } else {
        n + 1
    }
}

fn proxy_dimensions(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
    if width >= height {
        let out_w = width.min(max_dimension);
        let out_h = (out_w as u64 * height as u64 / width as u64) as u32;
        (even(out_w), even(out_h.max(2)))
    } else {
        let out_h = height.min(max_dimension);
        let out_w = (out_h as u64 * width as u64 / height as u64) as u32;
        (even(out_w.max(2)), even(out_h))
    }
}

/// Video-only (spec's proxy workflow is about smooth visual scrub; audio
/// still comes from the source or from `generate_audio_peaks`, so it isn't
/// duplicated into the proxy file). All-intra: every frame is a keyframe,
/// so seeking within the proxy is trivial — that's the point of a proxy.
pub fn generate_proxy(source: &Path, options: &ProxyOptions, output: &Path) -> Result<(), ProbeError> {
    let mut input = ffmpeg_next::format::input(source)?;
    let in_stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
        .ok_or(ProbeError::NoDecodableStreams)?;

    let in_stream = input.stream(in_stream_index).unwrap();
    let in_time_base = in_stream.time_base();
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(in_stream.parameters())?
        .decoder()
        .video()?;

    let (out_w, out_h) = proxy_dimensions(decoder.width(), decoder.height(), options.max_dimension);

    let mut octx = ffmpeg_next::format::output(output)?;
    let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::H264)
        .ok_or(ProbeError::NoDecodableStreams)?;
    let mut ost = octx.add_stream(codec)?;
    let encoder_ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);
    let mut encoder = encoder_ctx.encoder().video()?;
    encoder.set_width(out_w);
    encoder.set_height(out_h);
    encoder.set_format(ffmpeg_next::format::Pixel::YUV420P);
    encoder.set_time_base(in_time_base);
    encoder.set_gop(1); // all-intra: every frame a keyframe
    encoder.set_max_b_frames(0);

    let mut dict = ffmpeg_next::Dictionary::new();
    dict.set("preset", "ultrafast");
    dict.set("crf", "28");
    let mut opened_encoder = encoder.open_with(dict)?;
    ost.set_parameters(&opened_encoder);
    ost.set_time_base(in_time_base);

    let mut scaler = ffmpeg_next::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg_next::format::Pixel::YUV420P,
        out_w,
        out_h,
        ffmpeg_next::software::scaling::Flags::BILINEAR,
    )?;

    octx.write_header()?;

    let mut decoded = ffmpeg_next::frame::Video::empty();
    let mut scaled = ffmpeg_next::frame::Video::empty();
    let mut encoded_packet = ffmpeg_next::Packet::empty();
    let mut next_pts: i64 = 0;

    for (stream, packet) in input.packets() {
        if stream.index() != in_stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            scaler.run(&decoded, &mut scaled)?;
            scaled.set_pts(Some(next_pts));
            next_pts += 1;
            opened_encoder.send_frame(&scaled)?;
            while opened_encoder.receive_packet(&mut encoded_packet).is_ok() {
                encoded_packet.set_stream(0);
                encoded_packet.write_interleaved(&mut octx)?;
            }
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        scaler.run(&decoded, &mut scaled)?;
        scaled.set_pts(Some(next_pts));
        next_pts += 1;
        opened_encoder.send_frame(&scaled)?;
        while opened_encoder.receive_packet(&mut encoded_packet).is_ok() {
            encoded_packet.set_stream(0);
            encoded_packet.write_interleaved(&mut octx)?;
        }
    }
    opened_encoder.send_eof()?;
    while opened_encoder.receive_packet(&mut encoded_packet).is_ok() {
        encoded_packet.set_stream(0);
        encoded_packet.write_interleaved(&mut octx)?;
    }
    octx.write_trailer()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(name)
    }

    #[test]
    fn proxy_dimensions_preserve_aspect_and_are_even() {
        assert_eq!(proxy_dimensions(3840, 2160, 960), (960, 540));
        assert_eq!(proxy_dimensions(1080, 1920, 960), (540, 960));
        assert_eq!(proxy_dimensions(639, 359, 960), (640, 360));
    }

    #[test]
    fn generates_a_smaller_all_intra_proxy() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("proxy.mp4");

        generate_proxy(&fixture("test_4k.mp4"), &ProxyOptions { max_dimension: 960 }, &output).unwrap();

        let asset = probe(&output).unwrap();
        let video = asset.video.expect("proxy has video");
        assert_eq!((video.width, video.height), (960, 540));
        // All-intra: keyframe count should equal (or nearly equal) total
        // frame count, unlike the long-GOP source it came from.
        assert!(video.keyframe_index.len() >= 25, "expected ~30 keyframes for a 1s@30fps all-intra proxy, got {}", video.keyframe_index.len());
    }
}
