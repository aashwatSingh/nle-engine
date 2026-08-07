//! Generic, effect-agnostic parameter + keyframe types. Lives in `timeline`
//! (not `render`) because `ClipInstance` embeds these directly and `timeline`
//! must not depend on `render` — see docs/architecture.md "crate dependency
//! direction". `render` interprets a param's meaning via `effect_type` +
//! param name; it doesn't own the storage.

use crate::time::TimeTick;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    Number(f64),
    Vec2(f64, f64),
    Color([f32; 4]),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InterpolationMode {
    Hold,
    Linear,
    Bezier,
    AutoBezier,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Keyframe {
    pub at: TimeTick,
    pub value: ParamValue,
    pub interpolation: InterpolationMode,
    /// Only meaningful when `interpolation == Bezier`; (in, out) tangent
    /// handles in (time-ticks-delta, value-delta) space.
    pub tangents: Option<((f64, f64), (f64, f64))>,
}

/// A single parameter's value over time. Zero keyframes = constant at
/// `default`; one keyframe = effectively constant at that keyframe's value.
/// Evaluating this at a given tick (interpolation) is M5 work — this is the
/// storage shape only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamTrack {
    pub default: ParamValue,
    pub keyframes: Vec<Keyframe>,
}

impl ParamTrack {
    pub fn constant(value: ParamValue) -> Self {
        ParamTrack { default: value, keyframes: Vec::new() }
    }
}
