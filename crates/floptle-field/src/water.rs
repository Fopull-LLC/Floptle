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
        let cell_of = |d: Vec3| sphere_cell(n, d);
        let dirs: Vec<Vec3> = (0..cells).map(|c| sphere_centre(n, c)).collect();
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

/// The direction through cube face `f` at `(u, v)`, both in [-1, 1]: where a
/// [`WaterGrid::sphere`] cell is. Cell `c` of an `n`-cell face grid is face
/// `c / n²`, at column `c % n` and row `(c % n²) / n`.
pub fn face_dir(f: usize, u: f32, v: f32) -> Vec3 {
    from_face(f, u, v)
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
    flood(grid, sea_level).0
}

/// Where water runs from every cell: the neighbour it drains into, or
/// [`WALL`] at an outlet (the sea, a flat grid's edge, or a closed world's
/// lowest cell). Following it from any cell reaches an outlet without ever
/// climbing above the cell's [`lake_levels`] surface: down the ground, across
/// each lake it fills toward the lake's spill point, and on.
pub fn drainage(grid: &WaterGrid, sea_level: f32) -> Vec<u32> {
    flood(grid, sea_level).1
}

/// Priority flood, from the outlets outwards: a cell's surface is the lowest
/// height water can reach it from an outlet at, which is its spill height, and
/// the cell it was reached from is the one it drains into.
fn flood(grid: &WaterGrid, sea_level: f32) -> (Vec<f32>, Vec<u32>) {
    let n = grid.len();
    let mut level = vec![f32::INFINITY; n];
    let mut from = vec![WALL; n];
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
                from[o as usize] = c;
                heap.push(Lowest(lo, o));
            }
        }
    }
    (level, from)
}

/// The cell of an `n`-cell-a-face [`WaterGrid::sphere`] that direction `d`
/// falls in.
pub fn sphere_cell(n: usize, d: Vec3) -> usize {
    let n = n.max(1);
    let (f, u, v) = to_face(d);
    let i = (((u + 1.0) * 0.5 * n as f32) as usize).min(n - 1);
    let j = (((v + 1.0) * 0.5 * n as f32) as usize).min(n - 1);
    f * n * n + j * n + i
}

/// The direction through the middle of cell `c` of an `n`-cell-a-face
/// [`WaterGrid::sphere`].
pub fn sphere_centre(n: usize, c: usize) -> Vec3 {
    let n = n.max(1);
    let (f, r) = (c / (n * n), c % (n * n));
    let (i, j) = (r % n, r / n);
    from_face(f, -1.0 + (i as f32 + 0.5) * 2.0 / n as f32, -1.0 + (j as f32 + 0.5) * 2.0 / n as f32)
}

/// A surface difference below this is level, metres.
const LEVEL: f32 = 1e-4;
/// A flow below this is still, cubic metres a second.
const STILL: f32 = 1e-4;
/// Steps a cell must be still for before it sleeps.
const SLEEP_STEPS: u8 = 30;
/// A depth change below this in a step is steady, metres: a cell flowing at
/// a steady rate for [`SLEEP_STEPS`] freezes with its flow kept.
const STEADY: f32 = 2e-6;
/// How far a frozen cell's depth may drift from where it froze before it is
/// solved again, metres.
const DRIFT: f32 = 1e-3;
/// The broad-crested weir coefficient: the most a face passes where water
/// pours off an edge is `C·w·√(2g)·h^{3/2}`.
const WEIR: f32 = 0.385;

/// The depth a river runs at in steady flow: Manning's equation for a wide
/// channel, `h = (Q·n / (w·√S))^{3/5}`. `q` is the discharge in m³/s, `width`
/// the bed's width in metres, `slope` its fall per metre and `n` the bed's
/// roughness (0.035 is a natural river). The speed is then `Q / (w·h)`.
pub fn manning_depth(q: f32, width: f32, slope: f32, n: f32) -> f32 {
    (q.max(0.0) * n / (width.max(1e-6) * slope.max(1e-9).sqrt())).powf(0.6)
}

/// Where flowing water pours off a drop: see [`ShallowWater::falls`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fall {
    /// The cell the water leaves, and which of its faces it pours through.
    pub cell: u32,
    pub face: u8,
    /// The flow over the edge, m³/s.
    pub discharge: f32,
    /// From the water surface at the top to the surface at the foot, metres.
    pub drop: f32,
    /// The width of the edge it pours over, metres.
    pub width: f32,
}

