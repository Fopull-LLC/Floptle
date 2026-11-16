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
//! **What a cell holds** (four 32-bit words): the cone's answer for each of
//! the six axis directions (8 bits each), the static field's distance at
//! the centre, whether it has been filled and whether its centre is outside
//! solid matter; then which lattice cell it was filled for (ten bits of each
//! coordinate) and the level's epoch then. A cell whose place or epoch is
//! not the one it stands for now reads as not filled.
//!
//! **What goes in, and what is left to each pixel.** Everything the field
//! holds that stands still: volumes, blobs, and collider proxies that have
//! not moved for [`SKY_SETTLE_FRAMES`] frames (a static dock or roof is a
//! proxy too). Still means within [`STILL_TOLERANCE`] of where it was when
//! it last started holding still, or of where it was baked in, so a slow
//! drift adds up and is caught. A proxy that moves is left out and each
//! pixel near it reads a short-range term instead, found through a small
//! grid of masks around the camera ([`SKY_GRID`]) so no pixel loops over
//! every slot. A scene with Field Shapes, whose code can animate with time,
//! is not cached; switching them on or off starts the cache again.
//!
//! **Refilling.** The planner here runs on the CPU each frame. Everything to
//! refill is a box of the lattice on some of the levels: a level's whole
//! window when it starts again, the slabs a scroll exposed, and the boxes
//! round a field edit, a volume or blob that moved, a proxy that settled or
//! started moving. Boxes wait in a queue by priority (a level starting
//! again, coarsest first; then whatever would leave wrong cover showing: an
//! edit, a bake-out; then scrolls and bake-ins) and start when the budget
//! of [`SKY_BUDGET`] cells a frame has room. A started box refills its cells
//! in a dither over its period, a fixed share each frame, so a change
//! dissolves in instead of wiping, and it is never merged or restarted:
//! after its period every cell in it has been refilled once. Waiting boxes
//! are merged with others like them. A still camera with nothing dirty
//! refills nothing and runs no pass.
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
/// Cells refilled per frame, at most.
pub const SKY_BUDGET: u32 = 4096;
/// Frames a proxy must hold still before the cache takes it in.
pub const SKY_SETTLE_FRAMES: u32 = 30;
/// Boxes refilling at once (the header's room for them).
pub const SKY_MAX_BOXES: usize = 16;
/// Boxes waiting at most; past it, a new one merges into a waiting one.
const MAX_WAITING: usize = 64;
/// Cells along each side of the moving-proxy mask grid.
pub const SKY_GRID: u32 = 16;
/// The mask grid's cell size, world units.
pub const SKY_GRID_CELL: f32 = 4.0;
/// How far past its bound a moving proxy's short-range term reaches.
pub const SKY_MOVING_REACH: f32 = 8.0;
/// How far a placement may drift, world units, from where it was baked in
/// (or started holding still) and still count as unmoved.
pub const STILL_TOLERANCE: f64 = 0.01;
/// How far a proxy may go in a frame and still be the same proxy, for
/// following it across the slots' re-sorting.
const FOLLOW_TOLERANCE: f64 = 1.0;
/// The cone's penumbra constant and its furthest lift (`field.wgsl`).
const CONE_K: f32 = 8.0;
const CONE_LIFT_CAP: f32 = 3.0;
/// Longest dither, frames, by priority: wrong cover showing (an edit, a
/// bake-out) is put right within a second; anything else within ten.
const CAP_URGENT: u32 = 60;
const CAP_CALM: u32 = 600;

/// Texels across the cache's atlas (rows of it are what the buffer holds).
/// A multiple of 16, so a row is a whole number of 256-byte copy rows.
pub const SKY_ATLAS_W: u32 = 1536;
/// Cells in every level together.
pub const SKY_CELLS: u32 = SKY_DIMS * SKY_DIMS * SKY_DIMS * SKY_LEVELS as u32;
/// Rows of the atlas.
pub const SKY_ATLAS_H: u32 = SKY_CELLS.div_ceil(SKY_ATLAS_W);
/// Bytes of one cell.
pub const SKY_TEXEL: u32 = 16;
/// Rows of the atlas each level takes.
pub const SKY_LEVEL_ROWS: u32 = SKY_DIMS * SKY_DIMS * SKY_DIMS / SKY_ATLAS_W;
const _: () = assert!(SKY_LEVEL_ROWS * SKY_ATLAS_W == SKY_DIMS * SKY_DIMS * SKY_DIMS);

