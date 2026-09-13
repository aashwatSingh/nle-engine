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

pub mod audio_peaks;
pub mod audio_stream;
pub mod matte_video;
pub mod proxy;
pub mod source_reader;
pub mod stream_decoder;
pub use audio_peaks::{generate_audio_peaks, AudioPeaks};
pub use audio_stream::{AudioChunk, AudioDecoderStream};
pub use matte_video::{generate_matte_video, MatteVideoOptions};
pub use proxy::{generate_proxy, ProxyOptions};
pub use source_reader::SourceReader;
pub use stream_decoder::VideoDecoderStream;

/// Reads one packet, treating *any* read error as end-of-stream — not just
/// `ffmpeg_next::Error::Eof`.
///
/// This works around a real bug found empirically while building the M2
/// playback engine: `ffmpeg_next::format::context::input::PacketIter::next`
/// only stops on the exact `Error::Eof` variant and otherwise loops forever
/// retrying `av_read_frame` on *any* other error — including whatever a real
/// (non-corrupt, ffmpeg-generated) file's true end-of-stream condition
/// actually returns, which isn't always exactly `Eof`. Using `.packets()` /
/// `for ... in input.packets()` anywhere near true EOF can hang forever with
/// no error, no panic, and 100% CPU. Every packet-read loop in this crate
/// goes through this function instead of `Input::packets()`.
pub(crate) fn read_next_packet(input: &mut ffmpeg_next::format::context::Input) -> Option<ffmpeg_next::Packet> {
    let mut packet = ffmpeg_next::Packet::empty();
    match packet.read(input) {
        Ok(()) => Some(packet),
        Err(_) => None,
    }
}

/// As `read_next_packet`, but skips packets belonging to other streams.
pub(crate) fn read_next_packet_for_stream(
    input: &mut ffmpeg_next::format::context::Input,
    stream_index: usize,
) -> Option<ffmpeg_next::Packet> {
    loop {
        match read_next_packet(input) {
            Some(packet) if packet.stream() == stream_index => return Some(packet),
            Some(_) => continue,
            None => return None,
        }
    }
}

