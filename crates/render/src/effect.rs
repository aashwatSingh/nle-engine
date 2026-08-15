//! Effect interface, per spec 4.5: an effect declares a parameter schema,
//! whether it's spatially local or global, and a shader entry point. The
//! host (compositor, M4/M5) resolves keyframes from `timeline::ParamTrack`
//! and hands the shader concrete values for the current frame.
//!
//! Shared with `audio` (spec 4.6 effects reuse `ParamSchema`/`ParamType`)
//! rather than duplicated — see docs/architecture.md "crate dependency
//! direction" for why `audio` depends on `render` for just this.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamType {
    Number,
    Vec2,
    Color,
    Bool,
}

#[derive(Debug, Clone)]
pub struct ParamSchema {
    pub name: &'static str,
    pub display_name: &'static str,
    pub param_type: ParamType,
    /// (min, max), meaningful only for `ParamType::Number`.
    pub range: Option<(f64, f64)>,
    pub default: timeline::ParamValue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectLocality {
    /// Pixel-independent (e.g. exposure): can run as a single full-frame pass.
    Global,
    /// Depends on neighboring pixels (e.g. blur, masks with feather): needs
    /// padding/tile awareness in the compositor.
    SpatiallyLocal,
}

/// Static description of one effect type. `type_id` is what
/// `timeline::EffectInstance::effect_type` refers to.
///
/// ABI note (working agreement item 7 / spec 4.5 "third-party plugin API"):
/// this struct and `ParamSchema` are the two types a future plugin ABI would
/// need to cross a dynamic-library boundary. Neither currently derives a
/// stable `#[repr(C)]` layout — that's a deliberate M0 deferral, not an
/// oversight: locking the ABI down now, before any real effect has exercised
/// it, would be guessing. Revisit when v1.0's built-in effect set (spec
/// 4.5's list) is implemented and the hardest case (keyframed spatial mask
/// with feather) has actually been built against this trait.
#[derive(Debug, Clone)]
pub struct EffectDescriptor {
    pub type_id: &'static str,
    pub display_name: &'static str,
    pub locality: EffectLocality,
    pub params: Vec<ParamSchema>,
    /// Name of the shader entry point (e.g. a WGSL function) implementing
    /// this effect. The shader itself is M5 work.
    pub shader_entry_point: &'static str,
}

/// Looks up an `EffectDescriptor` by the `effect_type` string stored on a
/// `timeline::EffectInstance`. Implemented for real once the v1.0 built-in
/// effect set (spec 4.5) exists (M5) — for now this is the seam the render
/// graph compiler (`graph.rs`) will call through.
pub trait EffectRegistry {
    fn lookup(&self, type_id: &str) -> Option<&EffectDescriptor>;
}

/// The Transform effect — the one built-in effect M4 implements, covering
/// spec 4.5's "Transform: position, scale, rotation, anchor point, opacity".
/// The rest of the v1.0 effect set is M5.
///
/// These names are the contract between the graph compiler (which resolves
/// `timeline::ParamTrack`s into concrete values by name) and the compositor
/// (which reads those values back out by name). Both sides reference these
/// constants rather than bare string literals so a rename can't silently
/// half-apply.
pub mod transform {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "transform";

    /// `Vec2`, in pixels, relative to the sequence centre.
    pub const POSITION: &str = "position";
    /// `Vec2`, 1.0 = 100%. Non-uniform scaling is expressible.
    pub const SCALE: &str = "scale";
    /// `Number`, degrees, clockwise.
    pub const ROTATION: &str = "rotation";
    /// `Vec2`, normalised 0..1 within the source frame; the point that
    /// `position` positions and that `scale`/`rotation` pivot around.
    /// (0.5, 0.5) is the frame centre.
    pub const ANCHOR: &str = "anchor";
    /// `Number`, 0.0..1.0.
    pub const OPACITY: &str = "opacity";

