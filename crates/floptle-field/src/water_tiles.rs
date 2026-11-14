//! Simulated water stored only where it is: square tiles of cells on a patch
//! of a round world or of a plane, made where water reaches and dropped
//! again where it has dried, each one stepped only while its water moves.
//!
//! A patch can be hundreds of metres across at a metre or two a cell, a
//! quarter of a million cells, of which a river or a flooded trench wets a
//! few thousand. Only the tiles holding those exist; a tile whose water has
//! come to rest sleeps and costs nothing a step, and a sleeping tile wakes
//! when water beside it moves, when its ground is dug, or when a script
//! holds, releases or adds water in it.
//!
//! Two models move the water, over the same cells, faces and boundaries:
//!
//! * [`Solver::Swe`], the default: the shallow-water equations with
//!   momentum. Depth lives at each cell's centre and velocity on each face
//!   between two cells; the surface's slope accelerates the water, and the
//!   water carries its own speed along with it, so a breach sends a surge
//!   down a channel, a bore runs up a beach and a jet keeps going past the
//!   end of a weir.
//! * [`Solver::Pipes`]: the same without carrying momentum, the cheaper
//!   model for calm water that only has to find its level.
//!
//! Either way the water a face passes leaves one cell and arrives in the
//! other, and no cell gives more than it holds, so the volume is what it was
//! apart from what springs, held cells and scripts add or take, which is
//! counted.

use std::collections::HashMap;
use std::sync::Arc;

use floptle_core::water::{tile_key, Patch, TileLevels, TiledSurface, TILE};

const CELLS: usize = TILE * TILE;
/// No tile there.
const NONE: u32 = u32::MAX;
/// The least a face's water depth is divided by, metres.
const DRY: f32 = 1e-4;
/// Depth read back as water rather than a damp cell, metres.
pub const WET: f32 = 1e-3;
/// A surface difference below this is level, metres.
const LEVEL: f32 = 1e-4;
/// A face slower than this is still, m/s.
const STILL: f32 = 1e-3;
/// A depth change below this in a step is no change, metres.
const STEADY: f32 = 2e-6;
/// Steps a tile must be still for before it sleeps.
const SLEEP_STEPS: u16 = 30;
/// The broad-crested weir coefficient: the most a face passes where water
/// pours off an edge is `C·w·√(2g)·h^{3/2}`.
const WEIR: f32 = 0.385;
/// The Manning roughness of the smoothest bed water with momentum runs over.
const SMOOTH_BED: f32 = 0.012;
/// The fastest water moves, m/s: what keeps a fall of hundreds of metres
/// from blowing up a step.
const FASTEST: f32 = 40.0;
/// Steps a dry, sleeping tile is kept for before it is dropped.
const EVICT_STEPS: u32 = 600;

/// How the water moves: see the module docs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Solver {
    #[default]
    Swe,
    Pipes,
}

/// The four sides of a tile, and of a cell.
const EAST: usize = 0;
const WEST: usize = 1;
const NORTH: usize = 2;
const SOUTH: usize = 3;

/// The cells a side of a tile's copy with the ring round it.
const PADW: usize = TILE + 2;
/// Tiles awake at once past which a step is shared out across threads.
const PARALLEL_TILES: usize = 8;

/// A tile and the ring of cells round it, for the face solve: depth,
/// ground, both face velocities and both face flows per metre of face.
struct Pad {
    ground: Vec<f32>,
    depth: Vec<f32>,
    u: Vec<f32>,
    v: Vec<f32>,
    qx: Vec<f32>,
    qz: Vec<f32>,
}

impl Pad {
    fn dry() -> Self {
        let n = PADW * PADW;
        Self { ground: vec![f32::INFINITY; n], depth: vec![0.0; n], u: vec![0.0; n], v: vec![0.0; n], qx: vec![0.0; n], qz: vec![0.0; n] }
    }

    fn copy(&mut self, i: usize, t: &Tile, c: usize) {
        self.ground[i] = t.ground[c];
        self.depth[i] = t.depth[c];
        self.u[i] = t.u[c];
        self.v[i] = t.v[c];
        self.qx[i] = t.qx[c] / t.wx[c].max(1e-6);
        self.qz[i] = t.qz[c] / t.wz[c].max(1e-6);
    }
}

#[derive(Clone)]
struct Tile {
    ti: usize,
    tj: usize,
    /// Ground height above the patch's reference; infinite where there is no
    /// cell (past the patch's edge) or no ground to stand water on.
    ground: Vec<f32>,
    depth: Vec<f32>,
    /// Velocity on each cell's +column face (towards `(i + 1, j)`) and +row
    /// face (towards `(i, j + 1)`), m/s, and the next step's.
    u: Vec<f32>,
    v: Vec<f32>,
    un: Vec<f32>,
    vn: Vec<f32>,
    /// What each of those faces passes this step, m³/s.
    qx: Vec<f32>,
    qz: Vec<f32>,
    /// The share of its outflow each cell can give this step.
    scale: Vec<f32>,
    /// Held surface level per cell (NaN = free) and the group it counts in.
    hold: Vec<f32>,
    group: Vec<u16>,
    held: u32,
    /// Each cell's area, the distance to its +column and +row neighbours
    /// and the width of the faces between.
    area: Vec<f32>,
    dx: Vec<f32>,
    dz: Vec<f32>,
    wx: Vec<f32>,
    wz: Vec<f32>,
    nbr: [u32; 4],
    awake: bool,
    calm: u16,
    /// Steps it has been dry and asleep.
    idle: u32,
    /// Anything a reader sees changed since it was last published.
    changed: bool,
    springs: u32,
    serial: u64,
    published: Option<Arc<TileLevels>>,
}

/// Simulated water on a [`Patch`], in tiles. See the module docs.
#[derive(Clone)]
pub struct TiledWater {
    pub patch: Patch,
    pub solver: Solver,
    pub gravity: f32,
    /// The bed's Manning roughness: 0 is the frictionless lake, about 0.035
    /// a natural river bed.
    pub roughness: f32,
    /// The share of its speed water loses a second when `roughness` is 0,
    /// so a slosh dies away rather than ringing.
    pub damping: f32,
    /// The least depth that moves, and that reads back as water, metres:
    /// a film thinner than this on a slope stays put, unseen.
    pub min_depth: f32,
    tiles: Vec<Tile>,
    index: HashMap<u32, u32>,
    /// Tiles water has reached the edge of, or that a script asked for,
    /// waiting for their ground.
    wanted: Vec<u32>,
    springs: Vec<(u32, f32)>,
    /// Volume each held group has given (less what it took), m³, since
    /// [`Self::take_exchanged`]; and in all.
    exchanged: HashMap<u16, f64>,
    added: f64,
    awake: Vec<u32>,
    solved: usize,
    /// The most tiles held at once.
    pub max_tiles: usize,
    serial: u64,
}