// The header: 32-bit words at the front of the buffer, ahead of the cells.
// `field.wgsl` reads them at the same offsets (`SKYC_*`).
pub(crate) const H_ON: usize = 0;
pub(crate) const H_REACH: usize = 1;
pub(crate) const H_SETTLED: usize = 2;
pub(crate) const H_MOVING: usize = 3;
pub(crate) const H_BOXES: usize = 4;
pub(crate) const H_GRID_CELL: usize = 5;
pub(crate) const H_GRID_ORIGIN: usize = 8;
/// Per level, [`H_LEVEL_WORDS`] words: window corner (camera-relative xyz),
/// cell size, the corner's lattice coordinate modulo the size (xyz), its
/// low ten bits on each axis packed, and the level's epoch.
pub(crate) const H_LEVEL: usize = 16;
pub(crate) const H_LEVEL_WORDS: usize = 12;
/// Per box, 8 words: min (camera-relative xyz), period with the levels it
/// covers in the top byte, max (xyz), phase.
pub(crate) const H_BOX: usize = H_LEVEL + SKY_LEVELS * H_LEVEL_WORDS;
pub(crate) const H_GRID: usize = 256;
/// Words in the header.
pub const SKY_HEADER_WORDS: usize = H_GRID + (SKY_GRID * SKY_GRID * SKY_GRID) as usize;
/// Bytes of the header: a multiple of 256, so the cells start on a copy row.
pub const SKY_HEADER_BYTES: u64 = (SKY_HEADER_WORDS * 4) as u64;
const _: () = assert!(H_BOX + SKY_MAX_BOXES * 8 <= H_GRID);
const _: () = assert!(SKY_HEADER_BYTES.is_multiple_of(256));
const _: () = assert!((SKY_ATLAS_W * SKY_TEXEL).is_multiple_of(256));

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
    /// Cells the fill refills this frame, about (the boxes' shares).
    pub cells: u32,
    /// The levels the fill pass need visit: those a refilling box covers.
    /// Each is [`SKY_LEVEL_ROWS`] rows of the atlas, so the pass and the
    /// copy skip the rest.
    pub due: [bool; SKY_LEVELS],
}

/// What [`SkyCache::plan`] has been doing, for counters and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SkyStats {
    /// Cells scrolled into each level's window this frame.
    pub scrolled: [u32; SKY_LEVELS],
    /// Levels whose window started again this frame.
    pub reset: [bool; SKY_LEVELS],
    /// Boxes refilling, and boxes waiting for room.
    pub boxes: usize,
    pub waiting: usize,
    /// Proxies baked into the cache, and proxies read per pixel.
    pub settled: u32,
    pub moving: u32,
}

/// A box to refill, anchor-relative, waiting for room.
#[derive(Clone, Debug)]
struct Job {
    lo: DVec3,
    hi: DVec3,
    levels: u8,
    /// Lower goes first.
    prio: u32,
    cap: u32,
    /// The proxies this box bakes in: each is read per pixel until it is done.
    bakes: Vec<u64>,
}

/// A box refilling: started at `start`, done after `period` frames.
#[derive(Clone, Debug)]
struct Active {
    job: Job,
    start: u64,
    period: u32,
    rate: f64,
}

/// A proxy as tracked, anchor-relative.
#[derive(Clone, Copy, Debug)]
struct Pose {
    kind: u32,
    a: DVec3,
    b: DVec3,
    size: f32,
    rot: [f32; 4],
}

#[derive(Clone, Copy, Debug)]
struct Tracked {
    id: u64,
    /// Where it is this frame.
    now: Pose,
    /// Where it started holding still.
    rest: Pose,
    still: u32,
    /// Where it was baked in, while it is.
    baked: Option<Pose>,
}

/// A volume as baked, anchor-relative.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VolSeen {
    kind: u32,
    centre: DVec3,
    half: [f32; 3],
    k: f32,
    key: u64,
}

/// The CPU half of the sky cache, for one view. See the module docs.
pub struct SkyCache {
    /// The `skyOcclusion` reach the cache answers; calls at any other reach
    /// march as before.
    pub reach: f32,
    /// Cells refilled per frame, at most.
    pub budget: u32,
    frame: u64,
    anchor_id: Option<u64>,
    /// Each level's window corner, in lattice cells from the anchor.
    mins: Option<[[i64; 3]; SKY_LEVELS]>,
    epoch: [u32; SKY_LEVELS],
    waiting: Vec<Job>,
    active: Vec<Active>,
    proxies: Vec<Tracked>,
    next_id: u64,
    vols: Vec<VolSeen>,
    blobs: Vec<(DVec3, f32)>,
    blend: f32,
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
            waiting: Vec::new(),
            active: Vec::new(),
            proxies: Vec::new(),
            next_id: 1,
            vols: Vec::new(),
            blobs: Vec::new(),
            blend: 0.0,
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
    /// `g` (camera-relative, as uploaded) against what the cache holds,
    /// take in `edits`, scroll the levels, start the boxes the budget has
    /// room for, and lay out the header. `vol_keys[i]` names what volume
    /// slot `i` holds: a slot whose key changes is refilled.
    pub fn plan(&mut self, view: &SkyView, g: &RaymarchGlobals, vol_keys: &[u64], edits: &[SkyEdit]) -> SkyFrame {
        self.frame += 1;
        let frame = self.frame;
        let mut stats = SkyStats::default();
        let shapes = g.shape_meta[0] >= 0.5;
        if shapes {
            // Not cached, and nothing it held can be trusted once it is
            // again: the shapes may have been anywhere meanwhile.
            self.mins = None;
            self.stats = stats;
            return SkyFrame { header: vec![0; SKY_HEADER_WORDS], fill: false, cells: 0, due: [false; SKY_LEVELS] };
        }
        let rel = view.cam - view.anchor; // anchor → camera
        if self.anchor_id != Some(view.anchor_id) || (g.params[2] - self.blend).abs() > 1e-6 {
            self.anchor_id = Some(view.anchor_id);
            self.blend = g.params[2];
            self.mins = None;
        }
        let fresh = self.mins.is_none();
        if fresh {
            self.waiting.clear();
            self.active.clear();
            self.proxies.clear();
            self.vols.clear();
            self.blobs.clear();
        }
        let feel = self.feel();
        let to_anchor = |p: [f32; 4]| DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) + rel;

