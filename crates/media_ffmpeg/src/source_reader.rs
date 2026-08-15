//! Sequential source-frame reader: "give me this asset's frame at source time
//! T", for callers that walk T mostly forward.
//!
//! Both consumers of this — export and real-time playback — ask for source
//! frames in *mostly* increasing order, one output frame at a time, each
//! clip's source time advancing with the timeline. The naive implementation
//! (`decode_frame_at` per frame) reopens and re-seeks the file for every
//! single frame, which for a 30s 30fps export is ~900 file opens and ~900
//! keyframe-to-target decode runs. This keeps one decoder open per asset and
//! walks it forward instead, seeking only when the request actually jumps —
//! which is what makes cost scale with the number of frames rather than with
//! (frames x GOP length).
//!
//! Originally written for the export loop; playback needs exactly the same
//! access pattern, so it lives here in the decode layer rather than being
//! reimplemented (and drifting) on the playback side.

use crate::{timeline_timebase, DecodedRgbaFrame, VideoDecoderStream};
use std::path::Path;

/// Forward distance beyond which seeking beats decoding-and-discarding.
/// Below it, walking forward is cheaper than a seek (which has to restart
/// from a keyframe and decode forward anyway); above it, the seek wins.
/// One second is a coarse but safe split: it's longer than a typical GOP, so
/// a within-threshold walk is usually decoding frames we'd have had to
/// decode after seeking regardless.
const SEEK_THRESHOLD_TICKS: i64 = timeline_timebase();

pub struct SourceReader {
    stream: VideoDecoderStream,
    /// The frame currently being presented: the latest one decoded whose PTS
    /// is at or before the most recent request.
    current: Option<DecodedRgbaFrame>,
    /// One frame of lookahead. Needed because "the latest frame at or before
    /// `wanted`" can only be identified by decoding one frame *past* it and
    /// finding it's too late — that frame then has to be kept for the next
    /// request rather than thrown away.
    lookahead: Option<DecodedRgbaFrame>,
}

impl SourceReader {
    pub fn open(path: &Path) -> Result<Self, crate::ProbeError> {
        Ok(SourceReader { stream: VideoDecoderStream::open(path)?, current: None, lookahead: None })
    }

    /// The frame to present at source time `wanted`: the latest frame at or
    /// before it, matching `render::SourceFrames`' presentation rule. `None`
    /// only at end of stream (or if the file stops yielding frames), which
    /// the caller treats as "this track contributes nothing to this frame"
    /// rather than an error.
    pub fn frame_at(&mut self, wanted: i64) -> Option<&DecodedRgbaFrame> {
        let needs_seek = match &self.current {
            // First use, or the request went backwards past what we hold.
            None => true,
            Some(f) => f.pts_ticks > wanted || wanted - f.pts_ticks > SEEK_THRESHOLD_TICKS,
        };

        if needs_seek {
            // `seek` lands on the first frame at or *after* the target. For a
            // presentation lookup that's off by at most one frame interval,
            // and `render::SourceFrames::get` treats a request preceding
            // everything held as "show the earliest frame" — so this stays
            // correct at a clip's very first frame, where no earlier frame
            // exists to find.
            self.stream.seek(wanted).ok()?;
            self.lookahead = None;
            self.current = self.stream.next_frame().ok().flatten();
        }

        // Walk forward while the *following* frame is still due.
        loop {
            if self.lookahead.is_none() {
                self.lookahead = self.stream.next_frame().ok().flatten();
            }
            match &self.lookahead {
                Some(next) if next.pts_ticks <= wanted => self.current = self.lookahead.take(),
                // Either the next frame is in the future (keep it for later)
                // or the stream ended (hold the last frame we got).
                _ => break,
            }
        }
        self.current.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("test_fixtures")
            .join(name)
    }

    /// Ticks between frames at 30fps, spelled out rather than pulled from
    /// `timeline` (which this crate must not depend on — see `lib.rs`).
    const TICKS_PER_FRAME_30: i64 = timeline_timebase() / 30;

    #[test]
    fn walks_forward_without_reseeking_and_beats_reopening_per_frame() {
        // This is the measurement behind the whole playback-pipeline change.
        // The claim was "reopening and re-seeking the file per frame is what
        // kept picture from holding rate" — an assumption until it's timed, so
        // time it: same file, same 60 target ticks, both access patterns.
        crate::init().unwrap();
        let path = fixture("test_playback_demo.mp4");
        let targets: Vec<i64> = (0..60).map(|i| i * TICKS_PER_FRAME_30).collect();

        let mut reader = SourceReader::open(&path).unwrap();
        // Prime it, so neither side is charged for first-open cost.
        reader.frame_at(0).unwrap();
        let persistent_start = Instant::now();
        for &t in &targets {
            assert!(reader.frame_at(t).is_some(), "should have a frame at {t}");
        }
        let persistent = persistent_start.elapsed();

        let reopen_start = Instant::now();
        for &t in &targets {
            crate::decode_frame_at(&path, t).unwrap();
        }
        let reopen = reopen_start.elapsed();

        let speedup = reopen.as_secs_f64() / persistent.as_secs_f64();
        println!(
            "60 frames: persistent {persistent:?}, reopen-per-frame {reopen:?} ({speedup:.1}x)"
        );
        assert!(
            speedup >= 3.0,
            "walking a persistent decoder forward should be far cheaper than reopening per \
             frame, but was only {speedup:.1}x ({persistent:?} vs {reopen:?}) — if this ever \
             stops being true, the playback pipeline's whole reason to exist is gone"
        );
    }

    #[test]
    fn a_backwards_request_reseeks_and_still_returns_the_right_frame() {
        // Scrubbing backwards during playback, or a clip whose source time
        // runs backwards under a negative speed, must not return a stale
        // forward frame.
        crate::init().unwrap();
        let mut reader = SourceReader::open(&fixture("test_playback_demo.mp4")).unwrap();

        let late = reader.frame_at(TICKS_PER_FRAME_30 * 40).unwrap().pts_ticks;
        let early = reader.frame_at(TICKS_PER_FRAME_30 * 2).unwrap().pts_ticks;
        assert!(
            early < late,
            "after asking for an earlier tick the reader should have gone back, but reported \
             {early} having previously been at {late}"
        );
        assert!(
            (early - TICKS_PER_FRAME_30 * 2).abs() <= TICKS_PER_FRAME_30,
            "expected a frame within one frame of the requested tick, got {early}"
        );
    }
}
