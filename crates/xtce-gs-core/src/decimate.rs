//! Fitting more points than there are pixels onto an axis.
//!
//! A watched parameter keeps thousands of points and a plot is a thousand pixels wide. Handing
//! all of them to a plot widget means tessellating a polyline with one segment per point, on
//! the processor, every frame — which is where an immediate-mode interface stops being free.
//!
//! # Why not averaging
//!
//! An operator watches telemetry for the sample that does not belong: a single-sample spike on
//! a current monitor is a latch-up, not noise. Averaging a bucket removes exactly that, and a
//! min/max envelope keeps the spike but throws away the shape between spikes, so a slow drift
//! reads as a band. Largest-Triangle-Three-Buckets keeps the point in each bucket that
//! contributes most to the *visible* shape, which is the one a human would have kept.
//!
//! # On the x axis
//!
//! Times here are seconds since the Unix epoch — around 1.8e9 — and the areas below are
//! computed from differences of a few milliseconds. Subtracting the first point's time before
//! any arithmetic keeps sixteen digits of significance for the difference instead of seven.

use crate::store::Point;

/// Reduces `points` to at most `threshold` points, keeping the shape of the curve.
///
/// Largest-Triangle-Three-Buckets, Steinarsson 2013. The first and last points are always
/// kept, so the ends of a plotted line never move as the operator zooms.
///
/// `out` is cleared first and reused, because this runs once per plot per frame and an
/// allocation there is an allocation sixty times a second.
pub fn lttb(points: &[Point], threshold: usize, out: &mut Vec<Point>) {
    out.clear();

    if threshold >= points.len() || threshold < 3 {
        // Fewer points than the budget, or a budget too small for the algorithm to mean
        // anything: hand back what came in. A caller asking for two points of a thousand is
        // asking for the ends, and gets them below.
        if threshold >= points.len() {
            out.extend_from_slice(points);
        } else if !points.is_empty() {
            if let Some(first) = points.first() {
                out.push(*first);
            }
            if points.len() > 1
                && let Some(last) = points.last()
            {
                out.push(*last);
            }
        }
        return;
    }

    let origin = points.first().map_or(0.0, |p| p.t);
    let n = points.len();
    // Every bucket but the first and the last holds this many points.
    let every = (n - 2) as f64 / (threshold - 2) as f64;

    let Some(first) = points.first() else { return };
    out.push(*first);

    let mut anchor = 0usize; // the point already chosen, one corner of every triangle

    for i in 0..threshold - 2 {
        // The bucket after the one being chosen from, averaged into the third corner.
        let next_start = ((i + 1) as f64 * every) as usize + 1;
        let next_end = (((i + 2) as f64 * every) as usize + 1).min(n);
        let (mut sum_t, mut sum_v, mut count) = (0.0f64, 0.0f64, 0usize);
        for point in points.get(next_start..next_end).unwrap_or_default() {
            sum_t += point.t - origin;
            sum_v += point.v;
            count += 1;
        }
        let (avg_t, avg_v) = if count == 0 {
            // The last bucket can be empty when the points do not divide evenly; the final
            // point stands in for it.
            points.last().map_or((0.0, 0.0), |p| (p.t - origin, p.v))
        } else {
            (sum_t / count as f64, sum_v / count as f64)
        };

        let start = (i as f64 * every) as usize + 1;
        let end = (((i + 1) as f64 * every) as usize + 1).min(n);
        let Some(anchor_point) = points.get(anchor) else {
            break;
        };
        let anchor_t = anchor_point.t - origin;
        let anchor_v = anchor_point.v;

        let mut best_area = -1.0f64;
        let mut best = start;
        for (offset, point) in points
            .get(start..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            // Twice the area of the triangle (anchor, candidate, next-bucket average). The
            // factor of two is constant across candidates, so it is not divided out.
            let area = ((anchor_t - avg_t) * (point.v - anchor_v)
                - (anchor_t - (point.t - origin)) * (avg_v - anchor_v))
                .abs();
            if area > best_area {
                best_area = area;
                best = start + offset;
            }
        }

        if let Some(point) = points.get(best) {
            out.push(*point);
            anchor = best;
        }
    }

    if let Some(last) = points.last() {
        out.push(*last);
    }
}