#[derive(Debug)]
pub enum ProbeError {
    Io(std::io::Error),
    Ffmpeg(ffmpeg_next::Error),
    NoDecodableStreams,
    /// The file declares a frame too large to decode — see
    /// `MAX_DECODED_PIXELS`.
    FrameTooLarge { width: u32, height: u32 },
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

/// Demuxers media import can need: the MP4/QuickTime family (which also
/// covers this app's own proxies, mattes and exports), Matroska/WebM, AVI,
/// MPEG-TS, ASF/WMV, FLV, and the common audio containers. Names are matched
/// against each demuxer's whole alias list, so `mov` also admits mp4, m4a and
/// 3gp, and `matroska` admits webm.
const ALLOWED_DEMUXERS: &str = "mov,matroska,avi,mpegts,asf,flv,wav,mp3,flac,ogg,aac";

/// Opens `path` for demuxing, restricted to what media import needs. Every
/// open in this crate goes through here.
///
/// Every file this crate reads is untrusted — it's whatever the user
/// imported — and FFmpeg parses it in-process, unsandboxed. Two limits
/// shrink what that parsing can reach. FFmpeg picks a demuxer by content, not
/// by extension, so a file named `.mp4` could otherwise select any of several
/// hundred, including ones that go on to open other files or URLs (concat
/// scripts, HLS playlists, image sequences); `format_whitelist` refuses all
/// but `ALLOWED_DEMUXERS`. And `protocol_whitelist=file` keeps even an
/// allowed demuxer from reaching past local files.
pub(crate) fn open_input(path: &Path) -> Result<ffmpeg_next::format::context::Input, ffmpeg_next::Error> {
    let mut options = ffmpeg_next::Dictionary::new();
    options.set("format_whitelist", ALLOWED_DEMUXERS);
    options.set("protocol_whitelist", "file");
    ffmpeg_next::format::input_with_dictionary(path, options)
}

/// The largest frame this editor will decode, in pixels: 8192x8192,
/// comfortably above 8K DCI (8192x4320) and every format an editor
/// plausibly ingests.
///
/// A limit has to exist somewhere. FFmpeg's own default (`max_pixels` =
/// `INT_MAX`) is far too generous for a buffer we allocate per frame at 4
/// bytes a pixel, and dimensions come from the file's own headers — so
/// without a bound, a file declaring an absurd frame size decides how much
/// memory this process asks for. At the very top of that range the
/// arithmetic stops being merely large and starts being wrong: `width *
/// height * 4` is `u32`, which wraps in release builds, and a wrapped
/// length allocates a buffer far too small for the copy that follows.
const MAX_DECODED_PIXELS: u64 = 8192 * 8192;

/// Rejects a frame the editor won't decode, and returns its RGBA buffer
/// size. Checked in `u64` on purpose: the multiplication this replaces
/// overflowed silently in exactly the case worth catching.
pub(crate) fn rgba_buffer_len(width: u32, height: u32) -> Result<usize, ProbeError> {
    let pixels = width as u64 * height as u64;
    if pixels == 0 || pixels > MAX_DECODED_PIXELS {
        return Err(ProbeError::FrameTooLarge { width, height });
    }
    Ok((pixels * 4) as usize)
}

fn to_rational(r: ffmpeg_next::Rational) -> Rational {
    Rational { num: r.numerator().max(0) as u32, den: r.denominator().max(1) as u32 }
}

/// Converts a packet/frame presentation timestamp in `time_base` units to
/// timeline ticks, treating a timestamp that can't be placed as tick 0.
///
/// The zero-denominator guard is the point. A stream's time base comes
/// straight out of the container's own metadata — `open_input` restricts
/// which demuxers run, not what they report — and a file declaring `0`
/// there would otherwise reach an integer division and panic the decode
/// thread. Every other reader of a raw FFmpeg time base in this crate
/// already guards it (`to_rational` clamps, `probe`'s duration maths and
/// both encoders test explicitly); the two decoder streams divided by it
/// unchecked, so this is the shared version they now share rather than a
/// third and fourth copy of the same check.
pub(crate) fn pts_to_ticks(pts: Option<i64>, time_base: ffmpeg_next::Rational) -> i64 {
    let (Some(pts), den) = (pts, time_base.denominator() as i128) else { return 0 };
    if den == 0 {
        return 0;
    }
    (pts as i128 * timeline_timebase() as i128 * time_base.numerator() as i128 / den) as i64
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
    let mut input = open_input(path)?;

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
            // Derived from the stream's own duration in its own timebase,
            // then converted to samples at the decoder's rate. Previously
            // hardcoded to 0 with a TODO, which is worse than absent: it's a
            // public field that silently reports "no audio length" for every
            // file, so anything trusting it (an export length check, a
            // waveform's extent) gets a wrong answer rather than an error.
            // Falls back to the container duration when the stream doesn't
            // carry its own, and to 0 only when neither is known.
            let rate = decoder.rate();
            let stream_duration = stream.duration();
            let tb = stream.time_base();
            let duration_samples = if stream_duration > 0 && tb.denominator() != 0 {
                (stream_duration as i128 * tb.numerator() as i128 * rate as i128
                    / tb.denominator() as i128) as u64
            } else if input.duration() > 0 {
                // Container duration is in AV_TIME_BASE (microseconds).
                (input.duration() as i128 * rate as i128 / 1_000_000) as u64
            } else {
                0
            };
            Some(AudioStreamInfo {
                sample_rate: rate,
                channel_count: decoder.channels(),
                duration_samples,
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
        while let Some(packet) = read_next_packet_for_stream(&mut input, *v_idx) {
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
    let mut input = open_input(path)?;
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
    // Before the scaler, which would otherwise be the first thing to
    // allocate against these dimensions.
    rgba_buffer_len(decoder.width(), decoder.height())?;
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

    while let Some(packet) = read_next_packet_for_stream(&mut input, stream_index) {
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
            let mut rgba = vec![0u8; rgba_buffer_len(width, height)?];
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

pub(crate) const fn timeline_timebase() -> i64 {
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
        // Compares against the real `timeline::TIMEBASE`, not a second copy of
        // the literal — otherwise this test passes no matter how far the two
        // drift, which is exactly the failure it exists to catch.
        assert_eq!(
            timeline_timebase(),
            timeline::TIMEBASE,
            "media_ffmpeg's duplicated timebase has drifted from timeline::TIMEBASE"
        );
    }

    #[test]
    fn an_absurd_frame_size_is_refused_instead_of_allocated() {
        // The dimensions come from the file's own headers, so without this
        // the file decides how much memory the process asks for. 65535
        // square is the interesting case: times four it exceeds u32, which
        // is what the old `width * height * 4` wrapped on, producing a
        // buffer far too small for the copy that followed.
        assert!(matches!(
            rgba_buffer_len(65535, 65535),
            Err(ProbeError::FrameTooLarge { .. })
        ));
        assert!(matches!(rgba_buffer_len(0, 1080), Err(ProbeError::FrameTooLarge { .. })));
        assert!(matches!(rgba_buffer_len(1920, 0), Err(ProbeError::FrameTooLarge { .. })));
    }

    #[test]
    fn real_frame_sizes_up_to_8k_are_still_allowed() {
        // The guard against a limit set so low it refuses real footage.
        assert_eq!(rgba_buffer_len(1920, 1080).unwrap(), 1920 * 1080 * 4);
        assert_eq!(rgba_buffer_len(3840, 2160).unwrap(), 3840 * 2160 * 4);
        assert_eq!(rgba_buffer_len(8192, 4320).unwrap(), 8192 * 4320 * 4);
    }

    #[test]
    fn a_zero_time_base_denominator_does_not_panic_the_decode() {
        // A stream's time base is the container's own metadata, so `0` is
        // something a malformed or crafted file can declare. Before the
        // guard this divided by it and panicked — on a worker thread, where
        // the panic is silent and strands the job rather than crashing
        // visibly. Placing the frame at 0 is the same answer as for a frame
        // carrying no timestamp at all.
        assert_eq!(pts_to_ticks(Some(9000), ffmpeg_next::Rational::new(1, 0)), 0);
        assert_eq!(pts_to_ticks(Some(9000), ffmpeg_next::Rational::new(0, 0)), 0);
        assert_eq!(pts_to_ticks(None, ffmpeg_next::Rational::new(1, 90_000)), 0);
    }

    #[test]
    fn pts_converts_through_the_stream_time_base() {
        // One second at a 90kHz time base is one second of timeline ticks.
        assert_eq!(
            pts_to_ticks(Some(90_000), ffmpeg_next::Rational::new(1, 90_000)),
            timeline_timebase()
        );
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

    /// An imported "video" that is really a concat script makes FFmpeg open
    /// whatever files it names — the same class of trick as an HLS playlist
    /// pointing at local files or URLs. Nothing a user imports as media needs
    /// a demuxer that opens other files, so those demuxers must be refused.
    #[test]
    fn a_concat_script_disguised_as_media_is_refused() {
        init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::copy(fixture("test_h264.mp4"), dir.path().join("inner.mp4")).unwrap();
        let script = dir.path().join("holiday.mp4");
        std::fs::write(&script, "ffconcat version 1.0\nfile 'inner.mp4'\n").unwrap();

        assert!(probe(&script).is_err(), "a concat script must not open as media");
        assert!(
            decode_frame_at(&script, 0).is_err(),
            "decode must refuse it too — every open goes through the same allowlist"
        );
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
    fn probes_h265_mp4() {
        init().unwrap();
        let asset = probe(&fixture("test_h265.mp4")).unwrap();
        assert_eq!((asset.video.unwrap().width, 360), (640, 360));
    }

    #[test]
    fn probes_av1_mkv() {
        init().unwrap();
        let asset = probe(&fixture("test_av1.mkv")).unwrap();
        assert_eq!((asset.video.unwrap().width, 360), (640, 360));
    }

    #[test]
    fn probes_4k() {
        init().unwrap();
        let asset = probe(&fixture("test_4k.mp4")).unwrap();
        let video = asset.video.unwrap();
        assert_eq!((video.width, video.height), (3840, 2160));
    }

    #[test]
    fn probes_vertical_phone_style() {
        init().unwrap();
        let asset = probe(&fixture("test_vertical.mp4")).unwrap();
        let video = asset.video.unwrap();
        assert_eq!((video.width, video.height), (1080, 1920));
    }

    #[test]
    fn detects_vfr_source_as_variable_not_silently_constant() {
        // Spec 4.1: "Do not silently treat VFR as CFR — this is the single
        // most common source of drifting audio in amateur editors."
        init().unwrap();
        let asset = probe(&fixture("test_vfr.mp4")).unwrap();
        let video = asset.video.unwrap();
        assert!(
            matches!(video.frame_rate, FrameRateKind::Variable { .. }),
            "expected Variable, got {:?}",
            video.frame_rate
        );
        assert!(
            !video.pts_index.is_empty(),
            "VFR sources must get a real PTS index, not just a nominal rate"
        );
    }

    #[test]
    fn corrupt_file_fails_cleanly_instead_of_panicking() {
        // Not a substitute for the real fuzzing pass spec section 8
        // requires before v1.0 — just a first, cheap check that the obvious
        // case (truncated file) returns Err rather than panicking or
        // hanging, since that's the actual M1-relevant risk right now.
        init().unwrap();
        let result = probe(&fixture("test_corrupt.mp4"));
        assert!(result.is_err());
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
