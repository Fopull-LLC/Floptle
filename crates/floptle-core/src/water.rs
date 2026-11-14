//! Water a game declares from a script: a lake that fits its basin, a
//! heightfield of levels, a river with a current.
//!
//! A Water Volume node is a sphere sea or a box pool. Lakes on a round world
//! are neither: their surface is part of a sphere about the planet's centre,
//! and only inside the basin. These shapes are declared in the frame of the
//! body they lie on, so a lake turns with its planet, and every system that
//! asks how deep a point is (the solver that floats a crate, the script that
//! decides a crew is swimming) asks the same function here.

use std::sync::Arc;

use glam::{DVec3, Vec3};

use crate::scatter::Frame;

/// The surface of a declared body of water, in its anchor's frame.
#[derive(Clone, Debug, PartialEq)]
pub enum Surface {
    /// A lake on a round world: the sphere of `radius` about the frame's
    /// origin (the planet's centre), wherever it lies within `reach` metres of
    /// `axis` along the surface. `terrain.planetLakes` gives exactly this: a
    /// point on the lake's surface and how far its farthest shore is.
    Lake { radius: f64, axis: DVec3, reach: f64 },
    /// A grid of surface heights over the frame's x–z plane, its +Y up.
    Grid(Arc<HeightGrid>),
    /// Simulated water stored only where it is: square tiles of cells laid
    /// on a patch of a round world or of a plane, each tile's levels shared
    /// until it changes. What a `sim` declared with `center` reads from.
    Tiles(Arc<TiledSurface>),
}

/// Surface heights on a regular grid, and optionally the water's velocity at
/// each point. A level that is not finite is dry ground.
#[derive(Clone, Debug, PartialEq)]
pub struct HeightGrid {
    /// The local x and z of the first point.
    pub x0: f64,
    pub z0: f64,
    /// Metres between points.
    pub cell: f64,
    pub columns: usize,
    pub rows: usize,
    /// `columns × rows` surface heights, a row of x at a time.
    pub levels: Vec<f32>,
    /// The water's local x–z velocity at each point, m/s, or empty for still
    /// water.
    pub flow: Vec<[f32; 2]>,
}

impl HeightGrid {
    /// The surface height and the flow at local `(x, z)`, blended from the
    /// wet points of the cell it falls in; `None` over dry ground or off the
    /// grid. Blending only the wet corners keeps a shore cell at the water's
    /// own level rather than dragged towards a dry neighbour's.
    pub fn sample(&self, x: f64, z: f64) -> Option<(f64, [f64; 2])> {
        if self.columns == 0 || self.rows == 0 || self.cell <= 0.0 {
            return None;
        }
        let gx = (x - self.x0) / self.cell;
        let gz = (z - self.z0) / self.cell;
        let (maxx, maxz) = ((self.columns - 1) as f64, (self.rows - 1) as f64);
        if !(0.0..=maxx).contains(&gx) || !(0.0..=maxz).contains(&gz) {
            return None;
        }
        let (ix, iz) = ((gx.floor() as usize).min(self.columns.saturating_sub(2)), (gz.floor() as usize).min(self.rows.saturating_sub(2)));
        let (fx, fz) = (gx - ix as f64, gz - iz as f64);
        let (mut level, mut flow, mut weight) = (0.0, [0.0; 2], 0.0);
        for (dx, dz, w) in [(0, 0, (1.0 - fx) * (1.0 - fz)), (1, 0, fx * (1.0 - fz)), (0, 1, (1.0 - fx) * fz), (1, 1, fx * fz)] {
            let (cx, cz) = ((ix + dx).min(self.columns - 1), (iz + dz).min(self.rows - 1));
            let i = cz * self.columns + cx;
            let Some(&l) = self.levels.get(i) else { continue };
            // A corner the point is not near at all has no say: at the last
            // wet row, the dry row beyond must not lend it its level.
            if !l.is_finite() || w <= 0.0 {
                continue;
            }
            level += l as f64 * w;
            if let Some(f) = self.flow.get(i) {
                flow[0] += f[0] as f64 * w;
                flow[1] += f[1] as f64 * w;
            }
            weight += w;
        }
        (weight > 0.0).then(|| (level / weight, [flow[0] / weight, flow[1] / weight]))
    }
}

