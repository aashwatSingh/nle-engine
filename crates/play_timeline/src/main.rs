//! M4 capstone: **timeline playback with real compositing** — now pointed at
//! whatever files the user picks, rather than only the fixed sample clips.
//!
//! Opens a native "choose video file(s)" dialog at startup. Each selected
//! file becomes its own video track (first = full-frame background,
//! subsequent = cascaded picture-in-picture overlays, purely so multiple
//! selections are visible at once — no colour grading is forced onto the
//! user's own footage). Cancel the dialog (or pick nothing usable) and it
//! falls back to the built-in two-clip demo with its original colour
//! correction + blur + rotated-overlay treatment.
//!
//! Audio comes from the first selected file that actually has an audio
//! stream. If none do (a silent screen recording, a muted clip), playback
//! falls back to a wall-clock-driven `SilentClock` instead of failing
//! outright — see `silent_clock.rs`.
//!
//! Nothing here is mocked: frames are FFmpeg-decoded, the clock is real
//! (audio-sample-derived when there is audio), and the picture is the GPU
//! compositor's actual output.
//!
//! Controls: Space = play/pause, Left/Right = seek 2s.
//!
//! Scope: still not the editor UI (M3's timeline *widget* and the panel
//! system are not built) — this is "pick files, watch them play," not
//! "build and edit a timeline."

mod silent_clock;

use playback::{AudioClock, AudioEngine, VideoPlayback};
use render::wgpu;
use render::{
    color_correction, gaussian_blur, transform, BuiltinRegistry, Compositor, DeliverySpace,
    GraphCompiler, SourceFrames,
};
use silent_clock::{SilentClock, SilentClockHandle};
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

const VIDEO_EXTENSIONS: &[&str] =
    &["mp4", "mov", "m4v", "mkv", "webm", "avi", "wmv", "flv", "ts", "mts", "m2ts", "3gp"];

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("test_fixtures")
        .join(name)
}

/// Owns the transport (play/pause/seek/underrun-count); never shared across
/// threads. `AudioEngine` itself can't be (it holds a `cpal::Stream`, which
/// is deliberately `!Send`/`!Sync` — see `playback::audio_engine`'s own
/// doc comment) — that's exactly why `clock_handle()` below hands out a
/// separate, genuinely thread-safe type instead.
enum Control {
    Audio(AudioEngine),
    Silent(SilentClock),
}

impl Control {
    fn play(&self) {
        match self {
            Control::Audio(a) => a.play(),
            Control::Silent(s) => s.play(),
        }
    }
    fn pause(&self) {
        match self {
            Control::Audio(a) => a.pause(),
            Control::Silent(s) => s.pause(),
        }
    }
    fn seek(&self, ticks: i64) {
        match self {
            Control::Audio(a) => a.seek(ticks),
            Control::Silent(s) => s.seek(ticks),
        }
    }
    fn current_tick(&self) -> i64 {
        match self {
            Control::Audio(a) => a.current_tick(),
            Control::Silent(s) => s.current_tick(),
        }
    }
    fn underrun_count(&self) -> u64 {
        match self {
            Control::Audio(a) => a.underrun_count(),
            Control::Silent(_) => 0,
        }
    }
    fn clock_handle(&self) -> ClockHandle {
        match self {
            Control::Audio(a) => ClockHandle::Audio(a.clock()),
            Control::Silent(s) => ClockHandle::Silent(s.handle()),
        }
    }
}

/// The `Send + Sync + Clone` read side, handed to each track's
/// `VideoPlayback` decode-ahead thread.
#[derive(Clone)]
enum ClockHandle {
    Audio(AudioClock),
    Silent(SilentClockHandle),
}

impl ClockHandle {
    fn current_tick(&self) -> i64 {
        match self {
            ClockHandle::Audio(c) => c.current_tick(),
            ClockHandle::Silent(c) => c.current_tick(),
        }
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
        transitions: vec![], gain_db: timeline::unity_gain(), pan: 0.0,
        locked: false,
        sync_locked: true,
        muted: false,
        solo: false,
        height_px: 60,
    }
}

/// A picked-and-probed source, plus the demo-only bookkeeping the render
/// loop needs per track.
struct TrackHandle {
    asset: media::MediaAsset,
    color: media::ColorMetadata,
    video: VideoPlayback,
    last_pts: Option<i64>,
}