        // The levels' windows, and what each scroll exposed.
        let d = SKY_DIMS as i64;
        let mut mins = [[0i64; 3]; SKY_LEVELS];
        for (l, m) in mins.iter_mut().enumerate() {
            let cell = level_cell(l) as f64;
            for a in 0..3 {
                m[a] = (rel[a] / cell).floor() as i64 - d / 2;
            }
        }
        let old = self.mins;
        self.mins = Some(mins);
        for l in 0..SKY_LEVELS {
            let window = |m: [i64; 3]| {
                let c = level_cell(l) as f64;
                (DVec3::new(m[0] as f64, m[1] as f64, m[2] as f64) * c, DVec3::new((m[0] + d) as f64, (m[1] + d) as f64, (m[2] + d) as f64) * c)
            };
            let jumped = old.is_none_or(|o| (0..3).any(|a| (mins[l][a] - o[l][a]).abs() >= d));
            if jumped {
                self.epoch[l] = self.epoch[l].wrapping_add(1);
                stats.reset[l] = true;
                stats.scrolled[l] = SKY_DIMS * SKY_DIMS * SKY_DIMS;
                // Its old boxes refill cells that are gone.
                self.drop_level(l);
                let (lo, hi) = window(mins[l]);
                self.queue(Job { lo, hi, levels: 1 << l, prio: (SKY_LEVELS - 1 - l) as u32, cap: CAP_CALM, bakes: Vec::new() });
                continue;
            }
            let o = old.unwrap_or(mins)[l];
            if o == mins[l] {
                continue;
            }
            // The new window less the old: up to three slabs, one per axis
            // that moved, each clear of the ones before it.
            let (mut lo, mut hi) = window(mins[l]);
            let (olo, ohi) = window(o);
            let mut n = 0.0;
            for a in 0..3 {
                if mins[l][a] == o[a] {
                    continue;
                }
                let (mut slo, mut shi) = (lo, hi);
                if mins[l][a] > o[a] {
                    slo[a] = ohi[a];
                    hi[a] = ohi[a];
                } else {
                    shi[a] = olo[a];
                    lo[a] = olo[a];
                }
                n += self.level_cells(l, slo, shi);
                self.queue(Job { lo: slo, hi: shi, levels: 1 << l, prio: 10, cap: CAP_URGENT, bakes: Vec::new() });
            }
            stats.scrolled[l] = n.round() as u32;
        }

        let mut urgent: Vec<(DVec3, DVec3)> = Vec::new();

        // Volumes that arrived, left, moved or changed what they hold, since
        // they were baked in.
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
        let vol_box = |v: &VolSeen| {
            let h = DVec3::new(v.half[0] as f64, v.half[1] as f64, v.half[2] as f64) + DVec3::splat(v.k as f64);
            (v.centre - h, v.centre + h)
        };
        let mut kept: Vec<VolSeen> = Vec::new();
        for v in &vols {
            match self.vols.iter().find(|o| same_vol(o, v)) {
                // Still as baked: keep the baked placement, so a drift
                // too slow to see in a frame adds up against it.
                Some(o) => kept.push(*o),
                None => {
                    urgent.push(vol_box(v));
                    kept.push(*v);
                }
            }
        }
        for o in &self.vols {
            if !vols.iter().any(|v| same_vol(o, v)) {
                urgent.push(vol_box(o));
            }
        }
        self.vols = kept;

        // Blobs, by slot, against what was baked.
        let bc = (g.params[1].max(0.0) as usize).min(MAX_BLOBS);
        let blob_r = |s: f32| {
            let s = s.max(0.02);
            (s + (0.3 * s).max(g.params[2])) as f64
        };
        let mut blobs: Vec<(DVec3, f32)> = Vec::with_capacity(bc);
        let seen: Vec<(DVec3, f32)> = g.blobs[..bc].iter().map(|b| (to_anchor(*b), b[3])).collect();
        for i in 0..bc.max(self.blobs.len()) {
            let now = seen.get(i).copied();
            let was = self.blobs.get(i).copied();
            let moved = match (now, was) {
                (Some(a), Some(b)) => (a.0 - b.0).abs().max_element() > STILL_TOLERANCE || (a.1 - b.1).abs() > 1e-3,
                (None, None) => false,
                _ => true,
            };
            if moved {
                for (c, s) in [now, was].into_iter().flatten() {
                    let r = DVec3::splat(blob_r(s));
                    urgent.push((c - r, c + r));
                }
            }
            if let Some(n) = now {
                blobs.push(if moved { n } else { was.unwrap_or(n) });
            }
        }
        self.blobs = blobs;

        for e in edits {
            urgent.push((to_anchor([e.min.x, e.min.y, e.min.z, 0.0]), to_anchor([e.max.x, e.max.y, e.max.z, 0.0])));
        }