/// What the engine's simulation of a grid knows at each of its points beyond
/// the level and the flow: the ground under the water, and how broken the
/// surface is. `water.surface` turns these into per-vertex colours, so a
/// water shader has depth and foam without a pass over the grid in Lua.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimReport {
    /// The bed's height at each point, in the grid's frame: the surface
    /// level less this is the water's depth, and a dry point lies on it.
    pub ground: Vec<f32>,
    /// 0 (glassy) to 1 (white water) at each point: [`foam`], and 1 where
    /// the water pours off a drop.
    pub foam: Vec<f32>,
}

/// How broken the surface of water `depth` metres deep is when it runs at
/// `speed` m/s, 0 to 1. It is the flow's Froude number, speed over the speed
/// of a wave in water that deep: under about 0.6 a river runs smooth, and
/// past 1 (faster than its own waves) it is white water. A film too thin to
/// carry foam has none, so a drying bank does not sparkle.
pub fn foam(depth: f32, speed: f32) -> f32 {
    if depth.is_nan() || depth <= 0.0 {
        return 0.0;
    }
    let froude = speed / (9.81 * depth).sqrt();
    let smooth = |lo: f32, hi: f32, x: f32| {
        let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    };
    smooth(0.6, 1.4, froude) * smooth(0.02, 0.10, depth)
}

/// Cells a side of one tile of a [`TiledSurface`].
pub const TILE: usize = 32;

/// How the cells of a tiled water patch lie in its anchor's frame.
///
/// Cell `(i, j)` has its centre at grid coordinates `(i, j)`; the patch's
/// middle, `centre`, is at `((columns - 1) / 2, (rows - 1) / 2)`, and a
/// point's height is measured along its own up.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Patch {
    /// On a round world, the radius heights are measured from: a height is
    /// the distance from the frame's origin (the world's centre) less this.
    /// `None` for a flat patch, whose heights are along `up` from `centre`'s
    /// plane through the frame's origin.
    pub radius: Option<f64>,
    /// Out of the ground at the patch's middle, and the two directions its
    /// columns and rows run along, all unit and at right angles.
    pub up: DVec3,
    pub e1: DVec3,
    pub e2: DVec3,
    /// The middle of the patch, on the reference surface: on a round world
    /// `up * radius`.
    pub centre: DVec3,
    /// Metres between cell centres at the middle.
    pub cell: f64,
    pub columns: usize,
    pub rows: usize,
}

impl Patch {
    /// A patch on a round world, about the point `on` of its surface (in the
    /// world's frame, whose origin is its centre).
    pub fn sphere(on: DVec3, cell: f64, columns: usize, rows: usize) -> Self {
        let up = on.try_normalize().unwrap_or(DVec3::Y);
        let (e1, e2) = Self::axes(up);
        Self { radius: Some(on.length().max(1e-6)), up, e1, e2, centre: up * on.length().max(1e-6), cell, columns, rows }
    }

    /// A patch on a plane, about `centre`, its up the frame's +Y.
    pub fn flat(centre: DVec3, cell: f64, columns: usize, rows: usize) -> Self {
        let centre = DVec3::new(centre.x, 0.0, centre.z);
        Self { radius: None, up: DVec3::Y, e1: DVec3::X, e2: DVec3::Z, centre, cell, columns, rows }
    }

    /// Columns along the frame's x and rows along its z wherever that is
    /// possible, so a patch at a pole lies as a flat grid would.
    fn axes(up: DVec3) -> (DVec3, DVec3) {
        let z = DVec3::Z - up * up.z;
        let e2 = if z.length_squared() > 1e-6 { z.normalize() } else { (DVec3::X - up * up.x).normalize() };
        // So that e1 × e2 points down, as +X × +Z does against +Y: a patch
        // at the +Y pole runs its columns along +X.
        let e1 = up.cross(e2).normalize();
        (e1, e2)
    }

