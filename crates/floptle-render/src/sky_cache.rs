//! The sky cache: `skyOcclusion`'s answer kept in a grid around the camera,
//! so a surface reads it instead of marching four rays.
//!
//! `skyOcclusion(p, n, reach)` marches four rays from every pixel that calls
//! it. Where something is within reach (a shore, a cave mouth, a beach full
//! of crew and crates), that is four shadow rays a pixel, and a sea filling
//! the screen pays for them on every pixel of it. Most of what the rays meet
//! does not move: the terrain, the baked mesh occluders, a dock, a crate
//! that has come to rest. The cache asks the question once per cell of a
//! grid around the camera, for the six axis directions, and a pixel reads
//! the eight cells around it.
//!
//! **Shape.** Four nested levels, each a cube of [`SKY_DIMS`] cells, the
//! finest [`SKY_CELL0`] across and each next one twice as coarse: about ±24
//! units round the camera at the finest, ±192 at the coarsest. A cube and
//! not a slab, because a world need not have +y as its up. The levels are
//! anchored to a point the caller names (the world origin, or the terrain
//! of a world moving along its orbit) and scroll with the camera a whole
//! cell at a time: a cell's place in storage is its lattice coordinate
//! modulo the level's size, so a step exposes one slab and copies nothing.
//!
//! **What a cell holds** (64 bits): the cone's answer for each of the six
//! axis directions (8 bits each), the static field's distance at the
//! centre, and flags: whether the cell has been filled for the place it
//! stands for now, and whether its centre is outside solid matter.
//!
//! **What goes in, and what is left to each pixel.** Everything the field
//! holds that stands still: volumes, blobs, and collider proxies that have
//! not moved for [`SKY_SETTLE_FRAMES`] frames (a static dock or roof is a
//! proxy too). A proxy that moves is left out and each pixel near it reads
//! a short-range term instead, found through a small grid of masks around
//! the camera ([`SKY_GRID`]) so no pixel loops over every slot. A scene with
//! Field Shapes, whose code can animate with time, is not cached.
//!
//! **Refilling.** The planner here runs on the CPU each frame. It works out
//! which cells are due: those scrolled into view (a level whose window
//! moved), and those in a dirty box (a field edit, a volume or blob that
//! moved, a proxy that settled or started moving). Dirty cells are refilled
//! in a dither over several frames, so a change dissolves in instead of
//! wiping, and the total stays near a budget of [`SKY_BUDGET`] cells a frame.
//! A still camera with nothing dirty refills nothing.
//!
//! The GPU half is in `raymarch.rs` (the fill pass and the buffer) and
//! `field.wgsl` (`sky_cached`, the read).

use glam::{DVec3, Vec3};

use crate::raymarch::{RaymarchGlobals, MAX_BLOBS, MAX_SHADOW_PROXIES, MAX_VOLUMES};

/// Nested levels of the cache.
pub const SKY_LEVELS: usize = 4;
/// Cells along each side of a level.
pub const SKY_DIMS: u32 = 48;
/// The finest level's cell size, in world units; each level doubles it.
pub const SKY_CELL0: f32 = 1.0;
/// Cells refilled per frame, about.
pub const SKY_BUDGET: u32 = 8192;
/// Frames a proxy must hold still before the cache takes it in.
pub const SKY_SETTLE_FRAMES: u32 = 30;
/// Dirty boxes refilled at once; more are merged into these.
pub const SKY_MAX_BOXES: usize = 8;
/// Cells along each side of the moving-proxy mask grid.
pub const SKY_GRID: u32 = 16;
/// The mask grid's cell size, world units.
pub const SKY_GRID_CELL: f32 = 4.0;
/// How far past its bound a moving proxy's short-range term reaches.
pub const SKY_MOVING_REACH: f32 = 8.0;
/// How far a placement may drift, world units, and still count as unmoved.
const STILL_TOLERANCE: f64 = 0.01;
/// The cone's penumbra constant and its furthest lift (`field.wgsl`).
const CONE_K: f32 = 8.0;
const CONE_LIFT_CAP: f32 = 3.0;
/// Frames between refill passes over every level when nothing is due, to
/// catch any cell the budget's estimates lost track of.
const HEAL_EVERY: u64 = 8;

/// Texels across the cache's atlas (rows of it are what the buffer holds).
/// A multiple of 32, so a row is a whole number of 256-byte copy rows.
pub const SKY_ATLAS_W: u32 = 1536;
/// Cells in every level together.
pub const SKY_CELLS: u32 = SKY_DIMS * SKY_DIMS * SKY_DIMS * SKY_LEVELS as u32;
/// Rows of the atlas.
pub const SKY_ATLAS_H: u32 = SKY_CELLS.div_ceil(SKY_ATLAS_W);
/// Bytes of one cell.
pub const SKY_TEXEL: u32 = 8;

