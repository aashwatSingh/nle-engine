//! The real editor shell: a timeline you can see and edit, a media bin, a
//! live preview, and an effects panel — built on top of the M0-M5 engine
//! (real decode, real GPU compositing, real effects, real undo/redo) rather
//! than a hardcoded demo. This is the "core editor UI" scope: not a
//! Premiere Pro clone (no multicam, Lumetri color wheels, audio mixer
//! console, motion graphics templates, or export yet) — see each module's
//! doc comment for what's deliberately deferred.
//!
//! UI is `egui` (immediate mode), rendered via `egui-wgpu` sharing the same
//! `wgpu::Device`/`Queue` as the engine's own `Compositor` — one GPU
//! context for both, no separate render loop to keep in sync.

mod autosave;
mod background;
mod cache_dirs;
mod effects_panel;
mod export_job;
mod home_screen;
mod icons;
mod matting_jobs;
mod mixer_panel;
mod proxy_jobs;
mod preview;
mod project_panel;
mod recent_projects;
mod scopes_panel;
mod state;
mod theme;
mod title_panel;
mod transcript_panel;
mod waveform_cache;
mod timeline_widget;

use render::wgpu;
use state::EditorState;
use std::sync::Arc;
use std::time::Instant;
use timeline::{TimeTick, TIMEBASE};
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::WindowBuilder;

/// Which top-level screen is showing. Starts on `Home` — landing straight in
/// a blank untitled editor with no memory of past projects is the thing
/// this exists to fix.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Editor,
}

/// Which panel is showing in the right-side dock. Previously all four
/// were stacked vertically, which meant scrolling past Effects and
/// Transcript to reach Scopes; this makes only one visible at a time.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RightPanelTab {
    Effects,
    Transcript,
    Scopes,
    Mixer,
}

struct RightPanelState {
    tab: RightPanelTab,
}

impl Default for RightPanelState {
    fn default() -> Self {
        RightPanelState { tab: RightPanelTab::Effects }
    }
}