        // Proxies: followed across the slots' re-sorting by where they are,
        // still against where they started holding still, and once baked,
        // against where they were baked.
        let pc = (g.prox_count[0].max(0.0) as usize).min(MAX_SHADOW_PROXIES);
        let mut poses: Vec<Pose> = Vec::with_capacity(pc);
        for i in 0..pc {
            let (a, b) = (g.prox_a[i], g.prox_b[i]);
            let kind = b[3].round() as u32;
            poses.push(Pose {
                kind,
                a: to_anchor(a),
                b: if kind == 1 { to_anchor(b) } else { DVec3::new(b[0] as f64, b[1] as f64, b[2] as f64) },
                size: a[3],
                rot: if kind == 2 { g.prox_rot[i] } else { [0.0, 0.0, 0.0, 1.0] },
            });
        }
        let mut matched = vec![false; self.proxies.len()];
        let mut now: Vec<Tracked> = Vec::with_capacity(pc);
        let mut bake_in: Vec<(u64, Pose)> = Vec::new();
        for p in poses {
            let hit = (0..self.proxies.len())
                .filter(|&j| !matched[j] && same_shape(&self.proxies[j].now, &p) && pose_gap(&self.proxies[j].now, &p) <= FOLLOW_TOLERANCE)
                .min_by(|&i, &j| pose_gap(&self.proxies[i].now, &p).total_cmp(&pose_gap(&self.proxies[j].now, &p)));
            let t = match hit {
                Some(j) => {
                    matched[j] = true;
                    let o = self.proxies[j];
                    let mut t = Tracked { now: p, ..o };
                    if still(&o.rest, &p) {
                        t.still = o.still.saturating_add(1);
                    } else {
                        t.still = 0;
                        t.rest = p;
                    }
                    if let Some(b) = o.baked
                        && !still(&b, &p)
                    {
                        // It has left where it was baked: bake it out.
                        urgent.push(proxy_box(&b));
                        t.baked = None;
                    }
                    t
                }
                None => {
                    self.next_id += 1;
                    Tracked { id: self.next_id, now: p, rest: p, still: 0, baked: None }
                }
            };
            let mut t = t;
            if t.baked.is_none() && t.still >= SKY_SETTLE_FRAMES {
                t.baked = Some(t.rest);
                bake_in.push((t.id, t.rest));
            }
            now.push(t);
        }
        for (j, o) in self.proxies.iter().enumerate() {
            if !matched[j]
                && let Some(b) = o.baked
            {
                // A baked proxy left the slots: bake it out.
                urgent.push(proxy_box(&b));
            }
        }
        if fresh {
            // A cache starting again refills everything anyway.
            urgent.clear();
        }
        let grow = DVec3::splat(feel);
        for (lo, hi) in unique(urgent) {
            self.queue(Job { lo: lo - grow, hi: hi + grow, levels: 0xF, prio: 5, cap: CAP_URGENT, bakes: Vec::new() });
        }
        for (id, pose) in bake_in {
            let (lo, hi) = proxy_box(&pose);
            self.queue(Job { lo: lo - grow, hi: hi + grow, levels: 0xF, prio: 10, cap: CAP_URGENT, bakes: vec![id] });
        }

        // Retire the boxes done, then start what the budget has room for,
        // in order of priority. A started box keeps its period and phase
        // until it is done.
        self.active.retain(|b| frame < b.start + b.period as u64);
        self.waiting.sort_by_key(|j| j.prio);
        let mut left = self.budget as f64 - self.active.iter().map(|b| b.rate).sum::<f64>();
        let mut k = 0;
        while k < self.waiting.len() && self.active.len() < SKY_MAX_BOXES && left >= 1.0 {
            let cells = self.job_cells(&self.waiting[k]);
            if cells < 0.5 {
                // Nothing of it is in any window now.
                self.waiting.remove(k);
                continue;
            }
            let period = (cells / left).ceil().max(1.0);
            if period > self.waiting[k].cap as f64 {
                // Too big for the room left: wait for more, and let the
                // smaller ones behind it in.
                k += 1;
                continue;
            }
            let period = period as u32;
            let rate = cells / period as f64;
            left -= rate;
            let job = self.waiting.remove(k);
            self.active.push(Active { job, start: frame, period, rate });
        }
        if self.active.is_empty()
            && let Some(job) = (!self.waiting.is_empty()).then(|| self.waiting.remove(0))
        {
            // Nothing refilling and the first box too big for its cap: it
            // goes anyway, over as many frames as the budget asks.
            let cells = self.job_cells(&job);
            let period = (cells / self.budget as f64).ceil().max(1.0) as u32;
            self.active.push(Active { rate: cells / period as f64, job, start: frame, period });
        }

        // Read per pixel: every proxy not baked in, and every one whose
        // bake-in box has not finished.
        let baking: Vec<u64> = self.active.iter().map(|b| &b.job).chain(self.waiting.iter()).flat_map(|j| j.bakes.iter().copied()).collect();
        let mut settled = 0u32;
        let mut moving = 0u32;
        for (i, t) in now.iter().enumerate() {
            if t.baked.is_some() {
                settled |= 1 << i;
            }
            if t.baked.is_none() || baking.contains(&t.id) {
                moving |= 1 << i;
            }
        }
        self.proxies = now;
        stats.settled = settled;
        stats.moving = moving;
        stats.boxes = self.active.len();
        stats.waiting = self.waiting.len();

