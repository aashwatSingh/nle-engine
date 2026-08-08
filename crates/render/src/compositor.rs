//! GPU compositor — executes what `graph.rs` compiles, per spec 4.5's
//! pipeline: source frames -> per-clip transform -> track blending -> output,
//! with all blending done in a **linear-light Rec.709 working space at 16-bit
//! float**, and conversion to the delivery space happening once, at the end.
//!
//! Structure of a single `render` call:
//!
//! 1. Allocate/clear an `Rgba16Float` working target at sequence resolution.
//! 2. For each video track bottom-to-top with an active clip: draw its
//!    source as a transformed quad with alpha blending, converting the
//!    source out of its encoded colour space into linear light in the
//!    fragment shader.
//! 3. Convert the working target into the encoded delivery space, writing
//!    `Rgba8Unorm` output.
//!
//! Nested sequences recurse through step 1-2 into their own working target
//! first, then get treated as an ordinary source. Each nesting level
//! allocates its own intermediates rather than drawing from a pool — fine at
//! M4's scale, and the place to optimise if profiling ever says nesting depth
//! costs real time.

use crate::color;
use crate::graph::{CompiledFrameGraph, FrameSource, Transform2D};
use crate::DeliverySpace;
use media::{ColorMetadata, ColorPrimaries, MediaAssetId, TransferFunction};
use std::collections::HashMap;
use std::sync::Arc;

/// The working space every composite happens in. 16-bit float so
/// out-of-gamut and over-1.0 values survive intermediate passes — see
/// `color.rs` for why that headroom matters.
pub const WORKING_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;
/// Delivery-encoded 8-bit output. Deliberately *not* an `*Srgb` format: the
/// delivery pass does the encoding explicitly in the shader, so letting the
/// hardware also apply an sRGB transfer would double-encode.
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ClipUniforms {
    src_size: [f32; 2],
    seq_size: [f32; 2],
    position: [f32; 2],
    scale: [f32; 2],
    anchor: [f32; 2],
    rotation: f32,
    opacity: f32,
    gamut0: [f32; 4],
    gamut1: [f32; 4],
    gamut2: [f32; 4],
    transfer_code: u32,
    _pad: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct OutputUniforms {
    delivery_code: u32,
    _pad: [u32; 3],
}

/// A decoded source frame living on the GPU.
pub struct SourceTexture {
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    pub color: ColorMetadata,
}

/// The frames a `CompiledFrameGraph` needs, keyed exactly the way
/// `FrameSource::Media` names them. The compositor never decodes — it asks
/// for what the graph said it needs and skips any track whose frame is
/// missing (a dropped frame, not a crash; see spec 4.4's frame-drop policy).
#[derive(Default)]
pub struct SourceFrames {
    frames: HashMap<(MediaAssetId, i64), SourceTexture>,
}

impl SourceFrames {
    pub fn insert(&mut self, asset: MediaAssetId, pts_ticks: i64, texture: SourceTexture) {
        self.frames.insert((asset, pts_ticks), texture);
    }

    pub fn get(&self, asset: MediaAssetId, pts_ticks: i64) -> Option<&SourceTexture> {
        self.frames.get(&(asset, pts_ticks))
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// A composited frame read back to the CPU, in encoded delivery space.
pub struct RenderedFrame {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, `width * height * 4` bytes (row padding from
    /// the GPU copy is already stripped).
    pub rgba: Vec<u8>,
}

impl RenderedFrame {
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2], self.rgba[i + 3]]
    }
}

/// Report from one composite: what actually got drawn, and what didn't.
/// Returned rather than logged so callers (and tests) can assert on it —
/// "the frame rendered but two tracks silently contributed nothing" is
/// exactly the class of bug that otherwise ships unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompositeStats {
    pub layers_drawn: usize,
    /// Tracks that had an active clip but whose source frame wasn't in
    /// `SourceFrames`.
    pub layers_missing_source: usize,
    pub nested_sequences_rendered: usize,
}

pub struct Compositor {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    clip_pipeline: wgpu::RenderPipeline,
    deliver_pipeline: wgpu::RenderPipeline,
    clip_bgl: wgpu::BindGroupLayout,
    deliver_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Compositor {
    pub fn new(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });

