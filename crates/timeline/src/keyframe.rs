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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamTrack {
    pub default: ParamValue,
    pub keyframes: Vec<Keyframe>,
}

impl ParamValue {
    /// Component-wise linear interpolation. `Bool` never interpolates (it
    /// holds `self` until the next keyframe), and mismatched variants fall
    /// back to `self` rather than panicking — a type mismatch between two
    /// keyframes on the same track is a caller bug, but a render loop is
    /// the wrong place to discover it.
    pub fn lerp(&self, other: &ParamValue, t: f64) -> ParamValue {
        match (self, other) {
            (ParamValue::Number(a), ParamValue::Number(b)) => ParamValue::Number(a + (b - a) * t),
            (ParamValue::Vec2(ax, ay), ParamValue::Vec2(bx, by)) => {
                ParamValue::Vec2(ax + (bx - ax) * t, ay + (by - ay) * t)
            }
            (ParamValue::Color(a), ParamValue::Color(b)) => {
                let mut out = [0.0f32; 4];
                for i in 0..4 {
                    out[i] = a[i] + (b[i] - a[i]) * t as f32;
                }
                ParamValue::Color(out)
            }
            _ => *self,
        }
    }

    /// The numeric value, or `None` for non-`Number` variants. Public
    /// because every consumer that evaluates a scalar parameter (the render
    /// graph's effect params, the audio mixer's gain/pan) needs exactly this
    /// and would otherwise re-match the enum locally.
    pub fn as_scalar(&self) -> Option<f64> {
        match self {
            ParamValue::Number(v) => Some(*v),
            _ => None,
        }
    }
}

fn cubic_bezier_at(p0: f64, p1: f64, p2: f64, p3: f64, s: f64) -> f64 {
    let u = 1.0 - s;
    u * u * u * p0 + 3.0 * u * u * s * p1 + 3.0 * u * s * s * p2 + s * s * s * p3
}

/// Solves `x(s) = target_x` for `s` in [0,1] by bisection. Bisection rather
/// than Newton deliberately: it needs no derivative, can't diverge, and
/// degrades gracefully into "closest available s" if a user drags tangent
/// handles into a non-monotonic (self-overlapping) time curve — which the
/// UI will eventually allow and which would make Newton misbehave.
fn solve_bezier_s_for_x(x0: f64, x1: f64, x2: f64, x3: f64, target_x: f64) -> f64 {
    let mut lo = 0.0f64;
    let mut hi = 1.0f64;
    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);
        if cubic_bezier_at(x0, x1, x2, x3, mid) < target_x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Catmull-Rom-style automatic tangent: slope through the neighbours on
/// either side, scaled to a third of the outgoing segment's width. This is
/// what `AutoBezier` means, and it's also the fallback when `Bezier` has no
/// explicit tangents stored yet.
fn auto_tangents(keyframes: &[Keyframe], i: usize) -> ((f64, f64), (f64, f64)) {
    let point = |k: &Keyframe| (k.at.0 as f64, k.value.as_scalar().unwrap_or(0.0));
    let cur = point(&keyframes[i]);
    let next = point(&keyframes[i + 1]);
    let prev = if i > 0 { point(&keyframes[i - 1]) } else { cur };
    let after_next = if i + 2 < keyframes.len() { point(&keyframes[i + 2]) } else { next };

    let seg_third = (next.0 - cur.0) / 3.0;

    let slope_at_cur = {
        let dt = next.0 - prev.0;
        if dt != 0.0 { (next.1 - prev.1) / dt } else { 0.0 }
    };
    let slope_at_next = {
        let dt = after_next.0 - cur.0;
        if dt != 0.0 { (after_next.1 - cur.1) / dt } else { 0.0 }
    };

    let out_of_cur = (seg_third, slope_at_cur * seg_third);
    let in_of_next = (-seg_third, -slope_at_next * seg_third);
    (out_of_cur, in_of_next)
}

impl ParamTrack {
    pub fn constant(value: ParamValue) -> Self {
        ParamTrack { default: value, keyframes: Vec::new() }
    }

    /// True when this parameter animates. An empty keyframe list means the
    /// parameter is a constant at `default` — that's the distinction the UI's
    /// stopwatch toggle exposes.
    pub fn is_animated(&self) -> bool {
        !self.keyframes.is_empty()
    }

    /// Index of the keyframe exactly at `at`, if there is one.
    pub fn keyframe_index_at(&self, at: TimeTick) -> Option<usize> {
        self.keyframes.binary_search_by(|k| k.at.cmp(&at)).ok()
    }

