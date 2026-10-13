//! A convex outline in the XY plane: the shape of a 2D polygon body.
//!
//! Fixed-size and `Copy`, because a body's shape is rebuilt from its
//! `RigidBody` every step and both are plain values copied all over the solver.
//! Eight corners covers a character, a slope-sided crate, a boulder or a
//! rounded platform, and a sprite traced from its art reduces to that without
//! any corner sitting visibly off the picture.

/// The most corners an [`Outline`] keeps.
pub const OUTLINE_MAX: usize = 8;

/// A convex polygon, counter-clockwise, local to the body's centre.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Outline {
    pts: [[f32; 2]; OUTLINE_MAX],
    n: u8,
}

impl Outline {
    /// The convex hull of `points`, cut down to [`OUTLINE_MAX`] corners.
    ///
    /// The hull rather than the points as given, so any set of points makes a
    /// shape the solver can trust: a dent or a crossed edge would leave places
    /// where the inside and the outside disagree. Past eight corners, the
    /// corner whose removal loses the least area goes first, which takes the
    /// near-straight ones and keeps the silhouette. Empty when the points
    /// enclose no area (fewer than three, or all on one line).
    pub fn convex(points: &[[f32; 2]]) -> Self {
        let mut p: Vec<[f32; 2]> = points.iter().copied().filter(|q| q[0].is_finite() && q[1].is_finite()).collect();
        p.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        p.dedup();
        if p.len() < 3 {
            return Self::default();
        }
        // Andrew's monotone chain: lower hull then upper, both turning left.
        let mut hull: Vec<[f32; 2]> = Vec::with_capacity(p.len() * 2);
        let chain = |hull: &mut Vec<[f32; 2]>, q: [f32; 2], start: usize| {
            while hull.len() >= start + 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], q) <= 0.0 {
                hull.pop();
            }
            hull.push(q);
        };
        for &q in &p {
            chain(&mut hull, q, 0);
        }
        hull.pop();
        let start = hull.len();
        for &q in p.iter().rev() {
            chain(&mut hull, q, start);
        }
        hull.pop();
        // A long hull (a traced sprite's) is thinned cheaply first, a corner
        // at a time, taking whichever loses the least area.
        while hull.len() > 4 * OUTLINE_MAX {
            let n = hull.len();
            let area = |k: usize| cross(hull[(k + n - 1) % n], hull[k], hull[(k + 1) % n]).abs();
            let cut = (0..n).min_by(|&i, &j| area(i).total_cmp(&area(j))).unwrap_or(0);
            hull.remove(cut);
        }
        if hull.len() > OUTLINE_MAX {
            hull = largest_inscribed(&hull, OUTLINE_MAX);
        }
        let mut out = Self::default();
        if hull.len() < 3 || area2(&hull) <= 1e-9 {
            return out;
        }
        for (i, q) in hull.iter().enumerate() {
            out.pts[i] = *q;
        }
        out.n = hull.len() as u8;
        out
    }

    /// An axis-aligned rectangle of half-size `hx` × `hy`: the outline a box of
    /// the same size would have.
    pub fn rect(hx: f32, hy: f32) -> Self {
        Self::convex(&[[-hx, -hy], [hx, -hy], [hx, hy], [-hx, hy]])
    }

    /// The corners, counter-clockwise.
    pub fn points(&self) -> &[[f32; 2]] {
        &self.pts[..self.n as usize]
    }

    /// True when there is no outline at all.
    pub fn is_empty(&self) -> bool {
        self.n < 3
    }

    /// Stretched by `sx` along X and `sy` along Y, the way a node's scale
    /// stretches its sprite. A mirroring scale reverses the corner order so
    /// the outline stays counter-clockwise.
    pub fn scaled(&self, sx: f32, sy: f32) -> Self {
        let mut out = *self;
        for q in &mut out.pts[..self.n as usize] {
            *q = [q[0] * sx, q[1] * sy];
        }
        if sx * sy < 0.0 {
            out.pts[..self.n as usize].reverse();
        }
        out
    }

    /// Mirrored across the vertical line `x = cx` and/or the horizontal line
    /// `y = cy`, the way a flipped sprite mirrors its picture.
    pub fn mirrored(&self, flip_x: Option<f32>, flip_y: Option<f32>) -> Self {
        let mut out = *self;
        for q in &mut out.pts[..self.n as usize] {
            if let Some(cx) = flip_x {
                q[0] = 2.0 * cx - q[0];
            }
            if let Some(cy) = flip_y {
                q[1] = 2.0 * cy - q[1];
            }
        }
        if flip_x.is_some() != flip_y.is_some() {
            out.pts[..self.n as usize].reverse();
        }
        out
    }

    /// Signed distance from `(x, y)` to the outline: negative inside.
    pub fn distance(&self, x: f32, y: f32) -> f32 {
        let pts = self.points();
        if pts.len() < 3 {
            return f32::MAX;
        }
        let mut d2 = f32::MAX;
        let mut inside = true;
        for i in 0..pts.len() {
            let a = pts[i];
            let b = pts[(i + 1) % pts.len()];
            let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
            let (wx, wy) = (x - a[0], y - a[1]);
            let t = ((wx * ex + wy * ey) / (ex * ex + ey * ey).max(1e-12)).clamp(0.0, 1.0);
            let (dx, dy) = (wx - ex * t, wy - ey * t);
            d2 = d2.min(dx * dx + dy * dy);
            // Counter-clockwise, so inside is left of every edge.
            if ex * wy - ey * wx < 0.0 {
                inside = false;
            }
        }
        let d = d2.sqrt();
        if inside { -d } else { d }
    }

    /// Half the size of the box around the outline, measured from the centre
    /// the outline is local to.
    pub fn half_extents(&self) -> [f32; 2] {
        self.points()
            .iter()
            .fold([0.0f32, 0.0], |m, q| [m[0].max(q[0].abs()), m[1].max(q[1].abs())])
    }

    /// The middle of the outline's area.
    pub fn centroid(&self) -> [f32; 2] {
        let p = self.points();
        let (mut a, mut cx, mut cy) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..p.len() {
            let (q, r) = (p[i], p[(i + 1) % p.len()]);
            let c = q[0] * r[1] - r[0] * q[1];
            a += c;
            cx += (q[0] + r[0]) * c;
            cy += (q[1] + r[1]) * c;
        }
        if a.abs() < 1e-12 {
            return [0.0, 0.0];
        }
        [cx / (3.0 * a), cy / (3.0 * a)]
    }

    /// The area's second moment about its centroid, divided by the area:
    /// times a mass, the moment of inertia of a flat plate this shape about
    /// the axis through its middle, out of the plane.
    pub fn polar_moment_per_area(&self) -> f32 {
        let p = self.points();
        let [gx, gy] = self.centroid();
        let (mut a, mut j) = (0.0f32, 0.0f32);
        for i in 0..p.len() {
            let (q, r) = ([p[i][0] - gx, p[i][1] - gy], [p[(i + 1) % p.len()][0] - gx, p[(i + 1) % p.len()][1] - gy]);
            let c = q[0] * r[1] - r[0] * q[1];
            a += c;
            j += c * (q[0] * q[0] + q[0] * r[0] + r[0] * r[0] + q[1] * q[1] + q[1] * r[1] + r[1] * r[1]);
        }
        if a.abs() < 1e-12 {
            return 0.0;
        }
        // The sums are twice the area and twelve times the moment.
        (j / 12.0) / (a / 2.0)
    }

    /// The farthest any corner is from the centre.
    pub fn reach(&self) -> f32 {
        self.points().iter().map(|q| q[0].hypot(q[1])).fold(0.0, f32::max)
    }
}

