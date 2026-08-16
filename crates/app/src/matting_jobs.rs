//! Background AI background-removal generation — the same shape as
//! `proxy_jobs.rs`, deliberately: a matte video is produced and cached
//! exactly the way a proxy is (a real derived video file, substituted in
//! for the original at the same resolution points), just built from a
//! local ML model's output instead of a plain re-encode. See that file's
//! module doc for the reasoning behind why substitution beats new
//! render-pipeline plumbing.
//!
//! **Scope, stated plainly**: matting is keyed by asset, the same as
//! proxies — if the same source file is placed on the timeline twice, both
//! clips get the background removed together, not independently. A true
//! per-clip-instance toggle needs the render graph to carry a per-clip
//! source override, which doesn't exist yet; this ships the useful case
//! (a talking-head clip used once) rather than waiting on that.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

struct Done {
    asset: media::MediaAssetId,
    result: Result<PathBuf, String>,
}

pub struct MattingJobs {
    ready: HashMap<media::MediaAssetId, PathBuf>,
    in_flight: std::collections::HashSet<media::MediaAssetId>,
    failed: std::collections::HashMap<media::MediaAssetId, String>,
    dir: Option<PathBuf>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
}

impl Default for MattingJobs {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        MattingJobs { ready: HashMap::new(), in_flight: std::collections::HashSet::new(), failed: HashMap::new(), dir: None, tx, rx }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MattingState {
    None,
    Building,
    Ready,
    Failed,
}

impl MattingJobs {
    /// Collects finished jobs. Must be called once per frame, same
    /// requirement as `ProxyJobs::poll` and for the same reason.
    pub fn poll(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            self.in_flight.remove(&done.asset);
            match done.result {
                Ok(path) => {
                    self.ready.insert(done.asset, path);
                }
                Err(e) => {
                    self.failed.insert(done.asset, e);
                }
            }
        }
    }

    pub fn state_of(&self, asset: media::MediaAssetId) -> MattingState {
        if self.ready.contains_key(&asset) {
            MattingState::Ready
        } else if self.in_flight.contains(&asset) {
            MattingState::Building
        } else if self.failed.contains_key(&asset) {
            MattingState::Failed
        } else {
            MattingState::None
        }
    }

    pub fn error_for(&self, asset: media::MediaAssetId) -> Option<&str> {
        self.failed.get(&asset).map(|s| s.as_str())
    }

    /// Starts background removal for `asset` unless one exists, is
    /// running, or previously failed. Returns whether a job was started.
    pub fn request(&mut self, asset: media::MediaAssetId, source: PathBuf) -> bool {
        if self.state_of(asset) != MattingState::None {
            return false;
        }
        let Some(dir) = self.ensure_dir() else { return false };
        let output = dir.join(format!("matte_{}.mov", asset.0));
        let tx = self.tx.clone();
        self.in_flight.insert(asset);
        std::thread::spawn(move || {
            let result = (|| {
                let mut session = matting::RvmSession::load(std::path::Path::new(matting::ONNXRUNTIME_MODEL_DEFAULT))
                    .map_err(|e| format!("could not load background-removal model: {e}"))?;
                let options = media_ffmpeg::MatteVideoOptions::default();
                // 0.25 matches RVM's own reference guidance for roughly
                // 1080p-scale source — reasonable for the matte's own
                // capped resolution (see MatteVideoOptions::default).
                const DOWNSAMPLE_RATIO: f32 = 0.25;
                media_ffmpeg::generate_matte_video(
                    &source,
                    &options,
                    |rgb, w, h| match session.infer(rgb, w, h, DOWNSAMPLE_RATIO) {
                        Ok(m) => m.alpha,
                        // The callback can't propagate a Result through
                        // FFmpeg's decode loop; a failed frame keys out as
                        // fully opaque (visually a no-op for that frame)
                        // rather than aborting the whole clip over one bad
                        // inference.
                        Err(_) => vec![1.0; w * h],
                    },
                    &output,
                )
                .map_err(|e| format!("background removal failed for {}: {e:?}", source.display()))?;
                Ok(output)
            })();
            let _ = tx.send(Done { asset, result });
        });
        true
    }

