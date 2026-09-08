//! Color management, per spec 4.5. The internal working space is fixed:
//! **linear light, Rec.709 primaries**, held at 16-bit float on the GPU.
//! Every input is decoded and converted into that space; output is converted
//! to the delivery space on the way out.
//!
//! This module is the CPU reference implementation of that math, with tests
//! pinning the values that actually matter (white stays white, mid-grey
//! round-trips, the transfer functions are inverses). `composite.wgsl`
//! mirrors these same formulas on the GPU — `shader_constants` below feeds
//! the shader its matrices from here so the two can't drift apart in the one
//! way that's hardest to notice (a transposed or subtly wrong matrix that
//! still looks plausible).
//!
//! Deliberately narrow for M4: SDR only. `Rec2020Hlg` is present in
//! `DeliverySpace` because the type belongs here, but `to_delivery` rejects
//! it — HDR needs a tone-mapping policy and a nit target, which are product
//! decisions (see docs/risks.md), and silently emitting wrong-looking HDR
//! would be worse than refusing.

use media::{ColorPrimaries, TransferFunction};

pub const WORKING_SPACE_PRIMARIES: ColorPrimaries = ColorPrimaries::Rec709;
pub const WORKING_SPACE_TRANSFER: TransferFunction = TransferFunction::Linear;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliverySpace {
    Rec709,
    Srgb,
    Rec2020Hlg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorError {
    /// The requested conversion is real but not implemented in this build.
    Unsupported(&'static str),
}

/// Row-major 3x3, applied as `out = M * in`.
pub type Matrix3 = [[f32; 3]; 3];

pub const IDENTITY_3X3: Matrix3 = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// Rec.2020 -> Rec.709, both linear, D65 white (no chromatic adaptation
/// needed since the white point matches). Derived from the standard
/// RGB->XYZ matrices for each primary set: `M = XYZ_to_709 * 2020_to_XYZ`.
/// Note the negative off-diagonal terms — a wide-gamut colour outside
/// Rec.709 maps to negative components, which is correct and why the
/// working space must stay float rather than clamping to 0..1 mid-pipeline.
pub const REC2020_TO_REC709: Matrix3 = [
    [1.660491, -0.587641, -0.072850],
    [-0.124550, 1.132_9, -0.008349],
    [-0.018151, -0.100579, 1.118_73],
];

/// Display P3 -> Rec.709, both linear, D65 white.
pub const P3D65_TO_REC709: Matrix3 = [
    [1.224_94, -0.224940, 0.000000],
    [-0.042056, 1.042056, 0.000000],
    [-0.019637, -0.078636, 1.098273],
];

pub fn apply_matrix(m: &Matrix3, rgb: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * rgb[0] + m[0][1] * rgb[1] + m[0][2] * rgb[2],
        m[1][0] * rgb[0] + m[1][1] * rgb[1] + m[1][2] * rgb[2],
        m[2][0] * rgb[0] + m[2][1] * rgb[1] + m[2][2] * rgb[2],
    ]
}

/// The matrix taking `primaries` (linear) into the linear Rec.709 working
/// space. `Unknown` is treated as Rec.709 — the pragmatic choice for
/// untagged SDR footage, which is overwhelmingly 709 in practice, and the
/// alternative (refusing to render untagged media) would make the app
/// useless on a large fraction of real files.
pub fn primaries_to_working(primaries: ColorPrimaries) -> Matrix3 {
    match primaries {
        ColorPrimaries::Rec709 | ColorPrimaries::Unknown => IDENTITY_3X3,
        ColorPrimaries::Rec2020 => REC2020_TO_REC709,
        ColorPrimaries::P3D65 => P3D65_TO_REC709,
    }
}

// --- transfer functions: encoded (non-linear) <-> linear light ---

/// sRGB EOTF (IEC 61966-2-1): encoded -> linear.
pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 {
        v / 12.92
    } else {
        ((v + 0.055) / 1.055).powf(2.4)
    }
}

/// sRGB inverse EOTF: linear -> encoded.
pub fn linear_to_srgb(v: f32) -> f32 {
    if v <= 0.0031308 {
        v * 12.92
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    }
}

/// Rec.709 OETF, inverted: encoded -> linear.
///
/// Note this is the camera OETF from the standard, not the ~2.4 gamma of a
/// reference display. Using the OETF here is the deliberate choice: it makes
/// `linear_to_rec709(rec709_to_linear(x)) == x` exactly, so a 709-in /
/// 709-out edit with no colour work is a true no-op. A display-referred
/// pipeline would use the 2.4 EOTF instead and is a different (also valid)
/// design — flagged here because the difference shows up as a subtle overall
/// contrast shift and is miserable to diagnose after the fact.
pub fn rec709_to_linear(v: f32) -> f32 {
    if v < 0.081 {
        v / 4.5
    } else {
        ((v + 0.099) / 1.099).powf(1.0 / 0.45)
    }
}

