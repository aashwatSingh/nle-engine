//! GPU compositor tests (M4d) and the M4 acceptance check (M4e).
//!
//! These run against a real headless GPU device. They deliberately do NOT
//! skip when no adapter is found: this application cannot function without a
//! GPU, so "no adapter" is a real failure worth surfacing loudly rather than
//! a reason to quietly report success.
//!
//! Everything here goes through the crate's public API only — building a
//! `Project`, compiling it, and rendering it — so the tests exercise the same
//! path a caller would.

use media::{ColorMetadata, ColorPrimaries, MatrixCoefficients, MediaAssetId, TransferFunction};
use render::wgpu;
use render::{
    chroma_key, color_correction, crop, gaussian_blur, headless_context, mask, transform,
    BuiltinRegistry, Compositor, DeliverySpace, GraphCompiler, SourceFrames,
};
use std::collections::BTreeMap;
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, EffectInstance, EffectInstanceId, FrameRate,
    ParamTrack, ParamValue, Project, Sequence, SequenceId, SequenceSettings, SpeedCurve, TimeTick,
    Track, TrackId, TrackKind,
};

const SEQ_W: u32 = 64;
const SEQ_H: u32 = 64;

/// All tests render at tick 0 and upload their sources at source-pts 0.
/// The graph maps timeline tick -> source pts (`source_in + speed_delta`), so
/// rendering at a nonzero tick would request a pts these uploads don't have
/// and every layer would correctly report as a missing source. Keeping both
/// at 0 keeps the tests about compositing rather than about pts arithmetic
/// (which `graph.rs`'s own tests already cover).
const RENDER_TICK: i64 = 0;

fn compositor() -> Compositor {
    let (device, queue) = headless_context()
        .expect("no GPU adapter available — this project requires a working GPU, so this is a real failure, not a skip");
    Compositor::new(device, queue)
}

fn rec709() -> ColorMetadata {
    ColorMetadata {
        primaries: ColorPrimaries::Rec709,
        transfer: TransferFunction::Bt709,
        matrix: MatrixCoefficients::Bt709,
        full_range: true,
    }
}

fn solid(w: u32, h: u32, rgba: [u8; 4]) -> Vec<u8> {
    rgba.iter().copied().cycle().take((w * h * 4) as usize).collect()
}

/// Left half `a`, right half `b` — an asymmetric pattern so rotation
/// direction is observable.
fn split_horizontal(w: u32, h: u32, a: [u8; 4], b: [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for _ in 0..h {
        for x in 0..w {
            out.extend_from_slice(if x < w / 2 { &a } else { &b });
        }
    }
    out
}

fn clip(id: u64, asset: u128, tin: i64, tout: i64) -> ClipInstance {
    ClipInstance {
        id: ClipInstanceId(id),
        source: ClipSource::Media(MediaAssetId(asset)),
        source_in: TimeTick(0),
        source_out: TimeTick(tout - tin),
        timeline_in: TimeTick(tin),
        timeline_out: TimeTick(tout),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects: vec![],
        audio_gain_db: ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    }
}

fn effect_fx(id: u64, type_id: &str, params: Vec<(&str, ParamValue)>) -> EffectInstance {
    let mut map = BTreeMap::new();
    for (name, value) in params {
        map.insert(name.to_string(), ParamTrack::constant(value));
    }
    EffectInstance { id: EffectInstanceId(id), effect_type: type_id.to_string(), enabled: true, params: map }
}

fn transform_fx(params: Vec<(&str, ParamValue)>) -> EffectInstance {
    effect_fx(1, transform::TYPE_ID, params)
}

/// A checkerboard so blur has real high-frequency content to smooth out.
fn checkerboard(w: u32, h: u32, cell: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let on = ((x / cell) + (y / cell)).is_multiple_of(2);
            let v = if on { 255 } else { 0 };
            out.extend_from_slice(&[v, v, v, 255]);
        }
    }
    out
}

fn video_track(id: u64, clips: Vec<ClipInstance>) -> Track {
    Track {
        id: TrackId(id),
        kind: TrackKind::Video,
        name: format!("V{id}"),
        clips,
        transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    }
}

fn project_with(tracks: Vec<Track>) -> Project {
    Project {
        sequences: vec![Sequence {
            id: SequenceId(1),
            name: "S1".into(),
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: SEQ_W,
                height: SEQ_H,
                sample_rate: 48000,
                working_color_primaries: ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }],
        assets: vec![],
        bins: vec![],
    }
}

fn render(
    comp: &Compositor,
    project: &Project,
    sources: &SourceFrames,
) -> (render::RenderedFrame, render::CompositeStats) {
    let compiler = GraphCompiler::new(BuiltinRegistry::default());
    let graph = compiler
        .compile(project, SequenceId(1), TimeTick(RENDER_TICK))
        .expect("sequence exists");
    comp.render_to_rgba(&graph, sources, DeliverySpace::Rec709)
}

fn assert_channel_near(actual: u8, expected: u8, tolerance: u8, what: &str) {
    let diff = actual.abs_diff(expected);
    assert!(
        diff <= tolerance,
        "{what}: expected ~{expected}, got {actual} (diff {diff} > tolerance {tolerance})"
    );
}

#[test]
fn empty_sequence_renders_fully_transparent() {
    let comp = compositor();
    let p = project_with(vec![video_track(1, vec![])]);
    let (frame, stats) = render(&comp, &p, &SourceFrames::default());
    assert_eq!(stats.layers_drawn, 0);
    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2), [0, 0, 0, 0], "no layers should leave alpha 0");
}

#[test]
fn single_opaque_clip_fills_the_frame() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()),
    );
    let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
    let (frame, stats) = render(&comp, &p, &sources);

    assert_eq!(stats.layers_drawn, 1);
    for (x, y) in [(0, 0), (SEQ_W - 1, 0), (0, SEQ_H - 1), (SEQ_W - 1, SEQ_H - 1), (SEQ_W / 2, SEQ_H / 2)] {
        let px = frame.pixel(x, y);
        assert_channel_near(px[0], 255, 2, &format!("white at ({x},{y}) R"));
        assert_eq!(px[3], 255, "white should be fully opaque at ({x},{y})");
    }
}

#[test]
fn rec709_source_round_trips_through_the_gpu_unchanged() {
    // The GPU mirror of color.rs's `rec709_identity_conversion_is_a_true_no_op`.
    // A 709-tagged source delivered as 709 with no colour work must come back
    // out at the same code value. This is the test that catches the shader's
    // transfer functions or gamut matrix drifting from the CPU reference —
    // a mismatch shows up as a global contrast or tint shift that's very hard
    // to spot by eye but obvious here.
    let comp = compositor();
    for level in [0u8, 32, 64, 128, 192, 255] {
        let mut sources = SourceFrames::default();
        sources.insert(
            MediaAssetId(1),
            0,
            comp.upload_rgba(&solid(SEQ_W, SEQ_H, [level, level, level, 255]), SEQ_W, SEQ_H, rec709()),
        );
        let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
        let (frame, _) = render(&comp, &p, &sources);
        let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
        // Tolerance of 2/255 covers 16-bit-float working-space rounding plus
        // the 8-bit quantisation on the way in and out.
        assert_channel_near(px[0], level, 2, &format!("709 round-trip of {level}"));
    }
}