        // The header.
        let mut h = vec![0u32; SKY_HEADER_WORDS];
        let f = |x: f32| x.to_bits();
        h[H_ON] = 1;
        h[H_REACH] = f(self.reach);
        h[H_SETTLED] = settled;
        h[H_MOVING] = moving;
        for (l, m) in mins.iter().enumerate() {
            let cell = level_cell(l);
            let o = H_LEVEL + l * H_LEVEL_WORDS;
            let mut low = 0u32;
            for a in 0..3 {
                let corner = view.anchor[a] + m[a] as f64 * cell as f64 - view.cam[a];
                h[o + a] = f(corner as f32);
                h[o + 4 + a] = m[a].rem_euclid(d) as u32;
                low |= ((m[a] & 1023) as u32) << (10 * a);
            }
            h[o + 3] = f(cell);
            h[o + 7] = low;
            h[o + 8] = self.epoch[l];
        }
        let mut due = [false; SKY_LEVELS];
        let mut cells = 0.0;
        h[H_BOXES] = self.active.len() as u32;
        for (k, b) in self.active.iter().enumerate() {
            let o = H_BOX + k * 8;
            for a in 0..3 {
                h[o + a] = f((b.job.lo[a] - rel[a]) as f32);
                h[o + 4 + a] = f((b.job.hi[a] - rel[a]) as f32);
            }
            h[o + 3] = b.period | ((b.job.levels as u32) << 24);
            h[o + 7] = ((frame - b.start) % b.period as u64) as u32;
            for (l, dl) in due.iter_mut().enumerate() {
                if b.job.levels & (1 << l) != 0 && self.level_cells(l, b.job.lo, b.job.hi) > 0.0 {
                    *dl = true;
                }
            }
            cells += b.rate;
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
        SkyFrame { header: h, fill: due.iter().any(|d| *d), cells: cells.round() as u32, due }
    }

    /// Forget level `l` in every box: its cells there are gone.
    fn drop_level(&mut self, l: usize) {
        let bit = !(1u8 << l);
        for j in &mut self.waiting {
            j.levels &= bit;
        }
        self.waiting.retain(|j| j.levels != 0);
        // A refilling box keeps its period; refilling a level's new cells
        // in it is harmless.
    }

    /// Queue a box. A waiting box of the same priority and levels that it
    /// overlaps takes it in (a waiting box has no progress to lose); past
    /// [`MAX_WAITING`], it merges into whichever waiting box grows least.
    fn queue(&mut self, job: Job) {
        let overlaps = |a: &Job| (0..3).all(|k| a.lo[k] <= job.hi[k] && job.lo[k] <= a.hi[k]);
        let pick = self.waiting.iter().position(|w| w.prio == job.prio && w.levels == job.levels && overlaps(w)).or_else(|| {
            (self.waiting.len() >= MAX_WAITING).then(|| {
                let vol = |l: DVec3, h: DVec3| (h - l).max(DVec3::ZERO).element_product();
                (0..self.waiting.len())
                    .min_by(|&i, &j| {
                        let g = |w: &Job| vol(w.lo.min(job.lo), w.hi.max(job.hi)) - vol(w.lo, w.hi);
                        g(&self.waiting[i]).total_cmp(&g(&self.waiting[j]))
                    })
                    .unwrap_or(0)
            })
        });
        match pick {
            Some(i) => {
                let w = &mut self.waiting[i];
                w.lo = w.lo.min(job.lo);
                w.hi = w.hi.max(job.hi);
                w.levels |= job.levels;
                w.prio = w.prio.min(job.prio);
                w.cap = w.cap.min(job.cap);
                w.bakes.extend(job.bakes);
            }
            None => self.waiting.push(job),
        }
    }

    fn job_cells(&self, j: &Job) -> f64 {
        (0..SKY_LEVELS).filter(|l| j.levels & (1 << l) != 0).map(|l| self.level_cells(l, j.lo, j.hi)).sum()
    }

    /// Cells of level `l`'s window inside an anchor-relative box.
    fn level_cells(&self, l: usize, lo: DVec3, hi: DVec3) -> f64 {
        let Some(mins) = self.mins else { return 0.0 };
        let cell = level_cell(l) as f64;
        let mut v = 1.0;
        for a in 0..3 {
            let w0 = mins[l][a] as f64 * cell;
            let w1 = w0 + SKY_DIMS as f64 * cell;
            v *= ((hi[a].min(w1) - lo[a].max(w0)) / cell).max(0.0);
        }
        v
    }
}

/// Level `l`'s cell size.
pub fn level_cell(l: usize) -> f32 {
    SKY_CELL0 * (1u32 << l) as f32
}

fn same_shape(a: &Pose, b: &Pose) -> bool {
    a.kind == b.kind && (a.size - b.size).abs() <= 1e-3 && (a.kind != 2 || (a.b - b.b).abs().max_element() <= 1e-3)
}

/// How far apart two poses' placements are, world units.
fn pose_gap(a: &Pose, b: &Pose) -> f64 {
    let g = (a.a - b.a).abs().max_element();
    if a.kind == 1 { g.max((a.b - b.b).abs().max_element()) } else { g }
}

/// Is `b` where `a` was: same shape, within [`STILL_TOLERANCE`], turned by
/// no more than about a quarter of a degree.
fn still(a: &Pose, b: &Pose) -> bool {
    same_shape(a, b) && pose_gap(a, b) <= STILL_TOLERANCE && {
        let d: f32 = a.rot.iter().zip(&b.rot).map(|(x, y)| x * y).sum();
        d.abs() >= 1.0 - 2e-6
    }
}

fn unique(boxes: Vec<(DVec3, DVec3)>) -> Vec<(DVec3, DVec3)> {
    let mut out: Vec<(DVec3, DVec3)> = Vec::with_capacity(boxes.len());
    for b in boxes {
        if !out.iter().any(|u| (u.0 - b.0).abs().max_element() <= STILL_TOLERANCE && (u.1 - b.1).abs().max_element() <= STILL_TOLERANCE) {
            out.push(b);
        }
    }
    out
}