    /// Sets `at` to `value`, replacing an existing keyframe there or inserting
    /// a new one in sorted position.
    ///
    /// **Every mutator here maintains `keyframes` sorted ascending by `at`.**
    /// That isn't cosmetic: `evaluate_at` locates the surrounding segment with
    /// `binary_search_by`, which silently returns nonsense on an unsorted
    /// slice — the parameter would interpolate between the wrong pair of
    /// keyframes with no error anywhere. Appending and forgetting to re-sort
    /// is the obvious way to introduce that, so insertion position is computed
    /// rather than fixed up afterwards.
    pub fn upsert_keyframe(
        &mut self,
        at: TimeTick,
        value: ParamValue,
        interpolation: InterpolationMode,
    ) {
        match self.keyframes.binary_search_by(|k| k.at.cmp(&at)) {
            Ok(i) => self.keyframes[i].value = value,
            Err(i) => self
                .keyframes
                .insert(i, Keyframe { at, value, interpolation, tangents: None }),
        }
    }

    /// Removes the keyframe at `at`. Returns whether one was there.
    pub fn remove_keyframe_at(&mut self, at: TimeTick) -> bool {
        match self.keyframes.binary_search_by(|k| k.at.cmp(&at)) {
            Ok(i) => {
                self.keyframes.remove(i);
                true
            }
            Err(_) => false,
        }
    }

    /// Moves the keyframe at `from` to `to`, keeping the list sorted. A move
    /// onto an existing keyframe's time replaces it, matching how dragging one
    /// keyframe onto another behaves in an NLE.
    pub fn move_keyframe(&mut self, from: TimeTick, to: TimeTick) -> bool {
        let Ok(i) = self.keyframes.binary_search_by(|k| k.at.cmp(&from)) else {
            return false;
        };
        if from == to {
            return true;
        }
        let mut kf = self.keyframes.remove(i);
        kf.at = to;
        match self.keyframes.binary_search_by(|k| k.at.cmp(&to)) {
            Ok(j) => self.keyframes[j] = kf,
            Err(j) => self.keyframes.insert(j, kf),
        }
        true
    }

    /// Sets the interpolation mode of the keyframe at `at`. Since the *left*
    /// keyframe governs the segment leaving it, this changes the curve from
    /// `at` to the following keyframe.
    pub fn set_interpolation_at(&mut self, at: TimeTick, mode: InterpolationMode) -> bool {
        match self.keyframes.binary_search_by(|k| k.at.cmp(&at)) {
            Ok(i) => {
                self.keyframes[i].interpolation = mode;
                true
            }
            Err(_) => false,
        }
    }

    /// Sets the explicit (out, in) tangent handles of the keyframe at `at`.
    /// Only meaningful when that keyframe's interpolation is `Bezier` —
    /// `evaluate_at` ignores `tangents` for every other mode — but setting
    /// them regardless of the current mode lets the UI drag a handle before
    /// committing to Bezier without losing the drag's result.
    pub fn set_tangents_at(&mut self, at: TimeTick, tangents: ((f64, f64), (f64, f64))) -> bool {
        match self.keyframes.binary_search_by(|k| k.at.cmp(&at)) {
            Ok(i) => {
                self.keyframes[i].tangents = Some(tangents);
                true
            }
            Err(_) => false,
        }
    }

    /// Latest keyframe strictly before `at` — "go to previous keyframe".
    pub fn prev_keyframe_before(&self, at: TimeTick) -> Option<TimeTick> {
        self.keyframes.iter().rev().find(|k| k.at < at).map(|k| k.at)
    }

    /// Earliest keyframe strictly after `at` — "go to next keyframe".
    pub fn next_keyframe_after(&self, at: TimeTick) -> Option<TimeTick> {
        self.keyframes.iter().find(|k| k.at > at).map(|k| k.at)
    }

