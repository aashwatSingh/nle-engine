# Architecture — M0

## Crate map and dependency direction

Eight subsystems from the spec, mapped to seven crates (command + timeline's
undo system are one crate; the UI/app layer is the eighth):

```
media  (no internal deps)
  ^
  |
timeline  (depends on: media)
  ^   ^
  |   |
  |   +---- audio  (depends on: timeline, media, render [ParamSchema only])
  |
  +---- render  (depends on: timeline, media)
  |
  +---- command  (depends on: timeline, media)
  |
  +---- project  (depends on: timeline, media)
  |
  +---- app  (depends on: everything)
```

**Rule:** dependencies only point toward `media`. Nothing in `media` or
`timeline` may import `render`, `audio`, `project`, `command`, or `app`. This
is what keeps the timeline data model usable by the export pipeline, the
persistence layer, and the compositor without any of them needing each
other. `audio` depending on `render` is the one deliberate exception — it
reuses `render::ParamSchema`/`ParamType` for its effect parameter shape
rather than duplicating that type; `render` does not depend back on `audio`,
so this stays acyclic.

Why `EffectInstance` (the data stored on a clip) lives in `timeline`, not
`render`: `ClipInstance` embeds effect instances directly, and `timeline`
cannot depend on `render` without creating a cycle (`render` already needs
`timeline` to read clips and build the render graph). `render` interprets an
effect instance's meaning by looking up its `effect_type` string in an
`EffectRegistry` — the same pattern real NLEs use to keep project data valid
even when a plugin isn't loaded.

## Subsystem responsibilities (spec section 4)

| Subsystem | Crate | M0 status |
|---|---|---|
| Media layer | `media` | Types only: `MediaAsset`, `Frame`, `DecoderPool` trait. No FFmpeg FFI yet. |
| Timeline data model | `timeline` | Types + working time math + `check_no_overlaps`. `edit_ops::apply` is `todo!()` — M3. |
| Command/undo | `command` | Fully implemented (push, coalescing, bounded history, undo/redo) — simple enough to build for real now. |
| Playback engine | *(none yet)* | No crate yet. This is the M2 deliverable; M0 only fixes the thread/queue shape (see below) so M1/M2 build toward it deliberately. |
| Render graph & effects | `render` | Types only: `EffectDescriptor`, `CompiledFrameGraph`, `ColorConverter` trait. No wgpu yet — that's the M0 spike, kept separate from this crate until it proves out. |
| Audio engine | `audio` | Types only: `MixerGraph`, `TrackStrip`, metering shapes. No CPAL yet. |
| Project persistence | `project` | Fully implemented for schema v1: CBOR encode/decode, atomic save (temp+fsync+rename), schema-version rejection. |
| Export pipeline | *(none yet)* | M8. Depends on render graph + media encode paths not built yet. |

## The play-press-to-pixels path (the hardest question, restated as the contract these crates must satisfy)

**Status: built, with one documented shortfall.** The contract below was
written in M0 before any thread, queue, or GPU code existed. It now maps onto
real code, so each step is annotated with where it lives:

- Steps 1-3 → `app::main`'s `Transport` and `render::GraphCompiler`. The
  `Arc<Project>` clone in step 2 happens at play-press and is *held* for the
  duration, so edits made during playback are not heard or seen until playback
  restarts. Consistent between audio and video, so the two never disagree
  about which version they're playing.
- Step 4 → **mostly.** `playback::SequenceVideoPlayback` keeps one persistent
  decoder per asset (`media_ffmpeg::SourceReader`) on a decode-ahead thread,
  which is what made real-time picture possible — measured at 27x cheaper per
  frame than the reopen-per-frame path it replaced. It can optionally upload
  straight to a GPU texture on that same thread (`SourcePixels::Gpu`, gated on
  an `Option<GpuContext>` passed to `start`) instead of handing back a CPU
  buffer for the UI thread to upload later — `app::main` wires this up, so the
  running editor always takes this path. **Measured real-footage impact: none
  worth claiming.** Three clean back-to-back runs against the same 2560x1588
  file (`crates/playback/tests/real_footage_smoke.rs`,
  `video_holds_frame_rate_on_real_footage{,_with_gpu_upload}`) gave CPU
  starved counts of 13/11/8 and GPU starved counts of 29/12/10 out of ~120
  boundaries per 4s window — GPU is not clearly better, and the per-frame
  `device.create_texture` call (no pooling/reuse) is a real cost the CPU path
  doesn't pay. The **334-starved figure recorded 2026-08-10 does not
  reproduce** under a clean run and was very likely a noisy measurement taken
  under concurrent system load (a release build was running around the same
  time that session), not a steady-state bottleneck — a caution to weigh
  against any single-capture number rather than trust it as ground truth.
  Texture pooling (reuse a per-asset texture instead of allocating one per
  frame) is the next thing to try if this is revisited, but isn't justified
  by evidence yet.
- Steps 5-6 → `playback::SequenceAudioEngine`. Lock-free ring buffer, CPAL
  callback that only pops, playhead derived from consumed samples.
