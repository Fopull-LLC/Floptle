//! Water over terrain: where lakes settle, and how water moves.
//!
//! Two pieces, both over the same grid of cells laid on the ground:
//!
//! * [`lake_levels`] fills every basin to the height it spills at, so a world
//!   starts with its seas and lakes already where water would have come to
//!   rest, rather than empty or flooding on the first frame.
//! * [`ShallowWater`] moves water between neighbouring cells by the height
//!   difference of their surfaces (the "virtual pipes" model). Every transfer
//!   is taken from one cell and given to the other, so the water's volume is
//!   conserved exactly, and a settled lake is a fixed point: it stays put.
//!
//! The grid is a cube map for a planet ([`WaterGrid::sphere`]) or a rectangle
//! ([`WaterGrid::flat`]); the simulation does not know which.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use floptle_core::math::Vec3;

/// No neighbour on that side: the edge of a flat grid.
pub const WALL: u32 = u32::MAX;

/// Cells, their ground and who borders whom.
#[derive(Clone, Debug)]
pub struct WaterGrid {
    /// Ground height of each cell, in metres (above the radius on a planet).
    pub ground: Vec<f32>,
    /// Each cell's area, square metres.
    pub area: Vec<f32>,
    /// The four neighbours, or [`WALL`].
    pub nbr: Vec<[u32; 4]>,
    /// Centre-to-centre distance to each neighbour, metres.
    pub dist: Vec<[f32; 4]>,
    /// The width of the face shared with each neighbour, metres.
    pub width: Vec<[f32; 4]>,
}

impl WaterGrid {
    /// A `w × h` rectangle of square cells `cell` metres across, its ground
    /// from `ground(x, y)`.
    pub fn flat(w: usize, h: usize, cell: f32, ground: impl Fn(usize, usize) -> f32) -> Self {
        let n = w * h;
        let mut g = WaterGrid {
            ground: Vec::with_capacity(n),
            area: vec![cell * cell; n],
            nbr: Vec::with_capacity(n),
            dist: vec![[cell; 4]; n],
            width: vec![[cell; 4]; n],
        };
        for y in 0..h {
            for x in 0..w {
                g.ground.push(ground(x, y));
                let at = |x: usize, y: usize| (y * w + x) as u32;
                g.nbr.push([
                    if x + 1 < w { at(x + 1, y) } else { WALL },
                    if x > 0 { at(x - 1, y) } else { WALL },
                    if y + 1 < h { at(x, y + 1) } else { WALL },
                    if y > 0 { at(x, y - 1) } else { WALL },
                ]);
            }
        }
        g
    }

    /// A planet of `radius` metres as a cube map of `n × n` cells a face, its
    /// ground from `ground(direction)`. Neighbours carry across the cube's
    /// edges, so water flows round the planet as it would round the ground.
    pub fn sphere(n: usize, radius: f32, ground: impl Fn(Vec3) -> f32 + Sync) -> Self {
        let n = n.max(1);
        let cells = 6 * n * n;
        let centre = |c: usize| {
            let (f, r) = (c / (n * n), c % (n * n));
            let (i, j) = (r % n, r / n);
            let u = -1.0 + (i as f32 + 0.5) * 2.0 / n as f32;
            let v = -1.0 + (j as f32 + 0.5) * 2.0 / n as f32;
            from_face(f, u, v)
        };
        let cell_of = |d: Vec3| {
            let (f, u, v) = to_face(d);
            let i = (((u + 1.0) * 0.5 * n as f32) as usize).min(n - 1);
            let j = (((v + 1.0) * 0.5 * n as f32) as usize).min(n - 1);
            f * n * n + j * n + i
        };
        let dirs: Vec<Vec3> = (0..cells).map(centre).collect();
        let ground: Vec<f32> = crate::chunks::par_map(cells, |c| ground(dirs[c]));
        let step = 2.0 / n as f32;
        let mut g = WaterGrid {
            ground,
            area: Vec::with_capacity(cells),
            nbr: Vec::with_capacity(cells),
            dist: Vec::with_capacity(cells),
            width: Vec::with_capacity(cells),
        };
        for (c, &d) in dirs.iter().enumerate() {
            let (f, u, v) = to_face(d);
            // A step of one cell along the face's own axes, taken in 3D and
            // looked up again: across an edge that lands on the next face.
            let du = from_face(f, u + step, v) - from_face(f, u - step, v);
            let dv = from_face(f, u, v + step) - from_face(f, u, v - step);
            let (tu, tv) = (du.normalize(), dv.normalize());
            let reach = 0.5 * (du.length() + dv.length()) * 0.5;
            let mut nb = [WALL; 4];
            let mut dist = [0.0; 4];
            for (k, t) in [tu, -tu, tv, -tv].into_iter().enumerate() {
                let o = cell_of((d + t * reach).normalize());
                nb[k] = if o == c { WALL } else { o as u32 };
                dist[k] = if o == c { 1.0 } else { dirs[o].angle_between(d) * radius };
            }
            g.nbr.push(nb);
            g.dist.push(dist);
            let (wu, wv) = (du.length() * 0.5 * radius, dv.length() * 0.5 * radius);
            g.width.push([wv, wv, wu, wu]);
            g.area.push(wu * wv);
        }
        g
    }