fn main() {
    media_ffmpeg::init().expect("ffmpeg init failed");

    let event_loop = EventLoop::new().unwrap();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("nle-engine editor")
            .with_inner_size(winit::dpi::LogicalSize::new(1440.0, 900.0))
            .build(&event_loop)
            .unwrap(),
    );

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let surface = instance
        .create_surface(window.clone())
        .expect("create_surface");
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no GPU adapter");
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("editor"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
        },
        None,
    ))
    .expect("request_device");
    let (device, queue) = (Arc::new(device), Arc::new(queue));

    let caps = surface.get_capabilities(&adapter);
    // egui's own renderer handles sRGB-correctness for its widgets as long
    // as the surface format is reported honestly, so — unlike
    // `play_timeline`'s own compositor output — the swapchain format is
    // left as whatever wgpu prefers rather than forced non-sRGB here.
    let surface_format = caps.formats[0];
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

    let egui_ctx = egui::Context::default();
    theme::apply(&egui_ctx);
    let mut egui_winit_state = egui_winit::State::new(
        egui_ctx.clone(),
        egui::ViewportId::ROOT,
        &window,
        Some(window.scale_factor() as f32),
        None,
    );
    let mut egui_renderer = egui_wgpu::Renderer::new(&device, surface_format, None, 1);

    let mut state = EditorState::new();
    let mut preview = preview::Preview::new(device.clone(), queue.clone());
    let mut export = ExportUi::default();
    let mut project_panel_state = project_panel::ProjectPanelState::default();
    let mut effects_panel_state = effects_panel::EffectsPanelState::default();
    let mut scopes_panel_state = scopes_panel::ScopesPanelState::default();
    let mut right_panel_state = RightPanelState::default();
    let mut transcript_panel_state = transcript_panel::TranscriptPanelState::default();
    let mut waveforms = waveform_cache::WaveformCache::default();
    // Reclaim scratch directories from runs that never reached a clean exit
    // (a crash or a force-kill skips the cleanup below). Only touches
    // day-old directories that aren't ours — see `cache_dirs::sweep`.
    cache_dirs::sweep_temp();
    let mut proxies = proxy_jobs::ProxyJobs::default();
    let mut matting_jobs = matting_jobs::MattingJobs::default();
    let mut autosaver = autosave::Autosave::default();
    // Offered once, at startup, if the last session left a recovery file behind.
    let mut recovery_offer = autosave::Autosave::find_recovery(None);
    let mut screen = Screen::Home;
    let mut recent_projects = recent_projects::RecentProjects::load_default();
    // Owns the audio device only while playing (see `Transport`).
    let mut transport = Transport::default();
    // Tracked here rather than read from egui: shortcuts are dispatched from
    // the winit event branch, before egui runs for the frame, so egui's input
    // state would be one frame stale.
    let mut modifiers = winit::keyboard::ModifiersState::empty();
    // Set by WindowEvent::HoveredFile, cleared by HoveredFileCancelled or
    // DroppedFile — drives a border overlay in build_ui so dropping a
    // file has some visual feedback before it lands.
    let mut dragging_file = false;
    // Files dropped this frame, flushed together below. winit delivers one
    // DroppedFile event per file, so importing each as it arrives would make
    // a 10-file drop 10 separate undo steps; the Import button passes its
    // whole selection to `import_assets` at once and gets one. Batching here
    // makes the two paths behave the same.
    let mut pending_drops: Vec<std::path::PathBuf> = Vec::new();

    event_loop.set_control_flow(ControlFlow::Poll);
    event_loop
        .run(move |event, elwt| match event {
            Event::WindowEvent { event, .. } => {
                let response = egui_winit_state.on_window_event(&window, &event);
                if response.consumed {
                    if response.repaint {
                        window.request_redraw();
                    }
                    // Still let resize/close through even if egui "consumed" it.
                    if !matches!(event, WindowEvent::Resized(_) | WindowEvent::CloseRequested) {
                        return;
                    }
                }
                match event {
                    WindowEvent::CloseRequested => {
                        // Closing deliberately is a clean exit, so the recovery
                        // file must go — otherwise the next launch would offer
                        // to restore work the user chose to walk away from.
                        autosaver.discard(state.undo.revision() as usize);
                        // Proxies and mattes are derived data — regenerating
                        // them is cheap, keeping gigabytes of lossless QTRLE
                        // around forever is not.
                        if let Some(d) = proxies.scratch_dir() {
                            cache_dirs::remove(d);
                        }
                        if let Some(d) = matting_jobs.scratch_dir() {
                            cache_dirs::remove(d);
                        }
                        elwt.exit()
                    }
                    WindowEvent::Resized(new_size) => {
                        if new_size.width > 0 && new_size.height > 0 {
                            config.width = new_size.width;
                            config.height = new_size.height;
                            surface.configure(&device, &config);
                        }
                    }
                    WindowEvent::ModifiersChanged(new) => modifiers = new.state(),
                    WindowEvent::HoveredFile(_) => {
                        dragging_file = true;
                    }
                    WindowEvent::HoveredFileCancelled => {
                        dragging_file = false;
                    }
                    WindowEvent::DroppedFile(path) => {
                        dragging_file = false;
                        // Screen is checked here, not at flush time: the drop
                        // happened against whatever was on screen when the user
                        // released. No project is open on the Home screen, so
                        // there's nothing to import into.
                        if screen == Screen::Editor {
                            pending_drops.push(path);
                        }
                    }
                    WindowEvent::KeyboardInput { event: key, .. } => {
                        if key.state == ElementState::Pressed {
                            handle_shortcut(
                                &mut state,
                                &mut transport,
                                &proxies,
                                &matting_jobs,
                                &device,
                                &queue,
                                &key.logical_key,
                                &modifiers,
                                key.repeat,
                            );
                        }
                    }
                    WindowEvent::RedrawRequested => {
                        advance_playhead(&mut state, &mut transport);
                        // Collect any waveforms that finished on a worker
                        // thread. Without this the results are never taken off
                        // the channel and no waveform ever appears.
                        waveforms.poll();
                        // Same requirement as waveforms: without this, finished
                        // proxies sit on the channel and are never adopted.
                        proxies.poll();
                        matting_jobs.poll();
                        // Scene cuts, silence, captions and the rest: their
                        // results are edits, applied here on the UI thread.
                        state.poll_analysis();
                        // One import, one undo step, however many files landed.
                        if !pending_drops.is_empty() {
                            state.import_assets(std::mem::take(&mut pending_drops));
                        }

                        let raw_input = egui_winit_state.take_egui_input(&window);
                        let full_output = egui_ctx.run(raw_input, |ctx| {
                            build_ui(
                                ctx,
                                &mut state,
                                &device,
                                &device,
                                &queue,
                                &mut egui_renderer,
                                &mut preview,
                                &mut export,
                                &mut project_panel_state,
                                &mut effects_panel_state,
                                &mut scopes_panel_state,
                                &mut right_panel_state,
                                &mut transcript_panel_state,
                                &mut waveforms,
                                &mut proxies,
                                &mut matting_jobs,
                                &mut autosaver,
                                &mut recovery_offer,
                                &mut transport,
                                &mut screen,
                                &mut recent_projects,
                                dragging_file,
                            )
                        });
                        egui_winit_state
                            .handle_platform_output(&window, full_output.platform_output);

                        state.force_close_stale_drag(egui_ctx.input(|i| i.pointer.any_down()));

                        // Reconcile the playback devices to the transport's
                        // intent. `state.playing` is the single source of
                        // truth, and several places clear it without knowing
                        // the devices exist — scrubbing the ruler, for one.
                        // Without this the picture would freeze while sound
                        // kept playing, and the decode thread would keep
                        // running against a clock nobody advances.
                        if !state.playing && transport.is_running() {
                            stop_playback(&mut state, &mut transport);
                        }

                        tick_autosave(&mut state, &mut autosaver);

                        let tris =
                            egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
                        for (id, delta) in &full_output.textures_delta.set {
                            egui_renderer.update_texture(&device, &queue, *id, delta);
                        }
                        let screen_descriptor = egui_wgpu::ScreenDescriptor {
                            size_in_pixels: [config.width, config.height],
                            pixels_per_point: full_output.pixels_per_point,
                        };
                        let mut encoder =
                            device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("editor frame"),
                            });
                        egui_renderer.update_buffers(
                            &device,
                            &queue,
                            &mut encoder,
                            &tris,
                            &screen_descriptor,
                        );

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
                        let view = output
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());
                        {
                            let mut rpass =
                                encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                    label: Some("egui"),
                                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                        view: &view,
                                        resolve_target: None,
                                        ops: wgpu::Operations {
                                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                                r: 0.08,
                                                g: 0.08,
                                                b: 0.09,
                                                a: 1.0,
                                            }),
                                            store: wgpu::StoreOp::Store,
                                        },
                                    })],
                                    depth_stencil_attachment: None,
                                    timestamp_writes: None,
                                    occlusion_query_set: None,
                                });
                            egui_renderer.render(&mut rpass, &tris, &screen_descriptor);
                        }
                        for id in &full_output.textures_delta.free {
                            egui_renderer.free_texture(id);
                        }
                        queue.submit(std::iter::once(encoder.finish()));
                        output.present();
                    }
                    _ => {}
                }
            }
            Event::AboutToWait => window.request_redraw(),
            _ => {}
        })
        .unwrap();
}

/// Owns the playback devices while playing: the audio engine, which is the
/// master clock, and the video decode-ahead pipeline that paces against it.
///
/// Both are `None` when stopped. `audio` alone is `None` when no output device
/// could be opened, in which case the wall clock stands in (see
/// `state.play_anchor`) and video paces against that instead — so picture
/// still plays on a machine with no working sound output.
#[derive(Default)]
struct Transport {
    audio: Option<playback::SequenceAudioEngine>,
    video: Option<playback::SequenceVideoPlayback>,
}

impl Transport {
    fn is_running(&self) -> bool {
        self.audio.is_some() || self.video.is_some()
    }
}

