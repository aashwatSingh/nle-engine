# Decisions log

## 2026-08-07 — M5 scope: four real effects + masks, not spec 4.5's full list

Spec 4.5's built-in effect set is ~15 items plus a full masking system and a
titling/graphics layer. Building that entire list at M4's level of rigor
(real shaders, real pixel-verified tests, not stubs) in one pass isn't
credible, so M5 shipped a representative, real slice and left the rest
explicitly open rather than half-implementing everything.

**Shipped, with real GPU shaders and pixel-verified tests (26 tests):**
- **Gaussian Blur** — separable two-pass kernel, `SpatiallyLocal` (the first
  effect to actually exercise that `EffectLocality` distinction from M0).
- **Color Correction** — exposure, contrast, saturation, temperature, tint,
  as *one* effect with five params — matching how Lumetri is actually one
  effect with many params in real NLEs, not ten separate effect instances.
  This covers a meaningful slice of spec 4.5's "Lumetri-class color" without
  claiming to be Lumetri.
- **Crop** — geometry group.
- **Mask** — rectangle + ellipse with feather, applied clip-level (gates the
  whole clip, evaluated in the clip's own source-UV space so it travels with
  the clip when transformed — verified by
  `mask_moves_with_the_clip_not_with_the_sequence`).

**Architecture change this forced, which is the more durable part of the
work:** a clip with pixel effects now goes through a three-stage pipeline
(`fs_prepare` -> effect chain via ping-pong intermediates -> `fs_clip`
placement) instead of M4's single fused convert+place pass. A clip with no
pixel effects still uses the M4 fast path unchanged
(`gaussian_blur_radius_zero_matches_the_fast_path` pins that the two paths
produce numerically identical output). This is the seam the rest of spec
4.5's effect list plugs into — adding curves or HSL secondary later is
"write a shader + register it," not another compositor rewrite.

**Explicitly NOT built — real gaps, not oversights:**
- **Curves, vibrance, highlights/shadows/whites/blacks, HSL secondary, LUT
  loading** — the rest of "Lumetri-class color." Curves in particular need
  UI (a spline editor) as much as shader work.
- **Mirror, sharpen** — small, but genuinely not done; sharpen would reuse
  the blur infrastructure (unsharp mask) as a natural follow-up.
- **Transitions** (cross dissolve, dip to black/white, wipe, slide) — these
  need two clips active simultaneously on one track during the overlap
  region, which the timeline model and render graph don't support yet (the
  graph compiler picks exactly one active clip per track). This is a data
  model change, not just a shader — sized more like a graph-compiler task
  than an effect.
- **Text/titling** — a separate graphics layer, not a pixel effect on video.
- **Pen-tool bezier masks** — only rectangle/ellipse exist. A bezier mask
  needs point-in-polygon evaluation (or a rasterized coverage texture) in
  the shader, plus a fair amount of UI for the pen tool itself.
- **Effect Controls panel** — this is UI. There is still no UI at all in
  this project; every effect here is exercised through hand-built
  `timeline::EffectInstance` values in tests and demos, not through anything
  a user could click.

**One real simplification inside what WAS built, stated plainly:** the blur
shader operates on straight (non-premultiplied) alpha, which can fringe
colour at a semi-transparent edge inside the blurred radius. The correct fix
(premultiply before blur, un-premultiply after) is real follow-up work, not
done here — noted in the shader itself, not hidden.

## 2026-08-07 — M4 compositing: conventions fixed, and why each one

Five decisions that the rest of the renderer now depends on. Recording them
because each had a defensible alternative and picking differently later
would be a rewrite rather than a tweak.

**1. Track order is bottom-to-top (`tracks[0]` is the bottom layer).**
Matches how V1/V2/V3 read in every NLE. The compiler emits `track_plans` in
that order and the compositor draws in that order, so "the list order *is*
the composite order" — no separate z-index to keep consistent.

