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

/// Mirrors `composite.wgsl`'s `MaskUniforms`. Vec2 fields are grouped first
/// deliberately: WGSL aligns `vec2<f32>` to 8 bytes, while Rust's `[f32; 2]`
/// only naturally aligns to 4. Two consecutive 8-byte fields starting at
/// offset 0 land on 8-byte boundaries under *either* alignment rule, so this
/// ordering is what makes Rust's `#[repr(C)]` packing coincide with what WGSL
/// expects without hand-computed padding. Breaking this grouping (e.g.
/// interleaving a lone `f32` between two `[f32; 2]` fields) would silently
/// desync the two layouts — there's no compiler check across the language
/// boundary here, which is exactly why the ordering rule is spelled out
/// rather than left implicit.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct MaskUniforms {
    center: [f32; 2],
    size: [f32; 2],
    feather: f32,
    enabled: u32,
    is_rectangle: u32,
    invert: u32,
}

impl MaskUniforms {
    fn from_shape(mask: crate::graph::MaskShape) -> Self {
        MaskUniforms {
            center: [mask.center.0, mask.center.1],
            size: [mask.size.0, mask.size.1],
            feather: mask.feather,
            enabled: mask.enabled as u32,
            is_rectangle: mask.is_rectangle as u32,
            invert: mask.invert as u32,
        }
    }
}

/// Mirrors `composite.wgsl`'s `PrepareUniforms`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PrepareUniforms {
    gamut0: [f32; 4],
    gamut1: [f32; 4],
    gamut2: [f32; 4],
    transfer_code: u32,
    _pad: [u32; 3],
}

/// Mirrors `composite.wgsl`'s `BlurUniforms`. `direction` first — see the
/// `MaskUniforms` comment on why vec2 fields must come before scalars.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct BlurUniforms {
    direction: [f32; 2],
    radius: f32,
    _pad: f32,
}

/// Mirrors `composite.wgsl`'s `ColorCorrectionUniforms`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ColorCorrectionUniforms {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    temperature: f32,
    tint: f32,
    _pad: [f32; 3],
}

/// Mirrors `composite.wgsl`'s `CropUniforms`.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CropUniforms {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
}

/// A decoded source frame living on the GPU.
pub struct SourceTexture {
    pub view: wgpu::TextureView,
    pub width: u32,
    pub height: u32,
    pub color: ColorMetadata,
}

/// The frames available to satisfy a `CompiledFrameGraph`'s
/// `FrameSource::Media` requests. The compositor never decodes — it asks for
/// what the graph said it needs and skips any track whose frame is missing (a
/// dropped frame, not a crash; see spec 4.4's frame-drop policy).
///
/// Lookup is deliberately **nearest-at-or-before**, not exact match. The
/// graph computes an exact source tick from timeline arithmetic, but a real
/// decoder only ever yields the frames that actually exist in the file, whose
/// PTS values almost never land on that exact tick. Exact matching therefore
/// works fine in a test that uploads at the tick it renders and fails ~100%
/// of the time during real playback. "The most recent frame whose PTS is at
/// or before the requested time" is the correct presentation rule.
#[derive(Default)]
pub struct SourceFrames {
    /// Per asset, sorted ascending by PTS.
    frames: HashMap<MediaAssetId, Vec<(i64, SourceTexture)>>,
}

impl SourceFrames {
    pub fn insert(&mut self, asset: MediaAssetId, pts_ticks: i64, texture: SourceTexture) {
        let entries = self.frames.entry(asset).or_default();
        match entries.binary_search_by_key(&pts_ticks, |(p, _)| *p) {
            Ok(existing) => entries[existing] = (pts_ticks, texture),
            Err(pos) => entries.insert(pos, (pts_ticks, texture)),
        }
    }