/// An anchor-relative box round a tracked proxy.
fn proxy_box(p: &Pose) -> (DVec3, DVec3) {
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
impl SkyCache {
    /// The refilling boxes' (start, period), for tests.
    fn active_boxes(&self) -> Vec<(u64, u32)> {
        self.active.iter().map(|b| (b.start, b.period)).collect()
    }

    /// The longest period of a refilling box that is not a level starting
    /// again, for tests.
    fn longest_urgent(&self) -> u32 {
        self.active.iter().filter(|b| b.job.prio >= SKY_LEVELS as u32).map(|b| b.period).max().unwrap_or(0)
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

    /// Plan until nothing is refilling or waiting; the frames it took.
    fn fill_up(c: &mut SkyCache, cam: DVec3, g: &RaymarchGlobals) -> u32 {
        for k in 1..3000 {
            c.plan(&view(cam), g, &[], &[]);
            if c.stats().boxes == 0 && c.stats().waiting == 0 {
                return k;
            }
        }
        panic!("the cache never settles: {:?}", c.stats());
    }

    #[test]
    fn a_still_camera_refills_nothing_once_full() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::new(3.2, 4.0, -7.5);
        let g = globals(cam);
        let first = c.plan(&view(cam), &g, &[], &[]);
        assert!(c.stats().reset.iter().all(|r| *r), "{:?}", c.stats());
        assert!(first.fill && first.cells > 0);
        // The coarsest level goes first.
        assert_eq!(first.due, [false, false, false, true]);
        let frames = 1 + fill_up(&mut c, cam, &g) as i32;
        // A full refill of all four levels, at the budget, takes about
        // cells / budget frames.
        let want = (SKY_CELLS / SKY_BUDGET) as i32;
        assert!((frames - want).abs() <= want / 4 + 8, "a full fill took {frames} frames, about {want} expected");
        for _ in 0..40 {
            let f = c.plan(&view(cam), &g, &[], &[]);
            assert_eq!(f.cells, 0);
            assert!(!f.fill, "a still camera with nothing dirty runs no pass: {:?}", f.due);
        }
    }

    #[test]
    fn a_one_cell_step_scrolls_one_slab() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::new(0.5, 0.5, 0.5);
        fill_up(&mut c, cam, &globals(cam));
        // One finest cell along x: one slab of the finest level; the 2-unit
        // level's window stays (0.5 → 1.5 is inside one of its cells).
        let cam2 = cam + DVec3::X;
        let f = c.plan(&view(cam2), &globals(cam2), &[], &[]);
        let d2 = SKY_DIMS * SKY_DIMS;
        assert_eq!(c.stats().scrolled, [d2, 0, 0, 0]);
        assert!(!c.stats().reset.iter().any(|r| *r));
        // Only that slab refills, and only the finest level is visited.
        assert_eq!(f.cells, d2);
        assert_eq!(f.due, [true, false, false, false]);
        let cam3 = cam2 + DVec3::X;
        c.plan(&view(cam3), &globals(cam3), &[], &[]);
        assert_eq!(c.stats().scrolled, [d2, d2, 0, 0]);
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
        let f = c.plan(&v, &globals(far), &[], &[]);
        assert_eq!(f.header[H_LEVEL + 8], 3, "finest level started three times");
        assert_eq!(f.header[H_LEVEL + 3 * H_LEVEL_WORDS + 8], 2);
    }

    #[test]
    fn an_edit_refills_the_cells_that_can_feel_it() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 5.0, 0.0);
        let g = globals(cam);
        fill_up(&mut c, cam, &g);
        let edit = SkyEdit { min: Vec3::new(1.0, -6.0, 2.0), max: Vec3::new(3.0, -4.0, 3.0) };
        let f = c.plan(&view(cam), &g, &[], &[edit]);
        assert_eq!(f.header[H_BOXES], 1);
        let feel = c.feel() as f32;
        let at = |w: usize| f32::from_bits(f.header[H_BOX + w]);
        for a in 0..3 {
            assert!((at(a) - (edit.min[a] - feel)).abs() < 1e-3, "min {a}: {}", at(a));
            assert!((at(4 + a) - (edit.max[a] + feel)).abs() < 1e-3, "max {a}: {}", at(4 + a));
        }
        assert_eq!(f.header[H_BOX + 3] >> 24, 0xF, "on every level");
        let period = f.header[H_BOX + 3] & 0xFF_FFFF;
        assert!(period <= CAP_URGENT, "an edit is put right within a second: {period} frames");
        // The box is camera-relative: the camera moving moves it the other way.
        let cam2 = cam + DVec3::new(0.0, 0.0, 0.25);
        let f = c.plan(&view(cam2), &globals(cam2), &[], &[]);
        assert!((f32::from_bits(f.header[H_BOX + 2]) - (edit.min.z - feel - 0.25)).abs() < 1e-3);
        for _ in 0..period {
            c.plan(&view(cam2), &globals(cam2), &[], &[]);
        }
        assert_eq!(c.stats().boxes, 0, "retired once its dither has gone round once");
    }

    #[test]
    fn the_budget_holds_strictly_through_a_reset_and_a_storm_of_edits() {
        let mut c = SkyCache::new(32.0);
        let cam = DVec3::ZERO;
        let g = globals(cam);
        for k in 0..400 {
            let edits: Vec<SkyEdit> = (0..3)
                .map(|j| {
                    let p = Vec3::new(((k * 7 + j * 13) % 90) as f32 - 45.0, 0.0, ((k * 11 + j * 5) % 90) as f32 - 45.0);
                    SkyEdit { min: p, max: p + Vec3::ONE }
                })
                .collect();
            let f = c.plan(&view(cam), &g, &[], &edits);
            assert!(c.stats().boxes <= SKY_MAX_BOXES && c.stats().waiting <= MAX_WAITING);
            assert!(f.cells <= SKY_BUDGET, "frame {k}: {} cells", f.cells);
            // With the budget busy, an edit waits for room rather than start
            // over many seconds.
            assert!(c.longest_urgent() <= CAP_URGENT, "frame {k}: an edit refilling over {} frames", c.longest_urgent());
        }
    }

    #[test]
    fn a_refilling_box_is_never_restarted_or_merged() {
        // A proxy that rests, moves, rests: bake-ins and bake-outs keep
        // coming. Every box, once started, keeps its start and period until
        // it is done, and is done within its period.
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        put_box(&mut g, cam, 0, DVec3::new(5.0, 0.5, 0.0));
        fill_up(&mut c, cam, &g);
        let mut seen: Vec<(u64, u32)> = Vec::new();
        for k in 0..1500u32 {
            // Moves for 10 frames out of every 50.
            if k % 50 < 10 {
                let x = 5.0 + 0.1 * (k / 50 * 10 + k % 50) as f64;
                put_box(&mut g, cam, 0, DVec3::new(x, 0.5, 0.0));
            }
            c.plan(&view(cam), &g, &[], &[]);
            let now = c.active_boxes();
            for b in &seen {
                let alive = now.contains(b);
                let due = c.frame >= b.0 + b.1 as u64;
                assert_eq!(alive, !due, "box {b:?} at frame {}", c.frame);
            }
            seen = now;
            for b in &seen {
                assert!(b.1 <= CAP_CALM);
            }
        }
    }

    #[test]
    fn a_slow_drift_adds_up_and_is_caught() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        // Half a centimetre a frame, under the tolerance each frame: never
        // holds still long enough to be baked in.
        for k in 0..200 {
            put_box(&mut g, cam, 0, DVec3::new(5.0 + 0.005 * k as f64, 0.5, 0.0));
            c.plan(&view(cam), &g, &[], &[]);
            assert_eq!(c.stats().settled, 0, "frame {k}");
            assert_eq!(c.stats().moving, 1);
        }
        // Baked, then turning slowly: left as soon as it is a quarter of a
        // degree from where it was baked (eight frames of this turn).
        put_box(&mut g, cam, 0, DVec3::new(9.0, 0.5, 0.0));
        for _ in 0..SKY_SETTLE_FRAMES + 1 {
            c.plan(&view(cam), &g, &[], &[]);
        }
        fill_up(&mut c, cam, &g);
        assert_eq!(c.stats().settled, 1);
        let mut left = None;
        for k in 1..100 {
            let a = 0.0005f32 * k as f32; // half a milliradian a frame
            g.prox_rot[0] = [0.0, (a * 0.5).sin(), 0.0, (a * 0.5).cos()];
            c.plan(&view(cam), &g, &[], &[]);
            if c.stats().settled == 0 {
                left = Some(k);
                break;
            }
        }
        let k = left.expect("a slow turn never left the cache");
        assert!(k <= 10, "left after {k} frames");
        assert!(c.stats().boxes + c.stats().waiting >= 1, "its baked place is refilled");
    }

    #[test]
    fn a_volume_drifting_slowly_is_refilled_once_it_has_drifted() {
        let mut c = SkyCache::new(24.0);
        let mut cam = DVec3::new(0.0, 5.0, 0.0);
        let mut g = globals(cam);
        fill_up(&mut c, cam, &g);
        // The camera stands still, the volume drifts 2 mm a frame (the
        // globals are camera-relative, so moving the camera the other way
        // is the same).
        let mut hit = None;
        for k in 1..20 {
            cam.x -= 0.002;
            g = globals(cam);
            c.plan(&view(DVec3::new(0.0, 5.0, 0.0)), &g, &[], &[]);
            if c.stats().boxes + c.stats().waiting > 0 {
                hit = Some(k);
                break;
            }
        }
        let k = hit.expect("a slow drift never refilled anything");
        assert!((5..=7).contains(&k), "refilled after {k} frames");
    }

    #[test]
    fn a_proxy_is_read_per_pixel_until_its_bake_is_done() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        fill_up(&mut c, cam, &g);
        put_box(&mut g, cam, 0, DVec3::new(5.0, 0.5, 0.0));
        put_box(&mut g, cam, 1, DVec3::new(-6.0, 0.5, 3.0));
        let mut baked_at = None;
        for k in 0..2000u32 {
            c.plan(&view(cam), &g, &[], &[]);
            let st = c.stats();
            if st.settled == 0b11 && baked_at.is_none() {
                baked_at = Some(k);
            }
            if baked_at.is_some() && st.boxes == 0 && st.waiting == 0 {
                // Done: read from the cache alone.
                assert_eq!(st.moving, 0, "{st:?}");
                break;
            }
            // Until then, read per pixel too: never a frame with neither.
            assert_eq!(st.moving, 0b11, "frame {k}: {st:?}");
        }
        assert!(baked_at.is_some());
    }

    #[test]
    fn a_bake_out_is_put_right_within_a_second() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        put_box(&mut g, cam, 0, DVec3::new(5.0, 0.5, 0.0));
        fill_up(&mut c, cam, &g);
        assert_eq!(c.stats().settled, 1);
        g.prox_a[0][0] += 3.0;
        c.plan(&view(cam), &g, &[], &[]);
        let boxes = c.active_boxes();
        assert_eq!(boxes.len(), 1, "the bake-out started at once: {:?}", c.stats());
        assert!(boxes[0].1 <= CAP_URGENT, "over {} frames", boxes[0].1);
    }

    #[test]
    fn a_proxy_that_moves_leaves_the_cache_and_one_resorted_stays() {
        let mut c = SkyCache::new(24.0);
        let cam = DVec3::new(0.0, 2.0, 0.0);
        let mut g = globals(cam);
        put_box(&mut g, cam, 0, DVec3::new(5.0, 0.5, 0.0));
        put_box(&mut g, cam, 1, DVec3::new(-5.0, 0.5, 3.0));
        fill_up(&mut c, cam, &g);
        assert_eq!(c.stats().settled, 0b11);
        assert_eq!(c.stats().moving, 0);
        // Re-sorted: the same two proxies in each other's slots.
        let mut swapped = g;
        swapped.prox_a.swap(0, 1);
        swapped.prox_b.swap(0, 1);
        c.plan(&view(cam), &swapped, &[], &[]);
        assert_eq!(c.stats().settled, 0b11);
        assert_eq!(c.stats().boxes + c.stats().waiting, 0, "a re-sort dirtied nothing");
        let mut moved = swapped;
        moved.prox_a[1][0] += 0.05;
        let f = c.plan(&view(cam), &moved, &[], &[]);
        assert_eq!(c.stats().settled, 0b01, "the moved one left the cache");
        assert_eq!(c.stats().moving, 0b10, "and is read per pixel");
        assert_eq!(c.stats().boxes, 1, "its old place is refilled");
        let lo = f32::from_bits(f.header[H_BOX]);
        let hi = f32::from_bits(f.header[H_BOX + 4]);
        let x = 5.0 - cam.x as f32;
        assert!(lo < x && x < hi, "{lo}..{hi} misses {x}");
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
    fn a_world_moving_along_its_orbit_refills_nothing() {
        let mut c = SkyCache::new(32.0);
        let mut anchor = DVec3::new(1.0e6, 0.0, 0.0);
        for k in 0..400 {
            anchor += DVec3::new(30.0, 0.0, 12.0);
            let cam = anchor + DVec3::new(0.3, 4.0, -2.0);
            let mut g = RaymarchGlobals::default();
            let t = (anchor + DVec3::new(0.0, -10.0, 0.0) - cam).as_vec3();
            g.vol_center[0] = [t.x, t.y, t.z, 3.0];
            g.vol_half[0] = [100.0, 20.0, 100.0, 0.6];
            let f = c.plan(&SkyView { cam, anchor, anchor_id: 7 }, &g, &[1], &[]);
            if k > 250 {
                assert_eq!(f.cells, 0, "frame {k}: {:?}", c.stats());
                assert!(!f.fill);
            }
        }
    }

    #[test]
    fn field_shapes_switch_the_cache_off_and_back_from_scratch() {
        let mut c = SkyCache::new(32.0);
        let mut g = globals(DVec3::ZERO);
        fill_up(&mut c, DVec3::ZERO, &g);
        g.shape_meta[0] = 1.0;
        let f = c.plan(&view(DVec3::ZERO), &g, &[], &[]);
        assert_eq!(f.header[H_ON], 0);
        assert!(!f.fill);
        g.shape_meta[0] = 0.0;
        c.plan(&view(DVec3::ZERO), &g, &[], &[]);
        assert_eq!(c.stats().reset, [true; SKY_LEVELS], "back on, it starts again");
    }

    #[test]
    fn a_volume_whose_key_changes_is_refilled() {
        let mut c = SkyCache::new(32.0);
        let g = globals(DVec3::ZERO);
        for _ in 0..3 {
            c.plan(&view(DVec3::ZERO), &g, &[5], &[]);
        }
        fill_up_keys(&mut c, &g, 5);
        assert_eq!(c.stats().boxes + c.stats().waiting, 0);
        c.plan(&view(DVec3::ZERO), &g, &[6], &[]);
        assert_eq!(c.stats().boxes + c.stats().waiting, 1);
    }

    fn fill_up_keys(c: &mut SkyCache, g: &RaymarchGlobals, key: u64) {
        for _ in 0..3000 {
            c.plan(&view(DVec3::ZERO), g, &[key], &[]);
            if c.stats().boxes == 0 && c.stats().waiting == 0 {
                return;
            }
        }
        panic!("never settles");
    }

    #[test]
    fn a_changed_blend_starts_again() {
        let mut c = SkyCache::new(32.0);
        let mut g = globals(DVec3::ZERO);
        fill_up(&mut c, DVec3::ZERO, &g);
        g.params[2] = 0.4;
        c.plan(&view(DVec3::ZERO), &g, &[], &[]);
        assert_eq!(c.stats().reset, [true; SKY_LEVELS]);
    }
}