/// Shows the native file picker; `None` means the user cancelled.
fn pick_video_files() -> Option<Vec<PathBuf>> {
    rfd::FileDialog::new()
        .set_title("Choose video file(s) to play (pick more than one to see them composited)")
        .add_filter("Video files", VIDEO_EXTENSIONS)
        .add_filter("All files", &["*"])
        .pick_files()
}

/// Probes every candidate path, keeping only files that actually have a
/// video stream and printing a reason for each one dropped — a bad pick
/// should be visible in the console, not a silent gap in the timeline.
fn probe_all(paths: &[PathBuf]) -> Vec<(PathBuf, media::MediaAsset)> {
    paths
        .iter()
        .filter_map(|p| match media_ffmpeg::probe(p) {
            Ok(asset) if asset.video.is_some() => Some((p.clone(), asset)),
            Ok(_) => {
                eprintln!("skipping {p:?}: no video stream");
                None
            }
            Err(e) => {
                eprintln!("skipping {p:?}: failed to probe ({e:?})");
                None
            }
        })
        .collect()
}

fn main() {
    media_ffmpeg::init().expect("ffmpeg init failed");

    let user_selected = pick_video_files().unwrap_or_default();
    let mut probed = probe_all(&user_selected);
    let using_fallback = probed.is_empty();
    if using_fallback {
        if user_selected.is_empty() {
            println!("No files selected — playing the built-in demo clips instead.");
        } else {
            println!("None of the selected files were usable — playing the built-in demo clips instead.");
        }
        probed = probe_all(&[fixture("test_playback_demo.mp4"), fixture("test_h264.mp4")]);
    }
    assert!(!probed.is_empty(), "even the built-in demo fixtures failed to probe — installation is broken");

    for (path, asset) in &probed {
        println!(
            "loaded: {} ({:.2}s, {}x{})",
            path.file_name().unwrap().to_string_lossy(),
            asset.duration_ticks as f64 / TIMEBASE as f64,
            asset.video.as_ref().unwrap().width,
            asset.video.as_ref().unwrap().height,
        );
    }

    // --- Sequence dimensions from the first (background) file ---
    let seq_w = probed[0].1.video.as_ref().unwrap().width;
    let seq_h = probed[0].1.video.as_ref().unwrap().height;

    // --- Pick an audio source: the first selected file that has one ---
    let audio_source = probed.iter().find(|(_, a)| a.audio.is_some()).map(|(p, _)| p.clone());
    if audio_source.is_none() {
        println!("none of the loaded files have an audio track — using a silent wall-clock instead of audio-clocked playback.");
    }

    // --- Timeline (M3) ---
    let mut tracks = Vec::new();
    for (i, (_, asset)) in probed.iter().enumerate() {
        let track_id = (i + 1) as u64;
        let effects = if using_fallback {
            // Preserve the original built-in demo's exact look.
            if i == 0 {
                vec![
                    effect_fx(
                        10,
                        color_correction::TYPE_ID,
                        vec![
                            (color_correction::TEMPERATURE, ParamValue::Number(0.25)),
                            (color_correction::CONTRAST, ParamValue::Number(0.1)),
                            (color_correction::SATURATION, ParamValue::Number(1.15)),
                        ],
                    ),
                    effect_fx(11, gaussian_blur::TYPE_ID, vec![(gaussian_blur::RADIUS, ParamValue::Number(1.5))]),
                ]
            } else {
                vec![transform_fx(vec![
                    (transform::SCALE, ParamValue::Vec2(0.45, 0.45)),
                    (transform::POSITION, ParamValue::Vec2(150.0, -80.0)),
                    (transform::ROTATION, ParamValue::Number(-8.0)),
                    (transform::OPACITY, ParamValue::Number(0.85)),
                ])]
            }
        } else if i == 0 {
            // The user's own background clip: play it clean, no forced grade.
            vec![]
        } else {
            // Cascade additional picks as picture-in-picture overlays so
            // they're all visible at once, rather than one full-frame clip
            // simply covering another with no visible compositing at all.
            let step = 60.0 * i as f64;
            vec![transform_fx(vec![
                (transform::SCALE, ParamValue::Vec2(0.4, 0.4)),
                (transform::POSITION, ParamValue::Vec2(-120.0 + step, -80.0 + step * 0.5)),
            ])]
        };
        tracks.push(video_track(track_id, &format!("V{}", i + 1), vec![clip(track_id, asset.id, asset.duration_ticks, effects)]));
    }

    let project = Arc::new(Project {
        sequences: vec![Sequence {
            id: SequenceId(1),
            name: if using_fallback { "Composite Demo".into() } else { "User Selection".into() },
            settings: SequenceSettings {
                frame_rate: FrameRate::Fps30,
                width: seq_w,
                height: seq_h,
                sample_rate: 48_000,
                working_color_primaries: media::ColorPrimaries::Rec709,
                drop_frame_timecode: false,
            },
            tracks,
            markers: vec![],
        }],
        assets: probed.iter().map(|(_, a)| a.clone()).collect(),
        bins: vec![],
    });
    let sequence_duration = project.sequences[0].duration();

    // --- Window + GPU ---
    let event_loop = EventLoop::new().unwrap();
    let title = if using_fallback {
        "nle-engine: built-in demo — Space=play/pause, arrows=seek".to_string()
    } else {
        let names: Vec<String> =
            probed.iter().map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        format!("nle-engine: {} — Space=play/pause, arrows=seek", names.join(" + "))
    };
    let window = Arc::new(
        WindowBuilder::new()
            .with_title(title)
            .with_inner_size(winit::dpi::LogicalSize::new(seq_w as f64, seq_h as f64))
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

    // --- Master clock (M2), audio-derived when possible ---
    //
    // Started *after* GPU/window setup on purpose: the clock begins
    // advancing the moment it starts, so starting it earlier meant playback
    // ran ahead during device init and the window opened already mid-clip.
    let control = match &audio_source {
        Some(path) => Control::Audio(AudioEngine::start(path.clone(), 0).expect("audio engine")),
        None => Control::Silent(SilentClock::new(0)),
    };
    let clock = control.clock_handle();
    let mut playing = true;

    // --- Video decode, paced against the clock (M2) ---
    //
    // Reusing `playback::VideoPlayback` rather than raw decode threads is
    // load-bearing, not just convenient: it already paces decoding against
    // the clock, drops frames when it falls behind, and handles seeks. An
    // earlier version of this demo spawned unpaced decode threads and, at
    // 166fps redraw against 30fps content, they raced ahead and decoded the
    // whole file — leaving ~150 GPU textures resident at once (~220MB at
    // 640x360, far worse at 4K). Pacing is what keeps that bounded.
    let mut track_handles: Vec<TrackHandle> = probed
        .into_iter()
        .map(|(path, asset)| {
            let color = asset.video.as_ref().unwrap().color;
            let video = VideoPlayback::start(path.clone(), {
                let c = clock.clone();
                move || c.current_tick()
            })
            .unwrap_or_else(|e| panic!("video playback failed for {path:?}: {e}"));
            TrackHandle { asset, color, video, last_pts: None }
        })
        .collect();

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
                                    control.play();
                                    for t in &track_handles {
                                        t.video.play();
                                    }
                                } else {
                                    control.pause();
                                    for t in &track_handles {
                                        t.video.pause();
                                    }
                                }
                                println!("{}", if playing { "playing" } else { "paused" });
                            }
                            PhysicalKey::Code(KeyCode::ArrowRight) | PhysicalKey::Code(KeyCode::ArrowLeft) => {
                                let delta = if key.physical_key == PhysicalKey::Code(KeyCode::ArrowRight) {
                                    TIMEBASE * 2
                                } else {
                                    -TIMEBASE * 2
                                };
                                let t = (control.current_tick() + delta).clamp(0, sequence_duration.0);
                                control.seek(t);
                                for track in &mut track_handles {
                                    track.video.seek(t);
                                    track.last_pts = None;
                                }
                                sources.clear();
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
                    for track in &mut track_handles {
                        if let Some(frame) = track.video.latest_frame() {
                            if track.last_pts != Some(frame.pts_ticks) {
                                let tex =
                                    compositor.upload_rgba(&frame.rgba, frame.width, frame.height, track.color);
                                sources.insert(track.asset.id, frame.pts_ticks, tex);
                                track.last_pts = Some(frame.pts_ticks);
                            }
                        }
                        sources.retain_from(track.asset.id, now_tick - TIMEBASE / 4);
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
                    let dropped: Vec<String> =
                        track_handles.iter().map(|t| t.video.dropped_frame_count().to_string()).collect();
                    println!(
                        "t={:.2}s | {} fps | layers/frame={} | textures held={} | underruns={} | dropped={}",
                        clock.current_tick() as f64 / TIMEBASE as f64,
                        frames_presented,
                        layers_last_frame,
                        sources.len(),
                        control.underrun_count(),
                        dropped.join("/"),
                    );
                    frames_presented = 0;
                    last_report = Instant::now();
                }
            }
            _ => {}
        })
        .unwrap();
}
