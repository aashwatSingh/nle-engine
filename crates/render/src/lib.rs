pub mod color;
pub mod compositor;
pub mod effect;
pub mod graph;

/// Re-exported so callers bind against exactly the wgpu version this crate
/// was compiled with. Two different wgpu versions in one binary produce
/// type-mismatch errors that read as if the API changed under you.
pub use wgpu;

pub use compositor::{
    headless_context, Compositor, CompositeStats, RenderedFrame, SourceFrames, SourceTexture,
};

pub use color::{
    apply_matrix, primaries_to_working, to_delivery, to_working, ColorError, DeliverySpace, Matrix3,
    WORKING_SPACE_PRIMARIES, WORKING_SPACE_TRANSFER,
};
pub use effect::{
    transform, BuiltinRegistry, EffectDescriptor, EffectLocality, EffectRegistry, ParamSchema,
    ParamType,
};
pub use graph::{
    ActiveClipPlan, CompiledFrameGraph, EffectPass, FrameSource, GraphCache, GraphCompiler,
    TrackRenderPlan, Transform2D,
};