    pub fn len(&self) -> usize {
        self.ground.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ground.is_empty()
    }
}

/// Which face of the cube `d` points through, and where on it, in [-1, 1].
/// The same mapping the planet's relief grid uses.
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

#[derive(PartialEq)]
struct Lowest(f32, u32);
impl Eq for Lowest {}
impl PartialOrd for Lowest {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for Lowest {
    fn cmp(&self, o: &Self) -> Ordering {
        // A max-heap, turned round: the lowest surface first.
        o.0.total_cmp(&self.0).then(o.1.cmp(&self.1))
    }
}

/// The surface water settles at in every cell: each basin filled to the
/// height it spills at, the sea at `sea_level`.
///
/// Ground below `sea_level` is water at `sea_level`, wherever it is: the sea
/// level is a water table. A basin above it fills until it would overflow
/// towards an outlet, and stands at that height. A cell no water would rest on
/// answers its ground. With nothing below the sea level and no edge to run off,
/// the lowest cell is where water drains to, as a closed world's would, and
/// holds none itself.
///
/// Priority flood, from the outlets outwards: a cell's surface is the lowest
/// height water can reach it from an outlet at, which is its spill height.
pub fn lake_levels(grid: &WaterGrid, sea_level: f32) -> Vec<f32> {
    let n = grid.len();
    let mut level = vec![f32::INFINITY; n];
    let mut heap = BinaryHeap::new();
    let seed = |c: usize, at: f32, level: &mut [f32], heap: &mut BinaryHeap<Lowest>| {
        level[c] = at;
        heap.push(Lowest(at, c as u32));
    };
    let mut any = false;
    for c in 0..n {
        if grid.ground[c] < sea_level {
            seed(c, sea_level, &mut level, &mut heap);
            any = true;
        }
    }
    // A flat grid's edge is an outlet too: water runs off the map.
    for c in 0..n {
        if grid.nbr[c].contains(&WALL) && level[c].is_infinite() {
            seed(c, grid.ground[c], &mut level, &mut heap);
            any = true;
        }
    }
    if !any && n > 0 {
        let low = (0..n).min_by(|&a, &b| grid.ground[a].total_cmp(&grid.ground[b])).unwrap_or(0);
        seed(low, grid.ground[low], &mut level, &mut heap);
    }
    while let Some(Lowest(at, c)) = heap.pop() {
        if at > level[c as usize] {
            continue;
        }
        for &o in &grid.nbr[c as usize] {
            if o == WALL {
                continue;
            }
            let lo = at.max(grid.ground[o as usize]);
            if lo < level[o as usize] {
                level[o as usize] = lo;
                heap.push(Lowest(lo, o));
            }
        }
    }
    level
}

/// Water moving over a [`WaterGrid`].
#[derive(Clone, Debug)]
pub struct ShallowWater {
    /// Water depth in each cell, metres.
    pub depth: Vec<f32>,
    /// Outflow to each neighbour, cubic metres a second.
    flux: Vec<[f32; 4]>,
    /// Gravity, metres a second squared.
    pub gravity: f32,
    /// How much of last step's flow carries into this one, 0 to 1. Below 1 a
    /// slosh dies away rather than ringing forever.
    pub damping: f32,
}

impl ShallowWater {
    /// Water standing at `surface` (a level per cell, as [`lake_levels`]
    /// returns), still.
    pub fn at_rest(grid: &WaterGrid, surface: &[f32]) -> Self {
        let depth = grid.ground.iter().zip(surface).map(|(g, s)| (s - g).max(0.0)).collect();
        Self { depth, flux: vec![[0.0; 4]; grid.len()], gravity: 9.81, damping: 0.995 }
    }

