//! Drawing the navmesh so a person can read it.
//!
//! The bake cuts the walkable surface into [rectangles](crate::Poly), which is
//! the right shape to *search* and the wrong shape to *look at*. Drawing every
//! rectangle's own outline — which is what the editor did first — turns one
//! continuous floor into a field of floating boxes, and the one question the
//! picture exists to answer is the one it cannot:
//!
//! > *are these two pieces of ground actually joined?*
//!
//! An outline around each rectangle says nothing about that. Two rectangles that
//! a character can walk between look exactly like two that it cannot.
//!
//! # What this builds instead
//!
//! Three things, from the polygons and the portals the bake already produced:
//!
//! - **[`Overlay::tris`]** — the walkable surface, filled. A room reads as a
//!   floor rather than as a wireframe of nothing.
//! - **[`Overlay::boundary`]** — the outline, drawn **only where the walkable
//!   surface actually ends**. An edge between two rectangles that are linked is
//!   interior and is not drawn, so a floor cut into forty rectangles reads as
//!   one shape with one edge. This is the whole readability change.
//! - **[`Overlay::steps`]** — where two pieces of ground at *different heights*
//!   are joined, drawn explicitly as the surface that joins them. This is what
//!   makes the settings legible: raise `max_slope` and a ledge that was two
//!   separated elevations becomes one connected run, and you can see it happen.
//!
//! [`Overlay::cells`] keeps the old per-rectangle wireframe for when you want to
//! see how the bake actually cut things up — which is a debugging question, not
//! the everyday one, so it is not the default.
//!
//! # Interior is decided by the links, not by adjacency
//!
//! Two rectangles can touch in plan and not be connected at all: a walkway over
//! a floor, or a ledge too tall to step onto. The test for "do not draw this
//! edge" is therefore *is the polygon on the other side of it **linked** to this
//! one*, which is exactly the relation a path is allowed to use. Drawing follows
//! reachability, so what you see is what a character can do.
//!
//! Nothing here touches the GPU: it is arrays of points, so it is testable by
//! writing down a floor and asserting how many edges come back.

use std::collections::{HashMap, HashSet};

use crate::mesh::NavMesh;

/// A line to draw, in the mesh's own (anchor-relative) space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub a: [f32; 3],
    pub b: [f32; 3],
    /// Which walkable region it belongs to — the bake's own grouping, before
    /// any link joined two of them.
    pub region: u32,
    /// Which **island** it belongs to: what a character can actually reach, once
    /// the links are counted. **This is what to colour by.** See
    /// [`NavMesh::islands`](crate::NavMesh::islands).
    pub island: u32,
}

/// A filled triangle of walkable surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceTri {
    pub a: [f32; 3],
    pub b: [f32; 3],
    pub c: [f32; 3],
    pub region: u32,
    /// What a character can actually reach — see [`Edge::island`].
    pub island: u32,
    /// Which kind of ground this is. Painted ground has to *look* painted, or
    /// "did my mud volume do anything" is a question the picture cannot answer
    /// and everyone answers by baking again and squinting.
    pub area: u8,
}

/// Two pieces of ground at different heights that a character can move between.
///
/// Drawn as the quad joining them — the low portal edge and the same edge at the
/// high side — so a step, a kerb or a walkable ramp reads as a connection rather
/// than as two unrelated slabs that happen to be near each other.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    /// The portal at the lower polygon's height.
    pub low: [[f32; 3]; 2],
    /// The same portal at the higher polygon's height.
    pub high: [[f32; 3]; 2],
    pub region: u32,
    /// What a character can actually reach — see [`Edge::island`].
    pub island: u32,
    /// How far up it goes. Worth showing: a 5cm kerb and a 70cm clamber are both
    /// "connected" and only one of them is what the designer meant.
    pub rise: f32,
}

/// A link, as the bake resolved it.
///
/// Drawn from the overlay rather than from the node, because these are the ends
/// the bake actually *found* — snapped onto the floor, or nowhere. A link whose
/// mouth missed the ground is the failure worth seeing, and the node's own gizmo
/// cannot show it: the node draws where you put it, which is exactly the thing
/// that turned out to be wrong.
#[derive(Clone, Debug, PartialEq)]
pub struct LinkArc {
    pub from: [f32; 3],
    pub to: [f32; 3],
    pub bidirectional: bool,
    /// Both ends landed on walkable ground. False means this link does nothing.
    pub resolved: bool,
    /// Off — a shut door. Still drawn, because a door you cannot see is a level
    /// nobody can debug.
    pub enabled: bool,
    pub name: String,
    /// A ladder somebody placed, or a drop the bake found. Drawn differently,
    /// because "the bake decided this" and "I put this here" are different
    /// claims and only one of them is worth arguing with.
    pub kind: crate::LinkKind,
    /// How far the two ends are apart, so a drawing can bow an arc in
    /// proportion rather than by a constant that looks right at one scale.
    pub span: f32,
}

