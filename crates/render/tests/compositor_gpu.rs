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
    headless_context, transform, BuiltinRegistry, Compositor, DeliverySpace, GraphCompiler,
    SourceFrames,
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

fn transform_fx(params: Vec<(&str, ParamValue)>) -> EffectInstance {
    let mut map = BTreeMap::new();
    for (name, value) in params {
        map.insert(name.to_string(), ParamTrack::constant(value));
    }
    EffectInstance {
        id: EffectInstanceId(1),
        effect_type: transform::TYPE_ID.to_string(),
        enabled: true,
        params: map,
    }
}

fn video_track(id: u64, clips: Vec<ClipInstance>) -> Track {
    Track {
        id: TrackId(id),
        kind: TrackKind::Video,
        name: format!("V{id}"),
        clips,
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
