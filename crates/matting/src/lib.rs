//! AI background removal via Robust Video Matting (RVM), run locally
//! through ONNX Runtime — the local-inference counterpart to `speech`'s
//! whisper.cpp subprocess, chosen for the same reason: this project keeps
//! everything it can running on the user's own machine rather than a
//! cloud API.
//!
//! RVM is a *recurrent* model: each frame's inference also depends on a
//! hidden state produced by the previous frame (`r1..r4` below), which is
//! exactly what gives it temporal consistency a single-frame segmentation
//! model can't have — without it, the matte would flicker frame to frame
//! instead of tracking the subject smoothly. That means frames must be run
//! through in source order, threading the state forward; it is not safe to
//! infer frames out of order or in parallel.

use ort::session::Session;
use ort::value::Tensor;

pub struct RvmSession {
    session: Session,
    state: RecurrentState,
}

/// The four hidden-state tensors RVM threads between frames. `None`
/// (all-zero, shape `[1,1,1,1]`) is the correct starting state for the
/// first frame of a clip — RVM's own reference inference loop starts the
/// same way.
struct RecurrentState {
    r1: Tensor<f32>,
    r2: Tensor<f32>,
    r3: Tensor<f32>,
    r4: Tensor<f32>,
}

impl RecurrentState {
    fn zeroed() -> ort::Result<Self> {
        Ok(RecurrentState {
            r1: Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?,
            r2: Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?,
            r3: Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?,
            r4: Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?,
        })
    }
}

/// One frame's matte: `alpha` is a single-channel 0..1 mask, row-major,
/// `width * height` long.
pub struct Matte {
    pub width: usize,
    pub height: usize,
    pub alpha: Vec<f32>,
}

/// Where Microsoft's ONNX Runtime release was unpacked, under
/// `integrity::tools_dir` — same trade this project already made for FFmpeg
/// and Whisper: a fixed place, not searched for, since a search path would
/// fail silently on a different machine anyway. `ort`'s own prebuilt-binary
/// fetch has no x86_64-pc-windows-gnu build, so this points `ort` at
/// Microsoft's official release DLL instead (see this crate's Cargo.toml).
fn onnxruntime_lib_dir() -> std::path::PathBuf {
    integrity::tools_dir().join(r"onnxruntime\extracted\onnxruntime-win-x64-1.29.0\lib")
}

/// Where the downloaded RVM model lives — same trade as `onnxruntime_lib_dir`.
fn default_model_path() -> std::path::PathBuf {
    integrity::tools_dir().join(r"rvm-models\rvm_mobilenetv3_fp32.onnx")
}

// SHA-256 pins for everything above, as vetted — see the `integrity` crate for
// why, and docs/security.md for how to re-pin after a deliberate upgrade.
// The two DLLs come from Microsoft's official onnxruntime-win-x64-1.29.0.zip.
const ONNXRUNTIME_DLL_SHA256: &str = "69d8e6d3879a3b4001cdc74c8ed9ccc7e7f799a5b847059738323404519ec471";

/// `onnxruntime_providers_shared.dll` ships beside `onnxruntime.dll` in the
/// same release, and the runtime can load it from that directory, so it's
/// pinned along with the runtime.
const ONNXRUNTIME_PROVIDERS_SHARED_SHA256: &str = "87f6878cdc1f80b3a9afa5b0c84663315030b4957f5bbb6b66470557cb2f48d8";

pub const DEFAULT_MODEL_SHA256: &str = "88d4531297118f595bf2fd60f6f566aec2e559393802d1f436c380f0cbbd2828";

/// The RVM model this app uses, with its pin.
pub fn default_model() -> integrity::Pin {
    integrity::Pin::new(default_model_path(), DEFAULT_MODEL_SHA256)
}