**2. Effect keyframes are clip-relative, not absolute sequence time.**
A `ParamTrack` on a clip's effect is evaluated at `tick -
clip.timeline_in`. This is what makes a clip carry its own animation when
it's moved or rippled. The alternative (absolute ticks) would mean every
ripple edit silently re-times every animation downstream of it — which is
the kind of bug that only shows up after a long edit session and is
miserable to attribute. Pinned by
`effect_keyframes_are_clip_relative_so_moving_a_clip_carries_its_animation`.

**3. Blending happens in linear light, in a 16-bit-float working space.**
Non-negotiable per spec 4.5, and now actually verified rather than assumed:
`opacity_blends_the_top_track_over_the_bottom` asserts a 50% white-over-black
blend produces ~180/255, not 128. 128 would mean blending in encoded space —
the classic cause of "why do my dissolves look muddy". The working target is
never clamped mid-pipeline, so out-of-gamut negatives from wide-gamut
sources survive to the delivery pass, where clamping happens once.

**4. The Rec.709 transfer function uses the camera OETF, not a display
EOTF.** So `linear_to_rec709(rec709_to_linear(x)) == x` exactly, making a
709-in/709-out edit with no colour work a true no-op — verified on both CPU
(`rec709_identity_conversion_is_a_true_no_op`) and GPU
(`rec709_source_round_trips_through_the_gpu_unchanged`). A display-referred
pipeline using the ~2.4 EOTF is an equally valid design; the difference
manifests as a subtle global contrast shift, so it's called out in
`color.rs` rather than left implicit.

**5. The WGSL colour math mirrors `color.rs`, and the matrices are passed in
from it.** The transfer functions are necessarily duplicated (CPU reference
vs. GPU implementation), but the gamut matrices are uploaded as uniforms
from the tested CPU constants rather than hardcoded in the shader —
specifically because a transposed or subtly wrong matrix still looks
plausible and is the hardest kind of colour bug to see. The GPU round-trip
test is what catches the transfer functions drifting.

**Deferred, deliberately:** HDR (PQ/HLG) is *refused* by
`transfer_to_linear`/`to_delivery` rather than approximated — it needs a
tone-mapping policy and nit target, which are product decisions, and
silently emitting wrong-looking HDR is worse than a clear error. Effects
other than Transform are M5; the compiler already resolves them generically
and counts unrecognised ones in `unknown_effects` rather than dropping them
silently.

## 2026-08-07 — Fourth/fifth real bugs: found by the M3 property-based test suite, fixed with a universal safety net

Building the property-based test suite (spec 4.2/8: "run these tests
against randomized edit sequences of length 1000+") immediately found two
more real corruption bugs in the edit operations it was written to police:

**Bug 4 — sync-lock ripple could compress a clip on top of unrelated,
pre-existing content.** `ripple_shift`'s fix for the spanning-clip case
(the "third bug" below it in this log, chronologically earlier) computes a
new, compressed position for a clip that spans the ripple point. That new
position can land exactly on top of a *different* clip already sitting on
the same track — one that was never touched by the ripple at all, just
structurally in the way. Found on a track dense with prior small inserts
(the fuzzer's favorite move: repeatedly `Insert` 10-tick clips at position
0), where the compressed landing spot happened to already be occupied.

**Bug 5 — `TrimSlide` could produce a similar corruption** through a
different interaction the fuzzer found but didn't fully pin to one root
cause in the time available (source ranges were observed drifting to
implausible values like `source_in: -289` under heavy repeated sliding,
which is itself only a symptom of the model not bounds-checking against a
real asset's duration — a separate, expected limitation of this pure
data-model layer, not this bug).

**Fix (for both, and any future one like them):** rather than chase every
individual interaction bug in every operation, `apply()` now validates the
*entire* resulting project — every track, `check_no_overlaps` — before
ever returning it, rejecting with `EditError::WouldOverlap` if anything
is wrong. Storage order is normalized (sorted by `timeline_in`) first,
since operations like `TrimSlide` mutate a clip's position in place
without re-sorting, and checking unsorted storage order directly would
flag harmless reordering as a fake violation.

**Why this is the right fix, not a cop-out:** for a pure function whose
entire contract is `(Project, EditOp) -> Result<Project, EditError>`, "an
operation that would corrupt state returns Err instead of Ok" is exactly
the correctness bar the spec asks for — spec 4.2 never promises every
*conceivable* edit succeeds, only that accepted edits preserve the
invariants. This converts "undiscovered corruption slips through" into
"an edge case we haven't individually reasoned through gets rejected
instead of silently corrupting the project," which is the safe failure
mode. It doesn't relieve pressure to keep improving individual operations'
logic (a `TrimSlide` that spuriously rejects valid edits because of this
is a real usability bug worth fixing later with more targeted logic) — it
just guarantees the one failure mode the spec cares most about (silent
corruption) can't happen regardless of what future interaction bugs turn up.

## 2026-08-07 — Third real bug: audio seek could silently discard all post-seek audio (race)

Found while starting M3 and re-running the full test suite as a sanity
check: `seek_resets_clock_to_target_and_keeps_playing` failed consistently
(not flaky — same failure every run), with the audio clock freezing exactly
at the seek target forever, zero underruns logged, `has_ended()` true
almost immediately.

**Root cause:** `AudioEngine`'s seek used a shared epoch counter — bump it,
and the real-time callback flushes (discards) whatever's in the ring buffer
on its next invocation, to clear stale pre-seek audio. The bug: the
decode-ahead thread races far ahead of real time (it decoded and pushed an
entire remaining ~1.8s of a 3s test clip in microseconds after seeking), so
it could — and reliably did, given how CPU-cheap decoding a small audio
file is relative to a ~10ms callback period — push *all* of the fresh
post-seek audio into the ring buffer before the callback's one-time flush
ran. The flush doesn't distinguish "stale, pre-seek" from "fresh,
post-seek" — it just clears everything currently in the buffer — so it
discarded the real post-seek audio right along with the stale audio it was
meant to clear, and since decode had already reached the file's actual end
producing it, there was nothing left to replace what got thrown away.

**Fix:** added `flush_acked_epoch`, set by the callback immediately after
it performs the flush. The decode-ahead thread now bumps the epoch and
*waits* (bounded, 200ms ceiling) for the callback to acknowledge that
epoch before repositioning the decoder and pushing any post-seek content —
so the flush can only ever discard stale audio, never fresh audio, because
none exists yet in the buffer when the flush runs.

**Why this one is worth flagging on its own:** the original code even had
a doc comment anticipating "some stale audio might play right after a
seek" — but that framing assumed the failure mode was *too little*
flushing (leftover staleness), when the actual failure mode empirically
was the opposite: *too aggressive* flushing racing ahead of correctness
and eating audio that hadn't even had a chance to be stale yet. Recorded
because the lesson generalizes: a race's actual failure mode can invert the
one you designed against, and "I already wrote a caveat comment about
this" is not the same as having verified the caveat is the right one.

## 2026-08-07 — Two real bugs found building M2, both fixed at the media_ffmpeg layer

Found while getting `play_clip` (the M2 capstone demo) to run cleanly to the
end of an 8-second test clip — not from fuzzing or adversarial input, from
completely ordinary use.

**Bug 1 — `ffmpeg_next`'s `PacketIter` can hang forever, not just on corrupt
files.** `Input::packets()`'s `Iterator::next()` only stops on the exact
`Error::Eof` variant; any other error from `av_read_frame` (which real,
non-corrupt files can legitimately return at true end-of-stream depending on
container/codec specifics) makes it retry forever in a tight loop — no
panic, no error, 100% CPU, forever. Reproduced empirically: audio decode on
`test_playback_demo.mp4` hung exactly this way near end-of-stream. Spec
section 8 requires ingest to never hang — this violated that on ordinary
input, not even adversarial input. Fixed by adding `read_next_packet` /
`read_next_packet_for_stream` in `media_ffmpeg/src/lib.rs`, which treat *any*
read error as end-of-stream, and routing every packet-read loop in the crate
through them instead of `Input::packets()`.

**Bug 2 — `seek()` only did half of spec 4.1's seek algorithm.** The spec
text is explicit: "seek to nearest preceding keyframe, decode forward,
present target frame." The original `VideoDecoderStream::seek` /
`AudioDecoderStream::seek` only did the container-level keyframe seek and
stopped — for a sparse-keyframe encode (found empirically: a libx264 test
fixture with exactly **one** keyframe across its entire 8-second, 240-frame
duration, from unspecified `-g`/GOP settings), that meant seeking to *any*
point in the file silently snapped back to frame 0, and every subsequent
`next_frame()`/`next_samples()` call replayed from the start instead of the
requested position. Fixed by decoding-and-discarding forward inside `seek()`
until reaching the actual target: exact per-frame for video
(`pending_frame` stash), coarser whole-chunk discarding for audio
(`discard_before_ticks`) — documented as a bounded, honest imprecision
(up to one resampled chunk's duration, ~10-40ms) rather than pretending it's
sample-exact.

**Why this matters beyond the fix:** this is exactly the kind of thing that
would have shipped invisibly in a demo that only ever seeks near real
keyframes, and only surfaced because the M2 capstone demo actually ran a
full clip end-to-end with real seeks. Reinforces the project's own
"verify by measuring, not reading" lesson from prior projects — the bug was
invisible in code review and only found by running the real thing.


## 2026-08-07 — FFmpeg 7.1 (BtbN win64-gpl-shared), not 8.1; requires libclang + explicit MinGW target triple

**Decision:** `media_ffmpeg` links against BtbN's `ffmpeg-n7.1-latest-win64-gpl-shared-7.1` build (extracted to
`%USERPROFILE%\tools\ffmpeg-n7.1-latest-win64-gpl-shared-7.1`), not the newer 8.1 build also downloaded during
this session. Build requires three env vars set: `FFMPEG_DIR` (pointing at that directory),
`LIBCLANG_PATH` (pointing at a standalone libclang.dll — see below), and
`BINDGEN_EXTRA_CLANG_ARGS=--target=x86_64-w64-mingw32 -I<mingw64>/x86_64-w64-mingw32/include -I<mingw64>/lib/gcc/x86_64-w64-mingw32/16.1.0/include`.

**Why 7.1 not 8.1:** `ffmpeg-sys-next` 7.1.3 (the crate version compatible
with our Rust toolchain at the time) unconditionally binds `avfft.h`, which
FFmpeg 8.x removed. Building against 8.1 failed with a confusing fallback
error (`/usr/include/libavcodec/avfft.h not found`) that looked at first
like a path-detection bug — it was actually a crate/library version
mismatch. If `ffmpeg-next`/`ffmpeg-sys-next` publishes an 8.x-compatible
release later, revisit.

**Why libclang + explicit target triple:** `ffmpeg-sys-next`'s build.rs
generates bindings via `bindgen`, which needs a real `libclang.dll` (not
present anywhere on this machine) and, because we're on the GNU/MinGW
target (see below), needs to be told explicitly to parse headers as
`x86_64-w64-mingw32` — without the `--target` flag, clang misreads
MinGW's GCC-attribute-heavy system headers (`stdlib.h` et al.) and fails
with syntax errors that have nothing to do with FFmpeg itself.

**libclang source:** installed via `pip install --target <dir> libclang`
(the PyPI wheel), not the official ~450MB LLVM Windows installer — the
wheel ships only `libclang.dll` (~26MB), which is all `bindgen` needs. No
bundled clang resource headers come with it, hence still needing to point
at MinGW's own headers via `BINDGEN_EXTRA_CLANG_ARGS`.

**Consequence to track:** this whole chain (GNU toolchain -> needs
MinGW-built FFmpeg -> needs bindgen -> needs libclang -> needs the right
clang target triple to read MinGW headers) is fragile and version-coupled
in a way the MSVC route likely wouldn't be (MSVC + a matching FFmpeg MSVC
build + bindgen tends to be the much more common, better-tested path in
the Rust ecosystem). If this keeps causing friction into M1's real
probing/decode work, revisit the earlier GNU-vs-MSVC decision — it may be
worth asking the user to run the elevated VS C++ install after all.

## 2026-08-07 — In-process FFmpeg FFI for M1, not sandboxed decode

**Decision:** M1's decoder pool wraps FFmpeg via FFI directly in the app
process, behind the `media::DecoderPool` trait already defined in M0.

**Alternatives considered:** sandboxing each decoder in a child process
(spec's own crash-isolation concern, docs/risks.md #4).

**Why:** sandboxing adds a genuinely hard unproven problem (IPC handoff of
decoded frame data, ideally without a copy, across a process boundary)
before decoding itself works at all — sequencing it first would block M1 on
solving M1's *and* a chunk of M8-level hardening at once. Taking the crash
risk now, mitigated by the spec's required fuzzing pass (section 8) before
v1.0 ships, is the right order: get decode working, then decide if real
fuzzing results justify the sandboxing cost. `DecoderPool` being a trait
(not a concrete struct used directly) is what keeps this reversible.

## 2026-08-07 — GNU toolchain (MinGW-w64), not MSVC

**Decision:** the Rust toolchain on this machine targets
`x86_64-pc-windows-gnu`, using a portable winlibs MinGW-w64 build (GCC
16.1.0) extracted to `%USERPROFILE%\tools\mingw64`, not the MSVC target.

**Alternatives considered:** MSVC target with the VS2022 "Desktop
development with C++" workload (the earlier-stated default/recommended
route).

**Why:** the VS installer refuses a silent/quiet `modify` unless launched
from an already-elevated process, and this agent has no way to grant itself
admin rights or answer a UAC prompt. The GNU route needed no elevation at
all — MinGW-w64 ships its own Windows import libraries and doesn't depend on
the Windows SDK the way the MSVC linker does, so extracting a zip into a
user-owned directory was sufficient. Verified with a real `cargo run`, not
just `rustc --version`.

**Consequence to track:** the GNU target has historically seen more crate
build friction on Windows than MSVC (a few C-dependent crates assume MSVC's
`link.exe`/`lib.exe` toolchain). If `ffmpeg-sys`-style crates or `wgpu`'s
Windows backends hit friction under GNU during M1/M2, revisit — at that
point either ask the user to run the elevated MSVC install themselves, or
keep both toolchains installed and switch per-crate via `rustup override`.

Per the working agreement: every significant architectural decision, the
alternatives considered, and why. Newest first.

## 2026-08-07 — Windows is the first validated platform, not macOS

**Decision:** M0 spikes and all early builds target Windows (D3D12/Vulkan via
wgpu, Media Foundation for hardware decode, CPAL's WASAPI backend) rather
than macOS.

**Alternatives considered:** macOS-first per the original spec's ship order.

**Why:** the actual development machine is Windows. I cannot build, run, or
measure anything on macOS from this session — validating there would mean
guessing at spike results or blocking on a separate machine's feedback loop
for every risk item. macOS remains ship-order priority #1 for v1.0 itself;
only the *validation* order changes. VideoToolbox/Metal-specific work is
deferred until there's a Mac in the loop.

## 2026-08-07 — Undo history is persisted from v1.0, bounded

**Decision:** `ProjectDocument.undo_history` (schema v1) stores a bounded
list of `(label, before, after)` snapshots, capped by
`command::UndoStack::max_history` (default 100).

**Alternatives considered:** deferring undo persistence to v1.1, as the
original spec left optional.

**Why:** the spec's own persistent-timeline design (every edit produces a new
`Arc<Project>`) makes this nearly free — the versions already exist, storing
N of them is not new engineering. What isn't free is deciding *later*: the
schema v1 has to know the shape of a persisted command now, or a v1.1 change
becomes a migration instead of a day-one field. Bounded rather than
unbounded to keep autosave file size predictable on long sessions.

## 2026-08-07 — De-noise cut from the v1.0 audio effect list

**Decision:** ship gain, parametric EQ, compressor, limiter, high/low-pass,
and reverb in v1.0. No de-noise.

**Alternatives considered:** including a basic noise-gate-style de-noise, or
a full spectral/ML de-noise per the original spec's ambiguous mention.

**Why:** real de-noise (spectral subtraction or ML-based) is a standalone R&D
project, not an incremental mixer insert. Scoping it into v1.0 without
knowing which approach was intended risks either shipping something
embarrassingly weak or absorbing months of unplanned work. Flagged for v1.1.

## 2026-08-07 — CBOR over FlatBuffers for the project file format

**Decision:** `project` crate serializes `ProjectDocument` as CBOR via
`ciborium`, with schema v1 defined in `crates/project/src/schema.rs`.

**Alternatives considered:** FlatBuffers (the original spec's suggestion).

**Why:** FlatBuffers' main advantage — zero-copy reads — matters for a
tight decode/render hot path, not a document saved every 30s and loaded once
per session. CBOR is self-describing, has first-class serde support (so the
same `#[derive(Serialize, Deserialize)]` on the timeline types works
unmodified), and versions/migrates with ordinary Rust code rather than a
separate schema compiler step.

## 2026-08-07 — AAF and MXF/DNx deferred past v1.0; EDL added instead

**Decision:** v1.0 interchange is OTIO + EDL export. No AAF. v1.0 export
targets are H.264/HEVC in MP4, ProRes in MOV, and WAV/AAC audio-only. No
MXF/DNxHD.

**Alternatives considered:** including AAF (explicitly `[DECIDE]` in the
original spec) and DNxHD/MXF export.

**Why:** AAF is effectively a second project model to implement correctly,
not an export format — high effort for a v1.0 feature that isn't on the
Definition of Done list. MXF (OP1a) muxing correctness is its own scope
distinct from the codecs already committed to. EDL is plain text and cheap,
and covers much of the same "hand this to another tool" need OTIO doesn't
(OTIO isn't universally supported yet by older tools EDL still is).

## 2026-08-07 — Multicam feature deferred, but the seam is kept open

**Decision:** `timeline::ClipSource` has a `NestedSequence` variant from M0,
which is also multicam's foundation (a multicam clip is a nested sequence
whose active angle switches). No multicam-specific sync/switching logic is
built in v1.0.

**Why:** deferring the *feature* is fine; deferring the *data-model seam* is
the anti-pattern the spec warns about (proxies retrofitted late = a rewrite).
Keeping `ClipSource` open now costs nothing and avoids relearning this at
v1.1.

## 2026-08-07 — Hybrid UI: egui shell + bespoke wgpu timeline widget

**Decision:** panels (bins, effect controls, mixer) will use egui;
the Timeline panel will be a custom wgpu-rendered widget embedded within it.

**Alternatives considered:** pure egui throughout.

**Why:** the 60fps-with-500+-clips-and-waveforms budget row is a real risk
for pure immediate-mode redraw of a dense custom widget. A bespoke
renderer for just the Timeline keeps the risk contained to the one place
that needs it, while everything else gets egui's much faster time-to-build.
Not yet spiked — revisit if the M0 wgpu spike surfaces a reason not to.

## 2026-08-09 — Undo-history persistence implemented; its on-disk cost measured