- Step 7 → `render::Compositor`, shared with export (proven pixel-identical,
  M4e).
- Step 8 → `SequenceVideoPlayback::frame_for`, which never blocks: it returns
  the newest due frame, discards older ones, and holds the last picture when
  nothing is ready. Dropped and starved counts surface in the editor's toolbar.

Original contract:

1. UI thread posts a transport command to a **Scheduler** (not yet built).
2. Scheduler clones the current `Arc<timeline::Project>` (cheap — this is
   why `Project` is the unit of versioning, not something more granular).
3. Scheduler asks a `render::RenderGraphCompiler` for a `CompiledFrameGraph`
   at the target tick. This is memoized per `Project` version.
4. Scheduler issues `media::DecodeRequest`s through a `media::DecoderPool`.
   Decoders produce `media::Frame` (GPU texture handle + color metadata +
   PTS + source ref) — the one frame type everything downstream consumes.
5. An audio mixer (not yet built, will implement `audio::MixerEngine`) fills
   a lock-free ring buffer ahead of real time; CPAL's callback only memcpys
   out of it — never allocates, locks, or blocks.
6. The authoritative playhead is derived from audio samples actually
   consumed (an atomic counter inside the CPAL callback), not a UI timer.
7. A compositor (not yet built) walks the `CompiledFrameGraph`, executing
   `render::EffectDescriptor.shader_entry_point` passes per clip, then track
   blend, then `render::ColorConverter::to_delivery_space`.
8. Present picks the composited frame nearest the audio-clock tick; on
   starvation it holds the last frame and increments a dropped-frame
   counter — it never blocks.

Full failure-mode analysis (seek-during-playback races, GPU cross-thread
upload safety, audio underrun policy, live-parameter-edit-during-playback)
is in `docs/risks.md`.

## What's explicitly NOT here yet

Struck-through items were true when this doc was written in M0 and have since
been built; they're kept for the record rather than deleted.

- ~~No FFmpeg FFI, no wgpu, no CPAL~~ — all three integrated (M1/M2).
- ~~No property-based test suite~~ — built on `check_no_overlaps` (M3).
- No plugin ABI — `render::effect.rs` documents why locking one down now
  would be guessing.

### Transitions (schema v3)

`Track::transitions` anchors a `Transition` to a cut, centred on it, so both
clips are sampled **past their own trim points** during it — into their handles,
which is what makes a transition possible at all. `render::graph` emits a
`TransitionRender` carrying the outgoing clip's plan plus a progress value, and
the compositor draws the outgoing layer opaque then the incoming one at
`progress` over it. Standard alpha-over then gives `B*p + A*(1-p)` at alpha 1 —
a mathematically exact cross dissolve with **no dedicated shader and no second
render target**. Drawing both layers at partial opacity, the obvious-looking
alternative, gives `B*p + A*(1-p)^2` and sags visibly in the middle of every
dissolve; `compositor::layers_of` exists to get that ordering right in one place.

`CompiledFrameGraph::media_requests` is what keeps the three renderers honest:
the scrub path, the playback decode thread, and export all ask it what a frame
needs, so none of them can miss a transition's second input. Requests carry
their clip id because two clips can reference the same asset at different source
times, and a decoder cache keyed only by asset would seek back and forth between
them every frame.

Current gaps, in rough order of how much they're felt:

- **GPU-resident decode exists but didn't measurably help.** See step 4 above
  — the editor takes the GPU-upload path today, but clean remeasurement showed
  no real win over the CPU path on the same file. Texture pooling is the
  untried next step if this is revisited.
- **Four transition kinds** (cross dissolve, dip to black, wipe, slide), each
  offered in the timeline's context menu via `TransitionKind::ALL` so a kind
  can't be implemented in the renderer and left unreachable in the editor.
  Wipe and slide are both fully opaque throughout — the transition is *where*
  each clip is drawn, not how transparent it is: wipe uses a dedicated
  `ClipUniforms::wipe_progress`/`wipe_enabled` pair (kept out of
  `MaskUniforms` so a wipe composes with a clip's own mask instead of
  replacing it), and slide is purely a `Transform2D::position` offset with no
  shader involvement at all. **Both are one direction only** (wipe sweeps
  left-to-right, slide enters from the right); other directions each need a
  direction field on `Transition`, which is a schema change rather than a
  rendering one. Dip-to-black used to dip to *transparent*, which read
  correctly on the bottom video track and let the tracks below show through on
  any other one; it now scales the clip's **colour** to zero at full alpha
  (`ClipUniforms::rgb_scale`), so it stays opaque and covers what's underneath.
  Its remaining limitation: the black fills the clip's placement rectangle, not
  the whole frame, so a scaled-down or offset clip dips only itself.