/// Starts playback from the current playhead.
///
/// The audio engine is the master clock (spec 4.4), so it's created even for a
/// sequence with no audio clips: the mix is then silence but the clock still
/// advances correctly, which keeps one code path driving the transport instead
/// of two that can disagree. The project is snapshotted by `Arc` here — see
/// `playback::sequence_audio`'s note on snapshot semantics.
/// Which file each engine should read during playback.
///
/// Video takes every substitution: a proxy for scrub performance, with
/// matting layered on top so a background-removed asset wins where both exist.
///
/// Audio takes none of them. `media_ffmpeg::generate_proxy` writes a
/// video-only file by design, so handing a proxy to the audio engine makes
/// `AudioDecoderStream::open` fail and the clip decode as permanent silence
/// for the rest of the session. Neither substitution ever changes what the
/// audio should be, so the original is both the simplest and the correct
/// answer.
fn playback_paths(
    originals: &std::collections::HashMap<media::MediaAssetId, std::path::PathBuf>,
    proxies: &proxy_jobs::ProxyJobs,
    matting_jobs: &matting_jobs::MattingJobs,
) -> (
    std::collections::HashMap<media::MediaAssetId, std::path::PathBuf>,
    std::collections::HashMap<media::MediaAssetId, std::path::PathBuf>,
) {
    let video = matting_jobs.resolve(&proxies.resolve(originals));
    (video, originals.clone())
}

fn start_playback(
    state: &mut EditorState,
    transport: &mut Transport,
    proxies: &proxy_jobs::ProxyJobs,
    matting_jobs: &matting_jobs::MattingJobs,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
) {
    state.playing = true;
    state.shuttle_rate = 1.0;
    let project = state.project().clone();
    let (video_paths, audio_paths) = playback_paths(&state.asset_paths, proxies, matting_jobs);

    // Audio first: video needs a handle to whatever clock ends up authoritative.
    let audio = match playback::SequenceAudioEngine::start(
        project.clone(),
        state.seq_id,
        audio_paths,
        state.playhead,
        state.master_gain_db,
    ) {
        Ok(engine) => {
            state.play_anchor = None;
            Some(engine)
        }
        Err(e) => {
            // No audio device (or it's in use). Fall back to the wall clock so
            // the editor is still usable, and say so rather than looking broken.
            state.play_anchor = Some((Instant::now(), state.playhead));
            state.status = format!("no audio output ({e}); playing without sound");
            None
        }
    };

    // The decode thread reads the clock to decide what to prepare next and
    // when to skip. Boxed so both clock sources have one type; `Box<F: Fn>`
    // is itself `Fn`, so it satisfies the engine's bound directly.
    let clock: Box<dyn Fn() -> i64 + Send + 'static> = match audio.as_ref() {
        Some(engine) => {
            let clock = engine.clock();
            Box::new(move || clock.current_tick())
        }
        None => {
            let (anchor_at, anchor_tick) = (Instant::now(), state.playhead);
            Box::new(move || {
                anchor_tick + (anchor_at.elapsed().as_secs_f64() * TIMEBASE as f64) as i64
            })
        }
    };

    let video = playback::SequenceVideoPlayback::start(
        project,
        state.seq_id,
        video_paths,
        state.playhead,
        clock,
        // Uploads decoded frames to GPU textures on the decode thread itself
        // instead of handing back CPU buffers for the UI thread to upload —
        // see `playback::sequence_video`'s module doc for the real-footage
        // measurement (334 starved frames at 2560x1588 before this) that
        // motivated it.
        Some((device.clone(), queue.clone())),
    );

    *transport = Transport { audio, video: Some(video) };
}

fn stop_playback(state: &mut EditorState, transport: &mut Transport) {
    state.playing = false;
    state.play_anchor = None;
    state.shuttle_rate = 0.0;
    state.playback_health.clear();
    // Dropping the engines closes the audio stream (letting the mixer thread
    // exit as its command channel disconnects) and joins the video decode
    // thread, releasing the device and the per-asset decoders.
    *transport = Transport::default();
}

fn advance_playhead(state: &mut EditorState, transport: &mut Transport) {
    // Shuttle at anything other than 1x isn't "playing": no audio device, no
    // decode-ahead pipeline, just the playhead moving at a multiple of wall
    // time with the preview rendering on demand. See `set_shuttle`.
    if !state.playing && state.shuttle_rate != 0.0 {
        advance_shuttle(state);
        return;
    }
    if !state.playing {
        return;
    }
    let end = state.sequence_duration_ticks();

    // Report playback health while it's happening. Dropped frames mean the
    // decoder had to skip to keep up with the clock; starvation means the UI
    // asked for a frame and none was ready. Either one is "this machine can't
    // play this sequence at full rate" — the thing proxies exist to fix.
    if let Some(video) = transport.video.as_ref() {
        let dropped = video.dropped_frame_count();
        let starved = video.starved_count();
        state.playback_health = if dropped == 0 && starved == 0 {
            String::new()
        } else {
            format!("dropped {dropped}, held {starved}")
        };
    }

    if let Some(engine) = transport.audio.as_ref() {
        // Audio-mastered: the playhead is where the hardware actually is.
        state.playhead = engine.current_tick().min(end);
        if engine.has_ended() || state.playhead >= end {
            state.playhead = end;
            stop_playback(state, transport);
        }
        return;
    }

    // Wall-clock fallback (no audio device).
    let Some((anchor_instant, anchor_tick)) = state.play_anchor else {
        return;
    };
    let elapsed = anchor_instant.elapsed().as_secs_f64();
    let new_tick = anchor_tick + (elapsed * TIMEBASE as f64) as i64;
    if new_tick >= end {
        state.playhead = end;
        stop_playback(state, transport);
    } else {
        state.playhead = new_tick;
    }
}

/// Advances the playhead at `shuttle_rate` x wall time, in either direction,
/// stopping at either end of the sequence.
fn advance_shuttle(state: &mut EditorState) {
    let Some((anchor_at, anchor_tick)) = state.play_anchor else {
        // No anchor means nothing is actually shuttling; clear the rate so the
        // toolbar doesn't claim otherwise.
        state.shuttle_rate = 0.0;
        return;
    };
    let elapsed = anchor_at.elapsed().as_secs_f64();
    let target = anchor_tick + (elapsed * state.shuttle_rate * TIMEBASE as f64) as i64;
    let end = state.sequence_duration_ticks();

    if target <= 0 || target >= end {
        state.playhead = target.clamp(0, end);
        state.shuttle_rate = 0.0;
        state.play_anchor = None;
    } else {
        state.playhead = target;
    }
}

