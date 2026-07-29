//! Vector layers: paths, fills, strokes, and the anti-aliased rasterizer that
//! turns them into pixels at composite time.
//!
//! Deliberately Scratch-shaped (proposal §8): a path is a ring of nodes, each
//! either a **corner** or a **curve**, and the primary interaction is *reshape* —
//! drag a node and the shape follows. Bezier handles exist and are draggable, but
//! a curve node with no handles derives smooth ones from its neighbours, so you
//! can draw a rounded blob without ever learning what a handle is.
//!
//! The rasterizer is in-house rather than a third-party one (the proposal's open
//! decision 3): a scanline filler with 4× vertical supersampling and exact
//! horizontal span coverage, nonzero or even-odd winding, and a stroker that
//! emits consistently-wound convex pieces so overlapping segments union for free
//! under nonzero. `vector.rs` is the only module that would change if that call
//! is ever revisited.

use serde::{Deserialize, Serialize};

use crate::Rect;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum NodeKind {
    /// A sharp point — the segments either side are straight unless handles are set.
    #[default]
    Corner,
    /// A smooth point — handles derive from the neighbours when left at zero.
    Curve,
}

/// One node of a path. Handles are **offsets from `p`**, in layer pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default, Serialize, Deserialize)]
pub struct VNode {
    pub p: [f32; 2],
    pub kind: NodeKind,
    pub h_in: [f32; 2],
    pub h_out: [f32; 2],
}

