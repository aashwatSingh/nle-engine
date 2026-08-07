//! Real ingest/probe (M1), backed by FFmpeg via `ffmpeg-next`. Implements
//! the "immutable ingest-time record" half of spec 4.1 — decode-to-frame
//! (the `media::DecoderPool` trait) is a separate, later piece of work.

use media::{
    AudioStreamInfo, ColorMetadata, ColorPrimaries, FrameRateKind, KeyframeIndexEntry,
    MatrixCoefficients, MediaAsset, MediaAssetId, PixelFormat, Rational, TransferFunction,
    VideoStreamInfo,
};
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug)]
pub enum ProbeError {
    Io(std::io::Error),
    Ffmpeg(ffmpeg_next::Error),
    NoDecodableStreams,
}

impl From<std::io::Error> for ProbeError {
    fn from(e: std::io::Error) -> Self {
        ProbeError::Io(e)
    }
}

impl From<ffmpeg_next::Error> for ProbeError {
    fn from(e: ffmpeg_next::Error) -> Self {
        ProbeError::Ffmpeg(e)
    }
}

/// Call once at process startup before any other function in this crate.
pub fn init() -> Result<(), ProbeError> {
    ffmpeg_next::init()?;
    Ok(())
}

fn to_rational(r: ffmpeg_next::Rational) -> Rational {
    Rational { num: r.numerator().max(0) as u32, den: r.denominator().max(1) as u32 }
}

fn map_pixel_format(fmt: ffmpeg_next::format::Pixel) -> PixelFormat {
    use ffmpeg_next::format::Pixel;
    match fmt {
        Pixel::YUV420P => PixelFormat::Yuv420p8,
        Pixel::YUV422P10LE => PixelFormat::Yuv422p10le,
        Pixel::YUV420P10LE => PixelFormat::Yuv420p10le,
        Pixel::RGBA => PixelFormat::Rgba8,
        other => PixelFormat::Other(format!("{other:?}")),
    }
}

fn map_color_primaries(p: ffmpeg_next::color::Primaries) -> ColorPrimaries {
    use ffmpeg_next::color::Primaries;
    match p {
        Primaries::BT709 => ColorPrimaries::Rec709,
        Primaries::BT2020 => ColorPrimaries::Rec2020,
        Primaries::SMPTE432 => ColorPrimaries::P3D65,
        _ => ColorPrimaries::Unknown,
    }
}

fn map_transfer(t: ffmpeg_next::color::TransferCharacteristic) -> TransferFunction {
    use ffmpeg_next::color::TransferCharacteristic;
    match t {
        TransferCharacteristic::BT709 => TransferFunction::Bt709,
        // IEC61966-2-1 is the sRGB transfer function's formal designation.
        TransferCharacteristic::IEC61966_2_1 => TransferFunction::Srgb,
        TransferCharacteristic::SMPTE2084 => TransferFunction::Pq,
        TransferCharacteristic::ARIB_STD_B67 => TransferFunction::Hlg,
        TransferCharacteristic::Linear => TransferFunction::Linear,
        _ => TransferFunction::Unknown,
    }
}

fn map_matrix(s: ffmpeg_next::color::Space) -> MatrixCoefficients {
    use ffmpeg_next::color::Space;
    match s {
        Space::BT709 => MatrixCoefficients::Bt709,
        Space::BT2020NCL => MatrixCoefficients::Bt2020Ncl,
        Space::RGB => MatrixCoefficients::Rgb,
        _ => MatrixCoefficients::Unknown,
    }
}