#[test]
fn wide_gamut_source_is_actually_converted_on_the_gpu() {
    // A Rec.2020-tagged source must NOT render identically to the same code
    // values tagged Rec.709 — if it does, the gamut matrix isn't reaching the
    // shader and wide-gamut footage would silently render over-saturated.
    let comp = compositor();
    let pixels = solid(SEQ_W, SEQ_H, [40, 200, 90, 255]);

    let mut as_709 = SourceFrames::default();
    as_709.insert(MediaAssetId(1), 0, comp.upload_rgba(&pixels, SEQ_W, SEQ_H, rec709()));

    let mut as_2020 = SourceFrames::default();
    as_2020.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(
            &pixels,
            SEQ_W,
            SEQ_H,
            ColorMetadata {
                primaries: ColorPrimaries::Rec2020,
                transfer: TransferFunction::Bt709,
                matrix: MatrixCoefficients::Bt2020Ncl,
                full_range: true,
            },
        ),
    );

    let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
    let (out_709, _) = render(&comp, &p, &as_709);
    let (out_2020, _) = render(&comp, &p, &as_2020);

    let a = out_709.pixel(SEQ_W / 2, SEQ_H / 2);
    let b = out_2020.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_ne!(a, b, "Rec.2020 and Rec.709 tags must not produce identical output ({a:?} vs {b:?})");
}

#[test]
fn opacity_blends_the_top_track_over_the_bottom() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    // Bottom: black. Top: white at 50% opacity.
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );
    sources.insert(
        MediaAssetId(2),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()),
    );

    let mut top = clip(2, 2, 0, 100);
    top.effects = vec![transform_fx(vec![(transform::OPACITY, ParamValue::Number(0.5))])];
    let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)]), video_track(2, vec![top])]);
    let (frame, stats) = render(&comp, &p, &sources);

    assert_eq!(stats.layers_drawn, 2);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    // Blending happens in LINEAR light: 50% of linear-white over linear-black
    // is linear 0.5, which re-encodes through the Rec.709 OETF to ~0.7055
    // (~180/255) — NOT 128. Getting 128 here would mean blending happened in
    // encoded space, which is the classic "why do my dissolves look muddy"
    // bug this working-space design exists to prevent.
    let expected = (render::color::linear_to_rec709(0.5) * 255.0).round() as u8;
    assert_channel_near(px[0], expected, 3, "50% linear blend");
    assert!(px[0] > 150, "a linear-light 50% blend must be much brighter than 128, got {}", px[0]);
}

#[test]
fn opaque_top_track_fully_covers_the_bottom() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );
    sources.insert(
        MediaAssetId(2),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 0, 255, 255]), SEQ_W, SEQ_H, rec709()),
    );
    let p = project_with(vec![
        video_track(1, vec![clip(1, 1, 0, 100)]),
        video_track(2, vec![clip(2, 2, 0, 100)]),
    ]);
    let (frame, _) = render(&comp, &p, &sources);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    assert!(px[2] > 200 && px[0] < 60, "top (blue) track should win, got {px:?}");
}

#[test]
fn position_offset_moves_the_image() {
    let comp = compositor();
    // A 32x32 white square in a 64x64 sequence, shifted right by 16px.
    let src = 32u32;
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(src, src, [255, 255, 255, 255]), src, src, rec709()),
    );
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![transform_fx(vec![(transform::POSITION, ParamValue::Vec2(16.0, 0.0))])];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    // Centred it would span x in [16,48). Shifted +16 it spans [32,64).
    assert_eq!(frame.pixel(20, 32)[3], 0, "x=20 should now be outside the shifted square");
    assert_eq!(frame.pixel(40, 32)[3], 255, "x=40 should be inside the shifted square");
}

#[test]
fn scale_enlarges_the_image() {
    let comp = compositor();
    let src = 16u32;
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(src, src, [255, 255, 255, 255]), src, src, rec709()),
    );

    let unscaled = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
    let (before, _) = render(&comp, &unscaled, &sources);

    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![transform_fx(vec![(transform::SCALE, ParamValue::Vec2(2.0, 2.0))])];
    let scaled = project_with(vec![video_track(1, vec![c])]);
    let (after, _) = render(&comp, &scaled, &sources);

    let covered = |f: &render::RenderedFrame| {
        (0..SEQ_W * SEQ_H).filter(|i| f.pixel(i % SEQ_W, i / SEQ_W)[3] > 128).count()
    };
    let (a, b) = (covered(&before), covered(&after));
    assert!(b > a * 3, "2x scale should cover ~4x the area: {a} -> {b}");
}

#[test]
fn positive_rotation_reads_as_clockwise() {
    let comp = compositor();
    // Left half red, right half blue. Rotated +90 degrees clockwise, the
    // left (red) half should end up on TOP.
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(
            &split_horizontal(SEQ_W, SEQ_H, [255, 0, 0, 255], [0, 0, 255, 255]),
            SEQ_W,
            SEQ_H,
            rec709(),
        ),
    );
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![transform_fx(vec![(transform::ROTATION, ParamValue::Number(90.0))])];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    let top = frame.pixel(SEQ_W / 2, 8);
    let bottom = frame.pixel(SEQ_W / 2, SEQ_H - 8);
    assert!(top[0] > 150 && top[2] < 100, "after +90deg the red (left) half should be on top, got {top:?}");
    assert!(bottom[2] > 150 && bottom[0] < 100, "and blue on the bottom, got {bottom:?}");
}

#[test]
fn source_lookup_presents_the_latest_frame_at_or_before_the_request() {
    // The rule real playback depends on: the graph asks for an exact source
    // tick, and the decoder only has the PTS values that exist in the file.
    let comp = compositor();
    let mut sources = SourceFrames::default();
    let red = solid(4, 4, [255, 0, 0, 255]);
    let green = solid(4, 4, [0, 255, 0, 255]);
    sources.insert(MediaAssetId(1), 1000, comp.upload_rgba(&red, 4, 4, rec709()));
    sources.insert(MediaAssetId(1), 2000, comp.upload_rgba(&green, 4, 4, rec709()));

    assert!(sources.get(MediaAssetId(1), 1000).is_some(), "exact hit");
    assert!(sources.get(MediaAssetId(1), 1500).is_some(), "between frames should resolve, not miss");
    assert!(sources.get(MediaAssetId(1), 99_999).is_some(), "past the last frame holds the last frame");
    assert!(sources.get(MediaAssetId(1), 0).is_some(), "before the first frame falls back to the first");
    assert!(sources.get(MediaAssetId(2), 1000).is_none(), "an asset with no frames at all is a real miss");

    assert_eq!(sources.len(), 2);
    // Retaining from 2000 should drop nothing yet: the frame at 1000 is still
    // the one presented for ticks in [1000, 2000).
    sources.retain_from(MediaAssetId(1), 2000);
    assert_eq!(sources.len(), 2, "must keep the frame currently being presented");
    sources.retain_from(MediaAssetId(1), 5000);
    assert_eq!(sources.len(), 1, "older frames beyond the playhead should be released");
}

#[test]
fn missing_source_frame_is_reported_not_crashed() {
    let comp = compositor();
    // The graph asks for asset 1 @ pts 0, but nothing is uploaded.
    let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
    let (frame, stats) = render(&comp, &p, &SourceFrames::default());
    assert_eq!(stats.layers_drawn, 0);
    assert_eq!(stats.layers_missing_source, 1, "a missing frame must be counted, not silently ignored");
    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 0);
}

#[test]
fn nested_sequence_renders_through_an_intermediate_target() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 255, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );

    let inner = Sequence {
        id: SequenceId(2),
        name: "inner".into(),
        settings: SequenceSettings {
            frame_rate: FrameRate::Fps30,
            width: SEQ_W,
            height: SEQ_H,
            sample_rate: 48000,
            working_color_primaries: ColorPrimaries::Rec709,
            drop_frame_timecode: false,
        },
        tracks: vec![video_track(10, vec![clip(100, 1, 0, 100)])],
        markers: vec![],
    };
    let mut host_clip = clip(1, 1, 0, 100);
    host_clip.source = ClipSource::NestedSequence(SequenceId(2));
    let mut p = project_with(vec![video_track(1, vec![host_clip])]);
    p.sequences.push(inner);

    let (frame, stats) = render(&comp, &p, &sources);
    assert_eq!(stats.nested_sequences_rendered, 1);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    assert!(px[1] > 200 && px[0] < 60, "nested green should reach the output, got {px:?}");
}