impl VNode {
    pub fn corner(x: f32, y: f32) -> Self {
        VNode { p: [x, y], kind: NodeKind::Corner, ..Default::default() }
    }
    pub fn curve(x: f32, y: f32) -> Self {
        VNode { p: [x, y], kind: NodeKind::Curve, ..Default::default() }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Cap {
    #[default]
    Butt,
    Round,
    Square,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum Join {
    #[default]
    Round,
    Miter,
    Bevel,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Stroke {
    pub color: [u8; 4],
    pub width: f32,
    #[serde(default)]
    pub cap: Cap,
    #[serde(default)]
    pub join: Join,
}

impl Default for Stroke {
    fn default() -> Self {
        Stroke { color: [20, 20, 24, 255], width: 2.0, cap: Cap::Round, join: Join::Round }
    }
}

/// How a fill is coloured.
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum Paint {
    Solid([u8; 4]),
    /// Linear ramp from `a` to `b`, `stops` at 0..1 along it.
    Linear { a: [f32; 2], b: [f32; 2], stops: Vec<(f32, [u8; 4])> },
    /// Radial ramp from `c` outward to `r`.
    Radial { c: [f32; 2], r: f32, stops: Vec<(f32, [u8; 4])> },
}

impl Paint {
    pub fn sample(&self, x: f32, y: f32) -> [u8; 4] {
        match self {
            Paint::Solid(c) => *c,
            Paint::Linear { a, b, stops } => {
                let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
                let len2 = dx * dx + dy * dy;
                let t = if len2 <= 1e-6 {
                    0.0
                } else {
                    ((x - a[0]) * dx + (y - a[1]) * dy) / len2
                };
                sample_stops(stops, t)
            }
            Paint::Radial { c, r, stops } => {
                let d = ((x - c[0]).powi(2) + (y - c[1]).powi(2)).sqrt();
                sample_stops(stops, if *r <= 1e-6 { 0.0 } else { d / r })
            }
        }
    }
}

/// Interpolate a stop list at `t` (clamped). An empty list is transparent.
pub fn sample_stops(stops: &[(f32, [u8; 4])], t: f32) -> [u8; 4] {
    if stops.is_empty() {
        return [0, 0, 0, 0];
    }
    let t = t.clamp(0.0, 1.0);
    if t <= stops[0].0 {
        return stops[0].1;
    }
    if t >= stops[stops.len() - 1].0 {
        return stops[stops.len() - 1].1;
    }
    for w in stops.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t >= t0 && t <= t1 {
            let f = if (t1 - t0).abs() < 1e-6 { 0.0 } else { (t - t0) / (t1 - t0) };
            let mut out = [0u8; 4];
            for i in 0..4 {
                out[i] = crate::u8c(c0[i] as f32 + (c1[i] as f32 - c0[i] as f32) * f);
            }
            return out;
        }
    }
    stops[stops.len() - 1].1
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct VPath {
    pub nodes: Vec<VNode>,
    pub closed: bool,
    pub fill: Option<Paint>,
    pub stroke: Option<Stroke>,
    /// Even-odd instead of nonzero winding (donut holes from one ring).
    #[serde(default)]
    pub even_odd: bool,
}

impl Default for VPath {
    fn default() -> Self {
        VPath {
            nodes: Vec::new(),
            closed: true,
            fill: Some(Paint::Solid([220, 220, 230, 255])),
            stroke: None,
            even_odd: false,
        }
    }
}

impl VPath {
    /// A rectangle as four corner nodes.
    pub fn rect(x: f32, y: f32, w: f32, h: f32) -> Self {
        VPath {
            nodes: vec![
                VNode::corner(x, y),
                VNode::corner(x + w, y),
                VNode::corner(x + w, y + h),
                VNode::corner(x, y + h),
            ],
            ..Default::default()
        }
    }

    /// An ellipse as four curve nodes (handles derived — the Scratch default).
    pub fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> Self {
        // 4/3*(sqrt(2)-1) is the classic circle-from-cubics constant.
        let k = 0.5522847;
        let mut nodes = vec![
            VNode::curve(cx, cy - ry),
            VNode::curve(cx + rx, cy),
            VNode::curve(cx, cy + ry),
            VNode::curve(cx - rx, cy),
        ];
        nodes[0].h_out = [rx * k, 0.0];
        nodes[0].h_in = [-rx * k, 0.0];
        nodes[1].h_out = [0.0, ry * k];
        nodes[1].h_in = [0.0, -ry * k];
        nodes[2].h_out = [-rx * k, 0.0];
        nodes[2].h_in = [rx * k, 0.0];
        nodes[3].h_out = [0.0, -ry * k];
        nodes[3].h_in = [0.0, ry * k];
        VPath { nodes, ..Default::default() }
    }

    /// A regular polygon with `sides` corners.
    pub fn polygon(cx: f32, cy: f32, r: f32, sides: usize) -> Self {
        let n = sides.max(3);
        let nodes = (0..n)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::TAU / n as f32;
                VNode::corner(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        VPath { nodes, ..Default::default() }
    }

    /// A straight open segment (the Line tool's vector form).
    pub fn line(x0: f32, y0: f32, x1: f32, y1: f32, stroke: Stroke) -> Self {
        VPath {
            nodes: vec![VNode::corner(x0, y0), VNode::corner(x1, y1)],
            closed: false,
            fill: None,
            stroke: Some(stroke),
            even_odd: false,
        }
    }

    /// The effective outgoing handle of node `i` (derived when a curve node's is zero).
    fn h_out(&self, i: usize) -> [f32; 2] {
        let n = &self.nodes[i];
        if n.kind == NodeKind::Corner {
            return [0.0, 0.0];
        }
        if n.h_out != [0.0, 0.0] {
            return n.h_out;
        }
        self.derived(i, false)
    }

    fn h_in(&self, i: usize) -> [f32; 2] {
        let n = &self.nodes[i];
        if n.kind == NodeKind::Corner {
            return [0.0, 0.0];
        }
        if n.h_in != [0.0, 0.0] {
            return n.h_in;
        }
        self.derived(i, true)
    }

    /// Catmull-Rom-ish auto handles: a third of the vector between the neighbours.
    fn derived(&self, i: usize, incoming: bool) -> [f32; 2] {
        let n = self.nodes.len();
        if n < 2 {
            return [0.0, 0.0];
        }
        let prev = if i == 0 {
            if self.closed { self.nodes[n - 1].p } else { self.nodes[i].p }
        } else {
            self.nodes[i - 1].p
        };
        let next = if i + 1 >= n {
            if self.closed { self.nodes[0].p } else { self.nodes[i].p }
        } else {
            self.nodes[i + 1].p
        };
        let t = [(next[0] - prev[0]) / 3.0, (next[1] - prev[1]) / 3.0];
        if incoming { [-t[0], -t[1]] } else { t }
    }

    /// Flatten to a polyline in layer pixels. Closed paths repeat no point.
    pub fn flatten(&self) -> Vec<(f32, f32)> {
        let n = self.nodes.len();
        let mut out: Vec<(f32, f32)> = Vec::new();
        if n == 0 {
            return out;
        }
        if n == 1 {
            out.push((self.nodes[0].p[0], self.nodes[0].p[1]));
            return out;
        }
        let last = if self.closed { n } else { n - 1 };
        for i in 0..last {
            let j = (i + 1) % n;
            let p0 = self.nodes[i].p;
            let p3 = self.nodes[j].p;
            let ho = self.h_out(i);
            let hi = self.h_in(j);
            let p1 = [p0[0] + ho[0], p0[1] + ho[1]];
            let p2 = [p3[0] + hi[0], p3[1] + hi[1]];
            if ho == [0.0, 0.0] && hi == [0.0, 0.0] {
                out.push((p0[0], p0[1]));
                continue;
            }
            let chord = ((p3[0] - p0[0]).powi(2) + (p3[1] - p0[1]).powi(2)).sqrt()
                + ((p1[0] - p0[0]).powi(2) + (p1[1] - p0[1]).powi(2)).sqrt()
                + ((p2[0] - p3[0]).powi(2) + (p2[1] - p3[1]).powi(2)).sqrt();
            let steps = ((chord / 3.0).ceil() as usize).clamp(6, 96);
            for s in 0..steps {
                let t = s as f32 / steps as f32;
                out.push(cubic(p0, p1, p2, p3, t));
            }
        }
        if !self.closed {
            let l = self.nodes[n - 1].p;
            out.push((l[0], l[1]));
        }
        out
    }

    /// Bounding box in layer pixels, stroke width included.
    pub fn bounds(&self) -> Rect {
        let pts = self.flatten();
        if pts.is_empty() {
            return Rect::EMPTY;
        }
        let pad = self.stroke.as_ref().map_or(0.0, |s| s.width) * 0.5 + 2.0;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in pts {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
        Rect::from_points(
            (x0 - pad).floor() as i32,
            (y0 - pad).floor() as i32,
            (x1 + pad).ceil() as i32,
            (y1 + pad).ceil() as i32,
        )
    }

    /// Move every node by (dx, dy).
    pub fn translate(&mut self, dx: f32, dy: f32) {
        for n in &mut self.nodes {
            n.p[0] += dx;
            n.p[1] += dy;
        }
        match &mut self.fill {
            Some(Paint::Linear { a, b, .. }) => {
                a[0] += dx;
                a[1] += dy;
                b[0] += dx;
                b[1] += dy;
            }
            Some(Paint::Radial { c, .. }) => {
                c[0] += dx;
                c[1] += dy;
            }
            _ => {}
        }
    }

    /// Index of the node within `tol` pixels of (x, y), nearest first.
    pub fn hit_node(&self, x: f32, y: f32, tol: f32) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            let d = ((n.p[0] - x).powi(2) + (n.p[1] - y).powi(2)).sqrt();
            if d <= tol && best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| i)
    }

    /// The segment (node index it starts at) whose flattened curve passes within
    /// `tol` of the point, plus the 0..1 position along that segment.
    pub fn hit_segment(&self, x: f32, y: f32, tol: f32) -> Option<(usize, f32)> {
        let n = self.nodes.len();
        if n < 2 {
            return None;
        }
        let last = if self.closed { n } else { n - 1 };
        let mut best: Option<(f32, usize, f32)> = None;
        for i in 0..last {
            let j = (i + 1) % n;
            let p0 = self.nodes[i].p;
            let p3 = self.nodes[j].p;
            let ho = self.h_out(i);
            let hi = self.h_in(j);
            let p1 = [p0[0] + ho[0], p0[1] + ho[1]];
            let p2 = [p3[0] + hi[0], p3[1] + hi[1]];
            const STEPS: usize = 32;
            for s in 0..=STEPS {
                let t = s as f32 / STEPS as f32;
                let (px, py) = cubic(p0, p1, p2, p3, t);
                let d = ((px - x).powi(2) + (py - y).powi(2)).sqrt();
                if d <= tol && best.is_none_or(|(bd, _, _)| d < bd) {
                    best = Some((d, i, t));
                }
            }
        }
        best.map(|(_, i, t)| (i, t))
    }

    /// Insert a node on segment `seg` at parameter `t` — click an edge to add a point.
    pub fn insert_node(&mut self, seg: usize, t: f32) -> usize {
        let n = self.nodes.len();
        if seg >= n {
            return seg;
        }
        let j = (seg + 1) % n;
        let p0 = self.nodes[seg].p;
        let p3 = self.nodes[j].p;
        let ho = self.h_out(seg);
        let hi = self.h_in(j);
        let p1 = [p0[0] + ho[0], p0[1] + ho[1]];
        let p2 = [p3[0] + hi[0], p3[1] + hi[1]];
        let (x, y) = cubic(p0, p1, p2, p3, t);
        let smooth = ho != [0.0, 0.0] || hi != [0.0, 0.0];
        let mut node = if smooth { VNode::curve(x, y) } else { VNode::corner(x, y) };
        if smooth {
            // Tangent at t, scaled to a third of the segment — visually continuous.
            let (tx, ty) = cubic_tangent(p0, p1, p2, p3, t);
            let len = (tx * tx + ty * ty).sqrt().max(1e-4);
            let s = len / 3.0 / len;
            node.h_out = [tx * s, ty * s];
            node.h_in = [-tx * s, -ty * s];
        }
        self.nodes.insert(seg + 1, node);
        seg + 1
    }
}

fn cubic(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p0[0] + b * p1[0] + c * p2[0] + d * p3[0],
        a * p0[1] + b * p1[1] + c * p2[1] + d * p3[1],
    )
}

fn cubic_tangent(p0: [f32; 2], p1: [f32; 2], p2: [f32; 2], p3: [f32; 2], t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, c) = (3.0 * u * u, 6.0 * u * t, 3.0 * t * t);
    (
        a * (p1[0] - p0[0]) + b * (p2[0] - p1[0]) + c * (p3[0] - p2[0]),
        a * (p1[1] - p0[1]) + b * (p2[1] - p1[1]) + c * (p3[1] - p2[1]),
    )
}

// --- the rasterizer ------------------------------------------------------

/// Sub-scanlines per pixel row. 4 is the sweet spot between quality and cost:
/// horizontal coverage is exact, so only near-horizontal edges see the stepping.
const SUB: usize = 4;

/// Accumulate polygon coverage (0..1 per pixel) into `cov`, a `w*h` buffer.
/// `rings` are closed polylines in pixel space; `even_odd` picks the fill rule.
pub fn coverage(rings: &[Vec<(f32, f32)>], w: u32, h: u32, even_odd: bool, cov: &mut [f32]) {
    // Edge list: (y0, y1, x-at-y0, dx/dy, winding).
    struct Edge {
        y0: f32,
        y1: f32,
        x0: f32,
        slope: f32,
        dir: i32,
    }
    let mut edges: Vec<Edge> = Vec::new();
    let mut min_y = f32::MAX;
    let mut max_y = f32::MIN;
    for ring in rings {
        if ring.len() < 2 {
            continue;
        }
        for i in 0..ring.len() {
            let (ax, ay) = ring[i];
            let (bx, by) = ring[(i + 1) % ring.len()];
            if (ay - by).abs() < 1e-9 {
                continue;
            }
            let (top, bot, dir) = if ay < by { ((ax, ay), (bx, by), 1) } else { ((bx, by), (ax, ay), -1) };
            edges.push(Edge {
                y0: top.1,
                y1: bot.1,
                x0: top.0,
                slope: (bot.0 - top.0) / (bot.1 - top.1),
                dir,
            });
            min_y = min_y.min(top.1);
            max_y = max_y.max(bot.1);
        }
    }
    if edges.is_empty() {
        return;
    }
    let y_start = (min_y.floor().max(0.0)) as i32;
    let y_end = (max_y.ceil().min(h as f32)) as i32;
    let mut xs: Vec<(f32, i32)> = Vec::new();
    let inv = 1.0 / SUB as f32;
    for y in y_start..y_end {
        for s in 0..SUB {
            let sy = y as f32 + (s as f32 + 0.5) * inv;
            xs.clear();
            for e in &edges {
                if sy >= e.y0 && sy < e.y1 {
                    xs.push((e.x0 + (sy - e.y0) * e.slope, e.dir));
                }
            }
            if xs.len() < 2 {
                continue;
            }
            xs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let mut wind = 0;
            let mut span_start = 0.0f32;
            let mut inside = false;
            for &(x, dir) in xs.iter() {
                let was = inside;
                wind += if even_odd { 1 } else { dir };
                inside = if even_odd { wind % 2 != 0 } else { wind != 0 };
                if !was && inside {
                    span_start = x;
                } else if was && !inside {
                    add_span(cov, w, y, span_start, x, inv);
                }
            }
        }
    }
}

/// Add a horizontal span's coverage to row `y`, with exact partial pixels at the ends.
fn add_span(cov: &mut [f32], w: u32, y: i32, a: f32, b: f32, weight: f32) {
    if y < 0 || b <= a {
        return;
    }
    let row = y as usize * w as usize;
    let x0 = a.floor().max(0.0) as usize;
    let x1 = (b.ceil().max(0.0) as usize).min(w as usize);
    for x in x0..x1 {
        let c = (b.min(x as f32 + 1.0) - a.max(x as f32)).clamp(0.0, 1.0);
        if let Some(slot) = cov.get_mut(row + x) {
            *slot += c * weight;
        }
    }
}

/// Rasterize a path set into a fresh straight-RGBA8 buffer of `w`×`h`.
///
/// `aa: false` hard-thresholds coverage at 50 %, which is what pixel mode wants —
/// a vector logo dropped into a 32² sprite must not arrive pre-blurred.
pub fn render(paths: &[VPath], w: u32, h: u32, aa: bool) -> Vec<u8> {
    let mut out = vec![0u8; w as usize * h as usize * 4];
    let mut cov = vec![0f32; w as usize * h as usize];
    for path in paths {
        // --- fill ---
        if let Some(paint) = &path.fill
            && path.nodes.len() >= 3
        {
            cov.iter_mut().for_each(|c| *c = 0.0);
            coverage(&[path.flatten()], w, h, path.even_odd, &mut cov);
            blit(&mut out, &cov, w, h, paint, aa);
        }
        // --- stroke ---
        if let Some(stroke) = &path.stroke
            && stroke.width > 0.0
            && path.nodes.len() >= 2
        {
            cov.iter_mut().for_each(|c| *c = 0.0);
            let rings = stroke_rings(&path.flatten(), path.closed, stroke);
            // Nonzero unions the overlapping pieces; even-odd would punch holes.
            coverage(&rings, w, h, false, &mut cov);
            blit(&mut out, &cov, w, h, &Paint::Solid(stroke.color), aa);
        }
    }
    out
}

fn blit(out: &mut [u8], cov: &[f32], w: u32, h: u32, paint: &Paint, aa: bool) {
    for y in 0..h as usize {
        for x in 0..w as usize {
            let mut c = cov[y * w as usize + x].clamp(0.0, 1.0);
            if !aa {
                c = if c >= 0.5 { 1.0 } else { 0.0 };
            }
            if c <= 0.0 {
                continue;
            }
            let px = paint.sample(x as f32 + 0.5, y as f32 + 0.5);
            let o = (y * w as usize + x) * 4;
            let src = [px[0], px[1], px[2], crate::u8c(px[3] as f32 * c)];
            let dst = [out[o], out[o + 1], out[o + 2], out[o + 3]];
            let r = crate::blend::over(dst, src, crate::Blend::Mix, 1.0);
            out[o..o + 4].copy_from_slice(&r);
        }
    }
}

/// Turn a polyline into a set of consistently-wound convex rings whose nonzero
/// union is the stroked outline: one quad per segment, one wedge per joint, plus
/// caps. Simple, robust, and correct for self-overlapping strokes.
pub fn stroke_rings(pts: &[(f32, f32)], closed: bool, stroke: &Stroke) -> Vec<Vec<(f32, f32)>> {
    let mut rings = Vec::new();
    let hw = (stroke.width * 0.5).max(0.05);
    let n = pts.len();
    if n < 2 {
        if n == 1 && stroke.cap == Cap::Round {
            rings.push(circle_ring(pts[0].0, pts[0].1, hw));
        }
        return rings;
    }
    let segs = if closed { n } else { n - 1 };
    for i in 0..segs {
        let (ax, ay) = pts[i];
        let (bx, by) = pts[(i + 1) % n];
        let (dx, dy) = (bx - ax, by - ay);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-6 {
            continue;
        }
        let (nx, ny) = (-dy / len * hw, dx / len * hw);
        rings.push(vec![
            (ax + nx, ay + ny),
            (bx + nx, by + ny),
            (bx - nx, by - ny),
            (ax - nx, ay - ny),
        ]);
    }
    // Joints.
    let joints: Vec<usize> = if closed { (0..n).collect() } else { (1..n - 1).collect() };
    for &i in &joints {
        let (x, y) = pts[i];
        match stroke.join {
            Join::Round => rings.push(circle_ring(x, y, hw)),
            Join::Bevel | Join::Miter => {
                let prev = pts[(i + n - 1) % n];
                let next = pts[(i + 1) % n];
                let d0 = norm(x - prev.0, y - prev.1);
                let d1 = norm(next.0 - x, next.1 - y);
                let n0 = (-d0.1 * hw, d0.0 * hw);
                let n1 = (-d1.1 * hw, d1.0 * hw);
                // Bevel triangles on both sides (one is degenerate for a straight run).
                rings.push(vec![(x, y), (x + n0.0, y + n0.1), (x + n1.0, y + n1.1)]);
                rings.push(vec![(x, y), (x - n1.0, y - n1.1), (x - n0.0, y - n0.1)]);
                if stroke.join == Join::Miter {
                    // The miter tip: extend along the angle bisector, capped at 4×.
                    let bx = d0.0 - d1.0;
                    let by = d0.1 - d1.1;
                    let bl = (bx * bx + by * by).sqrt();
                    if bl > 1e-4 {
                        let cosh = ((1.0 - (d0.0 * d1.0 + d0.1 * d1.1)) * 0.5).max(1e-4).sqrt();
                        let ext = (hw / cosh).min(hw * 4.0);
                        let (mx, my) = (bx / bl * ext, by / bl * ext);
                        let side = if d0.0 * d1.1 - d0.1 * d1.0 > 0.0 { -1.0 } else { 1.0 };
                        rings.push(vec![
                            (x, y),
                            (x + side * n0.0, y + side * n0.1),
                            (x + mx, y + my),
                            (x + side * n1.0, y + side * n1.1),
                        ]);
                    }
                }
            }
        }
    }
    if !closed {
        match stroke.cap {
            Cap::Butt => {}
            Cap::Round => {
                rings.push(circle_ring(pts[0].0, pts[0].1, hw));
                rings.push(circle_ring(pts[n - 1].0, pts[n - 1].1, hw));
            }
            Cap::Square => {
                for (p, q) in [(pts[0], pts[1]), (pts[n - 1], pts[n - 2])] {
                    let d = norm(p.0 - q.0, p.1 - q.1);
                    let (nx, ny) = (-d.1 * hw, d.0 * hw);
                    let (ex, ey) = (p.0 + d.0 * hw, p.1 + d.1 * hw);
                    rings.push(vec![
                        (p.0 + nx, p.1 + ny),
                        (ex + nx, ey + ny),
                        (ex - nx, ey - ny),
                        (p.0 - nx, p.1 - ny),
                    ]);
                }
            }
        }
    }
    unify_winding(&mut rings);
    rings
}

fn norm(x: f32, y: f32) -> (f32, f32) {
    let l = (x * x + y * y).sqrt().max(1e-6);
    (x / l, y / l)
}

/// Twice the signed area of a ring (shoelace). Sign = orientation.
fn signed_area2(ring: &[(f32, f32)]) -> f32 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (x0, y0) = ring[i];
            let (x1, y1) = ring[(i + 1) % n];
            x0 * y1 - x1 * y0
        })
        .sum()
}