/// The `k` corners of convex `hull` that enclose the most area, in order.
///
/// Exact rather than greedy: dropping one least-loss corner at a time leaves
/// the survivors bunched wherever the ties happened to fall, which on a round
/// shape left one side a seventh of the radius inside the curve. For each
/// starting corner, `best[c][j]` is the largest fan from that start to corner
/// `j` through `c` more corners.
fn largest_inscribed(hull: &[[f32; 2]], k: usize) -> Vec<[f32; 2]> {
    let n = hull.len();
    let mut best_area = f32::MIN;
    let mut best_pick: Vec<usize> = (0..k).collect();
    for s in 0..n {
        let at = |i: usize| hull[(s + i) % n];
        // best[c][j]: the fan's area with corner j as its c-th corner after
        // the start; from[c][j] is the corner before it.
        let mut best = vec![vec![f32::MIN; n]; k];
        let mut from = vec![vec![0usize; n]; k];
        for b in &mut best[1][1..] {
            *b = 0.0;
        }
        for c in 2..k {
            for j in c..n {
                for i in (c - 1)..j {
                    if best[c - 1][i] == f32::MIN {
                        continue;
                    }
                    let a = best[c - 1][i] + cross(at(0), at(i), at(j));
                    if a > best[c][j] {
                        best[c][j] = a;
                        from[c][j] = i;
                    }
                }
            }
        }
        for (j, &area) in best[k - 1].iter().enumerate().skip(k - 1) {
            if area > best_area {
                best_area = area;
                let mut pick = vec![0usize; k];
                let mut cur = j;
                for c in (1..k).rev() {
                    pick[c] = cur;
                    cur = from[c][cur];
                }
                best_pick = pick.iter().map(|i| (s + i) % n).collect();
            }
        }
    }
    best_pick.sort_unstable();
    best_pick.into_iter().map(|i| hull[i]).collect()
}