// The header: 32-bit words at the front of the buffer, ahead of the cells.
// `field.wgsl` reads them at the same offsets (`SKYC_*`).
pub(crate) const H_ON: usize = 0;
pub(crate) const H_REACH: usize = 1;
pub(crate) const H_SETTLED: usize = 2;
pub(crate) const H_MOVING: usize = 3;
pub(crate) const H_BOXES: usize = 4;
pub(crate) const H_GRID_CELL: usize = 5;
/// Fill everything stale on every level (the heal pass).
pub(crate) const H_HEAL: usize = 6;
pub(crate) const H_GRID_ORIGIN: usize = 8;
/// Per level, [`H_LEVEL_WORDS`] words: window corner (camera-relative xyz),
/// cell size, the corner's lattice coordinate modulo the size (xyz), the
/// tag (parity of the corner's lattice coordinate divided by the size, per
/// axis, then the epoch), the dither period and phase.
pub(crate) const H_LEVEL: usize = 16;
pub(crate) const H_LEVEL_WORDS: usize = 12;
/// Per box, 8 words: min (camera-relative xyz), period, max (xyz), phase.
pub(crate) const H_BOX: usize = H_LEVEL + SKY_LEVELS * H_LEVEL_WORDS;
pub(crate) const H_GRID: usize = 128;
/// Words in the header.
pub const SKY_HEADER_WORDS: usize = H_GRID + (SKY_GRID * SKY_GRID * SKY_GRID) as usize;
/// Bytes of the header: a multiple of 256, so the cells start on a copy row.
pub const SKY_HEADER_BYTES: u64 = (SKY_HEADER_WORDS * 4) as u64;
const _: () = assert!(H_BOX + SKY_MAX_BOXES * 8 <= H_GRID);
const _: () = assert!(SKY_HEADER_BYTES.is_multiple_of(256));
const _: () = assert!(SKY_ATLAS_W.is_multiple_of(32));

/// Where a frame's cache stands: the camera, and the point the lattice is
/// anchored to, both in world units at full precision.
#[derive(Clone, Copy, Debug)]
pub struct SkyView {
    pub cam: DVec3,
    /// The lattice's anchor: the world origin for a world that holds still,
    /// the terrain's own position for one moving along its orbit. Its
    /// volumes then stay put relative to it and need no refill.
    pub anchor: DVec3,
    /// Names the anchor: a different id resets the cache.
    pub anchor_id: u64,
}

/// A box of the field that changed, camera-relative, as the renderer
/// records an edit uploaded into a volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SkyEdit {
    pub min: Vec3,
    pub max: Vec3,
}

/// What [`SkyCache::plan`] decided for a frame.
#[derive(Clone, Debug)]
pub struct SkyFrame {
    /// The header to write ahead of the cells.
    pub header: Vec<u32>,
    /// Whether the fill pass runs this frame.
    pub fill: bool,
    /// Cells the fill is expected to refill.
    pub cells: u32,
}

/// What [`SkyCache::plan`] has been doing, for counters and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SkyStats {
    /// Cells scrolled into each level's window this frame.
    pub scrolled: [u32; SKY_LEVELS],
    /// Levels whose window jumped clear of where it was (or reset).
    pub reset: [bool; SKY_LEVELS],
    /// Dirty boxes being refilled.
    pub boxes: usize,
    /// Proxies baked into the cache, and proxies read per pixel.
    pub settled: u32,
    pub moving: u32,
}

#[derive(Clone, Copy, Debug)]
struct DirtyBox {
    /// Anchor-relative.
    lo: DVec3,
    hi: DVec3,
    start: u64,
    period: u32,
    /// Cells a frame it refills.
    rate: f64,
}

/// A proxy as last seen, anchor-relative.
#[derive(Clone, Copy, Debug)]
struct Tracked {
    kind: u32,
    a: DVec3,
    b: DVec3,
    size: f32,
    rot: [f32; 4],
    still: u32,
    /// Read per pixel until this frame, while the box its settling dirtied
    /// is refilled (so its cover never drops out in between).
    analytic_until: u64,
}

/// A volume as last seen, anchor-relative.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VolSeen {
    kind: u32,
    centre: DVec3,
    half: [f32; 3],
    k: f32,
    key: u64,
}

/// The CPU half of the sky cache: one per cached view. See the module docs.
pub struct SkyCache {
    /// The `skyOcclusion` reach the cache answers; calls at any other reach
    /// march as before.
    pub reach: f32,
    /// Cells refilled per frame, about.
    pub budget: u32,
    frame: u64,
    anchor_id: Option<u64>,
    /// Each level's window corner, in lattice cells from the anchor.
    mins: Option<[[i64; 3]; SKY_LEVELS]>,
    epoch: [u32; SKY_LEVELS],
    /// Stale cells each level is estimated to still hold.
    pending: [f64; SKY_LEVELS],
    phase: [u32; SKY_LEVELS],
    boxes: Vec<DirtyBox>,
    proxies: Vec<Tracked>,
    vols: Vec<VolSeen>,
    blobs: Vec<(DVec3, f32)>,
    stats: SkyStats,
}