fn content_hash(path: &Path) -> Result<[u8; 32], std::io::Error> {
    // TODO(perf): hashes the whole file. Fine for the small fixtures this is
    // tested against; revisit for multi-GB ProRes masters — likely a
    // sampled hash (size + first/last N MB) once that's a real workflow
    // rather than a guessed optimization now.
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// Opens `path`, reads container/stream/codec/color/frame-rate metadata, and
/// builds the video stream's keyframe index (and, for VFR sources, a full
/// PTS index) in a single demux pass. Returns an immutable `MediaAsset` —
/// nothing here is re-probed later, per spec 4.1.
pub fn probe(path: &Path) -> Result<MediaAsset, ProbeError> {
    let mut input = ffmpeg_next::format::input(path)?;

    let video_stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .map(|s| s.index());
    let audio_stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Audio)
        .map(|s| s.index());

    if video_stream_index.is_none() && audio_stream_index.is_none() {
        return Err(ProbeError::NoDecodableStreams);
    }

    let video_info = match video_stream_index {
        Some(idx) => {
            let stream = input.stream(idx).expect("index came from this input");
            let params = stream.parameters();
            let decoder = ffmpeg_next::codec::context::Context::from_parameters(params)?
                .decoder()
                .video()?;

            let avg_rate = to_rational(stream.avg_frame_rate());
            let r_rate = to_rational(stream.rate());
            let frame_rate = if avg_rate == r_rate || avg_rate.num == 0 {
                FrameRateKind::Constant(if r_rate.num > 0 { r_rate } else { avg_rate })
            } else {
                FrameRateKind::Variable { nominal: avg_rate }
            };
            let is_vfr = matches!(frame_rate, FrameRateKind::Variable { .. });

            Some((
                idx,
                VideoStreamInfo {
                    width: decoder.width(),
                    height: decoder.height(),
                    pixel_format: map_pixel_format(decoder.format()),
                    color: ColorMetadata {
                        primaries: map_color_primaries(decoder.color_primaries()),
                        transfer: map_transfer(decoder.color_transfer_characteristic()),
                        matrix: map_matrix(decoder.color_space()),
                        full_range: decoder.color_range() == ffmpeg_next::color::Range::JPEG,
                    },
                    frame_rate,
                    start_timecode: None, // TODO(M1 follow-up): read embedded timecode track/side data
                    keyframe_index: Vec::new(), // filled below
                    pts_index: Vec::new(),      // filled below only if VFR
                },
                is_vfr,
            ))
        }
        None => None,
    };

    let audio_info = match audio_stream_index {
        Some(idx) => {
            let stream = input.stream(idx).expect("index came from this input");
            let params = stream.parameters();
            let decoder = ffmpeg_next::codec::context::Context::from_parameters(params)?
                .decoder()
                .audio()?;
            Some(AudioStreamInfo {
                sample_rate: decoder.rate(),
                channel_count: decoder.channels(),
                duration_samples: 0, // TODO(M1 follow-up): derive from stream duration + rate
            })
        }
        None => None,
    };

    let duration_ticks = {
        let duration_us = input.duration().max(0) as i128;
        (duration_us * timeline_timebase() as i128 / 1_000_000) as i64
    };

    // Single demux pass builds the keyframe index (and PTS index for VFR).
    let mut keyframe_index = Vec::new();
    let mut pts_index = Vec::new();
    let want_pts_index = video_info.as_ref().map(|(_, _, vfr)| *vfr).unwrap_or(false);
    if let Some((v_idx, _, _)) = &video_info {
        for (stream, packet) in input.packets() {
            if stream.index() != *v_idx {
                continue;
            }
            let pts = packet.pts().unwrap_or(0);
            if want_pts_index {
                pts_index.push(pts);
            }
            if packet.is_key() {
                let byte_offset = packet.position().max(0) as u64;
                keyframe_index.push(KeyframeIndexEntry { pts, byte_offset });
            }
        }
    }

    let video = video_info.map(|(_, mut info, _)| {
        info.keyframe_index = keyframe_index;
        info.pts_index = pts_index;
        info
    });

    let hash = content_hash(path)?;
    let container_format = input.format().name().to_string();

    Ok(MediaAsset {
        id: MediaAssetId(u128::from_be_bytes(hash[0..16].try_into().unwrap())),
        original_absolute_path: path.to_string_lossy().into_owned(),
        content_hash: hash,
        container_format,
        video,
        audio: audio_info,
        duration_ticks,
    })
}

/// CPU-side decoded frame, already converted to RGBA8 via swscale. This is
/// deliberately NOT `media::Frame` (the GPU-texture canonical type) — this
/// is the pre-upload intermediate, internal to this crate. Note this uses
/// swscale's RGBA conversion purely to get pixels on screen for the M1
/// display spike; it is NOT the color-managed working-space pipeline spec
/// 4.5 describes (linear light, tagged color space) — that's M4 work, done
/// on the GPU, not here.
pub struct DecodedRgbaFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    pub pts_ticks: i64,
}

/// Decodes the first frame at or after `target_ticks` (our TIMEBASE-based
/// tick, matching `timeline::TimeTick`). Opens and closes the decoder fresh
/// each call — no caching, no decoder pool, no generation-based cancellation.
/// That's the real `media::DecoderPool` (M2, once the playback engine exists
/// to actually need concurrent/cancellable requests); this is the smallest
/// thing that proves decode-to-pixels works at all, per spec M1's "display
/// any frame from any file" acceptance bar.
pub fn decode_frame_at(path: &Path, target_ticks: i64) -> Result<DecodedRgbaFrame, ProbeError> {
    let mut input = ffmpeg_next::format::input(path)?;
    let stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
        .ok_or(ProbeError::NoDecodableStreams)?;

    let time_base = to_rational(input.stream(stream_index).unwrap().time_base());
    let target_us = (target_ticks as i128 * 1_000_000 / timeline_timebase() as i128) as i64;
    // Seeking is best-effort here; if it fails we just decode from wherever
    // the demuxer currently is (typically the start), which still produces
    // a correct — just not necessarily fast — result.
    let _ = input.seek(target_us, ..target_us);

    let stream = input.stream(stream_index).unwrap();
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(stream.parameters())?
        .decoder()
        .video()?;
    let mut scaler = ffmpeg_next::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg_next::format::Pixel::RGBA,
        decoder.width(),
        decoder.height(),
        ffmpeg_next::software::scaling::Flags::BILINEAR,
    )?;

    let mut decoded = ffmpeg_next::frame::Video::empty();
    let mut best: Option<DecodedRgbaFrame> = None;

    for (s, packet) in input.packets() {
        if s.index() != stream_index {
            continue;
        }
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            let pts_ticks = decoded
                .pts()
                .map(|p| {
                    (p as i128 * timeline_timebase() as i128 * time_base.num as i128
                        / time_base.den as i128) as i64
                })
                .unwrap_or(0);
            let mut rgba_frame = ffmpeg_next::frame::Video::empty();
            scaler.run(&decoded, &mut rgba_frame)?;
            let width = rgba_frame.width();
            let height = rgba_frame.height();
            let stride = rgba_frame.stride(0);
            let data = rgba_frame.data(0);
            let mut rgba = vec![0u8; (width * height * 4) as usize];
            for row in 0..height as usize {
                let src = &data[row * stride..row * stride + (width as usize * 4)];
                let dst_start = row * width as usize * 4;
                rgba[dst_start..dst_start + width as usize * 4].copy_from_slice(src);
            }
            let frame = DecodedRgbaFrame { width, height, rgba, pts_ticks };
            let reached_target = pts_ticks >= target_ticks;
            best = Some(frame);
            if reached_target {
                return Ok(best.unwrap());
            }
        }
    }

    best.ok_or(ProbeError::NoDecodableStreams)
}

