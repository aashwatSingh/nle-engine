//! M2 capstone demo: real audio-clocked playback with a synchronized video
//! window, transport controls, and seeking. This is the visual/audible
//! proof for spec M2's acceptance bar (audio-clocked playback, transport
//! controls, seeking) — not the full JKL variable-speed shuttle (that's a
//! documented follow-up; docs/risks.md already flags reverse playback in
//! particular as a hard, separately-spiked problem).
//!
//! Controls: Space = play/pause, Right/Left arrow = seek +/-5s.

use playback::{AudioEngine, VideoPlayback};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use timeline::TIMEBASE;
use winit::event::{ElementState, Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::WindowBuilder;

fn ticks_to_seconds(ticks: i64) -> f64 {
    ticks as f64 / TIMEBASE as f64
}

fn main() {
    media_ffmpeg::init().expect("ffmpeg init failed");

    let arg = std::env::args().nth(1).unwrap_or_else(|| "test_playback_demo.mp4".to_string());
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join("..").join("test_fixtures").join(&arg);

    let asset = media_ffmpeg::probe(&path).expect("probe failed");
    let video_info = asset.video.clone().expect("fixture must have video");
    let duration_ticks = asset.duration_ticks;
    println!(
        "{arg}: {}x{} duration={:.2}s",
        video_info.width,
        video_info.height,
        ticks_to_seconds(duration_ticks)
    );

    let audio = AudioEngine::start(path.clone(), 0).expect("audio engine failed to start");
    let audio_clock = audio.clock();
    let video = VideoPlayback::start(path.clone(), move || audio_clock.current_tick())
        .expect("video playback failed to start");
    let mut playing = true;

    let event_loop = EventLoop::new().unwrap();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title(format!("nle-engine: {arg} (M2 A/V playback proof) — Space=play/pause, arrows=seek"))
            .with_inner_size(winit::dpi::LogicalSize::new(video_info.width as f64, video_info.height as f64))
            .build(&event_loop)
            .unwrap(),
    );

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor { backends: wgpu::Backends::PRIMARY, ..Default::default() });
    let surface = instance.create_surface(window.clone()).expect("create_surface failed");
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no suitable GPU adapter found");
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor { label: None, required_features: wgpu::Features::empty(), required_limits: wgpu::Limits::default() },
        None,
    ))
    .expect("request_device failed");

    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    let format = caps.formats[0];
    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::Fifo,
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("video frame texture"),
        size: wgpu::Extent3d { width: video_info.width, height: video_info.height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("frame bind group layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("frame bind group"),
        layout: &bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&texture_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    });
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("play_clip shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("pipeline layout"),
        bind_group_layouts: &[&bind_group_layout],
        push_constant_ranges: &[],
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("frame pipeline"),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState { module: &shader, entry_point: "vs_main", buffers: &[], compilation_options: Default::default() },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState { format: config.format, blend: Some(wgpu::BlendState::REPLACE), write_mask: wgpu::ColorWrites::ALL })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    });

    let mut last_uploaded_pts = i64::MIN;
    let mut last_status_print = Instant::now();

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
                WindowEvent::KeyboardInput { event: key_event, .. } => {
                    if key_event.state == ElementState::Pressed && !key_event.repeat {
                        match key_event.physical_key {
                            PhysicalKey::Code(KeyCode::Space) => {
                                playing = !playing;
                                if playing {
                                    audio.play();
                                    video.play();
                                } else {
                                    audio.pause();
                                    video.pause();
                                }
                                println!("{}", if playing { "playing" } else { "paused" });
                            }
                            PhysicalKey::Code(KeyCode::ArrowRight) => {
                                let target = (audio.current_tick() + TIMEBASE * 5).min(duration_ticks);
                                audio.seek(target);
                                video.seek(target);
                            }
                            PhysicalKey::Code(KeyCode::ArrowLeft) => {
                                let target = (audio.current_tick() - TIMEBASE * 5).max(0);
                                audio.seek(target);
                                video.seek(target);
                            }
                            _ => {}
                        }
                    }
                }
                WindowEvent::RedrawRequested => {
                    if let Some(frame) = video.latest_frame() {
                        if frame.pts_ticks != last_uploaded_pts {
                            queue.write_texture(
                                wgpu::ImageCopyTexture { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
                                &frame.rgba,
                                wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(4 * frame.width), rows_per_image: Some(frame.height) },
                                wgpu::Extent3d { width: frame.width, height: frame.height, depth_or_array_layers: 1 },
                            );
                            last_uploaded_pts = frame.pts_ticks;
                        }
                    }

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
                    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame encoder") });
                    {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("main pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        pass.set_pipeline(&pipeline);
                        pass.set_bind_group(0, &bind_group, &[]);
                        pass.draw(0..6, 0..1);
                    }
                    queue.submit(std::iter::once(encoder.finish()));
                    output.present();
                }
                _ => {}
            },
            Event::AboutToWait => {
                window.request_redraw();
                if last_status_print.elapsed() >= Duration::from_secs(1) {
                    println!(
                        "t={:.2}s underruns={} dropped_frames={}",
                        ticks_to_seconds(audio.current_tick()),
                        audio.underrun_count(),
                        video.dropped_frame_count()
                    );
                    last_status_print = Instant::now();
                }
            }
            _ => {}
        })
        .unwrap();
}
