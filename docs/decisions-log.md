# Decisions log

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