#[test]
fn acceptance_preview_and_export_paths_produce_identical_pixels() {
    // Spec M4 acceptance criterion. Both entry points (`render_to_view`, used
    // by preview, and `render_to_rgba`, used by export) run the exact same
    // `composite_to_working` + `deliver` core, so this asserts that the shared
    // path really is deterministic and resolution-matched output is
    // bit-identical — not merely perceptually close.
    //
    // What this does NOT claim: that a proxy-resolution preview matches a
    // full-resolution export. It can't, by construction. See
    // docs/risks.md #5 for how that tension is resolved.
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [10, 120, 200, 255]), SEQ_W, SEQ_H, rec709()),
    );
    sources.insert(
        MediaAssetId(2),
        0,
        comp.upload_rgba(
            &split_horizontal(SEQ_W, SEQ_H, [240, 30, 30, 255], [30, 240, 30, 255]),
            SEQ_W,
            SEQ_H,
            rec709(),
        ),
    );

    // A non-trivial graph: two tracks, the top one transformed and partly
    // transparent, so the comparison covers blending, transform and colour.
    let mut top = clip(2, 2, 0, 100);
    top.effects = vec![transform_fx(vec![
        (transform::OPACITY, ParamValue::Number(0.6)),
        (transform::SCALE, ParamValue::Vec2(0.75, 0.75)),
        (transform::ROTATION, ParamValue::Number(15.0)),
        (transform::POSITION, ParamValue::Vec2(4.0, -3.0)),
    ])];
    let p = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)]), video_track(2, vec![top])]);

    let compiler = GraphCompiler::new(BuiltinRegistry::default());
    let graph = compiler.compile(&p, SequenceId(1), TimeTick(RENDER_TICK)).unwrap();

    // "Export" path.
    let (export_frame, export_stats) = comp.render_to_rgba(&graph, &sources, DeliverySpace::Rec709);

    // "Preview" path: render into a caller-owned texture, exactly as a
    // swapchain image would be, then read it back so the pixels can be
    // compared at all.
    let preview_texture = comp.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("preview target"),
        size: wgpu::Extent3d { width: SEQ_W, height: SEQ_H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: render::compositor::OUTPUT_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let preview_view = preview_texture.create_view(&wgpu::TextureViewDescriptor::default());
    let preview_stats = comp.render_to_view(&graph, &sources, DeliverySpace::Rec709, &preview_view);

    assert_eq!(export_stats, preview_stats, "both paths should draw the same layers");
    assert_eq!(export_stats.layers_drawn, 2);

    let preview_rgba = read_back(&comp, &preview_texture, SEQ_W, SEQ_H);
    assert_eq!(
        preview_rgba, export_frame.rgba,
        "preview and export must be pixel-identical at matched resolution"
    );

    // Guard against the whole thing being trivially equal because nothing
    // rendered: the frame must actually contain varied content.
    let first = export_frame.rgba[0];
    assert!(
        export_frame.rgba.iter().any(|b| *b != first),
        "rendered frame is a flat colour, so the comparison above proved nothing"
    );
}

/// Copies a texture back to the CPU, stripping the 256-byte row padding the
/// GPU requires.
// --- M5: Gaussian Blur ---

#[test]
fn gaussian_blur_smooths_a_checkerboard() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    // Cells wider than the blur radius so the centre of each cell stays
    // near its original value while cell BOUNDARIES get smoothed.
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&checkerboard(SEQ_W, SEQ_H, 16), SEQ_W, SEQ_H, rec709()));

    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![effect_fx(2, gaussian_blur::TYPE_ID, vec![(gaussian_blur::RADIUS, ParamValue::Number(6.0))])];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, stats) = render(&comp, &p, &sources);
    assert_eq!(stats.layers_drawn, 1);

    // A cell boundary (e.g. x=16, the edge between the first two cells) must
    // no longer be pure black or white — the whole point of the blur.
    let boundary = frame.pixel(16, 32)[0];
    assert!(
        boundary > 20 && boundary < 235,
        "cell boundary should be smoothed to an intermediate value, got {boundary}"
    );
    // Deep inside a cell, far from any boundary, should stay close to the
    // original value — the blur must not smear the whole image uniformly.
    let cell_centre = frame.pixel(8, 8)[0];
    assert!(cell_centre > 200, "cell centre should stay close to its original white, got {cell_centre}");
}

#[test]
fn gaussian_blur_radius_zero_matches_the_fast_path() {
    // Regression test for the multi-stage plumbing itself: prepare + a
    // skipped (radius=0) blur pass + place should be numerically identical
    // to the single-pass fast path, since it's the same conversion math
    // split across two draws instead of one.
    let comp = compositor();
    let pixels = solid(SEQ_W, SEQ_H, [180, 90, 40, 255]);

    let mut sources_plain = SourceFrames::default();
    sources_plain.insert(MediaAssetId(1), 0, comp.upload_rgba(&pixels, SEQ_W, SEQ_H, rec709()));
    let plain = project_with(vec![video_track(1, vec![clip(1, 1, 0, 100)])]);
    let (plain_frame, _) = render(&comp, &plain, &sources_plain);

    let mut sources_blur = SourceFrames::default();
    sources_blur.insert(MediaAssetId(1), 0, comp.upload_rgba(&pixels, SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![effect_fx(2, gaussian_blur::TYPE_ID, vec![(gaussian_blur::RADIUS, ParamValue::Number(0.0))])];
    let with_zero_blur = project_with(vec![video_track(1, vec![c])]);
    let (blurred_frame, _) = render(&comp, &with_zero_blur, &sources_blur);

    assert_eq!(
        plain_frame.pixel(SEQ_W / 2, SEQ_H / 2),
        blurred_frame.pixel(SEQ_W / 2, SEQ_H / 2),
        "a zero-radius blur must not change the image"
    );
}

// --- M5: Color Correction ---

#[test]
fn color_correction_exposure_brightens_and_darkens() {
    let comp = compositor();
    let base = [80u8, 80, 80, 255];

    let render_with_exposure = |ev: f64| {
        let mut sources = SourceFrames::default();
        sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, base), SEQ_W, SEQ_H, rec709()));
        let mut c = clip(1, 1, 0, 100);
        c.effects = vec![effect_fx(2, color_correction::TYPE_ID, vec![(color_correction::EXPOSURE, ParamValue::Number(ev))])];
        let p = project_with(vec![video_track(1, vec![c])]);
        render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2)[0]
    };

    let neutral = render_with_exposure(0.0);
    let brighter = render_with_exposure(1.0);
    let darker = render_with_exposure(-1.0);
    assert!(brighter > neutral, "+1 stop should brighten: {brighter} vs {neutral}");
    assert!(darker < neutral, "-1 stop should darken: {darker} vs {neutral}");
}

#[test]
fn color_correction_saturation_zero_desaturates_to_grey() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [220, 40, 40, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![effect_fx(2, color_correction::TYPE_ID, vec![(color_correction::SATURATION, ParamValue::Number(0.0))])];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    let max_diff = px[0].abs_diff(px[1]).max(px[1].abs_diff(px[2])).max(px[0].abs_diff(px[2]));
    assert!(max_diff <= 2, "saturation=0 should produce a neutral grey, got {px:?}");
}