/// Highest shuttle multiplier J/L will step up to. Beyond 8x the on-demand
/// decode path can't produce enough distinct frames for the motion to read as
/// anything but noise, so more speed would be a worse tool, not a faster one.
const MAX_SHUTTLE: f64 = 8.0;

/// Sets the transport rate for JKL shuttle.
///
/// Only `1.0` gets real playback. Every other rate deliberately runs silent,
/// off the wall clock and the on-demand preview path, because two engine
/// limits make a "fast/reverse A/V playback" claim untrue: the mixer has no
/// resampler (so it can only produce 1x audio), and the video decode pipeline
/// only walks forward. Rather than fake it, shuttle drops to scrub-quality
/// picture and says "silent" in the toolbar. Premiere also drops audio at
/// extreme shuttle speeds; it pitches it at moderate ones, which is the part
/// that needs the resampler.
fn set_shuttle(
    state: &mut EditorState,
    transport: &mut Transport,
    proxies: &proxy_jobs::ProxyJobs,
    matting_jobs: &matting_jobs::MattingJobs,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    rate: f64,
) {
    if rate == 1.0 {
        start_playback(state, transport, proxies, matting_jobs, device, queue);
        return;
    }
    // Tear down the A/V engines first — `stop_playback` also zeroes the rate,
    // so the new one is set after.
    stop_playback(state, transport);
    state.shuttle_rate = rate;
    if rate != 0.0 {
        state.play_anchor = Some((Instant::now(), state.playhead));
    }
}

/// Next rate for an L (forward) or J (reverse) press: engage at 1x in that
/// direction, then double on each further press, as in Premiere/Avid.
fn next_shuttle_rate(current: f64, forward: bool) -> f64 {
    let want: f64 = if forward { 1.0 } else { -1.0 };
    if current.signum() != want.signum() || current == 0.0 {
        want
    } else {
        (current * 2.0).clamp(-MAX_SHUTTLE, MAX_SHUTTLE)
    }
}

fn ticks_per_frame(state: &EditorState) -> i64 {
    state.sequence().settings.frame_rate.ticks_per_frame().max(1)
}

/// Moves the playhead by `frames`, stopping any transport first — stepping
/// while the audio clock is driving the playhead would just be overwritten on
/// the next frame.
fn step_frames(
    state: &mut EditorState,
    transport: &mut Transport,
    proxies: &proxy_jobs::ProxyJobs,
    matting_jobs: &matting_jobs::MattingJobs,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    frames: i64,
) {
    if state.playing || state.shuttle_rate != 0.0 {
        set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
    }
    let delta = frames * ticks_per_frame(state);
    let end = state.sequence_duration_ticks();
    state.playhead = (state.playhead + delta).clamp(0, end);
}

/// Every clip edge in the sequence, which is what "go to previous/next edit
/// point" navigates between.
fn edit_points(state: &EditorState) -> Vec<i64> {
    let mut points: Vec<i64> = vec![0];
    for track in &state.sequence().tracks {
        for clip in &track.clips {
            points.push(clip.timeline_in.0);
            points.push(clip.timeline_out.0);
        }
    }
    points.sort_unstable();
    points.dedup();
    points
}

// Six of these nine are the same transport context that `start_playback`,
// `set_shuttle` and `step_frames` also take. Bundling them into one struct
// is the obvious answer, but this function's ten internal calls hand that
// context straight back to those functions, so a struct would have to be
// reborrowed at every one — more ceremony than the parameter list costs.
#[allow(clippy::too_many_arguments)]
fn handle_shortcut(
    state: &mut EditorState,
    transport: &mut Transport,
    proxies: &proxy_jobs::ProxyJobs,
    matting_jobs: &matting_jobs::MattingJobs,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    key: &Key,
    modifiers: &winit::keyboard::ModifiersState,
    repeat: bool,
) {
    let ctrl = modifiers.control_key() || modifiers.super_key();
    let shift = modifiers.shift_key();

    // Held-key repeat is wanted for navigation (holding an arrow to walk the
    // playhead) but not for anything that mutates the project — an autorepeated
    // delete or paste would fire dozens of times from one keypress.
    let navigation_only = repeat;

    match key {
        Key::Named(NamedKey::Space) if !navigation_only => {
            if state.playing || state.shuttle_rate != 0.0 {
                set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
            } else {
                set_shuttle(state, transport, proxies, matting_jobs, device, queue, 1.0);
            }
        }

        // --- JKL shuttle ---
        Key::Character(c) if c.eq_ignore_ascii_case("l") && !navigation_only => {
            let rate = next_shuttle_rate(state.shuttle_rate, true);
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, rate);
        }
        Key::Character(c) if c.eq_ignore_ascii_case("j") && !navigation_only => {
            let rate = next_shuttle_rate(state.shuttle_rate, false);
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, rate);
        }
        Key::Character(c) if c.eq_ignore_ascii_case("k") && !navigation_only => {
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
        }

        // --- Navigation (repeat allowed) ---
        Key::Named(NamedKey::ArrowLeft) => {
            step_frames(state, transport, proxies, matting_jobs, device, queue, if shift { -5 } else { -1 })
        }
        Key::Named(NamedKey::ArrowRight) => {
            step_frames(state, transport, proxies, matting_jobs, device, queue, if shift { 5 } else { 1 })
        }
        Key::Named(NamedKey::Home) if !navigation_only => {
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
            state.playhead = 0;
        }
        Key::Named(NamedKey::End) if !navigation_only => {
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
            state.playhead = state.sequence_duration_ticks();
        }
        Key::Named(NamedKey::ArrowUp) => {
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
            if let Some(p) = edit_points(state).into_iter().rev().find(|p| *p < state.playhead) {
                state.playhead = p;
            }
        }
        Key::Named(NamedKey::ArrowDown) => {
            set_shuttle(state, transport, proxies, matting_jobs, device, queue, 0.0);
            if let Some(p) = edit_points(state).into_iter().find(|p| *p > state.playhead) {
                state.playhead = p;
            }
        }

        // --- Marks ---
        Key::Character(c) if c.eq_ignore_ascii_case("i") && !navigation_only => state.mark_in(),
        Key::Character(c) if c.eq_ignore_ascii_case("o") && !navigation_only => state.mark_out(),

        // --- Edit ---
        // Shift+Delete is ripple delete (close the gap); plain Delete lifts
        // (leave a gap). Premiere's convention, and the distinction matters
        // enough that guessing one would be wrong half the time.
        Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) if !navigation_only => {
            if shift {
                state.ripple_delete_selection();
            } else {
                state.lift_selection();
            }
        }
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("c") && !navigation_only => {
            state.copy_selection()
        }
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("x") && !navigation_only => {
            state.copy_selection();
            state.ripple_delete_selection();
        }
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("v") && !navigation_only => {
            state.paste_at_playhead()
        }
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("a") && !navigation_only => {
            state.selected_clips = state
                .sequence()
                .tracks
                .iter()
                .flat_map(|t| t.clips.iter().map(|c| c.id))
                .collect();
        }
        Key::Named(NamedKey::Escape) if !navigation_only => state.clear_selection(),

        // --- Undo/redo. Ctrl-qualified, so bare V/C stay available as tools.
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("z") && !navigation_only => {
            if shift {
                state.undo.redo();
            } else {
                state.undo.undo();
            }
        }
        Key::Character(c) if ctrl && c.eq_ignore_ascii_case("y") && !navigation_only => {
            state.undo.redo();
        }

        // --- Tools and toggles (bare keys, so they must come after the
        // Ctrl-qualified arms above — `c` is both Razor and Copy.
        Key::Character(c) if c.eq_ignore_ascii_case("v") && !navigation_only => {
            state.tool = state::Tool::Select
        }
        Key::Character(c) if c.eq_ignore_ascii_case("c") && !navigation_only => {
            state.tool = state::Tool::Razor
        }
        Key::Character(c) if c.eq_ignore_ascii_case("s") && !navigation_only => {
            state.snapping = !state.snapping;
            state.status = if state.snapping { "snapping on".into() } else { "snapping off".into() };
        }
        _ => {}
    }
}