/// A cell's place: tile index and cell within it.
type At = (usize, usize);

impl TiledWater {
    /// An empty patch: dry until water is put on it.
    pub fn new(patch: Patch, solver: Solver) -> Self {
        Self {
            patch,
            solver,
            gravity: 9.81,
            roughness: 0.0,
            damping: if solver == Solver::Swe { 0.2 } else { 0.3 },
            min_depth: WET,
            tiles: Vec::new(),
            index: HashMap::new(),
            wanted: Vec::new(),
            springs: Vec::new(),
            exchanged: HashMap::new(),
            added: 0.0,
            awake: Vec::new(),
            solved: 0,
            max_tiles: 256,
            serial: 0,
        }
    }

    /// The tile holding cell `(i, j)`, as `(ti, tj)`.
    pub fn tile_of(i: usize, j: usize) -> (usize, usize) {
        (i / TILE, j / TILE)
    }

    /// Is the cell `(i, j)` on the patch?
    pub fn on_patch(&self, i: i64, j: i64) -> bool {
        i >= 0 && j >= 0 && (i as usize) < self.patch.columns && (j as usize) < self.patch.rows
    }

    /// Tiles that need their ground read before water can go there, by key.
    /// Taken: [`Self::add_tile`] each, or they are asked for again when the
    /// water reaches them again.
    pub fn take_wanted(&mut self) -> Vec<u32> {
        std::mem::take(&mut self.wanted)
    }

    /// Ask for tile `(ti, tj)`.
    pub fn want(&mut self, ti: usize, tj: usize) {
        let (nx, nz) = self.patch.tiles();
        if ti < nx && tj < nz {
            let k = tile_key(ti, tj);
            if !self.index.contains_key(&k) && !self.wanted.contains(&k) {
                self.wanted.push(k);
            }
        }
    }

    pub fn has_tile(&self, ti: usize, tj: usize) -> bool {
        self.index.contains_key(&tile_key(ti, tj))
    }

    /// How many tiles exist.
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Every tile's `(ti, tj)`.
    pub fn tile_coords(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.tiles.iter().map(|t| (t.ti, t.tj))
    }

    /// Put tile `(ti, tj)` on the patch with its ground, `TILE × TILE`
    /// heights a row at a time (infinite for no ground), and its water
    /// standing at `surface` (a level per cell; below the ground is dry).
    /// It wakes. False when the patch already holds as many tiles as it may.
    pub fn add_tile(&mut self, ti: usize, tj: usize, mut ground: Vec<f32>, surface: Option<&[f32]>) -> bool {
        let key = tile_key(ti, tj);
        if self.index.contains_key(&key) {
            return true;
        }
        let (nx, nz) = self.patch.tiles();
        if ti >= nx || tj >= nz || self.tiles.len() >= self.max_tiles {
            return false;
        }
        ground.resize(CELLS, f32::INFINITY);
        let mut t = Tile {
            ti,
            tj,
            depth: vec![0.0; CELLS],
            u: vec![0.0; CELLS],
            v: vec![0.0; CELLS],
            un: vec![0.0; CELLS],
            vn: vec![0.0; CELLS],
            qx: vec![0.0; CELLS],
            qz: vec![0.0; CELLS],
            scale: vec![1.0; CELLS],
            hold: vec![f32::NAN; CELLS],
            group: vec![0; CELLS],
            held: 0,
            area: vec![0.0; CELLS],
            dx: vec![1.0; CELLS],
            dz: vec![1.0; CELLS],
            wx: vec![0.0; CELLS],
            wz: vec![0.0; CELLS],
            nbr: [NONE; 4],
            awake: true,
            calm: 0,
            idle: 0,
            changed: true,
            springs: 0,
            serial: 0,
            published: None,
            ground: Vec::new(),
        };
        let p = self.patch;
        for y in 0..TILE {
            for x in 0..TILE {
                let c = y * TILE + x;
                let (i, j) = (ti * TILE + x, tj * TILE + y);
                if i >= p.columns || j >= p.rows {
                    ground[c] = f32::INFINITY;
                }
                let (gx, gz) = (i as f64, j as f64);
                let at = |a: f64, b: f64| p.point(a, b, 0.0);
                let centre = at(gx, gz);
                t.dx[c] = (at(gx + 1.0, gz) - centre).length().max(1e-6) as f32;
                t.dz[c] = (at(gx, gz + 1.0) - centre).length().max(1e-6) as f32;
                t.wx[c] = (at(gx + 0.5, gz + 0.5) - at(gx + 0.5, gz - 0.5)).length() as f32;
                t.wz[c] = (at(gx + 0.5, gz + 0.5) - at(gx - 0.5, gz + 0.5)).length() as f32;
                let (a, b) = (at(gx + 0.5, gz + 0.5) - at(gx - 0.5, gz - 0.5), at(gx - 0.5, gz + 0.5) - at(gx + 0.5, gz - 0.5));
                t.area[c] = (a.cross(b).length() * 0.5).max(1e-6) as f32;
                if let Some(s) = surface {
                    let g = ground[c];
                    if g.is_finite() {
                        t.depth[c] = (s.get(c).copied().unwrap_or(f32::NAN) - g).max(0.0);
                    }
                }
            }
        }
        t.ground = ground;
        let idx = self.tiles.len() as u32;
        self.tiles.push(t);
        self.index.insert(key, idx);
        self.wanted.retain(|k| *k != key);
        self.link(idx as usize);
        // Springs set before the tile existed.
        let here = self.springs.iter().filter(|(cell, _)| self.locate_cell(*cell).is_some_and(|(t, _)| t == idx as usize)).count();
        self.tiles[idx as usize].springs = here as u32;
        self.awake.push(idx);
        true
    }

    /// Join tile `t` to its neighbours, and them to it.
    fn link(&mut self, t: usize) {
        let (ti, tj) = (self.tiles[t].ti as i64, self.tiles[t].tj as i64);
        for (side, (di, dj), back) in [(EAST, (1, 0), WEST), (WEST, (-1, 0), EAST), (NORTH, (0, 1), SOUTH), (SOUTH, (0, -1), NORTH)] {
            let (a, b) = (ti + di, tj + dj);
            if a < 0 || b < 0 {
                continue;
            }
            if let Some(&o) = self.index.get(&tile_key(a as usize, b as usize)) {
                self.tiles[t].nbr[side] = o;
                self.tiles[o as usize].nbr[back] = t as u32;
            }
        }
    }

    /// The tile and cell of global cell number `cell` (`j * columns + i`).
    fn locate_cell(&self, cell: u32) -> Option<At> {
        let cols = self.patch.columns.max(1);
        let (i, j) = (cell as usize % cols, cell as usize / cols);
        self.at(i, j)
    }