#[test]
fn color_correction_saturation_two_increases_difference_from_grey() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [180, 100, 100, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![effect_fx(2, color_correction::TYPE_ID, vec![(color_correction::SATURATION, ParamValue::Number(2.0))])];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    assert!(px[0].abs_diff(px[1]) > 80, "saturation=2 should exaggerate the red/green gap, got {px:?}");
}

#[test]
fn color_correction_temperature_shifts_red_blue_balance() {
    let comp = compositor();
    let render_with_temp = |t: f64| {
        let mut sources = SourceFrames::default();
        sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [128, 128, 128, 255]), SEQ_W, SEQ_H, rec709()));
        let mut c = clip(1, 1, 0, 100);
        c.effects = vec![effect_fx(2, color_correction::TYPE_ID, vec![(color_correction::TEMPERATURE, ParamValue::Number(t))])];
        let p = project_with(vec![video_track(1, vec![c])]);
        render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2)
    };
    let warm = render_with_temp(1.0);
    let cool = render_with_temp(-1.0);
    assert!(warm[0] > warm[2], "warm should push red above blue, got {warm:?}");
    assert!(cool[2] > cool[0], "cool should push blue above red, got {cool:?}");
}

#[test]
fn color_correction_contrast_increases_spread_between_tones() {
    let comp = compositor();
    let render_patch = |contrast: f64, value: u8| {
        let mut sources = SourceFrames::default();
        sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [value, value, value, 255]), SEQ_W, SEQ_H, rec709()));
        let mut c = clip(1, 1, 0, 100);
        c.effects = vec![effect_fx(2, color_correction::TYPE_ID, vec![(color_correction::CONTRAST, ParamValue::Number(contrast))])];
        let p = project_with(vec![video_track(1, vec![c])]);
        render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2)[0]
    };
    let dark_neutral = render_patch(0.0, 40);
    let light_neutral = render_patch(0.0, 220);
    let dark_high = render_patch(0.8, 40);
    let light_high = render_patch(0.8, 220);
    assert!(
        light_high - dark_high > light_neutral - dark_neutral,
        "higher contrast should widen the gap between a dark and light tone: neutral gap {} vs high-contrast gap {}",
        light_neutral - dark_neutral,
        light_high - dark_high
    );
}

// --- M5: Crop ---

#[test]
fn crop_makes_edges_transparent_and_preserves_the_centre() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [200, 200, 200, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![effect_fx(
        2,
        crop::TYPE_ID,
        vec![
            (crop::LEFT, ParamValue::Number(0.25)),
            (crop::RIGHT, ParamValue::Number(0.25)),
            (crop::TOP, ParamValue::Number(0.25)),
            (crop::BOTTOM, ParamValue::Number(0.25)),
        ],
    )];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    assert_eq!(frame.pixel(2, 2)[3], 0, "corner should be cropped away");
    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 255, "centre should survive a 25%-per-edge crop");
    assert_channel_near(frame.pixel(SEQ_W / 2, SEQ_H / 2)[0], 200, 2, "centre colour preserved");
}

// --- Chroma key ---

fn chroma_key_fx(key: [f32; 4], similarity: f64, smoothness: f64, spill: f64) -> EffectInstance {
    effect_fx(
        4,
        chroma_key::TYPE_ID,
        vec![
            (chroma_key::KEY_COLOR, ParamValue::Color(key)),
            (chroma_key::SIMILARITY, ParamValue::Number(similarity)),
            (chroma_key::SMOOTHNESS, ParamValue::Number(smoothness)),
            (chroma_key::SPILL_SUPPRESSION, ParamValue::Number(spill)),
        ],
    )
}

const GREEN: [f32; 4] = [0.0, 1.0, 0.0, 1.0];

#[test]
fn a_pixel_matching_the_key_colour_becomes_transparent() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 255, 0, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![chroma_key_fx(GREEN, 0.4, 0.1, 0.0)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let px = render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_eq!(px[3], 0, "pure key-colour green should key out to fully transparent, got alpha {}", px[3]);
}

#[test]
fn a_pixel_far_from_the_key_colour_stays_fully_opaque() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    // Solid red — as far from key green as a fully saturated colour gets.
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 0, 0, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![chroma_key_fx(GREEN, 0.4, 0.1, 0.0)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let px = render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_eq!(px[3], 255, "a colour far from the key should stay fully opaque, got alpha {}", px[3]);
}

#[test]
fn alpha_ramps_monotonically_with_distance_from_the_key_colour() {
    // A left-to-right gradient from pure key-green to pure red sweeps chroma
    // distance from 0 up. Alpha must never decrease along it — a dip would be
    // a visible ring artefact around the edge of anything keyed.
    let comp = compositor();
    let w = 64u32;
    let mut ramp = Vec::with_capacity((w * SEQ_H * 4) as usize);
    for _ in 0..SEQ_H {
        for x in 0..w {
            let t = x as f32 / (w - 1) as f32;
            let r = (t * 255.0).round() as u8;
            let g = ((1.0 - t) * 255.0).round() as u8;
            ramp.extend_from_slice(&[r, g, 0, 255]);
        }
    }
    // Sequence sized to the ramp so every source column maps to one output
    // column (no resampling to reason about).
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&ramp, w, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![chroma_key_fx(GREEN, 0.15, 0.35, 0.0)];
    let tracks = vec![video_track(1, vec![c])];
    let p = Project {
        sequences: vec![timeline::Sequence {
            id: SequenceId(1),
            name: "S".into(),
            settings: timeline::SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: w,
                height: SEQ_H,
                sample_rate: 48_000,
                working_color_primaries: ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }],
        assets: vec![],
        bins: vec![],
    };
    let (frame, _) = render(&comp, &p, &sources);

    let alphas: Vec<u8> = (0..w).map(|x| frame.pixel(x, SEQ_H / 2)[3]).collect();
    assert_eq!(alphas[0], 0, "the key-green end must be fully transparent");
    assert_eq!(*alphas.last().unwrap(), 255, "the red end must be fully opaque");
    assert!(
        alphas.windows(2).all(|p| p[1] >= p[0]),
        "alpha must never decrease moving away from the key colour: {alphas:?}"
    );
}

#[test]
fn a_key_colour_picked_from_real_footage_keys_out_the_pixel_it_was_picked_from() {
    // `GREEN` above ([0,1,0,1]) is a fixed point of the transfer curve (0
    // stays 0, 1 stays 1), so it can't tell a correct linear-space
    // comparison from a buggy encoded-vs-linear one — both give the same
    // answer. A real eyedropper pick off actual green-screen footage looks
    // nothing like that: a genuine mid-tone value in every channel. Picking
    // the key colour directly from a pixel and keying that same pixel must
    // key it out completely (distance 0, by construction) regardless of
    // which colour space the comparison happens in — if it doesn't, the two
    // sides of the comparison are in different spaces.
    let mid_tone_green_8bit = [51u8, 199, 61, 255];
    let key_as_float = [
        mid_tone_green_8bit[0] as f32 / 255.0,
        mid_tone_green_8bit[1] as f32 / 255.0,
        mid_tone_green_8bit[2] as f32 / 255.0,
        1.0,
    ];
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, mid_tone_green_8bit), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    // Tight enough that any conversion-mismatch distance (which is large,
    // not a rounding error — the two colour spaces disagree by a lot for a
    // mid-tone value) shows up as visibly non-zero alpha rather than being
    // swallowed by the smoothness ramp.
    c.effects = vec![chroma_key_fx(key_as_float, 0.001, 0.0001, 0.0)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let px = render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_eq!(px[3], 0, "a pixel keyed against its own exact colour must key out fully, got alpha {}", px[3]);
}

