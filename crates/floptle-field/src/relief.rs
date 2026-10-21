//! A planet's ground as a height over its sphere, precomputed on a cube map:
//! mountains, warp, plateaus, terraces, and the ravines and rivers carved into
//! them.
//!
//! The plain generator evaluates its relief noise once a voxel, in that voxel's
//! direction. These layers cost several times that noise, and the height they
//! make depends only on direction, so it is worked out once per direction on a
//! grid a voxel or so across and read back bilinearly. Each voxel then pays a
//! lookup rather than all the layers, and the grid's min/max pyramid tells the
//! block classifier the highest and lowest ground over any block exactly, which
//! keeps the generator skipping what it skips.
//!
//! A world with none of these layers never builds a grid: its ground is the
//! one it always was, voxel for voxel.

use floptle_core::math::{Vec2, Vec3};
use floptle_core::noise::{Noise, Rng};

use crate::procgen::{PlanetFill, Ravines, Rivers};

/// The most cells along a cube face's edge. At 1024 a 1.5 km world's grid is
/// three metres a cell, which the relief layers' features dwarf.
const MAX_CELLS: usize = 1024;
/// The river distance stored where no river is near, in world units.
const FAR: f32 = 1.0e6;
/// Cells along a face of the coarse grid rivers are traced on.
const TRACE_CELLS: usize = 192;

/// A carved line on the planet: a river or a ravine, as directions from the
/// planet's centre, with the width and depth it is carved at.
#[derive(Clone, Debug)]
pub struct Channel {
    /// Points along it, unit directions from the planet's centre, source first.
    pub points: Vec<Vec3>,
    /// Width in world units at each point (a river widens as it flows).
    pub widths: Vec<f32>,
    /// How deep it is carved below the ground around it.
    pub depth: f32,
    /// A river (`true`) or a ravine.
    pub river: bool,
}

/// A face of the cube map: the ground height at every grid corner, the distance
/// to the nearest river there, and the min/max pyramid of the heights.
struct Face {
    h: Vec<f32>,
    water: Vec<f32>,
    /// Level k: (cells across, min, max), each cell spanning 2^k cells of level 0.
    mips: Vec<(usize, Vec<f32>, Vec<f32>)>,
}

/// A planet's precomputed ground.
pub(crate) struct Relief {
    n: usize,
    faces: Vec<Face>,
    lo: f32,
    hi: f32,
}

/// Which face of the cube `d` points through, and where on it, in [-1, 1].
fn to_face(d: Vec3) -> (usize, f32, f32) {
    let a = d.abs();
    if a.x >= a.y && a.x >= a.z {
        if d.x > 0.0 { (0, -d.z / a.x, d.y / a.x) } else { (1, d.z / a.x, d.y / a.x) }
    } else if a.y >= a.z {
        if d.y > 0.0 { (2, d.x / a.y, -d.z / a.y) } else { (3, d.x / a.y, d.z / a.y) }
    } else if d.z > 0.0 {
        (4, d.x / a.z, d.y / a.z)
    } else {
        (5, -d.x / a.z, d.y / a.z)
    }
}

/// The direction through face `f` at `(u, v)`. The inverse of [`to_face`].
fn from_face(f: usize, u: f32, v: f32) -> Vec3 {
    match f {
        0 => Vec3::new(1.0, v, -u),
        1 => Vec3::new(-1.0, v, u),
        2 => Vec3::new(u, 1.0, -v),
        3 => Vec3::new(u, -1.0, v),
        4 => Vec3::new(u, v, 1.0),
        _ => Vec3::new(-u, v, -1.0),
    }
    .normalize()
}

/// The layers that shape the ground, evaluated directly: what the grid stores.
struct Layers<'a> {
    spec: &'a PlanetFill,
    noise: Noise,
}