const PROJECT_EXTENSION: &str = "nleproj";

/// The export-related slice of UI state: at most one job at a time (a second
/// concurrent export would contend for the same GPU and encoder throughput
/// and just make both slower), plus the last outcome to show afterwards.
struct ExportUi {
    job: Option<export_job::ExportJob>,
    last_result: Option<String>,
    quality: export::QualityPreset,
    /// Export only the marked in/out range. Sticky across exports, since a
    /// range workflow tends to be several exports in a row.
    use_range: bool,
    output_scale: export::OutputScale,
    /// Whether the pre-export options window (resolution/quality/range)
    /// is open. Set by the File menu's "Export..." item; cleared either
    /// by the window's own close button or by successfully starting a
    /// job in `export_dialog_ui`.
    show_dialog: bool,
}

impl Default for ExportUi {
    fn default() -> Self {
        ExportUi {
            job: None,
            last_result: None,
            quality: export::QualityPreset::High,
            use_range: false,
            output_scale: export::OutputScale::Native,
            show_dialog: false,
        }
    }
}

/// The delivery figures, appended to the export's result line. Empty when the
/// export had no audio — see `ExportStats::loudness`.
///
/// The dBTP figure carries a warning above -1.0: most delivery specs cap true
/// peak at -1 dBTP, and a file over it is the kind of thing that comes back
/// rejected after the fact. Stated rather than silently included, because the
/// number alone means nothing to someone who hasn't memorised the spec.
fn loudness_summary(stats: &export::ExportStats) -> String {
    let Some(l) = stats.loudness else { return String::new() };
    let warn = if l.true_peak_dbtp > -1.0 { "  (above the usual -1 dBTP delivery limit)" } else { "" };
    format!(
        " — {:.1} LUFS integrated, {:.1} dBTP true peak, {:.1} LU range{warn}",
        l.integrated_lufs, l.true_peak_dbtp, l.loudness_range_lu
    )
}

fn start_export(state: &mut EditorState, export: &mut ExportUi, transport: &mut Transport, matting_jobs: &matting_jobs::MattingJobs) {
    if export.job.is_some() {
        return; // already exporting
    }
    if state.sequence_duration_ticks() <= 0 {
        state.status = "nothing to export — the timeline is empty".into();
        return;
    }
    // Only honour the range when there actually is one; "export range" with no
    // marks set should not silently export nothing.
    let range_ticks = if export.use_range {
        match state.marked_range() {
            Some(r) => Some(r),
            None => {
                state.status = "no in/out range marked — set I and O first, or untick range".into();
                return;
            }
        }
    } else {
        None
    };
    let default_name = state
        .project_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|s| format!("{}.mp4", s.to_string_lossy()))
        .unwrap_or_else(|| "export.mp4".into());
    let Some(output) = rfd::FileDialog::new()
        .set_title("Export video")
        .add_filter("MP4 video", &["mp4"])
        .set_file_name(default_name)
        .save_file()
    else {
        return;
    };

    // Playback and export both drive the same decoders; stopping playback
    // first keeps the export from competing with it for decode throughput,
    // and frees the audio device.
    stop_playback(state, transport);
    export.last_result = None;
    export.job = Some(export_job::ExportJob::start(
        state.project().clone(),
        state.seq_id,
        // Deliberately NOT proxy-resolved: export delivers from the originals.
        // See `proxy_jobs`' module doc. Matting *is* applied here, unlike
        // proxying — background removal is the user's actual edit, not a
        // performance shortcut, so it belongs in the delivered file.
        matting_jobs.resolve(&state.asset_paths),
        output,
        export::ExportOptions {
            quality: export.quality,
            range_ticks,
            output_scale: export.output_scale,
            ..Default::default()
        },
    ));
}

/// The pre-export options window: resolution, quality, range. Opened
/// from File > "Export...". Confirming it closes the dialog and hands
/// off to `start_export`, which owns the actual save-file dialog and job
/// creation — unchanged from before this task existed.
fn export_dialog_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    export: &mut ExportUi,
    transport: &mut Transport,
    matting_jobs: &matting_jobs::MattingJobs,
) {
    if !export.show_dialog {
        return;
    }
    let mut open = true;
    egui::Window::new("Export")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label("Resolution:");
            for scale in export::OutputScale::ALL {
                ui.radio_value(&mut export.output_scale, scale, scale.label());
            }
            ui.separator();
            ui.label("Quality:");
            for preset in export::QualityPreset::ALL {
                ui.radio_value(&mut export.quality, preset, preset.label());
            }
            ui.separator();
            let has_range = state.marked_range().is_some();
            ui.add_enabled(
                has_range,
                egui::Checkbox::new(&mut export.use_range, "Only the in/out range"),
            )
            .on_disabled_hover_text("mark in and out on the timeline first (I and O)");
            if !has_range {
                export.use_range = false;
            }
            ui.separator();
            if ui.button("Export...").clicked() {
                export.show_dialog = false;
                start_export(state, export, transport, matting_jobs);
            }
        });
    if !open {
        export.show_dialog = false;
    }
}