#[test]
fn spill_suppression_pulls_the_key_channel_down_toward_the_other_two() {
    // A pixel with a green cast (spill from a green screen reflecting onto
    // the subject) but far enough from the key colour overall to stay
    // opaque. Real min/max despill only ever pulls the dominant key channel
    // *down* to the max of the other two — it must never touch red or blue.
    let comp = compositor();
    let spilled = [40u8, 210, 60, 255]; // green well above both red and blue
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, spilled), SEQ_W, SEQ_H, rec709()));

    let render_with_spill = |spill: f64| {
        let mut c = clip(1, 1, 0, 100);
        c.effects = vec![chroma_key_fx(GREEN, 0.1, 0.05, spill)];
        let p = project_with(vec![video_track(1, vec![c])]);
        render(&comp, &p, &sources).0.pixel(SEQ_W / 2, SEQ_H / 2)
    };

    let none = render_with_spill(0.0);
    let full = render_with_spill(1.0);
    assert!(full[1] < none[1], "full despill should reduce green below the untouched value: {full:?} vs {none:?}");
    assert!(full[1] >= full[0].max(full[2]) - 3, "despill should pull green down to about max(r,b), got {full:?}");
    assert_channel_near(full[0], none[0], 3, "red must be untouched by despill");
    assert_channel_near(full[2], none[2], 3, "blue must be untouched by despill");
}

// --- M5: Mask ---

fn mask_fx(is_rect: bool, center: (f64, f64), size: (f64, f64), feather: f64, invert: bool) -> EffectInstance {
    effect_fx(
        3,
        mask::TYPE_ID,
        vec![
            (mask::IS_RECTANGLE, ParamValue::Bool(is_rect)),
            (mask::CENTER, ParamValue::Vec2(center.0, center.1)),
            (mask::SIZE, ParamValue::Vec2(size.0, size.1)),
            (mask::FEATHER, ParamValue::Number(feather)),
            (mask::INVERT, ParamValue::Bool(invert)),
        ],
    )
}

#[test]
fn rectangle_mask_shows_inside_and_hides_outside() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![mask_fx(true, (0.5, 0.5), (0.2, 0.2), 0.001, false)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 255, "centre is inside the rectangle mask");
    assert_eq!(frame.pixel(2, 2)[3], 0, "corner is well outside the rectangle mask");
}

#[test]
fn ellipse_mask_shows_inside_and_hides_outside() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![mask_fx(false, (0.5, 0.5), (0.2, 0.2), 0.001, false)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 255, "centre is inside the ellipse mask");
    assert_eq!(frame.pixel(2, 2)[3], 0, "corner is well outside the ellipse mask's radius");
    // A point inside the mask's bounding box but outside its actual circular
    // radius (a corner of the box, not of the frame) — this is what
    // distinguishes an ellipse from a same-sized rectangle mask.
    let box_corner_but_outside_circle = frame.pixel(
        (0.5 * SEQ_W as f32 + 0.19 * SEQ_W as f32) as u32,
        (0.5 * SEQ_H as f32 + 0.19 * SEQ_H as f32) as u32,
    );
    assert_eq!(box_corner_but_outside_circle[3], 0, "diagonal corner of the bounding box should be outside the ellipse");
}

#[test]
fn mask_feather_produces_a_smooth_gradient_not_a_hard_edge() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![mask_fx(true, (0.5, 0.5), (0.2, 0.2), 0.15, false)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    // Walking outward from the centre across the feathered edge, alpha
    // should decrease monotonically rather than jumping straight from
    // 255 to 0.
    let y = SEQ_H / 2;
    let alphas: Vec<u8> = (SEQ_W / 2..SEQ_W).map(|x| frame.pixel(x, y)[3]).collect();
    assert_eq!(alphas[0], 255, "still fully inside at the centre");
    assert!(*alphas.last().unwrap() < 10, "fully outside by the frame edge");
    let mut saw_intermediate = false;
    for &a in &alphas {
        if a > 10 && a < 245 {
            saw_intermediate = true;
            break;
        }
    }
    assert!(saw_intermediate, "expected a soft gradient somewhere in the feather zone, got {alphas:?}");
}

#[test]
fn mask_invert_swaps_inside_and_outside() {
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![mask_fx(true, (0.5, 0.5), (0.2, 0.2), 0.001, true)];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);

    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 0, "inverted mask hides the centre");
    assert_eq!(frame.pixel(2, 2)[3], 255, "inverted mask shows the corner");
}

#[test]
fn mask_moves_with_the_clip_not_with_the_sequence() {
    // Masks are defined in the clip's own source-UV space (see
    // effect.rs's `mask` module doc comment), so a clip repositioned via
    // `transform::POSITION` must carry its mask along with it rather than
    // leaving the mask fixed in sequence space.
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(MediaAssetId(1), 0, comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()));
    let mut c = clip(1, 1, 0, 100);
    c.effects = vec![
        mask_fx(true, (0.5, 0.5), (0.2, 0.2), 0.001, false),
        transform_fx(vec![(transform::POSITION, ParamValue::Vec2(1000.0, 0.0))]),
    ];
    let p = project_with(vec![video_track(1, vec![c])]);
    let (frame, _) = render(&comp, &p, &sources);
    // The clip (and its mask) have been pushed far off-frame, so nothing
    // from this clip should be visible anywhere in the sequence — if the
    // mask were evaluated in sequence space instead, the old on-screen mask
    // position could still show content.
    for (x, y) in [(0, 0), (SEQ_W / 2, SEQ_H / 2), (SEQ_W - 1, SEQ_H - 1)] {
        assert_eq!(frame.pixel(x, y)[3], 0, "clip moved off-frame; nothing of it (mask included) should show at ({x},{y})");
    }
}

fn read_back(comp: &Compositor, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let staging = comp.device().create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
        size: (padded * height) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = comp
        .device()
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("test readback") });
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &staging,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
    );
    comp.queue().submit(std::iter::once(encoder.finish()));

    let slice = staging.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    comp.device().poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();

    let mapped = slice.get_mapped_range();
    let mut out = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        out.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    drop(mapped);
    staging.unmap();
    out
}

// ---------------------------------------------------------------------------
// Transitions
// ---------------------------------------------------------------------------

/// Renders at an explicit tick. The transition tests need a nonzero tick
/// (the transition region straddles a cut partway through the sequence);
/// `SourceFrames`' nearest-at-or-before lookup still resolves the uploads made
/// at pts 0, so this stays a compositing test rather than a pts-arithmetic one.
fn render_at(
    comp: &Compositor,
    project: &Project,
    sources: &SourceFrames,
    tick: i64,
) -> (render::RenderedFrame, render::CompositeStats) {
    let compiler = GraphCompiler::new(BuiltinRegistry::default());
    let graph = compiler
        .compile(project, SequenceId(1), TimeTick(tick))
        .expect("sequence exists");
    comp.render_to_rgba(&graph, sources, DeliverySpace::Rec709)
}

/// White clip cut to a black clip at tick 100, with `kind` over `duration`
/// centred on the cut — so the transition region is `100 +/- duration/2`.
fn transition_project(kind: timeline::TransitionKind, duration: i64) -> Project {
    let mut track = video_track(1, vec![clip(1, 1, 0, 100), clip(2, 2, 100, 200)]);
    track.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind,
        at: TimeTick(100),
        duration: TimeTick(duration),
    }];
    project_with(vec![track])
}

fn white_and_black_sources(comp: &Compositor) -> SourceFrames {
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 255, 255, 255]), SEQ_W, SEQ_H, rec709()),
    );
    sources.insert(
        MediaAssetId(2),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );
    sources
}

