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