/// Progress window while an export runs, plus a one-shot result line after.
/// Shown as a real modal-ish window rather than a status string because an
/// export takes long enough that "did I actually start it?" is a real
/// question, and because it needs somewhere to put Cancel.
fn export_ui(ctx: &egui::Context, export: &mut ExportUi) {
    let mut finished = false;
    if let Some(job) = &export.job {
        if let Some(result) = job.take_result() {
            export.last_result = Some(match result {
                Ok(stats) if stats.frames_with_missing_sources > 0 => format!(
                    "exported {} frames to {} — {} frame(s) had missing media and rendered as gaps{}",
                    stats.frames_written,
                    job.output.display(),
                    stats.frames_with_missing_sources,
                    loudness_summary(&stats)
                ),
                Ok(stats) => format!(
                    "exported {} frames ({}x{}) to {}{}",
                    stats.frames_written,
                    stats.width,
                    stats.height,
                    job.output.display(),
                    loudness_summary(&stats)
                ),
                Err(e) => format!("export failed: {e}"),
            });
            finished = true;
        } else {
            egui::Window::new("Exporting")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    match job.fraction() {
                        Some(f) => {
                            ui.add(egui::ProgressBar::new(f).show_percentage());
                            ui.label(format!(
                                "frame {} / {}",
                                job.frames_done(),
                                job.total_frames()
                            ));
                        }
                        None => {
                            ui.spinner();
                            ui.label("preparing…");
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        job.request_cancel();
                    }
                });
            // The worker thread doesn't wake the UI, so keep repainting
            // while it runs or the progress bar would sit frozen until the
            // user happened to move the mouse.
            ctx.request_repaint();
        }
    }
    if finished {
        export.job = None;
    }
}

fn open_project(state: &mut EditorState, recent_projects: &mut recent_projects::RecentProjects, screen: &mut Screen) {
    if let Some(path) = rfd::FileDialog::new()
        .set_title("Open project")
        .add_filter("nle-engine project", &[PROJECT_EXTENSION])
        .pick_file()
    {
        open_project_path(state, recent_projects, screen, &path);
    }
}

/// Shared by the File menu's dialog-driven open and the home screen's
/// click-a-recent-project path, so both go through the same "did it
/// actually load" check and the same recent-list bookkeeping.
fn open_project_path(
    state: &mut EditorState,
    recent_projects: &mut recent_projects::RecentProjects,
    screen: &mut Screen,
    path: &std::path::Path,
) {
    if state.open_from(path) {
        recent_projects.record(path);
        *screen = Screen::Editor;
    }
}

/// `force_dialog` is Save As; plain Save reuses the known path and only
/// prompts when there isn't one yet.
fn save_project(
    state: &mut EditorState,
    autosaver: &mut autosave::Autosave,
    recent_projects: &mut recent_projects::RecentProjects,
    force_dialog: bool,
) {
    let existing = state.project_path.clone();
    let path = match (&existing, force_dialog) {
        (Some(p), false) => Some(p.clone()),
        _ => rfd::FileDialog::new()
            .set_title("Save project")
            .add_filter("nle-engine project", &[PROJECT_EXTENSION])
            .set_file_name(
                existing
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| format!("untitled.{PROJECT_EXTENSION}")),
            )
            .save_file(),
    };
    if let Some(path) = path {
        state.save_to(&path);
        recent_projects.record(&path);
        // The user's own file now holds this work, so the recovery file would
        // only produce a spurious "restore?" prompt on the next launch.
        autosaver.discard(state.undo.revision() as usize);
    }
}