impl Layers<'_> {
    /// The ground's height above the radius in direction `d`, before carving.
    fn height(&self, d: Vec3) -> f32 {
        let s = self.spec;
        let n = &self.noise;
        let w = match &s.warp {
            Some(w) if w.strength != 0.0 => {
                let q = d * w.scale;
                d + Vec3::new(
                    n.fbm(q + Vec3::splat(311.0), 3),
                    n.fbm(q + Vec3::splat(-77.0), 3),
                    n.fbm(q + Vec3::splat(19.5), 3),
                ) * w.strength
            }
            _ => d,
        };
        let mut h = n.fbm(w * s.bump_freq, 5) * s.relief;
        if let Some(m) = &s.mountains {
            // Ranges where a slow mask is high, ridged peaks inside them.
            let edge = 1.0 - m.coverage.clamp(0.0, 1.0);
            let mask = smoothstep(edge - 0.12, edge + 0.12, 0.5 + 0.5 * n.fbm(w * m.scale * 0.3 + Vec3::splat(241.0), 3));
            let ridges = n.ridged(w * m.scale + Vec3::splat(57.0), m.octaves.clamp(1, 8), m.sharpness.max(0.1));
            h += m.height * mask * ridges;
        }
        if let Some(p) = &s.plateau {
            h = smin(h, p.height, p.smooth.max(0.01));
        }
        if let Some(t) = &s.terrace
            && t.step > 0.0
        {
            h = terrace(h, t.step, t.sharpness);
        }
        h
    }
}

impl Relief {
    /// Does this spec ask for anything the grid is for?
    pub(crate) fn wanted(s: &PlanetFill) -> bool {
        s.mountains.is_some()
            || s.warp.as_ref().is_some_and(|w| w.strength != 0.0)
            || s.plateau.is_some()
            || s.terrace.is_some()
            || s.ravines.is_some()
            || s.rivers.is_some()
            || !s.biomes.is_empty()
    }

    /// Build the grid for `s`, the channels traced across it carved in.
    pub(crate) fn new(s: &PlanetFill) -> Self {
        let n = ((2.0 * s.radius / s.voxel.max(0.25)).ceil() as usize).clamp(16, MAX_CELLS);
        let layers = Layers { spec: s, noise: Noise::new(s.seed) };
        let channels = trace_channels(s, &layers);
        let index = ChannelIndex::new(&channels, s.radius);
        // A row of a face to a job: six faces to six threads left most of a
        // machine idle through the slowest part of making a layered world.
        let rows = crate::chunks::par_map(6 * (n + 1), |k| {
            let (f, j) = (k / (n + 1), k % (n + 1));
            (0..=n)
                .map(|i| {
                    let d = from_face(f, -1.0 + 2.0 * i as f32 / n as f32, -1.0 + 2.0 * j as f32 / n as f32);
                    let (carve, river) = index.carve(d, s.radius);
                    (layers.height(d) - carve, river)
                })
                .collect::<Vec<_>>()
        });
        let mut faces = Vec::with_capacity(6);
        for f in 0..6 {
            let mut h = Vec::with_capacity((n + 1) * (n + 1));
            let mut water = Vec::with_capacity((n + 1) * (n + 1));
            for row in &rows[f * (n + 1)..(f + 1) * (n + 1)] {
                for &(hv, w) in row {
                    h.push(hv);
                    water.push(w);
                }
            }
            let mips = pyramid(&h, n);
            faces.push(Face { h, water, mips });
        }
        let lo = faces.iter().flat_map(|f| f.h.iter()).fold(f32::INFINITY, |a, b| a.min(*b));
        let hi = faces.iter().flat_map(|f| f.h.iter()).fold(f32::NEG_INFINITY, |a, b| a.max(*b));
        Relief { n, faces, lo, hi }
    }

    /// The ground's height above the radius in direction `d` (unit).
    pub(crate) fn height(&self, d: Vec3) -> f32 {
        self.sample(d, |f| &f.h)
    }

    /// How far along the ground the nearest river is from `d`, in world units.
    pub(crate) fn river_distance(&self, d: Vec3) -> f32 {
        self.sample(d, |f| &f.water)
    }

    /// The ground's slope in direction `d`, in degrees from level.
    pub(crate) fn slope_degrees(&self, d: Vec3, radius: f32) -> f32 {
        let t1 = d.any_orthonormal_vector();
        let t2 = d.cross(t1);
        let e = 2.0 / self.n as f32;
        let dh1 = self.height((d + t1 * e).normalize()) - self.height((d - t1 * e).normalize());
        let dh2 = self.height((d + t2 * e).normalize()) - self.height((d - t2 * e).normalize());
        let run = 2.0 * e * radius.max(1.0);
        Vec2::new(dh1, dh2).length().atan2(run).to_degrees()
    }

