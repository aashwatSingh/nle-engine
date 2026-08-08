//! M4 capstone: **timeline playback with real compositing.**
//!
//! Builds a genuine two-video-track `timeline::Project` from real media,
//! plays it back against the audio-derived master clock (M2), and for every
//! displayed frame compiles the render graph at the current tick (M4b) and
//! composites it on the GPU in linear light (M4d). Nothing here is mocked:
//! the frames are FFmpeg-decoded, the clock comes from samples the audio
//! device actually consumed, and the picture is the compositor's output.
//!
//! Track 2 carries a real transform (scaled, offset, rotated, semi-opaque)
//! so the composite is visibly a composite rather than one clip covering
//! another.
//!
//! Controls: Space = play/pause, Left/Right = seek 2s.
//!
//! Scope: still not the editor UI (M3's timeline *widget* and the panel
//! system are not built). There is no timeline to click on here — the
//! project is constructed in code.

use playback::{AudioEngine, VideoPlayback};
use render::wgpu;
use render::{
    transform, BuiltinRegistry, Compositor, DeliverySpace, GraphCompiler, SourceFrames,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use timeline::{
    ClipInstance, ClipInstanceId, ClipSource, EffectInstance, EffectInstanceId, FrameRate,
    ParamTrack, ParamValue, Project, Sequence, SequenceId, SequenceSettings, SpeedCurve, TimeTick,
    Track, TrackId, TrackKind, TIMEBASE,
};
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::WindowBuilder;

const SEQ_W: u32 = 640;
const SEQ_H: u32 = 360;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures")
        .join(name)
}