/// `MinMax` pre-selection followed by LTTB.
///
/// LTTB is O(n) but reads every point; over a long history that is the cost, not the
/// tessellation. `MinMaxLTTB` (Van Der Donckt 2023) first keeps only the extremes of `ratio`
/// times as many buckets as the final budget — a cheap pass that cannot lose a spike, since a
/// spike *is* an extreme — and runs LTTB over that. With `ratio = 4` the result is visually
/// indistinguishable from LTTB over everything at a fraction of the reads.
pub fn min_max_lttb(points: &[Point], threshold: usize, ratio: usize, out: &mut Vec<Point>) {
    let ratio = ratio.max(1);
    let preselect = threshold.saturating_mul(ratio);
    if threshold >= points.len() || preselect >= points.len() || threshold < 3 {
        lttb(points, threshold, out);
        return;
    }

    let mut reduced = Vec::with_capacity(preselect + 2);
    let buckets = threshold.saturating_sub(2).max(1) * ratio;
    let every = points.len() as f64 / buckets as f64;

    if let Some(first) = points.first() {
        reduced.push(*first);
    }
    for bucket in 0..buckets {
        let start = (bucket as f64 * every) as usize;
        let end = (((bucket + 1) as f64 * every) as usize).min(points.len());
        let slice = points.get(start..end).unwrap_or_default();
        let (mut lo, mut hi): (Option<usize>, Option<usize>) = (None, None);
        for (offset, point) in slice.iter().enumerate() {
            if lo.is_none_or(|at| slice.get(at).is_some_and(|p| point.v < p.v)) {
                lo = Some(offset);
            }
            if hi.is_none_or(|at| slice.get(at).is_some_and(|p| point.v > p.v)) {
                hi = Some(offset);
            }
        }
        // In time order, so the reduced series is still monotonic in x.
        let (first, second) = match (lo, hi) {
            (Some(a), Some(b)) if a > b => (Some(b), Some(a)),
            pair => pair,
        };
        if let Some(offset) = first
            && let Some(point) = slice.get(offset)
        {
            reduced.push(*point);
        }
        // A bucket whose minimum and maximum are the same sample contributes it once.
        if let Some(offset) = second
            && second != first
            && let Some(point) = slice.get(offset)
        {
            reduced.push(*point);
        }
    }
    if let Some(last) = points.last() {
        reduced.push(*last);
    }

    lttb(&reduced, threshold, out);
}

