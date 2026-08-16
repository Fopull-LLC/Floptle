//! Navigation — where a character can stand, and how it gets from here to there.
//!
//! The engine had no answer to "walk to that door". This is it: bake the level's
//! geometry once into a **navmesh** — the walkable surface as convex polygons —
//! and then paths are a search over a few hundred polygons rather than a march
//! through the whole world.
//!
//! # The four numbers
//!
//! Everything about a bake is [`NavSettings`], and only four of its fields
//! describe the character:
//!
//! | | |
//! |---|---|
//! | `agent_radius` | how wide it is — walkable ground closer than this to a wall is not walkable, so a path never scrapes a corner |
//! | `agent_height` | how tall it is — ground with less headroom than this is not walkable |
//! | `max_slope` | how steep a floor it will walk up |
//! | `step_height` | how big a lip it steps over rather than walks around |
//!
//! They are the four Unity asks for, in the same words, because they are the
//! four that actually describe walking and a designer coming from there should
//! not have to learn a new vocabulary to get the same result.
//!
//! # How it is built
//!
//! Triangles in, polygons out, through a **heightfield**: the world is divided
//! into square columns, every triangle is sampled into the columns it covers,
//! and each column ends up with a sorted stack of surfaces. A surface is
//! walkable if it is not too steep and has enough room above it. Then the
//! walkable cells are eroded by the agent's radius, grouped into connected
//! regions, and each region is cut into convex rectangles.
//!
//! **Rectangles rather than Recast's traced-and-simplified contours.** They are
//! convex by construction, they share whole edges (which is exactly what a
//! funnel needs to smooth a path), and the whole step is a few dozen lines
//! instead of a thousand. What it costs is that a diagonal wall comes out
//! stair-stepped rather than as one long edge — more polygons than strictly
//! needed, and funnel smoothing hides the shape in the path itself. That is a
//! trade worth revisiting with a merge pass later; it is not worth paying for
//! up front.
//!
//! Nothing here touches the GPU, the ECS or egui: a bake is triangles and
//! numbers, so it can be tested by writing down a floor and asserting what comes
//! back.

use serde::{Deserialize, Serialize};

pub mod agent;
pub mod carve;
mod splice;
pub mod filter;
pub mod heightfield;
pub mod index;
pub mod link;
pub mod mesh;
pub mod overlay;
pub mod path;
pub mod walkable;

pub use agent::{Agent, AgentId, AgentParams, AgentState, Crowd, Ride};
pub use carve::Obstacle;
pub use filter::{Area, AreaVolume, QueryFilter, MAX_AREAS, WALKABLE};
pub use heightfield::{Column, Heightfield, Surface};
pub use index::PolyIndex;
pub use link::OffLink;
pub use mesh::{Link, NavMesh, Poly};
pub use overlay::{Edge, Overlay, Step, SurfaceTri};
pub use path::{Crossing, Path};
pub use walkable::{Cell, WalkableGrid};

/// Bake a navmesh from triangles — the whole pipeline in one call.
///
/// `None` when there is nothing to walk on: no triangles, or no floor in them
/// that this character fits on. That is worth telling apart from a mesh with no
/// polygons, which cannot happen.
///
/// ```
/// use floptle_nav::{bake, NavSettings, Tri};
/// let floor = [
///     Tri::new([0.0, 0.0, 0.0], [8.0, 0.0, 0.0], [0.0, 0.0, 8.0]),
///     Tri::new([8.0, 0.0, 0.0], [8.0, 0.0, 8.0], [0.0, 0.0, 8.0]),
/// ];
/// let mesh = bake(&floor, &NavSettings::default()).unwrap();
/// let path = mesh.path([1.0, 0.0, 1.0], [7.0, 0.0, 7.0]).unwrap();
/// assert!(path.complete);
/// ```
pub fn bake(tris: &[Tri], settings: &NavSettings) -> Option<NavMesh> {
    let field = Heightfield::build(tris, settings)?;
    let grid = WalkableGrid::build(&field, settings)?;
    NavMesh::build(&grid, settings)
}

/// Bake with everything a designer put in the level besides its geometry:
/// volumes that paint or carve the ground, and links that join it up.
///
/// The three arrive together because they are resolved in one order and the
/// order matters — volumes change the ground, so they run first; links are
/// snapped onto whatever ground came out, so they run last. A caller doing this
/// by hand would get it right the first time and wrong after the next change.
///
/// ```
/// use floptle_nav::{bake_with, NavSettings, OffLink, Tri};
/// // Two floors, four metres apart in x, with a plank between them.
/// let quad = |x: f32| {
///     [
///         Tri::new([x, 0.0, 0.0], [x + 3.0, 0.0, 0.0], [x, 0.0, 3.0]),
///         Tri::new([x + 3.0, 0.0, 0.0], [x + 3.0, 0.0, 3.0], [x, 0.0, 3.0]),
///     ]
/// };
/// let tris: Vec<Tri> = quad(0.0).into_iter().chain(quad(7.0)).collect();
/// let plank = OffLink::new(1, "plank", [2.5, 0.0, 1.5], [7.5, 0.0, 1.5]);
///
/// let settings = NavSettings { agent_radius: 0.0, cell_size: 0.25, ..Default::default() };
/// let mesh = bake_with(&tris, &settings, &[], vec![plank]).unwrap();
/// let path = mesh.path([1.0, 0.0, 1.5], [9.0, 0.0, 1.5]).unwrap();
/// assert!(path.complete, "the plank joins two islands the bake cannot");
/// assert_eq!(path.crossings.len(), 1, "and the walk knows it is crossing one");
/// ```
pub fn bake_with(
    tris: &[Tri],
    settings: &NavSettings,
    volumes: &[AreaVolume],
    links: Vec<OffLink>,
) -> Option<NavMesh> {
    let field = Heightfield::build(tris, settings)?;
    let grid = WalkableGrid::build_with(&field, settings, volumes)?;
    Some(NavMesh::build(&grid, settings)?.with_links(links))
}