/// Water moving over a [`WaterGrid`].
///
/// Only cells that are **awake** are stepped. A cell wakes when something
/// asks it to ([`Self::wake`]), when water reaches it, or when a sleeping
/// cell's surface stands above an awake neighbour's; it sleeps again once it
/// has been still for a while. A settled sea costs nothing a step, and a
/// breach costs only the water it sets moving.
#[derive(Clone, Debug)]
pub struct ShallowWater {
    /// Water depth in each cell, metres.
    pub depth: Vec<f32>,
    /// Outflow to each neighbour, cubic metres a second. Zero while asleep.
    flux: Vec<[f32; 4]>,
    /// Gravity, metres a second squared.
    pub gravity: f32,
    /// How much of last step's flow carries into this one, 0 to 1. Below 1 a
    /// slosh dies away rather than ringing forever.
    pub damping: f32,
    awake: Vec<bool>,
    /// Steps each awake cell has been still for.
    calm: Vec<u8>,
    /// The awake cells.
    list: Vec<u32>,
    /// Per-cell volume change in a step, and the cells it touched.
    dv: Vec<f64>,
    touched: Vec<u32>,
    /// The bed's Manning roughness. Above 0 the bed holds the water back
    /// as a river bed does, and a river settles at [`manning_depth`]; it then
    /// does [`Self::damping`]'s job, which is skipped. 0 (the default) is the
    /// frictionless lake model.
    pub roughness: f32,
    /// Springs: a cell and what it gives, m³/s.
    springs: Vec<(u32, f32)>,
    /// A fixed surface level per cell (NaN = none): the sea at a coast, a
    /// river's mouth. What flows in or out there is the boundary's.
    hold: Vec<f32>,
    /// Volume the springs and held levels have added (less what they took),
    /// cubic metres: the volume now is the volume at the start plus this.
    exchanged: f64,
    /// Cells flowing at a steady rate, frozen: they pass their flow on each
    /// step but are not solved. Their depth where they froze.
    frozen: Vec<bool>,
    rails: Vec<u32>,
    froze_at: Vec<f32>,
    /// Cells whose depth moved more than [`STEADY`] this step.
    moved: Vec<bool>,
    /// Cells solved in the last step: what it cost.
    solved: usize,
}

impl ShallowWater {
    /// Water standing at `surface` (a level per cell, as [`lake_levels`]
    /// returns), still, and asleep.
    pub fn at_rest(grid: &WaterGrid, surface: &[f32]) -> Self {
        let n = grid.len();
        let depth = grid.ground.iter().zip(surface).map(|(g, s)| (s - g).max(0.0)).collect();
        Self {
            depth,
            flux: vec![[0.0; 4]; n],
            gravity: 9.81,
            damping: 0.995,
            awake: vec![false; n],
            calm: vec![0; n],
            list: Vec::new(),
            dv: vec![0.0; n],
            touched: Vec::new(),
            roughness: 0.0,
            springs: Vec::new(),
            hold: vec![f32::NAN; n],
            exchanged: 0.0,
            frozen: vec![false; n],
            rails: Vec::new(),
            froze_at: vec![0.0; n],
            moved: vec![false; n],
            solved: 0,
        }
    }

    /// Wake cell `c`: something changed there (a dig, a spring, a splash).
    /// A frozen cell is solved again, its flow kept.
    pub fn wake(&mut self, c: usize) {
        if let Some(a) = self.awake.get_mut(c) {
            self.calm[c] = 0;
            if !*a {
                *a = true;
                self.list.push(c as u32);
            }
            if self.frozen[c] {
                self.frozen[c] = false;
                if let Some(i) = self.rails.iter().position(|&r| r as usize == c) {
                    self.rails.swap_remove(i);
                }
            }
        }
    }

    /// A spring in cell `c` giving `q` cubic metres a second: a river's head.
    pub fn add_spring(&mut self, c: usize, q: f32) {
        if c < self.depth.len() {
            self.springs.push((c as u32, q));
            self.wake(c);
        }
    }