**Context:** the 2026-08-07 decision above ("Undo history is persisted from
v1.0, bounded") declared `ProjectDocument.undo_history` in schema v1 but was
never wired up. `undo_history` was written as `[]` on every save and ignored on
every load, so reopening a project silently discarded the ability to undo the
work in it. Now implemented as declared: `(label, before, after)` snapshots,
capped by `UndoStack::max_history`.

**Measured cost, because the original rationale was a size argument:** on a
3-clip project, the bare document is 2,718 bytes and each history entry adds
~2,352 bytes — so at the default cap of 100 entries a project file is roughly
**200x the size of the project it contains**. For this toy project that's
~235KB (fine). Extrapolated to a realistic 200-clip project with effects and
keyframes, it's several megabytes per save.

**The part of the original rationale that turns out to be wrong:** "the
versions already exist, storing N of them is not new engineering." That is true
*in memory* — history entries hold `Arc<Project>`, so the states are shared and
cost almost nothing. It is false *on disk*: CBOR has no way to express sharing,
so every `Arc` clone becomes a full independent copy. The design is cheap in RAM
and expensive in bytes, and those were conflated.

**Known redundancy, deliberately left in place:** `entry[i].after` is always
`entry[i+1].before`, and `history.last().after` is always the current project —
guaranteed by every mutator in `UndoStack`. So the `(before, after)` shape
stores every intermediate state exactly twice, and dropping `after` would halve
the cost with no information loss. Not done here because it changes a format
this log explicitly specified, which is a decision to take deliberately rather
than as a side effect of implementing it. `history_costs_roughly_two_project_
snapshots_per_entry` measures it so the number can't drift unnoticed.

**Two scope choices made while implementing:**
- **Redo is not persisted.** Redo entries describe states *ahead* of the saved
  project — work the user undid before saving. Restoring them would let a
  reopened file redo its way into a state the file never contained. Premiere and
  Resolve both drop redo on reopen.
- **Autosave writes no history at all.** Serialising N snapshots every 20
  seconds would make the cost of a background safety net scale with session
  length. A crash therefore loses undo history but not work.

**Loaded history is validated, not trusted:** if `history.last().after` doesn't
match the saved project, the history is discarded and the user is told. A
mismatched pair would otherwise make the first Ctrl+Z jump to an unrelated
state, which is worse than having no history.

## 2026-08-12 — GPU-resident decode shipped, then honestly reported as not helping

`playback::SequenceVideoPlayback::start` now takes `Option<GpuContext>`
(`Arc<wgpu::Device>`/`Arc<wgpu::Queue>`) and, when given one, uploads each
decoded frame straight to a GPU texture on the decode-ahead thread
(`SourcePixels::Gpu`) instead of handing back a CPU `Vec<u8>` for the UI
thread to upload later. `app::main` always passes a context, so the running
editor takes this path today. `render::upload_rgba_to_gpu` is the extracted
free function both this and `Compositor::upload_rgba` now share.

This was built to fix a specific measured problem: a real-footage smoke test
against 2560x1588 screen-capture recorded **334 starved frames** over a 4s
window on 2026-08-10, versus 0 on the synthetic 640x360 fixture — real
evidence, at the time, that resolution was breaking the CPU-upload path.

**It didn't work, and the original number didn't reproduce.** Re-running the
same test (`crates/playback/tests/real_footage_smoke.rs`,
`video_holds_frame_rate_on_real_footage` and its new `_with_gpu_upload`
sibling) three times back to back against the identical file gave CPU
starved counts of 13/11/8 and GPU counts of 29/12/10 out of ~120 boundaries
per window. GPU is not clearly better — if anything slightly worse, most
likely because `upload_rgba_to_gpu` calls `device.create_texture` fresh every
frame with no pooling, a real per-frame allocation cost the CPU path's plain
`Vec` clone doesn't pay. And the CPU path alone, measured clean, was never
close to the 334 figure — that earlier run was almost certainly taken while
a `cargo build --workspace --release` was running concurrently in the same
session (the desktop-packaging work happened right around then), making it
a measurement of system contention, not of the decode pipeline.

**Decision: leave the GPU path in place, but stop describing it as a
performance fix.** It isn't wrong or harmful — same test coverage, zero
regressions, 309 tests green — and it's a more direct pipeline than routing
every frame through the UI thread, which has its own value. But the honest
status is "architecturally cleaner, not measurably faster," not "closes the
resolution-scaling gap." If real degradation at high resolution is found
again, texture pooling (reuse one texture per asset instead of allocating
per frame) is the untried next step — this entry deliberately doesn't do it
speculatively, since nothing currently measured justifies it. The broader
lesson, worth repeating: a single-capture number is a hypothesis, not a
baseline, until it's been reproduced clean.

## 2026-08-12 — Dip-to-black dips colour, not alpha

`layers_of` implemented dip-to-black by fading the track's **alpha** to zero at
the midpoint. On the bottom video track that reads correctly — there is nothing
behind but the black backdrop — which is exactly why it survived: the only test
covering it was a single-track one asserting the frame went *transparent* and
calling that "what black is here".

It is wrong on every other track: the picture below shows through, so a dip to
black dips to whatever happens to be underneath.

Fixed by adding `ClipUniforms::rgb_scale`, which multiplies working-space
colour and leaves alpha alone (0.0 = opaque black). Dip-to-black now keeps full
alpha and ramps colour, so the track goes black *and* keeps covering what's
below. The scale is applied in `fs_clip` after linearisation and after the
effect chain, so the dim happens in linear light — dimming encoded values would
give a washed-out grey rather than a true fade to black. `rgb_scale` occupies
what was a padding word, so the uniform's size and every vec4 alignment are
unchanged.

Two adjacent behaviours settled at the same time. A dip with material on only
one side has no midpoint to meet at, so it ramps across the **whole** region
rather than sitting black for half of it and ramping over the rest — the
half-ramp shape has a visible pop at the midpoint. And the black fills the
clip's own placement rectangle, not the frame: identical for a full-frame clip,
and for a scaled or offset one it dips just that clip, which is what Premiere
does. A full-frame dip would need a solid-colour source the compositor doesn't
have.

## 2026-08-12 — Titles are a clip source, not an effect; rasterised on the CPU

Spec 4.5 wants titles. Two shapes were possible: a text *effect* applied to a
clip, or a *clip source* of its own. `ClipSource::Title(TitleSpec)` won,
because a title in an NLE is a thing on a track with its own in and out points,
not a modifier of something else — and because an effect would have needed
something underneath it to modify, which a title card has by definition not
got.

Cost of that choice, paid deliberately: `ClipSource` is no longer `Copy`/`Eq`
(a `TitleSpec` owns a `String` and `f64`s). The alternative — a title-asset
table so the enum stays two words wide — is a whole indirection existing only
to preserve a derive.

**Rasterised on the CPU into a frame-sized straight-alpha RGBA buffer**, then
uploaded and treated as an ordinary `Rec.709` source. That means titles get
transforms, opacity, masks, effects, transitions and export for free rather
than each needing a title-shaped special case; the compositor's title branch is
~15 lines and everything downstream is unchanged. Straight alpha specifically
(colour written everywhere, coverage in A) because `fs_clip` linearises RGB and
multiplies alpha separately — premultiplying in an encoded space and
linearising after would darken every antialiased edge into a grey fringe.

**`ab_glyph` rasterises; nothing shapes.** Characters map to glyphs with pair
kerning, which is right for Latin and wrong for Arabic, Devanagari, and
ligature-substituting fonts. A real shaper (HarfBuzz/rustybuzz) is the fix and
is not here. This is recorded rather than hidden because the failure is
visible and specific, which is better than a subtly-wrong result everywhere.

**Font families resolve by filename**, not by parsing each installed font's
name table, with an alias table for the common mismatches ("Segoe UI" ->
`segoeui`) and a fallback chain ending in whatever exists. An unknown family
therefore renders in the wrong typeface rather than not at all — a project made
on another machine must still show its text, since the text is the content and
the font is a preference.

**Caching was added second, after measuring, and the measurement was not what
was expected.** `title_rasterisation_cost.rs` found that an *empty* title costs
almost as much as a real one: the work is dominated by filling the 8.3MB
frame-sized buffer, not by drawing glyphs. So the useful cache is of the whole
raster (`compositor::TitleCache`, keyed on spec + frame size, LRU, 8 entries),
not of glyphs. Optimising glyph rasterisation — the intuitive target — would
have chased the smaller half of the cost. Hit/miss counters are exposed so the
reuse is asserted in tests rather than assumed, which is the correction this
project already had to make once (see the GPU-resident decode entry above).

**Typing coalesces via a new `UndoStack::push_or_amend`, not via
`begin/end_coalescing`.** A drag brackets itself with mouse-down and mouse-up;
typing has no reliable "done" event, and an open coalescing group that nothing
closes makes Ctrl+Z silently do nothing until focus happens to change.
`push_or_amend` amends the last entry when the label matches, so the history is
consistent after *every* keystroke — undo works mid-word — while a run still
collapses to one step. The label carries the clip id so two different titles
don't fold into one entry that undoes both.

**"Add Title" never overwrites.** `EditOp::Overwrite` does what it says, so
dropping a title onto an occupied track would eat a second of footage —
invisible until the user scrubs back to it. `add_title_at_playhead` uses the
top video track only when it is free at that range, and otherwise creates a new
track above, both halves as a single undo step.

## 2026-08-12 — Titles get real shaping and real font-name resolution, both measured

Two of the three documented title limitations closed in the same pass, both
using pure-Rust crates already resolvable offline (`rustybuzz` 0.20.1 —
HarfBuzz's algorithm reimplemented in Rust, no C toolchain link — and
`ttf-parser` 0.25.1, already in the dependency tree transitively via egui).

**Shaping.** `render::text::shape_line` now runs each line through
`rustybuzz::shape` before handing glyph ids and positions to
`ab_glyph::Font::outline_glyph`. `rustybuzz::Face<'a>` borrows its data rather
than owning it, so `FontFace` now carries the font bytes twice — once inside
the `ab_glyph::FontVec` used for rasterisation, once as `Arc<Vec<u8>>` a fresh
`rustybuzz::Face` is built from per shape call. The walk-forward-add-advances
loop needs no direction branch for right-to-left text: HarfBuzz-family shapers
reverse the glyph array internally for RTL specifically so a simple forward
walk already draws it correctly — that reversal is part of what shaping does,
not a caller's responsibility. Falls back to the old naive per-character path
if `rustybuzz::Face::from_slice` can't parse a font `ab_glyph` accepted (belt
and braces; not expected to fire on a real font).

Proven with a real font, not a synthetic case: Windows ships `Nirmala.ttc`
(the default Devanagari UI font since Windows 8), and shaping KA+VIRAMA+SSA+
VOWEL-SIGN-I — four codepoints, ordinary Devanagari, not a stress test —
correctly merges into two glyphs (the KSSA conjunct plus the vowel sign). A
naive one-glyph-per-character mapping can only ever produce four; it has no
mechanism to merge or reorder.

**First version of this test was worthless and mutation testing caught it.**
`shaped_glyph_count` originally called `rustybuzz::shape` independently of
`shape_line` — a second, parallel implementation of the same idea. Forcing
`shape_line` to always fall back to the naive path left the test's own
private shaping call untouched, so it kept passing. Fixed by making
`shaped_glyph_count` call `shape_line` and read its length — now it is
provably testing what `rasterize` actually draws, and the same mutation
correctly fails it. Recorded because it's a specific, repeatable trap: a test
written to *check* a code path is not the same as a test that *exercises*
that code path, and the two look identical until you mutate the thing you
meant to be testing.

**Font resolution.** `FontLibrary` now resolves a requested family against the
font's own `name` table (`family_name_from_table`, preferring the
typographic-family id 16 over the legacy id 1) instead of only matching
filenames. The obvious implementation — parse every installed font's name
table at `FontLibrary::system()` construction — was measured before being
built: ~3 seconds for ~350 fonts on this machine, almost entirely
`std::fs::read` I/O rather than parse time. That is a real, visible stall on
every `Compositor::new` (preview, export, and playback each build their own),
so it was rejected on evidence, not assumed away. Built instead as an
incremental, memoized scan: a lookup that misses the filename-stem/alias fast
path scans unindexed font files one at a time, caching *every* family it
discovers along the way — not just a match for the current query — so a later,
different unusual lookup is cheaper than the first, and the full directory is
never scanned twice. `available_families()` (a font-picker UI's use case, not
a per-frame one) forces the scan to completion and is the one place the full
cost is ever paid, on demand rather than at startup.

Verified against a real, deliberately unhelpful case: `SNAP____.TTF`, whose
declared family ("Snap ITC") shares no substring with its file stem — no
plausible alias-table entry could cover it by guesswork, so resolving it
correctly is proof the name table is what's actually being read.

## 2026-08-12 — Wipe and slide: two transitions, two very different amounts of machinery

Both were asked for by the same checklist line, and they cost wildly different
things, which is worth recording because the cheap one is the surprising one.

