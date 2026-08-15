//! Background proxy generation, and the switch that makes playback use them.
//!
//! `media_ffmpeg::generate_proxy` has existed and been tested since M1, but
//! nothing ever called it — so heavy footage had no fallback and the whole
//! feature was dead weight. This is the caller.
//!
//! ## What a proxy buys
//!
//! A proxy is a small, **all-intra** copy: every frame is a keyframe. Two
//! separate wins, and the second is the bigger one:
//!
//! 1. Fewer pixels to decode and upload per frame.
//! 2. No inter-frame dependencies, so seeking to an arbitrary frame doesn't
//!    require decoding forward from the previous keyframe. On long-GOP source
//!    (most camera and delivery codecs) that's the cost that makes scrubbing
//!    feel like mud.
//!
//! ## Where proxies are used, and where they must not be
//!
//! [`ProxyJobs::resolve`] rewrites an `asset_paths` map to point at proxies,
//! and it is called from exactly three places — starting playback, starting the
//! video decode thread, and the preview's scrub path.
//!
//! **`start_export` deliberately does not call it**, and passes
//! `state.asset_paths` straight through. A proxy is a working copy for editing;
//! quietly delivering a 960px all-intra render as the finished product would be
//! the worst possible outcome of this feature. There's no type-level barrier
//! enforcing that — the honest statement is that it's an invariant maintained by
//! those call sites, so anything new that renders for *delivery* rather than for
//! *monitoring* must keep using the original paths.
//!
//! Proxies are session state, not project state: they're a local performance
//! artefact, they live in a temp directory, and a project file that referenced
//! them would break the moment it moved to another machine.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;

/// Longer edge of a generated proxy. 960 keeps 1080p work comfortably real time
/// while staying big enough to frame a shot by eye.
const PROXY_MAX_DIMENSION: u32 = 960;

/// One finished (or failed) proxy job.
struct Done {
    asset: media::MediaAssetId,
    result: Result<PathBuf, String>,
}

pub struct ProxyJobs {
    /// Assets with a usable proxy on disk.
    ready: HashMap<media::MediaAssetId, PathBuf>,
    /// Jobs currently running, so a second request doesn't start a duplicate
    /// transcode of the same file.
    in_flight: std::collections::HashSet<media::MediaAssetId>,
    /// Assets whose proxy failed. Kept so a broken file isn't retried forever.
    failed: std::collections::HashSet<media::MediaAssetId>,
    /// Whether playback should prefer proxies. Off by default: generating one
    /// costs real time, so using proxies is a choice the user makes.
    pub enabled: bool,
    /// Directory the proxies live in, created lazily on first job.
    dir: Option<PathBuf>,
    tx: mpsc::Sender<Done>,
    rx: mpsc::Receiver<Done>,
    pub last_error: Option<String>,
}

impl Default for ProxyJobs {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        ProxyJobs {
            ready: HashMap::new(),
            in_flight: std::collections::HashSet::new(),
            failed: std::collections::HashSet::new(),
            enabled: false,
            dir: None,
            tx,
            rx,
            last_error: None,
        }
    }
}

