//! Bridges the edited `Project` to a picture in the preview panel: compiles
//! the render graph at the playhead (`render::GraphCompiler`, M4b), decodes
//! whatever source frame each active clip needs, uploads it, and composites
//! with the real GPU `Compositor` (M4d) — the same pipeline `play_timeline`
//! uses, not a simplified stand-in.
//!
//! Two paths in, one compositing tail:
//!
//! - [`Preview::render`] decodes on demand through
//!   `media_ffmpeg::decode_frame_at`, which reopens and seeks the source file
//!   per call. That's the right trade for **scrubbing**: any tick, any clip,
//!   no decoder state to invalidate when the playhead jumps. It cannot hold
//!   frame rate during playback, which is why it no longer has to.
//! - [`Preview::render_prepared`] takes frames already decoded by
//!   `playback::SequenceVideoPlayback` on its own thread, using persistent
//!   per-asset decoders. That's the **playback** path.
//!
//! The split matters: scrubbing wants random access and doesn't care about
//! throughput, playback wants throughput and is always moving forward. One
//! implementation serving both is what made picture fall behind sound.
//!
//! Both still upload a CPU RGBA buffer to the GPU per frame per clip.
//! `docs/architecture.md` step 4 wants decoders producing GPU texture handles
//! end to end, which would remove the upload entirely — not done here.
//!
//! Color note: `Compositor` writes already gamma-encoded bytes into a plain
//! `Rgba8Unorm` target (never `*Srgb` — see `render::compositor`'s own
//! comment on why a `*Srgb` target would double-encode). egui's
//! `register_native_texture` requires `Rgba8UnormSrgb` specifically, so
//! `PreviewTexture` allocates one physical texture and exposes two views
//! aliased to each format: the compositor renders into the `Unorm` view
//! (no conversion on write, since it already wrote the encoded byte
//! values), egui samples the `*Srgb`-aliased view of those same bytes
//! (correctly decoding them back to linear for egui's own blending). Same
//! bytes, two interpretations — not a copy, not a re-encode.

use render::wgpu;
use render::{BuiltinRegistry, Compositor, DeliverySpace, GraphCompiler, SourceFrames};
use std::collections::HashMap;
use std::path::PathBuf;
use timeline::{Project, SequenceId, TimeTick};

const PREVIEW_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const PREVIEW_FORMAT_SRGB: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

pub struct PreviewTexture {
    // Held only to keep the underlying GPU resource alive for as long as
    // the views below reference it, and `read_back_rgba` reads it directly
    // for the video scopes.
    texture: wgpu::Texture,
    render_view: wgpu::TextureView,
    pub egui_id: egui::TextureId,
    pub width: u32,
    pub height: u32,
}

pub struct Preview {
    compositor: Compositor,
    compiler: GraphCompiler<BuiltinRegistry>,
    sources: SourceFrames,
    texture: Option<PreviewTexture>,
    pub last_render_ok: bool,
}

impl Preview {
    pub fn new(device: std::sync::Arc<wgpu::Device>, queue: std::sync::Arc<wgpu::Queue>) -> Self {
        Preview {
            compositor: Compositor::with_output_format(device, queue, PREVIEW_FORMAT),
            compiler: GraphCompiler::new(BuiltinRegistry::default()),
            sources: SourceFrames::default(),
            texture: None,
            last_render_ok: true,
        }
    }

    /// Reads the current preview picture back to CPU RGBA, for the video
    /// scopes. `None` before anything has ever been composited.
    ///
    /// Blocking — `render::read_texture_rgba` waits for the GPU copy to
    /// land — so this is meant to be called when a scopes panel is open and
    /// the user wants to check levels, not unconditionally every frame
    /// during playback. The scopes panel gates on its own visibility for
    /// exactly that reason.
    pub fn read_back_rgba(&self, device: &wgpu::Device, queue: &wgpu::Queue) -> Option<(Vec<u8>, u32, u32)> {
        let tex = self.texture.as_ref()?;
        let rgba = render::read_texture_rgba(device, queue, &tex.texture, tex.width, tex.height);
        Some((rgba, tex.width, tex.height))
    }

    /// The texture as it currently stands, without rendering anything into it.
    ///
    /// This is how "hold the last frame" works during playback: when the
    /// decoder has nothing due yet, the caller displays this instead of
    /// blocking or clearing to black. The GPU texture still holds the last
    /// composite, so there is nothing to redo.
    pub fn current_texture(&self) -> Option<(egui::TextureId, u32, u32)> {
        self.texture.as_ref().map(|t| (t.egui_id, t.width, t.height))
    }

