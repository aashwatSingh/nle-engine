// Compositing shaders for M4. Two passes:
//
//   1. `vs_clip` / `fs_clip` — draw one clip's source frame into the linear
//      working-space target (Rgba16Float), applying transform + opacity and
//      converting the source out of its encoded colour space into linear
//      light on the way in.
//   2. `vs_fullscreen` / `fs_deliver` — convert the accumulated linear
//      working-space target into the encoded delivery space for output.
//
// The transfer-function and gamut math here MIRRORS crates/render/src/color.rs.
// That duplication is deliberate and load-bearing: the CPU version is the
// tested reference, and the gamut matrices are fed in from it via uniforms
// (rather than hardcoded here) precisely so the two can't silently diverge on
// the thing that's hardest to eyeball — a subtly wrong matrix.

struct ClipUniforms {
    src_size: vec2<f32>,
    seq_size: vec2<f32>,
    position: vec2<f32>,
    scale: vec2<f32>,
    anchor: vec2<f32>,
    rotation: f32,
    opacity: f32,
    // Rows of the source-primaries -> Rec.709 matrix; .w unused.
    gamut0: vec4<f32>,
    gamut1: vec4<f32>,
    gamut2: vec4<f32>,
    transfer_code: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> clip_u: ClipUniforms;
@group(0) @binding(1) var t_source: texture_2d<f32>;
@group(0) @binding(2) var s_source: sampler;

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

@fragment
fn fs_clip(in: ClipVertexOut) -> @location(0) vec4<f32> {
    let src = textureSample(t_source, s_source, in.uv);

    // Encoded -> linear light, then source primaries -> Rec.709 working
    // primaries. No clamping: out-of-gamut negatives are meaningful here and
    // only get clamped at delivery.
    let linear = to_linear(src.rgb, clip_u.transfer_code);
    let working = vec3<f32>(
        dot(clip_u.gamut0.xyz, linear),
        dot(clip_u.gamut1.xyz, linear),
        dot(clip_u.gamut2.xyz, linear),
    );

    return vec4<f32>(working, src.a * clip_u.opacity);
}

// --- delivery pass ---

struct OutputUniforms {
    delivery_code: u32,
    _pad0: u32,
    _pad1: u32,
    _pad2: u32,
};

@group(0) @binding(0) var<uniform> out_u: OutputUniforms;
@group(0) @binding(1) var t_working: texture_2d<f32>;
@group(0) @binding(2) var s_working: sampler;

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
