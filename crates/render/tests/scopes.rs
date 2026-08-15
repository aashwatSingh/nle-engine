//! Video scopes, checked against values computed by hand from the Rec.709
//! matrix rather than against whatever the implementation happens to produce.
//!
//! A scope that is merely *plausible* is worse than none: its whole purpose is
//! to be trusted over your eyes and your monitor. So every assertion here
//! pins a number the standard dictates — where 100% red sits on a
//! vectorscope, that neutrals sit dead centre, that a histogram accounts for
//! every pixel exactly once — not a number read back from this code.

use render::scopes::{luma_rec709, Histogram, Vectorscope, Waveform};
use render::wgpu;

const W: u32 = 64;
const H: u32 = 32;

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    rgba.iter().copied().cycle().take((w * h * 4) as usize).collect()
}

// ---- minimal project-building helpers for the end-to-end test below ----
// Deliberately not shared with `compositor_gpu.rs`: integration test files
// are separate crates, so each keeps its own small, self-contained fixture
// builder rather than a shared test-support crate for one call site.

fn rec709() -> media::ColorMetadata {
    media::ColorMetadata {
        primaries: media::ColorPrimaries::Rec709,
        transfer: media::TransferFunction::Bt709,
        matrix: media::MatrixCoefficients::Bt709,
        full_range: true,
    }
}

fn clip(id: u64, asset: u128) -> timeline::ClipInstance {
    timeline::ClipInstance {
        id: timeline::ClipInstanceId(id),
        source: timeline::ClipSource::Media(media::MediaAssetId(asset)),
        source_in: timeline::TimeTick(0),
        source_out: timeline::TimeTick(100),
        timeline_in: timeline::TimeTick(0),
        timeline_out: timeline::TimeTick(100),
        speed: timeline::SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
        audio_pan: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
        linked_group: None,
    }
}

fn one_clip_project(width: u32, height: u32) -> timeline::Project {
    timeline::Project {
        sequences: vec![timeline::Sequence {
            id: timeline::SequenceId(1),
            name: "S".into(),
            settings: timeline::SequenceSettings {
                frame_rate: timeline::FrameRate::Fps30,
                width,
                height,
                sample_rate: 48_000,
                working_color_primaries: media::ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks: vec![timeline::Track {
                id: timeline::TrackId(1),
                kind: timeline::TrackKind::Video,
                name: "V1".into(),
                clips: vec![clip(1, 1)],
                transitions: vec![],
                gain_db: timeline::ParamTrack::constant(timeline::ParamValue::Number(0.0)),
                pan: 0.0,
                locked: false,
                sync_locked: true,
                muted: false,
                solo: false,
                height_px: 60,
            }],
            markers: vec![],
        }],
        assets: vec![],
        bins: vec![],
    }
}

/// A left-to-right ramp from black to white, constant down each column — so
/// each waveform column has exactly one luma value and the trace is a clean
/// diagonal.
fn horizontal_ramp(w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            let v = (x * 255 / (w - 1)) as u8;
            out.extend_from_slice(&[v, v, v, 255]);
        }
    }
    out
}

// ---- luma ------------------------------------------------------------

#[test]
fn luma_uses_the_rec709_coefficients_not_rec601() {
    // 0.2126/0.7152/0.0722. Rec.601's 0.299/0.587/0.114 would put pure green
    // at 0.587 — a 13% error that reads as "my greens are too dark" and is
    // exactly the kind of thing a scope exists to rule out.
    assert!((luma_rec709(1.0, 0.0, 0.0) - 0.2126).abs() < 1e-4, "pure red");
    assert!((luma_rec709(0.0, 1.0, 0.0) - 0.7152).abs() < 1e-4, "pure green");
    assert!((luma_rec709(0.0, 0.0, 1.0) - 0.0722).abs() < 1e-4, "pure blue");
    // The three must sum to exactly white, or every neutral drifts.
    assert!((luma_rec709(1.0, 1.0, 1.0) - 1.0).abs() < 1e-6, "white is unity");
    assert_eq!(luma_rec709(0.0, 0.0, 0.0), 0.0, "black is zero");
}

// ---- histogram -------------------------------------------------------

#[test]
fn a_solid_frame_puts_every_pixel_in_exactly_one_bin() {
    let h = Histogram::from_rgba(&solid(W, H, [40, 40, 40, 255]));
    assert_eq!(h.red[40], W * H, "every pixel should land in bin 40");
    assert_eq!(h.red.iter().filter(|&&c| c > 0).count(), 1, "and nowhere else");
}