    /// Hold cell `c`'s surface at `level`, whatever flows in or out: the sea
    /// along a coast, so a trench dug inland from the beach fills from it and
    /// stays full, or a river's mouth. `NaN` lets it go again.
    pub fn hold_level(&mut self, c: usize, level: f32) {
        if c < self.depth.len() {
            self.hold[c] = level;
            self.wake(c);
        }
    }

    /// Seed the flow out of cell `c` through face `k`, m³/s: a river started
    /// at its steady state rather than from still water.
    pub fn set_flow(&mut self, c: usize, k: usize, q: f32) {
        if c < self.flux.len() && k < 4 {
            self.flux[c][k] = q.max(0.0);
            self.wake(c);
        }
    }

    /// Volume the springs and held levels have added, less what they took,
    /// cubic metres.
    pub fn exchanged(&self) -> f64 {
        self.exchanged
    }

    /// How many cells the last step solved. Frozen cells pass their flow on
    /// without being solved and are not counted.
    pub fn solved_count(&self) -> usize {
        self.solved
    }

    /// How many cells are frozen: flowing steadily, not solved.
    pub fn frozen_count(&self) -> usize {
        self.rails.len()
    }

    /// Every place flowing water pours off a drop of more than `min_drop`
    /// metres of ground from one cell to the next: where a waterfall goes.
    pub fn falls(&self, grid: &WaterGrid, min_drop: f32) -> Vec<Fall> {
        let mut out = Vec::new();
        for &c in self.list.iter().chain(&self.rails) {
            let c = c as usize;
            for k in 0..4 {
                let o = grid.nbr[c][k];
                let q = self.flux[c][k];
                if o == WALL || q < STILL || grid.ground[c] - grid.ground[o as usize] <= min_drop {
                    continue;
                }
                let o = o as usize;
                out.push(Fall {
                    cell: c as u32,
                    face: k as u8,
                    discharge: q,
                    drop: (self.depth[c] + grid.ground[c]) - (self.depth[o] + grid.ground[o]),
                    width: grid.width[c][k],
                });
            }
        }
        out
    }

    /// Wake every cell.
    pub fn wake_all(&mut self) {
        for c in 0..self.depth.len() {
            self.wake(c);
        }
    }

    /// How many cells are awake: what a step costs.
    pub fn awake_count(&self) -> usize {
        self.list.len()
    }

    /// The volume of water on the grid, cubic metres.
    pub fn volume(&self, grid: &WaterGrid) -> f64 {
        self.depth.iter().zip(&grid.area).map(|(d, a)| *d as f64 * *a as f64).sum()
    }

    /// The water surface in each cell.
    pub fn surface(&self, grid: &WaterGrid) -> Vec<f32> {
        self.depth.iter().zip(&grid.ground).map(|(d, g)| d + g).collect()
    }

    /// The water's velocity in cell `c` along the grid's two axes, m/s:
    /// `[toward neighbour 0 (+x), toward neighbour 2 (+y)]`. The flow through
    /// each pair of opposite faces, out and in, averaged and divided by the
    /// face the water stands on. Zero in a dry cell.
    pub fn velocity(&self, grid: &WaterGrid, c: usize) -> [f32; 2] {
        let h = self.depth[c];
        if h < 1e-3 {
            return [0.0; 2];
        }
        // What neighbour `o` sends back toward `c`.
        let back = |o: u32| -> f32 {
            if o == WALL {
                return 0.0;
            }
            let o = o as usize;
            grid.nbr[o].iter().position(|&x| x as usize == c).map_or(0.0, |k| self.flux[o][k])
        };
        let axis = |plus: usize, minus: usize| {
            let (po, mo) = (grid.nbr[c][plus], grid.nbr[c][minus]);
            // Out through the + face, in through the − face, less the reverse.
            let q = 0.5 * ((self.flux[c][plus] - back(po)) + (back(mo) - self.flux[c][minus]));
            q / (grid.width[c][plus].max(1e-6) * h)
        };
        [axis(0, 1), axis(2, 3)]
    }

    /// A step that stays stable on this grid at its current deepest water.
    ///
    /// Four neighbours pouring into one cell overshoot it, and the overshoot
    /// grows, once `dt` reaches `l / (2·√(g·h))`. This is half that. Water
    /// that gets deeper than it was (a hole filling) needs it asked again.
    pub fn stable_dt(&self, grid: &WaterGrid) -> f32 {
        let deepest = self.depth.iter().copied().fold(0.0f32, f32::max).max(0.01);
        let shortest = grid.dist.iter().flatten().copied().fold(f32::INFINITY, f32::min);
        0.25 * shortest / (self.gravity * deepest).sqrt()
    }