impl LinkArc {
    /// Where the drawn arc is, `t` running 0 at the mouth to 1 at the landing.
    ///
    /// The curve itself is [`crate::link::arc_point`], shared with the crowd
    /// that carries an agent across — so what the overlay draws is the path the
    /// character takes, rather than a second curve that merely resembles it.
    pub fn point_at(&self, t: f32) -> [f32; 3] {
        crate::link::arc_point(self.kind, self.from, self.to, t)
    }
}

/// How many segments an arc is drawn with. Enough to read as a curve, few
/// enough that a level's worth of them is still a handful of thousand lines.
pub const ARC_STEPS: usize = 8;

/// Everything needed to draw one baked navmesh.
#[derive(Clone, Debug, Default)]
pub struct Overlay {
    pub tris: Vec<SurfaceTri>,
    pub boundary: Vec<Edge>,
    pub steps: Vec<Step>,
    /// Every polygon's own rectangle — the bake's working, for when that is the
    /// question. Not drawn by default.
    pub cells: Vec<Edge>,
    /// The level's links, where the bake put them.
    pub links: Vec<LinkArc>,
}

/// A height difference below which two linked polygons are the same floor.
///
/// Not zero: a rectangle's height is the mean of the cells in it, so two halves
/// of one flat floor routinely differ in the last few bits. Drawing a step
/// ribbon for a half-millimetre would put a bright line down the middle of every
/// room.
fn step_epsilon(mesh: &NavMesh) -> f32 {
    (mesh.settings.cell_size * 0.25).max(0.02)
}