    /// The volume of water on the grid, cubic metres.
    pub fn volume(&self, grid: &WaterGrid) -> f64 {
        self.depth.iter().zip(&grid.area).map(|(d, a)| *d as f64 * *a as f64).sum()
    }

    /// The water surface in each cell.
    pub fn surface(&self, grid: &WaterGrid) -> Vec<f32> {
        self.depth.iter().zip(&grid.ground).map(|(d, g)| d + g).collect()
    }

    /// The largest step that stays stable on this grid: a wave must not cross
    /// more than a cell a step.
    pub fn stable_dt(&self, grid: &WaterGrid) -> f32 {
        let deepest = self.depth.iter().copied().fold(0.0f32, f32::max).max(0.01);
        let shortest = grid.dist.iter().flatten().copied().fold(f32::INFINITY, f32::min);
        0.5 * shortest / (self.gravity * deepest).sqrt()
    }

    /// Advance `dt` seconds.
    ///
    /// Each pipe's flow is driven by the difference in the two surfaces, then
    /// every cell's outflow is scaled down to what it holds, so no cell goes
    /// below empty. A transfer leaves one cell and arrives in the other, so
    /// the total volume is what it was.
    pub fn step(&mut self, grid: &WaterGrid, dt: f32) {
        let n = grid.len();
        let surface: Vec<f32> = (0..n).map(|c| self.depth[c] + grid.ground[c]).collect();
        for c in 0..n {
            let mut out = 0.0f32;
            for k in 0..4 {
                let o = grid.nbr[c][k];
                let f = if o == WALL {
                    0.0
                } else {
                    let dh = surface[c] - surface[o as usize];
                    let cross = grid.width[c][k] * self.depth[c].max(self.depth[o as usize]).min(1e3);
                    (self.flux[c][k] * self.damping + dt * self.gravity * cross * dh / grid.dist[c][k]).max(0.0)
                };
                self.flux[c][k] = f;
                out += f;
            }
            let holds = self.depth[c] * grid.area[c];
            if out * dt > holds && out > 0.0 {
                let k = holds / (out * dt);
                for f in &mut self.flux[c] {
                    *f *= k;
                }
            }
        }
        // Moved as transfers: what leaves `c` towards `o` is exactly what `o`
        // receives from `c`.
        let mut dv = vec![0.0f64; n];
        for c in 0..n {
            for k in 0..4 {
                let o = grid.nbr[c][k];
                if o == WALL {
                    continue;
                }
                let v = (self.flux[c][k] * dt) as f64;
                dv[c] -= v;
                dv[o as usize] += v;
            }
        }
        for ((d, v), a) in self.depth.iter_mut().zip(&dv).zip(&grid.area) {
            *d = ((*d as f64 + v / *a as f64).max(0.0)) as f32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bowl 40 cells across with a rim, inside a wider plain that slopes off
    /// to the edge: one basin whose spill height is the rim's lowest point.
    fn bowl(rim_gap: f32) -> WaterGrid {
        WaterGrid::flat(40, 40, 1.0, |x, y| {
            let (dx, dy) = (x as f32 - 20.0, y as f32 - 20.0);
            let r = (dx * dx + dy * dy).sqrt();
            if r < 12.0 {
                r * 0.2 // the basin floor, 0 at the centre
            } else if r < 14.0 {
                // The rim, 5 m high, with a notch towards +x that is `rim_gap` lower.
                if dx > 10.0 && dy.abs() < 2.0 { 5.0 - rim_gap } else { 5.0 }
            } else {
                1.0 - (r - 14.0) * 0.05 // the plain outside, running off
            }
        })
    }

    /// **A basin fills to the height it spills at.** Every cell inside the
    /// rim stands at the notch's height, and water rests on nothing outside.
    #[test]
    fn a_basin_fills_to_its_spill_height() {
        let g = bowl(1.5);
        let level = lake_levels(&g, -100.0);
        let centre = 20 * 40 + 20;
        assert!((level[centre] - 3.5).abs() < 1e-5, "the lake stands at {}", level[centre]);
        let outside = 20 * 40 + 38;
        assert_eq!(level[outside], g.ground[outside], "water rests on the plain outside");
        // Below the sea, ground is sea at the sea's level.
        let sea = lake_levels(&g, 2.0);
        assert_eq!(sea[outside], 2.0);
    }

    /// **A lake seeded at its spill height stays put.** The settled state is
    /// where the simulation already is: nothing moves, nothing drains.
    #[test]
    fn a_settled_lake_stays_put() {
        let g = bowl(1.5);
        let level = lake_levels(&g, -100.0);
        let mut w = ShallowWater::at_rest(&g, &level);
        let v0 = w.volume(&g);
        assert!(v0 > 100.0, "the basin holds {v0} m³");
        let dt = w.stable_dt(&g);
        for _ in 0..2000 {
            w.step(&g, dt);
        }
        let worst = w.surface(&g).iter().zip(&level).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "the lake moved {worst} m");
        assert!((w.volume(&g) - v0).abs() / v0 < 1e-6);
    }

    /// **A breached dam drains, and no water is made or lost.** The rim's
    /// notch is cut to the floor: the lake runs out over the plain, and the
    /// volume on the grid is what it was until water reaches the edge.
    #[test]
    fn a_breached_dam_drains_with_its_volume_conserved() {
        let full = bowl(1.5);
        let level = lake_levels(&full, -100.0);
        let mut breached = bowl(1.5);
        for y in 18..23 {
            for x in 30..35 {
                breached.ground[y * 40 + x] = breached.ground[y * 40 + x].min(0.5);
            }
        }
        let mut w = ShallowWater::at_rest(&breached, &level);
        let v0 = w.volume(&breached);
        let centre = 20 * 40 + 20;
        let d0 = w.depth[centre];
        let dt = w.stable_dt(&breached);
        for _ in 0..300 {
            w.step(&breached, dt);
        }
        let v = w.volume(&breached);
        assert!((v - v0).abs() / v0 < 0.01, "volume went from {v0} to {v}");
        assert!(w.depth[centre] < d0 - 0.2, "the lake did not drain: {} of {d0}", w.depth[centre]);
        assert!(w.depth[20 * 40 + 36] > 0.0, "nothing came through the breach");
    }

    /// The cube map's neighbours are mutual and round the planet: every cell's
    /// neighbour names it back, across the cube's edges too, and the areas add
    /// up to the sphere's.
    #[test]
    fn a_planet_grid_joins_up_across_the_cube() {
        let r = 100.0;
        let g = WaterGrid::sphere(24, r, |_| 0.0);
        assert_eq!(g.len(), 6 * 24 * 24);
        for c in 0..g.len() {
            for &o in &g.nbr[c] {
                assert_ne!(o, WALL, "a planet has no edge");
                assert!(g.nbr[o as usize].contains(&(c as u32)), "{o} does not name {c} back");
            }
        }
        let area: f32 = g.area.iter().sum();
        let sphere = 4.0 * std::f32::consts::PI * r * r;
        assert!((area - sphere).abs() / sphere < 0.03, "{area} against {sphere}");
    }

    /// On a planet, a crater fills to its rim and stays full, across the
    /// cube's edges, while the planet drains to a deeper pit elsewhere.
    #[test]
    fn a_crater_on_a_planet_holds_its_lake() {
        let crater = Vec3::new(0.3, 1.0, 0.2).normalize();
        let pit = -crater;
        let ground = |d: Vec3| {
            let a = d.angle_between(crater);
            if d.angle_between(pit) < 0.2 {
                -20.0
            } else if a < 0.25 {
                -6.0 + a * 10.0
            } else if a < 0.3 {
                2.0
            } else {
                0.0
            }
        };
        let g = WaterGrid::sphere(32, 200.0, ground);
        let level = lake_levels(&g, -100.0);
        let inside = (0..g.len()).filter(|&c| g.ground[c] < -1.0 && g.ground[c] > -10.0).collect::<Vec<_>>();
        assert!(!inside.is_empty());
        for &c in &inside {
            assert!((level[c] - 2.0).abs() < 1e-5, "the crater stands at {} not its rim", level[c]);
        }
        let mut w = ShallowWater::at_rest(&g, &level);
        let v0 = w.volume(&g);
        assert!(v0 > 0.0, "the crater holds nothing");
        let dt = w.stable_dt(&g);
        for _ in 0..500 {
            w.step(&g, dt);
        }
        assert!((w.volume(&g) - v0).abs() / v0 < 1e-5);
        let worst = w.surface(&g).iter().zip(&level).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 1e-3, "the crater's lake moved {worst} m");
    }
}
