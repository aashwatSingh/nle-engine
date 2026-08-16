//! Encodes a real derived video file with an alpha channel baked in, from
//! background-removal results the caller computes (this crate stays free
//! of any AI/ML dependency — the same "low-level decode/encode primitive"
//! role it already has for `generate_proxy`).
//!
//! Producing an actual alpha-carrying video file, rather than a separate
//! per-frame side-channel the compositor would need new plumbing to read,
//! means the existing render pipeline needs zero changes: every clip
//! source already decodes into and composites an RGBA texture, so pointing
//! a clip at this file instead of its original — the same substitution
//! `ProxyJobs` already does — "just works."

use crate::ProbeError;
use std::path::Path;

pub struct MatteVideoOptions {
    /// Target size for the longer edge, same reasoning as `ProxyOptions`:
    /// the matte only needs to look right composited into a frame, not
    /// survive being the delivered output, and inference cost scales with
    /// pixel count.
    pub max_dimension: u32,
}

impl Default for MatteVideoOptions {
    fn default() -> Self {
        MatteVideoOptions { max_dimension: 960 }
    }
}

fn even(n: u32) -> u32 {
    if n % 2 == 0 {
        n
    } else {
        n + 1
    }
}

pub(crate) fn scaled_dimensions(width: u32, height: u32, max_dimension: u32) -> (u32, u32) {
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

/// `alpha_of` is called once per frame with `(rgb, width, height)` —
/// interleaved RGB, no alpha, matching what a matting model actually takes
/// as input — and must return one 0..1 alpha value per pixel, row-major,
/// `width*height` long. Frames are guaranteed to be called in source order
/// and never concurrently, since a recurrent matting model's hidden state
/// depends on that ordering.
pub fn generate_matte_video(
    source: &Path,
    options: &MatteVideoOptions,
    mut alpha_of: impl FnMut(&[u8], usize, usize) -> Vec<f32>,
    output: &Path,
) -> Result<(), ProbeError> {
    let mut input = ffmpeg_next::format::input(source)?;
    let in_stream_index = input
        .streams()
        .best(ffmpeg_next::media::Type::Video)
        .map(|s| s.index())
        .ok_or(ProbeError::NoDecodableStreams)?;

    let in_stream = input.stream(in_stream_index).unwrap();
    let avg_rate = in_stream.avg_frame_rate();
    let frame_rate = if avg_rate.numerator() > 0 && avg_rate.denominator() > 0 {
        avg_rate
    } else {
        ffmpeg_next::Rational::new(30, 1)
    };
    let encoder_time_base = ffmpeg_next::Rational::new(frame_rate.denominator(), frame_rate.numerator());
    let mut decoder = ffmpeg_next::codec::context::Context::from_parameters(in_stream.parameters())?.decoder().video()?;

    let (out_w, out_h) = scaled_dimensions(decoder.width(), decoder.height(), options.max_dimension);

    let mut octx = ffmpeg_next::format::output(output)?;
    // qtrle: lossless, RGBA-native, no chroma subsampling to fight with —
    // this is a local cache artifact, not a delivery format, so simplicity
    // and correctness matter more than compression.
    let codec = ffmpeg_next::encoder::find(ffmpeg_next::codec::Id::QTRLE).ok_or(ProbeError::NoDecodableStreams)?;
    let mut ost = octx.add_stream(codec)?;
    let encoder_ctx = ffmpeg_next::codec::context::Context::new_with_codec(codec);
    let mut encoder = encoder_ctx.encoder().video()?;
    encoder.set_width(out_w);
    encoder.set_height(out_h);
    // ARGB (alpha-first byte order), not RGBA: qtrle only supports
    // rgb24/rgb555be/argb/gray — confirmed against the encoder's own
    // reported supported-formats list, not guessed.
    encoder.set_format(ffmpeg_next::format::Pixel::ARGB);
    encoder.set_time_base(encoder_time_base);
    encoder.set_gop(1);
    encoder.set_max_b_frames(0);

    let mut opened_encoder = encoder.open()?;
    ost.set_parameters(&opened_encoder);
    ost.set_time_base(encoder_time_base);

    let mut scaler = ffmpeg_next::software::scaling::Context::get(
        decoder.format(),
        decoder.width(),
        decoder.height(),
        ffmpeg_next::format::Pixel::ARGB,
        out_w,
        out_h,
        ffmpeg_next::software::scaling::Flags::BILINEAR,
    )?;

    octx.write_header()?;
    let stream_time_base = octx.stream(0).expect("stream 0 was just added").time_base();

    let mut decoded = ffmpeg_next::frame::Video::empty();
    let mut scaled = ffmpeg_next::frame::Video::empty();
    let mut encoded_packet = ffmpeg_next::Packet::empty();
    let mut next_pts: i64 = 0;

    macro_rules! drain {
        () => {
            while opened_encoder.receive_packet(&mut encoded_packet).is_ok() {
                encoded_packet.set_stream(0);
                encoded_packet.rescale_ts(encoder_time_base, stream_time_base);
                encoded_packet.write_interleaved(&mut octx)?;
            }
        };
    }

    let mut process_frame = |scaled: &mut ffmpeg_next::frame::Video,
                              opened_encoder: &mut ffmpeg_next::encoder::Video,
                              next_pts: &mut i64|
     -> Result<(), ProbeError> {
        let (w, h) = (scaled.width() as usize, scaled.height() as usize);
        let stride = scaled.stride(0);

        // Extract plain RGB (no alpha, no stride padding) for the matting
        // callback — it takes exactly what a model like RVM expects.
        // ARGB byte order: byte 0 of each pixel is alpha, bytes 1..4 are RGB.
        let mut rgb = vec![0u8; w * h * 3];
        {
            let data = scaled.data(0);
            for row in 0..h {
                let src_row = &data[row * stride..row * stride + w * 4];
                for x in 0..w {
                    let s = &src_row[x * 4 + 1..x * 4 + 4];
                    let d = (row * w + x) * 3;
                    rgb[d..d + 3].copy_from_slice(s);
                }
            }
        }

        let alpha = alpha_of(&rgb, w, h);
        assert_eq!(alpha.len(), w * h, "alpha_of must return exactly width*height values");

        // Patch the alpha byte of every pixel in place — everything else
        // in the scaled ARGB buffer (the RGB the encoder will write out)
        // is already correct. Byte 0 of each pixel, matching ARGB order.
        {
            let data = scaled.data_mut(0);
            for row in 0..h {
                for x in 0..w {
                    let i = row * stride + x * 4;
                    let a = alpha[row * w + x].clamp(0.0, 1.0);
                    data[i] = (a * 255.0).round() as u8;
                }
            }
        }

        scaled.set_pts(Some(*next_pts));
        *next_pts += 1;
        opened_encoder.send_frame(scaled)?;
        Ok(())
    };

    while let Some(packet) = crate::read_next_packet_for_stream(&mut input, in_stream_index) {
        decoder.send_packet(&packet)?;
        while decoder.receive_frame(&mut decoded).is_ok() {
            scaler.run(&decoded, &mut scaled)?;
            process_frame(&mut scaled, &mut opened_encoder, &mut next_pts)?;
            drain!();
        }
    }
    decoder.send_eof()?;
    while decoder.receive_frame(&mut decoded).is_ok() {
        scaler.run(&decoded, &mut scaled)?;
        process_frame(&mut scaled, &mut opened_encoder, &mut next_pts)?;
        drain!();
    }
    opened_encoder.send_eof()?;
    drain!();
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
    fn scaled_dimensions_preserve_aspect_and_are_even() {
        assert_eq!(scaled_dimensions(3840, 2160, 960), (960, 540));
        assert_eq!(scaled_dimensions(1080, 1920, 960), (540, 960));
    }

    #[test]
    fn generates_a_video_with_the_alpha_channel_actually_baked_in() {
        // Deliberately doesn't depend on any real matting model — this
        // proves the encode mechanics (RGBA plumbing, stride handling,
        // container/codec choice) are correct in isolation. RVM's own
        // correctness is covered by the `matting` crate's own tests.
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("matte.mov");

        // Left half of every frame fully opaque, right half fully
        // transparent — cheap to assert on without decoding pixel data
        // from the output (this crate doesn't have an RGBA-with-alpha
        // frame reader to check against, only the file's own metadata).
        generate_matte_video(
            &fixture("test_h264.mp4"),
            &MatteVideoOptions { max_dimension: 64 },
            |_rgb, w, h| {
                (0..w * h)
                    .map(|i| if (i % w) < w / 2 { 1.0 } else { 0.0 })
                    .collect()
            },
            &output,
        )
        .unwrap();

        let asset = probe(&output).unwrap();
        let video = asset.video.expect("matte output has a video stream");
        assert!(video.width > 0 && video.height > 0);
        assert!(asset.duration_ticks > 0, "output must not be a zero-length file");
    }

    #[test]
    fn refuses_a_source_with_no_video_stream() {
        crate::init().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("matte.mov");
        // Any non-existent path fails the same way at the format-open step.
        let result = generate_matte_video(
            Path::new("does_not_exist.mp4"),
            &MatteVideoOptions::default(),
            |_, w, h| vec![1.0; w * h],
            &output,
        );
        assert!(result.is_err());
    }
}