**Slide needed no rendering code at all.** `Transform2D::position` already
places a clip anywhere in the frame, so a slide is one number: offset the
incoming clip by `(1 - progress) * frame_width` and let the existing draw path
do the rest. `TrackLayer::position_offset` is the entire feature.

**Wipe needed a shader change, and deliberately not the cheap one.** The
obvious implementation reuses the existing `MaskUniforms` — a rectangle mask
from `uv.x = 0` to `progress` is exactly a left-to-right wipe, and it would
have cost zero new uniform fields. Rejected: a clip's own mask effect and the
transition's wipe boundary are independent things that must compose, and
sharing the uniform means every wipe silently discards the user's mask for its
duration. `ClipUniforms` grew `wipe_progress` + `wipe_enabled` instead (both
fitting in words that were already padding, so the struct's size and vec4
alignments are unchanged), and `a_wipe_does_not_discard_the_clips_own_mask`
pins the composition.

`wipe_enabled` is a tri-state (0 off / 1 reveal / 2 hide) rather than a bool,
because a wipe at the *tail* of a track has no incoming clip to reveal and has
to take the outgoing one away instead — the same shape as a cross dissolve
degenerating into a fade-out when there's nothing after it. The edge is
softened over ~0.003 UV rather than being a bare `step`: a hard vertical
boundary moving sub-pixel amounts per frame crawls visibly.

Both are **one direction only** (wipe left-to-right, slide in from the right).
Other directions each need a direction field on `Transition`, which is a schema
change rather than a rendering one, and guessing at eight variants nobody asked
for is how a feature list gets long and shallow.

**The UI menu is now generated from `TransitionKind::ALL`** rather than
hand-written per kind. Adding `Wipe` and `Slide` to the model would otherwise
have left them implemented in the renderer and unreachable in the editor — the
compiler catches a missing `match` arm in `label()`, and the menu picks up
whatever `ALL` contains, so the two can't drift.

## 2026-08-12 — True-peak and EBU R128 loudness implemented for real, against the standard's own numbers

These were the longest-standing "types only" entry in the architecture doc, kept
that way on the explicit ground that *a plausible-looking approximation of a
compliance number is worse than an absent one*. That reasoning is what shaped
how they were finally built: `audio::loudness` implements the actual specified
algorithm — BS.1770-4 K-weighting (both biquad stages, with the standard's
constants **derived from the sample rate** rather than the 48 kHz coefficient
table transcribed, so a 44.1 or 96 kHz programme measures right instead of
silently mis-weighted), 400 ms blocks at 75% overlap, the -70 LUFS absolute and
-10 LU relative gates applied in the power domain, EBU Tech 3342 loudness range
with its own wider 3 s blocks and -20 LU gate, and a 4x-oversampled polyphase
true peak.

**Every test is anchored to a value the standard fixes or to algebra**, never
to what this code happened to output: a 1 kHz sine at -23 dBFS reads -23.0
LUFS (the calibration point the -0.691 dB offset exists to make true); halving
amplitude is exactly 6.02 LU; the same signal in stereo is 3.01 LU above mono
(the property that breaks if channels are averaged rather than summed — a
natural-seeming slip that would make every stereo deliverable measure 3 dB
quiet); a sine at fs/4 offset an eighth of a cycle sits every sample at
-3.01 dBFS while truly peaking at 0 dBFS.

**Three of the eleven tests failed on first run, and all three were bugs in the
tests, not the implementation** — worth writing down because two of them were
tests that looked obviously correct:
- The inter-sample-peak signal was built from `PI * i as f32 / 2.0`, which
  loses enough precision by i≈100k that the samples drift off ±0.7071 and
  broke the test's own precondition. Rewritten from `i % 4`.
- The "true peak must not exceed sample peak on a signal with no inter-sample
  content" test used held DC. A constant starting abruptly at sample zero is a
  *step*, which genuinely does contain inter-sample content — a correct
  true-peak meter overshoots on one, so the test was asserting the
  reconstruction filter should be wrong. Replaced with a 100 Hz sine (480
  samples/cycle).
- The loudness-range test alternated levels every 2 s while LRA analyses 3 s
  blocks, so every block straddled a change and averaged the range away. It
  measured 3 LU on a signal spanning 20 dB — a fact about the block length, not
  the audio. Segments lengthened past the block.

All five algorithm stages were then mutation-tested (bypass the K-weighting
shelf, disable the relative gate, average channels instead of summing, skip
oversampling, zero the offset) and each was caught by exactly the test written
for it.

**Wired into export, not left as a library.** `ExportStats::loudness` measures
the samples actually handed to the encoder — clip gain, pan, track faders and
the master bus all sit between the source clips and the file, so measuring
anything earlier would describe a different signal. It's `Option`, and `None`
for a video-only export rather than a silence reading: `Some(-120 LUFS)` would
make a delivery check flag a silent-audio failure on a file that correctly has
no audio. The editor prints the figures on the export result line and warns
above -1 dBTP, since the number means nothing to someone who hasn't memorised
the delivery spec.

## 2026-08-12 — Clip-speed audio retiming: constant speeds only, varispeed, reverse refused