    /// Renders `project`'s sequence at `at`, decoding each needed source frame
    /// on demand. The scrubbing path — see the module doc.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        egui_renderer: &mut egui_wgpu::Renderer,
        project: &Project,
        seq_id: SequenceId,
        at: TimeTick,
        asset_paths: &HashMap<media::MediaAssetId, PathBuf>,
    ) -> Option<(egui::TextureId, u32, u32)> {
        Self::composite(
            &mut self.compositor,
            &mut self.sources,
            &self.compiler,
            &mut self.texture,
            &mut self.last_render_ok,
            device,
            egui_renderer,
            project,
            seq_id,
            at,
            |compositor, sources, graph| {
                // Driven by `media_requests` rather than a hand-walk of
                // `track_plans`, so transitions' second input and nested
                // sequences are covered here exactly as they are in playback
                // and export.
                for req in graph.media_requests() {
                    let Some(asset_meta) = project.assets.iter().find(|a| a.id == req.asset) else {
                        continue;
                    };
                    let Some(path) = asset_paths.get(&req.asset) else { continue };
                    let Some(video) = &asset_meta.video else { continue };
                    if let Ok(frame) = media_ffmpeg::decode_frame_at(path, req.source_pts_ticks) {
                        let tex_handle = compositor.upload_rgba(
                            &frame.rgba,
                            frame.width,
                            frame.height,
                            video.color,
                        );
                        sources.insert(req.asset, frame.pts_ticks, tex_handle);
                    }
                }
            },
        )
    }

    /// Composites a frame the playback decoder already prepared. The playback
    /// path — see the module doc.
    ///
    /// The graph is compiled at `frame.tick`, **not** at the caller's current
    /// playhead: these pictures were decoded for that tick, and compiling at
    /// any other one risks asking the compositor for an (asset, pts) pair
    /// that isn't in this frame.
    /// Takes `frame` by value (not `&PreparedFrame`) specifically so a
    /// `SourcePixels::Gpu` source can be *moved* straight into `SourceFrames`
    /// — `render::SourceTexture` isn't `Clone` (a GPU texture view has no
    /// cheap copy), so a borrowed frame would force re-uploading it, which is
    /// exactly the per-frame UI-thread cost `playback::SequenceVideoPlayback`
    /// uploading on the decode thread exists to remove. A `Cpu` source still
    /// goes through `compositor.upload_rgba` here, unavoidably — it was never
    /// uploaded anywhere, by design, when no GPU context was given at
    /// `SequenceVideoPlayback::start` (see that module's doc).
    pub fn render_prepared(
        &mut self,
        device: &wgpu::Device,
        egui_renderer: &mut egui_wgpu::Renderer,
        project: &Project,
        seq_id: SequenceId,
        frame: playback::PreparedFrame,
    ) -> Option<(egui::TextureId, u32, u32)> {
        let tick = frame.tick;
        Self::composite(
            &mut self.compositor,
            &mut self.sources,
            &self.compiler,
            &mut self.texture,
            &mut self.last_render_ok,
            device,
            egui_renderer,
            project,
            seq_id,
            tick,
            |compositor, sources, _graph| {
                for src in frame.sources {
                    match src.pixels {
                        playback::SourcePixels::Gpu(tex) => {
                            sources.insert(src.asset, src.pts_ticks, tex);
                        }
                        playback::SourcePixels::Cpu(rgba) => {
                            let tex_handle =
                                compositor.upload_rgba(&rgba, src.width, src.height, src.color);
                            sources.insert(src.asset, src.pts_ticks, tex_handle);
                        }
                    }
                }
            },
        )
    }

    /// Shared tail: size the target, compile the graph, let `fill` supply the
    /// source textures however it likes, composite.
    ///
    /// Takes its state as separate `&mut` fields rather than `&mut self` so
    /// `fill` can borrow the compositor and the source table at once.
    #[allow(clippy::too_many_arguments)]
    fn composite(
        compositor: &mut Compositor,
        sources: &mut SourceFrames,
        compiler: &GraphCompiler<BuiltinRegistry>,
        texture: &mut Option<PreviewTexture>,
        last_render_ok: &mut bool,
        device: &wgpu::Device,
        egui_renderer: &mut egui_wgpu::Renderer,
        project: &Project,
        seq_id: SequenceId,
        at: TimeTick,
        fill: impl FnOnce(&mut Compositor, &mut SourceFrames, &render::CompiledFrameGraph),
    ) -> Option<(egui::TextureId, u32, u32)> {
        let sequence = project.sequences.iter().find(|s| s.id == seq_id)?;
        let (width, height) = (
            sequence.settings.width.max(1),
            sequence.settings.height.max(1),
        );

        if texture.as_ref().map(|t| (t.width, t.height)) != Some((width, height)) {
            Self::recreate_texture(texture, device, egui_renderer, width, height);
        }
        let tex = texture.as_ref()?;

        let Some(graph) = compiler.compile(project, seq_id, at) else {
            *last_render_ok = false;
            return Some((tex.egui_id, width, height));
        };

        sources.clear();
        fill(compositor, sources, &graph);

        compositor.render_to_view(&graph, sources, DeliverySpace::Rec709, &tex.render_view);
        *last_render_ok = true;
        Some((tex.egui_id, width, height))
    }

    /// `slot` (not `texture`) because a local `texture` below holds the new
    /// GPU resource this writes into it.
    fn recreate_texture(
        slot: &mut Option<PreviewTexture>,
        device: &wgpu::Device,
        egui_renderer: &mut egui_wgpu::Renderer,
        width: u32,
        height: u32,
    ) {
        if let Some(old) = slot.take() {
            egui_renderer.free_texture(&old.egui_id);
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: PREVIEW_FORMAT,
            // COPY_SRC in addition to the two this always needed: `Preview::
            // read_back_rgba` reads the already-composited picture straight
            // off this texture for the video scopes, rather than
            // recompositing through `Compositor::render_to_rgba` — the
            // picture on screen and the picture the scopes measure must be
            // the literal same bytes, and re-rendering risks them silently
            // drifting apart (a race on a live edit, a rounding difference).
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[PREVIEW_FORMAT_SRGB],
        });
        let render_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(PREVIEW_FORMAT),
            ..Default::default()
        });
        let srgb_view = texture.create_view(&wgpu::TextureViewDescriptor {
            format: Some(PREVIEW_FORMAT_SRGB),
            ..Default::default()
        });
        let egui_id =
            egui_renderer.register_native_texture(device, &srgb_view, wgpu::FilterMode::Linear);
        *slot = Some(PreviewTexture {
            texture,
            render_view,
            egui_id,
            width,
            height,
        });
    }
}