        let clip_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("clip bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        // Same shape; separate layout so the two pipelines stay independent.
        let deliver_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("deliver bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let clip_pipeline = Self::make_pipeline(
            &device,
            &shader,
            &clip_bgl,
            "vs_clip",
            "fs_clip",
            WORKING_FORMAT,
            // Standard source-over alpha blending. This is what makes
            // stacking tracks bottom-to-top produce the expected result.
            Some(wgpu::BlendState {
                color: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::SrcAlpha,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
                alpha: wgpu::BlendComponent {
                    src_factor: wgpu::BlendFactor::One,
                    dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                    operation: wgpu::BlendOperation::Add,
                },
            }),
            "clip pipeline",
        );
        let deliver_pipeline = Self::make_pipeline(
            &device,
            &shader,
            &deliver_bgl,
            "vs_fullscreen",
            "fs_deliver",
            OUTPUT_FORMAT,
            Some(wgpu::BlendState::REPLACE),
            "deliver pipeline",
        );

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("composite sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Clamp so a rotated/scaled quad sampling slightly outside the
            // source doesn't wrap pixels in from the opposite edge.
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Compositor { device, queue, clip_pipeline, deliver_pipeline, clip_bgl, deliver_bgl, sampler }
    }

    #[allow(clippy::too_many_arguments)]
    fn make_pipeline(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        bgl: &wgpu::BindGroupLayout,
        vs: &str,
        fs: &str,
        format: wgpu::TextureFormat,
        blend: Option<wgpu::BlendState>,
        label: &str,
    ) -> wgpu::RenderPipeline {
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(label),
            bind_group_layouts: &[bgl],
            push_constant_ranges: &[],
        });
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: vs,
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: fs,
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        })
    }

    pub fn device(&self) -> &Arc<wgpu::Device> {
        &self.device
    }

    pub fn queue(&self) -> &Arc<wgpu::Queue> {
        &self.queue
    }

    /// Uploads a CPU RGBA8 frame as a source texture. `color` is the frame's
    /// tagged colour space, which the clip shader uses to get it into the
    /// linear working space.
    pub fn upload_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
        color: ColorMetadata,
    ) -> SourceTexture {
        let texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("source frame"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Non-sRGB: the shader applies the transfer function explicitly
            // based on the frame's tag, so hardware sRGB decode would
            // double-apply it.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * width),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        SourceTexture { view, width, height, color }
    }

    fn create_working_target(&self, width: u32, height: u32) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("working space target"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: WORKING_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// Composites `graph` into a fresh linear working-space texture,
    /// recursing into nested sequences as needed. This is the shared core
    /// of both the preview and export paths — neither gets its own copy of
    /// the compositing logic, which is what makes "preview matches export"
    /// a structural property rather than something to keep in sync by hand.
    fn composite_to_working(
        &self,
        graph: &CompiledFrameGraph,
        sources: &SourceFrames,
        stats: &mut CompositeStats,
    ) -> wgpu::Texture {
        let target = self.create_working_target(graph.width, graph.height);
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // Nested sequences must be fully rendered before the parent's render
        // pass opens, since they need render passes of their own. Their
        // resulting textures are kept alive in this vec for the duration of
        // the parent pass.
        let mut nested_textures: Vec<(usize, wgpu::Texture)> = Vec::new();
        for (i, plan) in graph.track_plans.iter().enumerate() {
            if let Some(active) = &plan.active_clip {
                if let FrameSource::Nested(inner) = &active.source {
                    let tex = self.composite_to_working(inner, sources, stats);
                    stats.nested_sequences_rendered += 1;
                    nested_textures.push((i, tex));
                }
            }
        }
        let nested_views: HashMap<usize, wgpu::TextureView> = nested_textures
            .iter()
            .map(|(i, t)| (*i, t.create_view(&wgpu::TextureViewDescriptor::default())))
            .collect();

        // Build every uniform buffer + bind group up front: a render pass
        // borrows them, so they can't be created while it's open.
        struct Layer<'a> {
            bind_group: wgpu::BindGroup,
            _marker: std::marker::PhantomData<&'a ()>,
        }
        let mut layers: Vec<Layer> = Vec::new();

        for (i, plan) in graph.track_plans.iter().enumerate() {
            let Some(active) = &plan.active_clip else { continue };

            let (view, src_w, src_h, src_color) = match &active.source {
                FrameSource::Media { asset, source_pts_ticks } => {
                    match sources.get(*asset, *source_pts_ticks) {
                        Some(tex) => (&tex.view, tex.width, tex.height, tex.color),
                        None => {
                            stats.layers_missing_source += 1;
                            continue;
                        }
                    }
                }
                FrameSource::Nested(inner) => {
                    let view = nested_views.get(&i).expect("nested view was rendered above");
                    (
                        view,
                        inner.width,
                        inner.height,
                        // A nested sequence's output is already in the linear
                        // working space, so it needs no further conversion.
                        ColorMetadata {
                            primaries: ColorPrimaries::Rec709,
                            transfer: TransferFunction::Linear,
                            matrix: media::MatrixCoefficients::Bt709,
                            full_range: true,
                        },
                    )
                }
            };

            let gamut = color::primaries_to_working(src_color.primaries);
            let t: Transform2D = active.transform;
            let uniforms = ClipUniforms {
                src_size: [src_w as f32, src_h as f32],
                seq_size: [graph.width as f32, graph.height as f32],
                position: [t.position.0, t.position.1],
                scale: [t.scale.0, t.scale.1],
                anchor: [t.anchor.0, t.anchor.1],
                rotation: t.rotation_degrees,
                opacity: t.opacity,
                gamut0: [gamut[0][0], gamut[0][1], gamut[0][2], 0.0],
                gamut1: [gamut[1][0], gamut[1][1], gamut[1][2], 0.0],
                gamut2: [gamut[2][0], gamut[2][1], gamut[2][2], 0.0],
                transfer_code: color::shader_transfer_code(src_color.transfer),
                _pad: [0; 3],
            };
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("clip uniforms"),
                size: std::mem::size_of::<ClipUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&uniforms));

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("clip bind group"),
                layout: &self.clip_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                ],
            });
            layers.push(Layer { bind_group, _marker: std::marker::PhantomData });
            stats.layers_drawn += 1;
        }

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("composite") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("composite pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // Transparent black, not opaque black: an empty
                        // track must not punch a hole through the layers
                        // below it.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.clip_pipeline);
            for layer in &layers {
                pass.set_bind_group(0, &layer.bind_group, &[]);
                pass.draw(0..6, 0..1);
            }
        }
        self.queue.submit(std::iter::once(encoder.finish()));
        target
    }

    /// Converts a linear working-space texture into the encoded delivery
    /// space, writing into `output_view` (which must be `OUTPUT_FORMAT`).
    fn deliver(
        &self,
        working: &wgpu::Texture,
        output_view: &wgpu::TextureView,
        delivery: DeliverySpace,
    ) {
        let working_view = working.create_view(&wgpu::TextureViewDescriptor::default());
        let uniforms = OutputUniforms {
            delivery_code: color::shader_delivery_code(delivery),
            _pad: [0; 3],
        };
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("output uniforms"),
            size: std::mem::size_of::<OutputUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&uniforms));

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("deliver bind group"),
            layout: &self.deliver_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&working_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("deliver") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("deliver pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.deliver_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    /// Full composite into a caller-owned output texture view. This is the
    /// **preview** entry point: the view is typically a swapchain texture.
    pub fn render_to_view(
        &self,
        graph: &CompiledFrameGraph,
        sources: &SourceFrames,
        delivery: DeliverySpace,
        output_view: &wgpu::TextureView,
    ) -> CompositeStats {
        let mut stats = CompositeStats::default();
        let working = self.composite_to_working(graph, sources, &mut stats);
        self.deliver(&working, output_view, delivery);
        stats
    }

    /// Full composite read back to the CPU. This is the **export** entry
    /// point, and the one tests assert on. It calls exactly the same
    /// `composite_to_working` + `deliver` as `render_to_view`.
    pub fn render_to_rgba(
        &self,
        graph: &CompiledFrameGraph,
        sources: &SourceFrames,
        delivery: DeliverySpace,
    ) -> (RenderedFrame, CompositeStats) {
        let (width, height) = (graph.width, graph.height);
        let output = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("output"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: OUTPUT_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());
        let stats = self.render_to_view(graph, sources, delivery, &output_view);

        // GPU->CPU copies require each row to start on a 256-byte boundary,
        // so the staging buffer is padded and the padding stripped below.
        let unpadded_bytes_per_row = width * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;

        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded_bytes_per_row * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &output,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_bytes_per_row),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        self.queue.submit(std::iter::once(encoder.finish()));

        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device.poll(wgpu::Maintain::Wait);
        rx.recv().expect("map_async never reported").expect("readback map failed");

        let mapped = slice.get_mapped_range();
        let mut rgba = Vec::with_capacity((unpadded_bytes_per_row * height) as usize);
        for row in 0..height {
            let start = (row * padded_bytes_per_row) as usize;
            rgba.extend_from_slice(&mapped[start..start + unpadded_bytes_per_row as usize]);
        }
        drop(mapped);
        staging.unmap();

        (RenderedFrame { width, height, rgba }, stats)
    }
}

/// Creates a headless wgpu device — no window, no surface. Used by the
/// compositor tests and by export (which has no window either).
pub fn headless_context() -> Option<(Arc<wgpu::Device>, Arc<wgpu::Queue>)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        ..Default::default()
    });
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))?;
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("headless"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
        },
        None,
    ))
    .ok()?;
    Some((Arc::new(device), Arc::new(queue)))
}
