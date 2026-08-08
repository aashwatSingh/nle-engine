# Top risks and spike plans — M0

Ranked by how much they'd force a structural rewrite if we got them wrong,
not by how likely they are.

## 1. Cross-platform GPU toolchain even working at all

**Risk:** before any of the other risks matter, we need wgpu to open a
window and render a frame on the actual dev machine. This sounds trivial and
isn't always — GPU driver state, wgpu backend selection (D3D12 vs Vulkan on
Windows), and toolchain gaps (see the MSVC linker issue hit during this same
M0 session) are exactly the kind of thing that silently blocks everything
downstream.

**Status:** in progress. Toolchain issues hit and fixed so far: missing
Rust install, missing MSVC C++ Build Tools (required elevation the agent
couldn't self-grant — needed the user to run an admin install). Confirmed
present: RTX 4060 Laptop GPU + Intel UHD (D3D12/Vulkan-capable), VS2022
Community installed.

**Spike:** open a wgpu window, pick a backend explicitly (try D3D12 first
since it's Windows' native API and best-supported), render a solid-color
triangle. Trivial in isolation — the point is proving the full chain
(rustc -> wgpu -> driver -> swapchain -> visible pixels) works before
anything is built on top of it.

**Result (2026-08-07):** ran. wgpu 0.20.1 (via `crates/spike_wgpu`) picked
`Backends::PRIMARY`, which selected **Vulkan** on the RTX 4060 Laptop GPU
(not D3D12 — wgpu's primary-backend selection on this machine preferred
Vulkan; not investigated further since either backend answers the question).
Adapter reported: `NVIDIA GeForce RTX 4060 Laptop GPU`, driver `560.94`.
Window opened, triangle+quad rendered, sustained 165+ fps uncapped (present
mode was whatever `caps.present_modes[0]` returned — not forced to
`Fifo`/vsync, so this number isn't a real playback-rate result, just
evidence the pipeline runs). Ran cleanly for 15s with no panics, no
validation errors on stderr. Toolchain chain confirmed working end to end
on the GNU/MinGW route (see decisions-log.md).

## 2. Concurrent multi-thread GPU texture upload

**Risk:** the playback pipeline's core assumption (spec 4.4) is that decoder
worker threads upload frames to GPU textures while the compositor thread
concurrently records render commands. If wgpu (or the underlying
D3D12/Vulkan backend) doesn't support this safely without serializing on a
single queue, the decode-pool-to-compositor handoff design in
`docs/architecture.md` step 4-7 needs rework before M2, not after.

**Spike:** prototype N worker threads each writing to a distinct
`wgpu::Texture` via `Queue::write_texture` while a separate thread
concurrently records and submits a render pass reading a different texture.
Measure whether this actually parallelizes or serializes on `wgpu::Queue`'s
internal lock.

**Result (2026-08-07):** ran with 1 background thread (not N — that's a
sharper version of this same test, worth doing once real decoder threads
exist in M1) writing a full 256x256 RGBA8 frame via `queue.write_texture`
at a 16ms-sleep target rate (~60Hz) while the main thread concurrently
recorded and submitted a render pass sampling that same texture every
frame. Measured over 15s: main thread sustained 165+ fps; background thread
sustained 52-55 writes/s (below the 60Hz target — bottlenecked by the CPU
cost of generating the 256KB gradient buffer per iteration in Rust, not by
GPU contention; the `write_texture` call itself wasn't separately timed).
No crash, no deadlock, no `wgpu` validation errors. This is real but weak
evidence: it shows the two threads *coexist* without corrupting state, not
that `wgpu::Queue` internals actually let them execute in parallel rather
than politely taking turns on an internal lock — telling those apart needs
per-call timing instrumentation, which wasn't built. Good enough to
unblock M1/M2 design; not good enough to certify the throughput headroom
the playback budget table needs. Revisit with N decoder threads and
instrumented timing once real decode exists.

## 3. Real-time reverse playback on long-GOP 4K

**Risk:** JKL reverse shuttle (spec 4.4) requires decoding forward from a
keyframe and buffering backward — for HEVC 4K with GOPs longer than ~1
second, this may simply not sustain real-time rates. The spec treats this as
"plan for it, not free"; it may be closer to "doesn't work at full
resolution, needs a documented fallback" (e.g., proxy-only reverse, or
capped reverse frame rate).

**Spike:** with a real HEVC 4K sample (once ingest/decode exists — this
spike depends on M1, not M0), measure achievable reverse fps at varying GOP
lengths. Until then, this stays a named risk with no code — spiking it
before decode exists would just be re-deriving FFmpeg's own documented
seek costs.

## 4. FFmpeg-via-FFI crash isolation

**Risk:** spec section 8 requires the ingest path to survive fuzzed/corrupt
media without ever crashing, over an 8-hour soak. FFmpeg, wrapped via FFI in
the same process as the UI and playback engine, occasionally does crash on
adversarial input regardless of how carefully the Rust wrapper is written —
that's a property of the C library, not the wrapper.

**Decision needed before M1:** in-process (accept the residual crash risk,
mitigate with aggressive fuzzing) vs. out-of-process decode (isolate each
decoder in a child process, like Chrome does for codecs — much stronger
guarantee, adds IPC complexity for handing GPU texture data across a process
boundary, which itself may be its own spike).

**Spike:** not started. This is a decision-before-building item, not a
code spike — the two options lead to different M1 architectures for the
decoder pool.

## 5. "Preview equals export" is in tension with proxies

**Risk:** spec's Definition of Done item 9 (correct export) plus section
4.8 ("preview must be a true representation of export... if they diverge,
that is a P0 bug") implies pixel-identical output. But the performance
budget table explicitly permits proxy playback at reduced resolution for
heavy timelines. Bit-identical preview vs. full-res export is not
achievable by construction if preview is ever proxy-res.

**What "true representation" should actually mean, pending user input:**
same render graph, same effect math, same color pipeline — differing only in
resolution/precision when proxies are on. Perceptual-diff-threshold
equivalence (already the spec's own golden-frame test methodology, section
8) rather than bit-identical.

**Spike:** once the render graph and one real source exist (M4), render one
frame both through the "preview" path and the "export" path at matched
resolution and diff them. This tells us now whether the render graph itself
is deterministic and reusable, independent of the proxy-resolution question,
which needs a product decision, not a spike.

**RESOLVED (M4).** Two parts:

*The engineering half is done and proven.* `Compositor::render_to_view`
(preview) and `Compositor::render_to_rgba` (export) are thin wrappers over
one shared `composite_to_working` + `deliver` core — there is no second
copy of the compositing logic to keep in sync, so "preview matches export"
is structural rather than maintained by hand.
`acceptance_preview_and_export_paths_produce_identical_pixels` (in
`crates/render/tests/compositor_gpu.rs`) renders a non-trivial two-track
graph — blending, opacity, scale, rotation, position, colour conversion —
through both entry points and asserts the readbacks are **byte-identical**,
plus asserts the frame isn't a flat colour so the comparison can't pass
vacuously. This satisfies spec M4's acceptance criterion.

*The definitional half is now settled as a documented decision:* "preview is
a true representation of export" means **same render graph, same effect
math, same colour pipeline, byte-identical at matched resolution.** It
explicitly does *not* claim a proxy-resolution preview is byte-identical to
a full-resolution export — that's impossible by construction, and any spec
reading that demands it is unachievable rather than merely unimplemented.
When proxies are enabled, the correct bar is the spec's own golden-frame
methodology (section 8): perceptual-difference threshold, not bit equality.
The P0-bug bar from section 4.8 therefore applies to *graph/math/colour*
divergence, which is what's now tested, and not to resolution.