/// Twice the signed area of triangle `a b c`: positive when it turns left.
fn cross(a: [f32; 2], b: [f32; 2], c: [f32; 2]) -> f32 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

fn area2(p: &[[f32; 2]]) -> f32 {
    (0..p.len()).map(|i| {
        let (a, b) = (p[i], p[(i + 1) % p.len()]);
        a[0] * b[1] - b[0] * a[1]
    }).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hull_drops_the_points_inside_it_and_runs_counter_clockwise() {
        let o = Outline::convex(&[[0.0, 0.0], [2.0, 0.0], [1.0, 0.5], [2.0, 2.0], [0.0, 2.0]]);
        assert_eq!(o.points().len(), 4);
        assert!(area2(o.points()) > 0.0);
        assert!(o.distance(1.0, 1.0) < -0.9);
        assert!((o.distance(3.0, 1.0) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_circle_of_points_keeps_eight_corners_spread_round_it() {
        let ring: Vec<[f32; 2]> = (0..40)
            .map(|i| {
                let a = i as f32 / 40.0 * std::f32::consts::TAU;
                [a.cos(), a.sin()]
            })
            .collect();
        let o = Outline::convex(&ring);
        assert_eq!(o.points().len(), OUTLINE_MAX);
        // As even as a regular octagon, whose edges dip 1 − cos 22.5° = 0.076
        // inside the circle.
        for i in 0..16 {
            let a = i as f32 / 16.0 * std::f32::consts::TAU;
            let d = o.distance(a.cos(), a.sin());
            assert!(d < 0.08, "a gap at {a}: {d}");
        }
    }

    /// A 2×1 rectangle's middle and its plate moment, (w² + h²) / 12 per unit
    /// mass, wherever the rectangle sits.
    #[test]
    fn a_rectangle_knows_its_middle_and_its_moment() {
        let o = Outline::convex(&[[1.0, 0.0], [3.0, 0.0], [3.0, 1.0], [1.0, 1.0]]);
        assert_eq!(o.centroid(), [2.0, 0.5]);
        assert!((o.polar_moment_per_area() - 5.0 / 12.0).abs() < 1e-5, "{}", o.polar_moment_per_area());
    }

    #[test]
    fn nothing_on_a_line_is_an_outline() {
        assert!(Outline::convex(&[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0]]).is_empty());
        assert!(Outline::convex(&[[0.0, 0.0], [1.0, 1.0]]).is_empty());
    }

    #[test]
    fn a_mirror_keeps_the_inside_inside() {
        let o = Outline::convex(&[[0.0, 0.0], [2.0, 0.0], [0.0, 1.0]]).mirrored(Some(0.0), None);
        assert!(o.distance(-0.5, 0.25) < 0.0);
        assert!(o.distance(0.5, 0.25) > 0.0);
        let both = Outline::convex(&[[0.0, 0.0], [2.0, 0.0], [0.0, 1.0]]).mirrored(Some(1.0), Some(0.0));
        assert!(both.distance(1.5, -0.25) < 0.0);
    }

    #[test]
    fn a_mirroring_scale_keeps_the_inside_inside() {
        let o = Outline::convex(&[[0.0, 0.0], [2.0, 0.0], [0.0, 1.0]]).scaled(-1.0, 1.0);
        assert!(o.distance(-0.5, 0.25) < 0.0);
        assert!(o.distance(0.5, 0.25) > 0.0);
    }
}
