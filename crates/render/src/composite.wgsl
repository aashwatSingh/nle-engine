// Compositing shaders, M4 + M5.
//
// Per-clip pipeline (see compositor.rs's `process_clip_source` /
// `composite_to_working`):
//
//   1. `vs_fullscreen` / `fs_prepare` — convert the encoded source into the
//      linear working space, into an intermediate texture at source
//      resolution. Skipped entirely for a clip with no pixel effects (the
//      fast path folds this into step 4).
//   2. `vs_fullscreen` / `fs_gaussian_blur`, `fs_color_correction`, `fs_crop`
//      — the clip's pixel-effect stack, in order, ping-ponging between two
//      intermediate textures. Only present if the clip has that effect.
//   3. `vs_clip` / `fs_clip` — place the (already-linear) result into the
//      shared working-space composite target: transform, opacity, mask, and
//      blend. The FAST PATH (no pixel effects) also uses this shader
//      directly on the raw encoded source, with its transfer/gamut uniforms
//      doing the colour conversion that step 1 would otherwise have done —
//      one draw call instead of three for the common case of an untouched
//      clip.
//   4. `vs_fullscreen` / `fs_deliver` — convert the accumulated working-space
//      target into the encoded delivery space for output.
//
// The transfer-function, gamut, and effect math here MIRRORS
// crates/render/src/color.rs and crates/render/src/effect.rs's documented
// formulas. That duplication is deliberate: the CPU side is the tested
// reference, and matrices/codes are fed in via uniforms rather than
// hardcoded here, specifically so the two can't silently diverge.

struct ClipUniforms {
    src_size: vec2<f32>,
    seq_size: vec2<f32>,
    position: vec2<f32>,
    scale: vec2<f32>,
    anchor: vec2<f32>,
    rotation: f32,
    opacity: f32,
    // Rows of the source-primaries -> Rec.709 matrix; .w unused. Identity
    // when the source texture is already linear-working-space (the
    // multi-stage path's placement step).
    gamut0: vec4<f32>,
    gamut1: vec4<f32>,
    gamut2: vec4<f32>,
    transfer_code: u32,
    // Multiplies working-space colour only, never alpha. 1.0 = untouched,
    // 0.0 = opaque black. See compositor.rs's mirror of this struct for why
    // dip-to-black needs this rather than reusing `opacity`.
    rgb_scale: f32,
    // Wipe boundary position in this clip's UV space, 0..1, read only when
    // `wipe_enabled` is 1. Kept separate from MaskUniforms so a wipe and the
    // clip's own mask compose instead of one replacing the other.
    wipe_progress: f32,
    wipe_enabled: u32,
};

struct MaskUniforms {
    // Vec2 fields grouped first: WGSL aligns vec2<f32> to 8 bytes, and this
    // ordering is what makes the Rust #[repr(C)] mirror's natural 4-byte
    // packing land on the same offsets without manual padding — see
    // compositor.rs's comment on this struct for the reasoning.
    center: vec2<f32>,
    size: vec2<f32>,
    feather: f32,
    enabled: u32,
    is_rectangle: u32,
    invert: u32,
};

@group(0) @binding(0) var<uniform> clip_u: ClipUniforms;
@group(0) @binding(1) var t_source: texture_2d<f32>;
@group(0) @binding(2) var s_source: sampler;
@group(0) @binding(3) var<uniform> mask_u: MaskUniforms;

struct ClipVertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

fn quad_uv(vertex_index: u32) -> vec2<f32> {
    var uvs = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
    );
    return uvs[vertex_index];
}

@vertex
fn vs_clip(@builtin(vertex_index) vertex_index: u32) -> ClipVertexOut {
    let uv = quad_uv(vertex_index);

    // Offset from the anchor point, in source pixels, after scaling.
    let scaled = clip_u.src_size * clip_u.scale;
    let local = (uv - clip_u.anchor) * scaled;

    // Clockwise rotation as seen on screen. Screen-space y grows downward
    // while this is applied before the NDC y-flip below, so the sign pattern
    // here is the one that makes a positive `rotation` read as clockwise to
    // the viewer (pinned by a compositor test, not by inspection).
    let theta = radians(clip_u.rotation);
    let c = cos(theta);
    let s = sin(theta);
    let rotated = vec2<f32>(
        local.x * c - local.y * s,
        local.x * s + local.y * c,
    );

    // Sequence-pixel position relative to frame centre, then to NDC.
    let centred = clip_u.position + rotated;
    let ndc = vec2<f32>(
        centred.x / (clip_u.seq_size.x * 0.5),
        -centred.y / (clip_u.seq_size.y * 0.5),
    );

    var out: ClipVertexOut;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    return out;
}