// The immediate-mode UI root: every panel's state has to reach it somehow,
// and it hands ten separate egui closures their own disjoint `&mut`
// borrows. Collapsing these into one context struct would make those
// closures borrow the whole struct instead of individual fields, which is
// precisely the conflict the split parameters avoid.
#[allow(clippy::too_many_arguments)]
fn build_ui(
    ctx: &egui::Context,
    state: &mut EditorState,
    device: &wgpu::Device,
    // Arc handles, kept separate from the deref'd `device: &wgpu::Device`
    // above (which every existing rendering call site here already expects):
    // starting playback needs to *own* a clone of each to hand to
    // `SequenceVideoPlayback`'s decode thread, which a plain `&wgpu::Device`
    // can't provide.
    device_arc: &Arc<wgpu::Device>,
    queue_arc: &Arc<wgpu::Queue>,
    egui_renderer: &mut egui_wgpu::Renderer,
    preview: &mut preview::Preview,
    export: &mut ExportUi,
    project_panel_state: &mut project_panel::ProjectPanelState,
    effects_panel_state: &mut effects_panel::EffectsPanelState,
    scopes_panel_state: &mut scopes_panel::ScopesPanelState,
    right_panel_state: &mut RightPanelState,
    transcript_panel_state: &mut transcript_panel::TranscriptPanelState,
    waveforms: &mut waveform_cache::WaveformCache,
    proxies: &mut proxy_jobs::ProxyJobs,
    matting_jobs: &mut matting_jobs::MattingJobs,
    autosaver: &mut autosave::Autosave,
    recovery_offer: &mut Option<std::path::PathBuf>,
    transport: &mut Transport,
    screen: &mut Screen,
    recent_projects: &mut recent_projects::RecentProjects,
    dragging_file: bool,
) {
    recovery_prompt(ctx, state, autosaver, recovery_offer, screen);

    if *screen == Screen::Home {
        egui::CentralPanel::default().show(ctx, |ui| {
            match home_screen::show(ui, recent_projects.entries()) {
                home_screen::HomeAction::None => {}
                home_screen::HomeAction::NewProject => {
                    *state = EditorState::new();
                    *screen = Screen::Editor;
                }
                home_screen::HomeAction::OpenDialog => {
                    open_project(state, recent_projects, screen);
                }
                home_screen::HomeAction::OpenPath(path) => {
                    open_project_path(state, recent_projects, screen, &path);
                }
            }
        });
        return;
    }

    if dragging_file {
        let screen_rect = ctx.screen_rect();
        ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drag_overlay")))
            .rect_stroke(
                screen_rect.shrink(3.0),
                0.0,
                egui::Stroke::new(4.0f32, egui::Color32::from_rgb(230, 200, 120)),
            );
    }

    egui::TopBottomPanel::top("menu_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.menu_button("File", |ui| {
                if ui.button("Open...").clicked() {
                    ui.close_menu();
                    open_project(state, recent_projects, screen);
                }
                if ui.button("Save").clicked() {
                    ui.close_menu();
                    save_project(state, autosaver, recent_projects, false);
                }
                if ui.button("Save As...").clicked() {
                    ui.close_menu();
                    save_project(state, autosaver, recent_projects, true);
                }
                ui.separator();
                if ui
                    .button("Back to Home")
                    .on_hover_text("close this project and return to the project list — unsaved work is still protected by autosave")
                    .clicked()
                {
                    ui.close_menu();
                    *screen = Screen::Home;
                }
                ui.separator();
                if ui
                    .add_enabled(export.job.is_none(), egui::Button::new("Export..."))
                    .clicked()
                {
                    ui.close_menu();
                    export.show_dialog = true;
                }
            });
            ui.separator();
            if icons::icon_button(ui, icons::Icon::AddTitle, "Add Title")
                .on_hover_text("insert a text title at the playhead on the topmost video track")
                .clicked()
            {
                state.add_title_at_playhead(TimeTick(TIMEBASE * 3));
            }
            ui.separator();
            if icons::icon_button(ui, icons::Icon::Undo, "Undo").on_hover_text("Z").clicked() {
                state.undo.undo();
            }
            if icons::icon_button(ui, icons::Icon::Redo, "Redo").on_hover_text("Y").clicked() {
                state.undo.redo();
            }
            ui.separator();
            let (play_icon, play_label) =
                if state.playing { (icons::Icon::Pause, "Pause") } else { (icons::Icon::Play, "Play") };
            if icons::icon_button(ui, play_icon, play_label).on_hover_text("Space").clicked() {
                if state.playing {
                    stop_playback(state, transport);
                } else {
                    start_playback(state, transport, proxies, matting_jobs, device_arc, queue_arc);
                }
            }
            ui.separator();
            ui.label(
                state
                    .project_path
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "untitled".into()),
            );
            if let Some(msg) = &export.last_result {
                ui.separator();
                let failed = msg.starts_with("export failed");
                ui.colored_label(
                    if failed {
                        egui::Color32::LIGHT_RED
                    } else {
                        egui::Color32::LIGHT_GREEN
                    },
                    msg,
                );
            }
        });
    });

    export_dialog_ui(ctx, state, export, transport, matting_jobs);
    export_ui(ctx, export);

    egui::SidePanel::left("project_panel")
        .resizable(true)
        .default_width(400.0)
        .show(ctx, |ui| {
            project_panel::show(ui, state, project_panel_state, proxies);
        });

    egui::SidePanel::right("effects")
        .resizable(true)
        .default_width(300.0)
        .show(ctx, |ui| {
            // Above the tabs because a title's own text is what you came
            // to the panel for; effects applied *to* the title are the
            // secondary concern. Draws nothing when no title is selected.
            title_panel::show(ui, state);
            ui.separator();
            // Wrapping rather than plain horizontal because the panel is
            // resizable down to ~96px and SidePanel clips overflow -- a
            // clipped tab would be both invisible and unclickable, and
            // these tabs are the only route to their panels.
            ui.horizontal_wrapped(|ui| {
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Effects, "Effects");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Transcript, "Transcript");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Scopes, "Scopes");
                ui.selectable_value(&mut right_panel_state.tab, RightPanelTab::Mixer, "Mixer");
            });
            ui.separator();
            match right_panel_state.tab {
                RightPanelTab::Effects => {
                    effects_panel::show(ui, state, effects_panel_state, matting_jobs);
                }
                RightPanelTab::Transcript => {
                    transcript_panel::show(ui, state, transcript_panel_state);
                }
                RightPanelTab::Scopes => {
                    // Selecting the tab is now the intent signal that this
                    // checkbox used to carry, back when the panel was
                    // always-present in a vertical stack. Force it open so
                    // the tab doesn't land on a blank panel with an
                    // unticked checkbox.
                    scopes_panel_state.open = true;
                    scopes_panel::show(ui, scopes_panel_state, preview, device, queue_arc);
                }
                RightPanelTab::Mixer => {
                    let snapshot = transport.audio.as_ref().and_then(|a| a.meters());
                    mixer_panel::show(ui, state, snapshot.as_ref());
                }
            }
        });

    egui::TopBottomPanel::bottom("timeline")
        .resizable(true)
        .default_height(320.0)
        .show(ctx, |ui| {
            egui::ScrollArea::both().show(ui, |ui| {
                timeline_widget::show(ui, state, waveforms);
            });
        });

    // Keep painting while waveforms are still being computed, so they appear
    // as they finish rather than waiting for the next mouse move to trigger a
    // frame.
    if waveforms.is_busy() {
        ctx.request_repaint();
    }

    egui::CentralPanel::default().show(ctx, |ui| {
        ui.heading("Preview");
        let project = state.project().clone();
        let seq_id = state.seq_id;
        let playhead = state.display_tick();

        // Playing: take whatever the decode-ahead thread has ready for the
        // current clock position. Nothing due yet means hold the frame already
        // on screen — never block the UI waiting for a decoder (spec 4.4).
        // Stopped or scrubbing: decode this exact tick on demand.
        let rendered = match transport.video.as_ref().filter(|_| state.playing) {
            Some(video) => match video.frame_for(state.playhead) {
                Some(frame) => {
                    preview.render_prepared(device, egui_renderer, &project, seq_id, frame)
                }
                None => preview.current_texture(),
            },
            None => preview.render(
                device,
                egui_renderer,
                &project,
                seq_id,
                playhead,
                // Scrubbing benefits most of all from an all-intra proxy: no
                // decoding forward from a distant keyframe on every jump.
                // Video-only path: this renders picture for the preview, so a
                // proxy is exactly what's wanted. Audio never comes through
                // here — see `playback_paths` for the split.
                &matting_jobs.resolve(&proxies.resolve(&state.asset_paths)),
            ),
        };

        if let Some((tex_id, w, h)) = rendered {
            let avail = ui.available_size();
            let aspect = w as f32 / h as f32;
            let mut size = avail;
            if size.x / size.y > aspect {
                size.x = size.y * aspect;
            } else {
                size.y = size.x / aspect;
            }
            ui.centered_and_justified(|ui| {
                ui.add(egui::Image::new((tex_id, size)));
            });
        } else {
            ui.label("Nothing to preview yet — import media and add it to the timeline.");
        }
    });
}

