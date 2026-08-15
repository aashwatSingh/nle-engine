//! Background waveform (peak envelope) generation for the timeline.
//!
//! `generate_audio_peaks` decodes an asset's entire audio stream, which takes
//! long enough on real footage that doing it on the UI thread would freeze the
//! window — and doing it during import would make dropping in a folder of
//! clips feel broken. So peaks are computed on worker threads and the timeline
//! draws whatever is ready, filling in as results land. That's what Premiere
//! does too: waveforms appear progressively rather than gating the edit.
//!
//! Not persisted. Premiere writes `.pek` sidecar files so waveforms survive
//! reopening a project; here they're recomputed per session. That's a real
//! follow-up (the peaks type is already `Serialize`), but caching to disk
//! needs a cache-location and invalidation policy — keyed on content hash,
//! which `MediaAsset` already carries — rather than just a file write.

use media::MediaAssetId;
use media_ffmpeg::AudioPeaks;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// Samples per peak at the base resolution. At 48 kHz this is ~10.7 ms per
/// peak — about 93 peaks per second, which is a little over one per pixel at
/// the default zoom (60 px/s), so the finest zoom still has real detail to
/// draw. Coarser zooms aggregate from this (see `WaveformCache::column_range`)
/// rather than needing a second decode pass.
const SAMPLES_PER_PEAK: u32 = 512;

/// How many peak jobs may run at once. Each one is a full audio decode, so
/// letting a 50-clip import spawn 50 threads would thrash the disk and starve
/// the UI thread of CPU for no gain — they'd all finish later than if queued.
const MAX_CONCURRENT_JOBS: usize = 2;

pub struct WaveformCache {
    ready: HashMap<MediaAssetId, Arc<AudioPeaks>>,
    /// Jobs currently on a worker thread.
    in_flight: HashSet<MediaAssetId>,
    /// Requested while `in_flight` was full, waiting for a slot.
    queued: Vec<(MediaAssetId, PathBuf)>,
    /// Assets whose peak generation failed (no audio stream, unreadable
    /// file). Remembered so a failing asset isn't retried on every frame —
    /// which would mean re-decoding, and re-failing, 60 times a second.
    failed: HashSet<MediaAssetId>,
    tx: Sender<(MediaAssetId, Option<AudioPeaks>)>,
    rx: Receiver<(MediaAssetId, Option<AudioPeaks>)>,
}

impl Default for WaveformCache {
    fn default() -> Self {
        let (tx, rx) = channel();
        WaveformCache {
            ready: HashMap::new(),
            in_flight: HashSet::new(),
            queued: Vec::new(),
            failed: HashSet::new(),
            tx,
            rx,
        }
    }
}

impl WaveformCache {
    /// Collects finished jobs and starts queued ones. Call once per frame.
    pub fn poll(&mut self) {
        while let Ok((asset, result)) = self.rx.try_recv() {
            self.in_flight.remove(&asset);
            match result {
                Some(peaks) => {
                    self.ready.insert(asset, Arc::new(peaks));
                }
                None => {
                    self.failed.insert(asset);
                }
            }
        }
        while self.in_flight.len() < MAX_CONCURRENT_JOBS && !self.queued.is_empty() {
            let (asset, path) = self.queued.remove(0);
            self.spawn(asset, path);
        }
    }

    /// Peaks for `asset`, or `None` if they aren't ready yet — in which case
    /// generation is started (or queued) so a later frame will have them.
    pub fn peaks(&mut self, asset: MediaAssetId, path: &std::path::Path) -> Option<Arc<AudioPeaks>> {
        if let Some(peaks) = self.ready.get(&asset) {
            return Some(peaks.clone());
        }
        if self.failed.contains(&asset)
            || self.in_flight.contains(&asset)
            || self.queued.iter().any(|(a, _)| *a == asset)
        {
            return None;
        }
        if self.in_flight.len() < MAX_CONCURRENT_JOBS {
            self.spawn(asset, path.to_path_buf());
        } else {
            self.queued.push((asset, path.to_path_buf()));
        }
        None
    }

    /// True while anything is still being computed — lets the UI keep
    /// repainting so freshly-finished waveforms actually appear instead of
    /// waiting for the next mouse move.
    pub fn is_busy(&self) -> bool {
        !self.in_flight.is_empty() || !self.queued.is_empty()
    }

