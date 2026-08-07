//! M0 risk spike (docs/risks.md #1 and #2):
//! 1. Does wgpu open a window and render at all on this machine/toolchain?
//! 2. Can a worker thread concurrently `queue.write_texture` a
//!    decoded-frame-shaped texture while the main thread records and
//!    submits render commands, without crashing or visibly stalling either
//!    side? This is the playback pipeline's core threading assumption
//!    (decoder threads -> GPU upload, compositor thread -> render), so it's
//!    validated before anything is built on top of it.
//!
//! Not a product feature. Throwaway-but-honest: kept in the repo because
//! its answer (recorded in docs/risks.md once run) is a real M0 deliverable.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use winit::event::{Event, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoop};
use winit::window::WindowBuilder;

const TEXTURE_SIZE: u32 = 256;

fn main() {
    let event_loop = EventLoop::new().unwrap();
    let window = Arc::new(
        WindowBuilder::new()
            .with_title("nle-engine spike: concurrent GPU texture upload")
            .with_inner_size(winit::dpi::LogicalSize::new(900.0, 600.0))
            .build(&event_loop)
            .unwrap(),
    );

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });

    let surface = instance
        .create_surface(window.clone())
        .expect("create_surface failed");

    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: Some(&surface),
        force_fallback_adapter: false,
    }))
    .expect("no suitable GPU adapter found");

    println!("adapter: {:?}", adapter.get_info());

    let (device, queue_owned) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
        },
        None,
    ))
    .expect("request_device failed");
    // wgpu::Queue/Texture don't implement Clone directly; Arc is the
    // standard way to share a handle with the background upload thread.
    let queue = Arc::new(queue_owned);

    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);
    let mut config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: caps.present_modes[0],
        alpha_mode: caps.alpha_modes[0],
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };
    surface.configure(&device, &config);

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("spike shader"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
    });

    let triangle_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("triangle pipeline layout"),
        bind_group_layouts: &[],
        push_constant_ranges: &[],
    });
    let triangle_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("triangle pipeline"),
        layout: Some(&triangle_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_triangle",
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_triangle",
            targets: &[Some(wgpu::ColorTargetState {
                format: config.format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    });

    // The texture a background "decoder" thread writes into concurrently.
    let texture = Arc::new(device.create_texture(&wgpu::TextureDescriptor {
        label: Some("upload spike texture"),
        size: wgpu::Extent3d {
            width: TEXTURE_SIZE,
            height: TEXTURE_SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    }));
    let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });

    let quad_bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("quad bind group layout"),
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
    let quad_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("quad bind group"),
        layout: &quad_bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&texture_view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
        ],
    });
    let quad_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("quad pipeline layout"),
        bind_group_layouts: &[&quad_bind_group_layout],
        push_constant_ranges: &[],
    });
    let quad_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("quad pipeline"),
        layout: Some(&quad_pipeline_layout),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_quad",
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_quad",
            targets: &[Some(wgpu::ColorTargetState {
                format: config.format,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None,
    });

    // --- Background "decoder" thread: continuous concurrent GPU uploads ---
    let bg_queue = queue.clone();
    let bg_texture = texture.clone();
    let write_count = Arc::new(AtomicU64::new(0));
    let bg_write_count = write_count.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let bg_stop = stop.clone();

    let decoder_thread = std::thread::spawn(move || {
        let mut buf = vec![0u8; (TEXTURE_SIZE * TEXTURE_SIZE * 4) as usize];
        let start = Instant::now();
        while !bg_stop.load(Ordering::Relaxed) {
            let t = start.elapsed().as_secs_f32();
            for y in 0..TEXTURE_SIZE {
                for x in 0..TEXTURE_SIZE {
                    let i = ((y * TEXTURE_SIZE + x) * 4) as usize;
                    let u = x as f32 / TEXTURE_SIZE as f32;
                    let v = y as f32 / TEXTURE_SIZE as f32;
                    buf[i] = (((u + t * 0.3).fract()) * 255.0) as u8;
                    buf[i + 1] = (((v + t * 0.2).fract()) * 255.0) as u8;
                    buf[i + 2] = ((((u + v) * 0.5 + t * 0.5).fract()) * 255.0) as u8;
                    buf[i + 3] = 255;
                }
            }
            bg_queue.write_texture(
                wgpu::ImageCopyTexture {
                    texture: bg_texture.as_ref(),
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &buf,
                wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(4 * TEXTURE_SIZE),
                    rows_per_image: Some(TEXTURE_SIZE),
                },
                wgpu::Extent3d {
                    width: TEXTURE_SIZE,
                    height: TEXTURE_SIZE,
                    depth_or_array_layers: 1,
                },
            );
            bg_write_count.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(16)); // target ~60Hz "decode" rate
        }
    });

    let mut frame_count: u64 = 0;
    let mut last_report = Instant::now();
    let mut last_write_count = 0u64;

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
                WindowEvent::RedrawRequested => {
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
                    let mut encoder =
                        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                            label: Some("frame encoder"),
                        });
                    {
                        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("main pass"),
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color {
                                        r: 0.05,
                                        g: 0.05,
                                        b: 0.08,
                                        a: 1.0,
                                    }),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        pass.set_pipeline(&triangle_pipeline);
                        pass.draw(0..3, 0..1);
                        pass.set_pipeline(&quad_pipeline);
                        pass.set_bind_group(0, &quad_bind_group, &[]);
                        pass.draw(0..6, 0..1);
                    }
                    queue.submit(std::iter::once(encoder.finish()));
                    output.present();
                    frame_count += 1;
                }
                _ => {}
            },
            Event::AboutToWait => {
                window.request_redraw();
                if last_report.elapsed() >= Duration::from_secs(1) {
                    let writes_now = write_count.load(Ordering::Relaxed);
                    println!(
                        "render: {} fps | decoder-thread uploads: {} writes/s",
                        frame_count,
                        writes_now - last_write_count
                    );
                    frame_count = 0;
                    last_write_count = writes_now;
                    last_report = Instant::now();
                }
            }
            Event::LoopExiting => {
                stop.store(true, Ordering::Relaxed);
            }
            _ => {}
        })
        .unwrap();

    decoder_thread.join().ok();
}