    /// The tile and cell of `(i, j)`, if its tile exists.
    fn at(&self, i: usize, j: usize) -> Option<At> {
        let t = *self.index.get(&tile_key(i / TILE, j / TILE))?;
        Some((t as usize, (j % TILE) * TILE + i % TILE))
    }

    /// The cell beside `(t, c)` on `side`, if its tile exists.
    #[inline]
    fn beside(&self, t: usize, c: usize, side: usize) -> Option<At> {
        let (x, y) = (c % TILE, c / TILE);
        let tile = &self.tiles[t];
        let step = |n: u32, nc: usize| if n == NONE { None } else { Some((n as usize, nc)) };
        match side {
            EAST if x + 1 < TILE => Some((t, c + 1)),
            EAST => step(tile.nbr[EAST], y * TILE),
            WEST if x > 0 => Some((t, c - 1)),
            WEST => step(tile.nbr[WEST], y * TILE + TILE - 1),
            NORTH if y + 1 < TILE => Some((t, c + TILE)),
            NORTH => step(tile.nbr[NORTH], x),
            _ if y > 0 => Some((t, c - TILE)),
            _ => step(tile.nbr[SOUTH], (TILE - 1) * TILE + x),
        }
    }

    /// Wake tile `t`.
    fn wake_tile(&mut self, t: usize) {
        let tile = &mut self.tiles[t];
        tile.calm = 0;
        tile.idle = 0;
        tile.changed = true;
        if !tile.awake {
            tile.awake = true;
            self.awake.push(t as u32);
        }
    }

    /// Wake the water at cell `(i, j)` and round it: something changed there.
    pub fn wake(&mut self, i: usize, j: usize) {
        if let Some((t, c)) = self.at(i, j) {
            self.wake_tile(t);
            for side in 0..4 {
                if let Some((o, _)) = self.beside(t, c, side) {
                    self.wake_tile(o);
                }
            }
        }
    }

    /// The ground at cell `(i, j)` is now `height` (a dig, a fill); the water
    /// there wakes. Nothing happens in a tile that does not exist.
    pub fn set_ground(&mut self, i: usize, j: usize, height: f32) {
        if let Some((t, c)) = self.at(i, j)
            && self.tiles[t].ground[c] != height
        {
            // The water in the cell stays: dug out, it stands lower; filled
            // under, it is lifted, and flows off.
            let tile = &mut self.tiles[t];
            if !height.is_finite() {
                self.added -= tile.depth[c] as f64 * tile.area[c] as f64;
                tile.depth[c] = 0.0;
            }
            tile.ground[c] = height;
            self.wake(i, j);
        }
    }

    /// The ground and depth at cell `(i, j)`, where its tile exists.
    pub fn cell(&self, i: usize, j: usize) -> Option<(f32, f32)> {
        let (t, c) = self.at(i, j)?;
        Some((self.tiles[t].ground[c], self.tiles[t].depth[c]))
    }

    /// Hold cell `(i, j)`'s surface at `level` (a height above the patch's
    /// reference), counting what it gives and takes in `group`; `NaN` lets it
    /// go. A cell whose tile does not exist yet asks for it, and is held once
    /// it does: false then, so the caller can ask again.
    pub fn hold(&mut self, i: usize, j: usize, level: f32, group: u16) -> bool {
        let Some((t, c)) = self.at(i, j) else {
            if level.is_finite() {
                let (ti, tj) = Self::tile_of(i, j);
                self.want(ti, tj);
            }
            return false;
        };
        let tile = &mut self.tiles[t];
        let was = tile.hold[c].is_finite();
        tile.hold[c] = level;
        tile.group[c] = group;
        match (was, level.is_finite()) {
            (false, true) => tile.held += 1,
            (true, false) => tile.held -= 1,
            _ => {}
        }
        self.wake(i, j);
        true
    }

    /// Move every held cell to `level`, whatever group it is in.
    pub fn hold_all_at(&mut self, level: f32) {
        let mut wake = Vec::new();
        for (t, tile) in self.tiles.iter_mut().enumerate() {
            if tile.held == 0 {
                continue;
            }
            for h in tile.hold.iter_mut().filter(|h| h.is_finite()) {
                *h = level;
            }
            wake.push(t);
        }
        for t in wake {
            self.wake_tile(t);
        }
    }

    /// Every held cell, as `(i, j, level, group)`.
    pub fn held(&self) -> Vec<(usize, usize, f32, u16)> {
        let mut out = Vec::new();
        for tile in self.tiles.iter().filter(|t| t.held > 0) {
            for c in 0..CELLS {
                if tile.hold[c].is_finite() {
                    out.push((tile.ti * TILE + c % TILE, tile.tj * TILE + c / TILE, tile.hold[c], tile.group[c]));
                }
            }
        }
        out
    }

    /// A spring at cell `(i, j)` giving `rate` m³/s; a rate of 0 or less
    /// takes the spring there away.
    pub fn set_spring(&mut self, i: usize, j: usize, rate: f32) {
        let cell = (j * self.patch.columns + i) as u32;
        self.springs.retain(|(c, _)| *c != cell);
        if rate > 0.0 {
            self.springs.push((cell, rate));
        }
        for tile in &mut self.tiles {
            tile.springs = 0;
        }
        for k in 0..self.springs.len() {
            if let Some((t, _)) = self.locate_cell(self.springs[k].0) {
                self.tiles[t].springs += 1;
            }
        }
        match self.at(i, j) {
            Some(_) => self.wake(i, j),
            None if rate > 0.0 => {
                let (ti, tj) = Self::tile_of(i, j);
                self.want(ti, tj);
            }
            None => {}
        }
    }

    /// Add `volume` m³ of water at cell `(i, j)` (take it, if negative, as
    /// far as the cell holds). Returns what was added; nothing where the
    /// cell's tile does not exist yet (it is asked for).
    pub fn add_water(&mut self, i: usize, j: usize, volume: f64) -> f64 {
        let Some((t, c)) = self.at(i, j) else {
            let (ti, tj) = Self::tile_of(i, j);
            self.want(ti, tj);
            return 0.0;
        };
        let tile = &mut self.tiles[t];
        if !tile.ground[c].is_finite() {
            return 0.0;
        }
        let a = tile.area[c] as f64;
        let before = tile.depth[c] as f64;
        let after = (before + volume / a).max(0.0);
        tile.depth[c] = after as f32;
        let done = (after - before) * a;
        self.added += done;
        self.wake(i, j);
        done
    }

    /// What each held group has given the water (less what it took), m³,
    /// since this was last asked.
    pub fn take_exchanged(&mut self) -> Vec<(u16, f64)> {
        let mut out: Vec<(u16, f64)> = self.exchanged.drain().collect();
        out.sort_by_key(|(g, _)| *g);
        out
    }