impl Overlay {
    /// Build the drawable form of `mesh`.
    ///
    /// `lift` raises everything off the ground by that much, because an overlay
    /// drawn exactly on the floor fights the floor it describes.
    pub fn build(mesh: &NavMesh, lift: f32) -> Overlay {
        let mut out = Overlay::default();
        if mesh.polys.is_empty() {
            return out;
        }
        let cell = mesh.cell_size;
        let y_of = |i: usize| mesh.polys[i].centre[1] + lift;
        // What a character can actually reach, once the links are counted.
        // Colouring by REGION was right when a region was the only kind of
        // connection there was; it is actively misleading now, because a ledge
        // and the floor its drop lands on are two regions and one place.
        let island = mesh.islands();

        // Which polygons cover each column. A column can hold more than one —
        // a walkway over a floor is exactly that — so this is a list, and
        // "adjacent" alone is never enough to decide anything.
        let mut columns: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
        for (i, p) in mesh.polys.iter().enumerate() {
            for x in p.x0..p.x0 + p.w {
                for z in p.z0..p.z0 + p.d {
                    columns.entry((x, z)).or_default().push(i);
                }
            }
        }

        // The relation that decides what is interior: reachability, not touching.
        let mut linked: HashSet<(usize, usize)> = HashSet::new();
        for (i, ls) in mesh.links.iter().enumerate() {
            for l in ls {
                linked.insert((i.min(l.to), i.max(l.to)));
            }
        }
        let joined = |i: usize, j: usize| i == j || linked.contains(&(i.min(j), i.max(j)));

        for (i, p) in mesh.polys.iter().enumerate() {
            let y = y_of(i);
            let (x0, z0) = (p.min[0], p.min[1]);
            let (x1, z1) = (p.max[0], p.max[1]);

            // --- the filled surface ---------------------------------------
            let corner = |x: f32, z: f32| [x, y, z];
            out.tris.push(SurfaceTri {
                a: corner(x0, z0),
                b: corner(x1, z0),
                c: corner(x1, z1),
                region: p.region,
                island: island[i],
                area: p.area,
            });
            out.tris.push(SurfaceTri {
                a: corner(x0, z0),
                b: corner(x1, z1),
                c: corner(x0, z1),
                region: p.region,
                island: island[i],
                area: p.area,
            });

            // --- the rectangle, for the cells view ------------------------
            let c = [corner(x0, z0), corner(x1, z0), corner(x1, z1), corner(x0, z1)];
            for k in 0..4 {
                out.cells.push(Edge {
                    a: c[k],
                    b: c[(k + 1) % 4],
                    region: p.region,
                    island: island[i],
                });
            }

            // --- the outline, where the surface actually ends --------------
            //
            // Each of the four sides is walked one column at a time, asking the
            // column on the far side whether anything there is linked to this
            // polygon. Runs of consecutive open columns are merged, so a wall
            // forty cells long is one line and not forty.
            //
            // `outward` is which neighbouring column to ask; `along` turns a
            // step index into the two endpoints of that column's edge.
            let sides: [(i64, i64, bool, f32); 4] = [
                // (dx, dz, runs_along_z, the fixed coordinate of the edge)
                (-1, 0, true, x0),  // west
                (1, 0, true, x1),   // east
                (0, -1, false, z0), // north
                (0, 1, false, z1),  // south
            ];
            for (dx, dz, along_z, fixed) in sides {
                let n = if along_z { p.d } else { p.w };
                let mut run: Option<(usize, usize)> = None;
                for k in 0..=n {
                    let open = k < n && {
                        let (cx, cz) = if along_z {
                            (p.x0 as i64 + if dx < 0 { -1 } else { p.w as i64 }, (p.z0 + k) as i64)
                        } else {
                            ((p.x0 + k) as i64, p.z0 as i64 + if dz < 0 { -1 } else { p.d as i64 })
                        };
                        if cx < 0 || cz < 0 {
                            true
                        } else {
                            !columns
                                .get(&(cx as usize, cz as usize))
                                .is_some_and(|there| there.iter().any(|&j| joined(i, j)))
                        }
                    };
                    match (open, run) {
                        (true, None) => run = Some((k, k + 1)),
                        (true, Some((s, _))) => run = Some((s, k + 1)),
                        (false, Some((s, e))) => {
                            let (a, b) = if along_z {
                                let za = z0 + s as f32 * cell;
                                let zb = z0 + e as f32 * cell;
                                ([fixed, y, za], [fixed, y, zb])
                            } else {
                                let xa = x0 + s as f32 * cell;
                                let xb = x0 + e as f32 * cell;
                                ([xa, y, fixed], [xb, y, fixed])
                            };
                            out.boundary.push(Edge { a, b, region: p.region, island: island[i] });
                            run = None;
                        }
                        (false, None) => {}
                    }
                }
            }
        }

        // --- where two heights are genuinely joined -----------------------
        let eps = step_epsilon(mesh);
        for (i, ls) in mesh.links.iter().enumerate() {
            for l in ls {
                // Each portal once, from the lower side.
                if l.to <= i {
                    continue;
                }
                let (ya, yb) = (y_of(i), y_of(l.to));
                let rise = (yb - ya).abs();
                if rise <= eps {
                    continue;
                }
                let (lo, hi) = if ya <= yb { (ya, yb) } else { (yb, ya) };
                let at = |p: [f32; 3], y: f32| [p[0], y, p[2]];
                out.steps.push(Step {
                    low: [at(l.left, lo), at(l.right, lo)],
                    high: [at(l.left, hi), at(l.right, hi)],
                    region: mesh.polys[i].region,
                    island: island[i],
                    rise,
                });
            }
        }

        // --- the links, where the bake actually put them ---------------------
        //
        // Lifted like everything else, so an arc drawn along the floor is not
        // fighting the floor for the same pixels.
        for l in &mesh.off_links {
            let up = |p: [f32; 3]| [p[0], p[1] + lift, p[2]];
            out.links.push(LinkArc {
                from: up(l.from),
                to: up(l.to),
                bidirectional: l.bidirectional,
                resolved: l.resolved(),
                enabled: l.enabled,
                name: l.name.clone(),
                kind: l.kind,
                span: l.length(),
            });
        }
        out
    }