/// Seeks audio and every video track together, and drops held frames so the
/// nearest-at-or-before lookup can't present picture from the old position.
#[allow(clippy::too_many_arguments)]
fn seek_all(
    to_ticks: i64,
    audio: &AudioEngine,
    bottom: &VideoPlayback,
    top: &VideoPlayback,
    sources: &mut SourceFrames,
    bottom_last_pts: &mut Option<i64>,
    top_last_pts: &mut Option<i64>,
) {
    audio.seek(to_ticks);
    bottom.seek(to_ticks);
    top.seek(to_ticks);
    sources.clear();
    *bottom_last_pts = None;
    *top_last_pts = None;
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

fn clip(id: u64, asset: media::MediaAssetId, duration: i64, effects: Vec<EffectInstance>) -> ClipInstance {
    ClipInstance {
        id: ClipInstanceId(id),
        source: ClipSource::Media(asset),
        source_in: TimeTick(0),
        source_out: TimeTick(duration),
        timeline_in: TimeTick(0),
        timeline_out: TimeTick(duration),
        speed: SpeedCurve::Constant { numerator: 1, denominator: 1 },
        effects,
        audio_gain_db: ParamTrack::constant(ParamValue::Number(0.0)),
        audio_pan: ParamTrack::constant(ParamValue::Number(0.0)),
        linked_group: None,
    }
}

fn video_track(id: u64, name: &str, clips: Vec<ClipInstance>) -> Track {
    Track {
        id: TrackId(id),
        kind: TrackKind::Video,
        name: name.into(),
        clips,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    }
}

fn main() {
    media_ffmpeg::init().expect("ffmpeg init failed");

    // --- Ingest (M1) ---
    let bottom_path = fixture("test_playback_demo.mp4"); // 8s, has audio
    let top_path = fixture("test_h264.mp4"); // 3s
    let bottom_asset = media_ffmpeg::probe(&bottom_path).expect("probe bottom");
    let top_asset = media_ffmpeg::probe(&top_path).expect("probe top");
    println!(
        "V1 <- {} ({:.2}s)\nV2 <- {} ({:.2}s)",
        bottom_path.file_name().unwrap().to_string_lossy(),
        bottom_asset.duration_ticks as f64 / TIMEBASE as f64,
        top_path.file_name().unwrap().to_string_lossy(),
        top_asset.duration_ticks as f64 / TIMEBASE as f64,
    );
    let bottom_color = bottom_asset.video.as_ref().expect("bottom has video").color;
    let top_color = top_asset.video.as_ref().expect("top has video").color;

    // --- Timeline (M3) ---
    let project = Arc::new(Project {
        sequences: vec![Sequence {
            id: SequenceId(1),
            name: "Composite Demo".into(),
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: SEQ_W,
                height: SEQ_H,
                sample_rate: 48_000,
                working_color_primaries: media::ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks: vec![
                video_track(1, "V1", vec![clip(1, bottom_asset.id, bottom_asset.duration_ticks, vec![])]),
                video_track(
                    2,
                    "V2",
                    // Its own natural duration, not the bottom clip's: past
                    // 3s this track simply has no active clip, so the graph
                    // reports `None` for it and only V1 composites. Visible
                    // in the per-second `layers/frame` readout.
                    vec![clip(
                        2,
                        top_asset.id,
                        top_asset.duration_ticks,
                        vec![transform_fx(vec![
                            (transform::SCALE, ParamValue::Vec2(0.45, 0.45)),
                            (transform::POSITION, ParamValue::Vec2(150.0, -80.0)),
                            (transform::ROTATION, ParamValue::Number(-8.0)),
                            (transform::OPACITY, ParamValue::Number(0.85)),
                        ])],
                    )],
                ),
            ],
            markers: vec![],
        }],
        assets: vec![bottom_asset.clone(), top_asset.clone()],
    });
    let sequence_duration = project.sequences[0].duration();

    // --- Window + GPU ---
    let event_loop = EventLoop::new().unwrap();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("nle-engine: timeline playback with real compositing (M4) — Space=play/pause, arrows=seek")
            .with_inner_size(winit::dpi::LogicalSize::new(SEQ_W as f64, SEQ_H as f64))
            .build(&event_loop)
            .unwrap(),
    );

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let surface = instance.create_surface(window.clone()).expect("create_surface");
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no GPU adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("play_timeline"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
        },
        None,
    ))
    .expect("request_device");
    let (device, queue) = (Arc::new(device), Arc::new(queue));

    let caps = surface.get_capabilities(&adapter);
    // Prefer a NON-sRGB surface format: the delivery pass encodes the
    // transfer function itself, so an sRGB swapchain would apply it twice and
    // wash the picture out.
    let surface_format = caps
        .formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .unwrap_or_else(|| {
            eprintln!(
                "warning: no non-sRGB surface format available; picture will be double-encoded. \
                 Available: {:?}",
                caps.formats
            );
            caps.formats[0]
        });
    println!("adapter: {} ({:?})", adapter.get_info().name, adapter.get_info().backend);
    println!("surface format: {surface_format:?}");

    let size = window.inner_size();
    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format: surface_format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let compositor = Compositor::with_output_format(device.clone(), queue.clone(), surface_format);
    let compiler = GraphCompiler::new(BuiltinRegistry::default());
    let mut sources = SourceFrames::default();

    // --- Audio clock (M2) ---
    //
    // Started *after* GPU/window setup on purpose: the audio clock begins
    // advancing the moment the engine starts, so starting it earlier meant
    // ~2s of the timeline played out during device init and the window opened
    // already mid-sequence.
    let audio = AudioEngine::start(bottom_path.clone(), 0).expect("audio engine");
    let clock = audio.clock();
    let mut playing = true;

    // --- Video decode, paced against the audio clock (M2) ---
    //
    // Reusing `playback::VideoPlayback` rather than raw decode threads is
    // load-bearing, not just convenient: it already paces decoding against
    // the clock, drops frames when it falls behind, and handles seeks. An
    // earlier version of this demo spawned unpaced decode threads and, at
    // 166fps redraw against 30fps content, they raced ahead and decoded the
    // whole file — leaving ~150 GPU textures (~220MB at 640x360, and far
    // worse at 4K) resident at once. Pacing is what keeps that bounded.
    let bottom_video = VideoPlayback::start(bottom_path.clone(), {
        let c = clock.clone();
        move || c.current_tick()
    })
    .expect("bottom video playback");
    let top_video = VideoPlayback::start(top_path.clone(), {
        let c = clock.clone();
        move || c.current_tick()
    })
    .expect("top video playback");

    // Last pts uploaded per asset, so a frame isn't re-uploaded to the GPU on
    // every redraw (the render loop runs far faster than the frame rate).
    let mut bottom_last_pts: Option<i64> = None;
    let mut top_last_pts: Option<i64> = None;

    let mut last_report = Instant::now();
    let mut frames_presented = 0u32;
    let mut layers_last_frame = 0usize;

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop
        .run(move |event, elwt| match event {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => elwt.exit(),
                WindowEvent::Resized(new_size) => {
                    if new_size.width > 0 && new_size.height > 0 {
                        config.width = new_size.width;
                        config.height = new_size.height;
                        surface.configure(&device, &config);
                    }
                }
                WindowEvent::KeyboardInput { event: key, .. } => {
                    if key.state == ElementState::Pressed && !key.repeat {
                        match key.physical_key {
                            PhysicalKey::Code(KeyCode::Space) => {
                                playing = !playing;
                                if playing {
                                    audio.play();
                                    bottom_video.play();
                                    top_video.play();
                                } else {
                                    audio.pause();
                                    bottom_video.pause();
                                    top_video.pause();
                                }
                                println!("{}", if playing { "playing" } else { "paused" });
                            }
                            PhysicalKey::Code(KeyCode::ArrowRight) => {
                                let t = (clock.current_tick() + TIMEBASE * 2).min(sequence_duration.0);
                                seek_all(t, &audio, &bottom_video, &top_video, &mut sources, &mut bottom_last_pts, &mut top_last_pts);
                            }
                            PhysicalKey::Code(KeyCode::ArrowLeft) => {
                                let t = (clock.current_tick() - TIMEBASE * 2).max(0);
                                seek_all(t, &audio, &bottom_video, &top_video, &mut sources, &mut bottom_last_pts, &mut top_last_pts);
                            }
                            _ => {}
                        }
                    }
                }
                WindowEvent::RedrawRequested => {
                    let now_tick = clock.current_tick();

                    // Pull each track's current frame and upload it if it's
                    // new. A starved decoder just means `latest_frame` hasn't
                    // advanced, so the nearest-frame lookup holds the previous
                    // picture — the dropped-frame behaviour spec 4.4 asks for,
                    // rather than a stall or a black flash.
                    for (video, asset, color, last_pts) in [
                        (&bottom_video, bottom_asset.id, bottom_color, &mut bottom_last_pts),
                        (&top_video, top_asset.id, top_color, &mut top_last_pts),
                    ] {
                        if let Some(frame) = video.latest_frame() {
                            if *last_pts != Some(frame.pts_ticks) {
                                let tex = compositor.upload_rgba(
                                    &frame.rgba,
                                    frame.width,
                                    frame.height,
                                    color,
                                );
                                sources.insert(asset, frame.pts_ticks, tex);
                                *last_pts = Some(frame.pts_ticks);
                            }
                        }
                        // Release frames the playhead has moved well past.
                        sources.retain_from(asset, now_tick - TIMEBASE / 4);
                    }

                    let Some(graph) = compiler.compile(&project, SequenceId(1), TimeTick(now_tick))
                    else {
                        return;
                    };

                    let output = match surface.get_current_texture() {
                        Ok(t) => t,
                        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                            surface.configure(&device, &config);
                            return;
                        }
                        Err(e) => {
                            eprintln!("surface error: {e:?}");
                            return;
                        }
                    };
                    let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let stats =
                        compositor.render_to_view(&graph, &sources, DeliverySpace::Rec709, &view);
                    output.present();

                    layers_last_frame = stats.layers_drawn;
                    frames_presented += 1;
                }
                _ => {}
            },
            Event::AboutToWait => {
                window.request_redraw();
                if last_report.elapsed() >= Duration::from_secs(1) {
                    println!(
                        "t={:.2}s | {} fps | layers/frame={} | textures held={} | underruns={} | dropped v1/v2={}/{}",
                        clock.current_tick() as f64 / TIMEBASE as f64,
                        frames_presented,
                        layers_last_frame,
                        sources.len(),
                        audio.underrun_count(),
                        bottom_video.dropped_frame_count(),
                        top_video.dropped_frame_count(),
                    );
                    frames_presented = 0;
                    last_report = Instant::now();
                }
            }
            _ => {}
        })
        .unwrap();
}