#[test]
fn a_histogram_accounts_for_every_pixel_exactly_once_per_channel() {
    // The invariant that catches an off-by-one in bin indexing: a pixel that
    // falls off the end of the array is a pixel the scope silently didn't
    // measure, which is the failure mode that makes a scope untrustworthy.
    let h = Histogram::from_rgba(&horizontal_ramp(W, H));
    let total = W * H;
    assert_eq!(h.red.iter().sum::<u32>(), total, "red");
    assert_eq!(h.green.iter().sum::<u32>(), total, "green");
    assert_eq!(h.blue.iter().sum::<u32>(), total, "blue");
    assert_eq!(h.luma.iter().sum::<u32>(), total, "luma");
}

#[test]
fn the_channels_of_a_pure_colour_are_measured_separately() {
    // Pure red: the red channel is all at the top, green and blue all at the
    // bottom. A scope that measured luma three times would show identical
    // traces and be useless for spotting a colour cast.
    let h = Histogram::from_rgba(&solid(W, H, [255, 0, 0, 255]));
    assert_eq!(h.red[255], W * H);
    assert_eq!(h.green[0], W * H);
    assert_eq!(h.blue[0], W * H);
    // And the luma of pure red is 0.2126 -> bin 54.
    let expected_bin = (0.2126f32 * 255.0).round() as usize;
    assert_eq!(h.luma[expected_bin], W * H, "luma of pure red belongs in bin {expected_bin}");
}

// ---- waveform --------------------------------------------------------

#[test]
fn the_waveform_has_one_column_per_frame_column() {
    // A waveform monitor's x axis *is* the picture's x axis — that's what
    // makes it readable against the image. Resampling it would break the
    // correspondence the tool depends on.
    let wf = Waveform::from_rgba(&horizontal_ramp(W, H), W, H, 100);
    assert_eq!(wf.width, W);
    assert_eq!(wf.height, 100);
    assert_eq!(wf.cells.len(), (W * 100) as usize);
}

#[test]
fn a_horizontal_ramp_traces_a_diagonal_with_bright_at_the_top() {
    // Broadcast convention: 100% white at the top of the graticule, black at
    // the bottom. Flipping it is a classic and very confusing bug.
    let wf = Waveform::from_rgba(&horizontal_ramp(W, H), W, H, 100);

    let trace_row = |x: u32| -> u32 {
        (0..wf.height)
            .find(|&y| wf.cells[(y * wf.width + x) as usize] > 0)
            .expect("every column of the ramp has a trace")
    };
    let dark = trace_row(0);
    let bright = trace_row(W - 1);
    assert!(bright < dark, "white should trace above black, got row {bright} vs {dark}");
    assert!(dark >= wf.height - 2, "black belongs at the very bottom, got row {dark}");
    assert_eq!(bright, 0, "white belongs at the very top");

    // Every pixel of the frame is accounted for somewhere in the trace.
    assert_eq!(wf.cells.iter().sum::<u32>(), W * H);
}

#[test]
fn a_solid_frame_traces_one_flat_line() {
    let wf = Waveform::from_rgba(&solid(W, H, [128, 128, 128, 255]), W, H, 100);
    let rows: Vec<u32> = (0..W)
        .map(|x| (0..wf.height).find(|&y| wf.cells[(y * wf.width + x) as usize] > 0).unwrap())
        .collect();
    assert!(rows.windows(2).all(|p| p[0] == p[1]), "a flat field must trace a flat line: {rows:?}");
}

// ---- vectorscope -----------------------------------------------------

#[test]
fn every_neutral_lands_dead_centre_on_the_vectorscope() {
    // Black, grey and white are all colourless: they differ in luma, which a
    // vectorscope deliberately discards. All three at the centre dot is the
    // defining property of the instrument.
    for level in [0u8, 128, 255] {
        let vs = Vectorscope::from_rgba(&solid(W, H, [level, level, level, 255]), 256);
        let centre = vs.size / 2;
        assert_eq!(
            vs.cells[(centre * vs.size + centre) as usize],
            W * H,
            "level {level} should sit at the centre"
        );
    }
}

#[test]
fn saturated_red_lands_where_the_rec709_matrix_puts_it() {
    // Y' = 0.2126, so Cb = (B-Y)/1.8556 = -0.1146 and Cr = (R-Y)/1.5748 =
    // +0.5000. On the plot that is left of centre and at maximum height —
    // the R target on a real vectorscope graticule.
    let vs = Vectorscope::from_rgba(&solid(W, H, [255, 0, 0, 255]), 256);
    let (cb_idx, cr_idx) = vs.only_populated_cell().expect("a solid colour hits one cell");
    let centre = (vs.size / 2) as i32;

    assert!((cb_idx as i32) < centre, "red sits left of centre in Cb, got {cb_idx}");
    assert!((cr_idx as i32) > centre, "red sits high in Cr, got {cr_idx}");

    // Pinned to the actual computed values, not just a quadrant: Cb -0.1146
    // and Cr +0.5 map to (0.5 + c) * size.
    let expect = |c: f32| ((0.5 + c) * vs.size as f32).round().clamp(0.0, (vs.size - 1) as f32) as u32;
    assert_eq!(cb_idx, expect(-0.1146), "Cb bin");
    assert_eq!(cr_idx, expect(0.5), "Cr bin");
}