    fn sample(&self, d: Vec3, pick: impl Fn(&Face) -> &Vec<f32>) -> f32 {
        let (f, u, v) = to_face(d);
        let n = self.n;
        let x = ((u + 1.0) * 0.5 * n as f32).clamp(0.0, n as f32);
        let y = ((v + 1.0) * 0.5 * n as f32).clamp(0.0, n as f32);
        let (i, j) = ((x as usize).min(n - 1), (y as usize).min(n - 1));
        let (fx, fy) = (x - i as f32, y - j as f32);
        let g = pick(&self.faces[f]);
        let at = |a: usize, b: usize| g[b * (n + 1) + a];
        let top = at(i, j) + (at(i + 1, j) - at(i, j)) * fx;
        let bot = at(i, j + 1) + (at(i + 1, j + 1) - at(i, j + 1)) * fx;
        top + (bot - top) * fy
    }

    /// Bounds on the ground's height over every direction a box with corners
    /// `corners` covers. Exact for the interpolated ground: a box within one
    /// face covers the face region its corners project to, and the pyramid
    /// answers for that; one straddling faces gets the whole world's range.
    pub(crate) fn height_bounds(&self, corners: &[Vec3; 8]) -> (f32, f32) {
        let mut face = None;
        let (mut u0, mut u1, mut v0, mut v1) = (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
        for c in corners {
            if c.length_squared() < 1e-12 {
                return (self.lo, self.hi);
            }
            let (f, u, v) = to_face(c.normalize());
            if face.is_some_and(|g| g != f) {
                return (self.lo, self.hi);
            }
            face = Some(f);
            (u0, u1, v0, v1) = (u0.min(u), u1.max(u), v0.min(v), v1.max(v));
        }
        let Some(f) = face else { return (self.lo, self.hi) };
        let n = self.n as f32;
        let cell = |t: f32| (((t + 1.0) * 0.5 * n).floor().max(0.0) as usize).min(self.n - 1);
        let (i0, i1, j0, j1) = (cell(u0), cell(u1), cell(v0), cell(v1));
        let span = (i1 - i0).max(j1 - j0) + 1;
        let mut k = 0;
        while (1usize << k) < span && k + 1 < self.faces[f].mips.len() {
            k += 1;
        }
        let (m, mn, mx) = &self.faces[f].mips[k];
        let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
        for b in (j0 >> k)..=(j1 >> k).min(m - 1) {
            for a in (i0 >> k)..=(i1 >> k).min(m - 1) {
                lo = lo.min(mn[b * m + a]);
                hi = hi.max(mx[b * m + a]);
            }
        }
        (lo, hi)
    }

    /// The lowest and highest ground anywhere.
    pub(crate) fn range(&self) -> (f32, f32) {
        (self.lo, self.hi)
    }
}

/// The min/max pyramid over a face's `n × n` cells, each cell the four corners
/// around it.
fn pyramid(h: &[f32], n: usize) -> Vec<(usize, Vec<f32>, Vec<f32>)> {
    let at = |a: usize, b: usize| h[b * (n + 1) + a];
    let mut mn = vec![0.0; n * n];
    let mut mx = vec![0.0; n * n];
    for b in 0..n {
        for a in 0..n {
            let q = [at(a, b), at(a + 1, b), at(a, b + 1), at(a + 1, b + 1)];
            mn[b * n + a] = q.iter().fold(f32::INFINITY, |x, y| x.min(*y));
            mx[b * n + a] = q.iter().fold(f32::NEG_INFINITY, |x, y| x.max(*y));
        }
    }
    let mut levels = vec![(n, mn, mx)];
    while levels.last().unwrap().0 > 1 {
        let (m, pmn, pmx) = levels.last().unwrap();
        let m2 = m.div_ceil(2);
        let (mut cmn, mut cmx) = (vec![f32::INFINITY; m2 * m2], vec![f32::NEG_INFINITY; m2 * m2]);
        for b in 0..*m {
            for a in 0..*m {
                let c = (b / 2) * m2 + a / 2;
                cmn[c] = cmn[c].min(pmn[b * m + a]);
                cmx[c] = cmx[c].max(pmx[b * m + a]);
            }
        }
        levels.push((m2, cmn, cmx));
    }
    levels
}

/// The rivers and ravines a spec asks for, traced over a coarse grid of the
/// uncarved ground. Deterministic in the spec: the same opts give the same
/// lines, which is how a game asks where they are without generating.
pub fn channels_of(s: &PlanetFill) -> Vec<Channel> {
    trace_channels(s, &Layers { spec: s, noise: Noise::new(s.seed) })
}

fn trace_channels(s: &PlanetFill, layers: &Layers) -> Vec<Channel> {
    if s.rivers.is_none() && s.ravines.is_none() {
        return Vec::new();
    }
    // The ground rivers flow over, on a coarse grid: plenty for which way is
    // downhill, and cheap enough that asking for the rivers alone is quick.
    let n = TRACE_CELLS;
    let grid: Vec<Vec<f32>> = crate::chunks::par_map(6, |f| {
        let mut h = vec![0.0f32; (n + 1) * (n + 1)];
        for j in 0..=n {
            for i in 0..=n {
                h[j * (n + 1) + i] =
                    layers.height(from_face(f, -1.0 + 2.0 * i as f32 / n as f32, -1.0 + 2.0 * j as f32 / n as f32));
            }
        }
        h
    });
    let ground = |d: Vec3| {
        let (f, u, v) = to_face(d);
        let x = ((u + 1.0) * 0.5 * n as f32).clamp(0.0, n as f32);
        let y = ((v + 1.0) * 0.5 * n as f32).clamp(0.0, n as f32);
        let (i, j) = ((x as usize).min(n - 1), (y as usize).min(n - 1));
        let (fx, fy) = (x - i as f32, y - j as f32);
        let g = &grid[f];
        let at = |a: usize, b: usize| g[b * (n + 1) + a];
        let top = at(i, j) + (at(i + 1, j) - at(i, j)) * fx;
        let bot = at(i, j + 1) + (at(i + 1, j + 1) - at(i, j + 1)) * fx;
        top + (bot - top) * fy
    };
    let mut rng = Rng::new(s.seed.wrapping_add(907));
    let noise = Noise::new(s.seed.wrapping_add(5));
    let mut out = Vec::new();
    if let Some(r) = &s.rivers {
        out.extend(trace_rivers(r, s.radius, &ground, &mut rng, &noise));
    }
    if let Some(r) = &s.ravines {
        out.extend(trace_ravines(r, s.radius, &mut rng, &noise));
    }
    out
}

fn random_dir(rng: &mut Rng) -> Vec3 {
    loop {
        let d = Vec3::new(rng.range(-1.0, 1.0) as f32, rng.range(-1.0, 1.0) as f32, rng.range(-1.0, 1.0) as f32);
        let l = d.length();
        if l > 0.05 && l <= 1.0 {
            return d / l;
        }
    }
}

/// Rivers: from the highest of a field of candidate springs, the way water
/// drains from there to the sea.
///
/// Each follows [`crate::water::drainage`] over the trace grid: downhill, and
/// across any basin on its way at the height the basin fills to, toward where
/// it spills, so it ends at the sea (or, on a world with no sea, the lowest
/// ground, where its lake is). A river that reaches one already traced joins
/// it there. The path, a staircase of cells, is rounded off and given its
/// meander after.
///
/// They used to march downhill a step at a time and stop once forty steps in
/// a row had not gone down. That never ended a river circling a pit, since a
/// step down reset the count, and on an 800 m world every river ran to the
/// step cap, carving the same basin over and over: rivers doubled the cost of
/// generating a world.
fn trace_rivers(r: &Rivers, radius: f32, ground: &(impl Fn(Vec3) -> f32 + Sync), rng: &mut Rng, noise: &Noise) -> Vec<Channel> {
    use crate::water::{drainage, sphere_cell, sphere_centre, WaterGrid, WALL};
    let mut springs: Vec<(f32, Vec3)> = (0..2048).map(|_| random_dir(rng)).map(|d| (ground(d), d)).collect();
    springs.sort_by(|a, b| b.0.total_cmp(&a.0));
    // Spread apart, so the rivers drain different country.
    let apart = (0.6 / (r.count.max(1) as f32).sqrt()).min(0.5);
    let mut chosen: Vec<Vec3> = Vec::new();
    for (_, d) in springs {
        if chosen.len() >= r.count as usize {
            break;
        }
        if chosen.iter().all(|c| c.dot(d) < apart.cos()) {
            chosen.push(d);
        }
    }
    let n = TRACE_CELLS;
    let grid = WaterGrid::sphere(n, radius, ground);
    let down = drainage(&grid, r.sea_level);
    let step = (r.width.max(0.5) * 1.5 / radius.max(1.0)).clamp(1e-4, 0.05);
    let mut taken = vec![false; grid.len()];
    let mut out = Vec::new();
    for spring in chosen {
        // The cells the water runs through, to the sea or into a river
        // already traced.
        let mut cells = Vec::new();
        let mut c = sphere_cell(n, spring);
        loop {
            cells.push(c);
            if taken[c] || grid.ground[c] < r.sea_level || cells.len() > grid.len() {
                break;
            }
            match down[c] {
                WALL => break,
                next => c = next as usize,
            }
        }
        for &c in &cells {
            taken[c] = true;
        }
        let mut path: Vec<Vec3> = std::iter::once(spring).chain(cells[1..].iter().map(|&c| sphere_centre(n, c))).collect();
        // Round the staircase off: each pass cuts every corner a quarter of
        // the way along both of its edges.
        for _ in 0..3 {
            if path.len() < 3 {
                break;
            }
            let mut cut = vec![path[0]];
            for w in path.windows(2) {
                cut.push((w[0] * 0.75 + w[1] * 0.25).normalize());
                cut.push((w[0] * 0.25 + w[1] * 0.75).normalize());
            }
            cut.push(path[path.len() - 1]);
            path = cut;
        }
        // Points `step` apart along it, each swung aside by the meander.
        let mut points = vec![path[0]];
        let mut widths = vec![r.width * 0.5];
        let mut left = step;
        for w in path.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = a.angle_between(b);
            let mut t = 0.0;
            while len - t >= left {
                t += left;
                left = step;
                let p = a.slerp(b, t / len.max(1e-9));
                let k = points.len();
                let side = p.cross(b - a).normalize_or_zero();
                let swing = noise.fbm(p * 40.0, 2) * r.meander * 2.0 * step;
                points.push((p + side * swing).normalize());
                // Wider as it gathers water, up to the asked width.
                widths.push(r.width * (0.5 + 0.5 * (k as f32 / 200.0).min(1.0)));
            }
            left -= len - t;
        }
        if points.len() > 4 {
            out.push(Channel { points, widths, depth: r.depth, river: true });
        }
    }
    out
}

