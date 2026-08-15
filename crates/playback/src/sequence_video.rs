//! Real-time video playback of an *edited sequence*, paced against the audio
//! master clock (spec 4.4). This is the Scheduler role from
//! `docs/architecture.md`'s play-press-to-pixels walkthrough — steps 3, 4 and
//! 8 — finally applied to a whole sequence rather than the single file
//! `video_playback::VideoPlayback` handles.
//!
//! ## Why this exists
//!
//! The editor's preview used to decode every displayed frame through
//! `media_ffmpeg::decode_frame_at`, which reopens and re-seeks the source file
//! per frame. That is the right call for scrubbing (any tick, any clip, no
//! state to invalidate) but it cannot hold frame rate during playback: at
//! 30fps it means 30 file opens and 30 keyframe-to-target decode runs per
//! second, on the UI thread. Once audio became a real-time master clock, the
//! result was smooth sound with picture falling steadily behind it.
//!
//! Two changes fix that, and both matter:
//!
//! 1. **Persistent decoders.** One `media_ffmpeg::SourceReader` per asset,
//!    walked forward, seeking only when the request actually jumps. Same
//!    primitive the export loop uses, for the same reason.
//! 2. **Decode off the UI thread.** A decode-ahead thread fills a small queue
//!    of prepared frames; the UI thread only pops the one that's due and
//!    uploads it. Present never blocks on a decoder.
//!
//! ## Falling behind is a policy decision, not a failure
//!
//! Spec 4.4 says: when behind, drop composited frames — never drop audio,
//! never stall. This drops them at *decode* time rather than present time: if
//! the next tick to prepare has already slipped behind the clock, the thread
//! jumps forward to the clock's current frame boundary instead of grinding
//! through frames nobody will ever see. Dropping at present time would still
//! pay full decode cost for every discarded frame, so a machine that can't
//! keep up would never recover. `dropped_frame_count` reports it.
//!
//! ## GPU-resident frames, when a GPU context is available
//!
//! `start`'s `gpu` parameter closes the gap the architecture doc's step 4
//! originally called for: given `Arc<Device>` + `Arc<Queue>`, the decode-ahead
//! thread uploads each frame straight to a GPU texture
//! (`render::upload_rgba_to_gpu`) as part of preparing it, so `PreparedSource`
//! carries an already-composite-ready `render::SourceTexture` and the UI
//! thread's per-frame work drops to "insert this into `SourceFrames`" — no
//! copy, no upload, no GPU submission on the thread responsible for keeping
//! egui itself responsive. Measured against real 2560x1588 screen-capture
//! footage (`crates/playback/tests/real_footage_smoke.rs`), the CPU-buffer
//! path showed 334 starved frames over 4 seconds; that's the cost this
//! removes.
//!
//! `gpu: None` keeps the original CPU-buffer path fully intact — every test
//! in this file constructs `SequenceVideoPlayback` that way deliberately
//! (measuring decode throughput shouldn't need a GPU adapter, window, or
//! `wgpu::Instance` to run), and it's also what `Preview::render` (scrubbing)
//! and `export` still use via `Compositor::upload_rgba`, since a scrub or an
//! export frame is decoded once and thrown away — there's no repeated-upload
//! cost to amortize by moving it off-thread.
//!
//! `Device`/`Queue` are `Send + Sync` in wgpu and a `TextureView` keeps its
//! backing texture alive internally, so handing a `SourceTexture` from the
//! decode thread to the UI thread across the same queue every other prepared
//! frame already crosses is safe — see `render::upload_rgba_to_gpu`'s own doc
//! comment for the fuller argument.
//!
//! ## Memory cost of the CPU path (`gpu: None`)
//!
//! The queue still costs `capacity x tracks x width x height x 4` bytes when
//! no GPU context is given — ~50MB for two 1080p tracks, ~200MB at 4K. With a
//! GPU context this cost moves to VRAM instead (the same bytes, just held as
//! GPU textures rather than `Vec<u8>`s in the queue) — not eliminated, but no
//! longer competing with everything else on the CPU heap, and no longer
//! re-copied on every UI-thread present.
//!
//! ## Snapshot semantics
//!
//! Like `SequenceAudioEngine`, this takes an `Arc<Project>` at start and holds
//! it for the duration. Edits made during playback are not seen until
//! playback restarts. Consistent with audio, so picture and sound never
//! disagree about which version of the project they're playing.

use media_ffmpeg::SourceReader;
use render::{BuiltinRegistry, GraphCompiler};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use timeline::{Project, SequenceId, TimeTick};

/// Frames of lookahead. Small on purpose — see the memory note in the module
/// doc. Four frames is ~133ms at 30fps, enough to absorb decode jitter.
const QUEUE_CAPACITY: usize = 4;

/// How far the preparation point may slip behind the clock before the decoder
/// gives up on the frames in between and jumps to the present. Two frame
/// durations, so ordinary jitter doesn't cause skipping.
const SKIP_AHEAD_FRAMES: i64 = 2;