    /// Metres along the patch's tangent plane of grid point `(gx, gz)`.
    fn tangent(&self, gx: f64, gz: f64) -> (f64, f64) {
        let mid = |n: usize| (n.max(1) - 1) as f64 * 0.5;
        ((gx - mid(self.columns)) * self.cell, (gz - mid(self.rows)) * self.cell)
    }

    /// The reference height a level is measured from, as the level a
    /// script reads: a radius on a round world, 0 on a plane.
    pub fn base(&self) -> f64 {
        self.radius.unwrap_or(0.0)
    }

    /// Up at grid point `(gx, gz)`.
    pub fn up_at(&self, gx: f64, gz: f64) -> DVec3 {
        let (x, z) = self.tangent(gx, gz);
        match self.radius {
            Some(_) => (self.centre + self.e1 * x + self.e2 * z).normalize(),
            None => self.up,
        }
    }

    /// The point at grid point `(gx, gz)`, `height` above the reference.
    pub fn point(&self, gx: f64, gz: f64, height: f64) -> DVec3 {
        let (x, z) = self.tangent(gx, gz);
        match self.radius {
            Some(r) => (self.centre + self.e1 * x + self.e2 * z).normalize() * (r + height),
            None => self.centre + self.e1 * x + self.e2 * z + self.up * height,
        }
    }

    /// Where frame point `p` is on the patch: its grid coordinates, its
    /// height above the reference and the up there. `None` on the far side
    /// of a round world.
    pub fn locate(&self, p: DVec3) -> Option<(f64, f64, f64, DVec3)> {
        let mid = |n: usize| (n.max(1) - 1) as f64 * 0.5;
        match self.radius {
            Some(r) => {
                let len = p.length();
                let dir = p.try_normalize()?;
                let c = dir.dot(self.up);
                if c < 1e-3 {
                    return None;
                }
                // Onto the tangent plane along the radial.
                let t = dir * (r / c) - self.centre;
                Some((t.dot(self.e1) / self.cell + mid(self.columns), t.dot(self.e2) / self.cell + mid(self.rows), len - r, dir))
            }
            None => {
                let t = p - self.centre;
                Some((t.dot(self.e1) / self.cell + mid(self.columns), t.dot(self.e2) / self.cell + mid(self.rows), t.dot(self.up), self.up))
            }
        }
    }

    /// The directions the grid's columns and rows run at `up`, along the
    /// ground there: what a cell's flow is measured along.
    pub fn along(&self, up: DVec3) -> (DVec3, DVec3) {
        let flat = |e: DVec3| (e - up * e.dot(up)).try_normalize().unwrap_or(e);
        (flat(self.e1), flat(self.e2))
    }

    /// Tiles across and down.
    pub fn tiles(&self) -> (usize, usize) {
        (self.columns.div_ceil(TILE), self.rows.div_ceil(TILE))
    }
}

/// One tile's water, as the simulation last left it: `TILE × TILE` cells, a
/// row of columns at a time.
#[derive(Clone, Debug, PartialEq)]
pub struct TileLevels {
    /// The surface's height above the patch's reference; not finite where
    /// the cell is dry.
    pub level: Vec<f32>,
    /// Metres of water standing in each cell.
    pub depth: Vec<f32>,
    /// The water's velocity along the patch's columns and rows, m/s.
    pub flow: Vec<[f32; 2]>,
    /// Bumped every time this tile's water changes, so a reader can skip a
    /// tile it has already drawn.
    pub serial: u64,
}

/// Tiled water's surface: the tiles that hold any, by key.
#[derive(Clone, Debug, PartialEq)]
pub struct TiledSurface {
    pub patch: Patch,
    /// Keyed by [`tile_key`]; a tile not here is dry.
    pub tiles: std::collections::HashMap<u32, Arc<TileLevels>>,
}

/// The key of the tile `(ti, tj)` tiles in from the patch's corner.
pub fn tile_key(ti: usize, tj: usize) -> u32 {
    ((tj as u32) << 16) | ti as u32
}