    /// `asset_paths` with a ready matte substituted where available.
    /// Applied *after* proxy resolution by the caller (background removal
    /// is a deliberate editorial choice; a proxy is just a performance
    /// convenience, so matting should win where both exist for the same
    /// asset) — this function itself doesn't know about proxies at all,
    /// keeping the two substitution mechanisms independent.
    pub fn resolve(&self, asset_paths: &HashMap<media::MediaAssetId, PathBuf>) -> HashMap<media::MediaAssetId, PathBuf> {
        if self.ready.is_empty() {
            return asset_paths.clone();
        }
        asset_paths.iter().map(|(id, path)| (*id, self.ready.get(id).cloned().unwrap_or_else(|| path.clone()))).collect()
    }

    fn ensure_dir(&mut self) -> Option<PathBuf> {
        if let Some(d) = &self.dir {
            return Some(d.clone());
        }
        let dir = std::env::temp_dir().join(format!("nle-mattes-{}", std::process::id()));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                self.dir = Some(dir.clone());
                Some(dir)
            }
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(id: u128) -> media::MediaAssetId {
        media::MediaAssetId(id)
    }

    #[test]
    fn resolve_is_a_no_op_before_anything_is_ready() {
        let jobs = MattingJobs::default();
        let mut original = HashMap::new();
        original.insert(asset(1), PathBuf::from("/media/a.mp4"));
        assert_eq!(jobs.resolve(&original), original);
    }

    #[test]
    fn resolve_substitutes_only_assets_with_a_ready_matte() {
        let mut jobs = MattingJobs::default();
        jobs.ready.insert(asset(1), PathBuf::from("/mattes/1.mov"));

        let mut original = HashMap::new();
        original.insert(asset(1), PathBuf::from("/media/a.mp4"));
        original.insert(asset(2), PathBuf::from("/media/b.mp4"));

        let resolved = jobs.resolve(&original);
        assert_eq!(resolved[&asset(1)], PathBuf::from("/mattes/1.mov"));
        assert_eq!(resolved[&asset(2)], PathBuf::from("/media/b.mp4"));
    }

    #[test]
    fn a_second_request_for_the_same_asset_does_not_start_another_job() {
        let mut jobs = MattingJobs::default();
        jobs.in_flight.insert(asset(1));
        assert!(!jobs.request(asset(1), PathBuf::from("/media/a.mp4")));
    }

    #[test]
    fn a_failed_asset_is_not_retried_and_keeps_its_reason() {
        let mut jobs = MattingJobs::default();
        jobs.failed.insert(asset(1), "model missing".into());
        assert!(!jobs.request(asset(1), PathBuf::from("/media/a.mp4")));
        assert_eq!(jobs.state_of(asset(1)), MattingState::Failed);
        assert_eq!(jobs.error_for(asset(1)), Some("model missing"));
    }

    #[test]
    fn a_ready_asset_is_not_rebuilt() {
        let mut jobs = MattingJobs::default();
        jobs.ready.insert(asset(1), PathBuf::from("/mattes/1.mov"));
        assert!(!jobs.request(asset(1), PathBuf::from("/media/a.mp4")));
        assert_eq!(jobs.state_of(asset(1)), MattingState::Ready);
    }

    #[test]
    fn state_progresses_through_poll() {
        let mut jobs = MattingJobs::default();
        assert_eq!(jobs.state_of(asset(1)), MattingState::None);
        jobs.in_flight.insert(asset(1));
        assert_eq!(jobs.state_of(asset(1)), MattingState::Building);

        jobs.tx.send(Done { asset: asset(1), result: Ok(PathBuf::from("/mattes/1.mov")) }).unwrap();
        jobs.poll();

        assert_eq!(jobs.state_of(asset(1)), MattingState::Ready);
    }
}