    /// The volume of water on the patch, m³.
    pub fn volume(&self) -> f64 {
        self.tiles.iter().map(|t| t.depth.iter().zip(&t.area).map(|(d, a)| *d as f64 * *a as f64).sum::<f64>()).sum()
    }

    /// Tiles awake.
    pub fn awake_tiles(&self) -> usize {
        self.awake.len()
    }

    /// Cells the last step solved: the cells of the tiles awake.
    pub fn solved_count(&self) -> usize {
        self.solved
    }

    /// Cells holding water.
    pub fn wet_cells(&self) -> usize {
        self.tiles.iter().map(|t| t.depth.iter().filter(|d| **d > WET).count()).sum()
    }

    /// A step that stays stable at the water's deepest and fastest in the
    /// tiles awake: a wave crosses at most half a cell.
    pub fn stable_dt(&self) -> f32 {
        let (mut deepest, mut fastest, mut shortest) = (0.01f32, 0.0f32, f32::INFINITY);
        for &t in &self.awake {
            let t = &self.tiles[t as usize];
            for c in 0..CELLS {
                if t.depth[c] > 0.0 {
                    deepest = deepest.max(t.depth[c]);
                    fastest = fastest.max(t.u[c].abs()).max(t.v[c].abs());
                    shortest = shortest.min(t.dx[c]).min(t.dz[c]);
                }
            }
        }
        if !shortest.is_finite() {
            return 1.0;
        }
        0.5 * shortest / ((self.gravity * deepest).sqrt() + fastest)
    }

    /// Advance the tiles awake `dt` seconds.
    pub fn step(&mut self, dt: f32) {
        self.wake_beside_awake();
        self.grow();
        let awake: Vec<usize> = self.awake.iter().map(|&t| t as usize).collect();
        self.solved = awake.len() * CELLS;
        if awake.is_empty() || dt <= 0.0 {
            return;
        }
        // 1. Each face's new velocity, from the old ones: every tile reads
        // a copy of itself and the ring of cells round it, so the tiles can
        // be solved apart, on as many threads as there are.
        let solved = self.each(&awake, true, |t| self.solve_faces(t, &self.pad(t), dt));
        for (&t, (un, vn)) in awake.iter().zip(solved) {
            self.tiles[t].un = un;
            self.tiles[t].vn = vn;
        }
        for &t in &awake {
            let tile = &mut self.tiles[t];
            std::mem::swap(&mut tile.u, &mut tile.un);
            std::mem::swap(&mut tile.v, &mut tile.vn);
        }
        // 2. What each face passes.
        let flows = self.each(&awake, false, |t| {
            let mut qx = vec![0.0; CELLS];
            let mut qz = vec![0.0; CELLS];
            for c in 0..CELLS {
                qx[c] = self.face_flux(t, c, EAST);
                qz[c] = self.face_flux(t, c, NORTH);
            }
            (qx, qz)
        });
        for (&t, (qx, qz)) in awake.iter().zip(flows) {
            self.tiles[t].qx = qx;
            self.tiles[t].qz = qz;
        }
        // 3. No cell gives more than it holds.
        let scales = self.each(&awake, false, |t| {
            let tile = &self.tiles[t];
            (0..CELLS)
                .map(|c| {
                    let mut out = tile.qx[c].max(0.0) + tile.qz[c].max(0.0);
                    if let Some((o, oc)) = self.beside(t, c, WEST) {
                        out += (-self.tiles[o].qx[oc]).max(0.0);
                    }
                    if let Some((o, oc)) = self.beside(t, c, SOUTH) {
                        out += (-self.tiles[o].qz[oc]).max(0.0);
                    }
                    let holds = tile.depth[c] * tile.area[c];
                    if out * dt > holds && out > 0.0 { holds / (out * dt) } else { 1.0 }
                })
                .collect::<Vec<f32>>()
        });
        for (&t, scale) in awake.iter().zip(scales) {
            self.tiles[t].scale = scale;
        }
        // What a face passes is scaled by the share its giver can give.
        let limited = self.each(&awake, false, |t| {
            let tile = &self.tiles[t];
            let (mut qx, mut qz, mut u, mut v) = (tile.qx.clone(), tile.qz.clone(), tile.u.clone(), tile.v.clone());
            for c in 0..CELLS {
                for (side, q, vel) in [(EAST, &mut qx, &mut u), (NORTH, &mut qz, &mut v)] {
                    if q[c] == 0.0 {
                        continue;
                    }
                    let k = if q[c] > 0.0 { tile.scale[c] } else { self.beside(t, c, side).map_or(1.0, |(o, oc)| self.tiles[o].scale[oc]) };
                    if k < 1.0 {
                        q[c] *= k;
                        vel[c] *= k;
                    }
                }
            }
            (qx, qz, u, v)
        });
        for (&t, (qx, qz, u, v)) in awake.iter().zip(limited) {
            let tile = &mut self.tiles[t];
            (tile.qx, tile.qz, tile.u, tile.v) = (qx, qz, u, v);
        }
        // 4. Depths: what the four faces passed, the springs, the held cells.
        let mut spring_add: HashMap<At, f32> = HashMap::new();
        for &(cell, q) in &self.springs {
            if let Some(at) = self.locate_cell(cell) {
                *spring_add.entry(at).or_insert(0.0) += q * dt;
            }
        }
        let depths = self.each(&awake, false, |t| {
            let tile = &self.tiles[t];
            let (mut moved, mut fastest, mut added) = (0.0f32, 0.0f32, 0.0f64);
            let mut depth = tile.depth.clone();
            let mut exchanged: Vec<(u16, f64)> = Vec::new();
            for (c, d) in depth.iter_mut().enumerate() {
                if !tile.ground[c].is_finite() {
                    continue;
                }
                let mut net = -tile.qx[c] - tile.qz[c];
                if let Some((o, oc)) = self.beside(t, c, WEST) {
                    net += self.tiles[o].qx[oc];
                }
                if let Some((o, oc)) = self.beside(t, c, SOUTH) {
                    net += self.tiles[o].qz[oc];
                }
                let mut vol = net * dt;
                if let Some(q) = spring_add.get(&(t, c)) {
                    vol += q;
                    added += *q as f64;
                }
                let before = *d;
                let mut after = (before + vol / tile.area[c]).max(0.0);
                let hold = tile.hold[c];
                if hold.is_finite() {
                    let want = (hold - tile.ground[c]).max(0.0);
                    exchanged.push((tile.group[c], (want as f64 - after as f64) * tile.area[c] as f64));
                    after = want;
                }
                *d = after;
                moved = moved.max((after - before).abs());
                fastest = fastest.max(tile.u[c].abs()).max(tile.v[c].abs());
            }
            (depth, exchanged, moved, fastest, added)
        });
        for (&t, (depth, exchanged, moved, fastest, added)) in awake.iter().zip(depths) {
            self.added += added;
            for (g, v) in exchanged {
                *self.exchanged.entry(g).or_insert(0.0) += v;
                self.added += v;
            }
            let tile = &mut self.tiles[t];
            tile.depth = depth;
            tile.changed = true;
            let still = moved < STEADY && fastest < STILL && tile.springs == 0;
            tile.calm = if still { tile.calm.saturating_add(1) } else { 0 };
        }
        // 5. Still tiles sleep, their faces stopped.
        let mut i = 0;
        while i < self.awake.len() {
            let t = self.awake[i] as usize;
            if self.tiles[t].calm >= SLEEP_STEPS {
                let tile = &mut self.tiles[t];
                tile.awake = false;
                tile.u.fill(0.0);
                tile.v.fill(0.0);
                tile.qx.fill(0.0);
                tile.qz.fill(0.0);
                self.awake.swap_remove(i);
            } else {
                i += 1;
            }
        }
        self.evict_dry();
    }

