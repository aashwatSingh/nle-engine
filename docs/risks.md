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