// Mirrors color.rs::srgb_to_linear.
fn srgb_to_linear_1(v: f32) -> f32 {
    if (v <= 0.04045) {
        return v / 12.92;
    }
    return pow((v + 0.055) / 1.055, 2.4);
}

// Mirrors color.rs::rec709_to_linear.
fn rec709_to_linear_1(v: f32) -> f32 {
    if (v < 0.081) {
        return v / 4.5;
    }
    return pow((v + 0.099) / 1.099, 1.0 / 0.45);
}

// Mirrors color.rs::linear_to_srgb.
fn linear_to_srgb_1(v: f32) -> f32 {
    if (v <= 0.0031308) {
        return v * 12.92;
    }
    return 1.055 * pow(v, 1.0 / 2.4) - 0.055;
}

// Mirrors color.rs::linear_to_rec709.
fn linear_to_rec709_1(v: f32) -> f32 {
    if (v < 0.018) {
        return 4.5 * v;
    }
    return 1.099 * pow(v, 0.45) - 0.099;
}

// Codes must match color.rs::shader_codes.
const TRANSFER_LINEAR: u32 = 0u;
const TRANSFER_SRGB: u32 = 1u;
const TRANSFER_REC709: u32 = 2u;
const DELIVERY_REC709: u32 = 0u;
const DELIVERY_SRGB: u32 = 1u;

fn to_linear(rgb: vec3<f32>, code: u32) -> vec3<f32> {
    if (code == TRANSFER_LINEAR) {
        return rgb;
    }
    if (code == TRANSFER_SRGB) {
        return vec3<f32>(srgb_to_linear_1(rgb.r), srgb_to_linear_1(rgb.g), srgb_to_linear_1(rgb.b));
    }
    return vec3<f32>(rec709_to_linear_1(rgb.r), rec709_to_linear_1(rgb.g), rec709_to_linear_1(rgb.b));
}

// Mask factor at `uv` (source-space, 0..1): 1.0 = fully shown, 0.0 = fully
// masked out, with a smooth ramp across `feather`. Evaluated in source UV
// space — see effect.rs's `mask` module doc comment for why: a mask is
// attached to the clip's own frame, so it moves/scales/rotates with the clip
// rather than staying fixed in sequence space.
fn mask_factor(uv: vec2<f32>) -> f32 {
    if (mask_u.enabled == 0u) {
        return 1.0;
    }
    var d: f32;
    if (mask_u.is_rectangle == 1u) {
        // Signed distance to the rectangle's edge, positive outside, in a
        // space where the rectangle's half-extents are (1,1) — so `feather`
        // (a fraction of the frame diagonal) needs converting into that same
        // normalised space via the size, avoided here by instead measuring
        // distance in UV space directly and normalising by size below.
        let delta = abs(uv - mask_u.center) - mask_u.size;
        let outside = max(delta, vec2<f32>(0.0));
        let inside = min(max(delta.x, delta.y), 0.0);
        d = length(outside) + inside;
    } else {
        // Ellipse: scale into a unit-circle space so `size` can be
        // non-uniform (different x/y radii).
        let normalised = (uv - mask_u.center) / max(mask_u.size, vec2<f32>(1e-5));
        d = length(normalised) - 1.0;
    }
    // `d` is in UV units for the rectangle case and in "radii" units for the
    // ellipse case — close enough for a feather fraction of the frame
    // diagonal in both cases without over-engineering a unified metric.
    let feather = max(mask_u.feather, 1e-5);
    var factor = 1.0 - smoothstep(0.0, feather, d);
    if (mask_u.invert == 1u) {
        factor = 1.0 - factor;
    }
    return factor;
}