    /// Resolves this parameter to a concrete value at `tick`.
    ///
    /// The interpolation mode of the *left* keyframe governs the segment
    /// leaving it — the standard NLE/DAW convention.
    ///
    /// Bezier scope note: `tangents` is stored as one `(time, value)` pair
    /// per side, which can only describe a *scalar* value curve. So
    /// `Number` params get a true bezier value curve (overshoot and all),
    /// while `Vec2`/`Color` params use the bezier purely as a timing
    /// function (solve `s` from the time axis, then interpolate components
    /// linearly by `s`). Spec 4.5's "spatial parameters get a motion path
    /// with editable tangents" needs per-component tangent storage — a
    /// different shape than this, and M5 work rather than a guess now.
    pub fn evaluate_at(&self, tick: TimeTick) -> ParamValue {
        let kfs = &self.keyframes;
        if kfs.is_empty() {
            return self.default;
        }
        if tick <= kfs[0].at {
            return kfs[0].value;
        }
        let last = &kfs[kfs.len() - 1];
        if tick >= last.at {
            return last.value;
        }

        let i = match kfs.binary_search_by(|k| k.at.cmp(&tick)) {
            Ok(exact) => return kfs[exact].value,
            // `Err(pos)` is the first keyframe *after* `tick`; the segment
            // we're inside starts at `pos - 1`. `pos >= 1` here because the
            // `tick <= kfs[0].at` case returned above.
            Err(pos) => pos - 1,
        };
        let (left, right) = (&kfs[i], &kfs[i + 1]);

        let span = (right.at.0 - left.at.0) as f64;
        if span <= 0.0 {
            return left.value;
        }
        let linear_t = (tick.0 - left.at.0) as f64 / span;

        match left.interpolation {
            InterpolationMode::Hold => left.value,
            InterpolationMode::Linear => left.value.lerp(&right.value, linear_t),
            InterpolationMode::Bezier | InterpolationMode::AutoBezier => {
                let (out_tan, in_tan) = match (left.interpolation, left.tangents, right.tangents) {
                    // Explicit tangents: left keyframe's OUT handle and
                    // right keyframe's IN handle define this segment.
                    (InterpolationMode::Bezier, Some((_, out_t)), Some((in_t, _))) => (out_t, in_t),
                    (InterpolationMode::Bezier, Some((_, out_t)), None) => {
                        (out_t, auto_tangents(kfs, i).1)
                    }
                    (InterpolationMode::Bezier, None, Some((in_t, _))) => {
                        (auto_tangents(kfs, i).0, in_t)
                    }
                    _ => auto_tangents(kfs, i),
                };

                let x0 = left.at.0 as f64;
                let x3 = right.at.0 as f64;
                let x1 = x0 + out_tan.0;
                let x2 = x3 + in_tan.0;
                let s = solve_bezier_s_for_x(x0, x1, x2, x3, tick.0 as f64);

                match (left.value, right.value) {
                    (ParamValue::Number(v0), ParamValue::Number(v3)) => {
                        let v1 = v0 + out_tan.1;
                        let v2 = v3 + in_tan.1;
                        ParamValue::Number(cubic_bezier_at(v0, v1, v2, v3, s))
                    }
                    // Multi-component: bezier as a timing function only.
                    _ => left.value.lerp(&right.value, s),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kf(at: i64, v: f64, mode: InterpolationMode) -> Keyframe {
        Keyframe { at: TimeTick(at), value: ParamValue::Number(v), interpolation: mode, tangents: None }
    }

    fn num(v: ParamValue) -> f64 {
        match v {
            ParamValue::Number(n) => n,
            other => panic!("expected Number, got {other:?}"),
        }
    }

    #[test]
    fn no_keyframes_returns_default() {
        let t = ParamTrack::constant(ParamValue::Number(7.0));
        assert_eq!(num(t.evaluate_at(TimeTick(0))), 7.0);
        assert_eq!(num(t.evaluate_at(TimeTick(9999))), 7.0);
    }

    #[test]
    fn clamps_outside_the_keyframe_range() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![kf(100, 1.0, InterpolationMode::Linear), kf(200, 2.0, InterpolationMode::Linear)],
        };
        assert_eq!(num(t.evaluate_at(TimeTick(0))), 1.0, "before first keyframe holds first value");
        assert_eq!(num(t.evaluate_at(TimeTick(500))), 2.0, "after last keyframe holds last value");
    }

    #[test]
    fn linear_interpolates_midpoint() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![kf(0, 0.0, InterpolationMode::Linear), kf(100, 10.0, InterpolationMode::Linear)],
        };
        assert_eq!(num(t.evaluate_at(TimeTick(50))), 5.0);
        assert_eq!(num(t.evaluate_at(TimeTick(25))), 2.5);
    }