impl TiledSurface {
    /// The cell `(i, j)`'s level, depth and flow, when it is wet.
    pub fn cell(&self, i: i64, j: i64) -> Option<(f32, f32, [f32; 2])> {
        if i < 0 || j < 0 || i as usize >= self.patch.columns || j as usize >= self.patch.rows {
            return None;
        }
        let (i, j) = (i as usize, j as usize);
        let t = self.tiles.get(&tile_key(i / TILE, j / TILE))?;
        let c = (j % TILE) * TILE + i % TILE;
        let l = *t.level.get(c)?;
        l.is_finite().then(|| (l, t.depth.get(c).copied().unwrap_or(0.0), t.flow.get(c).copied().unwrap_or_default()))
    }

    /// The surface height, the depth and the flow at grid point `(gx, gz)`,
    /// blended from the wet cells round it, as [`HeightGrid::sample`] blends
    /// its points. A point in the half of a wet cell next to a dry one is
    /// the wet cell's water, out to the face between them.
    pub fn sample(&self, gx: f64, gz: f64) -> Option<(f64, f64, [f64; 2])> {
        let (ix, iz) = (gx.floor(), gz.floor());
        let (fx, fz) = (gx - ix, gz - iz);
        let (ix, iz) = (ix as i64, iz as i64);
        let (mut level, mut depth, mut flow, mut weight) = (0.0, 0.0, [0.0; 2], 0.0);
        for (dx, dz, w) in [(0, 0, (1.0 - fx) * (1.0 - fz)), (1, 0, fx * (1.0 - fz)), (0, 1, (1.0 - fx) * fz), (1, 1, fx * fz)] {
            if w <= 0.0 {
                continue;
            }
            let Some((l, d, f)) = self.cell(ix + dx, iz + dz) else { continue };
            level += l as f64 * w;
            depth += d as f64 * w;
            flow[0] += f[0] as f64 * w;
            flow[1] += f[1] as f64 * w;
            weight += w;
        }
        (weight > 0.0 && self.cell(gx.round() as i64, gz.round() as i64).is_some())
            .then(|| (level / weight, depth / weight, [flow[0] / weight, flow[1] / weight]))
    }

    /// The mean level of the wet cells, as a script reads a level (a radius
    /// on a round world), or `None` when it is all dry.
    pub fn mean_level(&self) -> Option<f64> {
        let (mut sum, mut n) = (0.0, 0usize);
        for t in self.tiles.values() {
            for l in t.level.iter().filter(|l| l.is_finite()) {
                sum += *l as f64;
                n += 1;
            }
        }
        (n > 0).then(|| self.patch.base() + sum / n as f64)
    }
}

/// One body of water a script declared.
#[derive(Clone, Debug, PartialEq)]
pub struct DeclaredWater {
    pub id: u32,
    /// The node it lies on, by name; `None` pins it to the world.
    pub anchor: Option<String>,
    /// Where the anchor is this frame, refreshed by the host.
    pub frame: Frame,
    pub surface: Surface,
    /// kg/m³, as a Water Volume's.
    pub density: f32,
    pub drag: f32,
    pub angular_drag: f32,
    /// A current everywhere in it, m/s in the anchor's frame, on top of a
    /// grid's own flow.
    pub current: Vec3,
    /// Its bed: how far below the surface the water reaches, in metres. A
    /// point deeper than this is in the rock under the lake (a cave beneath
    /// it), not in the lake. Infinite reaches down without end.
    pub floor: f64,
    /// What the camera sees from inside it, or `None` for no underwater look.
    pub look: Option<UnderwaterLook>,
    /// The engine moves this water: its grid is filled to a level over the
    /// ground the engine finds under it, and from then on flows, drains into
    /// holes dug beside it and settles, the grid's levels and flow following.
    /// `None` for water that stands where it was declared.
    pub simulated: Option<SimSpec>,
}