    /// The frame that should be *presented* at `pts_ticks`: the latest frame
    /// at or before it. Falls back to the earliest available frame when the
    /// request precedes everything held, so a slightly-early request shows
    /// the first frame rather than nothing.
    pub fn get(&self, asset: MediaAssetId, pts_ticks: i64) -> Option<&SourceTexture> {
        let entries = self.frames.get(&asset)?;
        match entries.binary_search_by_key(&pts_ticks, |(p, _)| *p) {
            Ok(exact) => Some(&entries[exact].1),
            Err(0) => entries.first().map(|(_, t)| t),
            Err(pos) => Some(&entries[pos - 1].1),
        }
    }

    /// Drops frames older than `keep_from_ticks` for `asset`, always leaving
    /// at least one so a still playhead never loses its frame. Bounds memory
    /// during sustained playback, where frames arrive continuously.
    pub fn retain_from(&mut self, asset: MediaAssetId, keep_from_ticks: i64) {
        if let Some(entries) = self.frames.get_mut(&asset) {
            let cutoff = entries.partition_point(|(p, _)| *p < keep_from_ticks);
            // Keep one frame before the cutoff: it's the one currently being
            // presented for any tick between it and the next.
            let drop_count = cutoff.saturating_sub(1);
            if drop_count > 0 {
                entries.drain(..drop_count);
            }
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Total frames held across all assets.
    pub fn len(&self) -> usize {
        self.frames.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    prepare_pipeline: wgpu::RenderPipeline,
    blur_pipeline: wgpu::RenderPipeline,
    color_correction_pipeline: wgpu::RenderPipeline,
    crop_pipeline: wgpu::RenderPipeline,
    deliver_pipeline: wgpu::RenderPipeline,
    /// Bind group layout for the "place" pass (`fs_clip`): a `ClipUniforms`
    /// buffer, the source texture + sampler, and a `MaskUniforms` buffer.
    clip_bgl: wgpu::BindGroupLayout,
    /// Shared by every pass that just transforms one texture into another of
    /// the same size (prepare, blur, color correction, crop, deliver): one
    /// small uniform buffer + source texture + sampler. The layout doesn't
    /// encode the uniform struct's actual size (`min_binding_size: None`), so
    /// one layout genuinely works for all of them despite their uniform
    /// structs differing.
    single_uniform_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    output_format: wgpu::TextureFormat,
}

impl Compositor {
    /// Compositor writing `OUTPUT_FORMAT` (`Rgba8Unorm`) — the export shape,
    /// and what `render_to_rgba` reads back.
    pub fn new(device: Arc<wgpu::Device>, queue: Arc<wgpu::Queue>) -> Self {
        Self::with_output_format(device, queue, OUTPUT_FORMAT)
    }

    /// Compositor writing a caller-chosen format, so preview can render
    /// straight into a swapchain texture instead of through an extra blit.
    ///
    /// Pass a **non-sRGB** format. The delivery pass applies the transfer
    /// function itself, so an `*Srgb` target would encode a second time and
    /// produce a visibly washed-out image.
    pub fn with_output_format(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        output_format: wgpu::TextureFormat,
    ) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("composite"),
            source: wgpu::ShaderSource::Wgsl(include_str!("composite.wgsl").into()),
        });

        let buffer_entry = |binding: u32, visibility: wgpu::ShaderStages| wgpu::BindGroupLayoutEntry {
            binding,
            visibility,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let texture_entry = wgpu::BindGroupLayoutEntry {
            binding: 1,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler_entry = wgpu::BindGroupLayoutEntry {
            binding: 2,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };

        let clip_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("clip bgl"),
            entries: &[
                buffer_entry(0, wgpu::ShaderStages::VERTEX_FRAGMENT),
                texture_entry,
                sampler_entry,
                buffer_entry(3, wgpu::ShaderStages::FRAGMENT),
            ],
        });
        let single_uniform_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("single uniform bgl"),
            entries: &[buffer_entry(0, wgpu::ShaderStages::FRAGMENT), texture_entry, sampler_entry],
        });

        // Standard source-over alpha blending. This is what makes stacking
        // tracks bottom-to-top produce the expected result.
        let source_over_blend = Some(wgpu::BlendState {
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
        });

        let clip_pipeline = Self::make_pipeline(
            &device, &shader, &clip_bgl, "vs_clip", "fs_clip", WORKING_FORMAT, source_over_blend, "clip pipeline",
        );
        // Prepare/blur/color-correction/crop all write a fresh intermediate
        // texture (never blend with what's already there), so REPLACE.
        let prepare_pipeline = Self::make_pipeline(
            &device, &shader, &single_uniform_bgl, "vs_fullscreen", "fs_prepare", WORKING_FORMAT,
            Some(wgpu::BlendState::REPLACE), "prepare pipeline",
        );
        let blur_pipeline = Self::make_pipeline(
            &device, &shader, &single_uniform_bgl, "vs_fullscreen", "fs_gaussian_blur", WORKING_FORMAT,
            Some(wgpu::BlendState::REPLACE), "blur pipeline",
        );
        let color_correction_pipeline = Self::make_pipeline(
            &device, &shader, &single_uniform_bgl, "vs_fullscreen", "fs_color_correction", WORKING_FORMAT,
            Some(wgpu::BlendState::REPLACE), "color correction pipeline",
        );
        let crop_pipeline = Self::make_pipeline(
            &device, &shader, &single_uniform_bgl, "vs_fullscreen", "fs_crop", WORKING_FORMAT,
            Some(wgpu::BlendState::REPLACE), "crop pipeline",
        );
        let deliver_pipeline = Self::make_pipeline(
            &device, &shader, &single_uniform_bgl, "vs_fullscreen", "fs_deliver", output_format,
            Some(wgpu::BlendState::REPLACE), "deliver pipeline",
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

        Compositor {
            device,
            queue,
            clip_pipeline,
            prepare_pipeline,
            blur_pipeline,
            color_correction_pipeline,
            crop_pipeline,
            deliver_pipeline,
            clip_bgl,
            single_uniform_bgl,
            sampler,
            output_format,
        }
    }

    pub fn output_format(&self) -> wgpu::TextureFormat {
        self.output_format
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
        // Keeps intermediate effect-chain textures alive until the shared
        // render pass below has executed — mirrors `nested_textures` above
        // for the same reason (the bind groups reference their views).
        let mut processed_textures: Vec<wgpu::Texture> = Vec::new();

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

            let has_pixel_effects = active.effect_passes.iter().any(|p| {
                p.type_id == crate::effect::gaussian_blur::TYPE_ID
                    || p.type_id == crate::effect::color_correction::TYPE_ID
                    || p.type_id == crate::effect::crop::TYPE_ID
            });

            // Either run the clip's pixel-effect chain into its own
            // intermediate (already linear when it comes back), or use the
            // raw source directly — the fast path for the common case of an
            // untouched clip, avoiding two extra texture allocations and
            // passes per layer.
            //
            // `processed_view_holder` exists purely so `place_view` below can
            // borrow from it: it's declared fresh each iteration and lives
            // exactly as long as this loop body needs it (through the
            // `create_bind_group` call), which Rust's borrow checker unifies
            // fine with `view`'s longer lifetime in the other branch — no
            // 'static tricks or leaking required.
            #[allow(unused_assignments)]
            let mut processed_view_holder: Option<wgpu::TextureView> = None;
            let (place_view, place_gamut, place_transfer): (&wgpu::TextureView, color::Matrix3, u32) =
                if has_pixel_effects {
                    let processed =
                        self.process_clip_source(view, src_w, src_h, src_color, &active.effect_passes);
                    processed_textures.push(processed);
                    processed_view_holder =
                        Some(processed_textures.last().unwrap().create_view(&wgpu::TextureViewDescriptor::default()));
                    (processed_view_holder.as_ref().unwrap(), color::IDENTITY_3X3, color::shader_codes::TRANSFER_LINEAR)
                } else {
                    (
                        view,
                        color::primaries_to_working(src_color.primaries),
                        color::shader_transfer_code(src_color.transfer),
                    )
                };

            let t: Transform2D = active.transform;
            let uniforms = ClipUniforms {
                src_size: [src_w as f32, src_h as f32],
                seq_size: [graph.width as f32, graph.height as f32],
                position: [t.position.0, t.position.1],
                scale: [t.scale.0, t.scale.1],
                anchor: [t.anchor.0, t.anchor.1],
                rotation: t.rotation_degrees,
                opacity: t.opacity,
                gamut0: [place_gamut[0][0], place_gamut[0][1], place_gamut[0][2], 0.0],
                gamut1: [place_gamut[1][0], place_gamut[1][1], place_gamut[1][2], 0.0],
                gamut2: [place_gamut[2][0], place_gamut[2][1], place_gamut[2][2], 0.0],
                transfer_code: place_transfer,
                _pad: [0; 3],
            };
            let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("clip uniforms"),
                size: std::mem::size_of::<ClipUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&buffer, 0, bytemuck::bytes_of(&uniforms));

            let mask_buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("mask uniforms"),
                size: std::mem::size_of::<MaskUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.queue.write_buffer(&mask_buffer, 0, bytemuck::bytes_of(&MaskUniforms::from_shape(active.mask)));

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("clip bind group"),
                layout: &self.clip_bgl,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(place_view) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
                    wgpu::BindGroupEntry { binding: 3, resource: mask_buffer.as_entire_binding() },
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
    /// space, writing into `output_view` (which must match this
    /// compositor's `output_format`).
    fn deliver(&self, working: &wgpu::Texture, output_view: &wgpu::TextureView, delivery: DeliverySpace) {
        let working_view = working.create_view(&wgpu::TextureViewDescriptor::default());
        let uniforms = OutputUniforms { delivery_code: color::shader_delivery_code(delivery), _pad: [0; 3] };
        self.run_single_uniform_pass(&self.deliver_pipeline, bytemuck::bytes_of(&uniforms), &working_view, output_view);
    }

    /// Runs one full-screen shader pass: `input_view` -> `output_view`,
    /// through `pipeline`, with `uniform_bytes` as the pass's single uniform
    /// buffer. Shared by deliver, prepare, blur, color correction, and crop
    /// — every pass with the "one small uniform + one input texture -> one
    /// output texture, no blending" shape.
    fn run_single_uniform_pass(
        &self,
        pipeline: &wgpu::RenderPipeline,
        uniform_bytes: &[u8],
        input_view: &wgpu::TextureView,
        output_view: &wgpu::TextureView,
    ) {
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("single uniform buffer"),
            size: uniform_bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        self.queue.write_buffer(&buffer, 0, uniform_bytes);
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("single uniform bind group"),
            layout: &self.single_uniform_bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(input_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("single pass") });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("single pass"),
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
            pass.set_pipeline(pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..6, 0..1);
        }
        self.queue.submit(std::iter::once(encoder.finish()));
    }

    fn make_scratch_texture(&self, width: u32, height: u32, label: &str) -> wgpu::Texture {
        self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: WORKING_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        })
    }

    /// Runs a clip's pixel-effect stack (Gaussian Blur, Color Correction,
    /// Crop — in stack order, skipping anything else including `transform`
    /// and `mask`, which are folded and applied separately) at the clip's own
    /// resolution. First converts the encoded source into the linear working
    /// space (`fs_prepare`), then ping-pongs through the effect chain.
    ///
    /// Returns an owned `Rgba16Float` texture already in the linear working
    /// space — the caller places it with `fs_clip` using an identity gamut
    /// and `TRANSFER_LINEAR`, since colour conversion already happened here.
    fn process_clip_source(
        &self,
        source_view: &wgpu::TextureView,
        width: u32,
        height: u32,
        color_meta: ColorMetadata,
        effects: &[crate::graph::EffectPass],
    ) -> wgpu::Texture {
        let mut current = self.make_scratch_texture(width, height, "effect chain A");
        let mut other = self.make_scratch_texture(width, height, "effect chain B");

        let gamut = color::primaries_to_working(color_meta.primaries);
        let prepare_uniforms = PrepareUniforms {
            gamut0: [gamut[0][0], gamut[0][1], gamut[0][2], 0.0],
            gamut1: [gamut[1][0], gamut[1][1], gamut[1][2], 0.0],
            gamut2: [gamut[2][0], gamut[2][1], gamut[2][2], 0.0],
            transfer_code: color::shader_transfer_code(color_meta.transfer),
            _pad: [0; 3],
        };
        self.run_single_uniform_pass(
            &self.prepare_pipeline,
            bytemuck::bytes_of(&prepare_uniforms),
            source_view,
            &current.create_view(&wgpu::TextureViewDescriptor::default()),
        );

        use crate::effect::{color_correction, crop, gaussian_blur};
        for pass in effects {
            if pass.type_id == gaussian_blur::TYPE_ID {
                let radius = pass.number(gaussian_blur::RADIUS).unwrap_or(0.0) as f32;
                if radius <= 0.0 {
                    continue;
                }
                for direction in [[1.0f32, 0.0], [0.0, 1.0]] {
                    let u = BlurUniforms { direction, radius, _pad: 0.0 };
                    self.run_single_uniform_pass(
                        &self.blur_pipeline,
                        bytemuck::bytes_of(&u),
                        &current.create_view(&wgpu::TextureViewDescriptor::default()),
                        &other.create_view(&wgpu::TextureViewDescriptor::default()),
                    );
                    std::mem::swap(&mut current, &mut other);
                }
            } else if pass.type_id == color_correction::TYPE_ID {
                let u = ColorCorrectionUniforms {
                    exposure: pass.number(color_correction::EXPOSURE).unwrap_or(0.0) as f32,
                    contrast: pass.number(color_correction::CONTRAST).unwrap_or(0.0) as f32,
                    saturation: pass.number(color_correction::SATURATION).unwrap_or(1.0) as f32,
                    temperature: pass.number(color_correction::TEMPERATURE).unwrap_or(0.0) as f32,
                    tint: pass.number(color_correction::TINT).unwrap_or(0.0) as f32,
                    _pad: [0.0; 3],
                };
                self.run_single_uniform_pass(
                    &self.color_correction_pipeline,
                    bytemuck::bytes_of(&u),
                    &current.create_view(&wgpu::TextureViewDescriptor::default()),
                    &other.create_view(&wgpu::TextureViewDescriptor::default()),
                );
                std::mem::swap(&mut current, &mut other);
            } else if pass.type_id == crop::TYPE_ID {
                let u = CropUniforms {
                    left: pass.number(crop::LEFT).unwrap_or(0.0) as f32,
                    right: pass.number(crop::RIGHT).unwrap_or(0.0) as f32,
                    top: pass.number(crop::TOP).unwrap_or(0.0) as f32,
                    bottom: pass.number(crop::BOTTOM).unwrap_or(0.0) as f32,
                };
                self.run_single_uniform_pass(
                    &self.crop_pipeline,
                    bytemuck::bytes_of(&u),
                    &current.create_view(&wgpu::TextureViewDescriptor::default()),
                    &other.create_view(&wgpu::TextureViewDescriptor::default()),
                );
                std::mem::swap(&mut current, &mut other);
            }
        }

        current
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
        assert_eq!(
            self.output_format, OUTPUT_FORMAT,
            "render_to_rgba reads back {OUTPUT_FORMAT:?}; this compositor was built for a different \
             output format, so use render_to_view instead"
        );
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