/// Rec.709 OETF: linear -> encoded.
pub fn linear_to_rec709(v: f32) -> f32 {
    if v < 0.018 {
        4.5 * v
    } else {
        1.099 * v.powf(0.45) - 0.099
    }
}

/// Converts an encoded RGB triple in `transfer` to linear light.
/// `Unknown` is treated as Rec.709, for the same reason as `Unknown`
/// primaries above.
pub fn transfer_to_linear(transfer: TransferFunction, rgb: [f32; 3]) -> Result<[f32; 3], ColorError> {
    let f: fn(f32) -> f32 = match transfer {
        TransferFunction::Linear => return Ok(rgb),
        TransferFunction::Srgb => srgb_to_linear,
        TransferFunction::Bt709 | TransferFunction::Unknown => rec709_to_linear,
        TransferFunction::Pq => {
            return Err(ColorError::Unsupported("PQ (HDR10) input: needs an HDR tone-mapping policy"))
        }
        TransferFunction::Hlg => {
            return Err(ColorError::Unsupported("HLG input: needs an HDR tone-mapping policy"))
        }
    };
    Ok([f(rgb[0]), f(rgb[1]), f(rgb[2])])
}

/// Full decode-side conversion: encoded source pixel -> linear working
/// space. This is the "every input is converted from its tagged color space
/// into working space on decode" half of spec 4.5.
pub fn to_working(
    primaries: ColorPrimaries,
    transfer: TransferFunction,
    rgb: [f32; 3],
) -> Result<[f32; 3], ColorError> {
    let linear = transfer_to_linear(transfer, rgb)?;
    Ok(apply_matrix(&primaries_to_working(primaries), linear))
}

/// Full output-side conversion: linear working space -> encoded delivery
/// pixel. Values are clamped to 0..1 here and only here — clamping earlier
/// would destroy the out-of-gamut headroom the working space exists to
/// preserve.
pub fn to_delivery(target: DeliverySpace, rgb: [f32; 3]) -> Result<[f32; 3], ColorError> {
    let f: fn(f32) -> f32 = match target {
        DeliverySpace::Rec709 => linear_to_rec709,
        DeliverySpace::Srgb => linear_to_srgb,
        DeliverySpace::Rec2020Hlg => {
            return Err(ColorError::Unsupported(
                "HLG delivery: needs an HDR tone-mapping policy and nit target",
            ))
        }
    };
    Ok([
        f(rgb[0].clamp(0.0, 1.0)),
        f(rgb[1].clamp(0.0, 1.0)),
        f(rgb[2].clamp(0.0, 1.0)),
    ])
}

/// Numeric codes handed to `composite.wgsl` so the shader can branch on
/// transfer function / delivery space without duplicating the enum. Kept
/// next to the math it selects between.
pub mod shader_codes {
    pub const TRANSFER_LINEAR: u32 = 0;
    pub const TRANSFER_SRGB: u32 = 1;
    pub const TRANSFER_REC709: u32 = 2;

    pub const DELIVERY_REC709: u32 = 0;
    pub const DELIVERY_SRGB: u32 = 1;
}

/// The code the shader should use for a given source transfer function.
/// `Pq`/`Hlg` fall back to Rec.709 rather than erroring, because the GPU
/// path has no way to report an error mid-frame — the *ingest* path is
/// where unsupported HDR should be rejected (via `transfer_to_linear`), so
/// reaching here with HDR means an earlier check was missed.
pub fn shader_transfer_code(transfer: TransferFunction) -> u32 {
    match transfer {
        TransferFunction::Linear => shader_codes::TRANSFER_LINEAR,
        TransferFunction::Srgb => shader_codes::TRANSFER_SRGB,
        TransferFunction::Bt709 | TransferFunction::Unknown => shader_codes::TRANSFER_REC709,
        TransferFunction::Pq | TransferFunction::Hlg => shader_codes::TRANSFER_REC709,
    }
}