/// What a bake needs to know. Defaults describe a human-ish character on a
/// metres-and-seconds scale, which is what Floptle's primitives are built at.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NavSettings {
    /// How wide the character is. Ground within this distance of something
    /// unwalkable is dropped, so a path can be followed by something with a
    /// body rather than by a point.
    pub agent_radius: f32,
    /// How tall the character is. Ground with less clearance than this above it
    /// is not walkable — the character would not fit.
    pub agent_height: f32,
    /// The steepest floor it will walk up, in degrees from flat.
    pub max_slope: f32,
    /// The tallest lip it steps over. Two neighbouring surfaces within this of
    /// each other are connected; beyond it they are a ledge.
    pub step_height: f32,
    /// How wide a column is. **The one performance knob**: halving it
    /// quadruples the columns and the bake's cost with them. Small enough to
    /// resolve the gaps that matter, no smaller.
    pub cell_size: f32,
}

impl Default for NavSettings {
    fn default() -> Self {
        // Unity's own defaults for the four that describe the character, so a
        // level baked there and baked here comes out the same shape. The cell
        // size is Unity's rule too — a third of the radius — which is also
        // comfortably inside what `cell_size_advice` asks for.
        //
        // These were originally 0.4 / 1.8 / 0.4 with a 0.3 cell, which
        // `cell_size_advice` flagged the moment it existed: a 0.3 cell against a
        // 0.4 radius erodes 0.8 m out of every doorway. The check found it in
        // its own crate's defaults before it ever found it in a level.
        Self {
            agent_radius: 0.5,
            agent_height: 2.0,
            max_slope: 45.0,
            step_height: 0.75,
            cell_size: 0.15,
        }
    }
}

impl NavSettings {
    /// The dot product a surface normal must reach to count as floor.
    ///
    /// Comparing cosines rather than converting each normal to an angle: one
    /// `cos` per bake instead of an `acos` per triangle, and the comparison is
    /// exact at the boundary rather than a rounding of one.
    pub fn walkable_dot(&self) -> f32 {
        self.max_slope.clamp(0.0, 89.9).to_radians().cos()
    }

    /// Erosion distance measured in whole columns, rounded **up** — a radius
    /// that rounds down would let a character's shoulder into a wall, and being
    /// slightly too careful is a path that goes around; being slightly not
    /// careful enough is a path that does not work.
    pub fn radius_in_cells(&self) -> i32 {
        if self.cell_size <= 0.0 {
            return 0;
        }
        (self.agent_radius / self.cell_size).ceil() as i32
    }
}

/// Settings that will quietly do something other than what they say, or `None`
/// when they are sound.
///
/// Erosion happens in whole columns and rounds up, so a cell size that is
/// coarse next to the agent's radius eats far more ground than the radius asked
/// for: at `cell_size` 0.25 and `agent_radius` 0.1, every edge loses 0.25 m
/// rather than 0.1, and a corridor two cells wide disappears entirely. The bake
/// is not wrong — it is the same rounding Recast does, and it is the safe
/// direction to round — but "my corridor vanished" gives no hint that a number
/// nobody thought about is the reason.
///
/// The useful ratio is a cell **at most half** the radius. That is where whole
/// columns stop being a coarse approximation of a circle.
pub fn cell_size_advice(settings: &NavSettings) -> Option<String> {
    if settings.agent_radius <= 0.0 || settings.cell_size <= 0.0 {
        return None;
    }
    let eroded = settings.radius_in_cells() as f32 * settings.cell_size;
    if settings.cell_size > settings.agent_radius / 2.0 {
        return Some(format!(
            "cell size {:.2} is coarse next to an agent radius of {:.2}: edges will lose \
             {:.2} rather than {:.2}, and gaps narrower than about {:.2} will close up. \
             Try a cell size of {:.2} or less.",
            settings.cell_size,
            settings.agent_radius,
            eroded,
            settings.agent_radius,
            eroded * 2.0,
            settings.agent_radius / 2.0,
        ));
    }
    None
}

/// One triangle of level geometry, in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tri {
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub c: [f32; 3],
}