    /// `f` for each tile of `tiles`, in order; a `heavy` stage on as many
    /// threads as there are when enough tiles are awake to pay for starting
    /// them. Every stage reads the tiles as the last stage left them and
    /// writes only its own tile's answer, so the order the tiles are solved
    /// in changes nothing.
    fn each<T: Send>(&self, tiles: &[usize], heavy: bool, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
        if heavy && tiles.len() >= PARALLEL_TILES {
            crate::chunks::par_map(tiles.len(), |k| f(tiles[k]))
        } else {
            tiles.iter().map(|&t| f(t)).collect()
        }
    }

    /// Tile `t` and the ring of cells round it, as the face solve reads
    /// them. A cell of a tile that is asleep, missing or past the patch is a
    /// wall this step: the sleeping side is not stepped, and what crossed
    /// into it would be lost; it is woken first where the two sides differ.
    fn pad(&self, t: usize) -> Pad {
        let mut p = Pad::dry();
        let tile = &self.tiles[t];
        for y in 0..TILE {
            for x in 0..TILE {
                p.copy(PADW * (y + 1) + x + 1, tile, y * TILE + x);
            }
        }
        let side = |n: u32| (n != NONE && self.tiles[n as usize].awake).then(|| &self.tiles[n as usize]);
        for k in 0..TILE {
            if let Some(e) = side(tile.nbr[EAST]) {
                p.copy(PADW * (k + 1) + TILE + 1, e, k * TILE);
            }
            if let Some(w) = side(tile.nbr[WEST]) {
                p.copy(PADW * (k + 1), w, k * TILE + TILE - 1);
            }
            if let Some(n) = side(tile.nbr[NORTH]) {
                p.copy(PADW * (TILE + 1) + k + 1, n, k);
            }
            if let Some(s) = side(tile.nbr[SOUTH]) {
                p.copy(k + 1, s, (TILE - 1) * TILE + k);
            }
        }
        // The corners, through either neighbour.
        let corner = |a: usize, b: usize| {
            let via = |first: usize, then: usize| {
                let n = tile.nbr[first];
                if n == NONE { NONE } else { self.tiles[n as usize].nbr[then] }
            };
            let n = via(a, b);
            side(if n != NONE { n } else { via(b, a) })
        };
        if let Some(c) = corner(EAST, NORTH) {
            p.copy(PADW * (TILE + 1) + TILE + 1, c, 0);
        }
        if let Some(c) = corner(WEST, NORTH) {
            p.copy(PADW * (TILE + 1), c, TILE - 1);
        }
        if let Some(c) = corner(EAST, SOUTH) {
            p.copy(TILE + 1, c, (TILE - 1) * TILE);
        }
        if let Some(c) = corner(WEST, SOUTH) {
            p.copy(0, c, CELLS - 1);
        }
        p
    }

    /// Every face of tile `t`'s new velocity, `(+column faces, +row faces)`.
    fn solve_faces(&self, t: usize, p: &Pad, dt: f32) -> (Vec<f32>, Vec<f32>) {
        let tile = &self.tiles[t];
        let mut un = vec![0.0; CELLS];
        let mut vn = vec![0.0; CELLS];
        let swe = self.solver == Solver::Swe;
        let n = if swe { self.roughness.max(SMOOTH_BED) } else { self.roughness };
        let damp = if self.roughness <= 0.0 { (1.0 - self.damping * dt).max(0.0) } else { 1.0 };
        let still = 0.5 * self.min_depth;
        let g = self.gravity;
        for y in 0..TILE {
            for x in 0..TILE {
                let c = y * TILE + x;
                let i = PADW * (y + 1) + x + 1;
                // Nothing to move on either side of either face.
                if p.depth[i] <= 0.0 && p.depth[i + 1] <= 0.0 && p.depth[i + PADW] <= 0.0 {
                    continue;
                }
                for (east, o, step, cross_step, dist, cross) in
                    [(true, i + 1, 1, PADW, tile.dx[c], tile.dz[c]), (false, i + PADW, PADW, 1, tile.dz[c], tile.dx[c])]
                {
                    let (ga, gb) = (p.ground[i], p.ground[o]);
                    if !ga.is_finite() || !gb.is_finite() {
                        continue;
                    }
                    let (ha, hb) = (p.depth[i], p.depth[o]);
                    let (ea, eb) = (ga + ha, gb + hb);
                    let hf = ea.max(eb) - ga.max(gb);
                    // A film thinner than half the least wet depth stays where
                    // it is: water draining off a slope pours as over a weir,
                    // ever more slowly as it thins, and would hold its tile
                    // awake for good.
                    if hf <= still {
                        continue;
                    }
                    let (vel, q, across) = if east { (&p.u, &p.qx, &p.v) } else { (&p.v, &p.qz, &p.u) };
                    let u0 = vel[i];
                    let mut u = u0 - dt * g * (eb - ea) / dist;
                    if swe {
                        // The water carries its own momentum, in the
                        // momentum-conserving upwind form of Stelling and
                        // Duinmeijer: what flows into the face's half-cells
                        // brings its speed with it, so a surge pushes its
                        // speed into still water ahead of it and a bore keeps
                        // its front.
                        let (qb, qf, qa) = (q[i - step], q[i], q[o]);
                        let ql = 0.5 * (qb + qf);
                        let qr = 0.5 * (qf + qa);
                        let ul = if ql > 0.0 { vel[i - step] } else { u0 };
                        let ur = if qr > 0.0 { u0 } else { vel[o] };
                        let hbar = (0.5 * (ha + hb)).max(hf).max(DRY);
                        let along = (qr * ur - ql * ul - u0 * (qr - ql)) / (dist * hbar);
                        // Across the face, upwind on the velocity across it.
                        let w = 0.25 * (across[i] + across[o] + across[i - cross_step] + across[o - cross_step]);
                        let by = if w > 0.0 { (u0 - vel[i - cross_step]) / cross } else { (vel[i + cross_step] - u0) / cross };
                        u -= dt * (along + w * by);
                    }
                    // The bed's drag, taken implicitly so it is stable at any
                    // depth. Water with momentum always meets a little of it
                    // (a smooth bed's), which is what stops a shore sloshing
                    // for good where the water is shallow.
                    if n > 0.0 {
                        u /= 1.0 + dt * g * n * n * u.abs() / hf.powf(4.0 / 3.0);
                    }
                    u *= damp;
                    let limit = FASTEST.min(0.5 * dist / dt);
                    let u = u.clamp(-limit, limit);
                    if east {
                        un[c] = u;
                    } else {
                        vn[c] = u;
                    }
                }
            }
        }
        (un, vn)
    }

