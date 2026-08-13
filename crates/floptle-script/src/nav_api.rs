//! `nav.*` — asking the scene's navmesh how to get somewhere.
//!
//! Three functions, because there are only three questions worth asking:
//! whether there is a navmesh at all, how to walk from here to there, and where
//! the nearest walkable ground is.
//!
//! Everything here is in **world coordinates**. The bake itself is measured
//! around its own node so that a level a million units out stays exact, and
//! that offset is the mesh's business rather than a script's.
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

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

use crate::math_api::{vec3_of, LuaVec3};

/// The scene's baked navmesh, shared with whoever loaded it.
///
/// `None` until a scene with a bake is open, which is the ordinary state of a
/// project that has not made one yet rather than an error.
pub type NavShared = Rc<RefCell<Option<floptle_nav::NavMesh>>>;

/// How far off the mesh an end may be before the answer is "not on it".
///
/// A character's own height, taken from the bake's settings: standing on top of
/// the floor rather than exactly in it is the normal case, and so is being half
/// a step off the edge of a ledge.
fn snap(mesh: &floptle_nav::NavMesh) -> f32 {
    mesh.settings.agent_height.max(0.1)
}

pub fn install_nav_api(lua: &Lua, mesh: NavShared) {
    let Ok(t) = lua.create_table() else { return };

    // nav.ready() — whether this scene has a navmesh to ask.
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, ()| Ok(m.borrow().is_some())) {
        let _ = t.set("ready", f);
    }

    // nav.path(from, to) -> {vec3...}, complete
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |lua, (from, to): (Value, Value)| {
        let (Some(a), Some(b)) = (vec3_of(&from), vec3_of(&to)) else {
            return Ok((None, None));
        };
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok((None, None)) };
        let s = snap(mesh);
        let Some(path) =
            mesh.path_within(mesh.to_local([a.x, a.y, a.z]), mesh.to_local([b.x, b.y, b.z]), s)
        else {
            return Ok((None, None));
        };
        let out = lua.create_table()?;
        for (i, p) in path.points.iter().enumerate() {
            let w = mesh.to_world(*p);
            out.set(i + 1, LuaVec3(glam::DVec3::new(w[0], w[1], w[2])))?;
        }
        Ok((Some(out), Some(path.complete)))
    }) {
        let _ = t.set("path", f);
    }

    // nav.nearest(point[, maxDistance]) -> vec3 | nil
    let m = mesh.clone();
    if let Ok(f) = lua.create_function(move |_, (at, max): (Value, Option<f64>)| {
        let Some(p) = vec3_of(&at) else { return Ok(None) };
        let guard = m.borrow();
        let Some(mesh) = guard.as_ref() else { return Ok(None) };
        let limit = max.map(|d| d as f32).unwrap_or_else(|| snap(mesh));
        let Some((_, on)) = mesh.nearest(mesh.to_local([p.x, p.y, p.z]), limit) else {
            return Ok(None);
        };
        let w = mesh.to_world(on);
        Ok(Some(LuaVec3(glam::DVec3::new(w[0], w[1], w[2]))))
    }) {
        let _ = t.set("nearest", f);
    }

    let _ = lua.globals().set("nav", t);
}