impl Tri {
    pub fn new(a: [f32; 3], b: [f32; 3], c: [f32; 3]) -> Self {
        Self { a, b, c }
    }

    /// Unit normal. Degenerate triangles (a spike, a repeated vertex) report
    /// straight up rather than a `NaN` that would poison every comparison it
    /// touched — a zero-area triangle cannot be stood on either way, and the
    /// clearance test drops it.
    pub fn normal(&self) -> [f32; 3] {
        let u = [self.b[0] - self.a[0], self.b[1] - self.a[1], self.b[2] - self.a[2]];
        let v = [self.c[0] - self.a[0], self.c[1] - self.a[1], self.c[2] - self.a[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        if len <= f32::EPSILON {
            return [0.0, 1.0, 0.0];
        }
        [n[0] / len, n[1] / len, n[2] / len]
    }

    /// Axis-aligned bounds, as (min, max).
    pub fn bounds(&self) -> ([f32; 3], [f32; 3]) {
        let mut lo = self.a;
        let mut hi = self.a;
        for p in [self.b, self.c] {
            for i in 0..3 {
                lo[i] = lo[i].min(p[i]);
                hi[i] = hi[i].max(p[i]);
            }
        }
        (lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_flat_floor_is_walkable_and_a_wall_is_not() {
        let s = NavSettings::default();
        let floor = Tri::new([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]);
        let wall = Tri::new([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 0.0, 0.0]);
        assert!(floor.normal()[1].abs() >= s.walkable_dot());
        assert!(wall.normal()[1].abs() < s.walkable_dot());
    }

    /// The boundary is the interesting part of a slope limit: 45° means 45°
    /// walks, not "about 45".
    #[test]
    fn the_slope_limit_is_exact_at_its_own_boundary() {
        let s = NavSettings { max_slope: 45.0, ..Default::default() };
        // A ramp at exactly 45° — its normal is 45° off vertical.
        let ramp = Tri::new([0.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 0.0, 1.0]);
        let n = ramp.normal();
        assert!(n[1].abs() >= s.walkable_dot() - 1e-6, "45° must walk: {}", n[1]);

        // A degree steeper must not.
        let steeper = Tri::new([0.0, 0.0, 0.0], [1.0, 1.2, 0.0], [0.0, 0.0, 1.0]);
        assert!(steeper.normal()[1].abs() < s.walkable_dot());
    }

    /// A radius that rounds DOWN puts a shoulder in a wall. Rounding up costs a
    /// path that goes slightly wide, which is the failure worth having.
    #[test]
    fn the_agent_radius_rounds_up_to_whole_cells() {
        let s = NavSettings { agent_radius: 0.4, cell_size: 0.3, ..Default::default() };
        assert_eq!(s.radius_in_cells(), 2, "0.4 / 0.3 is 1.33 cells, which must not be 1");
        let exact = NavSettings { agent_radius: 0.6, cell_size: 0.3, ..Default::default() };
        assert_eq!(exact.radius_in_cells(), 2);
        let none = NavSettings { agent_radius: 0.0, cell_size: 0.3, ..Default::default() };
        assert_eq!(none.radius_in_cells(), 0);
    }

    /// A degenerate triangle must not produce NaN — one NaN normal would make
    /// every comparison downstream of it false in ways that are very hard to
    /// see in a baked mesh.
    #[test]
    fn a_degenerate_triangle_reports_a_usable_normal() {
        let spike = Tri::new([0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        let n = spike.normal();
        assert!(n.iter().all(|c| c.is_finite()), "{n:?}");
    }

    /// A number nobody thought about silently eating the level is the failure
    /// shape worth naming rather than only avoiding.
    #[test]
    fn coarse_cells_next_to_a_small_agent_are_called_out() {
        // The case that broke a corridor: 0.25 cells, 0.1 radius — 2.5x the
        // erosion asked for.
        let coarse = NavSettings { cell_size: 0.25, agent_radius: 0.1, ..Default::default() };
        let advice = cell_size_advice(&coarse).expect("this must not pass silently");
        assert!(advice.contains("0.25"), "{advice}");
        assert!(advice.contains("0.05"), "it should name the cell size to use: {advice}");

        // Sound settings say nothing. Advice that fires on good input is advice
        // people learn to scroll past.
        let sound = NavSettings { cell_size: 0.2, agent_radius: 0.4, ..Default::default() };
        assert!(cell_size_advice(&sound).is_none());
        assert!(cell_size_advice(&NavSettings::default()).is_none(), "the defaults must be sound");

        // A point-sized agent erodes nothing, so there is nothing to warn about.
        let point = NavSettings { agent_radius: 0.0, ..Default::default() };
        assert!(cell_size_advice(&point).is_none());
    }

    #[test]
    fn bounds_cover_every_vertex() {
        let t = Tri::new([1.0, 5.0, -2.0], [-3.0, 0.0, 4.0], [0.0, 2.0, 0.0]);
        let (lo, hi) = t.bounds();
        assert_eq!(lo, [-3.0, 0.0, -2.0]);
        assert_eq!(hi, [1.0, 5.0, 4.0]);
    }
}