    /// What cell `(t, c)`'s face towards `side` passes this step, m³/s,
    /// signed along the face's axis.
    fn face_flux(&self, t: usize, c: usize, side: usize) -> f32 {
        let a = &self.tiles[t];
        let (u, width) = if side == EAST { (a.u[c], a.wx[c]) } else { (a.v[c], a.wz[c]) };
        if u == 0.0 {
            return 0.0;
        }
        let Some((o, oc)) = self.beside(t, c, side) else { return 0.0 };
        let b = &self.tiles[o];
        let (ga, gb) = (a.ground[c], b.ground[oc]);
        let sill = ga.max(gb);
        // The water over the sill on the side it comes from.
        let (hd, gd, other) = if u > 0.0 { (a.depth[c], ga, gb + b.depth[oc]) } else { (b.depth[oc], gb, ga + a.depth[c]) };
        let h = (gd + hd - sill).clamp(0.0, hd);
        let mut q = u.abs() * h * width;
        // Pouring off an edge passes no more than a weir does.
        if other < gd {
            q = q.min(WEIR * width * (2.0 * self.gravity).sqrt() * hd.powf(1.5));
        }
        q * u.signum()
    }

    /// Wake a sleeping tile beside an awake one where the water across their
    /// edge stands at two levels.
    fn wake_beside_awake(&mut self) {
        let mut wake = Vec::new();
        for &t in &self.awake {
            let t = t as usize;
            for side in 0..4 {
                let n = self.tiles[t].nbr[side];
                if n == NONE || self.tiles[n as usize].awake || wake.contains(&(n as usize)) {
                    continue;
                }
                for k in 0..TILE {
                    let c = match side {
                        EAST => k * TILE + TILE - 1,
                        WEST => k * TILE,
                        NORTH => (TILE - 1) * TILE + k,
                        _ => k,
                    };
                    let Some((o, oc)) = self.beside(t, c, side) else { continue };
                    let (a, b) = (&self.tiles[t], &self.tiles[o]);
                    if !a.ground[c].is_finite() || !b.ground[oc].is_finite() {
                        continue;
                    }
                    let (ea, eb) = (a.ground[c] + a.depth[c], b.ground[oc] + b.depth[oc]);
                    // Only water deep enough to move over the face between.
                    let moves = ea.max(eb) - a.ground[c].max(b.ground[oc]) > 0.5 * self.min_depth;
                    if moves && (ea - eb).abs() > LEVEL {
                        wake.push(o);
                        break;
                    }
                }
            }
        }
        for t in wake {
            self.wake_tile(t);
        }
    }

    /// Ask for the tiles beside an awake one that its water reaches the edge
    /// of.
    fn grow(&mut self) {
        let mut want = Vec::new();
        for &t in &self.awake {
            let tile = &self.tiles[t as usize];
            for side in 0..4 {
                if tile.nbr[side] != NONE {
                    continue;
                }
                let (di, dj): (i64, i64) = [(1, 0), (-1, 0), (0, 1), (0, -1)][side];
                let (ni, nj) = (tile.ti as i64 + di, tile.tj as i64 + dj);
                let (nx, nz) = self.patch.tiles();
                if ni < 0 || nj < 0 || ni as usize >= nx || nj as usize >= nz {
                    continue;
                }
                let reaches = (0..TILE).any(|k| {
                    let c = match side {
                        EAST => k * TILE + TILE - 1,
                        WEST => k * TILE,
                        NORTH => (TILE - 1) * TILE + k,
                        _ => k,
                    };
                    tile.depth[c] > WET
                });
                if reaches {
                    want.push((ni as usize, nj as usize));
                }
            }
        }
        for (i, j) in want {
            self.want(i, j);
        }
    }

    /// Drop tiles that have been dry and asleep a while, with nothing held
    /// or springing in them.
    fn evict_dry(&mut self) {
        let mut drop = Vec::new();
        for (t, tile) in self.tiles.iter_mut().enumerate() {
            if tile.awake || tile.held > 0 || tile.springs > 0 {
                tile.idle = 0;
                continue;
            }
            if tile.depth.iter().all(|d| *d <= 0.0) {
                tile.idle += 1;
                if tile.idle >= EVICT_STEPS {
                    drop.push(t);
                }
            } else {
                tile.idle = 0;
            }
        }
        for t in drop.into_iter().rev() {
            self.remove_tile(t);
        }
    }

    fn remove_tile(&mut self, t: usize) {
        let last = self.tiles.len() - 1;
        let gone = tile_key(self.tiles[t].ti, self.tiles[t].tj);
        self.index.remove(&gone);
        for side in 0..4 {
            let n = self.tiles[t].nbr[side];
            if n != NONE {
                let back = [WEST, EAST, SOUTH, NORTH][side];
                self.tiles[n as usize].nbr[back] = NONE;
            }
        }
        self.awake.retain(|&a| a as usize != t);
        self.tiles.swap_remove(t);
        if t != last {
            // The last tile moved into `t`'s place.
            let k = tile_key(self.tiles[t].ti, self.tiles[t].tj);
            self.index.insert(k, t as u32);
            for side in 0..4 {
                let n = self.tiles[t].nbr[side];
                if n != NONE {
                    let back = [WEST, EAST, SOUTH, NORTH][side];
                    self.tiles[n as usize].nbr[back] = t as u32;
                }
            }
            for a in &mut self.awake {
                if *a as usize == last {
                    *a = t as u32;
                }
            }
        }
    }

    /// The water as a reader sees it: every tile holding any, a tile that
    /// has not changed since the last call handed back as the same shared
    /// levels with the same serial.
    pub fn surface(&mut self) -> TiledSurface {
        let mut tiles = HashMap::with_capacity(self.tiles.len());
        for t in 0..self.tiles.len() {
            if self.tiles[t].changed || self.tiles[t].published.is_none() {
                let levels = self.read_tile(t);
                let tile = &mut self.tiles[t];
                tile.changed = false;
                let same = tile.published.as_ref().is_some_and(|p| p.level == levels.level && p.flow == levels.flow);
                if !same {
                    self.serial += 1;
                    tile.serial = self.serial;
                    tile.published = Some(Arc::new(TileLevels { serial: self.serial, ..levels }));
                }
            }
            let tile = &self.tiles[t];
            if let Some(p) = &tile.published
                && p.level.iter().any(|l| l.is_finite())
            {
                tiles.insert(tile_key(tile.ti, tile.tj), p.clone());
            }
        }
        TiledSurface { patch: self.patch, tiles }
    }