    pub fn descriptor() -> EffectDescriptor {
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Transform",
            locality: EffectLocality::Global,
            shader_entry_point: "fs_composite",
            params: vec![
                ParamSchema {
                    name: POSITION,
                    display_name: "Position",
                    param_type: ParamType::Vec2,
                    range: None,
                    default: ParamValue::Vec2(0.0, 0.0),
                },
                ParamSchema {
                    name: SCALE,
                    display_name: "Scale",
                    param_type: ParamType::Vec2,
                    range: None,
                    default: ParamValue::Vec2(1.0, 1.0),
                },
                ParamSchema {
                    name: ROTATION,
                    display_name: "Rotation",
                    param_type: ParamType::Number,
                    range: None,
                    default: ParamValue::Number(0.0),
                },
                ParamSchema {
                    name: ANCHOR,
                    display_name: "Anchor Point",
                    param_type: ParamType::Vec2,
                    range: None,
                    default: ParamValue::Vec2(0.5, 0.5),
                },
                ParamSchema {
                    name: OPACITY,
                    display_name: "Opacity",
                    param_type: ParamType::Number,
                    range: Some((0.0, 1.0)),
                    default: ParamValue::Number(1.0),
                },
            ],
        }
    }
}

/// Gaussian Blur — spec 4.5's spatially-local blur, implemented as a
/// separable two-pass (horizontal then vertical) kernel in `compositor.rs`.
/// `EffectLocality::SpatiallyLocal` here is load-bearing, not decorative: it's
/// what a future tiled/partial-redraw compositor would use to know this
/// effect needs pixel padding around a clip's visible region, unlike a
/// per-pixel effect such as Color Correction.
pub mod gaussian_blur {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "gaussian_blur";
    /// `Number`, in source pixels. 0 = no blur.
    pub const RADIUS: &str = "radius";

    pub fn descriptor() -> EffectDescriptor {
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Gaussian Blur",
            locality: EffectLocality::SpatiallyLocal,
            shader_entry_point: "fs_gaussian_blur",
            params: vec![ParamSchema {
                name: RADIUS,
                display_name: "Blur Radius",
                param_type: ParamType::Number,
                range: Some((0.0, 100.0)),
                default: ParamValue::Number(0.0),
            }],
        }
    }
}

/// Color Correction — spec 4.5's "Lumetri-class color", scoped to the
/// sub-list that's meaningfully implementable as one effect without a
/// dedicated curves UI: exposure, contrast, saturation, temperature, tint.
/// Real NLEs also model this as one effect with many params rather than as
/// separate effect instances, so this mirrors that rather than inventing a
/// different shape. Curves, vibrance, highlights/shadows/whites/blacks, HSL
/// secondary, and LUT loading are the parts of spec 4.5's list this does NOT
/// cover — see docs/decisions-log.md for the M5 scope note.
pub mod color_correction {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "color_correction";
    /// `Number`, stops (photographic EV). 0 = no change.
    pub const EXPOSURE: &str = "exposure";
    /// `Number`, 0 = no change. Negative reduces, positive increases.
    pub const CONTRAST: &str = "contrast";
    /// `Number`, 1.0 = no change, 0.0 = greyscale.
    pub const SATURATION: &str = "saturation";
    /// `Number`, -1.0 (cooler/blue) .. 1.0 (warmer/orange), 0 = no change.
    pub const TEMPERATURE: &str = "temperature";
    /// `Number`, -1.0 (green) .. 1.0 (magenta), 0 = no change.
    pub const TINT: &str = "tint";

    pub fn descriptor() -> EffectDescriptor {
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Color Correction",
            locality: EffectLocality::Global,
            shader_entry_point: "fs_color_correction",
            params: vec![
                ParamSchema { name: EXPOSURE, display_name: "Exposure", param_type: ParamType::Number, range: Some((-5.0, 5.0)), default: ParamValue::Number(0.0) },
                ParamSchema { name: CONTRAST, display_name: "Contrast", param_type: ParamType::Number, range: Some((-1.0, 1.0)), default: ParamValue::Number(0.0) },
                ParamSchema { name: SATURATION, display_name: "Saturation", param_type: ParamType::Number, range: Some((0.0, 2.0)), default: ParamValue::Number(1.0) },
                ParamSchema { name: TEMPERATURE, display_name: "Temperature", param_type: ParamType::Number, range: Some((-1.0, 1.0)), default: ParamValue::Number(0.0) },
                ParamSchema { name: TINT, display_name: "Tint", param_type: ParamType::Number, range: Some((-1.0, 1.0)), default: ParamValue::Number(0.0) },
            ],
        }
    }
}