    /// Advance the awake cells `dt` seconds.
    ///
    /// Each pipe's flow is driven by the difference in the two surfaces, then
    /// every cell's outflow is scaled down to what it holds, so no cell goes
    /// below empty. A transfer leaves one cell and arrives in the other, so
    /// the total volume is what it was.
    pub fn step(&mut self, grid: &WaterGrid, dt: f32) {
        let surf = |w: &Self, c: usize| w.depth[c] + grid.ground[c];
        // A sleeping cell standing above an awake one joins in, or water
        // would pile up against it as against a wall.
        for i in 0..self.list.len() {
            let c = self.list[i] as usize;
            for &o in &grid.nbr[c] {
                // A frozen cell already passes its water on.
                if o != WALL
                    && !self.awake[o as usize]
                    && !self.frozen[o as usize]
                    && self.depth[o as usize] > 0.0
                    && surf(self, o as usize) > surf(self, c) + LEVEL
                {
                    self.wake(o as usize);
                }
            }
        }
        for &c in &self.list {
            let c = c as usize;
            let mut out = 0.0f32;
            for k in 0..4 {
                let o = grid.nbr[c][k];
                let f = if o == WALL {
                    0.0
                } else {
                    let o = o as usize;
                    let dh = (self.depth[c] + grid.ground[c]) - (self.depth[o] + grid.ground[o]);
                    let h = self.depth[c].max(self.depth[o]).min(1e3);
                    let cross = grid.width[c][k] * h;
                    let n = self.roughness;
                    let damp = if n > 0.0 { 1.0 } else { self.damping };
                    let mut f = self.flux[c][k] * damp + dt * self.gravity * cross * dh / grid.dist[c][k];
                    // The bed's drag, taken implicitly so it is stable at
                    // any depth: in steady flow it balances the surface's
                    // fall, which is Manning's equation.
                    if n > 0.0 && h > 1e-4 {
                        let u = self.flux[c][k].abs() / cross.max(1e-6);
                        f /= 1.0 + dt * self.gravity * n * n * u / h.powf(4.0 / 3.0);
                    }
                    // Water pouring off an edge passes no more than a weir
                    // does, however far below the foot is: what keeps a
                    // river stable over a cliff.
                    let pours = self.depth[o] + grid.ground[o] < grid.ground[c];
                    if pours {
                        let weir = WEIR * grid.width[c][k] * (2.0 * self.gravity).sqrt() * self.depth[c].powf(1.5);
                        f = f.min(weir);
                    }
                    f.max(0.0)
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
        self.solved = self.list.len();
        // Springs give, as a transfer from outside the grid.
        for i in 0..self.springs.len() {
            let (c, q) = self.springs[i];
            let v = (q * dt) as f64;
            if self.dv[c as usize] == 0.0 {
                self.touched.push(c);
            }
            self.dv[c as usize] += v;
            self.exchanged += v;
        }
        // Moved as transfers: what leaves `c` towards `o` is exactly what `o`
        // receives from `c`. Frozen cells pass on their kept flow the same way.
        for i in 0..self.list.len() + self.rails.len() {
            let c = if i < self.list.len() { self.list[i] } else { self.rails[i - self.list.len()] } as usize;
            for k in 0..4 {
                let o = grid.nbr[c][k];
                let v = (self.flux[c][k] * dt) as f64;
                if o == WALL || v == 0.0 {
                    continue;
                }
                for x in [c, o as usize] {
                    if self.dv[x] == 0.0 {
                        self.touched.push(x as u32);
                    }
                }
                self.dv[c] -= v;
                self.dv[o as usize] += v;
            }
        }
        for i in 0..self.touched.len() {
            let c = self.touched[i] as usize;
            let v = std::mem::take(&mut self.dv[c]);
            let before = self.depth[c];
            let mut d = (before as f64 + v / grid.area[c] as f64).max(0.0);
            // A held level takes or gives whatever keeps it there.
            let hold = self.hold[c];
            if hold.is_finite() {
                let want = (hold - grid.ground[c]).max(0.0) as f64;
                self.exchanged += (want - d) * grid.area[c] as f64;
                d = want;
            }
            self.depth[c] = d as f32;
            let moved = (self.depth[c] - before).abs() > STEADY;
            self.moved[c] = moved;
            if self.frozen[c] {
                // A frozen cell is solved again once what reaches it has
                // changed enough to move it.
                if (self.depth[c] - self.froze_at[c]).abs() > DRIFT {
                    self.wake(c);
                }
            } else if v > (STILL * dt) as f64 && (moved || !self.awake[c]) {
                // Water arriving wakes a cell, but not rounding noise: a sea
                // whose level differs in the last bit would otherwise wake
                // whole.
                self.wake(c);
            }
        }
        self.touched.clear();
        // An awake cell whose water moved wakes the frozen cells beside it:
        // a dig downstream changes what a frozen reach should pass on.
        for i in 0..self.list.len() {
            let c = self.list[i] as usize;
            if self.moved[c] {
                for k in 0..4 {
                    let o = grid.nbr[c][k];
                    if o != WALL && self.frozen[o as usize] {
                        self.wake(o as usize);
                    }
                }
            }
        }
        // Still cells sleep: no flow out, and level with every neighbour that
        // has water to give or take.
        let mut i = 0;
        while i < self.list.len() {
            let c = self.list[i] as usize;
            let still = self.flux[c].iter().all(|f| *f < STILL)
                && grid.nbr[c].iter().all(|&o| {
                    o == WALL || {
                        let (sc, so) = (surf(self, c), surf(self, o as usize));
                        let higher_dry = if sc > so { self.depth[c] == 0.0 } else { self.depth[o as usize] == 0.0 };
                        higher_dry || (sc - so).abs() < LEVEL
                    }
                });
            // Flowing, but at a rate that no longer changes anything: a river
            // in its steady state. A spring or a held level keeps solving, so
            // the water it adds or takes is always the current answer.
            let steady = !still
                && !self.moved[c]
                && !self.hold[c].is_finite()
                && !self.springs.iter().any(|s| s.0 as usize == c);
            self.moved[c] = false;
            self.calm[c] = if still || steady { self.calm[c].saturating_add(1) } else { 0 };
            if self.calm[c] >= SLEEP_STEPS {
                self.awake[c] = false;
                self.list.swap_remove(i);
                if still {
                    self.flux[c] = [0.0; 4];
                } else {
                    self.frozen[c] = true;
                    self.froze_at[c] = self.depth[c];
                    self.rails.push(c as u32);
                }
            } else {
                i += 1;
            }
        }
        for &c in &self.rails {
            self.moved[c as usize] = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **Water flowing down a channel has a velocity, and still water has
    /// none.** A tilted channel fed from its high end runs toward the low end;
    /// a settled pool reads zero.
    #[test]
    fn flowing_water_has_a_velocity() {
        let flat = WaterGrid::flat(20, 3, 1.0, |_, _| 0.0);
        let mut pool = ShallowWater::at_rest(&flat, &vec![1.0; flat.len()]);
        pool.wake_all();
        for _ in 0..40 {
            pool.step(&flat, 0.02);
        }
        assert_eq!(pool.velocity(&flat, 30), [0.0, 0.0], "still water is still");
        // A thin film over a slope, falling toward +x.
        let grid = WaterGrid::flat(20, 3, 1.0, |x, _| 2.0 - x as f32 * 0.1);
        let film: Vec<f32> = grid.ground.iter().map(|g| g + 0.2).collect();
        let mut w = ShallowWater::at_rest(&grid, &film);
        w.wake_all();
        for _ in 0..40 {
            w.step(&grid, 0.02);
        }
        let v = w.velocity(&grid, 30);
        assert!(v[0] > 0.05 && v[1].abs() < 0.05, "water down the channel moves at {v:?}");
    }

    /// A straight river bed: `rivers` channels `len` cells long and one cell
    /// (2 m) wide, falling `slope` a metre toward +x, each between banks 5 m
    /// high. Seeded at its Manning depth and flow for `q` m³/s, a spring at
    /// its head and its mouth held at the river's own level.
    fn rivers(n_rivers: usize, len: usize, slope: f32, q: f32, cliff: Option<(usize, f32)>) -> (WaterGrid, ShallowWater, f32) {
        let rows = n_rivers * 2 + 1;
        let fall = move |x: usize| {
            let step = cliff.map_or(0.0, |(at, h)| if x >= at { h } else { 0.0 });
            20.0 - x as f32 * 2.0 * slope - step
        };
        let grid = WaterGrid::flat(len, rows, 2.0, |x, y| if y % 2 == 1 { fall(x) } else { fall(x) + 5.0 });
        let h = manning_depth(q, 2.0, slope, 0.035);
        let seeded: Vec<f32> = (0..grid.len())
            .map(|c| if (c / len) % 2 == 1 && cliff.is_none() { grid.ground[c] + h } else { grid.ground[c] })
            .collect();
        let mut w = ShallowWater::at_rest(&grid, &seeded);
        w.roughness = 0.035;
        for r in 0..n_rivers {
            let row = (2 * r + 1) * len;
            if cliff.is_none() {
                for x in 0..len - 1 {
                    w.set_flow(row + x, 0, q);
                }
            }
            w.add_spring(row, q);
            let mouth = row + len - 1;
            w.hold_level(mouth, grid.ground[mouth] + h);
        }
        (grid, w, h)
    }

    /// **A river seeded at its steady state stays there.** A fresh world's
    /// rivers start at Manning's normal depth with their flow already
    /// moving, and a minute of simulation changes no river surface by more
    /// than 10 cm. The water runs at `Q / (w·h)`.
    #[test]
    fn a_river_seeded_steady_stays_steady() {
        let (grid, mut w, h) = rivers(1, 60, 0.01, 2.0, None);
        let start = w.surface(&grid);
        let v0 = w.volume(&grid);
        let dt = w.stable_dt(&grid);
        let mut t = 0.0;
        while t < 60.0 {
            w.step(&grid, dt);
            t += dt;
        }
        let worst = w.surface(&grid).iter().zip(&start).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(worst < 0.1, "a steady river moved {worst} m in a minute (normal depth {h:.3} m)");
        let mid = 60 + 30;
        let want = 2.0 / (2.0 * h);
        let u = w.velocity(&grid, mid)[0];
        assert!((u - want).abs() / want < 0.15, "the river runs at {u:.3} m/s, not Q/(w·h) = {want:.3}");
        let drift = (w.volume(&grid) - v0 - w.exchanged()).abs();
        assert!(drift < 1e-3 * v0, "{drift} m³ of water appeared or vanished");
    }

    /// **Twenty steady rivers cost a handful of cells a step.** A reach flowing
    /// at a rate that no longer changes freezes with its flow kept and is not
    /// solved; only the springs, the mouths and what is still changing are.
    #[test]
    fn twenty_steady_rivers_solve_only_a_few_cells() {
        let (grid, mut w, _) = rivers(20, 300, 0.01, 2.0, None);
        let v0 = w.volume(&grid);
        let dt = w.stable_dt(&grid);
        for _ in 0..(30.0 / dt) as usize {
            w.step(&grid, dt);
        }
        assert!(w.frozen_count() > 5_000, "only {} of 6,000 river cells froze", w.frozen_count());
        assert!(w.solved_count() <= 2_000, "a step still solves {} cells", w.solved_count());
        let drift = (w.volume(&grid) - v0 - w.exchanged()).abs();
        assert!(drift < 1e-3 * v0, "{drift} m³ of water appeared or vanished");
    }

    /// **The sea is a boundary.** A trench dug from the beach inland, below
    /// sea level, fills from the sea and stays full: the sea's level is held,
    /// so a small sea does not drop to fill it.
    #[test]
    fn a_trench_dug_from_the_beach_fills_from_the_sea() {
        let mut grid = WaterGrid::flat(30, 5, 1.0, |x, _| if x < 10 { -3.0 } else { 0.2 * (x as f32 - 9.0) });
        let level = lake_levels(&grid, 0.0);
        let mut w = ShallowWater::at_rest(&grid, &level);
        for y in 0..5 {
            for x in 0..3 {
                w.hold_level(y * 30 + x, 0.0);
            }
        }
        let v0 = w.volume(&grid);
        for x in 10..26 {
            let c = 2 * 30 + x;
            grid.ground[c] = -1.0;
            w.wake(c);
            w.wake(c - 1);
        }
        for _ in 0..2 {
            let dt = w.stable_dt(&grid);
            for _ in 0..(60.0 / dt) as usize {
                w.step(&grid, dt);
            }
        }
        let surface = w.surface(&grid);
        for x in 10..26 {
            let s = surface[2 * 30 + x];
            assert!(s.abs() < 0.02, "the trench at {x} m stands at {s:.3}, not the sea's 0");
        }
        assert!(w.exchanged() > 10.0, "the sea gave only {:.2} m³ to a 16 m³ trench", w.exchanged());
        let drift = (w.volume(&grid) - v0 - w.exchanged()).abs();
        assert!(drift < 1e-3, "{drift} m³ of water appeared or vanished");
    }

    /// **A river over a 10 m cliff is stable, and the cliff is a fall.** The
    /// water pouring off the edge passes what a weir does, so the lip runs at
    /// a steady depth at the ordinary step rather than emptying and filling
    /// every step; `falls` reports the drop with the river's discharge.
    #[test]
    fn a_river_over_a_cliff_is_stable_and_reports_its_fall() {
        let q = 2.0;
        let (grid, mut w, _) = rivers(1, 40, 0.01, q, Some((20, 10.0)));
        let lip = 40 + 19;
        let mut lips = Vec::new();
        let mut t = 0.0;
        while t < 240.0 {
            let dt = w.stable_dt(&grid);
            w.step(&grid, dt);
            t += dt;
            if t > 220.0 {
                lips.push(w.depth[lip]);
            }
        }
        assert!(w.depth.iter().all(|d| d.is_finite() && *d < 3.0), "the river blew up");
        let weir_depth = (q / (WEIR * 2.0 * (2.0 * 9.81f32).sqrt())).powf(2.0 / 3.0);
        let (lo, hi) = lips.iter().fold((f32::INFINITY, 0.0f32), |(a, b), d| (a.min(*d), b.max(*d)));
        assert!(
            (lo - weir_depth).abs() / weir_depth < 0.25 && (hi - lo) < 0.05 * weir_depth,
            "the lip ran between {lo:.3} and {hi:.3} m, not steady at the weir's {weir_depth:.3}"
        );
        let falls = w.falls(&grid, 1.5);
        assert_eq!(falls.len(), 1, "falls: {falls:?}");
        let f = falls[0];
        assert_eq!((f.cell as usize, f.face), (lip, 0));
        assert!((f.discharge - q).abs() / q < 0.05, "{} m³/s over the edge, not {q}", f.discharge);
        assert!(f.drop > 9.0 && f.drop < 11.0, "a {} m drop", f.drop);
    }

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
        // Every cell woken, so this asks the step and not the sleep.
        w.wake_all();
        let dt = w.stable_dt(&g);
        for _ in 0..2000 {
            w.step(&g, dt);
        }
        assert_eq!(w.awake_count(), 0, "a settled lake should fall asleep");
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
        // Only the breach is woken: the rest of the lake has to join in.
        for y in 18..23 {
            for x in 30..35 {
                w.wake(y * 40 + x);
            }
        }
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

    /// **A hole dug in a lake floor fills from the lake.** Only the dug cell
    /// is woken, and it is lower than everything round it, so the lake that
    /// flows into it has to wake itself: water standing above a moving cell
    /// joins in. The hole fills, the lake drops a little, and nothing is lost.
    #[test]
    fn a_hole_dug_in_a_lake_floor_fills_from_the_lake() {
        let mut g = bowl(1.5);
        let level = lake_levels(&g, -100.0);
        let mut w = ShallowWater::at_rest(&g, &level);
        let v0 = w.volume(&g);
        let hole = 20 * 40 + 15;
        g.ground[hole] -= 3.0;
        w.wake(hole);
        // Asked after the dig: the hole's water is deeper than the lake's.
        let dt = w.stable_dt(&g);
        for _ in 0..1500 {
            w.step(&g, dt);
        }
        let s = w.surface(&g);
        assert!((s[hole] - s[20 * 40 + 20]).abs() < 0.01, "the hole stands at {} and the lake at {}", s[hole], s[20 * 40 + 20]);
        assert!(s[20 * 40 + 20] < 3.5, "the lake gave nothing to the hole");
        assert!((w.volume(&g) - v0).abs() / v0 < 1e-5);
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
        w.wake_all();
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