    /// Tile `t`'s levels, depths and cell-centre flow.
    fn read_tile(&self, t: usize) -> TileLevels {
        let tile = &self.tiles[t];
        let mut level = vec![f32::NAN; CELLS];
        let mut flow = vec![[0.0f32; 2]; CELLS];
        for c in 0..CELLS {
            if tile.depth[c] <= self.min_depth {
                continue;
            }
            level[c] = tile.ground[c] + tile.depth[c];
            let w = self.beside(t, c, WEST).map_or(0.0, |(o, oc)| self.tiles[o].u[oc]);
            let s = self.beside(t, c, SOUTH).map_or(0.0, |(o, oc)| self.tiles[o].v[oc]);
            flow[c] = [0.5 * (tile.u[c] + w), 0.5 * (tile.v[c] + s)];
        }
        TileLevels { level, depth: tile.depth.clone(), flow, serial: tile.serial }
    }

    /// Every place water pours off a drop of more than `min_drop` metres of
    /// ground between two cells: `(i, j, side, discharge m³/s, drop, width)`,
    /// side 0 to 3 being +column, −column, +row, −row.
    pub fn falls(&self, min_drop: f32) -> Vec<(usize, usize, u8, f32, f32, f32)> {
        let mut out = Vec::new();
        for &t in &self.awake {
            let t = t as usize;
            let tile = &self.tiles[t];
            for c in 0..CELLS {
                for (side, q) in [(EAST, tile.qx[c]), (NORTH, tile.qz[c])] {
                    if q.abs() < 1e-4 {
                        continue;
                    }
                    let Some((o, oc)) = self.beside(t, c, side) else { continue };
                    let (from, to, face) = if q > 0.0 { ((t, c), (o, oc), side) } else { ((o, oc), (t, c), side + 1) };
                    let (ft, tt) = (&self.tiles[from.0], &self.tiles[to.0]);
                    let drop = ft.ground[from.1] - tt.ground[to.1];
                    if drop <= min_drop {
                        continue;
                    }
                    let width = if side == EAST { tile.wx[c] } else { tile.wz[c] };
                    let (fi, fj) = (ft.ti * TILE + from.1 % TILE, ft.tj * TILE + from.1 / TILE);
                    let fall = (ft.ground[from.1] + ft.depth[from.1]) - (tt.ground[to.1] + tt.depth[to.1]);
                    out.push((fi, fj, face as u8, q.abs(), fall, width));
                }
            }
        }
        out
    }

    /// Every cell holding water, as `(i, j, depth)`: what a save keeps.
    pub fn wet(&self) -> Vec<(usize, usize, f32)> {
        let mut out = Vec::new();
        for tile in &self.tiles {
            for c in 0..CELLS {
                if tile.depth[c] > 0.0 {
                    out.push((tile.ti * TILE + c % TILE, tile.tj * TILE + c / TILE, tile.depth[c]));
                }
            }
        }
        out
    }

