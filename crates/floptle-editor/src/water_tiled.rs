//! The editor's side of tiled simulated water (`water.addVolume{ shape =
//! "sim", center = ... }`): the ground each tile stands on, read from the
//! terrain's field along each cell's own up; the tiles water reaches made as
//! it reaches them; the step, kept inside a budget so a flood slows the water
//! rather than the frame; and what the water has done handed back to the
//! scripts and the solver.

use floptle_core::math::DVec3;
use floptle_core::water::{tile_key, Patch, Surface, TILE};
use floptle_field::water_tiles::{Solver, TiledWater};

/// A tiled sim the editor steps.
pub(crate) struct TiledSim {
    pub water: TiledWater,
    /// Tiles still to be made before the water starts moving, when it was
    /// declared filled or with depths.
    seeding: Vec<u32>,
    /// Held cells waiting for their tile: column, row, level, group, and
    /// whether to hold it only where its ground is below the level (the
    /// patch's edge, `holdEdges`).
    pending_holds: Vec<(usize, usize, f32, u16, bool)>,
    /// Water added to points whose tile is not made yet: `(i, j, m³)`.
    pending_water: Vec<(usize, usize, f64)>,
    /// The level edit the ground was last read against.
    seen_edits: u64,
    /// The level it was last held at (`water.setLevel`).
    level: f32,
}

/// The longest a frame spends reading ground for new tiles, ms.
const GROUND_MS: f32 = 2.0;
/// The most steps a frame takes for one sim.
const SUBSTEPS: usize = 8;

/// What a tile's ground is read from.
enum Ground<'a> {
    /// The terrain's field, in its node's frame, at this scale.
    Field(&'a floptle_field::ChunkField, f32),
    /// Collision, cast down each cell's up.
    Colliders(&'a [floptle_physics::AnchoredCollider], floptle_core::scatter::Frame, DVec3),
}

impl Ground<'_> {
    /// The ground's height above the patch's reference at cell `(i, j)`,
    /// searched for from `guess`; infinite where there is none (a cell over
    /// a drop deeper than the search, or ground not generated yet).
    fn at(&self, p: &Patch, i: usize, j: usize, guess: f32) -> f32 {
        let (gx, gz) = (i as f64, j as f64);
        match self {
            Ground::Field(field, scale) => {
                let mut out = [0.0];
                self.field_batch(field, *scale, p, &[(i, j)], guess, &mut out);
                out[0]
            }
            Ground::Colliders(cols, frame, origin) => {
                let lift = 60.0;
                let from = frame.to_world(p.point(gx, gz, guess as f64 + lift));
                let up = frame.dir_to_world(p.up_at(gx, gz).as_vec3());
                floptle_physics::raycast_ground_colliders(cols, (from - *origin).as_vec3(), -up, (lift * 2.0) as f32)
                    .map_or(f32::INFINITY, |hit| guess + lift as f32 - hit.distance)
            }
        }
    }

    /// The ground at `cells` from the terrain's field, in one batch
    /// ([`floptle_field::ChunkField::surface_below`]), each searched for down
    /// its own up from just above `guess`.
    fn field_batch(&self, field: &floptle_field::ChunkField, scale: f32, p: &Patch, cells: &[(usize, usize)], guess: f32, out: &mut [f32]) {
        let s = scale.max(1e-6) as f64;
        let lift = 2.0 * field.voxel() as f64 * s;
        let from: Vec<DVec3> = cells.iter().map(|&(i, j)| p.point(i as f64, j as f64, guess as f64 + lift) / s).collect();
        let down: Vec<DVec3> = cells.iter().map(|&(i, j)| -p.up_at(i as f64, j as f64)).collect();
        let mut t = vec![0.0; cells.len()];
        let mut flags = vec![0u8; cells.len()];
        field.surface_below(&from, floptle_field::surface::Dirs::Each(&down), 400.0 / s, &mut t, Some(&mut flags));
        for k in 0..cells.len() {
            // Ground not generated yet is a stand-in: no ground to stand
            // water on until it is.
            out[k] = if t[k].is_finite() && flags[k] == 0 { (guess as f64 + lift - t[k] * s) as f32 } else { f32::INFINITY };
        }
    }

    /// Tile `(ti, tj)`'s ground, a row at a time; `guess` the height to
    /// search from.
    fn tile(&self, p: &Patch, ti: usize, tj: usize, guess: f32) -> Vec<f32> {
        if let Ground::Field(field, scale) = self {
            // The tile's middle first, then every point from there: the
            // ground across a tile is nearly always within a few voxels of it.
            let mid = ((ti * TILE + TILE / 2).min(p.columns - 1), (tj * TILE + TILE / 2).min(p.rows - 1));
            let mut m = [0.0];
            self.field_batch(field, *scale, p, &[mid], guess, &mut m);
            let near = if m[0].is_finite() { m[0] } else { guess };
            let cells: Vec<(usize, usize)> = (0..TILE * TILE).map(|c| (ti * TILE + c % TILE, tj * TILE + c / TILE)).collect();
            let inside: Vec<(usize, usize)> = cells.iter().copied().filter(|&(i, j)| i < p.columns && j < p.rows).collect();
            let mut got = vec![f32::INFINITY; inside.len()];
            self.field_batch(field, *scale, p, &inside, near, &mut got);
            let mut out = vec![f32::INFINITY; TILE * TILE];
            for (&(i, j), g) in inside.iter().zip(got) {
                out[(j - tj * TILE) * TILE + (i - ti * TILE)] = g;
            }
            return out;
        }
        let mut out = vec![f32::INFINITY; TILE * TILE];
        let mut last = guess;
        for y in 0..TILE {
            let mut row_start = None;
            for x in 0..TILE {
                let (i, j) = (ti * TILE + x, tj * TILE + y);
                if i >= p.columns || j >= p.rows {
                    continue;
                }
                let g = self.at(p, i, j, last);
                // A miss from a neighbour's height is tried again from the
                // level the water was declared at.
                let g = if g.is_finite() || last == guess { g } else { self.at(p, i, j, guess) };
                if g.is_finite() {
                    last = g;
                    row_start.get_or_insert(g);
                }
                out[y * TILE + x] = g;
            }
            if let Some(g) = row_start {
                last = g;
            }
        }
        out
    }
}

