//! `nav.*` — asking the scene's navmesh where a character can go.
//!
//! Everything here is in **world coordinates**. The bake itself is measured
//! around its own node so that a level a million units out stays exact, and that
//! offset is the mesh's business rather than a script's.
//!
//! ```lua
//! local route = nav.path(self.node.position, target.position)
//! if route then
//!     for _, point in ipairs(route) do walkTo(point) end
//! end
//! ```
//!
//! # Getting nil back
//!
//! `nav.path` answers `nil` when an end is not on the navmesh — off the edge of
//! the level, or inside a wall. That is a different thing from a goal that is on
//! the mesh but cut off, which comes back as a real route to the nearest
//! reachable point with a second return value of `false`. A character that walks
//! to the near side of a chasm and stops is behaving; one that stands still
//! because the answer was empty looks broken.
//!
//! # Why the shape data comes back as a flat array
//!
//! [`nav.areas`](install_nav_api) and `nav.links` hand back **one array of
//! numbers**, not an array of tables. A real bake is thousands of polygons — the
//! scene this was built against has 1,640 — and mlua keeps held Lua values in a
//! fixed pool of a few thousand auxiliary slots. A table per polygon exhausts
//! that pool and `create_table` *panics*: not an error a script can handle, the
//! whole editor. One array of numbers costs one slot however big the level is.
//!
//! It is a worse thing to read and a thing that works, which is the correct
//! trade for a function whose whole purpose is bulk. The stride is a constant
//! (`nav.AREA_STRIDE`) so the arithmetic is written once:
//!
//! ```lua
//! local a, n = nav.areas()
//! for i = 0, n - 1 do
//!     local o = i * nav.AREA_STRIDE
//!     local minX, minZ, maxX, maxZ = a[o+1], a[o+2], a[o+3], a[o+4]
//!     local yMin, yMax, region     = a[o+5], a[o+6], a[o+7]
//! end
//! ```

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

use crate::math_api::{vec3_of, LuaVec3};

/// The scene's baked navmesh, shared with whoever loaded it.
///
/// `None` until a scene with a bake is open, which is the ordinary state of a
/// project that has not made one yet rather than an error.
pub type NavShared = Rc<RefCell<Option<floptle_nav::NavMesh>>>;

/// Numbers per area in the flat array `nav.areas()` returns.
///
/// `minX minZ maxX maxZ yMin yMax region centreX centreY centreZ`
pub const AREA_STRIDE: usize = 10;

/// Numbers per link in the flat array `nav.links()` returns.
///
/// `from to leftX leftY leftZ rightX rightY rightZ`
pub const LINK_STRIDE: usize = 8;

/// How far off the mesh an end may be before the answer is "not on it".
///
/// A character's own height, taken from the bake's settings: standing on top of
/// the floor rather than exactly in it is the normal case, and so is being half
/// a step off the edge of a ledge.
fn snap(mesh: &floptle_nav::NavMesh) -> f32 {
    mesh.settings.agent_height.max(0.1)
}

/// A world point out of a Lua value, in the mesh's own frame.
fn local_of(mesh: &floptle_nav::NavMesh, v: &Value) -> Option<[f32; 3]> {
    vec3_of(v).map(|p| mesh.to_local([p.x, p.y, p.z]))
}

fn world_vec(mesh: &floptle_nav::NavMesh, local: [f32; 3]) -> LuaVec3 {
    let w = mesh.to_world(local);
    LuaVec3(glam::DVec3::new(w[0], w[1], w[2]))
}