/// How a script asked the engine to move a body of water, and what the
/// engine reports back about it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimSpec {
    /// The height it is filled to, in its parent's frame.
    pub level: f32,
    /// The bed's Manning roughness: 0 for still water's frictionless model,
    /// about 0.035 for a natural river bed.
    pub roughness: f32,
    /// Springs, as `(point index, m³/s)`: a river's head.
    pub springs: Vec<(usize, f32)>,
    /// Hold every point on the grid's edge whose ground is below `level` at
    /// `level`: the sea or a larger lake beyond the patch, which fills a
    /// trench dug from its shore and stays full.
    pub hold_edges: bool,
    /// Where its water pours over a drop of more than 1.5 m, refreshed by
    /// the engine as it flows.
    pub falls: Vec<WaterFall>,
    /// Water stored in tiles, only where it is, on a patch of a round world
    /// or a plane (declared with `center`); `None` for a dense grid.
    pub tiled: Option<TiledSpec>,
}

/// How a tiled sim starts, and how it moves.
#[derive(Clone, Debug, PartialEq)]
pub struct TiledSpec {
    pub patch: Patch,
    /// Filled to [`SimSpec::level`] wherever the ground is below it at the
    /// start; false starts dry, or from `depths`.
    pub fill: bool,
    /// Metres of water per cell at the start, a row at a time.
    pub depths: Option<Arc<Vec<f32>>>,
    /// Cells held at a level from the start.
    pub held: Vec<HeldCell>,
    /// The cheaper model, without momentum.
    pub pipes: bool,
    /// The most a frame spends stepping it, ms: past this the water slows.
    pub budget_ms: f32,
    /// Hold the patch's edge cells whose ground is below the level.
    pub hold_edges: bool,
    /// Wet cells to start from, `(column, row, depth)`: what `water.state`
    /// handed back.
    pub state: Option<Arc<Vec<(u32, u32, f32)>>>,
    /// The least depth that moves and reads as water, metres.
    pub min_depth: f32,
    /// Films thinner than `soak_below` soak away at `soak` metres a second.
    pub soak: f32,
    pub soak_below: f32,
    /// The surface change that counts, metres: water within it sleeps, and
    /// a tile's serial moves only for more.
    pub still_depth: f32,
}

/// A cell held at a level, and the group what it gives is counted in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HeldCell {
    pub i: u32,
    pub j: u32,
    /// As a script gives a level: a radius on a round world.
    pub level: f32,
    pub group: u16,
}

/// A change a script asked of a tiled sim, for the engine to make before
/// its next step.
#[derive(Clone, Debug, PartialEq)]
pub enum SimCmd {
    Hold { id: u32, cells: Vec<[u32; 2]>, level: f32, group: u16 },
    Release { id: u32, cells: Vec<[u32; 2]> },
    AddWater { id: u32, cell: [u32; 2], volume: f64 },
    /// A spring at a cell, `rate` m³/s; 0 takes it away.
    Spring { id: u32, cell: [u32; 2], rate: f32 },
}

impl SimCmd {
    /// The volume it is for.
    pub fn id(&self) -> u32 {
        match self {
            SimCmd::Hold { id, .. } | SimCmd::Release { id, .. } | SimCmd::AddWater { id, .. } | SimCmd::Spring { id, .. } => *id,
        }
    }
}

/// Water pouring off a drop: where a waterfall goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WaterFall {
    /// The top of the fall, at the water's surface, in world space.
    pub pos: DVec3,
    /// Which way it pours, across the ground, in world space.
    pub dir: Vec3,
    /// The edge it pours over, metres wide.
    pub width: f32,
    /// m³/s over the edge.
    pub discharge: f32,
    /// From the surface at the top to the surface at the foot, metres.
    pub drop: f32,
}

/// What being under water looks like: the colour everything fades toward and
/// how far one can see, as a Water Volume node's `tint` and `visibility`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnderwaterLook {
    pub tint: [f32; 3],
    pub visibility: f32,
}

impl Default for UnderwaterLook {
    /// A Water Volume node's own defaults, so declared water and a node's
    /// water look alike until a game says otherwise.
    fn default() -> Self {
        Self { tint: [0.10, 0.32, 0.38], visibility: 28.0 }
    }
}

/// Where a point is in a declared body of water.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WaterAt {
    /// Metres below the surface; 0 when dry.
    pub depth: f64,
    /// Signed metres above the surface (negative under it); infinite where
    /// this water is not, so "the nearest surface" never picks it.
    pub above: f64,
    /// Out of the water, in world space.
    pub up: DVec3,
    /// The water's velocity, world space, m/s.
    pub flow: DVec3,
}