impl crate::Editor {
    /// Step every tiled sim one frame: make the tiles its water has reached,
    /// read the ground again where it was dug, step inside the budget, and
    /// hand back what it reached. Returns `(tiles awake, cells solved,
    /// tiles)` summed.
    pub(crate) fn step_tiled_sims(&mut self, dt: f32) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        let Some(sim) = self.sim.as_ref() else {
            self.water_sims.tiled.clear();
            return counts;
        };
        let declared: Vec<floptle_core::water::DeclaredWater> = self
            .script_host
            .declared_water()
            .iter()
            .filter(|w| w.simulated.as_ref().is_some_and(|s| s.tiled.is_some()))
            .cloned()
            .collect();
        self.water_sims.tiled.retain(|id, _| declared.iter().any(|w| w.id == *id));
        let edits = sim.world.level_edits;
        let origin = sim.world.origin;
        let lent = self.script_host.lent_colliders();
        let cols: &[floptle_physics::AnchoredCollider] = if sim.world.colliders.is_empty() { &lent } else { &sim.world.colliders };
        let cmds = self.script_host.take_water_sim_cmds();
        for w in &declared {
            let Some(spec) = &w.simulated else { continue };
            let Some(t) = &spec.tiled else { continue };
            let p = t.patch;
            let base = p.base() as f32;
            let terrain = w
                .anchor
                .as_ref()
                .and_then(|_| self.water_sims.anchors.iter().find(|(id, _)| *id == w.id).map(|(_, e)| *e))
                .and_then(|e| {
                    let scale = floptle_core::world_transform(&self.world, e).scale.max_element();
                    self.terrains.get(&e).map(|t| (&t.field, scale))
                });
            let ground = match terrain {
                Some((f, s)) => Ground::Field(f, s),
                None => Ground::Colliders(cols, w.frame, origin),
            };
            let s = self.water_sims.tiled.entry(w.id).or_insert_with(|| {
                let mut water = TiledWater::new(p, if t.pipes { Solver::Pipes } else { Solver::Swe });
                water.roughness = spec.roughness;
                water.min_depth = t.min_depth;
                water.soak = t.soak;
                water.soak_below = t.soak_below;
                water.still_depth = t.still_depth;
                for &(cell, q) in &spec.springs {
                    water.set_spring(cell % p.columns, cell / p.columns, q);
                }
                let (nx, nz) = p.tiles();
                let seeding = if t.fill {
                    (0..nz).flat_map(|tj| (0..nx).map(move |ti| tile_key(ti, tj))).collect()
                } else if let Some(st) = &t.state {
                    let mut keys: Vec<u32> = st.iter().map(|&(i, j, _)| tile_key(i as usize / TILE, j as usize / TILE)).collect();
                    keys.sort_unstable();
                    keys.dedup();
                    keys
                } else if let Some(d) = &t.depths {
                    let mut keys: Vec<u32> = (0..d.len())
                        .filter(|&c| d[c] > 0.0)
                        .map(|c| tile_key((c % p.columns) / TILE, (c / p.columns) / TILE))
                        .collect();
                    keys.sort_unstable();
                    keys.dedup();
                    keys
                } else {
                    Vec::new()
                };
                let mut pending_holds: Vec<(usize, usize, f32, u16, bool)> =
                    t.held.iter().map(|h| (h.i as usize, h.j as usize, h.level - base, h.group, false)).collect();
                // The sea or the lake beyond the patch: its edge held at the
                // level wherever the ground there is under it.
                if t.hold_edges {
                    for j in 0..p.rows {
                        for i in 0..p.columns {
                            if i == 0 || j == 0 || i + 1 == p.columns || j + 1 == p.rows {
                                pending_holds.push((i, j, spec.level - base, 0, true));
                            }
                        }
                    }
                }
                TiledSim { water, seeding, pending_holds, pending_water: Vec::new(), seen_edits: edits, level: spec.level }
            });
            // Scripts' holds, releases and water added this frame.
            for c in cmds.iter().filter(|c| c.id() == w.id) {
                match c {
                    floptle_core::water::SimCmd::Hold { cells, level, group, .. } => {
                        for &[i, j] in cells {
                            s.pending_holds.push((i as usize, j as usize, level - base, *group, false));
                        }
                    }
                    floptle_core::water::SimCmd::Release { cells, .. } => {
                        for &[i, j] in cells {
                            s.pending_holds.retain(|h| (h.0, h.1) != (i as usize, j as usize));
                            s.water.hold(i as usize, j as usize, f32::NAN, 0);
                        }
                    }
                    floptle_core::water::SimCmd::AddWater { cell: [i, j], volume, .. } => {
                        s.pending_water.push((*i as usize, *j as usize, *volume));
                    }
                    floptle_core::water::SimCmd::Spring { cell: [i, j], rate, .. } => {
                        s.water.set_spring(*i as usize, *j as usize, *rate);
                    }
                }
            }
            if spec.level != s.level {
                s.level = spec.level;
                s.water.hold_all_at(spec.level - base);
                for h in &mut s.pending_holds {
                    h.2 = spec.level - base;
                }
            }
            // Ground for the tiles the water needs, inside the frame's share.
            let began = floptle_core::time::Instant::now();
            let fill = t.fill.then_some(spec.level - base);
            let guess = spec.level - base;
            loop {
                if began.elapsed().as_secs_f32() * 1000.0 > GROUND_MS {
                    break;
                }
                let (key, seed) = match s.seeding.pop() {
                    Some(k) => (k, true),
                    None => match s.water.take_wanted().first() {
                        Some(k) => (*k, false),
                        None => break,
                    },
                };
                let (ti, tj) = ((key & 0xffff) as usize, (key >> 16) as usize);
                let g = ground.tile(&p, ti, tj, guess);
                let surface: Option<Vec<f32>> = if !seed {
                    None
                } else if let Some(l) = fill {
                    Some(vec![l; TILE * TILE])
                } else if let Some(st) = &t.state {
                    let mut sf = vec![f32::NAN; TILE * TILE];
                    for &(i, j, d) in st.iter().filter(|(i, j, _)| (*i as usize / TILE, *j as usize / TILE) == (ti, tj)) {
                        let c = (j as usize % TILE) * TILE + i as usize % TILE;
                        sf[c] = g[c] + d;
                    }
                    Some(sf)
                } else {
                    t.depths.as_ref().map(|d| {
                        (0..TILE * TILE)
                            .map(|c| {
                                let (i, j) = (ti * TILE + c % TILE, tj * TILE + c / TILE);
                                if i < p.columns && j < p.rows { g[c] + d[j * p.columns + i] } else { f32::NAN }
                            })
                            .collect()
                    })
                };
                // Filled to a level, a tile all above it is dry ground and
                // is not kept, unless something is held in it.
                let wet = surface.as_ref().is_none_or(|sf| sf.iter().zip(&g).any(|(l, g)| l > g));
                let holds = s.pending_holds.iter().any(|h| TiledWater::tile_of(h.0, h.1) == (ti, tj));
                if wet || holds {
                    s.water.add_tile(ti, tj, g, surface.as_deref());
                }
            }
            // Water added where the tile is not made yet waits for it.
            s.pending_water.retain(|&(i, j, v)| s.water.add_water(i, j, v) == 0.0 && !s.water.has_tile(i / TILE, j / TILE));
            s.pending_holds.retain(|&(i, j, level, group, under)| {
                if under && s.water.cell(i, j).is_some_and(|(g, _)| g >= level) {
                    return false;
                }
                !s.water.hold(i, j, level, group)
            });
            // Ground dug or filled since: read again where it changed.
            if edits != s.seen_edits {
                let seen = std::mem::replace(&mut s.seen_edits, edits);
                let log = &sim.world.level_edit_log;
                let everywhere = log.front().is_none_or(|(first, _)| *first > seen + 1)
                    || log.iter().any(|(q, r)| *q > seen && r.is_none());
                let regions: Vec<(DVec3, f64)> = log.iter().filter(|(q, _)| *q > seen).filter_map(|(_, r)| *r).collect();
                let tiles: Vec<(usize, usize)> = s.water.tile_coords().collect();
                for (ti, tj) in tiles {
                    for c in 0..TILE * TILE {
                        let (i, j) = (ti * TILE + c % TILE, tj * TILE + c / TILE);
                        if i >= p.columns || j >= p.rows {
                            continue;
                        }
                        let Some((old, _)) = s.water.cell(i, j) else { continue };
                        let at = w.frame.to_world(p.point(i as f64, j as f64, if old.is_finite() { old as f64 } else { guess as f64 }));
                        if everywhere || regions.iter().any(|(centre, r)| (at - *centre).length() < r + p.cell * 1.5) {
                            let g = ground.at(&p, i, j, if old.is_finite() { old + 4.0 } else { guess });
                            s.water.set_ground(i, j, g);
                        }
                    }
                }
            }
            // Water waits for its ground before it moves.
            if s.seeding.is_empty() && dt > 0.0 {
                let began = floptle_core::time::Instant::now();
                let owed = dt;
                let step = s.water.stable_dt().max(1e-3);
                let n = ((owed / step).ceil() as usize).clamp(1, SUBSTEPS);
                let sub = (owed / n as f32).min(step);
                for _ in 0..n {
                    s.water.step(sub);
                    if began.elapsed().as_secs_f32() * 1000.0 > t.budget_ms {
                        break;
                    }
                }
                // What the budget did not run is dropped, not owed: the
                // water runs slow for a moment rather than catching up.
            }
            counts.0 += s.water.awake_tiles();
            counts.1 += s.water.solved_count();
            counts.2 += s.water.tile_count();
            let surface = s.water.surface();
            let exchanged = s.water.take_exchanged();
            let filled = s.water.take_filled();
            let falls: Vec<floptle_core::water::WaterFall> = s
                .water
                .falls(1.5)
                .into_iter()
                .map(|(i, j, face, discharge, drop, width)| {
                    let step = [(0.5, 0.0), (-0.5, 0.0), (0.0, 0.5), (0.0, -0.5)][face as usize];
                    let (gx, gz) = (i as f64 + step.0, j as f64 + step.1);
                    let (a, b) = p.along(p.up_at(gx, gz));
                    let level = s.water.cell(i, j).map_or(0.0, |(g, d)| (g + d) as f64);
                    let dir = (a * step.0 * 2.0 + b * step.1 * 2.0).as_vec3();
                    floptle_core::water::WaterFall {
                        pos: w.frame.to_world(p.point(gx, gz, level)),
                        dir: w.frame.dir_to_world(dir),
                        width,
                        discharge,
                        drop,
                    }
                })
                .collect();
            if falls != spec.falls {
                self.script_host.set_water_falls(w.id, falls);
            }
            if !exchanged.is_empty() {
                self.script_host.add_water_exchanged(w.id, &exchanged);
            }
            if !filled.is_empty() {
                self.script_host.add_water_filled(w.id, &filled);
            }
            self.script_host.set_water_surface(w.id, Surface::Tiles(std::sync::Arc::new(surface)));
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::Vec3;

    /// **A tile's ground is read from the terrain's field along each
    /// point's up**, a few samples a point, and finds a dug hole: flat ground
    /// at 2 m reads 2 m, and a pit lowered into it reads lower there and
    /// nowhere else.
    #[test]
    fn a_tiles_ground_comes_from_the_field_along_each_up() {
        let mut f = floptle_field::ChunkField::new(0.5);
        f.fill_slab(Vec3::new(-40.0, -10.0, -40.0), Vec3::new(40.0, 4.0, 40.0), 2.0, [0.5; 3]);
        let p = Patch::flat(DVec3::ZERO, 1.0, 32, 32);
        let flat = Ground::Field(&f, 1.0).tile(&p, 0, 0, 0.0);
        assert!(flat.iter().all(|g| (g - 2.0).abs() < 0.05), "flat ground reads {:?}", &flat[..8]);
        for _ in 0..6 {
            f.sculpt(floptle_field::Brush::Lower, Vec3::new(5.0, 2.0, 5.0), 3.0, 1.0, floptle_field::BrushProfile::default());
        }
        let dug = Ground::Field(&f, 1.0).tile(&p, 0, 0, 0.0);
        // Grid point (i, j) is at x = i - 15.5, z = j - 15.5.
        let at = |x: f64, z: f64| dug[((z + 15.5) as usize) * TILE + (x + 15.5) as usize];
        assert!(at(4.5, 4.5) < 1.5, "the pit reads {}", at(4.5, 4.5));
        assert!((at(-10.5, -10.5) - 2.0).abs() < 0.05, "ground far from the pit moved to {}", at(-10.5, -10.5));
    }
}