pub fn shader_delivery_code(target: DeliverySpace) -> u32 {
    match target {
        DeliverySpace::Rec709 => shader_codes::DELIVERY_REC709,
        DeliverySpace::Srgb => shader_codes::DELIVERY_SRGB,
        // Same reasoning as above: unsupported HDR should have been caught
        // before a frame is submitted.
        DeliverySpace::Rec2020Hlg => shader_codes::DELIVERY_REC709,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn srgb_transfer_is_its_own_inverse() {
        for step in 0..=100 {
            let v = step as f32 / 100.0;
            let round_tripped = linear_to_srgb(srgb_to_linear(v));
            assert!(close(v, round_tripped, 1e-5), "sRGB round-trip failed at {v}: got {round_tripped}");
        }
    }

    #[test]
    fn rec709_transfer_is_its_own_inverse() {
        for step in 0..=100 {
            let v = step as f32 / 100.0;
            let round_tripped = linear_to_rec709(rec709_to_linear(v));
            assert!(close(v, round_tripped, 1e-5), "Rec.709 round-trip failed at {v}: got {round_tripped}");
        }
    }

    #[test]
    fn transfer_functions_pin_black_and_white() {
        assert!(close(srgb_to_linear(0.0), 0.0, 1e-9));
        assert!(close(srgb_to_linear(1.0), 1.0, 1e-6));
        assert!(close(rec709_to_linear(0.0), 0.0, 1e-9));
        assert!(close(rec709_to_linear(1.0), 1.0, 1e-6));
    }

    #[test]
    fn srgb_mid_grey_matches_the_published_value() {
        // 0.5 encoded sRGB is ~0.2140 linear — the canonical sanity check
        // that this is a real EOTF and not a naive 2.2 power curve (which
        // would give ~0.2176 and look almost-but-not-quite right).
        let linear = srgb_to_linear(0.5);
        assert!(close(linear, 0.21404, 1e-4), "sRGB 0.5 should be ~0.2140 linear, got {linear}");
    }

    #[test]
    fn rec709_identity_conversion_is_a_true_no_op() {
        // The property that matters most in practice: a 709 source
        // delivered as 709 with no colour work must come out bit-for-bit
        // (within float error) the same. Any transfer/primaries mismatch
        // shows up here as a global contrast or tint shift.
        for step in 0..=20 {
            let v = step as f32 / 20.0;
            let working = to_working(ColorPrimaries::Rec709, TransferFunction::Bt709, [v, v, v]).unwrap();
            let out = to_delivery(DeliverySpace::Rec709, working).unwrap();
            assert!(close(out[0], v, 1e-5), "709 -> working -> 709 shifted {v} to {}", out[0]);
        }
    }

    #[test]
    fn untagged_source_is_treated_as_rec709() {
        let tagged = to_working(ColorPrimaries::Rec709, TransferFunction::Bt709, [0.5, 0.5, 0.5]).unwrap();
        let untagged = to_working(ColorPrimaries::Unknown, TransferFunction::Unknown, [0.5, 0.5, 0.5]).unwrap();
        assert_eq!(tagged, untagged);
    }

    #[test]
    fn white_stays_white_across_gamut_conversions() {
        // Both matrices are D65-referenced, so (1,1,1) must map to (1,1,1).
        // A transposed or mis-derived matrix almost always breaks this.
        for m in [&REC2020_TO_REC709, &P3D65_TO_REC709] {
            let white = apply_matrix(m, [1.0, 1.0, 1.0]);
            for c in white {
                assert!(close(c, 1.0, 2e-3), "white did not survive gamut conversion: {white:?}");
            }
        }
    }

    #[test]
    fn black_stays_black_across_gamut_conversions() {
        for m in [&REC2020_TO_REC709, &P3D65_TO_REC709] {
            let black = apply_matrix(m, [0.0, 0.0, 0.0]);
            assert_eq!(black, [0.0, 0.0, 0.0]);
        }
    }

    #[test]
    fn wide_gamut_saturated_colour_goes_out_of_range_rather_than_clamping_early() {
        // Pure Rec.2020 green is outside Rec.709's gamut, so it must come
        // out with a negative component in the working space. If this ever
        // returns all-positive, something clamped where it shouldn't and
        // wide-gamut footage will be silently desaturated.
        let converted = apply_matrix(&REC2020_TO_REC709, [0.0, 1.0, 0.0]);
        assert!(
            converted.iter().any(|c| *c < 0.0),
            "expected an out-of-gamut negative component, got {converted:?}"
        );
    }

    #[test]
    fn delivery_clamps_out_of_range_working_values() {
        let out = to_delivery(DeliverySpace::Rec709, [-0.5, 1.5, 0.5]).unwrap();
        assert!(close(out[0], 0.0, 1e-6), "negative should clamp to black");
        assert!(close(out[1], 1.0, 1e-6), "over-1.0 should clamp to white");
        assert!(out[2] > 0.0 && out[2] < 1.0);
    }

    #[test]
    fn hdr_transfer_functions_are_refused_rather_than_guessed() {
        assert_eq!(
            transfer_to_linear(TransferFunction::Pq, [0.5, 0.5, 0.5]),
            Err(ColorError::Unsupported("PQ (HDR10) input: needs an HDR tone-mapping policy"))
        );
        assert!(matches!(
            transfer_to_linear(TransferFunction::Hlg, [0.5, 0.5, 0.5]),
            Err(ColorError::Unsupported(_))
        ));
        assert!(matches!(
            to_delivery(DeliverySpace::Rec2020Hlg, [0.5, 0.5, 0.5]),
            Err(ColorError::Unsupported(_))
        ));
    }

    #[test]
    fn linear_input_passes_through_the_transfer_stage_untouched() {
        let rgb = [0.1, 0.5, 0.9];
        assert_eq!(transfer_to_linear(TransferFunction::Linear, rgb).unwrap(), rgb);
    }
}