- **No submixes or audio insert effects.** Track faders, pan, and per-track
  metering exist (schema v3), and **fader automation now does too** (schema
  v6): `Track::gain_db` is a `ParamTrack`, evaluated per output frame in
  sequence time when animated and hoisted out of the loop when not, with
  keyframe add/remove buttons on each mixer strip. A v5 project's bare-number
  fader still opens — via a compat deserializer rather than a migration step,
  since decoding happens before migration and a type change fails there first.
  What's still missing is the *routing*: the path is a fixed track-to-master
  one, and arbitrary track -> submix -> master needs a real `MixerGraph`.
- **Bezier tangent handles are draggable but timing-only.** The keyframe
  strip has no value axis (it's a 1D timing lane), so `EditorState::
  set_keyframe_tangents` only lets the horizontal (time-delta) component of
  `Keyframe::tangents` be edited by dragging; the value-delta component stays
  wherever it was. Full 2D tangent editing needs a real value-vs-time curve
  graph, which doesn't exist.
- **Shuttle above 1x is silent and scrub-quality.** The mixer has no
  resampler and the video pipeline only walks forward, so JKL rates other than
  1x run off the wall clock through the on-demand preview path.
- **Scopes exist**: `render::scopes` (`Histogram`, `Waveform`, `Vectorscope`),
  CPU-computed from an already-composited, delivery-encoded frame against the
  Rec.709 matrix, each value checked in `tests/scopes.rs` against what the
  standard dictates (where 100% red sits on the vectorscope, that every
  neutral lands dead centre) rather than against whatever the code produces.
  `app::scopes_panel` reads the live preview texture back
  (`Preview::read_back_rgba`, gated on the panel being open — it blocks on a
  GPU readback, a real cost not worth paying every frame during playback) and
  draws all three as egui images. The GPU-texture-to-scopes plumbing itself is
  covered by an end-to-end test creating the same `COPY_SRC` texture shape
  `Preview` uses and rendering a real frame into it — mutation-tested by
  removing `COPY_SRC` and confirming the test fails with wgpu's own
  validation error, not a vaguer one.
- **Clip-speed audio retiming works for constant speeds.** `audio::timeline_mix`
  resamples per clip (linear interpolation plus a box average when speeding up,
  which is what keeps decimation from aliasing), reading the source position
  through `SpeedCurve::source_delta` so it matches how the video graph advances
  through the same clip. It's **varispeed** — pitch moves with speed, like a
  tape machine and like Premiere with "Maintain Audio Pitch" off; a
  pitch-preserving stretch needs a phase vocoder and is a different feature.
  Two cases stay unsupported and are still reported through
  `ExportStats::audio_clips_with_unsupported_speed`: a *keyframed* speed curve
  (needs integrating a rate curve, which `source_delta` explicitly doesn't do),
  and a *reverse* speed — refused deliberately rather than half-built, because
  the video pipeline only walks forward and reversing the audio alone would
  desync it against picture that isn't reversed.
- **Titles exist, now with real shaping and real family resolution.**
  `ClipSource::Title` carries a `TitleSpec` (text, family, size, colour,
  alignment, position) that `render::text` rasterises to a frame-sized
  straight-alpha RGBA buffer, which the compositor uploads and treats as an
  ordinary source — so a title transforms, dissolves, masks and grades like
  any clip, and exports through the same path it previews through. Each line
  is shaped with `rustybuzz` (GSUB/GPOS: ligatures, Devanagari conjuncts and
  vowel-sign reordering, Arabic joining) before `ab_glyph` rasterises the
  resulting glyphs — verified against real Devanagari reordering using
  `Nirmala.ttc`, the Indic font Windows itself ships, not a synthetic case.
  `FontLibrary` resolves a requested family against the font's own `name`
  table (`ttf-parser`), not just its filename, with an incremental lazy scan
  so an unusual name doesn't cost what parsing every installed font up front
  measured at (~3s for ~350 fonts on this machine). Two limits remain: the
  `name`-table scan only indexes `.ttf`/`.otf`, not `.ttc` collections (so a
  `.ttc`-only family like "Nirmala UI" won't resolve through `FontLibrary`
  today, even though `render::text` can shape and rasterise one when handed
  directly); and the raster is frame-sized, so a title's black-box bounds are
  the whole frame rather than its own text extent.
- ~~**True-peak and EBU R128 loudness are still only types.**~~ Both are real
  now (`audio::loudness`): BS.1770-4 K-weighting with the standard's own filter
  constants derived from the sample rate (so a 44.1 or 96 kHz programme
  measures correctly, not just 48 kHz), 400 ms blocks at 75% overlap, the
  -70 LUFS absolute and -10 LU relative gates, EBU Tech 3342 loudness range,
  and a 4x-oversampled polyphase true peak. `LoudnessAnalyzer` streams, so
  export measures the samples it actually hands the encoder rather than
  estimating from source clips; `ExportStats::loudness` is `None` for a
  video-only export rather than a silence reading, and the editor shows the
  figures on the export result line with a warning above -1 dBTP. Verified
  against the standard's published calibration points (a 1 kHz sine at
  -23 dBFS reads -23.0 LUFS) rather than against this code's own output, and
  every stage mutation-tested.