/// Why `RvmSession::load` failed.
#[derive(Debug)]
pub enum LoadError {
    /// The model or a runtime DLL isn't the vetted file; nothing was loaded.
    Integrity(integrity::IntegrityError),
    /// ONNX Runtime refused to start or to load the model.
    Runtime(String),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Integrity(e) => write!(f, "{e}"),
            LoadError::Runtime(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<ort::Error> for LoadError {
    fn from(e: ort::Error) -> Self {
        LoadError::Runtime(e.to_string())
    }
}

/// Copies an extracted `(shape, data)` tensor view into a fresh owned
/// `Tensor`, for carrying a recurrent-state output forward as next frame's
/// input — the view returned by `try_extract_tensor` borrows from the
/// outputs it came from, so it can't outlive them as-is. A macro rather
/// than a generic function: the exact borrowed shape type is an
/// implementation detail of `ort` not worth naming explicitly here.
macro_rules! owned_tensor {
    ($shape:expr, $data:expr) => {{
        let dims: Vec<usize> = $shape.iter().map(|&d| d as usize).collect();
        Tensor::from_array((dims, $data.to_vec()))
    }};
}

impl RvmSession {
    pub fn load(model: &integrity::Pin) -> Result<Self, LoadError> {
        // Checked before anything is loaded or parsed, and held until the
        // session exists, so neither the runtime nor the model can be swapped
        // between the check and its use. The model is checked first, which is
        // also why the tampered-model test doesn't need the runtime installed.
        let lib_dir = onnxruntime_lib_dir();
        let runtime = lib_dir.join("onnxruntime.dll");
        let _verified = integrity::verify_all(&[
            model.clone(),
            integrity::Pin::new(&runtime, ONNXRUNTIME_DLL_SHA256),
            integrity::Pin::new(lib_dir.join("onnxruntime_providers_shared.dll"), ONNXRUNTIME_PROVIDERS_SHARED_SHA256),
        ])
        .map_err(LoadError::Integrity)?;
        // Stringified rather than converted into `ort::Error`: building one
        // initialises `ort`'s API, and before `init_from` has succeeded that
        // means `LoadLibrary("onnxruntime.dll")` by bare name — which found an
        // unrelated 1.17.1 copy elsewhere on this machine's search path and
        // panicked on the version mismatch.
        ort::init_from(&runtime).map_err(|e| LoadError::Runtime(e.to_string()))?.commit();
        let session = Session::builder()?.commit_from_file(&model.path)?;
        let state = RecurrentState::zeroed()?;
        Ok(RvmSession { session, state })
    }

    /// Resets the recurrent state to zero — call this whenever inference is
    /// about to start a new clip (or restart mid-clip after a seek), since
    /// carrying state across a discontinuity would blend the matte across a
    /// cut that has nothing to do with the previous frame.
    pub fn reset(&mut self) -> ort::Result<()> {
        self.state = RecurrentState::zeroed()?;
        Ok(())
    }

    /// Runs one frame of inference. `rgb` is interleaved `u8` RGB,
    /// `width * height * 3` long (no alpha — RVM's `src` input is RGB
    /// only). `downsample_ratio` trades matte precision for speed by
    /// running RVM's internal recurrent decoder at a fraction of the input
    /// resolution — RVM's own guidance is to target roughly 256-512px on
    /// the short side, so a full 1080p frame wants a small ratio (around
    /// 0.25) while a already-small frame wants something close to 1.0.
    pub fn infer(&mut self, rgb: &[u8], width: usize, height: usize, downsample_ratio: f32) -> ort::Result<Matte> {
        assert_eq!(rgb.len(), width * height * 3, "rgb buffer doesn't match width*height*3");

        // NCHW, normalized 0..1 — RVM's documented `src` input layout.
        let mut src = vec![0.0f32; 3 * width * height];
        let plane = width * height;
        for (i, chunk) in rgb.chunks_exact(3).enumerate() {
            src[i] = chunk[0] as f32 / 255.0;
            src[plane + i] = chunk[1] as f32 / 255.0;
            src[2 * plane + i] = chunk[2] as f32 / 255.0;
        }
        let src_tensor = Tensor::from_array(([1usize, 3, height, width], src))?;
        let ratio_tensor = Tensor::from_array(([1usize], vec![downsample_ratio]))?;

        let outputs = self.session.run(ort::inputs![
            "src" => src_tensor,
            "r1i" => std::mem::replace(&mut self.state.r1, Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?),
            "r2i" => std::mem::replace(&mut self.state.r2, Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?),
            "r3i" => std::mem::replace(&mut self.state.r3, Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?),
            "r4i" => std::mem::replace(&mut self.state.r4, Tensor::from_array(([1usize, 1, 1, 1], vec![0.0f32]))?),
            "downsample_ratio" => ratio_tensor,
        ])?;

        let (pha_shape, pha_data) = outputs["pha"].try_extract_tensor::<f32>()?;
        let (h, w) = (pha_shape[2] as usize, pha_shape[3] as usize);
        let alpha = pha_data.to_vec();

        // The extracted view borrows from `outputs`, so it can't be carried
        // forward as-is — copied into a fresh owned tensor for next frame's
        // r1i..r4i instead.
        let (r1s, r1d) = outputs["r1o"].try_extract_tensor::<f32>()?;
        self.state.r1 = owned_tensor!(r1s, r1d)?;
        let (r2s, r2d) = outputs["r2o"].try_extract_tensor::<f32>()?;
        self.state.r2 = owned_tensor!(r2s, r2d)?;
        let (r3s, r3d) = outputs["r3o"].try_extract_tensor::<f32>()?;
        self.state.r3 = owned_tensor!(r3s, r3d)?;
        let (r4s, r4d) = outputs["r4o"].try_extract_tensor::<f32>()?;
        self.state.r4 = owned_tensor!(r4s, r4d)?;

        Ok(Matte { width: w, height: h, alpha })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A model file that isn't the vetted one must be refused before ONNX
    /// Runtime parses it. Not `#[ignore]`d: the model is checked first, so
    /// this doesn't need the runtime installed to reach its assertion.
    #[test]
    fn a_model_that_fails_its_pin_is_refused_before_it_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("swapped-rvm.onnx");
        std::fs::write(&model, b"not the vetted model").unwrap();

        let err = RvmSession::load(&integrity::Pin::new(&model, DEFAULT_MODEL_SHA256))
            .err()
            .expect("a model that doesn't match its pin must be refused");

        let message = err.to_string();
        assert!(
            message.contains("integrity check") && message.contains("swapped-rvm.onnx"),
            "should say which file failed and why: {message}"
        );
    }

    /// Spike test: loads the real model and runs two frames of inference
    /// (enough to exercise the recurrent state hand-off, not just a single
    /// cold call) on synthetic input, to de-risk the ONNX Runtime
    /// integration itself before building the caching job and effect on
    /// top of it. `#[ignore]`d since it needs the real model file on disk.
    #[test]
    #[ignore]
    fn loads_the_real_model_and_infers_two_frames() {
        let model = default_model();
        assert!(model.path.is_file(), "model not found at {:?} — see docs/decisions-log.md's matting entry", model.path);

        let mut session = RvmSession::load(&model).expect("failed to load RVM model");
        let (w, h) = (64, 48);
        let frame = vec![128u8; w * h * 3];

        let m1 = session.infer(&frame, w, h, 0.5).expect("first inference failed");
        assert_eq!(m1.width, w);
        assert_eq!(m1.height, h);
        assert_eq!(m1.alpha.len(), w * h);
        assert!(m1.alpha.iter().all(|&a| (0.0..=1.0).contains(&a)), "alpha must be a 0..1 mask");

        // Second frame proves the recurrent state round-trips correctly —
        // a bug in the r1..r4 hand-off would panic or produce garbage here,
        // not on the first (all-zero-state) frame.
        let m2 = session.infer(&frame, w, h, 0.5).expect("second inference failed");
        assert_eq!(m2.alpha.len(), w * h);
    }
}
