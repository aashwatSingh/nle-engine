# Decisions log

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