    /// How much of the outline the merging saved — `(drawn, if every rectangle
    /// were outlined)`. The editor reports this so the readability change is a
    /// number rather than a claim.
    pub fn outline_saving(&self) -> (usize, usize) {
        (self.boundary.len(), self.cells.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bake, NavSettings, Tri};

    /// A flat square floor, `size` metres on a side.
    fn floor(size: f32, y: f32) -> Vec<Tri> {
        quad(0.0, size, 0.0, size, y)
    }

    /// A link is drawn where the BAKE put it, and an end that missed the floor
    /// has to look different from one that landed — that difference is the only
    /// thing that says why a door does nothing.
    #[test]
    fn links_are_drawn_as_resolved_rather_than_as_placed() {
        let mut tris = quad(0.0, 4.0, 0.0, 4.0, 0.0);
        tris.extend(quad(9.0, 13.0, 0.0, 4.0, 0.0));
        let s = NavSettings { agent_radius: 0.0, cell_size: 0.25, ..Default::default() };
        // One good ladder, and one whose far end is out in space.
        let good = crate::OffLink::new(1, "ladder", [3.5, 0.0, 2.0], [9.5, 0.0, 2.0]);
        let lost = crate::OffLink::new(2, "nowhere", [3.5, 0.0, 3.0], [400.0, 0.0, 400.0]);
        let mesh = crate::bake_with(&tris, &s, &[], vec![good, lost]).unwrap();

        let overlay = Overlay::build(&mesh, 0.05);
        assert_eq!(overlay.links.len(), 2, "both are drawn — the broken one most of all");
        let ladder = overlay.links.iter().find(|l| l.name == "ladder").unwrap();
        assert!(ladder.resolved && ladder.enabled);
        assert!(ladder.to[0] >= 9.0, "snapped onto the far floor: {:?}", ladder.to);
        let broken = overlay.links.iter().find(|l| l.name == "nowhere").unwrap();
        assert!(!broken.resolved, "an end 400 m off the mesh has not resolved");
    }

    /// Painted ground carries its area onto the picture, or a volume that did
    /// nothing looks exactly like one that worked.
    #[test]
    fn painted_ground_is_drawn_as_its_own_kind() {
        let s = NavSettings { agent_radius: 0.0, cell_size: 0.25, ..Default::default() };
        let tris = quad(0.0, 8.0, 0.0, 8.0, 0.0);
        // A band of something over the far half of the floor.
        let (centre, half) = ([4.0f32, 0.0, 6.0], [4.0f32, 2.0, 2.0]);
        let mut m = [0.0f32; 16];
        for i in 0..3 {
            m[i * 5] = 1.0 / half[i];
            m[12 + i] = -centre[i] / half[i];
        }
        m[15] = 1.0;
        let vol = crate::AreaVolume { inverse: m, area: 1, blocks: false };
        let mesh = crate::bake_with(&tris, &s, &[vol], Vec::new())
            .unwrap()
            .with_areas(vec![crate::Area::walkable(), crate::Area::new("mud", 4.0)]);

        let overlay = Overlay::build(&mesh, 0.05);
        assert!(
            overlay.tris.iter().any(|t| t.area == 1),
            "the painted half is not drawn as painted"
        );
        assert!(overlay.tris.iter().any(|t| t.area == 0), "and the rest is still plain ground");
    }

    /// One axis-aligned slab of ground, as two triangles.
    fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32) -> Vec<Tri> {
        vec![
            Tri::new([x0, y, z0], [x1, y, z0], [x0, y, z1]),
            Tri::new([x1, y, z0], [x1, y, z1], [x0, y, z1]),
        ]
    }

    /// A floor with a square hole in the middle — the smallest shape the greedy
    /// cut cannot express as one rectangle, so it is the smallest shape that
    /// shows the problem this module exists to fix. (A plain floor bakes into a
    /// single rectangle and looks fine either way, which is exactly why the bug
    /// survived: every simple test scene hid it.)
    fn floor_with_a_hole() -> Vec<Tri> {
        let mut t = quad(0.0, 12.0, 0.0, 4.0, 0.0);
        t.extend(quad(0.0, 12.0, 8.0, 12.0, 0.0));
        t.extend(quad(0.0, 4.0, 4.0, 8.0, 0.0));
        t.extend(quad(8.0, 12.0, 4.0, 8.0, 0.0));
        t
    }

    /// Four strips of ground, each `rise` above the last — a staircase. Adjacent
    /// strips are within a normal step of each other but the whole flight is
    /// not, so the bake cannot swallow it into one rectangle and the links
    /// between the strips are real.
    fn staircase(rise: f32) -> Vec<Tri> {
        let mut t = Vec::new();
        for i in 0..4 {
            let x0 = i as f32 * 3.0;
            t.extend(quad(x0, x0 + 3.0, 0.0, 6.0, i as f32 * rise));
        }
        t
    }