/// Crop — spec 4.5's geometry group. Edges are normalised 0..1 fractions of
/// the source frame cut away from each side.
pub mod crop {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "crop";
    pub const LEFT: &str = "left";
    pub const RIGHT: &str = "right";
    pub const TOP: &str = "top";
    pub const BOTTOM: &str = "bottom";

    pub fn descriptor() -> EffectDescriptor {
        let edge = |name: &'static str, display: &'static str| ParamSchema {
            name,
            display_name: display,
            param_type: ParamType::Number,
            range: Some((0.0, 1.0)),
            default: ParamValue::Number(0.0),
        };
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Crop",
            locality: EffectLocality::Global,
            shader_entry_point: "fs_crop",
            params: vec![
                edge(LEFT, "Left"),
                edge(RIGHT, "Right"),
                edge(TOP, "Top"),
                edge(BOTTOM, "Bottom"),
            ],
        }
    }
}

/// Mask — spec 4.5's masking system, scoped to rectangle/ellipse with
/// feather. Applied clip-level (gates the whole clip's contribution) rather
/// than per-sub-effect: the common real case ("only show this clip in this
/// region") needs that, and per-effect masking is a bigger data-model change
/// (which effect in the stack does the mask apply *before*?) that's a real
/// follow-up rather than a guess. Pen-tool bezier masks are NOT covered —
/// see docs/decisions-log.md.
pub mod mask {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "mask";
    /// `Bool`. `false` = ellipse, `true` = rectangle. A proper enum param
    /// type is more honest than this, but `ParamType` (spec-defined, M0)
    /// only has Number/Vec2/Color/Bool — extending it is a bigger, separate
    /// decision than this one effect needs to force.
    pub const IS_RECTANGLE: &str = "is_rectangle";
    /// `Vec2`, normalised 0..1, the shape's centre.
    pub const CENTER: &str = "center";
    /// `Vec2`, normalised 0..1, half-width/half-height (rectangle) or
    /// radii (ellipse).
    pub const SIZE: &str = "size";
    /// `Number`, normalised 0..1 (fraction of frame diagonal), the softness
    /// of the mask edge.
    pub const FEATHER: &str = "feather";
    /// `Bool`. When true, the mask is inverted (shows outside the shape).
    pub const INVERT: &str = "invert";

    pub fn descriptor() -> EffectDescriptor {
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Mask",
            locality: EffectLocality::SpatiallyLocal,
            shader_entry_point: "fs_mask",
            params: vec![
                ParamSchema { name: IS_RECTANGLE, display_name: "Rectangle", param_type: ParamType::Bool, range: None, default: ParamValue::Bool(false) },
                ParamSchema { name: CENTER, display_name: "Center", param_type: ParamType::Vec2, range: None, default: ParamValue::Vec2(0.5, 0.5) },
                ParamSchema { name: SIZE, display_name: "Size", param_type: ParamType::Vec2, range: None, default: ParamValue::Vec2(0.25, 0.25) },
                ParamSchema { name: FEATHER, display_name: "Feather", param_type: ParamType::Number, range: Some((0.0, 1.0)), default: ParamValue::Number(0.0) },
                ParamSchema { name: INVERT, display_name: "Invert", param_type: ParamType::Bool, range: None, default: ParamValue::Bool(false) },
            ],
        }
    }
}