/// Force every ring to wind the same way.
///
/// This is load-bearing, not tidiness: the stroker's union depends on nonzero
/// winding, and a piece wound the other way *cancels* where it overlaps its
/// neighbours instead of joining them. The symptom was a stroked ellipse
/// rendering as a dotted line — one ring per flattened point, every other one
/// punching a hole in the segment quad beside it.
fn unify_winding(rings: &mut [Vec<(f32, f32)>]) {
    for r in rings.iter_mut() {
        if signed_area2(r) < 0.0 {
            r.reverse();
        }
    }
}

fn circle_ring(cx: f32, cy: f32, r: f32) -> Vec<(f32, f32)> {
    let steps = ((r * 2.0) as usize).clamp(8, 48);
    (0..steps)
        .map(|i| {
            let a = i as f32 * std::f32::consts::TAU / steps as f32;
            (cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(buf: &[u8], w: u32, x: u32, y: u32) -> u8 {
        buf[((y * w + x) * 4 + 3) as usize]
    }

    #[test]
    fn a_filled_rect_covers_its_interior_and_nothing_else() {
        let p = VPath::rect(4.0, 4.0, 8.0, 8.0);
        let buf = render(&[p], 20, 20, true);
        assert_eq!(alpha_at(&buf, 20, 8, 8), 255);
        assert_eq!(alpha_at(&buf, 20, 2, 2), 0);
        assert_eq!(alpha_at(&buf, 20, 15, 15), 0);
    }

    #[test]
    fn edges_are_antialiased_and_pixel_mode_is_not() {
        // A rect on a half-pixel boundary: AA gives partial coverage, no-AA snaps.
        let paths = [VPath::rect(4.5, 4.0, 8.0, 8.0)];
        let aa = render(&paths, 20, 20, true);
        let a = alpha_at(&aa, 20, 4, 8);
        assert!(a > 0 && a < 255, "antialiased edge should be partial, got {a}");
        let hard = render(&paths, 20, 20, false);
        let h = alpha_at(&hard, 20, 4, 8);
        assert!(h == 0 || h == 255, "pixel mode must be hard-edged, got {h}");
    }

    #[test]
    fn ellipse_is_round() {
        let p = VPath::ellipse(20.0, 20.0, 15.0, 15.0);
        let buf = render(&[p], 40, 40, true);
        assert_eq!(alpha_at(&buf, 40, 20, 20), 255, "centre filled");
        assert_eq!(alpha_at(&buf, 40, 20, 2), 0, "above the top");
        assert_eq!(alpha_at(&buf, 40, 6, 6), 0, "corner outside the circle");
        assert_eq!(alpha_at(&buf, 40, 20, 8), 255, "inside the top edge");
    }

    #[test]
    fn even_odd_punches_a_hole() {
        // Outer ring CCW, inner ring CCW too: nonzero fills solid, even-odd holes.
        let mut path = VPath::rect(2.0, 2.0, 16.0, 16.0);
        for n in VPath::rect(6.0, 6.0, 8.0, 8.0).nodes {
            path.nodes.push(n);
        }
        path.even_odd = true;
        let buf = render(&[path], 20, 20, false);
        assert_eq!(alpha_at(&buf, 20, 3, 3), 255, "the ring itself is filled");
        assert_eq!(alpha_at(&buf, 20, 10, 10), 0, "the hole is empty");
    }

    #[test]
    fn strokes_have_width_and_do_not_leak_inside() {
        let mut p = VPath::rect(5.0, 5.0, 10.0, 10.0);
        p.fill = None;
        p.stroke = Some(Stroke { color: [255, 0, 0, 255], width: 3.0, ..Default::default() });
        let buf = render(&[p], 24, 24, false);
        assert_eq!(alpha_at(&buf, 24, 5, 10), 255, "on the edge");
        assert_eq!(alpha_at(&buf, 24, 10, 10), 0, "the middle stays empty");
    }

    /// The dotted-ellipse bug: a stroked curve is many overlapping pieces, and
    /// under nonzero winding a piece wound the other way CANCELS its neighbour.
    /// Every sample around the ring must be solid.
    #[test]
    fn a_stroked_curve_is_solid_all_the_way_round() {
        let mut p = VPath::ellipse(40.0, 30.0, 30.0, 20.0);
        p.fill = None;
        p.stroke = Some(Stroke { color: [255, 255, 255, 255], width: 3.0, cap: Cap::Round, join: Join::Round });
        let buf = render(&[p], 80, 60, false);
        let mut gaps = Vec::new();
        for i in 0..64 {
            let a = i as f32 * std::f32::consts::TAU / 64.0;
            let x = (40.0 + 30.0 * a.cos()).round() as u32;
            let y = (30.0 + 20.0 * a.sin()).round() as u32;
            // The exact perimeter point can land a hair off the 3 px band, so
            // accept the pixel or either neighbour.
            let hit = (-1i32..=1).any(|dy| {
                (-1i32..=1).any(|dx| {
                    let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                    sx >= 0 && sy >= 0 && sx < 80 && sy < 60 && alpha_at(&buf, 80, sx as u32, sy as u32) > 0
                })
            });
            if !hit {
                gaps.push((x, y));
            }
        }
        assert!(gaps.is_empty(), "stroke has holes at {gaps:?}");
    }

    #[test]
    fn open_line_stroke_draws_between_the_points() {
        let p = VPath::line(2.0, 10.0, 18.0, 10.0, Stroke { color: [0, 0, 0, 255], width: 3.0, cap: Cap::Butt, join: Join::Round });
        let buf = render(&[p], 20, 20, false);
        assert_eq!(alpha_at(&buf, 20, 10, 10), 255);
        assert_eq!(alpha_at(&buf, 20, 10, 4), 0);
    }

    #[test]
    fn gradients_ramp_along_their_axis() {
        let mut p = VPath::rect(0.0, 0.0, 20.0, 20.0);
        p.fill = Some(Paint::Linear {
            a: [0.0, 0.0],
            b: [20.0, 0.0],
            stops: vec![(0.0, [0, 0, 0, 255]), (1.0, [255, 255, 255, 255])],
        });
        let buf = render(&[p], 20, 20, true);
        let l = buf[(10 * 20 + 1) * 4];
        let r = buf[(10 * 20 + 18) * 4];
        assert!(l < 40 && r > 215, "ramp should run dark → light ({l}, {r})");
    }

    #[test]
    fn reshape_helpers_find_nodes_and_insert_on_edges() {
        let mut p = VPath::rect(0.0, 0.0, 10.0, 10.0);
        assert_eq!(p.hit_node(0.4, 0.4, 2.0), Some(0));
        assert_eq!(p.hit_node(5.0, 5.0, 2.0), None);
        let (seg, t) = p.hit_segment(5.0, 0.0, 1.5).expect("edge hit");
        assert_eq!(seg, 0);
        let idx = p.insert_node(seg, t);
        assert_eq!(p.nodes.len(), 5);
        assert!((p.nodes[idx].p[0] - 5.0).abs() < 1.0);
    }

    #[test]
    fn curve_nodes_without_handles_still_bend() {
        let mut p = VPath { nodes: vec![VNode::curve(2.0, 10.0), VNode::curve(10.0, 2.0), VNode::curve(18.0, 10.0)], closed: false, fill: None, stroke: Some(Stroke::default()), even_odd: false };
        let flat = p.flatten();
        assert!(flat.len() > 3, "auto handles must produce a subdivided curve");
        // Corner nodes give a straight polyline instead.
        p.nodes.iter_mut().for_each(|n| n.kind = NodeKind::Corner);
        assert_eq!(p.flatten().len(), 3);
    }

    #[test]
    fn bounds_include_the_stroke() {
        let mut p = VPath::rect(10.0, 10.0, 10.0, 10.0);
        p.stroke = Some(Stroke { width: 8.0, ..Default::default() });
        let b = p.bounds();
        assert!(b.x <= 4 && b.right() >= 26, "{b:?}");
    }
}