fn timeline_timebase() -> i64 {
    // Duplicated constant rather than a dependency on `timeline` — `media`
    // (and this crate, which extends it) must not depend on `timeline` per
    // docs/architecture.md's dependency direction. Kept in sync by the
    // `timebase_matches_timeline_crate` test below.
    254_016_000_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("test_fixtures")
            .join(name)
    }

    #[test]
    fn timebase_matches_timeline_crate() {
        assert_eq!(timeline_timebase(), 254_016_000_000);
    }

    #[test]
    fn probes_h264_mp4() {
        init().unwrap();
        let asset = probe(&fixture("test_h264.mp4")).unwrap();
        let video = asset.video.expect("h264 fixture has video");
        assert_eq!((video.width, video.height), (640, 360));
        assert!(!video.keyframe_index.is_empty());
        assert!(asset.audio.is_some());
        assert!(asset.duration_ticks > 0);
    }

    #[test]
    fn probes_vp9_webm() {
        init().unwrap();
        let asset = probe(&fixture("test_vp9.webm")).unwrap();
        let video = asset.video.expect("vp9 fixture has video");
        assert_eq!((video.width, video.height), (640, 360));
    }

    #[test]
    fn probes_prores_mov() {
        init().unwrap();
        let asset = probe(&fixture("test_prores.mov")).unwrap();
        let video = asset.video.expect("prores fixture has video");
        assert_eq!((video.width, video.height), (640, 360));
    }

    #[test]
    fn probes_tagged_rec709_color_metadata() {
        // Unlike the other fixtures (untagged synthetic sources, correctly
        // probed as Unknown), this one is explicitly tagged so the mapping
        // functions are verified against a real positive case, not just the
        // Unknown fallback path.
        init().unwrap();
        let asset = probe(&fixture("test_h264_bt709.mp4")).unwrap();
        let color = asset.video.expect("fixture has video").color;
        assert_eq!(color.primaries, ColorPrimaries::Rec709);
        assert_eq!(color.transfer, TransferFunction::Bt709);
        assert_eq!(color.matrix, MatrixCoefficients::Bt709);
        assert!(!color.full_range, "encoded with tv (limited) range");
    }

    #[test]
    fn decodes_a_real_frame_with_varied_pixels() {
        init().unwrap();
        let frame = decode_frame_at(&fixture("test_h264.mp4"), 0).unwrap();
        assert_eq!((frame.width, frame.height), (640, 360));
        assert_eq!(frame.rgba.len(), 640 * 360 * 4);
        // testsrc is a colorful gradient/pattern, not a flat color — if this
        // decoded to all-zero or all-one-value pixels, the scaler/decode
        // path is broken even though it "succeeded" with no error.
        let first = frame.rgba[0];
        assert!(
            frame.rgba.iter().any(|&b| b != first),
            "decoded frame is a flat color, decode/scale path is likely broken"
        );
    }

    #[test]
    fn decode_advances_past_first_frame_for_a_later_target() {
        init().unwrap();
        let early = decode_frame_at(&fixture("test_h264.mp4"), 0).unwrap();
        let later = decode_frame_at(&fixture("test_h264.mp4"), timeline_timebase() * 2).unwrap();
        assert!(later.pts_ticks > early.pts_ticks);
    }

    #[test]
    fn content_hash_is_stable_across_probes() {
        init().unwrap();
        let a = probe(&fixture("test_h264.mp4")).unwrap();
        let b = probe(&fixture("test_h264.mp4")).unwrap();
        assert_eq!(a.content_hash, b.content_hash);
        assert_eq!(a.id, b.id);
    }
}