/// Chroma Key — green/blue-screen keying. Distance from `key_color` is
/// measured in chroma (colour with luma discarded, the Rec.709 Cb/Cr the rest
/// of `render` already uses for its scopes), so shadows and highlights on the
/// screen still key out evenly rather than only the one exposure level that
/// exactly matches `key_color`. `similarity` is the chroma-distance radius
/// counted as "key"; `smoothness` is the width of the ramp beyond it, so the
/// edge anti-aliases instead of hard-cutting.
///
/// `spill_suppression` is real min/max despill — on a kept (non-transparent)
/// pixel, whichever channel `key_color` is dominant in gets pulled down to
/// the larger of the other two, never boosted, never touching them — not a
/// full desaturate, which would dull the subject's own real colour wherever
/// it happens to be greenish. This is the same technique most simple
/// real-time keyers use; it is not a full per-channel matte-based despill
/// (Ultra Key-class tools), which is real follow-up work if simple
/// min/max despill isn't clean enough on a given shoot.
pub mod chroma_key {
    use super::{EffectDescriptor, EffectLocality, ParamSchema, ParamType};
    use timeline::ParamValue;

    pub const TYPE_ID: &str = "chroma_key";
    /// `Color`. The screen colour to key out — green `[0,1,0,1]` or blue
    /// `[0,0,1,1]` for the ordinary cases, but any colour works.
    pub const KEY_COLOR: &str = "key_color";
    /// `Number`, 0..1. Chroma-distance radius counted as fully keyed.
    pub const SIMILARITY: &str = "similarity";
    /// `Number`, 0..1. Width of the transition ramp beyond `similarity`.
    pub const SMOOTHNESS: &str = "smoothness";
    /// `Number`, 0..1. How much min/max despill to apply to kept pixels.
    pub const SPILL_SUPPRESSION: &str = "spill_suppression";

    pub fn descriptor() -> EffectDescriptor {
        EffectDescriptor {
            type_id: TYPE_ID,
            display_name: "Chroma Key",
            locality: EffectLocality::Global,
            shader_entry_point: "fs_chroma_key",
            params: vec![
                ParamSchema {
                    name: KEY_COLOR,
                    display_name: "Key Colour",
                    param_type: ParamType::Color,
                    range: None,
                    default: ParamValue::Color([0.0, 1.0, 0.0, 1.0]),
                },
                ParamSchema {
                    name: SIMILARITY,
                    display_name: "Similarity",
                    param_type: ParamType::Number,
                    range: Some((0.0, 1.0)),
                    default: ParamValue::Number(0.2),
                },
                ParamSchema {
                    name: SMOOTHNESS,
                    display_name: "Smoothness",
                    param_type: ParamType::Number,
                    range: Some((0.0, 1.0)),
                    default: ParamValue::Number(0.1),
                },
                ParamSchema {
                    name: SPILL_SUPPRESSION,
                    display_name: "Spill Suppression",
                    param_type: ParamType::Number,
                    range: Some((0.0, 1.0)),
                    default: ParamValue::Number(0.5),
                },
            ],
        }
    }
}

/// The effect registry M5 ships: Transform (M4) plus Gaussian Blur, Color
/// Correction, Crop, and Mask. The rest of spec 4.5's built-in list —
/// curves, HSL secondary, LUT loading, mirror, sharpen, transitions,
/// text/titling — is documented as out of scope in docs/decisions-log.md,
/// not silently missing.
pub struct BuiltinRegistry {
    descriptors: Vec<EffectDescriptor>,
}

impl Default for BuiltinRegistry {
    fn default() -> Self {
        BuiltinRegistry {
            descriptors: vec![
                transform::descriptor(),
                gaussian_blur::descriptor(),
                color_correction::descriptor(),
                crop::descriptor(),
                mask::descriptor(),
                chroma_key::descriptor(),
            ],
        }
    }
}

impl EffectRegistry for BuiltinRegistry {
    fn lookup(&self, type_id: &str) -> Option<&EffectDescriptor> {
        self.descriptors.iter().find(|d| d.type_id == type_id)
    }
}

impl BuiltinRegistry {
    /// Every built-in effect's `type_id`. Exists so a UI's own "which effects
    /// can I offer" list can be checked against this one without hand-copying
    /// the id list a second time — see `app::effects_panel`'s test.
    pub fn all_type_ids(&self) -> Vec<&'static str> {
        self.descriptors.iter().map(|d| d.type_id).collect()
    }
}
