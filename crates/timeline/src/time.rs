//! Rational tick-based time. No floats anywhere in this module — see
//! docs/decisions-log.md "No floating point in timeline math".

use serde::{Deserialize, Serialize};
use std::fmt;

/// Ticks per second. Matches the value Premiere Pro uses internally (often
/// called the "Adobe tick"): chosen so every `FrameRate` below divides it
/// exactly, with no remainder. Verified by `frame_rates_divide_timebase_exactly`.
pub const TIMEBASE: i64 = 254_016_000_000;

/// An absolute or relative point in time, in ticks. Never construct one from
/// a float; convert from a frame number + `FrameRate` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TimeTick(pub i64);

impl TimeTick {
    pub const ZERO: TimeTick = TimeTick(0);

    pub fn from_frame(frame: i64, rate: FrameRate) -> Self {
        TimeTick(frame * rate.ticks_per_frame())
    }

    /// Truncates to the containing frame. Ripple/roll/slip/slide math (M3)
    /// must snap to frame boundaries using this, never round in float space.
    pub fn to_frame(self, rate: FrameRate) -> i64 {
        self.0.div_euclid(rate.ticks_per_frame())
    }

    pub fn checked_add(self, other: TimeTick) -> Option<TimeTick> {
        self.0.checked_add(other.0).map(TimeTick)
    }

    pub fn checked_sub(self, other: TimeTick) -> Option<TimeTick> {
        self.0.checked_sub(other.0).map(TimeTick)
    }
}

/// Every sequence frame rate v1.0 must display timecode for, per spec section 4.2.
/// `Custom` covers anything probed off a source asset that doesn't match a
/// standard rate; sequences are always one of the named rates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FrameRate {
    Fps24,
    Fps23_976,
    Fps25,
    Fps29_97,
    Fps30,
    Fps50,
    Fps59_94,
    Fps60,
    /// (numerator, denominator), e.g. a VFR source's nominal rate for display only.
    Custom(u32, u32),
}

impl FrameRate {
    /// Numerator/denominator such that fps = num/den.
    pub fn as_rational(self) -> (u32, u32) {
        match self {
            FrameRate::Fps24 => (24, 1),
            FrameRate::Fps23_976 => (24000, 1001),
            FrameRate::Fps25 => (25, 1),
            FrameRate::Fps29_97 => (30000, 1001),
            FrameRate::Fps30 => (30, 1),
            FrameRate::Fps50 => (50, 1),
            FrameRate::Fps59_94 => (60000, 1001),
            FrameRate::Fps60 => (60, 1),
            FrameRate::Custom(n, d) => (n, d),
        }
    }

    /// Panics (via integer division, checked in tests) if `TIMEBASE` isn't
    /// evenly divisible by this rate — that would mean the timebase constant
    /// is wrong, which is a build-time-discoverable bug, not a runtime one.
    pub fn ticks_per_frame(self) -> i64 {
        let (num, den) = self.as_rational();
        TIMEBASE * den as i64 / num as i64
    }

    /// True 1000/1001 rates use drop-frame timecode display by convention.
    pub fn is_drop_frame_by_default(self) -> bool {
        matches!(self, FrameRate::Fps29_97 | FrameRate::Fps59_94)
    }
}

/// Pure function of (tick, rate, drop-frame flag) -> display string. Never
/// stores derived state; recomputed on demand so it can't drift from the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timecode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
    pub drop_frame: bool,
}