// 1 where a wipe has already revealed this clip, 0 where it hasn't yet.
//
// The edge is softened over a fraction of a UV unit rather than being a bare
// `step`: a hard boundary on a diagonal-free vertical edge still crawls with
// visible stair-stepping as it moves sub-pixel amounts per frame, and this is
// the cheapest fix that doesn't need multisampling.
fn wipe_factor(uv: vec2<f32>) -> f32 {
    if (clip_u.wipe_enabled == 0u) {
        return 1.0;
    }
    let softness = 0.003;
    let revealed = 1.0 - smoothstep(
        clip_u.wipe_progress - softness,
        clip_u.wipe_progress + softness,
        uv.x,
    );
    // Mode 2 is the same boundary sweeping the same way, but hiding rather
    // than revealing — what a wipe at the tail of a track with nothing after
    // it has to do, mirroring how a cross dissolve there becomes a fade out.
    if (clip_u.wipe_enabled == 2u) {
        return 1.0 - revealed;
    }
    return revealed;
}

@fragment
fn fs_clip(in: ClipVertexOut) -> @location(0) vec4<f32> {
    let src = textureSample(t_source, s_source, in.uv);

    // Encoded -> linear light, then source primaries -> Rec.709 working
    // primaries. No clamping: out-of-gamut negatives are meaningful here and
    // only get clamped at delivery. A no-op (identity gamut, linear
    // transfer) when the source is already a linear working-space
    // intermediate from the multi-stage effect pipeline.
    let linear = to_linear(src.rgb, clip_u.transfer_code);
    let working = vec3<f32>(
        dot(clip_u.gamut0.xyz, linear),
        dot(clip_u.gamut1.xyz, linear),
        dot(clip_u.gamut2.xyz, linear),
    );

    // Scaling colour here — in linear light, after the gamut conversion and
    // after any effect chain — is what makes a dip fade to a true black rather
    // than to a gamma-encoded grey. Alpha deliberately untouched: see
    // `rgb_scale`'s doc.
    let m = mask_factor(in.uv);
    return vec4<f32>(working * clip_u.rgb_scale, src.a * clip_u.opacity * m * wipe_factor(in.uv));
}

// --- single-uniform passes: prepare, effect chain, deliver ---
//
// These all share one bind group shape (one small uniform buffer + source
// texture + sampler) and the same full-screen vertex stage, since none of
// them position anything — they transform a texture into another texture of
// the same size.