    fn plan_length(edges: &[Edge]) -> f32 {
        edges
            .iter()
            .map(|e| {
                let (dx, dz) = (e.b[0] - e.a[0], e.b[2] - e.a[2]);
                (dx * dx + dz * dz).sqrt()
            })
            .sum()
    }

    /// The change this module exists for: one floor reads as one shape.
    ///
    /// The bake cuts a plain floor into whatever rectangles the greedy pass
    /// happens to produce. Outlining each of them is what made the Scene view
    /// look like scattered boxes; the outline must instead be the edge of the
    /// floor and nothing else.
    #[test]
    fn one_floor_has_one_outline_however_many_rectangles_it_was_cut_into() {
        let mesh = bake(&floor_with_a_hole(), &NavSettings::default()).unwrap();
        let ov = Overlay::build(&mesh, 0.05);

        assert!(mesh.polys.len() > 1, "this shape must fragment or it tests nothing");
        assert!(!ov.tris.is_empty(), "a floor must be drawn as a surface");
        assert_eq!(ov.tris.len(), mesh.polys.len() * 2);

        // Every rectangle still has its four sides available for the cells view.
        assert_eq!(ov.cells.len(), mesh.polys.len() * 4);

        // …but the outline is only where the floor ends: its outer edge and the
        // edge of the hole. The seams where the greedy cut happened to divide
        // one continuous floor are interior, and interior edges are not drawn.
        let (drawn, naive) = ov.outline_saving();
        assert!(drawn < naive, "nothing was merged: {drawn} of {naive}");

        // The real measure is length, not count — one merged run replaces many
        // segments. The floor is 12x12 with a 4x4 hole, so the true outline is
        // about 48 + 16 = 64m before erosion pulls it in; every rectangle's own
        // perimeter added up is far more than that.
        let outline = plan_length(&ov.boundary);
        let every_rectangle = plan_length(&ov.cells);
        assert!(
            outline < every_rectangle * 0.8,
            "outline {outline} is not meaningfully less than {every_rectangle}"
        );
        assert!((40.0..90.0).contains(&outline), "outline {outline} is not the floor's edge");
    }

    /// Two floors a character cannot get between must look like two floors.
    #[test]
    fn ground_that_is_not_connected_keeps_its_own_outline() {
        let mut tris = floor(4.0, 0.0);
        // A second slab, well out of reach both across and up.
        for t in floor(4.0, 6.0) {
            tris.push(Tri::new(
                [t.a[0] + 20.0, t.a[1], t.a[2]],
                [t.b[0] + 20.0, t.b[1], t.b[2]],
                [t.c[0] + 20.0, t.c[1], t.c[2]],
            ));
        }
        let mesh = bake(&tris, &NavSettings::default()).unwrap();
        let ov = Overlay::build(&mesh, 0.05);

        let regions: HashSet<u32> = ov.boundary.iter().map(|e| e.region).collect();
        assert_eq!(regions.len(), 2, "two islands, two outlines");
        // Nothing joins them, so nothing is drawn as joining them.
        assert!(ov.steps.is_empty());
    }

    /// The setting has to be visible in the picture, which is the point of
    /// drawing connections at all: a lip the character steps over is drawn as a
    /// connection, and the same lip once it is too tall is not.
    #[test]
    fn a_step_is_drawn_as_a_connection_only_while_it_can_be_stepped_over() {
        let tris = staircase(0.3);
        let at = |step_height: f32| {
            let s = NavSettings { step_height, ..Default::default() };
            let mesh = bake(&tris, &s).unwrap();
            Overlay::build(&mesh, 0.05)
        };

        // A 0.3m rise with a 0.4m step: the character climbs it, so the picture
        // says so.
        let joined = at(0.4);
        assert!(!joined.steps.is_empty(), "a step it can climb must be drawn as a connection");
        for s in &joined.steps {
            // The rise is between the two *rectangles'* surfaces, not between
            // two stair treads: a rectangle may already span a step's worth of
            // height, so one ribbon can bridge more than one tread. That is what
            // is actually being drawn, so that is what is measured.
            assert!((0.2..1.0).contains(&s.rise), "rise {} is not a step", s.rise);
            assert!(s.high[0][1] > s.low[0][1], "the high side must be the high side");
            // The ribbon spans the portal it belongs to, so it is a surface
            // rather than a hairline.
            let width = (s.low[1][0] - s.low[0][0]).hypot(s.low[1][2] - s.low[0][2]);
            assert!(width > 0.0, "a connection with no width is not a connection");
        }

        // Shrink the step below the rise and the connection leaves the picture,
        // because it has left the mesh — which is the whole point: the setting
        // is visible in the scene rather than only in a number.
        assert!(at(0.1).steps.is_empty(), "a lip it cannot climb must not be drawn as joined");
    }