/// Writes a recovery snapshot when one is due. Called once per frame.
///
/// Errors are surfaced in the status line rather than being retried in a tight
/// loop: a full disk should say so once, not make the editor stutter.
fn tick_autosave(state: &mut EditorState, autosaver: &mut autosave::Autosave) {
    let revision = state.undo.revision() as usize;
    if !autosaver.is_due(revision) {
        return;
    }
    let path = autosave::Autosave::recovery_path_for(state.project_path.as_deref());
    match state.write_snapshot_to(&path) {
        Ok(()) => autosaver.mark_written(path, revision),
        Err(e) => {
            autosaver.mark_failed(e.clone());
            state.status = format!("autosave failed: {e}");
        }
    }
}

/// One-time offer to restore a recovery file left by a previous session.
///
/// Modal-ish and blocking the choice rather than restoring automatically: the
/// recovered state might be *worse* than the saved file (it captures whatever
/// was on screen when the crash happened, mid-edit), so silently adopting it
/// could destroy good work with bad.
fn recovery_prompt(
    ctx: &egui::Context,
    state: &mut EditorState,
    autosaver: &mut autosave::Autosave,
    offer: &mut Option<std::path::PathBuf>,
    screen: &mut Screen,
) {
    let Some(path) = offer.clone() else { return };
    egui::Window::new("Recover unsaved work?")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
        .show(ctx, |ui| {
            ui.label("The last session ended without saving. A recovery file was found:");
            ui.weak(path.display().to_string());
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Recover").clicked() {
                    if state.open_from(&path) {
                        // Deliberately clear `project_path`: the recovered
                        // project is not "saved at" the recovery file, and
                        // leaving it set would make Ctrl+S overwrite the
                        // recovery file instead of prompting for a real
                        // destination.
                        state.project_path = None;
                        state.status =
                            "recovered unsaved work — use Save As to write it somewhere".into();
                        // Recovering is exactly as much "opening a project"
                        // as any other path into the editor — staying on the
                        // home screen with recovered work loaded silently
                        // behind it would be confusing.
                        *screen = Screen::Editor;
                    }
                    // Either way the offer is spent, and the file has served
                    // its purpose.
                    let _ = std::fs::remove_file(&path);
                    autosaver.discard(state.undo.revision() as usize);
                    *offer = None;
                }
                if ui.button("Discard").clicked() {
                    let _ = std::fs::remove_file(&path);
                    *offer = None;
                }
            });
        });
}

#[cfg(test)]
mod tests {

    /// The "Use proxies" checkbox must not touch what the audio engine reads.
    /// A proxy is a video-only re-encode, so a proxied asset handed to the
    /// audio engine decodes as silence — the clip goes mute for the session.
    #[test]
    fn enabling_proxies_never_changes_the_audio_path() {
        let asset = media::MediaAssetId(1);
        let original = std::path::PathBuf::from("/media/original.mp4");
        let proxy = std::path::PathBuf::from("/cache/proxy.mp4");

        let mut originals = std::collections::HashMap::new();
        originals.insert(asset, original.clone());

        let proxies = proxy_jobs::ProxyJobs::enabled_with_ready_for_test(asset, proxy.clone());
        let matting = matting_jobs::MattingJobs::default();

        let (video, audio) = playback_paths(&originals, &proxies, &matting);

        assert_eq!(video[&asset], proxy, "video should scrub from the proxy");
        assert_eq!(audio[&asset], original, "audio must still come from the source file");
    }

    /// Matting is a real edit, so it applies to picture. Its output carries the
    /// source audio through, but audio still reads the original either way.
    #[test]
    fn a_ready_matte_substitutes_for_video_only() {
        let asset = media::MediaAssetId(7);
        let original = std::path::PathBuf::from("/media/shot.mp4");
        let matte = std::path::PathBuf::from("/cache/matte.mov");

        let mut originals = std::collections::HashMap::new();
        originals.insert(asset, original.clone());

        let proxies = proxy_jobs::ProxyJobs::default();
        let matting = matting_jobs::MattingJobs::with_ready_for_test(asset, matte.clone());

        let (video, audio) = playback_paths(&originals, &proxies, &matting);

        assert_eq!(video[&asset], matte);
        assert_eq!(audio[&asset], original);
    }

    use super::*;

    #[test]
    fn shuttle_engages_at_1x_then_doubles_and_clamps() {
        // L from a standstill must be plain 1x play (the only rate with audio),
        // not an immediate jump to a fast rate.
        assert_eq!(next_shuttle_rate(0.0, true), 1.0);
        assert_eq!(next_shuttle_rate(1.0, true), 2.0);
        assert_eq!(next_shuttle_rate(2.0, true), 4.0);
        assert_eq!(next_shuttle_rate(4.0, true), 8.0);
        assert_eq!(next_shuttle_rate(8.0, true), MAX_SHUTTLE, "must clamp, not run away");
    }

    #[test]
    fn shuttle_reverses_direction_at_1x_rather_than_stepping_down_through_speed() {
        // Pressing J while running forward at 4x should give -1x, not 2x. This
        // is the behaviour editors rely on to stop and back up in one keypress
        // instead of four.
        assert_eq!(next_shuttle_rate(4.0, false), -1.0);
        assert_eq!(next_shuttle_rate(-1.0, false), -2.0);
        assert_eq!(next_shuttle_rate(-4.0, true), 1.0);
        assert_eq!(next_shuttle_rate(-8.0, false), -MAX_SHUTTLE);
    }
}
