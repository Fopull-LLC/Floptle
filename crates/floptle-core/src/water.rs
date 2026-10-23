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
    /// The engine moves this water: its grid is filled to this level over
    /// the ground the engine finds under it, and from then on flows, drains
    /// into holes dug beside it and settles, the grid's levels and flow
    /// following. `None` for water that stands where it was declared.
    pub simulated: Option<f32>,
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