`audio::timeline_mix` now resamples a clip whose `SpeedCurve` is a constant
non-1x ratio, closing the longest-standing A/V mismatch in the project (video
honoured speed, audio didn't). The read position comes from
`SpeedCurve::source_delta` — the same function the video graph uses — so
picture and sound advance through a retimed clip identically by construction
rather than by two implementations agreeing.

**Deliberately still unsupported, and reported rather than approximated:**
a *keyframed* speed curve (integrating a rate curve is time remapping, which
`source_delta` explicitly refuses to fake), and a *reverse* speed. Reverse is
the interesting one: the audio side could read a backward span without much
trouble, but the video pipeline only walks forward, so reversing just the audio
would produce a file where sound runs backwards over forwards picture. Refusing
consistently with video is better than each half being individually defensible.

**Varispeed, not time-stretch.** Pitch rises and falls with speed, like a tape
machine and like Premiere with "Maintain Audio Pitch" off. Linear interpolation
between source frames, plus a box average across the span each output frame
covers when speeding up — decimation aliases, and folding out-of-band content
back as tones that were never in the recording is the artefact that actually
matters here. A windowed-sinc interpolator would be measurably sharper and
inaudibly different; a phase vocoder would sound different but solves a
different problem.

**One mutation escaped the first test pass and is worth recording.** Replacing
`clip.speed.source_delta(into_clip_ticks)` with plain `into_clip_ticks` — i.e.
ignoring speed when computing *where* to start reading — broke nothing. Every
retiming test mixed from tick 0, where "how far into the clip" is zero and
scaling it changes nothing. Playback and export request blocks from the middle
of clips constantly, so the bug would have shown up the instant anyone scrubbed
into a retimed clip rather than playing it from the top.
`a_block_starting_partway_into_a_retimed_clip_reads_the_scaled_source_position`
now covers it. The general lesson: a test suite that only ever starts at the
origin cannot see an error that scales with offset.

## 2026-08-12 — Fader automation, and a migration that couldn't live in the migration chain

`Track::gain_db` became a `ParamTrack` (schema v6). The mixer evaluates it
**per output frame** when it's animated — a block-rate fader steps at every
block boundary, which is audible as zipper noise — and **in sequence time**,
not block time, so a block mixed from the middle of the timeline reads the
middle of the curve instead of restarting it. When the track isn't animated the
value is hoisted out of the loop, so an ordinary fader costs exactly what the
plain `f64` did.

**The compat had to go in the deserializer, not `persist::migrate`.** This
project's rule is that every format change gets a real migration step, and this
one appears to break it. It doesn't: migration runs on an already-decoded
`ProjectDocument`, and a v5 file's bare CBOR float fails to decode into a
`ParamTrack` before `migrate` is ever called. So a `deserialize_with` that
accepts either shape is not a shortcut around the rule — it is the only place
the rule's actual purpose (old files still open) can be honoured for a field
whose *type* changed rather than whose presence did. The v6 step still exists
so the stamp moves and a v5 build refuses a v6 file rather than opening it with
its automation silently flattened.
`a_v5_project_whose_fader_was_a_bare_number_still_opens_at_the_same_level`
builds the old shape as raw CBOR rather than round-tripping today's types —
a test that writes with the current serialiser can never prove yesterday's
files load — and mutation-testing confirms removing the compat deserializer
fails it.

**Dragging an automated fader writes a keyframe; dragging a static one replaces
the value.** Both behaviours are what the user means in context: on a static
track a fader move is just a fader move and shouldn't silently start
automating, and on an automated track a fader move that wiped out the whole
curve would be destructive. The mixer strip's fader also *reads* at the
playhead, so an automated fader visibly follows its own curve as the playhead
moves rather than sitting wherever it was last dragged.

## 2026-08-13 — CapCut/Premiere-inspired batch: chroma key, scene-cut, silence, beats, loudness-match, stabilizer

User asked "what more can we add — take inspiration from CapCut and Adobe
Premiere Pro, we need to build something that can compete with that." Landed
a DSP/CV batch chosen deliberately for what's tractable *without* a new ML
dependency (a separate transcript/captions feature, needing a local Whisper
model, is the agreed-on next push, not part of this one):

- **Chroma key** (`chroma_key` effect) — Rec.709 chroma-distance keying
  (mirrors `render::scopes`'s vectorscope matrix), real min/max spill
  despill. First `ParamType::Color` effect param in the project, which
  exposed that `effects_panel.rs`'s generic param renderer had never actually
  handled `Color` (fell through to a "type mismatch" label) — fixed as part
  of landing this, not a pre-existing UI bug nobody noticed because nothing
  had used the type yet.
- **Scene-cut detection** (`render::scene_cut`) — 1D Earth Mover's Distance
  between per-frame luma histograms (plain bin-overlap can't tell "close" from
  "far apart" for two non-overlapping histograms), adaptive mean+k·stddev
  threshold so one clip's own jitter doesn't drown out its own real cuts.
- **Silence-based auto-cut** (`audio::silence`) — windowed RMS vs. a dBFS
  threshold, debounced by a minimum duration, padded inward before removal so
  cuts don't clip adjacent speech. Ripple-delete composes from existing
  `Razor`+`Razor`+`Extract` — no new `EditOp` needed.
- **Beat-sync** (`audio::beat`) — real spectral flux (STFT via `rustfft`,
  half-wave-rectified magnitude increase, periodic not symmetric Hann window
  — the symmetric form gives a bin-aligned tone spurious flux, caught by a
  test using a synthetic tone chosen to land exactly on an FFT bin).
  Needed an absolute flux floor on top of the relative adaptive threshold —
  a signal with *no* real onsets has flux at FFT floating-point-noise scale
  throughout, and a relative threshold computed from that same noise has no
  power to reject it. Beats become sequence markers, not edits — the
  timeline's `snap_tick` already treats every marker as a snap candidate, so
  this makes beats magnetic for free.
- **Per-clip loudness auto-match** (`audio::loudness::gain_to_reach_target`)
  — direct extension of the EBU R128 work already in the project; ±24dB
  clamp specifically to stop a near-silent measurement from computing a
  gain in the hundreds of dB.
- **Warp Stabilizer** (`render::stabilize`) — scoped honestly: block-matching
  translation estimation (bounded-window SAD search on a downsampled
  greyscale frame, not phase correlation or optical flow) plus moving-average
  path smoothing, written as `Transform::POSITION` keyframes — no new effect
  needed, since counter-animating position is exactly what Transform already
  does. Translation only: no rotation/scale/perspective correction, no
  border crop/fill, stated in the module doc rather than silently absent.

**Real bug found by the silence real-footage test, not by a synthetic one**:
`detect_scene_cuts` and `detect_silence_and_ripple_delete` both called
`apply_ops` and reported success without checking its boolean return —
`apply_ops` is all-or-nothing, so a failed batch left the project completely
untouched while the code still claimed "found and split at 8 cuts." Root
cause was a *test-fixture* bug shared by both real-footage tests: the
fixture hand-picks `ClipInstanceId(1)` without advancing `state.next_id`,
so `next_id()` inside the method under test collided with it, creating a
duplicate id, and a subsequent `Extract` looked up that id and got the wrong
clip. Fixed the fixture (`state.next_id = 1000` after construction) in both
tests, fixed the return-value bug in both methods, and added an
id-uniqueness assertion to both tests — the original scene-cut test had the
identical id-collision defect and never caught it, because nothing checked
for duplicate ids until the silence test's harder failure forced a look.

**Mutation testing earned its keep twice more**: the local-maximum
requirement in beat detection's peak-picking looked redundant with the
min-interval debounce against every test I'd written — removing it still
passed all 8 tests, because every synthetic click was an isolated spike.
Only after adding a sustained-swell test (a broad hump spanning longer than
the debounce window, which debounce alone can't collapse) did the mutation
get caught. And a warp-stabilizer sign-convention bug (`estimate_translation`
returning the negated correct answer) was caught immediately by the
recovers-a-known-shift test, not by staring at the algebra.

## 2026-08-15 — Full codebase audit, and the first commit in a week

Requested review: "check for bugs in the entire codebase and make sure its
organized." Dispatched eight parallel reviews, one per crate group
(`state.rs`; the UI panel layer; `render`; `audio`+`audio_source`;
`timeline`; `media`+`media_ffmpeg`; `playback`+the spike/demo binaries;
`project`+`command`+`export`+`speech`), cross-checked against a direct grep
pass (zero `unsafe` anywhere in 26k lines, four total `TODO`s, no leftover
debug prints in production code). 21 correctness bugs found, 14 organization
items. Full findings recorded as an artifact rather than duplicated here in
full; the four critical ones and their fixes are below.

**The organization finding that mattered most wasn't a code issue**: the
last git commit before this was 2026-08-07 — every feature built since
(the entire desktop editor app, undo/redo, export, and the whole
CapCut/Premiere batch from the entry above) had been sitting uncommitted for
a week with no version history. Committed it as one checkpoint rather than
trying to reconstruct artificial incremental history after the fact —
`state.rs` alone depends on nearly every crate added that week at once, so
there's no clean midpoint that both compiles and passes tests. Excluded
`dist/` (159MB of build output, now gitignored) and a stray, non-symlinked
`%SystemDrive%/ProgramData/...` junk directory found at the repo root
(flagged, not investigated further — wasn't created this session).

**Four critical bugs fixed, each with a test that failed against the old
code before it failed against the new one:**

- **Locking a track didn't actually protect it** (`timeline::edit_ops::
  ripple_shift`). Every caller checked whether the *target* track of an edit
  was locked before proceeding, but a sync-locked *sibling* track swept into
  the same ripple was never checked — so locking a track stopped direct
  edits to it but not edits rippling through it via a sync-locked neighbor.
  Fixed by checking every track the ripple is about to touch, not just the
  one it was explicitly aimed at, and refusing the whole operation
  (`EditError::TrackLocked`) rather than silently skipping the locked
  sibling — consistent with this function's own stated philosophy of
  rejecting rather than producing a silently-corrupt result.
- **The transcript cache could leak across projects** (`EditorState::
  open_from`). Never cleared `self.transcripts`, and a freshly loaded
  project's clip ids restart from a low number — easy to collide with ids
  left over from whatever was open before. Now cleared alongside
  `asset_paths` on every open.
- **The transcript panel's word selection didn't reset on clip switch**
  (`transcript_panel.rs`). Select a range in one clip's transcript, click a
  different clip, and "Delete" would act on the new clip using the old
  clip's word indices. Extracted the reset logic into a small
  `egui`-independent function (`sync_selected_clip`) specifically so it
  could be unit tested without a GUI harness, and added a
  `selected_clip: Option<ClipInstanceId>` field to know when to fire it.
- **Chroma key compared linear-light pixels against a gamma-encoded key
  color** (`composite.wgsl`'s `fs_chroma_key`). The source pixel is
  converted to linear light by `fs_prepare` earlier in the pipeline, but the
  eyedropper-picked key color never went through the same conversion before
  being compared against it. Invisible in the existing tests because pure
  primaries (`[0,1,0,1]`) are fixed points of the transfer curve — 0 stays 0,
  1 stays 1 either way. The regression test instead picks a genuine
  mid-tone green, uses it as its own key color, and asserts the pixel keys
  out completely (distance must be exactly 0 by construction) — which failed
  outright (alpha 255 instead of 0) against the old code. Fixed by adding
  `transfer_code` to `ChromaKeyUniforms` (reusing the struct's existing
  padding slot, so its size is unchanged) and running the key color through
  `to_linear` before every comparison, including inside `despill`.

Also fixed **`generate_captions` clobbering an existing transcript**
(`state.rs`): it wrote into `self.transcripts` *before* checking the clip's
speed curve was `Constant`, so running it on a keyframed-speed clip that
already had a real transcript from an earlier `transcribe_clip` call
correctly refused to create captions but, as a side effect, silently
overwrote that real transcript with an empty one. Moved the speed check to
the top of the function, before any transcription work starts — this was
flagged as "high confidence" rather than "critical" by the review since it
needs a keyframed clip specifically, but it's the same shape of bug as the
transcript-cache one above, so it's grouped with the critical fixes here.
Verified with a real-footage `#[ignore]`d test (the bug only manifests once
transcription actually succeeds and reaches the speed check) — ran in under
9 seconds via `cargo test` directly, confirming the earlier live-app
20-minute "freeze" while testing the transcript panel was unrelated decode
slowness from the computer-use environment's own rendering overhead, not
this bug or the analysis pipeline itself.

Remaining findings (17 more correctness bugs across severity levels, 14
organization items including splitting `state.rs`'s 5,391 lines by feature
area) are tracked for follow-up, not fixed in this pass.

## 2026-08-16 — AI background removal, and a second `ort-sys` toolchain gap

Requested feature: CapCut-style one-click AI background removal. Landed as
a new `matting` crate (Robust Video Matting via ONNX Runtime) plus a
`matting_jobs.rs` background-job manager mirroring `proxy_jobs.rs`, wired
into the effects panel as a "Remove Background" button.

**The `ort-sys` gap, and why it's a genuinely different failure from
whisper.cpp's:** `ort-sys` (the ONNX Runtime crate's build-time-linking
path) has no prebuilt binary for `x86_64-pc-windows-gnu` — same class of
problem as the whisper-rs FFI saga, but the `ort` crate itself ships an
escape hatch whisper-rs didn't: a `load-dynamic` feature that calls
`LoadLibrary` on `onnxruntime.dll` at *runtime* instead of linking against
`onnxruntime.lib` at *build* time. Switched to that, downloaded Microsoft's
official prebuilt `onnxruntime-win-x64-1.29.0` release (both this and the
14MB RVM model file were downloaded only after explicit approval, source
URL and size shown first). Verified with a real spike test — load the real
model, run two real recurrent frames through it — before building anything
on top.

**Architecture: a real derived video file, not new render-graph plumbing.**
The alternative was a per-frame alpha side-channel threaded through
`compositor.rs`/`graph.rs`. Instead, `matte_video.rs` decodes the source,
runs each frame through RVM, bakes the resulting alpha into a real QTRLE
`.mov` (lossless, alpha-capable — `ARGB` byte order specifically, since
qtrle's encoder rejects `RGBA`, confirmed from its own reported
supported-format list rather than guessed), and `MattingJobs` substitutes
that file for the original via the exact same asset-path-resolution
mechanism `ProxyJobs` already uses. The existing alpha-aware compositor
needed zero changes.

**Resolution order matters and is deliberate:** proxies are a
performance-only substitution, so `start_export` never resolves them
(export always uses originals). Matting is a deliberate edit, so
`start_export` *does* resolve it. At playback/preview call sites, both
apply with matting resolved last (`matting.resolve(&proxies.resolve(...))`)
so a matted asset wins over its own proxy.

**Scope limitation, stated plainly:** matting is keyed by `MediaAssetId`,
not per-clip-instance. If the same source file appears in two clips,
removing the background on one removes it on both — identical to the
tradeoff `ProxyJobs` already makes, not a new one.

**Verified live, not just via unit tests:** built release, redeployed, and
ran the actual button in the actual app — imported footage, clicked
"Remove Background," watched the button go None -> "Removing
Background…" -> "Background removed ✓" while the UI stayed fully
responsive (played back normally during the background job), then
confirmed the preview genuinely composited a transparent background both
while scrubbing (different playhead position -> different real matte
frame, not one static bake) and during live playback. Test footage was a
synthetic SMPTE color-bar pattern (no real people in this repo's fixtures),
so RVM's actual person-segmentation quality is unverified — what's verified
is that the full pipeline (inference -> alpha bake -> file substitution ->
compositor) is real and wired correctly end to end: RVM treated the flat
color fields as background and correctly kept the moving high-frequency
elements (a diagonal gradient streak, scattered dots) as foreground.

**Same day, caught on a follow-up recheck — background removal silently
muted the clip's audio.** `generate_matte_video` only ever touched the video
stream; the output `.mov` had no audio track at all. `MattingJobs::resolve`
substitutes that file for the original asset path unconditionally (no
opt-in gate the way `ProxyJobs::enabled` has), and the *same* resolved-path
map feeds both video decode and `audio_source::DecodedSampleSource` at
every call site (`start_playback`, `start_export`, the scrub-preview line).
`AudioDecoderStream::open` on a stream with no audio returns
`NoDecodableStreams`, which `DecodedSampleSource` treats as "permanently
silent for this asset" — so removing a clip's background silently killed
its audio in both live playback and the exported file, directly
contradicting this feature's own stated intent that matting "belongs in
the delivered file." Confirmed two ways before touching anything: traced
the code path, then `ffprobe`'d the actual matte file generated during the
live-app test above — one `qtrle` video stream, nothing else, against a
source that `ffprobe` confirmed has `h264` + `aac`.

Fixed by muxing the source's original audio through as a plain stream copy
(no re-decode/re-encode — `generate_matte_video`'s job is the alpha
channel, audio is already correct) using ffmpeg's standard remux idiom:
`add_stream(codec::Id::None)` to get an unencoded stream, then
`set_parameters`/`set_time_base` copied straight from the input audio
stream, with packets routed by `packet.stream()` and `rescale_ts` +
`write_interleaved` alongside the existing video encode loop. Kept the fix
local to `media_ffmpeg` rather than touching `playback`/`export`'s
signatures — the substituted file is now a true drop-in replacement, which
matches the "just works like a proxy substitution" architecture the whole
feature is built on. Two new tests: one with a real AAC-bearing fixture
asserting the matte's `probe()` result has `audio.is_some()` (would have
failed outright against the pre-fix code — confirmed by the actual
zero-audio-stream file already sitting on disk from the live test), and
one with a video-only fixture confirming the no-audio case doesn't panic
or error. Full workspace suite (529 tests) passes with zero regressions;
release rebuilt and redeployed.

**Related, lower-confidence finding, not fixed here:** `ProxyJobs` has the
identical single-map-feeds-both-audio-and-video structure, and
`generate_proxy` is also explicitly video-only by design. It's gated
behind the "Use proxies" checkbox (off by default) so it's less likely to
have been exercised with real audio-bearing footage, and wasn't
independently reconfirmed live this pass — worth checking if proxies ever
get exercised with real A/V footage with the checkbox on.

## 2026-08-17 — Right-panel tabs, export dialog with resolution scaling, drag-and-drop import

**Right-panel tabs.** The four right-side panels (Effects, Transcript,
Scopes, Mixer) used to be a vertical stack — reaching Scopes meant
scrolling past Effects and Transcript first. They're now a tab strip that
shows one panel at a time. The title panel stays pinned above the tabs
rather than becoming a fifth tab, because it only draws anything when a
title clip is selected — folding it into the tab set would mean an extra
click to reach it in the one situation (editing a title) where you want it
immediately. One detail worth flagging on its own: the tab strip uses
`ui.horizontal_wrapped` rather than a plain `ui.horizontal`. The panel is
`.resizable(true)` down to egui's default 96px minimum, the four tab
labels run close to 268px, and `SidePanel` clips its contents and
hit-tests against the clipped rect — so on a plain horizontal row, a tab
pushed past the panel edge by a narrow width would become both invisible
and unclickable, with no scrollbar to recover it. Since the tab strip is
now the *only* route to three of the four panels, that's not a cosmetic
bug, it's a reachability one. Wrapping to a second row costs nothing in
the common case and avoids it entirely.

**Export dialog + resolution scaling.** The File menu's inline quality
radios and range checkbox are gone, replaced by a real "Export..." dialog.
It adds resolution scaling — Native / 75% / 50% / 25% — alongside the
existing quality and range controls. The implementation subtlety worth
recording: no new scaling pass was added. The encoder already ran an
RGBA→YUV420P scaler purely for pixel-format conversion (yuv420p's 4:2:0
chroma subsampling requires even dimensions regardless of scale), so
giving that same scaler a different destination size than its source size
makes it do the resize for free in the same pass. The compositor still
renders at the sequence's native resolution — scaling happens only in that
final conversion step — which is what keeps preview and export
pixel-identical and avoids any risk of an effect behaving differently when
rendered at a scaled target instead of native. `OutputScale::Native`
returns `(width, height)` unchanged before any of the percent arithmetic
runs, so the default (and previously only) path takes no new branch at
all.

**Drag-and-drop import.** Dropping a file anywhere on the window imports
it into the project bin, via `state.import_assets` — unchanged, just a new
caller. It deliberately does *not* also place the clip on the timeline
(that's a separate, deliberate action elsewhere in the app), and it's a
no-op on the Home screen, where there's no open project for an asset to
land in.

**Verification status, stated plainly.** The export dialog and resolution
scaling were verified live end-to-end, including `ffprobe` cross-checks
against the actual output files: 50% on a 640x360 sequence produced
320x180, Native produced 640x360. The full workspace suite is 531 passing
/ 0 failing. Live tab-switching and drag-and-drop, however, were **not**
verifiable in this session — the test environment was intermittently
dropping mouse-button events application-wide (hover states rendered
correctly, clicks didn't land, including on unrelated controls that had
worked minutes earlier), and automated drag-from-Explorer was additionally
blocked by tool policy on top of that. What *was* confirmed: the tab strip
renders correctly, verified via screenshot. The drag-and-drop code was
instead validated by reading winit 0.29.15's actual Windows
`IDropTarget` implementation and checking the handler's event contract
against what the code assumes, event-for-event. One point of indirect
evidence worth naming: `RegisterDragDrop` is a hard assert at window
creation in winit's Windows backend, so the app launching and running at
all is itself evidence the drop-target plumbing registered successfully.

**Known follow-up, not fixed.** A multi-file drop calls `import_assets`
once per file, because winit delivers one `DroppedFile` event per file in
the drop — so dropping 10 files at once produces 10 separate project
clones and 10 separate undo steps instead of one. Fixing it needs a
"collect drops until the next redraw, then import as a batch" buffer.
Noted here, not built.

## 2026-08-18 — Multi-file drops are one undo step

Follow-up to the drag-and-drop work: winit fires a separate `DroppedFile`
event per file, so the original implementation (which called
`import_assets(vec![path])` as each arrived) turned a 10-file drop into 10
separate undo entries. The Import button never had this problem — it hands
its entire selection to `import_assets` in one call, and that function
already loops internally and pushes exactly one undo entry.

Fixed by accumulating dropped paths in the event loop and flushing them
together in the per-frame poll block, next to the existing waveform and
proxy pollers. The screen check stays at drop time rather than flush time:
the drop happened against whatever was on screen when the user released, and
deferring that check would let a screen change between the release and the
next frame silently redirect the import.

The regression test added here pins the invariant the batching depends on —
one `import_assets` call is one undo step regardless of file count — rather
than the accumulation itself, which lives in `main()`'s event loop and has
no test harness in this codebase (the same reason the tab strip and drop
wiring were verified live rather than unit tested).

This is a deliberate deviation from the approved plan, which specified the
per-event call; the whole-branch review flagged it as the one deferred minor
with real user-visible cost.

## 2026-08-19 — drag-and-drop confirmed by hand, closing the last gap

The drag-and-drop import shipped with a real verification gap: File Explorer
is granted at a computer-use tier that disallows automated drags, so nothing
in this project could exercise the actual drop path end to end. It was backed
only by indirect evidence — the reviewer checking winit 0.29.15's Windows
`IDropTarget` implementation event-for-event, and a live confirmation that
`RegisterDragDrop` had succeeded on the running window.

The user dragged files onto the running editor by hand and confirmed it
works. That closes the gap: every feature in this batch (right-panel tabs,
the Export dialog with resolution scaling, drag-and-drop import, and the
multi-file drop batching) is now confirmed working in the real app rather
than correct-by-construction.

Worth keeping from the episode: the same session twice concluded "the
environment drops mouse-button events" when the real cause was that the
screenshot tool hides the Claude window while that window still intercepts
clicks. Instrumenting the winit/egui seam settled it in one run — a working
click logged `MouseInput ... consumed=true`, a failing one logged no
`MouseInput` at all. Measure the boundary; don't reason backward from the
symptom.

## 2026-08-19 — the "flaky test" is a real GPU-driver crash, not a timing flake

An earlier full-workspace run reported one failure that never reproduced. I
guessed out loud that it was "probably the audio-device-dependent playback
tests" and moved on. That guess was wrong, and worth recording as wrong.

Investigating properly: six full-workspace runs, complete logs kept. One run
died with `STATUS_ACCESS_VIOLATION` (0xc0000005) in
`export --test export_sequence` — a native memory fault, not an assertion
failure (the summary read `failed=0`, because nothing failed; the process
crashed). Twelve isolated runs of that same binary crashed zero times, so it
needs the load of a full workspace run to surface. Evidence kept in
`docs/evidence/2026-08-19-export-access-violation.md`.

**Root cause:** `render::headless_context()` builds a brand-new
`wgpu::Instance`, adapter and `Device` on every call — nothing is cached or
shared. `export_sequence` has 18 tests that each run a real export, and Rust
runs them in parallel, so a workspace run has many simultaneous GPU device
creations and teardowns across four crates' test binaries. That is a known
way to trip Windows GPU drivers.

**Not caused by the export-scaling work.** At `OutputScale::Native` —
which every crashing test uses — `scaled_dimensions` early-returns, so
`out_width == width` and `Encoder::open` receives values identical to the
pre-change code. The scaler configuration is unchanged. The one test added
in that batch passed before the crash, visible in the log.

**Production is not affected.** `start_export` refuses to begin a second
export while one is running, so the app only ever holds one headless device
at a time. This is a test-parallelism problem.

**Deliberately not fixed here.** The obvious fix — cache the device in a
`OnceLock` so the suite shares one — touches shared `render` infrastructure
used by four crates plus two production call sites, and the crash cannot be
reproduced on demand (1 in 6 under load, 0 in 12 isolated), so a fix could
not be honestly verified as working. Flagged for a deliberate change rather
than a drive-by one.

## 2026-09-08 — the proxy toggle muted audio, the same way matting did

The 7 September audit turned up a confirmed bug: ticking "Use proxies" made
every proxied clip silent for the rest of the session. Same shape as the
background-removal audio loss fixed on 19 August, still live in the proxy
path, which that entry had flagged as "likely, unverified" and left alone.

`generate_proxy` writes a video-only file on purpose — that is the whole point
of a proxy — but `start_playback` built one resolved path map and gave it to
both engines. The audio engine got the proxy, `AudioDecoderStream::open`
failed on a file with no audio stream, and `DecodedSampleSource` cached that
failure permanently.

Fixed by making the decision explicit and testable instead of implicit in the
event loop: `playback_paths()` returns a pair — video takes the substitutions
(proxy, then matting layered on top so a background-removed asset wins), audio
takes the originals. Neither substitution ever changes what the audio should
be, so the original is both the simplest and the correct answer. Export was
never affected, because it does not resolve proxies at all.

Two tests pin the policy. The first was written against the old behaviour and
watched to fail — it resolved audio to `/cache/proxy.mp4` — so it demonstrably
detects the real bug rather than just documenting the fix.

The lasting note is in proxy.rs's own doc comment, which claimed "audio still
comes from the source" from the beginning. That was true of the function and
false of the pipeline around it. It now states the constraint on callers and
names where it is enforced. A comment describing an invariant nothing checks
is worth about as much as no comment.

## 2026-09-08 — scratch directories now get cleaned up

Second finding from the 7 September audit. `ProxyJobs` and `MattingJobs` each
created `%TEMP%/nle-{proxies,mattes}-<pid>` and nothing ever deleted them, so
every run left one behind permanently. Mattes are the expensive half: lossless
QTRLE, about 4 MB per second of footage, so a few background-removal sessions
reach gigabytes.

Cleaning up on exit alone would not have fixed it, because the directories
that actually accumulate come from runs that never reached a clean exit. So
there are two halves: a clean exit deletes this process's directories, and
startup sweeps what earlier runs left.

The sweep deletes recursively, which makes it the risky half, so it is
deliberately narrow — only the two prefixes it owns, never this process's own
directories, and nothing younger than a day. The age check is what protects a
second editor running right now: its cache is minutes old, never days. That
also means the design accepts leaving one crashed session's directory around
until the next launch, which is the right trade against any chance of deleting
a live cache.

The two safety tests were written against a deliberately unguarded sweep and
watched to fail: without the prefix and own-pid guards they delete an
unrelated application's directory and a live instance's cache. A test for a
destructive operation is worth little until you have seen it catch the
destructive version. Also verified in the real binary rather than only in
tests: a backdated scratch directory was removed at startup while a backdated
*unrelated* directory and a fresh scratch directory both survived.

Incidentally removed the duplicated `ensure_dir` both job managers carried.

## 2026-09-08 — state.rs split by responsibility

The last item from the 7 September audit, and the one carried furthest: it was
the top organization finding in August at 5,391 lines and had grown to 5,525.

The shape mattered more than the number. Of those lines, 3,070 were the test
module; the production half was almost entirely a single `impl EditorState`
block of 2,195 lines holding 89 methods. So this was never "a 5,500-line file"
in the way that number suggests — it was one enormous impl block with a large
test suite attached.

Reading the methods in source order made the seams obvious, because related
ones had been written next to each other all along: selection and clipboard,
per-track gain/pan/mute/solo, titles and transitions, the media-analysis batch,
the speech features, everyday editing, parameter animation, and project I/O.
The split follows those existing runs rather than imposing a new taxonomy —
`mod.rs` keeps the types, the lifecycle and the undo/op plumbing everything
else routes through, and eight sibling modules take a topic each.

Two mechanical notes worth keeping. Child modules use `use super::*`, so no
import churn: each file is a header, that line, and an `impl EditorState`
block. And the helpers now called across module lines became `pub(super)`
rather than `pub` — still private to `state`, just no longer private to one
file inside it. The compiler named every one of them; there was no guessing
about which helpers were shared.

Checked as behaviour-neutral rather than assumed: 223 functions before and
after, 116 tests before and after, 538 passing either way, clippy clean.

The tests stayed in one file deliberately. They were written against
`EditorState` as a whole and share fixture builders, so splitting them to
mirror the production split is a separate job with its own risk, and bundling
it here would have made a structural change impossible to review.

## 2026-09-10 — analysis actions run in the background

Scene cuts, silence removal, beats, loudness matching, stabilization, captions
and transcription all ran inside their button's click handler. Each one decodes
the whole clip first, so each froze the editor — no repaint, no input — for as
long as that took: seconds on a short clip, minutes on long footage. The
comment above the buttons had flagged it as follow-up work since August.

Each action is now split in two. The measure half is a free function over plain
data (the clip, the asset paths, the sample rate), so it runs on a worker
thread. The apply half is the existing `EditorState` code that turns a
measurement into edits through the undo stack. A click snapshots the inputs and
returns at once; `poll_analysis` runs every frame beside the proxy and matting
polls and applies whatever has finished. A running action shows a spinner where
its button was.

The real design question was what a result means once the user has kept
editing, because cut positions, silence ranges and stabilization keyframes are
all computed against where the clip *was*. The rule: a result applies only if
the clip's track, source, trim, timeline position and speed are all unchanged;
otherwise it's discarded, and the status line says why. Per-action rules would
let some results survive a harmless move — scene cuts are source-relative — but
one rule can't razor the wrong spot, and re-running after a nudge costs far
less than a wrong edit. Edits that don't touch those fields (effects, gain,
other clips) leave a result valid, which is the case that matters: nobody sits
idle for a minute waiting.

Three smaller decisions:

- A keyframed-speed clip is refused at the click, not after the decode. Every
  action except loudness would have thrown its result away there anyway.
- Opening a project replaces the job channel. Clip ids restart per project, so
  an old result would otherwise land on whichever new clip reused its id.
  Workers can't be cancelled mid-decode; they finish and their result is
  dropped.
- A panic inside a worker is caught and reported as a failure. The export and
  proxy jobs don't do this — the August audit's "worker panic strands jobs"
  finding — and new code shouldn't repeat it.

The blocking `detect_*` methods survive, compiled only for tests, because the
real-footage tests drive measure and apply together through them.
`transcribe_clip` had no other caller and is gone.

Known limit: nothing caps concurrent jobs, so starting several analyses on
several long clips runs all of them at once.

Checked: 13 new tests — applying against an unchanged clip; discarding after a
move and after a delete; still applying after an unrelated gain edit; failure
reporting; loudness, captions and transcription results; the project-switch
drop; the up-front keyframed refusal; one end to end through a real worker
thread; and, at the job layer, a panicking job and a refused duplicate click.
551 passing across the workspace.

## 2026-09-10 — security check follow-up: pinned binaries, a media allowlist, build inputs

The September security check found two Medium and three Low issues. All five are
addressed here. `docs/security.md` is the standing reference; this entry records
why each fix took the shape it did.

**Medium: nothing verified the native code loaded from outside the build.**
`onnxruntime.dll`, `whisper-cli.exe` and the DLLs beside it, both models, and
the FFmpeg DLLs were all trusted by path alone. A new `integrity` crate pins
SHA-256 hashes and refuses a mismatch. Two choices are worth keeping:

- **Hash through a handle that shares read access but denies write, rename and
  delete, and hold it until the load is done.** Checking and then loading by
  path leaves a window for a swap, and share modes close it. This was measured
  before anything was built on it: with the handle held, `whisper-cli.exe` still
  ran and `onnxruntime.dll` still loaded, while rename, write and delete were
  all refused.
- **Pin every file the CLI can load, not just the ones this machine uses.**
  `whisper-cli` picks a `ggml-cpu-*.dll` variant to match the CPU at startup.

FFmpeg's DLLs are imported by `nle.exe` itself, so they load before any of the
app's code runs. The deploy script is therefore the only place they can be
checked. It checks them at the source, then again on the copies.

Found along the way: turning the integrity error into an `ort::Error` made `ort`
initialise its API. Before `init_from` has run, that means
`LoadLibrary("onnxruntime.dll")` by bare name. It found an unrelated 1.17.1 copy
on this machine's search path and panicked on the version mismatch. A missing
runtime already took the same path before this change, so it crashed the
matting worker instead of reporting an error. `RvmSession::load` now has its own
error type, and it touches no `ort` API until the verified DLL is loaded.

**Medium: untrusted media reaches FFmpeg unsandboxed, and the bundled build is
ageing.** A sandbox is the real fix, and it isn't built. What changed instead:
every open in `media_ffmpeg` goes through `open_input`, which sets
`format_whitelist` to the demuxers import can need, plus
`protocol_whitelist=file`. FFmpeg picks a demuxer by content, so before this a
`.mp4` could select any of hundreds, including ones that open other files. The
regression test is a concat script named `holiday.mp4`. Before the change,
`probe` opened it and followed it to the file it named; now it's refused.

On the "never updated" part: the installed 7.1.5 is still the newest 7.1
release, but BtbN no longer builds 7.1 at all. Moving to 8.1 needs
`ffmpeg-next` 8, and hasn't been done.

**Low: `ort`'s default features pulled in a build-time HTTP client and native
TLS**, to download a runtime this project never uses. It's now
`default-features = false`, keeping only `load-dynamic` and `api-27`. `ureq`
and `native-tls` are gone from `Cargo.lock`.

**Low: the local account name appeared in eight public files.** Code and
scripts now find tools through `integrity::tools_dir()`: `NLE_TOOLS_DIR` if
set, otherwise `%USERPROFILE%\tools`. `.cargo/config.toml` can't expand
environment variables, so it's now generated by `scripts/configure-machine.ps1`
from a committed template and gitignored; a fresh clone runs that script once.
The docs had the path replaced. Git history still contains it, and rewriting a
published history is a decision, not a cleanup step.

**Low: an unexplained prebuilt `libshlwapi.a` was committed.** Its origin is
now proven, not just asserted: it's byte-identical to the copy in winlibs'
MinGW-w64 build (GCC 16.1.0 r4). `build.rs` refuses to build if it changes.

Checked:

- Each new test failed without its fix:
  - ONNX Runtime parsed the tampered model ("Protobuf parsing failed").
  - Transcription tried to execute the swapped file.
  - The concat script opened.
- The real RVM model and the real `whisper-cli` both ran with their files held.
- The deploy script's check accepted all eight FFmpeg DLLs, and refused a wrong
  hash and a missing file.
- The workspace build is clean, with the `libshlwapi.a` pin checked on every
  build.
- Clippy is clean, and 562 tests pass across the workspace (551 before, plus
  the 11 new ones).
- `Cargo.lock` no longer contains `ureq` or `native-tls`.
- The real-model and real-`whisper-cli` tests pass again with tool paths coming
  from `tools_dir()`.
- The regenerated `.cargo/config.toml` holds exactly the build settings of the
  file it replaced.
- No tracked or unignored file contains the account name.

## 2026-09-10 — a crafted project file could crash the editor with a divide-by-zero

Found in a follow-up check right after the fixes above shipped. Project files
are untrusted input (opened from disk, no schema-level bounds on the values
they carry), and nothing validated a clip's `SpeedCurve::Constant` before
seven call sites divided by its numerator: `scene_cut_ops`,
`silence_removal_ops`, `beat_markers` and `stabilization_keyframes` in
`analysis.rs`, and `apply_captions`, `caption_ops` and
`timeline_words_from_transcript` in `transcript.rs`. A clip with numerator `0`
reached `* denominator / numerator` on the UI thread the moment any of Detect
Scene Cuts, Remove Silence, Detect Beats or Generate Captions ran against it
— divide-by-zero panics in Rust, so the editor exited outright. `start_analysis`'s
up-front check only tested for `SpeedCurve::Constant { .. }`, which a zero or
negative numerator or denominator still matches.

Fixed with one guard, not seven: `constant_speed(&SpeedCurve) -> Option<(i64,
i64)>` in `state/mod.rs`, returning the pair only for a `Constant` with both
numerator and denominator positive — the shape every legitimate speed (the
UI never creates anything else) already has. Every division site now
destructures through it instead of the raw `SpeedCurve::Constant` pattern,
and `start_analysis`'s guard calls it too, so a crafted zero-speed clip is
refused before a worker thread even starts rather than discovered after
decoding.

Stabilize was already safe by construction: its division runs inside the
worker closure, which `AnalysisJobs::spawn` wraps in `catch_unwind`, so it
would have reported a failure rather than crashing. Fixed anyway, for the
same reason `start_analysis` now refuses it up front — decoding a whole clip
only to report "the analysis crashed" is worse than refusing immediately.

Checked: three new tests, each failing against the pre-fix code — poll_analysis
applying five different finished results against a zero-speed clip (all
discarded, nothing computed), start_analysis refused for numerator or
denominator zero or negative, and stabilization_keyframes returning empty
instead of panicking. 565 tests pass across the workspace, clippy clean.

## 2026-09-11 — a finished analysis could clobber a still-running one's status

Found by hand, on the deployed build: importing a real recording and running
Detect Scene Cuts, then Remove Silence on the same clip while it was still
going. Remove Silence finished first, and "removed 47 silent gaps" replaced
"Detecting scene cuts…" in the status line — while scene-cut detection was
still visibly running. Both jobs are independent by design (different
`AnalysisKind`s on one clip run concurrently, see the 09-10 entry above), so
this wasn't rare; it's the normal shape of using two of these buttons close
together.

Root cause: `start_analysis` wrote `kind.running_label()` into the single
shared `EditorState::status` the moment a job started, alongside whatever a
different, still-in-flight job had already put there. But `analysis_button`
already shows that exact label in the button's own place for as long as the
job runs — the write to `status` was pure redundancy, and the one place nothing
else was competing to keep it accurate.

Fixed by deleting the write, not by adding bookkeeping to arbitrate between
jobs: `status` is now left alone on a successful start, same as it already was
on a refused duplicate start ("its spinner is already showing"). The button
remains the one source of truth for "is this job still running"; `status` goes
back to being what it is everywhere else in the app — a one-shot line for
refusals, results and errors, with no job's "in progress" state competing to
own it.

Checked: a new regression test starts a job with an unrelated message already
in `status` and asserts starting doesn't touch it — failing against the
pre-fix code, passing after. 566 tests pass across the workspace, clippy clean.

## 2026-09-11 — the panic guard reaches the other four background jobs

The August audit's "worker panic strands jobs" finding was fixed in
`AnalysisJobs::spawn` and nowhere else — its own comment says so ("Export and
proxy jobs don't do this… New code shouldn't repeat it"), which turned out to
be a note about the other four rather than a note about the past. Export,
proxies, matting and waveforms all still spawned a bare thread whose last
statement was the send (or the store) that reports the result, so a panic
skipped it and left the job marked as running forever.

Each one wedges differently, and none of them recover:

- **Export** — `result` stays `None`, which is exactly what "still rendering"
  looks like. The progress window sits at its last frame, Cancel sets a flag
  no live thread reads, and `start_export`'s `if export.job.is_some()` refuses
  every later export for the rest of the session, silently.
- **Waveforms** — worst of the four, because nothing user-initiated is needed
  to reach it: peaks generate on their own for every audio clip. A stranded
  job holds one of only `MAX_CONCURRENT_JOBS` (2) slots permanently, so a
  second one deadlocks the queue and no clip ever gets a waveform again. Its
  `is_busy()` also feeds `request_repaint`, so the editor redraws at full rate
  forever.
- **Proxies and matting** — the asset reads "building" forever, and `request`
  refuses anything not in state `None`, so it can never be asked for again.

Fixed with one `catch_panic` in `background.rs` rather than a fifth and sixth
copy of the `catch_unwind` incantation: it runs the work, returns `None` if it
panicked, and each caller turns that `None` into whatever "this failed" already
means for its own job type. So a panic now surfaces through the same path as an
unreadable file — a failure the user can see and retry — instead of a job that
quietly never ends. `AnalysisJobs` keeps its own inline guard; it is already
correct and tested, and rewriting working code for uniformity is not worth the
diff.

The honest limit of the tests: `catch_panic` itself is covered both ways (a
panicking closure returns `None`, a normal one passes through), but the four
call sites are one-line wiring verified by compiling and by reading, because
none of those work functions can be made to panic from a test without
reshaping their APIs to take injectable work the way `AnalysisJobs::spawn`
already does.

## 2026-09-11 — a media file's own time base could divide by zero

Found while looking for something that could actually trigger the panics
above. A stream's time base is container metadata — `open_input`'s demuxer
and protocol allowlist restricts which parsers run, not what they report — so
a malformed or crafted file can declare a denominator of `0`.
`VideoDecoderStream::convert_to_rgba` and `AudioDecoderStream::next_chunk`
both divided by it unchecked when converting a frame's pts to ticks.

The same bug class as the zero-speed crash the day before, reached through a
media file instead of a project file, and the asymmetry is what gives it
away: every *other* reader of a raw FFmpeg time base in this crate already
guards the denominator — `to_rational` clamps with `.max(1)`, `probe`'s
duration maths tests `!= 0`, and both the proxy and matte encoders check
before inverting the frame rate. Only the two decoder streams didn't.

Fixed with a shared `pts_to_ticks(Option<i64>, Rational)`, so the two of them
now share one guard instead of holding a third and fourth copy of the check.
A frame whose timestamp can't be placed lands at tick 0 — the same answer the
old code already gave for a frame carrying no timestamp at all.

Where it landed mattered as much as the panic: both streams run on worker
threads (playback's mix and decode threads, export, and peaks generation), so
before the fix above this surfaced not as a visible crash but as one of those
silent permanent wedges.

Checked: a new test asserts a zero denominator returns 0 rather than panicking,
and the pre-fix arithmetic was confirmed to panic on the same input. 570 tests
pass across the workspace, clippy clean.

## 2026-09-11 — a stored transcript now expires the same way an in-flight one does

Two leftovers from the same hunt, both about state that outlived the thing it
described.

**In/out marks survived `open_from`.** That loader is careful — it clears
transcripts and asset paths, resets `next_id` past every id in the incoming
file, drops the selection and the playhead — but not the marks. Marks are
positions in *a* sequence, so they mean nothing in a different one, and they
aren't inert: `start_export` scopes a range export to them. Now cleared with
the rest.

**Stored transcripts were never revalidated.** `Placement` exists because an
analysis result computed against where a clip *was* must not be applied after
it moves, and `apply_finished` enforces that for results still in flight. But
once the words reached `EditorState::transcripts` nothing checked them again,
and word ticks are derived from exactly what `Placement` fingerprints — the
clip's `timeline_in`, its speed, and which slice of the source was
transcribed. So moving or trimming a transcribed clip left the panel offering
words whose ticks pointed somewhere else: clicking one seeks to where it used
to be, and `delete_word_range` ripple-deletes by those ticks.

What kept that from being a wrong cut was `delete_word_range`'s existing bounds
guard — a moved clip's stale ticks usually fall outside its new bounds and get
refused. "Usually" is the problem, and the message it refuses with ("it touches
the clip's own edge") describes a different situation entirely. The edits that
would defeat the guard outright while keeping the words in bounds — slip and
rate-stretch, which change what plays without changing where — exist in
`edit_ops` but aren't wired to any UI gesture yet, so this was a wart today and
a real miscut the moment a slip tool lands.

Fixed by storing the `Placement` next to the words and checking it on the way
out, through a new `EditorState::transcript` returning `Missing` / `Stale` /
`Ready`. Reusing `Placement` rather than working out a narrower rule is the
point: it is already the codebase's answer to "does this result still describe
this clip", and a second, subtly different answer to the same question is how
the two drift apart. It inherits the same conservatism — a nudge that happened
to leave the words valid still costs a re-transcribe, which is the cheaper of
the two mistakes.

The field is private now, so the panel and any future caller have to come
through that check; the panel gained a "this transcript is out of date —
transcribe again" state, which is also what it shows after a word-range delete
(the ripple shortens the clip, so the remaining words report themselves stale
on their own — the manual `transcripts.remove` that used to do that job is
gone, one mechanism instead of two).

Checked: two new tests — a moved clip's transcript reports `Stale` and refuses
to cut, and re-transcribing makes it `Ready` again, so staleness is
recoverable rather than a dead end — plus one that the marks don't survive a
load. The existing transcript tests moved onto `store_words`/`transcript`
rather than poking the map, so they now exercise the same path the editor
does. 573 tests pass across the workspace, clippy clean.

## 2026-09-13 — a project file is untrusted input, and now gets checked like one

A security pass over the whole codebase, not just the recent diff. The
reassuring half first: there is no `unsafe` anywhere in the workspace, the one
subprocess builds its argv as separate `.arg()` calls with nothing
user-controlled in it, the demuxer and protocol allowlists hold, and the
scratch directories are already per-process. With no network surface and no
privilege boundary in a single-user desktop app, the realistic ceiling for a
hostile file is a crash or a corrupted document — not code execution. Nothing
found was critical in that sense.

What the pass did find was a hole with a clear shape: **every defence this
project has was built around edits, and none of it was pointed at files.**

**Loaded projects skipped every invariant.** `edit_ops::apply` re-validates
the entire project after every single edit and refuses to hand back a result
that breaks the rules — the whole reason that check exists is that the
property suite kept finding corruption an operation's own logic couldn't see.
`project::load` checked the schema version and nothing else. So a file was the
one way into the editor that bypassed the check entirely, and the failure mode
was worse than "opens something wrong": because `apply` validates the *whole*
project, a single pre-existing overlap made every subsequent edit fail, with an
error naming the edit the user had just tried rather than the file they had
opened. The project loaded, played, and could not be worked on, and nothing
said why.

`invariants::check_project` now holds an arriving project to the same standard
as an edited one, and `load` refuses what fails — refusing rather than
repairing for the reason the schema-version check right above it already gives:
guessing at what a broken file meant is how the next save destroys the work.
Two deliberate asymmetries. Clip storage order is *normalised*, not rejected,
because `apply` sorts before it checks and an out-of-order vec is a thing the
editor itself writes. And undo history is checked but dropped rather than
refused — it is recoverable context rather than the work, schema v3 already set
that precedent, and left in place a poisoned entry would just move the failure
one Ctrl+Z away.

This also closed the `sample_rate` question the pass opened separately: it is
project-controlled and was never validated. Tracing it showed it never reaches
an integer divisor — export hardcodes 48kHz and playback uses the device rate —
so it was not the third divide-by-zero it first looked like, but a rate of zero
still produced `inf` timings in silence and beat detection. It is bounds-checked
on load now with the rest.

**Frame dimensions were whatever the file said.** `width * height * 4` was
`u32`, which wraps in release, and nothing capped the values going into it —
so a file's headers decided how much memory the process asked for, and at the
top of the range decided it *wrongly*, allocating a buffer too small for the
copy that followed. Now computed in `u64` against `MAX_DECODED_PIXELS`
(8192x8192, above 8K DCI) and checked once when the decoder is created, before
the scaler — which would otherwise be the first thing to allocate against
those dimensions — rather than per frame.

**A "relative" media path could be absolute.** `Path::join` silently discards
the base when the joined path is absolute, so the project field documented as
relative could name any file on the machine, and the editor would open and try
to decode it. `resolve_relative_media` refuses anything absolute or climbing
through `..`. The residual is deliberate and documented: `original_absolute_path`
is *supposed* to be an arbitrary path, so a crafted file can still point at one
— bounded by the allowlist, which admits no demuxer that can reach further
files or URLs, and by there being no network path out of this process.

**Untitled sessions shared one recovery file.** `cache_dirs` is per-process
precisely so two editors can't overwrite each other; the untitled recovery file
sat at one fixed temp path and did exactly that — last autosave wins, and the
loser's unsaved work is gone. Per-process now, with `find_recovery` searching
the prefix rather than testing one path, because the file that matters was
written by a process that no longer exists.

Checked: 13 new tests, including the ones that guard against overcorrecting —
an ordinary project still opens, out-of-order storage still opens and comes
back sorted, and real frame sizes up to 8K are still allowed. 586 tests
pass across the workspace, clippy clean.

## 2026-09-13 — an edit arriving mid-drag crashed the editor

`UndoStack::push` asserted that no coalescing group was open, and that
assert was reachable from ordinary use. Hold the mouse down on a clip and
press Delete: `lift_selection` -> `apply_ops` -> `push` -> panic, process
gone, unsaved project with it. Worse, no keystroke is needed — `poll_analysis`
runs every frame whatever the pointer is doing, so a scene-cut or silence job
finishing while a clip is being dragged lands on the open group by itself.

The per-frame safety net (`force_close_stale_drag`) was already wired up and
is not at fault: it closes a group whose mouse-up was never observed, and it
correctly does *not* fire while the pointer is still down, because at that
moment the gesture really is still live. That is precisely the window the
assert made fatal. (Worth recording that the first diagnosis of this claimed
the safety net was dead code — that was a bad grep for a function name that
doesn't exist, and it was wrong. The crash was real and reproduced; the
explanation of it wasn't.)

Fixed in the stack rather than at the keyboard. `push`, `push_or_amend` and
`begin_coalescing` now commit an open group instead of asserting, and
`end_coalescing` is idempotent — the widget that opened a gesture has no way
to know the stack already closed it underneath. Doing it there covers all 24
`undo.push` call sites and the background-job path at once; gating the key
handler would have fixed one of the two triggers and left the other.

The invariant the assert protected still holds. It existed so a drag couldn't
be recorded as 400 separate steps; closing the group first yields exactly two
— the gesture as one step, then the edit as its own — which is also the order
Ctrl+Z walks back through, and is what the new tests pin.

`EditorState::coalescing_open` stopped being a stored bool and became a read
of `undo.is_coalescing()`. It had always been a mirror of the stack's state,
which was survivable while only the widgets could open and close groups; now
that the stack closes one on its own, a mirrored flag has no way to learn that
happened. One source of truth removes the drift rather than documenting it.

Checked: 3 new tests — a keystroke mid-drag commits the drag and applies the
edit as two separate undo steps, a background analysis result landing mid-drag
leaves the drag intact, and ending a gesture twice is harmless. 589 tests pass
across the workspace, clippy clean.

## 2026-09-13 — ripples left transitions behind, and the renderer punished it

`edit_ops` maintains every timeline invariant there is, and it did not know
transitions existed — it only ever constructed empty `transitions: vec![]`.
So `ripple_shift` moved a track's clips and left its transitions at their old
absolute ticks. Measured, on A[0,1000) B[1000,2000) C[2000,3000) with a
dissolve on the A|B cut: ripple-delete A and the survivors slide to [0,1000)
and [1000,2000) while the transition stays at 1000 — which is now the *B|C*
cut. A dissolve placed between two particular shots silently moves to a
different pair, and exports that way.

The second symptom was worse, and came from the renderer rather than the
model. The compiler's transition branch resolves its two layers purely by
matching a clip edge to `tr.at` (`clips_at_cut`), and never falls back to the
"which clip covers this tick" lookup the ordinary branch uses. So a transition
whose anchor no longer lands on any cut found neither side and planned
*nothing* — a hole punched in footage that was perfectly intact underneath,
for the transition's whole duration, in preview and in export. Confirmed
directly: one clip [0,4000), stale transition at 2000, and the compiled plan
came back with no active clip and zero media requests, while the same clip one
tick outside the region planned fine.

Fixed at both ends, because each fix alone leaves a real gap. Ripples now
carry transitions using the same `>= at_or_after` rule the clips use, on
sync-locked sibling tracks too, so a transition can no longer drift onto a
neighbouring cut. And the compiler falls through to the ordinary lookup when a
transition's anchor matches neither a clip start nor a clip end, so any stale
anchor that still arises — from an older project file, say — is invisible
rather than destructive.

Deliberately narrow on that second one: a transition with only *one* side is
how a fade in/out is expressed (`clips_at_cut`'s own doc says so), so only a
transition with neither side falls through. A test pins that a head fade is
still a transition, because the obvious over-correction here is to treat every
one-sided transition as stale and quietly delete every fade in the project.

Not adding transitions to the loader's `check_project`: with the fallback in
place a stale anchor is harmless, and refusing to open a file over something
the renderer now handles gracefully would be the worse trade.

Checked: 4 new tests — a ripple carries a transition, a transition upstream of
the edit stays put, a stale anchor shows the footage underneath, and a head
fade still renders as a transition. 593 tests pass across the workspace,
clippy clean.