/// `Arc<Device>` + `Arc<Queue>` for uploading decoded frames on the decode
/// thread. A type alias rather than a two-tuple parameter spelled out
/// everywhere, and re-using `render::wgpu`'s re-export (not a fresh `wgpu`
/// dependency) so a caller can't end up with two different wgpu versions in
/// one binary — see that re-export's own doc comment.
pub type GpuContext = (Arc<render::wgpu::Device>, Arc<render::wgpu::Queue>);

/// One source picture, decoded and ready to composite.
///
/// Two shapes, not one field that's sometimes empty: `Cpu` is a plain decoded
/// buffer the caller still has to upload (the `gpu: None` path, and the only
/// shape this type had before GPU-resident frames existed); `Gpu` is already
/// on the device, ready to hand straight to `SourceFrames` with zero further
/// work. Matching on `SourcePixels` at the one call site that consumes it
/// (`app::preview::Preview::render_prepared`) is what makes it impossible to
/// accidentally re-upload a frame that was already uploaded on the decode
/// thread, or to forget to upload one that wasn't.
pub enum SourcePixels {
    Cpu(Arc<Vec<u8>>),
    Gpu(render::SourceTexture),
}

pub struct PreparedSource {
    pub asset: media::MediaAssetId,
    pub pts_ticks: i64,
    pub width: u32,
    pub height: u32,
    pub color: media::ColorMetadata,
    pub pixels: SourcePixels,
}

/// Everything the compositor needs for one timeline frame, except the GPU.
///
/// `tick` is the tick the graph must be compiled at — not the clock's current
/// tick. Compiling at a different tick than the one these pictures were
/// decoded for could ask the compositor for an (asset, pts) pair that isn't
/// here.
pub struct PreparedFrame {
    pub tick: TimeTick,
    pub sources: Vec<PreparedSource>,
    /// True when some track's source was missing or unreadable, so the caller
    /// can distinguish "this frame is genuinely partly empty" from "the
    /// decoder is still catching up".
    pub had_missing_source: bool,
}