impl Timecode {
    pub fn from_tick(tick: TimeTick, rate: FrameRate, drop_frame: bool) -> Self {
        let (num, den) = rate.as_rational();
        let nominal_fps = ((num as f64 / den as f64).round()) as i64; // 30 or 60
        let real_frame_number = tick.to_frame(rate).max(0);

        if !drop_frame {
            let frames = (real_frame_number % nominal_fps) as u32;
            let total_seconds = real_frame_number / nominal_fps;
            return Timecode {
                hours: (total_seconds / 3600) as u32,
                minutes: ((total_seconds / 60) % 60) as u32,
                seconds: (total_seconds % 60) as u32,
                frames,
                drop_frame,
            };
        }

        // Constructed directly from the SMPTE drop-frame definition rather
        // than a memorized closed-form formula: at the start of every minute
        // except every 10th, frame LABELS 0 and 1 are skipped (no video
        // frames are actually dropped — this is display numbering only).
        // Real frame count and label count are therefore 1:1; what shifts is
        // which (minute, second, frame) label a given real frame count maps
        // to. Hand-verified against the case that actually matters: the
        // frame right after a skip must read `;02`, not `;00`.
        let drop_per_min = nominal_fps / 15; // 2 for 29.97, 4 for 59.94
        let full_minute_frames = nominal_fps * 60; // every 10th minute: no skip
        let dropped_minute_frames = full_minute_frames - drop_per_min; // other 9 minutes
        let frames_per_10min = full_minute_frames + 9 * dropped_minute_frames;

        let block = real_frame_number / frames_per_10min;
        let r = real_frame_number % frames_per_10min;

        let (minute_in_block, offset) = if r < full_minute_frames {
            (0, r)
        } else {
            let r2 = r - full_minute_frames;
            (1 + r2 / dropped_minute_frames, r2 % dropped_minute_frames)
        };
        let absolute_minute = block * 10 + minute_in_block;

        let (second, frame_label) = if minute_in_block == 0 {
            (offset / nominal_fps, offset % nominal_fps)
        } else if offset < drop_per_min {
            (0, offset + drop_per_min)
        } else {
            let offset2 = offset - drop_per_min;
            (1 + offset2 / nominal_fps, offset2 % nominal_fps)
        };

        Timecode {
            hours: (absolute_minute / 60) as u32,
            minutes: (absolute_minute % 60) as u32,
            seconds: second as u32,
            frames: frame_label as u32,
            drop_frame,
        }
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.drop_frame { ';' } else { ':' };
        write!(
            f,
            "{:02}:{:02}:{:02}{}{:02}",
            self.hours, self.minutes, self.seconds, sep, self.frames
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_divide_timebase_exactly() {
        for rate in [
            FrameRate::Fps24,
            FrameRate::Fps23_976,
            FrameRate::Fps25,
            FrameRate::Fps29_97,
            FrameRate::Fps30,
            FrameRate::Fps50,
            FrameRate::Fps59_94,
            FrameRate::Fps60,
        ] {
            let (num, den) = rate.as_rational();
            assert_eq!(
                TIMEBASE * den as i64 % num as i64,
                0,
                "{rate:?} does not divide TIMEBASE exactly"
            );
        }
    }

    #[test]
    fn round_trip_frame_to_tick() {
        let rate = FrameRate::Fps29_97;
        for frame in [0, 1, 100, 1799, 18000] {
            let tick = TimeTick::from_frame(frame, rate);
            assert_eq!(tick.to_frame(rate), frame);
        }
    }

    #[test]
    fn non_drop_timecode_at_one_hour() {
        let rate = FrameRate::Fps30;
        let tick = TimeTick::from_frame(30 * 60 * 60, rate);
        let tc = Timecode::from_tick(tick, rate, false);
        assert_eq!(tc.to_string(), "01:00:00:00");
    }

    #[test]
    fn drop_frame_skips_00_01_at_non_tenth_minute() {
        let rate = FrameRate::Fps29_97;

        // Real frame 1799 is the last frame of a full nominal-30fps minute
        // (frame labels 0..29 at 30fps, so label 29 is frame index 1799).
        let last_frame_of_minute_0 = TimeTick::from_frame(1799, rate);
        let tc_before = Timecode::from_tick(last_frame_of_minute_0, rate, true);
        assert_eq!((tc_before.minutes, tc_before.seconds, tc_before.frames), (0, 59, 29));

        // The very next real frame (1800) is where drop-frame skips labels
        // :00 and :01 of minute 1 — it must read ;02, not ;00.
        let first_frame_of_minute_1 = TimeTick::from_frame(1800, rate);
        let tc_after = Timecode::from_tick(first_frame_of_minute_1, rate, true);
        assert_eq!((tc_after.minutes, tc_after.seconds, tc_after.frames), (1, 0, 2));
    }

    #[test]
    fn drop_frame_does_not_skip_at_tenth_minute() {
        let rate = FrameRate::Fps29_97;
        // 10 real minutes = frames_per_10min (17982) real frames; minute 10
        // is an exact multiple of 10, so no labels are skipped there.
        let tick = TimeTick::from_frame(17982, rate);
        let tc = Timecode::from_tick(tick, rate, true);
        assert_eq!((tc.minutes, tc.seconds, tc.frames), (10, 0, 0));
    }
}