/// Ravines: long meandering cuts with steep walls, laid without regard to the
/// slope, as faults and old floods leave them.
fn trace_ravines(r: &Ravines, radius: f32, rng: &mut Rng, noise: &Noise) -> Vec<Channel> {
    let step = (r.width.max(0.5) / radius.max(1.0)).clamp(1e-4, 0.05);
    let steps = ((r.length / radius.max(1.0)) / step).clamp(4.0, 6000.0) as usize;
    (0..r.count)
        .map(|_| {
            let mut d = random_dir(rng);
            let mut heading = d.any_orthonormal_vector().normalize();
            let (mut points, mut widths) = (vec![d], vec![r.width]);
            for k in 0..steps {
                let side = d.cross(heading);
                let bend = noise.fbm(d * 20.0 + Vec3::splat(k as f32 * 0.003 + 71.0), 2) * r.meander;
                heading = (heading + side * bend * 0.2).normalize();
                d = (d + heading * step).normalize();
                // Keep the heading on the sphere's tangent plane.
                heading = (heading - d * heading.dot(d)).normalize_or_zero();
                points.push(d);
                widths.push(r.width);
            }
            Channel { points, widths, depth: r.depth, river: false }
        })
        .collect()
}

/// The channels' segments bucketed by direction, so finding the ones near a
/// point asks a handful rather than all of them.
struct ChannelIndex<'a> {
    channels: &'a [Channel],
    cells: std::collections::HashMap<(i32, i32, i32), Vec<(u32, u32)>>,
    cell: f32,
}