impl DeclaredWater {
    /// Where world point `p` is in this water.
    pub fn at(&self, p: DVec3) -> WaterAt {
        let local = self.frame.to_local(p);
        let current = self.current.as_dvec3();
        let (above, up_local, flow_local) = match &self.surface {
            Surface::Lake { radius, axis, reach } => {
                let r = local.length();
                let up = local.try_normalize().unwrap_or(DVec3::Y);
                // Along the surface from the lake's centre, as an angle.
                let angle = up.dot(axis.try_normalize().unwrap_or(DVec3::Y)).clamp(-1.0, 1.0).acos();
                if angle * radius > *reach {
                    return WaterAt { above: f64::INFINITY, up: self.frame.to_world(up) - self.frame.origin, ..Default::default() };
                }
                (r - radius, up, current)
            }
            Surface::Grid(g) => match g.sample(local.x, local.z) {
                Some((level, f)) => (local.y - level, DVec3::Y, current + DVec3::new(f[0], 0.0, f[1])),
                None => return WaterAt { above: f64::INFINITY, up: self.frame.to_world(DVec3::Y) - self.frame.origin, ..Default::default() },
            },
            Surface::Tiles(t) => {
                let dry = |up: DVec3| WaterAt { above: f64::INFINITY, up: self.frame.to_world(up) - self.frame.origin, ..Default::default() };
                let Some((gx, gz, height, up)) = t.patch.locate(local) else { return dry(t.patch.up) };
                match t.sample(gx, gz) {
                    // Below the water's bed is the ground, or a cave in it.
                    Some((level, depth, _)) if level - height > depth + 0.5 => return dry(up),
                    Some((level, _, f)) => {
                        let (a, b) = t.patch.along(up);
                        (height - level, up, current + a * f[0] + b * f[1])
                    }
                    None => return dry(up),
                }
            }
        };
        let dir = |v: DVec3| self.frame.to_world(v) - self.frame.origin;
        // Below the bed is rock, or a cave in it: not this water.
        if -above > self.floor {
            return WaterAt { above: f64::INFINITY, up: dir(up_local), ..Default::default() };
        }
        WaterAt { depth: (-above).max(0.0), above, up: dir(up_local), flow: if above <= 0.0 { dir(flow_local) } else { DVec3::ZERO } }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lake() -> DeclaredWater {
        DeclaredWater {
            id: 1,
            anchor: None,
            frame: Frame::IDENTITY,
            surface: Surface::Lake { radius: 805.0, axis: DVec3::Y, reach: 60.0 },
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            current: Vec3::ZERO,
            floor: f64::INFINITY,
            look: Some(UnderwaterLook::default()),
            simulated: None,
        }
    }

    /// **A lake has a bed.** Water reaches down to its deepest point and no
    /// further: a cave under the lake is dry, where the lake's sphere alone
    /// would have flooded it to the planet's core.
    #[test]
    fn a_cave_under_a_lake_is_dry() {
        let w = DeclaredWater { floor: 6.0, ..lake() };
        assert!((w.at(DVec3::new(0.0, 800.0, 0.0)).depth - 5.0).abs() < 1e-9, "in the lake");
        let cave = w.at(DVec3::new(0.0, 790.0, 0.0));
        assert_eq!(cave.depth, 0.0, "a cave 15 m down is in the water");
        assert!(cave.above.is_infinite(), "a cave has no surface of this lake above it");
        assert!(lake().at(DVec3::new(0.0, 790.0, 0.0)).depth > 0.0, "without a bed it reaches down");
    }

    /// **A lake on a round world is round, and only in its basin.** Depth is
    /// measured along the planet's radius, the surface follows the curve, and
    /// past the farthest shore there is no lake however low the ground is.
    #[test]
    fn a_lake_follows_the_planet_and_stays_in_its_basin() {
        let w = lake();
        let at = w.at(DVec3::new(0.0, 803.0, 0.0));
        assert!((at.depth - 2.0).abs() < 1e-9 && (at.up - DVec3::Y).length() < 1e-12);
        // 50 m along the surface the water is still 805 m from the centre:
        // a flat plane through the centre point would be 1.55 m off here.
        let a: f64 = 50.0 / 805.0;
        let p = DVec3::new(a.sin(), a.cos(), 0.0) * 804.0;
        assert!((w.at(p).depth - 1.0).abs() < 1e-9, "{}", w.at(p).depth);
        // Past the shore: dry, and no surface to be above.
        let b: f64 = 70.0 / 805.0;
        let q = DVec3::new(b.sin(), b.cos(), 0.0) * 800.0;
        assert_eq!(w.at(q).depth, 0.0);
        assert!(w.at(q).above.is_infinite());
    }

    /// **Foam is white water, not every ripple.** A deep slow river is
    /// glassy, the same river run shallow and fast is white, and a film on a
    /// drying bank has none however fast it creeps.
    #[test]
    fn foam_follows_the_flow_running_faster_than_its_own_waves() {
        assert_eq!(foam(2.0, 0.5), 0.0, "a deep slow river");
        assert_eq!(foam(0.0, 3.0), 0.0, "dry ground");
        assert_eq!(foam(f32::NAN, 3.0), 0.0, "a dry point's depth is not a number");
        assert!(foam(0.3, 3.0) > 0.99, "0.3 m deep at 3 m/s outruns its waves: {}", foam(0.3, 3.0));
        let mid = foam(0.5, 2.2);
        assert!(mid > 0.1 && mid < 0.9, "near the speed of its own waves it is partly broken: {mid}");
        assert!(foam(0.01, 1.0) == 0.0, "a film has no foam: {}", foam(0.01, 1.0));
        assert!(foam(0.3, 3.0) > foam(0.3, 1.5), "faster is whiter");
    }

    /// **It turns with its planet.** Declared in the planet's frame, the lake
    /// is where the planet has carried it.
    #[test]
    fn a_lake_turns_with_its_planet() {
        let mut w = lake();
        w.frame = Frame { origin: DVec3::new(1000.0, 0.0, 0.0), rot: glam::Quat::from_rotation_z(std::f32::consts::FRAC_PI_2) };
        // The planet's +Y now points along world −X.
        let at = w.at(DVec3::new(1000.0 - 804.0, 0.0, 0.0));
        assert!((at.depth - 1.0).abs() < 1e-4, "{}", at.depth);
        assert!((at.up - DVec3::NEG_X).length() < 1e-6);
        assert_eq!(w.at(DVec3::new(1000.0, 804.0, 0.0)).depth, 0.0);
    }

    /// **A grid's surface is its levels, blended, and dry where it says
    /// dry.** A shore cell keeps the water's own level, and the flow comes
    /// with the depth.
    #[test]
    fn a_grid_is_wet_where_its_levels_are() {
        let nan = f32::NAN;
        let g = HeightGrid {
            x0: 0.0,
            z0: 0.0,
            cell: 2.0,
            columns: 3,
            rows: 2,
            levels: vec![1.0, 2.0, nan, 1.0, 2.0, nan],
            flow: vec![[1.0, 0.0]; 6],
        };
        let w = DeclaredWater { surface: Surface::Grid(Arc::new(g)), current: Vec3::new(0.0, 0.0, 0.5), ..lake() };
        assert!((w.at(DVec3::new(1.0, 0.0, 1.0)).depth - 1.5).abs() < 1e-9);
        // Halfway into the dry column: the wet corners' level, not half of it.
        assert!((w.at(DVec3::new(3.0, 0.0, 1.0)).depth - 2.0).abs() < 1e-9);
        assert_eq!(w.at(DVec3::new(9.0, 0.0, 1.0)).depth, 0.0, "off the grid");
        assert_eq!(w.at(DVec3::new(1.0, 0.0, 1.0)).flow, DVec3::new(1.0, 0.0, 0.5));
        assert_eq!(w.at(DVec3::new(1.0, 5.0, 1.0)).flow, DVec3::ZERO, "no current in the air");
    }
}