#[test]
fn cross_dissolve_midpoint_is_a_true_fifty_percent_linear_mix() {
    // The measurement that decides whether the dissolve is actually right.
    //
    // Correct (outgoing opaque, incoming over it at p): 0.5*white + 0.5*black
    // = linear 0.5, which re-encodes to ~180/255.
    //
    // The plausible-looking bug (draw BOTH layers at partial opacity) gives
    // white*(1-p)^2 + black*p = linear 0.25 -> ~137/255. Every dissolve would
    // visibly sag in the middle. The two numbers are far enough apart that this
    // test can tell them apart rather than just "looking about right".
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::CrossDissolve, 40);

    // Region is 80..120, so tick 100 is exactly halfway.
    let (frame, stats) = render_at(&comp, &p, &sources, 100);
    assert_eq!(stats.layers_drawn, 2, "a dissolve must draw both of its clips");

    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    let correct = (render::color::linear_to_rec709(0.5) * 255.0).round() as u8;
    let if_double_partial = (render::color::linear_to_rec709(0.25) * 255.0).round() as u8;
    assert_channel_near(px[0], correct, 3, "dissolve midpoint");
    assert!(
        px[0].abs_diff(if_double_partial) > 20,
        "midpoint {} is close to the double-partial-opacity value {} — the dissolve is \
         compositing both layers at partial alpha and sagging in the middle",
        px[0],
        if_double_partial
    );
    assert_eq!(px[3], 255, "a dissolve between two opaque clips must stay opaque");
}

#[test]
fn cross_dissolve_ends_match_the_clips_either_side_of_the_cut() {
    // The transition must be continuous with its neighbours: no visible jump
    // into or out of it. Sampled just inside each end of the region.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::CrossDissolve, 40);

    // At the region's start, progress is 0 — entirely the outgoing clip.
    let start = render_at(&comp, &p, &sources, 80).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_channel_near(start[0], 255, 3, "region start should still be the outgoing white clip");

    // Just *past* the region, the transition no longer applies at all, so this
    // is the plain incoming clip. This is the honest continuity check — the
    // last tick *inside* the region is at progress 39/40, which still carries
    // 2.5% of the white clip, and 2.5% linear encodes to 28/255 through the
    // Rec.709 toe rather than to near-zero. Asserting "< 20" there would be
    // asserting the transfer curve is wrong.
    let after = render_at(&comp, &p, &sources, 120).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_channel_near(after[0], 0, 3, "just past the region it's the plain incoming clip");

    // Before the region, likewise the plain outgoing clip.
    let before = render_at(&comp, &p, &sources, 50).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_channel_near(before[0], 255, 3, "before the transition");

    // And the last tick inside the region matches what the mix predicts, which
    // is a stronger statement than any hand-picked threshold.
    let last_inside = render_at(&comp, &p, &sources, 119).0.pixel(SEQ_W / 2, SEQ_H / 2);
    let expected = (render::color::linear_to_rec709(1.0 - 39.0 / 40.0) * 255.0).round() as u8;
    assert_channel_near(last_inside[0], expected, 3, "last tick inside the region");
}

#[test]
fn cross_dissolve_is_monotonic_across_its_region() {
    // A dissolve between white and black must darken steadily. This catches
    // any non-monotonic artefact (a mid dip, an endpoint discontinuity) that a
    // three-point check could step straight over.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::CrossDissolve, 100);

    // Region 50..150. Each sample is also checked against the value the mix
    // predicts, so this pins the actual curve rather than only its direction.
    let mut prev = 256i32;
    for tick in (50..150).step_by(5) {
        let v = render_at(&comp, &p, &sources, tick).0.pixel(SEQ_W / 2, SEQ_H / 2)[0] as i32;
        assert!(
            v <= prev + 2, // small tolerance for 8-bit quantisation
            "dissolve brightened at tick {tick}: {v} after {prev}"
        );
        let progress = (tick - 50) as f32 / 100.0;
        let expected = (render::color::linear_to_rec709(1.0 - progress) * 255.0).round() as i32;
        assert!(
            (v - expected).abs() <= 3,
            "at tick {tick} (progress {progress:.2}) expected ~{expected}, got {v}"
        );
        prev = v;
    }
    // 145 is progress 0.95, so 5% of the white clip remains — which encodes to
    // ~48/255, not to near-zero. Comparing against the model keeps this honest.
    let final_expected = (render::color::linear_to_rec709(0.05) * 255.0).round() as i32;
    assert!(
        (prev - final_expected).abs() <= 3,
        "final sample should match the 95%-through mix (~{final_expected}), got {prev}"
    );
}

#[test]
fn dip_to_black_reaches_opaque_black_at_its_midpoint() {
    // Dip-to-black dims the outgoing clip's colour to zero over the first half
    // and brings the incoming one back up over the second, so exactly at the
    // middle the track is opaque black. Opaque, not absent: the earlier version
    // of this faded alpha instead, which looked identical on this single-track
    // sequence and was wrong the moment anything sat underneath — see
    // `dip_to_black_on_an_upper_track_hides_the_track_underneath`.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::DipToBlack, 40);

    let (frame, stats) = render_at(&comp, &p, &sources, 100);
    let px = frame.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_eq!(stats.layers_drawn, 1, "the dip draws a layer — an opaque black one");
    assert_eq!(px[3], 255, "the midpoint of a dip must be opaque");
    assert!(px[0] < 8 && px[1] < 8 && px[2] < 8, "and black, got {px:?}");

    // A quarter in, half the outgoing white clip's light is left. Checked
    // against the transfer curve rather than a hand-picked band: dimming
    // happens in linear light, so linear 0.5 encodes to ~180/255, not to 128.
    // Landing on 128 here would mean the dim happened in encoded space.
    let quarter = render_at(&comp, &p, &sources, 90).0.pixel(SEQ_W / 2, SEQ_H / 2);
    let expected = (render::color::linear_to_rec709(0.5) * 255.0).round() as u8;
    assert_channel_near(quarter[0], expected, 3, "a quarter through the dip");
    assert_eq!(quarter[3], 255, "and still opaque on the way down");
}

#[test]
fn dip_to_black_with_no_outgoing_clip_fades_up_from_black_across_the_whole_region() {
    // A dip at the head of the first clip has nothing to dip *from*, so there
    // is no midpoint to meet at — it's a single fade up from black over the
    // whole region. Ramping over the full duration (rather than sitting black
    // for the first half and ramping over the second) is what keeps it free of
    // a visible pop at the midpoint.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    // One clip starting at 100, with the transition centred on its head, so
    // the region is 80..120 and only the second half has material.
    let mut track = video_track(1, vec![clip(2, 1, 100, 200)]);
    track.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind: timeline::TransitionKind::DipToBlack,
        at: TimeTick(100),
        duration: TimeTick(40),
    }];
    let p = project_with(vec![track]);

    // Monotonically brightening the whole way, with no step at the midpoint.
    let mut prev = -1i32;
    for tick in [80, 90, 100, 110, 119] {
        let px = render_at(&comp, &p, &sources, tick).0.pixel(SEQ_W / 2, SEQ_H / 2);
        let v = px[0] as i32;
        assert_eq!(px[3], 255, "opaque throughout the fade up, at tick {tick}");
        assert!(v >= prev, "fade up went backwards at tick {tick}: {v} after {prev}");
        let progress = (tick - 80) as f32 / 40.0;
        let expected = (render::color::linear_to_rec709(progress) * 255.0).round() as i32;
        assert!(
            (v - expected).abs() <= 3,
            "at tick {tick} (progress {progress:.2}) expected ~{expected}, got {v}"
        );
        prev = v;
    }
}