    #[test]
    fn hold_keeps_left_value_until_the_next_keyframe() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![kf(0, 0.0, InterpolationMode::Hold), kf(100, 10.0, InterpolationMode::Hold)],
        };
        assert_eq!(num(t.evaluate_at(TimeTick(50))), 0.0);
        assert_eq!(num(t.evaluate_at(TimeTick(99))), 0.0);
        assert_eq!(num(t.evaluate_at(TimeTick(100))), 10.0);
    }

    #[test]
    fn exact_keyframe_hit_returns_that_value() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![
                kf(0, 0.0, InterpolationMode::Linear),
                kf(100, 10.0, InterpolationMode::Linear),
                kf(200, 20.0, InterpolationMode::Linear),
            ],
        };
        assert_eq!(num(t.evaluate_at(TimeTick(100))), 10.0);
    }

    #[test]
    fn bezier_passes_through_its_endpoints_and_stays_monotonic_when_smooth() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![kf(0, 0.0, InterpolationMode::AutoBezier), kf(100, 10.0, InterpolationMode::AutoBezier)],
        };
        assert!((num(t.evaluate_at(TimeTick(0))) - 0.0).abs() < 1e-6);
        assert!((num(t.evaluate_at(TimeTick(100))) - 10.0).abs() < 1e-6);
        let mut prev = f64::NEG_INFINITY;
        for tick in (0..=100).step_by(5) {
            let v = num(t.evaluate_at(TimeTick(tick)));
            assert!(v >= prev - 1e-9, "auto-bezier over a rising pair should not dip: {v} after {prev}");
            prev = v;
        }
    }

    #[test]
    fn bezier_ease_in_lags_behind_linear_at_the_start() {
        // Out-handle pulled flat and far to the right = slow start.
        let mut a = kf(0, 0.0, InterpolationMode::Bezier);
        a.tangents = Some(((0.0, 0.0), (90.0, 0.0)));
        let mut b = kf(100, 10.0, InterpolationMode::Bezier);
        b.tangents = Some(((-10.0, 0.0), (0.0, 0.0)));
        let t = ParamTrack { default: ParamValue::Number(0.0), keyframes: vec![a, b] };
        let eased = num(t.evaluate_at(TimeTick(25)));
        assert!(eased < 2.5, "ease-in should trail linear's 2.5 at t=0.25, got {eased}");
    }

    #[test]
    fn vec2_interpolates_component_wise() {
        let t = ParamTrack {
            default: ParamValue::Vec2(0.0, 0.0),
            keyframes: vec![
                Keyframe { at: TimeTick(0), value: ParamValue::Vec2(0.0, 10.0), interpolation: InterpolationMode::Linear, tangents: None },
                Keyframe { at: TimeTick(100), value: ParamValue::Vec2(100.0, 20.0), interpolation: InterpolationMode::Linear, tangents: None },
            ],
        };
        match t.evaluate_at(TimeTick(50)) {
            ParamValue::Vec2(x, y) => {
                assert_eq!(x, 50.0);
                assert_eq!(y, 15.0);
            }
            other => panic!("expected Vec2, got {other:?}"),
        }
    }

    #[test]
    fn bool_holds_rather_than_interpolating() {
        let t = ParamTrack {
            default: ParamValue::Bool(false),
            keyframes: vec![
                Keyframe { at: TimeTick(0), value: ParamValue::Bool(false), interpolation: InterpolationMode::Linear, tangents: None },
                Keyframe { at: TimeTick(100), value: ParamValue::Bool(true), interpolation: InterpolationMode::Linear, tangents: None },
            ],
        };
        assert_eq!(t.evaluate_at(TimeTick(50)), ParamValue::Bool(false));
        assert_eq!(t.evaluate_at(TimeTick(100)), ParamValue::Bool(true));
    }

    /// The invariant every mutator must preserve, checked as a property
    /// rather than trusted: an unsorted list makes `evaluate_at`'s binary
    /// search interpolate between the wrong keyframes, silently.
    fn assert_sorted(t: &ParamTrack) {
        assert!(
            t.keyframes.windows(2).all(|w| w[0].at < w[1].at),
            "keyframes must stay sorted and unique by time, got {:?}",
            t.keyframes.iter().map(|k| k.at.0).collect::<Vec<_>>()
        );
    }

    #[test]
    fn upsert_inserts_in_sorted_position_regardless_of_call_order() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        // Deliberately out of order — the UI adds keyframes wherever the
        // playhead happens to be, which is not monotonic.
        for at in [500, 100, 900, 300, 700] {
            t.upsert_keyframe(TimeTick(at), ParamValue::Number(at as f64), InterpolationMode::Linear);
        }
        assert_sorted(&t);
        assert_eq!(t.keyframes.len(), 5);
        // And the engine reads them back correctly: midway between 100 and 300.
        assert_eq!(num(t.evaluate_at(TimeTick(200))), 200.0);
    }

    #[test]
    fn upsert_at_an_existing_time_replaces_rather_than_duplicating() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(1.0), InterpolationMode::Linear);
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(9.0), InterpolationMode::Linear);
        assert_eq!(t.keyframes.len(), 1, "a second edit at the same tick must not add a keyframe");
        assert_eq!(num(t.evaluate_at(TimeTick(100))), 9.0);
        assert_sorted(&t);
    }

    #[test]
    fn set_tangents_at_writes_the_handles_of_the_keyframe_at_that_time() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(1.0), InterpolationMode::Bezier);

        let tangents = ((-20.0, -0.5), (20.0, 0.5));
        assert!(t.set_tangents_at(TimeTick(100), tangents));
        assert_eq!(t.keyframes[0].tangents, Some(tangents));
    }

    #[test]
    fn set_tangents_at_a_time_with_no_keyframe_reports_failure() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(1.0), InterpolationMode::Bezier);
        assert!(!t.set_tangents_at(TimeTick(250), ((0.0, 0.0), (0.0, 0.0))));
    }

    #[test]
    fn moving_a_keyframe_past_its_neighbours_keeps_the_list_sorted() {
        // Dragging a keyframe across another one is ordinary UI behaviour, and
        // the naive implementation (mutate `at` in place) breaks the ordering
        // invariant without any visible error.
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        for at in [100, 200, 300] {
            t.upsert_keyframe(TimeTick(at), ParamValue::Number(at as f64), InterpolationMode::Linear);
        }
        assert!(t.move_keyframe(TimeTick(100), TimeTick(250)));
        assert_sorted(&t);
        assert_eq!(
            t.keyframes.iter().map(|k| k.at.0).collect::<Vec<_>>(),
            vec![200, 250, 300]
        );
        assert_eq!(num(t.evaluate_at(TimeTick(250))), 100.0, "the moved keyframe keeps its value");
    }

    #[test]
    fn moving_onto_an_occupied_time_replaces_instead_of_duplicating() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(1.0), InterpolationMode::Linear);
        t.upsert_keyframe(TimeTick(200), ParamValue::Number(2.0), InterpolationMode::Linear);
        assert!(t.move_keyframe(TimeTick(100), TimeTick(200)));
        assert_eq!(t.keyframes.len(), 1);
        assert_eq!(num(t.evaluate_at(TimeTick(200))), 1.0, "the dragged keyframe wins");
        assert_sorted(&t);
    }

    #[test]
    fn navigation_finds_strictly_adjacent_keyframes() {
        let mut t = ParamTrack::constant(ParamValue::Number(0.0));
        for at in [100, 200, 300] {
            t.upsert_keyframe(TimeTick(at), ParamValue::Number(0.0), InterpolationMode::Linear);
        }
        // Strictly, so that sitting exactly on a keyframe still steps off it
        // rather than returning the one under the playhead forever.
        assert_eq!(t.next_keyframe_after(TimeTick(200)), Some(TimeTick(300)));
        assert_eq!(t.prev_keyframe_before(TimeTick(200)), Some(TimeTick(100)));
        assert_eq!(t.next_keyframe_after(TimeTick(300)), None);
        assert_eq!(t.prev_keyframe_before(TimeTick(100)), None);
    }

    #[test]
    fn removing_the_last_keyframe_makes_the_track_constant_again() {
        let mut t = ParamTrack::constant(ParamValue::Number(4.0));
        t.upsert_keyframe(TimeTick(100), ParamValue::Number(1.0), InterpolationMode::Linear);
        assert!(t.is_animated());
        assert!(t.remove_keyframe_at(TimeTick(100)));
        assert!(!t.is_animated());
        assert_eq!(num(t.evaluate_at(TimeTick(100))), 4.0, "falls back to default");
        assert!(!t.remove_keyframe_at(TimeTick(100)), "removing again reports nothing removed");
    }

    #[test]
    fn multi_segment_track_uses_the_correct_segment() {
        let t = ParamTrack {
            default: ParamValue::Number(0.0),
            keyframes: vec![
                kf(0, 0.0, InterpolationMode::Linear),
                kf(100, 10.0, InterpolationMode::Hold),
                kf(200, 20.0, InterpolationMode::Linear),
            ],
        };
        assert_eq!(num(t.evaluate_at(TimeTick(50))), 5.0, "first segment is linear");
        assert_eq!(num(t.evaluate_at(TimeTick(150))), 10.0, "second segment is hold");
    }
}
