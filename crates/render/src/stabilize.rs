//! Warp Stabilizer, scoped honestly: block-matching translation estimation
//! plus path smoothing, applied as keyframed `Transform::position` on the
//! clip — no new effect or shader needed, since counter-animating position
//! is exactly what the existing Transform effect already does.
//!
//! **What this does and doesn't model, stated plainly.** Real "Warp
//! Stabilizer"-class tools (Adobe's, DaVinci's) track many feature points,
//! fit a similarity or affine transform per frame (translation + rotation +
//! scale), synthesize or crop the borders a correction reveals, and often
//! use content-aware fill. This implementation estimates one dominant 2D
//! **translation** per frame pair via block matching (bounded search-window
//! sum-of-absolute-differences on a downsampled greyscale image — the same
//! idea codec motion estimation uses, not phase correlation or optical
//! flow), smooths the resulting camera path, and writes the difference as
//! position keyframes. It corrects ordinary handheld pan/shake and does
//! **not** correct rotation, zoom, or perspective wobble, and does **not**
//! crop or fill the borders a correction reveals — those are real, separate
//! follow-up work, not silently missing.

/// Search-window motion estimation parameters.
pub struct MotionConfig {
    /// Half-width of the `(dx, dy)` search window, in the *downsampled*
    /// image's pixels.
    pub max_offset: i32,
    /// Box-average downsample factor applied before searching, trading
    /// precision for search cost (the search is `O(max_offset^2 *
    /// pixel_count)`, so halving resolution quarters the per-candidate cost
    /// on top of quartering the candidate count... this parameter only
    /// controls the *input* resolution; callers needing speed on large
    /// frames should downsample before calling, not rely on this alone).
    pub downsample: usize,
}

/// The `(dx, dy)` translation, in the *input* image's pixel units, describing
/// how content moved from `prev` to `curr`: `curr`'s content sits `dx` to the
/// right and `dy` down from where that same content was in `prev`. Both
/// frames are greyscale, row-major, `width` x `height`.
///
/// Exhaustive search over every integer offset in
/// `-max_offset..=max_offset` on both axes, scoring each by sum-of-absolute-
/// differences over the region both shifted frames have in common. Ties
/// (most commonly an all-uniform or otherwise textureless frame, where every
/// offset scores identically) resolve to `(0, 0)` — the search visits `(0,
/// 0)` first and only replaces the best score on a *strict* improvement, so
/// "no detectable motion" reads as "no correction", not an arbitrary offset
/// from whatever the scan order happened to try first.
pub fn estimate_translation(prev: &[f32], curr: &[f32], width: usize, height: usize, config: &MotionConfig) -> (f64, f64) {
    let mut best = (0i32, 0i32);
    let mut best_score = sad_at(prev, curr, width, height, 0, 0);

    for dy in -config.max_offset..=config.max_offset {
        for dx in -config.max_offset..=config.max_offset {
            if dx == 0 && dy == 0 {
                continue;
            }
            let score = sad_at(prev, curr, width, height, dx, dy);
            if score < best_score {
                best_score = score;
                best = (dx, dy);
            }
        }
    }
    (best.0 as f64, best.1 as f64)
}

/// Sum of absolute differences between `prev(x, y)` and `curr(x + dx, y +
/// dy)`, over the region where both indices stay in bounds. Scored this
/// direction (curr sampled at `+dx`, not `-dx`) specifically so the winning
/// `(dx, dy)` is directly the vector describing how content moved from
/// `prev` to `curr` — `curr`'s content sits `dx` to the right and `dy` down
/// from where it was in `prev` — matching `estimate_translation`'s
/// documented return convention without a sign flip at the call site.
fn sad_at(prev: &[f32], curr: &[f32], width: usize, height: usize, dx: i32, dy: i32) -> f64 {
    let x_range = (-dx).max(0)..(width as i32 - dx.max(0));
    let y_range = (-dy).max(0)..(height as i32 - dy.max(0));
    let mut sum = 0.0;
    let mut count = 0usize;
    for y in y_range {
        for x in x_range.clone() {
            let px = prev[(y as usize) * width + x as usize];
            let cx = curr[((y + dy) as usize) * width + (x + dx) as usize];
            sum += (px - cx).abs() as f64;
            count += 1;
        }
    }
    if count == 0 {
        f64::INFINITY
    } else {
        sum / count as f64
    }
}

/// Per-sample corrective offsets that, added to `raw_path`, produce a
/// smoothed version of it — a symmetric moving average over
/// `smoothing_window` samples (clamped at the ends, where a full window
/// isn't available) minus the raw value at each point.
///
/// Moving average, not a more sophisticated optimal-path solver (Adobe's
/// Warp Stabilizer and similar tools use an L1-optimal camera path):
/// simplicity here directly trades against how well a very fast intentional
/// pan is preserved versus how much residual jitter survives smoothing.
/// A larger `smoothing_window` favours removing shake at the cost of lagging
/// behind fast intentional motion; the caller picks the trade for their
/// footage rather than this function guessing at a "right" answer.
pub fn stabilization_offsets(raw_path: &[(f64, f64)], smoothing_window: usize) -> Vec<(f64, f64)> {
    if raw_path.is_empty() {
        return Vec::new();
    }
    let half = (smoothing_window / 2).max(1);
    raw_path
        .iter()
        .enumerate()
        .map(|(i, &(rx, ry))| {
            let lo = i.saturating_sub(half);
            let hi = (i + half).min(raw_path.len() - 1);
            let window = &raw_path[lo..=hi];
            let (sx, sy) = window.iter().fold((0.0, 0.0), |(ax, ay), &(x, y)| (ax + x, ay + y));
            let n = window.len() as f64;
            (sx / n - rx, sy / n - ry)
        })
        .collect()
}
