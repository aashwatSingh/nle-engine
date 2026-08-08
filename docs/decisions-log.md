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
`C:\Users\aashw\tools\ffmpeg-n7.1-latest-win64-gpl-shared-7.1`), not the newer 8.1 build also downloaded during
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
16.1.0) extracted to `C:\Users\aashw\tools\mingw64`, not the MSVC target.

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