impl<'a> ChannelIndex<'a> {
    fn new(channels: &'a [Channel], radius: f32) -> Self {
        let reach = channels
            .iter()
            .flat_map(|c| c.widths.iter().map(move |w| reach(*w, c.river)))
            .fold(0.0f32, f32::max)
            / radius.max(1.0);
        let cell = (reach * 2.0).max(0.01);
        let mut cells: std::collections::HashMap<(i32, i32, i32), Vec<(u32, u32)>> = std::collections::HashMap::new();
        for (ci, c) in channels.iter().enumerate() {
            for si in 0..c.points.len().saturating_sub(1) {
                let (a, b) = (c.points[si], c.points[si + 1]);
                let (lo, hi) = (a.min(b) - Vec3::splat(reach), a.max(b) + Vec3::splat(reach));
                let k = |v: f32| (v / cell).floor() as i32;
                for z in k(lo.z)..=k(hi.z) {
                    for y in k(lo.y)..=k(hi.y) {
                        for x in k(lo.x)..=k(hi.x) {
                            cells.entry((x, y, z)).or_default().push((ci as u32, si as u32));
                        }
                    }
                }
            }
        }
        ChannelIndex { channels, cells, cell }
    }

    /// How far below the ground around it direction `d` is carved, and how far
    /// (world units) the nearest river is.
    fn carve(&self, d: Vec3, radius: f32) -> (f32, f32) {
        let k = |v: f32| (v / self.cell).floor() as i32;
        // Far from every river is a long way, not infinitely far: the grid
        // interpolates this, and infinity minus infinity between two corners
        // is NaN, which every range test then passes.
        let Some(segs) = self.cells.get(&(k(d.x), k(d.y), k(d.z))) else { return (0.0, FAR) };
        let (mut cut, mut river) = (0.0f32, FAR);
        for &(ci, si) in segs {
            let c = &self.channels[ci as usize];
            let (a, b) = (c.points[si as usize], c.points[si as usize + 1]);
            let ab = b - a;
            let t = ((d - a).dot(ab) / ab.length_squared().max(1e-12)).clamp(0.0, 1.0);
            let dist = (d - (a + ab * t)).length() * radius;
            let w = c.widths[si as usize] + (c.widths[si as usize + 1] - c.widths[si as usize]) * t;
            cut = cut.max(c.depth * profile(dist, w, c.river));
            if c.river {
                river = river.min(dist);
            }
        }
        (cut, river)
    }
}