impl SkyCache {
    pub fn new(reach: f32) -> Self {
        SkyCache {
            reach,
            budget: SKY_BUDGET,
            frame: 0,
            anchor_id: None,
            mins: None,
            epoch: [0; SKY_LEVELS],
            pending: [0.0; SKY_LEVELS],
            phase: [0; SKY_LEVELS],
            boxes: Vec::new(),
            proxies: Vec::new(),
            vols: Vec::new(),
            blobs: Vec::new(),
            stats: SkyStats::default(),
        }
    }

    /// Start again: every cell is refilled, coarsest level first.
    pub fn invalidate(&mut self) {
        self.mins = None;
    }

    /// What the last [`plan`](Self::plan) did.
    pub fn stats(&self) -> SkyStats {
        self.stats
    }

    /// How far from a change a cell's cone can feel it: the furthest any
    /// ray starts off its centre, plus the reach and the penumbra's spread
    /// at its end.
    pub fn feel(&self) -> f64 {
        (CONE_LIFT_CAP + CONE_LIFT_CAP.max(self.reach) * (1.0 + 1.0 / CONE_K)) as f64
    }

    /// Plan this frame's cache for a view: diff the field's pieces in
    /// `g` (camera-relative, as uploaded) against last frame's, take in
    /// `edits`, scroll the levels, and lay out the header. `vol_keys[i]`
    /// names what volume slot `i` holds: a slot whose key changes is
    /// refilled.
    pub fn plan(&mut self, view: &SkyView, g: &RaymarchGlobals, vol_keys: &[u64], edits: &[SkyEdit]) -> SkyFrame {
        self.frame += 1;
        let mut stats = SkyStats::default();
        let rel = view.cam - view.anchor; // anchor → camera
        let shapes = g.shape_meta[0] >= 0.5;
        if self.anchor_id != Some(view.anchor_id) {
            self.anchor_id = Some(view.anchor_id);
            self.mins = None;
        }
        // A cache starting again refills everything anyway: nothing that
        // arrives with it is dirty.
        let fresh = self.mins.is_none();
        if fresh {
            self.boxes.clear();
            self.proxies.clear();
            self.vols.clear();
            self.blobs.clear();
        }
        let feel = self.feel();
        let to_anchor = |p: [f32; 4]| DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) + rel;

        // The levels' windows.
        let mut mins = [[0i64; 3]; SKY_LEVELS];
        for (l, m) in mins.iter_mut().enumerate() {
            let cell = level_cell(l) as f64;
            for a in 0..3 {
                m[a] = (rel[a] / cell).floor() as i64 - (SKY_DIMS / 2) as i64;
            }
        }
        match self.mins {
            None => {
                for l in 0..SKY_LEVELS {
                    self.reset_level(l, &mut stats);
                }
            }
            Some(old) => {
                for l in 0..SKY_LEVELS {
                    let d = SKY_DIMS as i64;
                    let mut keep = 1i64;
                    for a in 0..3 {
                        keep *= (d - (mins[l][a] - old[l][a]).abs()).max(0);
                    }
                    if keep == 0 {
                        self.reset_level(l, &mut stats);
                    } else {
                        let n = (d * d * d - keep) as u32;
                        stats.scrolled[l] = n;
                        self.pending[l] += n as f64;
                    }
                }
            }
        }
        self.mins = Some(mins);

        // Volumes that arrived, left, moved or changed what they hold.
        let mut vols = Vec::new();
        for i in 0..MAX_VOLUMES {
            let kind = g.vol_center[i][3];
            if kind < 0.5 {
                continue;
            }
            let h = g.vol_half[i];
            vols.push(VolSeen {
                kind: kind.round() as u32,
                centre: to_anchor(g.vol_center[i]),
                half: [h[0], h[1], h[2]],
                k: h[3],
                key: vol_keys.get(i).copied().unwrap_or(0) ^ ((g.vol_atlas[i][3].to_bits() as u64) << 32),
            });
        }
        let same_vol = |a: &VolSeen, b: &VolSeen| {
            a.kind == b.kind
                && a.key == b.key
                && (a.centre - b.centre).abs().max_element() <= STILL_TOLERANCE
                && a.half.iter().zip(&b.half).all(|(x, y)| (x - y).abs() <= 1e-3)
                && (a.k - b.k).abs() <= 1e-4
        };
        let mut dirty: Vec<(DVec3, DVec3)> = Vec::new();
        let vol_box = |v: &VolSeen| {
            let h = DVec3::new(v.half[0] as f64, v.half[1] as f64, v.half[2] as f64) + DVec3::splat(v.k as f64);
            (v.centre - h, v.centre + h)
        };
        for v in &vols {
            if !self.vols.iter().any(|o| same_vol(o, v)) {
                dirty.push(vol_box(v));
            }
        }
        for o in &self.vols {
            if !vols.iter().any(|v| same_vol(o, v)) {
                dirty.push(vol_box(o));
            }
        }
        self.vols = vols;