#[test]
fn dip_to_black_on_an_upper_track_hides_the_track_underneath() {
    // The bug this pins: a dip-to-black used to fade the track's *alpha* to
    // zero, so at the midpoint the track vanished rather than going black. On
    // the bottom track that reads correctly (there's nothing behind but the
    // black backdrop), which is exactly why it went unnoticed. On an upper
    // track it's plainly wrong — the picture underneath shows through, so a
    // "dip to black" dips to whatever happens to be below it instead.
    //
    // Black means opaque black: alpha stays 1 and the *colour* goes to zero.
    let comp = compositor();
    let mut sources = white_and_black_sources(&comp);
    sources.insert(
        MediaAssetId(3),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );

    // V1 (bottom): red for the whole sequence. V2 (top): white -> black with a
    // dip-to-black over ticks 80..120.
    let mut top = video_track(2, vec![clip(2, 1, 0, 100), clip(3, 2, 100, 200)]);
    top.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind: timeline::TransitionKind::DipToBlack,
        at: TimeTick(100),
        duration: TimeTick(40),
    }];
    let p = project_with(vec![video_track(1, vec![clip(1, 3, 0, 200)]), top]);

    let px = render_at(&comp, &p, &sources, 100).0.pixel(SEQ_W / 2, SEQ_H / 2);
    assert_eq!(px[3], 255, "the dip must stay opaque so it covers the track below");
    assert!(
        px[0] < 8 && px[1] < 8 && px[2] < 8,
        "midpoint of a dip-to-black must be black, not the red track underneath — got {px:?}"
    );
}

/// A title clip. Unlike `clip`, it references no asset at all — the spec is
/// the source, so nothing needs uploading into `SourceFrames` for it.
fn title_clip(id: u64, spec: timeline::TitleSpec, tin: i64, tout: i64) -> ClipInstance {
    ClipInstance { source: ClipSource::Title(spec), ..clip(id, 0, tin, tout) }
}

fn big_title(text: &str) -> timeline::TitleSpec {
    // Sized to the 64x64 test sequence: large enough to leave solid interior
    // pixels to sample, small enough to stay inside the frame.
    timeline::TitleSpec { text: text.into(), size_px: 28.0, ..Default::default() }
}

#[test]
fn a_title_clip_composites_without_any_uploaded_source() {
    // Titles are generated, not decoded, so the whole decode path is bypassed:
    // nothing was inserted into `SourceFrames`, and the frame must still come
    // out with picture on it and no missing-source report.
    let comp = compositor();
    let p = project_with(vec![video_track(1, vec![title_clip(1, big_title("HI"), 0, 100)])]);

    let (frame, stats) = render(&comp, &p, &SourceFrames::default());
    assert_eq!(stats.layers_missing_source, 0, "a title has no source to be missing");
    assert_eq!(stats.layers_drawn, 1);

    let lit = (0..SEQ_H)
        .flat_map(|y| (0..SEQ_W).map(move |x| (x, y)))
        .filter(|&(x, y)| frame.pixel(x, y)[3] > 128)
        .count();
    assert!(lit > 0, "the title drew nothing");
    assert!(lit < (SEQ_W * SEQ_H) as usize / 2, "the title should be glyphs, not a filled frame");
    assert_eq!(frame.pixel(0, 0)[3], 0, "the corner is outside the text and stays transparent");
}

#[test]
fn title_glyphs_are_opaque_over_a_track_below_and_the_gaps_are_not() {
    // The end-to-end statement: a title behaves like any other source with an
    // alpha channel. Glyph pixels cover the track below; the transparent gaps
    // between letters let it through. Getting straight-vs-premultiplied alpha
    // wrong shows up right here as dark fringing or a black box.
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(1),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [255, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );
    let p = project_with(vec![
        video_track(1, vec![clip(1, 1, 0, 100)]),
        video_track(2, vec![title_clip(2, big_title("HI"), 0, 100)]),
    ]);
    let (frame, _) = render(&comp, &p, &sources);

    let px: Vec<[u8; 4]> = (0..SEQ_H)
        .flat_map(|y| (0..SEQ_W).map(move |x| (x, y)))
        .map(|(x, y)| frame.pixel(x, y))
        .collect();
    // White text on red: a glyph pixel is bright in all three channels.
    let glyph = px.iter().find(|p| p[1] > 200 && p[2] > 200);
    assert!(glyph.is_some(), "no white glyph pixel found over the red track");
    // And the corner, outside the text, is still the red track underneath —
    // not black, which is what a premultiplied buffer would have left there.
    assert_eq!(frame.pixel(0, 0), [255, 0, 0, 255], "outside the glyphs the track below shows");
}

#[test]
fn a_title_naming_a_font_nobody_has_still_renders_in_a_fallback() {
    // A project made on another machine will name fonts this one lacks. That
    // must degrade to the wrong typeface, never to a blank frame — the text is
    // the content, the font is a preference.
    let comp = compositor();
    let mut spec = big_title("HI");
    spec.font_family = "No Such Font Installed Anywhere".into();
    let p = project_with(vec![video_track(1, vec![title_clip(1, spec, 0, 100)])]);

    let (frame, stats) = render(&comp, &p, &SourceFrames::default());
    assert_eq!(stats.layers_missing_source, 0);
    let lit = (0..SEQ_H)
        .flat_map(|y| (0..SEQ_W).map(move |x| (x, y)))
        .filter(|&(x, y)| frame.pixel(x, y)[3] > 128)
        .count();
    assert!(lit > 0, "an unknown font family should fall back, not render nothing");
}

#[test]
fn a_title_dissolves_like_any_other_clip() {
    // Titles go through the same transform/opacity/blend path as decoded
    // frames, which is the entire reason `FrameSource::Title` sits alongside
    // `Media` instead of being a special case bolted onto the track loop. A
    // cross dissolve from a title is the cheapest proof of that.
    let comp = compositor();
    let mut sources = SourceFrames::default();
    sources.insert(
        MediaAssetId(2),
        0,
        comp.upload_rgba(&solid(SEQ_W, SEQ_H, [0, 0, 0, 255]), SEQ_W, SEQ_H, rec709()),
    );
    let mut track = video_track(1, vec![title_clip(1, big_title("HI"), 0, 100), clip(2, 2, 100, 200)]);
    track.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind: timeline::TransitionKind::CrossDissolve,
        at: TimeTick(100),
        duration: TimeTick(40),
    }];
    let p = project_with(vec![track]);

    let (_, stats) = render_at(&comp, &p, &sources, 100);
    assert_eq!(stats.layers_drawn, 2, "both sides of the dissolve draw, title included");
    assert_eq!(stats.layers_missing_source, 0);
}

#[test]
fn an_unchanged_title_is_rasterised_once_and_reused() {
    // Measured at 1080p (`title_rasterisation_cost.rs`): a two-line title
    // costs ~3.5ms/frame, of which ~3.1ms is filling the frame-sized buffer
    // rather than drawing glyphs. That buffer is byte-identical every frame a
    // title holds still, which is nearly always — so re-doing it is ~10% of a
    // 30fps frame budget spent reproducing the previous answer.
    let comp = compositor();
    let p = project_with(vec![video_track(1, vec![title_clip(1, big_title("HI"), 0, 100)])]);

    render_at(&comp, &p, &SourceFrames::default(), 10);
    let (hits, misses) = comp.title_cache_stats();
    assert_eq!((hits, misses), (0, 1), "the first frame has to do the work");

    // Same title, different tick — a title looks the same at every tick of
    // its clip, so nothing needs redoing.
    render_at(&comp, &p, &SourceFrames::default(), 20);
    render_at(&comp, &p, &SourceFrames::default(), 30);
    let (hits, misses) = comp.title_cache_stats();
    assert_eq!((hits, misses), (2, 1), "later frames should reuse the raster");
}