/// How far from its line a channel of width `w` still lowers the ground.
fn reach(w: f32, river: bool) -> f32 {
    if river { w * 4.0 } else { w * 0.6 }
}

/// How much of a channel's depth the ground `dist` from its line loses: a
/// river cuts a narrow bed in a wide valley; a ravine falls away steeply to a
/// flat floor.
fn profile(dist: f32, w: f32, river: bool) -> f32 {
    let w = w.max(0.1);
    if river {
        let bed = 1.0 - smoothstep(0.0, w * 0.6, dist);
        let valley = 1.0 - smoothstep(0.0, w * 4.0, dist);
        (0.35 * bed + 0.65 * valley * valley).min(1.0)
    } else {
        1.0 - smoothstep(w * 0.4, w * 0.6, dist)
    }
}

fn smoothstep(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a).max(1e-6)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Smooth minimum, never above `min(a, b)`.
fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (0.5 + 0.5 * (b - a) / k).clamp(0.0, 1.0);
    b + (a - b) * h - k * h * (1.0 - h)
}

/// Steps of `step`, each a flat with a rise between; `sharpness` 0 is a gentle
/// ramp, 1 a cliff. Never decreasing in `h`, so bounds carry through it.
fn terrace(h: f32, step: f32, sharpness: f32) -> f32 {
    let t = h / step;
    let base = t.floor();
    let f = t - base;
    let w = 0.5 * (1.0 - sharpness.clamp(0.0, 0.98));
    (base + smoothstep(0.5 - w, 0.5 + w, f)) * step
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The face mapping round-trips, so every direction has one place on the grid.
    #[test]
    fn a_direction_goes_to_its_face_and_back() {
        let mut r = Rng::new(4);
        for _ in 0..5000 {
            let d = random_dir(&mut r);
            let (f, u, v) = to_face(d);
            assert!(u.abs() <= 1.0 + 1e-5 && v.abs() <= 1.0 + 1e-5);
            assert!((from_face(f, u, v) - d).length() < 1e-4, "{d:?} -> {f} {u} {v}");
        }
    }

    /// **The bounds hold every height in the box.** The classifier skips a
    /// block on this answer, so a bound too tight is a wall or a hole.
    #[test]
    fn height_bounds_hold_every_height_in_the_box() {
        let spec = PlanetFill {
            radius: 120.0,
            voxel: 1.0,
            mountains: Some(crate::procgen::Mountains { height: 30.0, scale: 3.0, sharpness: 2.0, coverage: 0.5, octaves: 4 }),
            warp: Some(crate::procgen::Warp { strength: 0.2, scale: 2.0 }),
            terrace: Some(crate::procgen::Terrace { step: 6.0, sharpness: 0.7 }),
            ravines: Some(Ravines { count: 3, depth: 12.0, width: 10.0, length: 300.0, meander: 1.0 }),
            ..PlanetFill::default()
        };
        let relief = Relief::new(&spec);
        let mut r = Rng::new(8);
        for _ in 0..3000 {
            let c = random_dir(&mut r) * (spec.radius + r.range(-20.0, 20.0) as f32);
            let h = Vec3::splat(r.range(0.5, 12.0) as f32);
            let corners: [Vec3; 8] = std::array::from_fn(|k| {
                c + Vec3::new(
                    if k & 1 == 0 { -h.x } else { h.x },
                    if k & 2 == 0 { -h.y } else { h.y },
                    if k & 4 == 0 { -h.z } else { h.z },
                )
            });
            let (lo, hi) = relief.height_bounds(&corners);
            for _ in 0..30 {
                let p = c + Vec3::new(r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32, r.range(-1.0, 1.0) as f32) * h;
                let v = relief.height(p.normalize());
                assert!(v >= lo - 1e-3 && v <= hi + 1e-3, "{v} outside [{lo}, {hi}]");
            }
        }
    }

    /// **A river runs to the sea, or into another river, and never round in
    /// circles.** Rivers used to march downhill and give up only after forty
    /// steps in a row that did not go down, which a river circling a pit never
    /// takes: on an 800 m world every one ran to the 4,000-step cap, back and
    /// forth over the same basin, and rivers doubled what the world cost to
    /// generate. On this world the old trace ran its first river 48 km, to end
    /// 73 m from its spring.
    #[test]
    fn a_river_runs_to_the_sea_and_never_round_in_circles() {
        let spec = PlanetFill {
            seed: 758290297,
            radius: 300.0,
            voxel: 1.5,
            relief: 20.0,
            bump_freq: 4.0,
            mountains: Some(crate::procgen::Mountains { height: 30.0, scale: 3.0, sharpness: 2.0, coverage: 0.5, octaves: 4 }),
            warp: Some(crate::procgen::Warp { strength: 0.2, scale: 2.0 }),
            rivers: Some(Rivers { count: 5, depth: 5.0, width: 8.0, sea_level: -6.0, meander: 1.0 }),
            ..PlanetFill::default()
        };
        let layers = Layers { spec: &spec, noise: Noise::new(spec.seed) };
        let rivers = channels_of(&spec);
        assert_eq!(rivers.len(), 5, "every spring should make a river");
        let step = 8.0 * 1.5 / spec.radius;
        for (k, r) in rivers.iter().enumerate() {
            let (start, end) = (r.points[0], *r.points.last().unwrap());
            let run: f32 = r.points.windows(2).map(|w| w[0].angle_between(w[1])).sum();
            let straight = start.angle_between(end);
            assert!(
                run <= 3.0 * straight + 20.0 * step,
                "river {k} runs {:.0} m to end {:.0} m from its spring",
                run * spec.radius,
                straight * spec.radius
            );
            for i in 20..r.points.len() {
                for j in 0..i - 20 {
                    assert!(
                        r.points[i].angle_between(r.points[j]) > 0.5 * step,
                        "river {k} comes back past point {j} at point {i}"
                    );
                }
            }
            let at_sea = layers.height(end) < spec.rivers.unwrap().sea_level + spec.relief * 0.25;
            let joined = rivers[..k].iter().any(|o| o.points.iter().any(|p| p.angle_between(end) < 4.0 * step));
            assert!(at_sea || joined, "river {k} ends {:.1} m up, away from the sea and every other river", layers.height(end));
        }
    }
}