    /// Water springs, held cells and scripts have added in all, less what
    /// they took, m³: the volume now is the volume at the start plus this.
    pub fn added(&self) -> f64 {
        self.added
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;

    /// A flat patch of `columns × rows` cells `cell` metres apart, every
    /// tile made, its ground from `ground(i, j)` and water standing at
    /// `level(i, j)`.
    fn patch(
        columns: usize,
        rows: usize,
        cell: f64,
        solver: Solver,
        ground: impl Fn(usize, usize) -> f32,
        level: impl Fn(usize, usize) -> f32,
    ) -> TiledWater {
        let mut w = TiledWater::new(Patch::flat(DVec3::ZERO, cell, columns, rows), solver);
        let (nx, nz) = w.patch.tiles();
        for tj in 0..nz {
            for ti in 0..nx {
                let mut g = vec![f32::INFINITY; CELLS];
                let mut s = vec![f32::NAN; CELLS];
                for c in 0..CELLS {
                    let (i, j) = (ti * TILE + c % TILE, tj * TILE + c / TILE);
                    if i < columns && j < rows {
                        g[c] = ground(i, j);
                        s[c] = level(i, j);
                    }
                }
                assert!(w.add_tile(ti, tj, g, Some(&s)));
            }
        }
        w
    }

    fn run(w: &mut TiledWater, seconds: f32) {
        let mut t = 0.0;
        while t < seconds {
            let dt = w.stable_dt().min(1.0 / 60.0);
            w.step(dt);
            t += dt;
        }
    }

    /// **Water held in a bowl finds its level and stays there.** A bowl
    /// across four tiles, filled 3 m higher on one side than the other,
    /// sloshes, settles flat, loses nothing, and within two minutes sleeps:
    /// a settled lake costs nothing a step.
    #[test]
    fn a_lake_across_tiles_levels_keeps_its_volume_and_sleeps() {
        for solver in [Solver::Swe, Solver::Pipes] {
            let n = 48;
            let bowl = |i: usize, j: usize| {
                let (x, z) = (i as f32 - 23.5, j as f32 - 23.5);
                (x * x + z * z) / 60.0
            };
            let mut w = patch(n, n, 1.0, solver, bowl, |i, _| if i < 24 { 6.0 } else { 3.0 });
            let v0 = w.volume();
            run(&mut w, 120.0);
            let dv = (w.volume() - v0).abs() / v0;
            assert!(dv < 1e-4, "{solver:?}: the lake's volume moved by {:.5} %", dv * 100.0);
            let s = w.surface();
            let levels: Vec<f32> = s.tiles.values().flat_map(|t| t.level.iter().copied().filter(|l| l.is_finite())).collect();
            let (lo, hi) = levels.iter().fold((f32::MAX, f32::MIN), |(a, b), l| (a.min(*l), b.max(*l)));
            assert!(hi - lo < 0.01, "{solver:?}: the surface runs from {lo} to {hi}");
            assert_eq!(w.awake_tiles(), 0, "{solver:?}: a settled lake is still being solved");
        }
    }

    /// **A trench dug from a held coast fills from it.** The sea held along
    /// one edge at 0 m, a dry trench 1 m deep running 36 m inland, across a
    /// tile's edge: within 10 s the water has run 20 m in, within 30 s the
    /// trench is four-fifths full at its far end, and the held cells say how much they
    /// gave.
    #[test]
    fn a_trench_from_a_held_coast_fills_and_the_coast_counts_what_it_gave() {
        let mut w = patch(64, 8, 1.0, Solver::Swe, |i, j| if ((3..5).contains(&j) && i <= 37) || i < 2 { -1.0 } else { 0.5 }, |_, _| f32::NAN);
        for j in 0..8 {
            assert!(w.hold(0, j, 0.0, 7));
        }
        let mut t = 0.0;
        let (mut half, mut end) = (None, None);
        while t < 30.0 {
            let dt = w.stable_dt().min(1.0 / 60.0);
            w.step(dt);
            t += dt;
            if half.is_none() && w.cell(22, 3).is_some_and(|(_, d)| d > 0.1) {
                half = Some(t);
            }
            if end.is_none() && w.cell(37, 3).is_some_and(|(_, d)| d > 0.8) {
                end = Some(t);
            }
        }
        assert!(half.is_some_and(|t| t < 10.0), "the water reached 20 m in at {half:?} s");
        let profile: Vec<String> = (0..40).step_by(3).map(|i| format!("{:.2}", w.cell(i, 3).map_or(-1.0, |c| c.1))).collect();
        assert!(end.is_some(), "the trench's far end never filled: reached 20 m in at {half:?} s; depths {profile:?}");
        let given: f64 = w.take_exchanged().iter().filter(|(g, _)| *g == 7).map(|(_, v)| v).sum();
        assert!((given - w.volume()).abs() < 0.01 * w.volume(), "the coast gave {given} m³ for {} m³ on the patch", w.volume());
        assert!(w.take_exchanged().is_empty(), "asked twice, it counted twice");
    }

    /// **A dam breaking sends a surge with momentum.** A 1 m deep reservoir
    /// let go down a dry channel: shallow-water theory (Ritter's solution)
    /// holds the water at the dam at 4/9 of the reservoir's depth while the
    /// surge runs off at up to `2·√(g·h)`. With momentum the dam's depth is
    /// that to within 5 %, the front is past half that speed, and the water
    /// is further down the channel than without momentum.
    #[test]
    fn a_dam_break_surges_with_momentum() {
        let run_dam = |solver: Solver| {
            let mut w = patch(160, 4, 0.5, solver, |_, _| 0.0, |i, _| if i < 40 { 1.0 } else { f32::NAN });
            run(&mut w, 3.0);
            let front = (0..160).rev().find(|&i| w.cell(i, 1).is_some_and(|(_, d)| d > 0.01)).unwrap_or(0) as f32 * 0.5 - 20.0;
            (front, w.cell(40, 1).map_or(0.0, |(_, d)| d))
        };
        let ((swe, at_dam), (pipes, _)) = (run_dam(Solver::Swe), run_dam(Solver::Pipes));
        let ideal = 2.0 * (9.81f32).sqrt() * 3.0;
        assert!((at_dam - 4.0 / 9.0).abs() < 0.1 * 4.0 / 9.0, "the dam holds {at_dam} m, theory says 0.444");
        assert!(swe > 0.5 * ideal && swe < 1.1 * ideal, "the surge ran {swe} m in 3 s, theory says at most {ideal}");
        assert!(swe > pipes, "with momentum the front ran {swe} m, without {pipes} m");
    }

    /// **Water reaching a tile's edge asks for the tile beyond, and gets
    /// there once it is made.** Tiles are made only where water goes.
    #[test]
    fn water_asks_for_the_tile_it_reaches() {
        let mut w = TiledWater::new(Patch::flat(DVec3::ZERO, 1.0, 128, 32), Solver::Swe);
        let flat = vec![0.0; CELLS];
        w.add_tile(0, 0, flat.clone(), Some(&vec![2.0; CELLS]));
        assert_eq!(w.tile_count(), 1);
        let mut asked = Vec::new();
        for _ in 0..600 {
            w.step(1.0 / 60.0);
            for k in w.take_wanted() {
                asked.push(k);
                w.add_tile((k & 0xffff) as usize, (k >> 16) as usize, flat.clone(), None);
            }
        }
        assert!(asked.contains(&tile_key(1, 0)), "the tile beside was never asked for: {asked:?}");
        assert!(w.cell(40, 5).is_some_and(|(_, d)| d > 0.05), "the water never reached the next tile");
        assert!(!asked.contains(&tile_key(0, 1)), "past the patch's edge was asked for");
    }

    /// **On a round world each cell's up is its own radial**, so a patch
    /// hundreds of metres across holds a lake flat to the sphere: water
    /// standing 2 m deep over ground at a constant radius stays put however
    /// far the patch runs from its middle.
    #[test]
    fn a_patch_on_a_sphere_holds_still_water_level_with_the_sphere() {
        let on = DVec3::new(800.0, 0.0, 0.0);
        let mut w = TiledWater::new(Patch::sphere(on, 2.0, 128, 128), Solver::Swe);
        let (nx, nz) = w.patch.tiles();
        for tj in 0..nz {
            for ti in 0..nx {
                w.add_tile(ti, tj, vec![-2.0; CELLS], Some(&vec![0.0; CELLS]));
            }
        }
        let v0 = w.volume();
        run(&mut w, 5.0);
        assert_eq!(w.awake_tiles(), 0, "level water on the sphere moved");
        assert!((w.volume() - v0).abs() < 1e-6 * v0);
        // The patch's corner is 180 m along the ground from its middle, and
        // the cells there are smaller than at the middle: the gnomonic grid
        // shrinks, and the areas know it.
        let s = w.surface();
        let p = w.patch.point(0.0, 0.0, 0.0);
        assert!((p.length() - 800.0).abs() < 1e-9);
        assert!(s.cell(0, 0).is_some_and(|(l, d, _)| l.abs() < 1e-6 && (d - 2.0).abs() < 1e-6));
    }

    /// What a tick of moving water costs at 500, 6,000 and 30,000 wet
    /// cells, every one of them moving: a tank of water 1 m deep sloshing
    /// end to end, 1.25 m cells, a sixtieth of a second stepped at the
    /// stable step. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn probe_tiled_cost() {
        for (columns, rows) in [(32usize, 16usize), (192, 32), (480, 64)] {
            let mut w = patch(columns, rows, 1.25, Solver::Swe, |_, _| 0.0, |i, _| 1.0 + 0.3 * (i as f32 / columns as f32 * std::f32::consts::PI).cos());
            run(&mut w, 0.5);
            let t = std::time::Instant::now();
            let ticks = 120;
            for _ in 0..ticks {
                let mut left = 1.0 / 60.0f32;
                while left > 1e-6 {
                    let dt = w.stable_dt().min(left);
                    w.step(dt);
                    left -= dt;
                }
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / ticks as f64;
            println!(
                "wet {:>6}: {:.3} ms a tick, {} tiles ({} awake), {} cells solved a step",
                w.wet_cells(),
                ms,
                w.tile_count(),
                w.awake_tiles(),
                w.solved_count()
            );
        }
    }
}