struct FullscreenVertexOut {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_fullscreen(@builtin(vertex_index) vertex_index: u32) -> FullscreenVertexOut {
    let uv = quad_uv(vertex_index);
    var out: FullscreenVertexOut;
    out.clip_position = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}

// -- prepare: encoded source -> linear working space --

struct PrepareUniforms {
    gamut0: vec4<f32>,
    gamut1: vec4<f32>,
    gamut2: vec4<f32>,
    transfer_code: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> prep_u: PrepareUniforms;
@group(0) @binding(1) var t_prep_src: texture_2d<f32>;
@group(0) @binding(2) var s_prep_src: sampler;

@fragment
fn fs_prepare(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    let src = textureSample(t_prep_src, s_prep_src, in.uv);
    let linear = to_linear(src.rgb, prep_u.transfer_code);
    let working = vec3<f32>(
        dot(prep_u.gamut0.xyz, linear),
        dot(prep_u.gamut1.xyz, linear),
        dot(prep_u.gamut2.xyz, linear),
    );
    return vec4<f32>(working, src.a);
}

// -- Gaussian Blur (separable: one draw per direction) --

struct BlurUniforms {
    direction: vec2<f32>, // (1,0) for the horizontal pass, (0,1) for vertical
    radius: f32,          // source pixels
    _pad0: f32,
};

@group(0) @binding(0) var<uniform> blur_u: BlurUniforms;
@group(0) @binding(1) var t_blur_src: texture_2d<f32>;
@group(0) @binding(2) var s_blur_src: sampler;

@fragment
fn fs_gaussian_blur(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    let radius = max(blur_u.radius, 0.0);
    if (radius < 0.001) {
        return textureSample(t_blur_src, s_blur_src, in.uv);
    }
    let dims = vec2<f32>(textureDimensions(t_blur_src));
    let texel = blur_u.direction / dims;
    // sigma chosen so `radius` is roughly "2 standard deviations", a
    // common, visually reasonable convention (not a claim of matching any
    // specific NLE's blur curve).
    let sigma = max(radius * 0.5, 0.5);
    let taps = i32(ceil(radius));

    // NOTE: blurs RGB and (straight, non-premultiplied) alpha independently.
    // For a clip with partial transparency inside the blurred radius this can
    // fringe colour at the alpha edge — the correct fix is premultiplying
    // before the blur and un-premultiplying after, which is real, scoped-out
    // follow-up work rather than a claim that this is colour-fringe-free.
    var sum = vec4<f32>(0.0);
    var weight_sum = 0.0;
    for (var i = -taps; i <= taps; i = i + 1) {
        let w = exp(-0.5 * f32(i * i) / (sigma * sigma));
        sum = sum + textureSample(t_blur_src, s_blur_src, in.uv + texel * f32(i)) * w;
        weight_sum = weight_sum + w;
    }
    return sum / weight_sum;
}

// -- Color Correction --

struct ColorCorrectionUniforms {
    exposure: f32,
    contrast: f32,
    saturation: f32,
    temperature: f32,
    tint: f32,
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
};

@group(0) @binding(0) var<uniform> cc_u: ColorCorrectionUniforms;
@group(0) @binding(1) var t_cc_src: texture_2d<f32>;
@group(0) @binding(2) var s_cc_src: sampler;

@fragment
fn fs_color_correction(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    let src = textureSample(t_cc_src, s_cc_src, in.uv);
    var rgb = src.rgb;

    // Exposure: stops, multiplicative in linear light — physically what a
    // camera exposure stop actually does.
    rgb = rgb * exp2(cc_u.exposure);

    // Temperature/tint: a simple linear RGB gain shift, NOT a physically
    // based Planckian-locus white balance. Warmer (+temperature) pushes red
    // up and blue down; +tint pushes toward magenta (red and blue up
    // slightly, green down). Good enough for "warm this up a bit"; a
    // colour-managed white-balance model is real follow-up work.
    rgb.r = rgb.r * (1.0 + cc_u.temperature * 0.3 + cc_u.tint * 0.15);
    rgb.g = rgb.g * (1.0 - cc_u.tint * 0.3);
    rgb.b = rgb.b * (1.0 - cc_u.temperature * 0.3 + cc_u.tint * 0.15);

    // Contrast: pivots around linear 0.18 (conventional "18% grey" mid-tone),
    // not around 0.5 — 0.5 in LINEAR light is far brighter than perceptual
    // mid-grey, and pivoting there makes "contrast" visibly also change
    // overall exposure, which is the wrong feel for the control.
    let pivot = 0.18;
    rgb = (rgb - pivot) * (1.0 + cc_u.contrast) + pivot;

    // Saturation: lerp toward Rec.709 luma (matches the working space's
    // primaries).
    let luma = dot(rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    rgb = mix(vec3<f32>(luma, luma, luma), rgb, cc_u.saturation);

    return vec4<f32>(rgb, src.a);
}

// -- Crop --

struct CropUniforms {
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
};

@group(0) @binding(0) var<uniform> crop_u: CropUniforms;
@group(0) @binding(1) var t_crop_src: texture_2d<f32>;
@group(0) @binding(2) var s_crop_src: sampler;

@fragment
fn fs_crop(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    if (in.uv.x < crop_u.left || in.uv.x > 1.0 - crop_u.right
        || in.uv.y < crop_u.top || in.uv.y > 1.0 - crop_u.bottom) {
        return vec4<f32>(0.0, 0.0, 0.0, 0.0);
    }
    return textureSample(t_crop_src, s_crop_src, in.uv);
}

// -- Chroma Key --

struct ChromaKeyUniforms {
    key_color: vec4<f32>,
    similarity: f32,
    smoothness: f32,
    spill_suppression: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> ck_u: ChromaKeyUniforms;
@group(0) @binding(1) var t_ck_src: texture_2d<f32>;
@group(0) @binding(2) var s_ck_src: sampler;

// Rec.709 Cb/Cr, luma discarded — the same matrix `render::scopes` uses on
// the CPU for the vectorscope, mirrored here for the same reason `color.rs`'s
// gamut matrices are: a transposed or subtly wrong copy still looks
// plausible, so the two are kept as small, easily-compared constants rather
// than one shared abstraction spanning the CPU/GPU boundary.
fn chroma_uv(rgb: vec3<f32>) -> vec2<f32> {
    let kr = 0.2126;
    let kb = 0.0722;
    let y = kr * rgb.r + (1.0 - kr - kb) * rgb.g + kb * rgb.b;
    let cb = (rgb.b - y) / (2.0 * (1.0 - kb));
    let cr = (rgb.r - y) / (2.0 * (1.0 - kr));
    return vec2<f32>(cb, cr);
}

// Real min/max despill: pulls whichever channel `key` is dominant in down to
// the larger of the other two, and only down — never boosts a channel, never
// touches the other two. That's what keeps this from just desaturating the
// subject wherever they happen to have some green in their own colour.
fn despill(rgb: vec3<f32>, key: vec3<f32>) -> vec3<f32> {
    if (key.g >= key.r && key.g >= key.b) {
        return vec3<f32>(rgb.r, min(rgb.g, max(rgb.r, rgb.b)), rgb.b);
    } else if (key.b >= key.r && key.b >= key.g) {
        return vec3<f32>(rgb.r, rgb.g, min(rgb.b, max(rgb.r, rgb.g)));
    } else {
        return vec3<f32>(min(rgb.r, max(rgb.g, rgb.b)), rgb.g, rgb.b);
    }
}

@fragment
fn fs_chroma_key(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    let src = textureSample(t_ck_src, s_ck_src, in.uv);
    let dist = distance(chroma_uv(src.rgb), chroma_uv(ck_u.key_color.rgb));

    // 0 (fully keyed) inside the similarity radius, ramping to 1 (fully
    // opaque) over the next `smoothness` of distance beyond it.
    let alpha = smoothstep(ck_u.similarity, ck_u.similarity + max(ck_u.smoothness, 1e-5), dist);

    // Despill scales with the suppression amount and `alpha`, so a fully
    // transparent (keyed-out) background isn't pointlessly desaturated —
    // deliberately NOT scaled by proximity to the key hue on top of that.
    // Real spill sits on edge/hair pixels that are a *mix* of foreground and
    // reflected key light, which is usually well outside the tight
    // `similarity` radius used for alpha — gating despill on that radius too
    // would exclude exactly the pixels it exists to fix. `despill` is
    // already self-limiting: it's a no-op wherever the key channel isn't
    // actually elevated above the other two, spilled or not.
    var rgb = src.rgb;
    if (ck_u.spill_suppression > 0.0) {
        rgb = mix(rgb, despill(rgb, ck_u.key_color.rgb), ck_u.spill_suppression * alpha);
    }

    return vec4<f32>(rgb, src.a * alpha);
}

// -- deliver: linear working space -> encoded delivery space --

struct OutputUniforms {
    delivery_code: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> out_u: OutputUniforms;
@group(0) @binding(1) var t_working: texture_2d<f32>;
@group(0) @binding(2) var s_working: sampler;

@fragment
fn fs_deliver(in: FullscreenVertexOut) -> @location(0) vec4<f32> {
    let working = textureSample(t_working, s_working, in.uv);
    let c = clamp(working.rgb, vec3<f32>(0.0), vec3<f32>(1.0));

    var encoded: vec3<f32>;
    if (out_u.delivery_code == DELIVERY_SRGB) {
        encoded = vec3<f32>(linear_to_srgb_1(c.r), linear_to_srgb_1(c.g), linear_to_srgb_1(c.b));
    } else {
        encoded = vec3<f32>(linear_to_rec709_1(c.r), linear_to_rec709_1(c.g), linear_to_rec709_1(c.b));
    }
    return vec4<f32>(encoded, working.a);
}