    /// The same reactivity, through the other setting a designer reaches for.
    /// A ramp at 30° is one connected run at the default slope limit and two
    /// separated elevations once the limit drops below it.
    #[test]
    fn the_slope_limit_shows_up_as_ground_joining_or_not() {
        // A 30° ramp between two floors.
        let mut tris = quad(0.0, 4.0, 0.0, 6.0, 0.0);
        tris.extend([
            Tri::new([4.0, 0.0, 0.0], [7.46, 2.0, 0.0], [4.0, 0.0, 6.0]),
            Tri::new([7.46, 2.0, 0.0], [7.46, 2.0, 6.0], [4.0, 0.0, 6.0]),
        ]);
        tris.extend(quad(7.46, 11.0, 0.0, 6.0, 2.0));

        let regions = |max_slope: f32| {
            let s = NavSettings { max_slope, ..Default::default() };
            let mesh = bake(&tris, &s).unwrap();
            mesh.polys.iter().map(|p| p.region).collect::<HashSet<u32>>().len()
        };
        assert_eq!(regions(45.0), 1, "a 30° ramp is walkable at 45° and joins the two floors");
        assert!(regions(20.0) > 1, "below the ramp's angle the two floors must come apart");
    }

    /// Flat ground must not sprout ribbons from floating-point noise in the
    /// per-rectangle mean height.
    #[test]
    fn a_flat_floor_has_no_step_ribbons_at_all() {
        let mesh = bake(&floor(10.0, 0.0), &NavSettings::default()).unwrap();
        assert!(Overlay::build(&mesh, 0.05).steps.is_empty());
    }

    /// A walkway over a floor touches it in plan and is not connected to it.
    /// Adjacency must therefore never be what suppresses an edge.
    #[test]
    fn a_bridge_over_a_floor_keeps_the_edge_between_them() {
        let mut tris = floor(8.0, 0.0);
        // A narrow deck 3m up, over the middle of it.
        tris.extend([
            Tri::new([2.0, 3.0, 0.0], [6.0, 3.0, 0.0], [2.0, 3.0, 8.0]),
            Tri::new([6.0, 3.0, 0.0], [6.0, 3.0, 8.0], [2.0, 3.0, 8.0]),
        ]);
        let mesh = bake(&tris, &NavSettings::default()).unwrap();
        let ov = Overlay::build(&mesh, 0.05);
        // The deck is its own surface with its own edge; nothing about it being
        // over the floor may erase either outline.
        let heights: Vec<f32> = ov.boundary.iter().map(|e| e.a[1]).collect();
        assert!(
            heights.iter().any(|h| *h > 2.0) && heights.iter().any(|h| *h < 1.0),
            "both levels must be outlined: {heights:?}"
        );
    }

    #[test]
    fn an_empty_mesh_draws_nothing_rather_than_panicking() {
        let mut mesh = bake(&floor(4.0, 0.0), &NavSettings::default()).unwrap();
        mesh.polys.clear();
        mesh.links.clear();
        let ov = Overlay::build(&mesh, 0.05);
        assert!(ov.tris.is_empty() && ov.boundary.is_empty() && ov.steps.is_empty());
    }

    /// The lift is what stops the overlay z-fighting the floor it describes, so
    /// it has to actually reach every part of it.
    #[test]
    fn everything_drawn_is_lifted_off_the_ground_it_describes() {
        let mesh = bake(&floor(6.0, 0.0), &NavSettings::default()).unwrap();
        let plain = Overlay::build(&mesh, 0.0);
        let lifted = Overlay::build(&mesh, 0.25);
        assert_eq!(plain.tris.len(), lifted.tris.len());
        for (p, l) in plain.tris.iter().zip(&lifted.tris) {
            assert!((l.a[1] - p.a[1] - 0.25).abs() < 1e-5);
        }
        for (p, l) in plain.boundary.iter().zip(&lifted.boundary) {
            assert!((l.a[1] - p.a[1] - 0.25).abs() < 1e-5);
        }
    }
}