    fn spawn(&mut self, asset: MediaAssetId, path: PathBuf) {
        self.in_flight.insert(asset);
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = media_ffmpeg::generate_audio_peaks(&path, SAMPLES_PER_PEAK).ok();
            // A closed receiver just means the editor shut down mid-job.
            let _ = tx.send((asset, result));
        });
    }
}

/// The (min, max) amplitude across the peaks covering one pixel column.
///
/// Aggregating rather than point-sampling matters when zoomed out: at, say,
/// 20 peaks per pixel, picking one peak per column would randomly miss
/// transients and draw a waveform that visibly changes shape as you scroll.
/// Taking the min and max over the whole column's range makes the drawing
/// stable and preserves the envelope, which is the point of a waveform.
pub fn column_range(peaks: &AudioPeaks, from_sample: i64, to_sample: i64) -> Option<(f32, f32)> {
    if peaks.peaks.is_empty() || peaks.samples_per_peak == 0 {
        return None;
    }
    let spp = peaks.samples_per_peak as i64;
    let first = (from_sample / spp).max(0) as usize;
    // At least one peak wide, so a sub-peak-width column still draws.
    let last = ((to_sample / spp).max(0) as usize).max(first);
    if first >= peaks.peaks.len() {
        return None;
    }
    let last = last.min(peaks.peaks.len() - 1);
    let slice = &peaks.peaks[first..=last];
    let min = slice.iter().map(|(mn, _)| *mn).fold(f32::INFINITY, f32::min);
    let max = slice.iter().map(|(_, mx)| *mx).fold(f32::NEG_INFINITY, f32::max);
    if min.is_finite() && max.is_finite() {
        Some((min, max))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peaks_of(values: &[(f32, f32)]) -> AudioPeaks {
        AudioPeaks { sample_rate: 48_000, samples_per_peak: 100, peaks: values.to_vec() }
    }

    #[test]
    fn a_column_spanning_many_peaks_takes_the_widest_extent() {
        // The anti-aliasing property: a zoomed-out column must report the
        // envelope across everything it covers, not one sampled peak.
        let peaks = peaks_of(&[(-0.1, 0.1), (-0.9, 0.2), (-0.2, 0.8)]);
        let (min, max) = column_range(&peaks, 0, 299).unwrap();
        assert!((min - -0.9).abs() < 1e-6, "min should come from peak 1, got {min}");
        assert!((max - 0.8).abs() < 1e-6, "max should come from peak 2, got {max}");
    }

    #[test]
    fn a_sub_peak_width_column_still_returns_its_peak() {
        // Zoomed all the way in, a column can be narrower than one peak; it
        // must still draw rather than collapsing to nothing.
        let peaks = peaks_of(&[(-0.5, 0.5), (-0.1, 0.1)]);
        let (min, max) = column_range(&peaks, 10, 20).unwrap();
        assert_eq!((min, max), (-0.5, 0.5));
    }

    #[test]
    fn a_range_past_the_end_is_none_rather_than_a_panic() {
        // Trimming a clip past its source's audio length is legal, and the
        // draw loop must not index out of bounds when it happens.
        let peaks = peaks_of(&[(-0.5, 0.5)]);
        assert!(column_range(&peaks, 10_000, 20_000).is_none());
        // Straddling the end clamps instead of panicking.
        assert!(column_range(&peaks, 0, 10_000).is_some());
    }

    #[test]
    fn empty_peaks_are_handled() {
        assert!(column_range(&peaks_of(&[]), 0, 100).is_none());
    }

    #[test]
    fn a_failed_asset_is_not_retried() {
        // Guards the per-frame re-decode trap: without the failure set, a
        // video-only clip on an audio track would kick off a fresh (doomed)
        // decode every frame.
        let mut cache = WaveformCache::default();
        let asset = MediaAssetId(1);
        cache.failed.insert(asset);

        assert!(cache.peaks(asset, std::path::Path::new("C:/nope.mp4")).is_none());
        assert!(!cache.is_busy(), "a known-failed asset must not start a job");
    }

    #[test]
    fn requests_beyond_the_concurrency_limit_are_queued_not_spawned() {
        let mut cache = WaveformCache::default();
        // Three requests, limit of two: the third waits rather than piling on
        // another full-file decode.
        for i in 1..=3u128 {
            cache.peaks(MediaAssetId(i), std::path::Path::new("C:/missing.mp4"));
        }
        assert_eq!(cache.in_flight.len(), MAX_CONCURRENT_JOBS);
        assert_eq!(cache.queued.len(), 1);
    }
}