        // Blobs, by slot.
        let bc = (g.params[1].max(0.0) as usize).min(MAX_BLOBS);
        let blob_r = |s: f32| {
            let s = s.max(0.02);
            (s + (0.3 * s).max(g.params[2])) as f64
        };
        let blobs: Vec<(DVec3, f32)> = (0..bc).map(|i| (to_anchor(g.blobs[i]), g.blobs[i][3])).collect();
        for i in 0..blobs.len().max(self.blobs.len()) {
            let (now, was) = (blobs.get(i), self.blobs.get(i));
            let moved = match (now, was) {
                (Some(a), Some(b)) => (a.0 - b.0).abs().max_element() > STILL_TOLERANCE || (a.1 - b.1).abs() > 1e-3,
                _ => true,
            };
            if moved {
                for (c, s) in [now, was].into_iter().flatten() {
                    let r = DVec3::splat(blob_r(*s));
                    dirty.push((*c - r, *c + r));
                }
            }
        }
        self.blobs = blobs;

        // Edits uploaded into the volumes.
        for e in edits {
            dirty.push((to_anchor([e.min.x, e.min.y, e.min.z, 0.0]), to_anchor([e.max.x, e.max.y, e.max.z, 0.0])));
        }

        // Proxies: matched to last frame's by where they are, not by slot,
        // since the slots are re-sorted by distance every frame.
        let pc = (g.prox_count[0].max(0.0) as usize).min(MAX_SHADOW_PROXIES);
        let mut now: Vec<Tracked> = Vec::with_capacity(pc);
        for i in 0..pc {
            let (a, b) = (g.prox_a[i], g.prox_b[i]);
            let kind = b[3].round() as u32;
            now.push(Tracked {
                kind,
                a: to_anchor(a),
                b: if kind == 1 { to_anchor(b) } else { DVec3::new(b[0] as f64, b[1] as f64, b[2] as f64) },
                size: a[3],
                rot: if kind == 2 { g.prox_rot[i] } else { [0.0, 0.0, 0.0, 1.0] },
                still: 0,
                analytic_until: 0,
            });
        }
        let mut matched = vec![false; self.proxies.len()];
        for p in now.iter_mut() {
            let hit = self.proxies.iter().enumerate().position(|(j, o)| !matched[j] && same_proxy(o, p));
            if let Some(j) = hit {
                matched[j] = true;
                let o = self.proxies[j];
                p.still = o.still.saturating_add(1);
                p.analytic_until = o.analytic_until;
                if o.still + 1 == SKY_SETTLE_FRAMES {
                    // Settled this frame: bake it in.
                    dirty.push(proxy_box(p));
                }
            }
        }
        for (j, o) in self.proxies.iter().enumerate() {
            if !matched[j] && o.still >= SKY_SETTLE_FRAMES {
                // A baked proxy moved or left the slots: bake it out.
                dirty.push(proxy_box(o));
            }
        }

        // The dirty boxes, grown by how far a cone can feel a change.
        if fresh {
            dirty.clear();
        }
        // A volume whose content changed in place leaves and arrives in the
        // same box: one refill.
        let same = |a: &(DVec3, DVec3), b: &(DVec3, DVec3)| {
            (a.0 - b.0).abs().max_element() <= STILL_TOLERANCE && (a.1 - b.1).abs().max_element() <= STILL_TOLERANCE
        };
        let mut unique: Vec<(DVec3, DVec3)> = Vec::with_capacity(dirty.len());
        for d in dirty {
            if !unique.iter().any(|u| same(u, &d)) {
                unique.push(d);
            }
        }
        for (lo, hi) in unique {
            self.add_box(lo - DVec3::splat(feel), hi + DVec3::splat(feel));
        }
        let frame = self.frame;
        self.boxes.retain(|b| frame < b.start + b.period as u64);
        let box_end = self.boxes.iter().map(|b| b.start + b.period as u64).max().unwrap_or(frame);
        let mut settled = 0u32;
        let mut moving = 0u32;
        for (i, p) in now.iter_mut().enumerate() {
            if p.still == SKY_SETTLE_FRAMES {
                p.analytic_until = box_end;
            }
            if p.still >= SKY_SETTLE_FRAMES {
                settled |= 1 << i;
            }
            if p.still < SKY_SETTLE_FRAMES || frame < p.analytic_until {
                moving |= 1 << i;
            }
        }
        self.proxies = now;
        stats.settled = settled;
        stats.moving = moving;
        stats.boxes = self.boxes.len();

