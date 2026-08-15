//! Runs an export on a background thread so the editor stays responsive.
//!
//! Export is inherently slow (decode + composite + encode, per frame), and
//! doing it on the UI thread would freeze the window for the whole render —
//! including the progress display meant to show it's working. The thread
//! communicates back through atomics (progress/cancel) and one mutex for the
//! final result, rather than a channel: the UI polls once per frame anyway,
//! so there's nothing to wake up and nothing to drain.

use export::{ExportOptions, ExportStats};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

pub struct ExportJob {
    frames_done: Arc<AtomicU32>,
    total_frames: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    /// `None` while running; `Some` once finished, holding either the stats
    /// or a human-readable failure.
    result: Arc<Mutex<Option<Result<ExportStats, String>>>>,
    pub output: PathBuf,
}

impl ExportJob {
    pub fn start(
        project: Arc<timeline::Project>,
        sequence: timeline::SequenceId,
        asset_paths: std::collections::HashMap<media::MediaAssetId, PathBuf>,
        output: PathBuf,
        options: ExportOptions,
    ) -> Self {
        let frames_done = Arc::new(AtomicU32::new(0));
        let total_frames = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));

        let job = ExportJob {
            frames_done: frames_done.clone(),
            total_frames: total_frames.clone(),
            cancel: cancel.clone(),
            result: result.clone(),
            output: output.clone(),
        };

        std::thread::spawn(move || {
            let outcome = export::export_sequence(
                &project,
                sequence,
                &asset_paths,
                &output,
                &options,
                |frame, total| {
                    frames_done.store(frame, Ordering::Relaxed);
                    total_frames.store(total, Ordering::Relaxed);
                    !cancel.load(Ordering::Relaxed)
                },
            );
            *result.lock().unwrap() = Some(outcome.map_err(|e| format!("{e:?}")));
        });

        job
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Fraction complete in 0.0..=1.0, or `None` before the first frame has
    /// reported a total (the render's own setup — GPU init, opening the
    /// encoder — happens before frame 0).
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total_frames.load(Ordering::Relaxed);
        if total == 0 {
            return None;
        }
        Some(self.frames_done.load(Ordering::Relaxed) as f32 / total as f32)
    }

    pub fn frames_done(&self) -> u32 {
        self.frames_done.load(Ordering::Relaxed)
    }

    pub fn total_frames(&self) -> u32 {
        self.total_frames.load(Ordering::Relaxed)
    }

    /// `Some` once the worker has finished, in which case the caller should
    /// drop this job.
    pub fn take_result(&self) -> Option<Result<ExportStats, String>> {
        self.result.lock().unwrap().take()
    }
}