#[test]
fn editing_a_title_rasterises_it_again() {
    // The other half: a cache that never invalidates would freeze the text at
    // whatever it said when first drawn, which is worse than no cache at all.
    let comp = compositor();
    let a = project_with(vec![video_track(1, vec![title_clip(1, big_title("HI"), 0, 100)])]);
    let b = project_with(vec![video_track(1, vec![title_clip(1, big_title("BYE"), 0, 100)])]);

    let frame_a = render_at(&comp, &a, &SourceFrames::default(), 10).0;
    let frame_b = render_at(&comp, &b, &SourceFrames::default(), 10).0;
    assert_eq!(comp.title_cache_stats(), (0, 2), "different text is a different raster");

    // And the pixels genuinely differ, so this isn't just a counter moving.
    let ink = |f: &render::RenderedFrame| {
        (0..SEQ_H)
            .flat_map(|y| (0..SEQ_W).map(move |x| (x, y)))
            .filter(|&(x, y)| f.pixel(x, y)[3] > 128)
            .count()
    };
    assert_ne!(ink(&frame_a), ink(&frame_b), "the frame should show the new text");
}

#[test]
fn a_resized_sequence_rasterises_the_title_again() {
    // The raster is frame-sized, so the same spec at a different resolution is
    // a different answer. Keying on the spec alone would stretch a 64px raster
    // across a 1080p frame.
    let comp = compositor();
    let spec = big_title("HI");
    let small = project_with(vec![video_track(1, vec![title_clip(1, spec.clone(), 0, 100)])]);
    let mut large = small.clone();
    large.sequences[0].settings.width = SEQ_W * 2;
    large.sequences[0].settings.height = SEQ_H * 2;

    render_at(&comp, &small, &SourceFrames::default(), 10);
    render_at(&comp, &large, &SourceFrames::default(), 10);
    assert_eq!(comp.title_cache_stats(), (0, 2), "a different frame size is a different raster");
}

#[test]
fn a_wipe_shows_the_incoming_clip_on_one_side_of_a_hard_edge_and_the_outgoing_on_the_other() {
    // The defining property of a wipe, and what separates it from a dissolve:
    // at the midpoint neither clip is faded — each is fully itself, on its own
    // side of the boundary. A dissolve at p=0.5 would give a uniform grey
    // everywhere; this must give white on one side and black on the other.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::Wipe, 40);

    // Region is 80..120, so tick 100 is exactly halfway.
    let frame = render_at(&comp, &p, &sources, 100).0;
    let left = frame.pixel(SEQ_W / 4, SEQ_H / 2);
    let right = frame.pixel(SEQ_W * 3 / 4, SEQ_H / 2);

    assert!(left[0] < 20, "the revealed (incoming, black) clip should be on the left, got {left:?}");
    assert!(right[0] > 235, "the outgoing (white) clip should still hold the right, got {right:?}");
    assert_eq!(left[3], 255, "a wipe is opaque on the revealed side");
    assert_eq!(right[3], 255, "and on the outgoing side");
}

#[test]
fn a_wipe_edge_advances_across_the_frame_as_it_progresses() {
    // One sample can't tell a wipe from a hard cut at the midpoint. Tracking
    // where the boundary actually sits over time is what proves it sweeps.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::Wipe, 100);

    // Region 50..150. The edge x is the count of black (incoming) columns.
    let edge_at = |tick: i64| {
        let frame = render_at(&comp, &p, &sources, tick).0;
        (0..SEQ_W).filter(|&x| frame.pixel(x, SEQ_H / 2)[0] < 128).count()
    };

    let quarter = edge_at(75);
    let half = edge_at(100);
    let three_quarters = edge_at(125);
    assert!(
        quarter < half && half < three_quarters,
        "the wipe edge should sweep across: {quarter} -> {half} -> {three_quarters} columns revealed"
    );
    // And it should land near the predicted fraction, not merely move.
    let expected_half = (SEQ_W / 2) as usize;
    assert!(
        half.abs_diff(expected_half) <= 3,
        "at the midpoint about half the frame should be revealed (~{expected_half}), got {half}"
    );
}

#[test]
fn a_slide_moves_the_incoming_clip_in_from_the_edge() {
    // A slide differs from a wipe in that the incoming clip *moves*: at the
    // midpoint its left edge sits mid-frame, so the far right shows the
    // incoming clip's own left half rather than its middle.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::Slide, 40);

    let frame = render_at(&comp, &p, &sources, 100).0;
    let left = frame.pixel(SEQ_W / 4, SEQ_H / 2);
    let right = frame.pixel(SEQ_W * 3 / 4, SEQ_H / 2);

    assert!(left[0] > 235, "the stationary outgoing clip should still hold the left, got {left:?}");
    assert!(right[0] < 20, "the incoming clip should have slid into the right, got {right:?}");
    assert_eq!(left[3], 255);
    assert_eq!(right[3], 255);
}

#[test]
fn wipe_and_slide_both_land_exactly_on_the_incoming_clip_when_finished() {
    // Continuity at the far end: the last moment of the transition has to
    // match the plain incoming clip, or every wipe and slide ends with a
    // visible jump.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);

    for kind in [timeline::TransitionKind::Wipe, timeline::TransitionKind::Slide] {
        let p = transition_project(kind, 40);
        // Just past the region: the transition no longer applies at all.
        let after = render_at(&comp, &p, &sources, 130).0.pixel(SEQ_W / 2, SEQ_H / 2);
        assert_channel_near(after[0], 0, 3, "past the region it is the plain incoming clip");
        // And just before it starts, the plain outgoing clip.
        let before = render_at(&comp, &p, &sources, 70).0.pixel(SEQ_W / 2, SEQ_H / 2);
        assert_channel_near(before[0], 255, 3, "before the region it is the plain outgoing clip");
    }
}

#[test]
fn a_wipe_does_not_discard_the_clips_own_mask() {
    // The wipe boundary and a clip's own mask effect are independent things
    // that must compose. Implementing the wipe by reusing the mask uniform
    // would have been cheaper and would silently drop the user's mask for the
    // duration of every wipe.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let mut track = video_track(1, vec![clip(1, 1, 0, 100), clip(2, 2, 100, 200)]);
    // Mask the incoming clip to a small centred ellipse; outside it the clip
    // is transparent regardless of what the wipe is doing.
    track.clips[1].effects = vec![mask_fx(false, (0.5, 0.5), (0.15, 0.15), 0.01, false)];
    track.transitions = vec![timeline::Transition {
        id: timeline::TransitionId(1),
        kind: timeline::TransitionKind::Wipe,
        at: TimeTick(100),
        duration: TimeTick(40),
    }];
    let p = project_with(vec![track]);

    // Near the left edge: inside the wiped-in region, but far outside the
    // incoming clip's mask — so the outgoing clip must still show through.
    let frame = render_at(&comp, &p, &sources, 100).0;
    let masked_out = frame.pixel(2, SEQ_H / 2);
    assert!(
        masked_out[0] > 235,
        "the incoming clip is masked away here, so the outgoing clip should show, got {masked_out:?}"
    );
}

#[test]
fn a_transition_longer_than_its_clips_still_renders() {
    // Nothing stops a project file from carrying a transition wider than the
    // material either side of it (hand-edited, or trimmed after the fact).
    // Both clips then get sampled well outside their handles, which the decoder
    // clamps — the renderer must not panic or drop the frame.
    let comp = compositor();
    let sources = white_and_black_sources(&comp);
    let p = transition_project(timeline::TransitionKind::CrossDissolve, 10_000);

    let (frame, stats) = render_at(&comp, &p, &sources, 100);
    assert_eq!(stats.layers_drawn, 2);
    assert_eq!(frame.pixel(SEQ_W / 2, SEQ_H / 2)[3], 255);
}