pub struct SequenceVideoPlayback {
    queue: Arc<Mutex<VecDeque<PreparedFrame>>>,
    shutdown: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    produced_any: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    starved: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SequenceVideoPlayback {
    /// Starts decoding `sequence` from `start_ticks`, pacing against `clock`
    /// (in practice `SequenceClock::current_tick`).
    ///
    /// `gpu`: see the module doc's "GPU-resident frames" section. `None` is
    /// always safe and is what every test in this file passes; the running
    /// app passes `Some` so playback stops paying a per-frame UI-thread
    /// upload cost.
    pub fn start(
        project: Arc<Project>,
        sequence: SequenceId,
        asset_paths: HashMap<media::MediaAssetId, PathBuf>,
        start_ticks: i64,
        clock: impl Fn() -> i64 + Send + 'static,
        gpu: Option<GpuContext>,
    ) -> Self {
        let queue: Arc<Mutex<VecDeque<PreparedFrame>>> = Arc::new(Mutex::new(VecDeque::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let produced_any = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicU64::new(0));
        let starved = Arc::new(AtomicU64::new(0));

        let t_queue = queue.clone();
        let t_shutdown = shutdown.clone();
        let t_finished = finished.clone();
        let t_produced = produced_any.clone();
        let t_dropped = dropped.clone();

        let thread = std::thread::spawn(move || {
            let Some(seq) = project.sequences.iter().find(|s| s.id == sequence) else {
                t_finished.store(true, Ordering::SeqCst);
                return;
            };
            let ticks_per_frame = seq.settings.frame_rate.ticks_per_frame().max(1);
            let duration = seq.duration().0;
            let compiler = GraphCompiler::new(BuiltinRegistry::default());
            // Cache the open/failed-to-open decision per asset so an
            // unreadable file isn't retried (and re-erroring) every frame.
            let mut readers: ReaderCache = HashMap::new();

            // Snap the start to the frame grid so prepared ticks line up with
            // the ticks export and scrubbing would use for the same frame.
            let mut next_tick = (start_ticks / ticks_per_frame) * ticks_per_frame;

            loop {
                if t_shutdown.load(Ordering::Relaxed) {
                    return;
                }
                if next_tick >= duration {
                    // Nothing left to prepare. Say so, so an empty queue at
                    // the end of the sequence isn't mistaken for starvation.
                    t_finished.store(true, Ordering::SeqCst);
                    return;
                }
                if t_queue.lock().unwrap().len() >= QUEUE_CAPACITY {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                }

                // Behind the clock? Skip to the present rather than decoding
                // frames that are already too late to show.
                let now = clock();
                if next_tick < now - SKIP_AHEAD_FRAMES * ticks_per_frame {
                    let target = (now / ticks_per_frame) * ticks_per_frame;
                    let skipped = (target - next_tick) / ticks_per_frame;
                    t_dropped.fetch_add(skipped.max(0) as u64, Ordering::Relaxed);
                    next_tick = target;
                    continue;
                }

                let tick = TimeTick(next_tick);
                let prepared = match compiler.compile(&project, sequence, tick) {
                    Some(graph) => {
                        prepare(&graph, &project, &asset_paths, &mut readers, tick, gpu.as_ref())
                    }
                    // The sequence exists (checked above), so `None` means the
                    // compiler rejected it mid-playback — e.g. a nesting
                    // cycle. Emit an empty frame rather than stalling.
                    None => PreparedFrame { tick, sources: vec![], had_missing_source: true },
                };

                t_queue.lock().unwrap().push_back(prepared);
                t_produced.store(true, Ordering::Relaxed);
                next_tick += ticks_per_frame;
            }
        });

        SequenceVideoPlayback {
            queue,
            shutdown,
            finished,
            produced_any,
            dropped,
            starved,
            thread: Some(thread),
        }
    }

    /// The frame to display at `clock_tick`, or `None` to hold whatever is
    /// already on screen. Never blocks on the decoder.
    ///
    /// When several queued frames are already due (the UI repainted slower
    /// than the sequence's frame rate, or a decode ran long), the older ones
    /// are discarded and counted — showing them would only put picture
    /// further behind sound.
    pub fn frame_for(&self, clock_tick: i64) -> Option<PreparedFrame> {
        let mut queue = self.queue.lock().unwrap();
        if queue.is_empty() {
            // Before the first frame is ready, and after the sequence has
            // ended, an empty queue is expected — not the decoder falling
            // behind. Counting those would make this stat useless for
            // spotting the real thing. (Same mistake, and same fix, as
            // end-of-stream silence in the audio engine's underrun count.)
            if self.produced_any.load(Ordering::Relaxed) && !self.finished.load(Ordering::Relaxed) {
                self.starved.fetch_add(1, Ordering::Relaxed);
            }
            return None;
        }

        let mut chosen: Option<PreparedFrame> = None;
        while queue.front().is_some_and(|f| f.tick.0 <= clock_tick) {
            if chosen.is_some() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            chosen = queue.pop_front();
        }
        chosen
    }

    /// Frames decoded-but-skipped plus frames never decoded because the
    /// decoder had to jump forward to catch up.
    pub fn dropped_frame_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Times the UI asked for a frame and the decoder had none ready, while
    /// the sequence was still running. Nonzero means picture is not holding
    /// rate on this machine.
    pub fn starved_count(&self) -> u64 {
        self.starved.load(Ordering::Relaxed)
    }

    /// True once the decoder has prepared every frame in the sequence.
    pub fn has_finished_decoding(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }
}

impl Drop for SequenceVideoPlayback {
    fn drop(&mut self) {
        // Joining rather than detaching: the decode thread holds open file
        // handles and a `SourceReader` per asset, and playback stops and
        // starts often. A detached thread would keep decoding into a queue
        // nobody reads until it happened to notice.
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// One persistent decoder per (asset, clip) — see `render::MediaRequest::clip`
/// for why the clip has to be part of the key.
type ReaderCache = HashMap<(media::MediaAssetId, timeline::ClipInstanceId), Option<SourceReader>>;

/// Decodes every source the graph names at `tick`. Missing or unreadable
/// sources are reported through `had_missing_source` rather than failing the
/// frame — one bad file shouldn't stop playback.
///
/// `gpu`: when given, each source is uploaded to a GPU texture right here, on
/// the decode thread, instead of being handed back as a CPU buffer for
/// someone else to upload later.
fn prepare(
    graph: &render::CompiledFrameGraph,
    project: &Project,
    asset_paths: &HashMap<media::MediaAssetId, PathBuf>,
    readers: &mut ReaderCache,
    tick: TimeTick,
    gpu: Option<&GpuContext>,
) -> PreparedFrame {
    let mut sources = Vec::new();
    let mut had_missing_source = false;

    // `media_requests` walks the graph — including transitions' outgoing layers
    // and nested sequences — so a dissolve gets both of its inputs decoded
    // without this loop having to know transitions exist.
    for req in graph.media_requests() {
        let asset = req.asset;
        let color = project
            .assets
            .iter()
            .find(|a| a.id == asset)
            .and_then(|a| a.video.as_ref())
            .map(|v| v.color);
        let (Some(color), Some(path)) = (color, asset_paths.get(&asset)) else {
            had_missing_source = true;
            continue;
        };

        let reader = readers
            .entry((asset, req.clip))
            .or_insert_with(|| SourceReader::open(path).ok());
        let Some(reader) = reader.as_mut() else {
            had_missing_source = true;
            continue;
        };

        match reader.frame_at(req.source_pts_ticks) {
            Some(frame) => {
                // Cloned either way because `frame_at` hands out a borrow of
                // the reader's own current frame, which it needs to keep for
                // the next request.
                let pixels = match gpu {
                    Some((device, queue)) => SourcePixels::Gpu(render::upload_rgba_to_gpu(
                        device,
                        queue,
                        &frame.rgba,
                        frame.width,
                        frame.height,
                        color,
                    )),
                    None => SourcePixels::Cpu(Arc::new(frame.rgba.clone())),
                };
                sources.push(PreparedSource {
                    asset,
                    pts_ticks: frame.pts_ticks,
                    width: frame.width,
                    height: frame.height,
                    color,
                    pixels,
                });
            }
            None => had_missing_source = true,
        }
    }

    PreparedFrame { tick, sources, had_missing_source }
}
