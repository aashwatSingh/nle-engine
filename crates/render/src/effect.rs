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

/// The effect registry M4 ships: Transform only. M5 extends this with the
/// rest of spec 4.5's built-in set.
pub struct BuiltinRegistry {
    descriptors: Vec<EffectDescriptor>,
}

impl Default for BuiltinRegistry {
    fn default() -> Self {
        BuiltinRegistry { descriptors: vec![transform::descriptor()] }
    }
}

impl EffectRegistry for BuiltinRegistry {
    fn lookup(&self, type_id: &str) -> Option<&EffectDescriptor> {
        self.descriptors.iter().find(|d| d.type_id == type_id)
    }
}