pub fn install_nav_api(lua: &Lua, mesh: NavShared) {
    let Ok(t) = lua.create_table() else { return };

    let _ = t.set("AREA_STRIDE", AREA_STRIDE);
    let _ = t.set("LINK_STRIDE", LINK_STRIDE);

    // nav.ready() — whether this scene has a navmesh to ask.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, ()| Ok(m.borrow().is_some())) {
        let _ = t.set("ready", f);
    }

    // nav.path(from, to) -> {vec3...}, complete
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |lua, (from, to): (Value, Value)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok((None, None)) };
        let (Some(a), Some(b)) = (local_of(mesh, &from), local_of(mesh, &to)) else {
            return Ok((None, None));
        };
        let Some(path) = mesh.path_within(a, b, snap(mesh)) else {
            return Ok((None, None));
        };
        let out = lua.create_table()?;
        for (i, p) in path.points.iter().enumerate() {
            out.set(i + 1, world_vec(mesh, *p))?;
        }
        Ok((Some(out), Some(path.complete)))
    }) {
        let _ = t.set("path", f);
    }

    // nav.nearest(point[, maxDistance]) -> vec3 | nil
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (at, max): (Value, Option<f64>)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let Some(p) = local_of(mesh, &at) else { return Ok(None) };
        let limit = max.map(|d| d as f32).unwrap_or_else(|| snap(mesh));
        let Some((_, on)) = mesh.nearest(p, limit) else { return Ok(None) };
        Ok(Some(world_vec(mesh, on)))
    }) {
        let _ = t.set("nearest", f);
    }

    // nav.onMesh(point[, tolerance]) -> bool
    //
    // The allocation-free version of nav.nearest, for the per-frame check
    // ("am I still on the floor?") that does not want the point back.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (at, tol): (Value, Option<f64>)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(false) };
        let Some(p) = local_of(mesh, &at) else { return Ok(false) };
        let limit = tol.map(|d| d as f32).unwrap_or_else(|| snap(mesh));
        Ok(mesh.nearest(p, limit).is_some())
    }) {
        let _ = t.set("onMesh", f);
    }

    // nav.regionOf(point[, tolerance]) -> id | nil
    //
    // Two points in different regions can never be walked between. One integer
    // compare rules out a search that was never going to succeed.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (at, tol): (Value, Option<f64>)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let Some(p) = local_of(mesh, &at) else { return Ok(None) };
        let limit = tol.map(|d| d as f32).unwrap_or_else(|| snap(mesh));
        Ok(mesh.region_at(p, limit))
    }) {
        let _ = t.set("regionOf", f);
    }

    // nav.reachable(from, to) -> bool
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (from, to): (Value, Value)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(false) };
        let (Some(a), Some(b)) = (local_of(mesh, &from), local_of(mesh, &to)) else {
            return Ok(false);
        };
        Ok(mesh.reachable(a, b, snap(mesh)))
    }) {
        let _ = t.set("reachable", f);
    }

    // nav.distance(from, to) -> metres | nil
    //
    // How far it is to WALK, which is the number a decision is made on — the
    // straight-line distance to something on the far side of a wall is a lie
    // that makes every "chase the nearest one" pick the wrong one.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (from, to): (Value, Value)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let (Some(a), Some(b)) = (local_of(mesh, &from), local_of(mesh, &to)) else {
            return Ok(None);
        };
        Ok(mesh
            .path_within(a, b, snap(mesh))
            .filter(|p| p.complete)
            .map(|p| p.length() as f64))
    }) {
        let _ = t.set("distance", f);
    }

    // nav.raycast(from, to) -> vec3 | nil
    //
    // nil means the whole line is walkable. A point means the walk leaves the
    // navmesh there — the walker's answer, not the collider's: a ledge this
    // character would fall off is empty air to a physics ray and a wall to this.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (from, to): (Value, Value)| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let (Some(a), Some(b)) = (local_of(mesh, &from), local_of(mesh, &to)) else {
            return Ok(None);
        };
        Ok(mesh.raycast(a, b, snap(mesh)).map(|hit| world_vec(mesh, hit)))
    }) {
        let _ = t.set("raycast", f);
    }

    // nav.random(u, v[, near, radius]) -> vec3 | nil
    //
    // The two random numbers come from the CALLER — `nav.random(math.random(),
    // math.random())`. This engine rolls back and re-simulates, so a wander
    // destination has to come out of the same seeded stream as everything else
    // the tick decided; a navmesh that reached for its own randomness would
    // desync every rollback that touched it.
    let m = mesh.clone();
    if let Ok(f) =
        lua.create_function(move |_, (u, v, near, radius): (f64, f64, Option<Value>, Option<f64>)| {
            let guard = m.borrow();
            let Some(mesh) = guard.as_ref() else { return Ok(None) };
            let within = near
                .as_ref()
                .and_then(|n| local_of(mesh, n))
                .map(|c| (c, radius.unwrap_or(10.0) as f32));
            Ok(mesh.random_point(within, u as f32, v as f32).map(|p| world_vec(mesh, p)))
        })
    {
        let _ = t.set("random", f);
    }

    // nav.settings() -> table
    //
    // The character the mesh was baked for. A script that wants to move a body
    // along a path needs the radius it was eroded by, and guessing it is how a
    // character ends up scraping the wall the erosion existed to avoid.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let s = &mesh.settings;
        let t = lua.create_table()?;
        t.set("radius", s.agent_radius)?;
        t.set("height", s.agent_height)?;
        t.set("maxSlope", s.max_slope)?;
        t.set("stepHeight", s.step_height)?;
        t.set("cellSize", s.cell_size)?;
        t.set("areaCount", mesh.polys.len())?;
        t.set("area", mesh.area())?;
        Ok(Some(t))
    }) {
        let _ = t.set("settings", f);
    }

    // nav.areas() -> flat array, count
    //
    // See the module docs for why this is numbers and not tables.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok((None, 0usize)) };
        let out = lua.create_table_with_capacity(mesh.polys.len() * AREA_STRIDE, 0)?;
        let mut i = 1;
        for p in &mesh.polys {
            // World space, like everything else here — a script must never have
            // to know the bake had an anchor.
            let lo = mesh.to_world([p.min[0], p.y_min, p.min[1]]);
            let hi = mesh.to_world([p.max[0], p.y_max, p.max[1]]);
            let c = mesh.to_world(p.centre);
            for n in [
                lo[0], lo[2], hi[0], hi[2], lo[1], hi[1], p.region as f64, c[0], c[1], c[2],
            ] {
                out.set(i, n)?;
                i += 1;
            }
        }
        Ok((Some(out), mesh.polys.len()))
    }) {
        let _ = t.set("areas", f);
    }

    // nav.links() -> flat array, count
    //
    // Every portal, once per direction — so `from` is an index into the areas
    // array and the left/right endpoints are stated as somebody walking that
    // way sees them.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |lua, ()| {
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok((None, 0usize)) };
        let total: usize = mesh.links.iter().map(|l| l.len()).sum();
        let out = lua.create_table_with_capacity(total * LINK_STRIDE, 0)?;
        let mut i = 1;
        for (from, ls) in mesh.links.iter().enumerate() {
            for l in ls {
                let left = mesh.to_world(l.left);
                let right = mesh.to_world(l.right);
                // One-based, because everything a Lua script indexes is.
                for n in [
                    (from + 1) as f64,
                    (l.to + 1) as f64,
                    left[0],
                    left[1],
                    left[2],
                    right[0],
                    right[1],
                    right[2],
                ] {
                    out.set(i, n)?;
                    i += 1;
                }
            }
        }
        Ok((Some(out), total))
    }) {
        let _ = t.set("links", f);
    }

    let _ = lua.globals().set("nav", t);
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_nav::{NavSettings, Tri};

    /// One `nav` table over a floor with a hole in it, anchored a long way from
    /// the origin — because "everything is world space" is the property most
    /// likely to be quietly wrong, and it is only wrong when the anchor is not
    /// zero.
    fn scene() -> (Lua, NavShared) {
        let quad = |x0: f32, x1: f32, z0: f32, z1: f32, y: f32| {
            vec![
                Tri::new([x0, y, z0], [x1, y, z0], [x0, y, z1]),
                Tri::new([x1, y, z0], [x1, y, z1], [x0, y, z1]),
            ]
        };
        let mut tris = quad(0.0, 12.0, 0.0, 4.0, 0.0);
        tris.extend(quad(0.0, 12.0, 8.0, 12.0, 0.0));
        tris.extend(quad(0.0, 4.0, 4.0, 8.0, 0.0));
        tris.extend(quad(8.0, 12.0, 4.0, 8.0, 0.0));

        let mesh = floptle_nav::bake(&tris, &NavSettings::default())
            .expect("this floor bakes")
            .anchored_at([1_000_000.0, 0.0, -250_000.0]);

        let lua = Lua::new();
        let _ = crate::math_api::install(&lua);
        let shared: NavShared = Rc::new(RefCell::new(Some(mesh)));
        install_nav_api(&lua, shared.clone());
        (lua, shared)
    }

    fn eval<T: mlua::FromLuaMulti>(lua: &Lua, src: &str) -> T {
        lua.load(src).eval().unwrap_or_else(|e| panic!("{src}\n{e}"))
    }

    /// The whole reason this is a flat array: a real bake is thousands of
    /// polygons, and one Lua table each exhausts mlua's auxiliary slots and
    /// panics the editor. One array costs one slot however big the level is.
    #[test]
    fn areas_come_back_as_one_flat_array_in_world_space() {
        let (lua, shared) = scene();
        let (n, total): (usize, usize) = eval(
            &lua,
            "local a, n = nav.areas() return n, #a",
        );
        assert!(n > 1, "this floor must fragment or the test proves nothing");
        assert_eq!(total, n * AREA_STRIDE, "the array is exactly stride * count");

        // Every area must be inside the floor, offset by the anchor — which is
        // the check that would fail if any of this leaked bake-local space.
        let anchor_x = shared.borrow().as_ref().unwrap().anchor[0];
        let (min_x, max_x): (f64, f64) = eval(
            &lua,
            "local a, n = nav.areas()
             local lo, hi = math.huge, -math.huge
             for i = 0, n - 1 do
                 local o = i * nav.AREA_STRIDE
                 lo = math.min(lo, a[o + 1])
                 hi = math.max(hi, a[o + 3])
             end
             return lo, hi",
        );
        assert!(min_x > anchor_x - 1.0 && min_x < anchor_x + 2.0, "{min_x} vs {anchor_x}");
        assert!(hi_is_sane(max_x, anchor_x), "{max_x} vs {anchor_x}");

        // The region column is a real id, and the hole does not split this
        // floor — you can walk around it.
        let regions: usize = eval(
            &lua,
            "local a, n = nav.areas()
             local seen = {}
             for i = 0, n - 1 do seen[a[i * nav.AREA_STRIDE + 7]] = true end
             local c = 0 for _ in pairs(seen) do c = c + 1 end return c",
        );
        assert_eq!(regions, 1, "a floor with a hole is still one island");
    }

    fn hi_is_sane(max_x: f64, anchor_x: f64) -> bool {
        max_x > anchor_x + 9.0 && max_x < anchor_x + 13.0
    }

    #[test]
    fn links_name_areas_by_their_one_based_index() {
        let (lua, _) = scene();
        let (count, len): (usize, usize) = eval(&lua, "local l, n = nav.links() return n, #l");
        assert!(count > 0, "a fragmented floor has portals");
        assert_eq!(len, count * LINK_STRIDE);

        // Every `from`/`to` must index the areas array, one-based.
        let ok: bool = eval(
            &lua,
            "local _, areas = nav.areas()
             local l, n = nav.links()
             for i = 0, n - 1 do
                 local o = i * nav.LINK_STRIDE
                 local from, to = l[o + 1], l[o + 2]
                 if from < 1 or from > areas or to < 1 or to > areas then return false end
                 if from == to then return false end
             end
             return true",
        );
        assert!(ok, "a link named an area that does not exist");
    }

    /// The walker's answer, not the collider's: straight across the floor is
    /// clear, and straight across the hole is not.
    #[test]
    fn a_raycast_is_about_walking_rather_than_about_geometry() {
        let (lua, shared) = scene();
        let a = shared.borrow().as_ref().unwrap().anchor;
        lua.globals().set("ax", a[0]).unwrap();
        lua.globals().set("az", a[2]).unwrap();

        let clear: bool = eval(
            &lua,
            "return nav.raycast(vec3(ax + 1, 0, az + 2), vec3(ax + 11, 0, az + 2)) == nil",
        );
        assert!(clear, "along the solid strip nothing blocks the walk");

        let blocked: Option<f64> = eval(
            &lua,
            "local hit = nav.raycast(vec3(ax + 6, 0, az + 1), vec3(ax + 6, 0, az + 11))
             if hit == nil then return nil end
             return hit.z - az",
        );
        let z = blocked.expect("walking through the hole must stop");
        assert!((2.0..6.0).contains(&z), "it should stop at the near lip of the hole: {z}");
    }

    #[test]
    fn the_cheap_questions_answer_without_a_search() {
        let (lua, shared) = scene();
        let a = shared.borrow().as_ref().unwrap().anchor;
        lua.globals().set("ax", a[0]).unwrap();
        lua.globals().set("az", a[2]).unwrap();

        assert!(eval::<bool>(&lua, "return nav.onMesh(vec3(ax + 2, 0, az + 2))"));
        assert!(!eval::<bool>(&lua, "return nav.onMesh(vec3(ax + 6, 0, az + 6))"),
                "the middle of the hole is not walkable");
        assert!(!eval::<bool>(&lua, "return nav.onMesh(vec3(ax + 500, 0, az))"));

        assert!(eval::<bool>(&lua, "return nav.regionOf(vec3(ax + 2, 0, az + 2)) ~= nil"));
        assert!(eval::<bool>(&lua, "return nav.regionOf(vec3(ax + 500, 0, az)) == nil"));

        assert!(eval::<bool>(
            &lua,
            "return nav.reachable(vec3(ax + 1, 0, az + 1), vec3(ax + 11, 0, az + 11))"
        ));
        assert!(!eval::<bool>(
            &lua,
            "return nav.reachable(vec3(ax + 1, 0, az + 1), vec3(ax + 500, 0, az))"
        ));

        // Walking round the hole is further than the straight line through it,
        // which is the entire reason this function exists.
        let (walk, straight): (f64, f64) = eval(
            &lua,
            "local a, b = vec3(ax + 1, 0, az + 6), vec3(ax + 11, 0, az + 6)
             return nav.distance(a, b), (b - a):length()",
        );
        assert!(walk > straight + 1.0, "walk {walk} should exceed the straight {straight}");
    }

    /// The randomness is the caller's, because this engine rolls back: the same
    /// two numbers must always give the same point, or every re-simulation that
    /// touched a wander desyncs.
    #[test]
    fn a_random_point_is_repeatable_and_lands_on_the_mesh() {
        let (lua, _) = scene();
        assert!(eval::<bool>(
            &lua,
            "local a = nav.random(0.37, 0.62)
             local b = nav.random(0.37, 0.62)
             return a ~= nil and (a - b):length() < 1e-6"
        ));
        assert!(eval::<bool>(&lua, "return nav.onMesh(nav.random(0.37, 0.62))"));
        assert!(eval::<bool>(
            &lua,
            "return (nav.random(0.1, 0.5) - nav.random(0.9, 0.5)):length() > 0.5"
        ));
    }

    #[test]
    fn settings_describe_the_character_the_mesh_was_baked_for() {
        let (lua, _) = scene();
        let (r, h, slope, areas): (f64, f64, f64, usize) = eval(
            &lua,
            "local s = nav.settings() return s.radius, s.height, s.maxSlope, s.areaCount",
        );
        assert!((r - 0.5).abs() < 1e-5);
        assert!((h - 2.0).abs() < 1e-5);
        assert!((slope - 45.0).abs() < 1e-5);
        assert_eq!(areas, eval::<usize>(&lua, "local _, n = nav.areas() return n"));
    }

    /// A project that has not baked anything is the ordinary state of a new
    /// project, not an error — every function must answer rather than raise.
    #[test]
    fn a_scene_with_no_bake_answers_every_question_with_nothing() {
        let lua = Lua::new();
        let _ = crate::math_api::install(&lua);
        install_nav_api(&lua, Rc::new(RefCell::new(None)));
        assert!(!eval::<bool>(&lua, "return nav.ready()"));
        assert!(eval::<bool>(
            &lua,
            "return nav.path(vec3(0,0,0), vec3(1,0,1)) == nil
                and nav.nearest(vec3(0,0,0)) == nil
                and nav.onMesh(vec3(0,0,0)) == false
                and nav.regionOf(vec3(0,0,0)) == nil
                and nav.reachable(vec3(0,0,0), vec3(1,0,1)) == false
                and nav.distance(vec3(0,0,0), vec3(1,0,1)) == nil
                and nav.raycast(vec3(0,0,0), vec3(1,0,1)) == nil
                and nav.random(0.5, 0.5) == nil
                and nav.settings() == nil"
        ));
        let (areas, n): (Option<mlua::Table>, usize) =
            eval(&lua, "local a, n = nav.areas() return a, n");
        assert!(areas.is_none() && n == 0);
    }
}