/// Ramer-Douglas-Peucker simplification with a tolerance in axis units.
///
/// Kept because it is the right algorithm for a *static* view — an exported plot, a report —
/// where the output size does not matter but the guarantee does: no retained point is further
/// than `epsilon` from the simplified line. LTTB gives a fixed output size and no such bound,
/// which is what a live plot wants and a printed one does not.
///
/// Iterative rather than recursive: a pathological series would otherwise put a stack overflow
/// on the drawing thread, and a stack overflow is not a `Result`.
pub fn rdp(points: &[Point], epsilon: f64, out: &mut Vec<Point>) {
    out.clear();
    if points.len() < 3 {
        out.extend_from_slice(points);
        return;
    }

    let origin = points.first().map_or(0.0, |p| p.t);
    let mut keep = vec![false; points.len()];
    if let Some(slot) = keep.first_mut() {
        *slot = true;
    }
    if let Some(slot) = keep.last_mut() {
        *slot = true;
    }

    let mut stack = vec![(0usize, points.len() - 1)];
    while let Some((start, end)) = stack.pop() {
        if end <= start + 1 {
            continue;
        }
        let (Some(a), Some(b)) = (points.get(start), points.get(end)) else {
            continue;
        };
        let (ax, ay) = (a.t - origin, a.v);
        let (bx, by) = (b.t - origin, b.v);
        let (dx, dy) = (bx - ax, by - ay);
        let norm = dx.hypot(dy);

        let mut worst = 0.0f64;
        let mut worst_at = start;
        for (offset, point) in points
            .get(start + 1..end)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            let (px, py) = (point.t - origin, point.v);
            let distance = if norm == 0.0 {
                (px - ax).hypot(py - ay)
            } else {
                ((bx - ax) * (ay - py) - (ax - px) * (by - ay)).abs() / norm
            };
            if distance > worst {
                worst = distance;
                worst_at = start + 1 + offset;
            }
        }

        if worst > epsilon {
            if let Some(slot) = keep.get_mut(worst_at) {
                *slot = true;
            }
            stack.push((start, worst_at));
            stack.push((worst_at, end));
        }
    }

    for (point, kept) in points.iter().zip(keep) {
        if kept {
            out.push(*point);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series(values: &[f64]) -> Vec<Point> {
        values
            .iter()
            .enumerate()
            .map(|(i, v)| Point {
                // A realistic axis: seconds since the epoch, not zero-based.
                t: 1_757_000_000.0 + i as f64 * 0.1,
                v: *v,
            })
            .collect()
    }

    #[test]
    fn fewer_points_than_the_budget_pass_through() {
        let points = series(&[1.0, 2.0, 3.0]);
        let mut out = Vec::new();
        lttb(&points, 10, &mut out);
        assert_eq!(out, points);
    }

    #[test]
    fn the_ends_are_always_kept() {
        let points = series(&(0..1000).map(|i| f64::from(i % 7)).collect::<Vec<_>>());
        let mut out = Vec::new();
        lttb(&points, 50, &mut out);
        assert_eq!(out.first(), points.first());
        assert_eq!(out.last(), points.last());
        assert_eq!(out.len(), 50);
    }

    #[test]
    fn a_one_sample_spike_survives() {
        // The failure this algorithm is chosen to avoid: a flat line with one outlier, reduced
        // to a hundred points. An averaging filter loses the spike entirely.
        let mut values = vec![0.0; 5000];
        if let Some(slot) = values.get_mut(3111) {
            *slot = 42.0;
        }
        let points = series(&values);
        let mut out = Vec::new();
        lttb(&points, 100, &mut out);
        assert!(
            out.iter().any(|p| p.v == 42.0),
            "LTTB dropped the only feature in the series"
        );
    }

    #[test]
    fn min_max_lttb_keeps_the_spike_too() {
        let mut values = vec![0.0; 50_000];
        if let Some(slot) = values.get_mut(31_111) {
            *slot = -17.0;
        }
        let points = series(&values);
        let mut out = Vec::new();
        min_max_lttb(&points, 200, 4, &mut out);
        assert!(out.iter().any(|p| p.v == -17.0));
        assert!(out.len() <= 200);
    }

    #[test]
    fn the_output_stays_in_time_order() {
        let values: Vec<f64> = (0..2000).map(|i| (f64::from(i) / 37.0).sin()).collect();
        let points = series(&values);
        for (threshold, ratio) in [(10, 4), (137, 2), (999, 8)] {
            let mut out = Vec::new();
            min_max_lttb(&points, threshold, ratio, &mut out);
            assert!(
                out.windows(2).all(|w| w[0].t <= w[1].t),
                "decimation reordered the x axis at threshold {threshold}"
            );
        }
    }

    #[test]
    fn rdp_keeps_every_point_within_its_tolerance() {
        let values: Vec<f64> = (0..500)
            .map(|i| (f64::from(i) / 50.0).sin() * 10.0)
            .collect();
        let points = series(&values);
        let mut out = Vec::new();
        rdp(&points, 0.01, &mut out);
        assert!(out.len() < points.len());
        assert_eq!(out.first(), points.first());
        assert_eq!(out.last(), points.last());

        // Nothing dropped is further than epsilon from the line that replaced it. Checked the
        // slow way, against the retained polyline, because that is the claim.
        let origin = points[0].t;
        for point in &points {
            let (px, py) = (point.t - origin, point.v);
            let mut best = f64::INFINITY;
            for pair in out.windows(2) {
                let (ax, ay) = (pair[0].t - origin, pair[0].v);
                let (bx, by) = (pair[1].t - origin, pair[1].v);
                if px < ax - 1e-9 || px > bx + 1e-9 {
                    continue;
                }
                let norm = (bx - ax).hypot(by - ay);
                let distance = if norm == 0.0 {
                    (px - ax).hypot(py - ay)
                } else {
                    ((bx - ax) * (ay - py) - (ax - px) * (by - ay)).abs() / norm
                };
                best = best.min(distance);
            }
            assert!(
                best <= 0.01 + 1e-9,
                "point at {px} is {best} from the simplification"
            );
        }
    }

    #[test]
    fn an_empty_series_decimates_to_nothing() {
        let mut out = vec![Point { t: 1.0, v: 1.0 }];
        lttb(&[], 100, &mut out);
        assert!(out.is_empty());
        rdp(&[], 1.0, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn a_budget_below_three_gives_the_ends() {
        let points = series(&[1.0, 2.0, 3.0, 4.0, 5.0]);
        let mut out = Vec::new();
        lttb(&points, 2, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out.first(), points.first());
        assert_eq!(out.last(), points.last());
    }
}