        // The levels' dither: the coarsest first, each taking what is left
        // of the budget after the boxes.
        let box_rate: f64 = self.boxes.iter().map(|b| b.rate).sum();
        let mut left = (self.budget as f64 - box_rate).max(self.budget as f64 / 4.0);
        let mut periods = [1u32; SKY_LEVELS];
        let mut cells = box_rate;
        for l in (0..SKY_LEVELS).rev() {
            let want = self.pending[l];
            if want < 1.0 {
                self.pending[l] = 0.0;
                continue;
            }
            let share = want.min(left).max(1.0);
            let p = (want / share).ceil().clamp(1.0, 4096.0) as u32;
            periods[l] = p;
            self.phase[l] = (self.phase[l] + 1) % p;
            let done = want / p as f64;
            left = (left - done).max(0.0);
            cells += done;
            self.pending[l] = (want - done).max(0.0);
        }
        let heal = frame.is_multiple_of(HEAL_EVERY);
        let fill = !shapes && (cells >= 1.0 || heal);

        // The header.
        let mut h = vec![0u32; SKY_HEADER_WORDS];
        let f = |x: f32| x.to_bits();
        h[H_ON] = (!shapes) as u32;
        h[H_REACH] = f(self.reach);
        h[H_SETTLED] = settled;
        h[H_MOVING] = moving;
        h[H_HEAL] = heal as u32;
        for l in 0..SKY_LEVELS {
            let cell = level_cell(l);
            let o = H_LEVEL + l * H_LEVEL_WORDS;
            let d = SKY_DIMS as i64;
            let mut tag = 0u32;
            for a in 0..3 {
                let corner = view.anchor[a] + mins[l][a] as f64 * cell as f64 - view.cam[a];
                h[o + a] = f(corner as f32);
                h[o + 4 + a] = mins[l][a].rem_euclid(d) as u32;
                tag |= ((mins[l][a].div_euclid(d) & 1) as u32) << a;
            }
            h[o + 3] = f(cell);
            h[o + 7] = tag | ((self.epoch[l] & 7) << 3);
            h[o + 8] = periods[l];
            h[o + 9] = if self.pending[l] > 0.0 || periods[l] > 1 { self.phase[l] } else { 0 };
        }
        h[H_BOXES] = self.boxes.len() as u32;
        for (k, b) in self.boxes.iter().enumerate() {
            let o = H_BOX + k * 8;
            for a in 0..3 {
                h[o + a] = f((b.lo[a] - rel[a]) as f32);
                h[o + 4 + a] = f((b.hi[a] - rel[a]) as f32);
            }
            h[o + 3] = b.period;
            h[o + 7] = ((frame - b.start) % b.period as u64) as u32;
        }
        // The moving proxies' mask grid, round the camera.
        let gh = SKY_GRID as f32 * SKY_GRID_CELL * 0.5;
        h[H_GRID_CELL] = f(SKY_GRID_CELL);
        for a in 0..3 {
            h[H_GRID_ORIGIN + a] = f(-gh);
        }
        let mut m = moving;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let (c, r) = prox_bound(g, i);
            let r = r + SKY_MOVING_REACH;
            let cell_of = |x: f32| ((x + gh) / SKY_GRID_CELL).floor() as i32;
            let (lo, hi) = (c - Vec3::splat(r), c + Vec3::splat(r));
            let n = SKY_GRID as i32;
            let (x0, x1) = (cell_of(lo.x).max(0), cell_of(hi.x).min(n - 1));
            let (y0, y1) = (cell_of(lo.y).max(0), cell_of(hi.y).min(n - 1));
            let (z0, z1) = (cell_of(lo.z).max(0), cell_of(hi.z).min(n - 1));
            for z in z0..=z1 {
                for y in y0..=y1 {
                    for x in x0..=x1 {
                        h[H_GRID + ((z * n + y) * n + x) as usize] |= 1 << i;
                    }
                }
            }
        }
        self.stats = stats;
        SkyFrame { header: h, fill, cells: cells as u32 }
    }

    fn reset_level(&mut self, l: usize, stats: &mut SkyStats) {
        self.epoch[l] = self.epoch[l].wrapping_add(1);
        self.pending[l] = (SKY_DIMS * SKY_DIMS * SKY_DIMS) as f64;
        self.phase[l] = 0;
        stats.reset[l] = true;
        stats.scrolled[l] = SKY_DIMS * SKY_DIMS * SKY_DIMS;
    }

    /// Queue an anchor-relative box for refilling, merging it into the box
    /// it grows least when there are already [`SKY_MAX_BOXES`].
    fn add_box(&mut self, lo: DVec3, hi: DVec3) {
        let (lo, hi) = if self.boxes.len() >= SKY_MAX_BOXES {
            let vol = |l: DVec3, h: DVec3| (h - l).max(DVec3::ZERO).element_product();
            let k = (0..self.boxes.len())
                .min_by(|&i, &j| {
                    let g = |b: &DirtyBox| vol(b.lo.min(lo), b.hi.max(hi)) - vol(b.lo, b.hi);
                    g(&self.boxes[i]).total_cmp(&g(&self.boxes[j]))
                })
                .unwrap_or(0);
            let b = self.boxes.remove(k);
            (b.lo.min(lo), b.hi.max(hi))
        } else {
            (lo, hi)
        };
        let cells = self.box_cells(lo, hi);
        // Half the budget goes to boxes; a new one takes what the others
        // leave, and never less than an eighth of that half.
        let half = self.budget as f64 * 0.5;
        let used: f64 = self.boxes.iter().map(|b| b.rate).sum();
        let room = (half - used).max(half / SKY_MAX_BOXES as f64);
        let period = (cells / room).ceil().clamp(1.0, 600.0) as u32;
        self.boxes.push(DirtyBox { lo, hi, start: self.frame, period, rate: cells / period as f64 });
    }

    /// Cells of every level's window inside an anchor-relative box.
    fn box_cells(&self, lo: DVec3, hi: DVec3) -> f64 {
        let Some(mins) = self.mins else { return 0.0 };
        let mut n = 0.0;
        for (l, m) in mins.iter().enumerate() {
            let cell = level_cell(l) as f64;
            let mut v = 1.0;
            for a in 0..3 {
                let w0 = m[a] as f64 * cell;
                let w1 = w0 + SKY_DIMS as f64 * cell;
                v *= ((hi[a].min(w1) - lo[a].max(w0)) / cell).max(0.0);
            }
            n += v;
        }
        n
    }
}

