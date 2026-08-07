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

This is not implemented yet — no thread, queue, or GPU code exists in M0 —
but every type above was shaped to support this without a structural
rewrite later:

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

- No FFmpeg FFI, no wgpu, no CPAL — those are M1/M2 integration work, kept
  out of M0 so the type layer isn't built to fit an unproven library choice.
- No property-based test suite — `check_no_overlaps` is the primitive it
  will be built on (M3).
- No plugin ABI — `render::effect.rs` documents why locking one down now
  would be guessing.