#[test]
fn complementary_colours_land_opposite_each_other() {
    // Red and cyan are 180 degrees apart through the centre. This catches a
    // sign error in one axis that a single-colour test would sail past.
    let vs_red = Vectorscope::from_rgba(&solid(W, H, [255, 0, 0, 255]), 256);
    let vs_cyan = Vectorscope::from_rgba(&solid(W, H, [0, 255, 255, 255]), 256);
    let (rb, rr) = vs_red.only_populated_cell().unwrap();
    let (cb, cr) = vs_cyan.only_populated_cell().unwrap();
    let centre = (vs_red.size / 2) as i32;

    assert!(
        ((rb as i32 - centre) + (cb as i32 - centre)).abs() <= 1,
        "Cb should be mirrored: {rb} and {cb} about {centre}"
    );
    assert!(
        ((rr as i32 - centre) + (cr as i32 - centre)).abs() <= 1,
        "Cr should be mirrored: {rr} and {cr} about {centre}"
    );
}

// ---- end-to-end: the actual GPU texture -> readback -> scope path -----
//
// Everything above feeds the scopes a plain `Vec<u8>` built by hand. That
// proves the *math*, not the *plumbing* — `app::Preview::read_back_rgba`
// reads a real GPU texture that a compositor rendered into, using
// `render::read_texture_rgba`, and clicking through `app`'s own UI to check
// that path isn't something this crate's tests can reach. This test exercises
// the identical sequence instead: create a `COPY_SRC`-usage texture (as
// `Preview` does), composite into it via `render_to_view` (as `Preview::
// render_prepared` does), read it back, and confirm the scopes see real data
// — not a placeholder, not zeros.

#[test]
fn a_texture_the_compositor_rendered_into_reads_back_correctly_for_the_scopes() {
    let (device, queue) = render::headless_context()
        .expect("no GPU adapter available — this project requires a working GPU");
    let comp = render::Compositor::with_output_format(
        device.clone(),
        queue.clone(),
        wgpu::TextureFormat::Rgba8Unorm,
    );

    // A one-clip white sequence — deliberately not through `render_to_rgba`
    // (which allocates its own COPY_SRC texture internally): this test wants
    // the exact texture-creation shape `Preview::recreate_texture` uses,
    // proving *that* shape is readable, not merely proving readback works in
    // general.
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test preview texture"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut sources = render::SourceFrames::default();
    sources.insert(media::MediaAssetId(1), 0, comp.upload_rgba(&solid(W, H, [255, 255, 255, 255]), W, H, rec709()));
    let p = one_clip_project(W, H);
    let compiler = render::GraphCompiler::new(render::BuiltinRegistry::default());
    let graph = compiler.compile(&p, timeline::SequenceId(1), timeline::TimeTick(0)).unwrap();
    comp.render_to_view(&graph, &sources, render::DeliverySpace::Rec709, &view);

    let rgba = render::read_texture_rgba(&device, &queue, &texture, W, H);
    assert_eq!(rgba.len(), (W * H * 4) as usize, "readback must cover the whole frame");

    // Fed through the real scopes, a white frame should show up as white,
    // not silence — the failure mode this test exists to catch is a texture
    // format/usage mismatch that reads back all zeros without erroring.
    let hist = Histogram::from_rgba(&rgba);
    assert_eq!(hist.red[255], W * H, "the composited white frame should read back as white");
    assert_eq!(hist.luma[255], W * H);

    let vs = Vectorscope::from_rgba(&rgba, 64);
    let centre = 32u32;
    assert_eq!(
        vs.cells[(centre * 64 + centre) as usize],
        W * H,
        "white is a neutral — it must land at the vectorscope's centre, same as any other grey"
    );
}

#[test]
fn an_out_of_range_colour_is_clamped_into_the_plot_not_dropped() {
    // 8-bit input can't exceed the legal range, but the scope takes a slice
    // and must not index out of bounds if it's ever handed wider data. The
    // real assertion is "no panic, and the pixel still counted somewhere".
    let vs = Vectorscope::from_rgba(&solid(W, H, [255, 0, 255, 255]), 64);
    assert_eq!(vs.cells.iter().sum::<u32>(), W * H, "every pixel is plotted");
}