/// Level `l`'s cell size.
pub fn level_cell(l: usize) -> f32 {
    SKY_CELL0 * (1u32 << l) as f32
}

fn same_proxy(a: &Tracked, b: &Tracked) -> bool {
    a.kind == b.kind
        && (a.a - b.a).abs().max_element() <= STILL_TOLERANCE
        && (a.b - b.b).abs().max_element() <= STILL_TOLERANCE
        && (a.size - b.size).abs() <= 1e-3
        && a.rot.iter().zip(&b.rot).all(|(x, y)| (x - y).abs() <= 1e-3)
}

/// An anchor-relative box round a tracked proxy.
fn proxy_box(p: &Tracked) -> (DVec3, DVec3) {
    match p.kind {
        0 => (p.a - DVec3::splat(p.size as f64), p.a + DVec3::splat(p.size as f64)),
        1 => {
            let r = DVec3::splat(p.size as f64);
            (p.a.min(p.b) - r, p.a.max(p.b) + r)
        }
        _ => {
            let r = DVec3::splat(p.b.length());
            (p.a - r, p.a + r)
        }
    }
}

/// Proxy `i`'s bounding sphere, camera-relative (`prox_bound` in `field.wgsl`).
fn prox_bound(g: &RaymarchGlobals, i: usize) -> (Vec3, f32) {
    let a = Vec3::new(g.prox_a[i][0], g.prox_a[i][1], g.prox_a[i][2]);
    let b = Vec3::new(g.prox_b[i][0], g.prox_b[i][1], g.prox_b[i][2]);
    match g.prox_b[i][3] {
        k if k < 0.5 => (a, g.prox_a[i][3]),
        k if k < 1.5 => ((a + b) * 0.5, (b - a).length() * 0.5 + g.prox_a[i][3]),
        _ => (a, b.length()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(cam: DVec3) -> SkyView {
        SkyView { cam, anchor: DVec3::ZERO, anchor_id: 1 }
    }

    /// Globals with one terrain volume round the world origin, as seen from `cam`.
    fn globals(cam: DVec3) -> RaymarchGlobals {
        let mut g = RaymarchGlobals::default();
        let c = (DVec3::new(0.0, -10.0, 0.0) - cam).as_vec3();
        g.vol_center[0] = [c.x, c.y, c.z, 3.0];
        g.vol_half[0] = [100.0, 20.0, 100.0, 0.6];
        g
    }

    /// Put a box proxy at world `p` in slot `i`.
    fn put_box(g: &mut RaymarchGlobals, cam: DVec3, i: usize, p: DVec3) {
        let c = (p - cam).as_vec3();
        g.prox_a[i] = [c.x, c.y, c.z, 0.0];
        g.prox_b[i] = [1.0, 0.5, 1.0, 2.0];
        g.prox_rot[i] = [0.0, 0.0, 0.0, 1.0];
        g.prox_count[0] = g.prox_count[0].max(i as f32 + 1.0);
    }

    fn settle(c: &mut SkyCache, cam: DVec3, g: &RaymarchGlobals) {
        for _ in 0..SKY_SETTLE_FRAMES + 1 {
            c.plan(&view(cam), g, &[], &[]);
        }
    }

    #[test]
    fn a_still_camera_refills_nothing_once_full() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::new(3.2, 4.0, -7.5);
        let g = globals(cam);
        let first = c.plan(&view(cam), &g, &[], &[]);
        assert!(c.stats().reset.iter().all(|r| *r), "{:?}", c.stats());
        assert!(first.fill && first.cells > 0);
        let mut frames = 1;
        loop {
            let f = c.plan(&view(cam), &g, &[], &[]);
            frames += 1;
            if f.cells == 0 {
                break;
            }
            assert!(frames < 400, "the cache never settles");
        }
        // A full refill of all four levels, at the budget, takes about
        // cells / budget frames.
        let want = (SKY_CELLS / SKY_BUDGET) as i32;
        assert!((frames - want).abs() <= want / 2 + 4, "a full fill took {frames} frames, about {want} expected");
        for _ in 0..20 {
            let f = c.plan(&view(cam), &g, &[], &[]);
            assert_eq!(f.cells, 0);
            assert_eq!(c.stats().scrolled, [0; SKY_LEVELS]);
        }
    }

    #[test]
    fn a_one_cell_step_scrolls_one_slab() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::new(0.5, 0.5, 0.5);
        c.plan(&view(cam), &globals(cam), &[], &[]);
        // One finest cell along x: one slab of the finest level; the 2-unit
        // level's window stays (0.5 → 1.5 is inside one of its cells).
        let cam2 = cam + DVec3::X;
        c.plan(&view(cam2), &globals(cam2), &[], &[]);
        let d2 = SKY_DIMS * SKY_DIMS;
        assert_eq!(c.stats().scrolled, [d2, 0, 0, 0]);
        assert!(!c.stats().reset.iter().any(|r| *r));
        // Into the next 2-unit cell: one slab of each of the two finest.
        let cam3 = cam2 + DVec3::X;
        c.plan(&view(cam3), &globals(cam3), &[], &[]);
        assert_eq!(c.stats().scrolled, [d2, d2, 0, 0]);
        // Diagonally one cell: three slabs less their overlaps.
        let cam4 = cam3 + DVec3::new(0.0, 1.0, 1.0);
        c.plan(&view(cam4), &globals(cam4), &[], &[]);
        let d = SKY_DIMS;
        assert_eq!(c.stats().scrolled[0], d * d * d - d * (d - 1) * (d - 1));
    }

    #[test]
    fn a_teleport_or_a_new_anchor_resets() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::ZERO;
        c.plan(&view(cam), &globals(cam), &[], &[]);
        // Sixty units: clear of the finest level's 48-unit window, inside
        // the coarser ones.
        let far = DVec3::new(60.0, 0.0, 0.0);
        c.plan(&view(far), &globals(far), &[], &[]);
        assert_eq!(c.stats().reset, [true, false, false, false]);
        let mut v = view(far);
        v.anchor_id = 2;
        c.plan(&v, &globals(far), &[], &[]);
        assert_eq!(c.stats().reset, [true; SKY_LEVELS]);
        // The tag's epoch moves on with each reset.
        let f = c.plan(&v, &globals(far), &[], &[]);
        let tag = f.header[H_LEVEL + 7];
        assert_eq!(tag >> 3, 3, "finest level reset three times");
    }

    #[test]
    fn an_edit_refills_the_cells_that_can_feel_it() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 5.0, 0.0);
        let g = globals(cam);
        c.plan(&view(cam), &g, &[], &[]);
        let edit = SkyEdit { min: Vec3::new(1.0, -6.0, 2.0), max: Vec3::new(3.0, -4.0, 3.0) };
        let f = c.plan(&view(cam), &g, &[], &[edit]);
        assert_eq!(f.header[H_BOXES], 1);
        let feel = c.feel() as f32;
        let at = |w: usize| f32::from_bits(f.header[H_BOX + w]);
        for a in 0..3 {
            assert!((at(a) - (edit.min[a] - feel)).abs() < 1e-3, "min {a}: {}", at(a));
            assert!((at(4 + a) - (edit.max[a] + feel)).abs() < 1e-3, "max {a}: {}", at(4 + a));
        }
        // The box is camera-relative: the camera moving moves it the other way.
        let cam2 = cam + DVec3::new(0.0, 0.0, 0.25);
        let f = c.plan(&view(cam2), &globals(cam2), &[], &[]);
        assert!((f32::from_bits(f.header[H_BOX + 2]) - (edit.min.z - feel - 0.25)).abs() < 1e-3);
        // It is retired once its dither has gone round once.
        let period = f.header[H_BOX + 3];
        for _ in 0..period {
            c.plan(&view(cam2), &globals(cam2), &[], &[]);
        }
        assert_eq!(c.stats().boxes, 0);
    }

    #[test]
    fn a_proxy_that_moves_leaves_the_cache_and_one_resorted_stays() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        put_box(&mut g, cam, 0, DVec3::new(5.0, 0.5, 0.0));
        put_box(&mut g, cam, 1, DVec3::new(-5.0, 0.5, 3.0));
        c.plan(&view(cam), &g, &[], &[]);
        assert_eq!(c.stats().settled, 0, "nothing settled on its first frame");
        assert_eq!(c.stats().moving, 0b11);
        settle(&mut c, cam, &g);
        assert_eq!(c.stats().settled, 0b11);
        // Settling queued a box each (merged or not), to bake them in.
        assert!(c.stats().boxes >= 1);
        // Re-sorted: the same two proxies in each other's slots stay settled.
        let mut swapped = g;
        swapped.prox_a.swap(0, 1);
        swapped.prox_b.swap(0, 1);
        let boxes = c.stats().boxes;
        c.plan(&view(cam), &swapped, &[], &[]);
        assert_eq!(c.stats().settled, 0b11);
        assert!(c.stats().boxes <= boxes, "a re-sort dirtied nothing");
        // Run the bake boxes out, then move one by 5 cm.
        for _ in 0..700 {
            c.plan(&view(cam), &swapped, &[], &[]);
        }
        assert_eq!(c.stats().boxes, 0);
        assert_eq!(c.stats().moving, 0, "both read from the cache alone once baked");
        let mut moved = swapped;
        moved.prox_a[1][0] += 0.05;
        let f = c.plan(&view(cam), &moved, &[], &[]);
        assert_eq!(c.stats().settled, 0b01, "the moved one left the cache");
        assert_eq!(c.stats().moving, 0b10, "and is read per pixel");
        assert_eq!(c.stats().boxes, 1, "its old place is refilled");
        // Its old box holds where it stood.
        let lo = f32::from_bits(f.header[H_BOX]);
        let hi = f32::from_bits(f.header[H_BOX + 4]);
        let x = 5.0 - cam.x as f32;
        assert!(lo < x && x < hi, "{lo}..{hi} misses {x}");
        // The mask grid marks it near its place, and nowhere far off.
        let gh = SKY_GRID as f32 * SKY_GRID_CELL * 0.5;
        let cell = |p: Vec3| {
            let i = ((p + Vec3::splat(gh)) / SKY_GRID_CELL).floor().as_ivec3();
            H_GRID + ((i.z * SKY_GRID as i32 + i.y) * SKY_GRID as i32 + i.x) as usize
        };
        let near = Vec3::new(5.05, 0.5, 0.0) - cam.as_vec3();
        assert_eq!(f.header[cell(near)], 0b10);
        assert_eq!(f.header[cell(Vec3::new(-28.0, 0.0, -28.0))], 0);
    }

    #[test]
    fn the_budget_holds_through_a_reset_and_a_storm_of_edits() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::ZERO;
        let g = globals(cam);
        for k in 0..200 {
            let edits: Vec<SkyEdit> = (0..3)
                .map(|j| {
                    let p = Vec3::new(((k * 7 + j * 13) % 90) as f32 - 45.0, 0.0, ((k * 11 + j * 5) % 90) as f32 - 45.0);
                    SkyEdit { min: p, max: p + Vec3::ONE }
                })
                .collect();
            let f = c.plan(&view(cam), &g, &[], &edits);
            assert!(c.stats().boxes <= SKY_MAX_BOXES);
            // The boxes' floor rate can push a little past the budget; never
            // more than half again.
            assert!(f.cells as f64 <= SKY_BUDGET as f64 * 1.5 + 1.0, "frame {k}: {} cells", f.cells);
        }
    }

    #[test]
    fn a_world_moving_along_its_orbit_refills_nothing() {
        // The anchor is the terrain's own position, and the terrain moves
        // with it; the camera rides along.
        let mut c = SkyCache::new(32.0);
        let mut g;
        let mut cam;
        let mut anchor = DVec3::new(1.0e6, 0.0, 0.0);
        for k in 0..400 {
            anchor += DVec3::new(30.0, 0.0, 12.0); // 30 units a frame
            cam = anchor + DVec3::new(0.3, 4.0, -2.0);
            g = RaymarchGlobals::default();
            let t = (anchor + DVec3::new(0.0, -10.0, 0.0) - cam).as_vec3();
            g.vol_center[0] = [t.x, t.y, t.z, 3.0];
            g.vol_half[0] = [100.0, 20.0, 100.0, 0.6];
            let f = c.plan(&SkyView { cam, anchor, anchor_id: 7 }, &g, &[1], &[]);
            if k > 100 {
                assert_eq!(f.cells, 0, "frame {k}: {:?}", c.stats());
                assert_eq!(c.stats().boxes, 0);
            }
        }
    }

    #[test]
    fn field_shapes_switch_the_cache_off() {
        let mut c = SkyCache::new(32.0);
        let mut g = globals(DVec3::ZERO);
        g.shape_meta[0] = 1.0;
        let f = c.plan(&view(DVec3::ZERO), &g, &[], &[]);
        assert_eq!(f.header[H_ON], 0);
        assert!(!f.fill);
    }

    #[test]
    fn a_volume_whose_key_changes_is_refilled() {
        let mut c = SkyCache::new(32.0);
        let g = globals(DVec3::ZERO);
        for _ in 0..3 {
            c.plan(&view(DVec3::ZERO), &g, &[5], &[]);
        }
        assert_eq!(c.stats().boxes, 0);
        c.plan(&view(DVec3::ZERO), &g, &[6], &[]);
        assert_eq!(c.stats().boxes, 1);
    }
}