impl ProxyJobs {
    /// Collects finished jobs. Must be called once per frame — without it
    /// results sit on the channel and no proxy ever becomes usable.
    pub fn poll(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            self.in_flight.remove(&done.asset);
            match done.result {
                Ok(path) => {
                    self.ready.insert(done.asset, path);
                }
                Err(e) => {
                    self.failed.insert(done.asset);
                    self.last_error = Some(e);
                }
            }
        }
    }

    pub fn is_busy(&self) -> bool {
        !self.in_flight.is_empty()
    }

    pub fn in_flight_count(&self) -> usize {
        self.in_flight.len()
    }

    pub fn ready_count(&self) -> usize {
        self.ready.len()
    }

    pub fn state_of(&self, asset: media::MediaAssetId) -> ProxyState {
        if self.ready.contains_key(&asset) {
            ProxyState::Ready
        } else if self.in_flight.contains(&asset) {
            ProxyState::Building
        } else if self.failed.contains(&asset) {
            ProxyState::Failed
        } else {
            ProxyState::None
        }
    }

    /// Starts a proxy for `asset` unless one exists, is running, or previously
    /// failed. Returns whether a job was started.
    pub fn request(&mut self, asset: media::MediaAssetId, source: PathBuf) -> bool {
        if self.state_of(asset) != ProxyState::None {
            return false;
        }
        let dir = match self.ensure_dir() {
            Some(d) => d,
            None => return false,
        };
        // Named by asset id: stable within a session, and two assets from
        // identically-named files in different folders can't collide.
        let output = dir.join(format!("proxy_{}.mp4", asset.0));
        let tx = self.tx.clone();
        self.in_flight.insert(asset);
        std::thread::spawn(move || {
            let options = media_ffmpeg::ProxyOptions { max_dimension: PROXY_MAX_DIMENSION };
            let result = match media_ffmpeg::generate_proxy(&source, &options, &output) {
                Ok(()) => Ok(output),
                Err(e) => Err(format!("proxy failed for {}: {e:?}", source.display())),
            };
            let _ = tx.send(Done { asset, result });
        });
        true
    }

    /// `asset_paths` with proxies substituted where available, for playback and
    /// preview. Returns the input unchanged when proxying is off.
    ///
    /// Assets without a proxy keep their original path, so enabling proxies
    /// mid-session degrades gracefully to "some clips are fast" rather than
    /// breaking the ones not yet built.
    pub fn resolve(
        &self,
        asset_paths: &HashMap<media::MediaAssetId, PathBuf>,
    ) -> HashMap<media::MediaAssetId, PathBuf> {
        if !self.enabled || self.ready.is_empty() {
            return asset_paths.clone();
        }
        asset_paths
            .iter()
            .map(|(id, path)| {
                (*id, self.ready.get(id).cloned().unwrap_or_else(|| path.clone()))
            })
            .collect()
    }

    fn ensure_dir(&mut self) -> Option<PathBuf> {
        if let Some(d) = &self.dir {
            return Some(d.clone());
        }
        // Under the OS temp dir, with the process id so two editors running at
        // once don't overwrite each other's proxies.
        let dir = std::env::temp_dir().join(format!("nle-proxies-{}", std::process::id()));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => {
                self.dir = Some(dir.clone());
                Some(dir)
            }
            Err(e) => {
                self.last_error = Some(format!("could not create proxy directory: {e}"));
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyState {
    None,
    Building,
    Ready,
    Failed,
}

impl ProxyState {
    /// Short marker for the Project panel's proxy column.
    pub fn label(self) -> &'static str {
        match self {
            ProxyState::None => "",
            ProxyState::Building => "...",
            ProxyState::Ready => "yes",
            ProxyState::Failed => "err",
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
    fn resolve_is_a_no_op_while_proxying_is_off() {
        // Default-off matters: generating proxies costs minutes of transcoding,
        // so it can't be something the editor starts doing on its own.
        let mut jobs = ProxyJobs::default();
        assert!(!jobs.enabled);
        jobs.ready.insert(asset(1), PathBuf::from("/proxies/1.mp4"));

        let mut original = HashMap::new();
        original.insert(asset(1), PathBuf::from("/media/original.mp4"));
        assert_eq!(jobs.resolve(&original), original);
    }

    #[test]
    fn resolve_substitutes_only_the_assets_that_have_a_proxy() {
        // Enabling proxies mid-session must not break clips whose proxy hasn't
        // finished — they keep playing from the original, just slower.
        let mut jobs = ProxyJobs { enabled: true, ..Default::default() };
        jobs.ready.insert(asset(1), PathBuf::from("/proxies/1.mp4"));

        let mut original = HashMap::new();
        original.insert(asset(1), PathBuf::from("/media/a.mp4"));
        original.insert(asset(2), PathBuf::from("/media/b.mp4"));

        let resolved = jobs.resolve(&original);
        assert_eq!(resolved[&asset(1)], PathBuf::from("/proxies/1.mp4"));
        assert_eq!(
            resolved[&asset(2)],
            PathBuf::from("/media/b.mp4"),
            "an asset with no proxy must keep its original path"
        );
    }

    #[test]
    fn a_second_request_for_the_same_asset_does_not_start_another_transcode() {
        let mut jobs = ProxyJobs::default();
        jobs.in_flight.insert(asset(1));
        assert!(
            !jobs.request(asset(1), PathBuf::from("/media/a.mp4")),
            "duplicate request should be refused, not queued as a second transcode"
        );
    }

    #[test]
    fn a_failed_asset_is_not_retried() {
        // Otherwise an unreadable file spawns a fresh doomed transcode every
        // time the panel is drawn.
        let mut jobs = ProxyJobs::default();
        jobs.failed.insert(asset(1));
        assert!(!jobs.request(asset(1), PathBuf::from("/media/broken.mp4")));
        assert_eq!(jobs.state_of(asset(1)), ProxyState::Failed);
    }

    #[test]
    fn a_ready_asset_is_not_rebuilt() {
        let mut jobs = ProxyJobs::default();
        jobs.ready.insert(asset(1), PathBuf::from("/proxies/1.mp4"));
        assert!(!jobs.request(asset(1), PathBuf::from("/media/a.mp4")));
        assert_eq!(jobs.state_of(asset(1)), ProxyState::Ready);
    }

    #[test]
    fn state_progresses_and_a_failure_is_recorded_with_its_reason() {
        let mut jobs = ProxyJobs::default();
        assert_eq!(jobs.state_of(asset(1)), ProxyState::None);
        jobs.in_flight.insert(asset(1));
        assert_eq!(jobs.state_of(asset(1)), ProxyState::Building);

        jobs.tx
            .send(Done { asset: asset(1), result: Err("bad file".into()) })
            .unwrap();
        jobs.poll();

        assert_eq!(jobs.state_of(asset(1)), ProxyState::Failed);
        assert!(!jobs.is_busy(), "a finished job must leave the in-flight set");
        assert_eq!(jobs.last_error.as_deref(), Some("bad file"));
    }
}
