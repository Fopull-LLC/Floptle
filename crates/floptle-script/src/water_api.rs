//! `water.*` — what a script asks about the wet parts of the world
//! (`floptle/0038`).
//!
//! The engine owns the volume, the buoyancy and the drag. What a game still has
//! to decide is everything *meaningful*: whether the player is swimming or
//! drowning, whether the engine floods, whether a gauge reads red, whether the
//! music ducks. All of those are the same question with different answers —
//! "how deep is this point" — so that is the one thing this module exports, and
//! everything else is derived from it in Lua where it belongs.
//!
//! Before this existed the game re-derived the geometry: `climate.lua` carried
//! its own `seaDepth(x,y,z)` against a sea radius it had to keep in step with
//! the sphere the generator drew, and `planet_walker.lua` implemented swimming
//! as a controller because a character is one capsule. The first of those is
//! now one call; the second still belongs to the game, but it no longer has to
//! agree with the renderer by hand.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};

/// One body of water, mirrored for scripts each frame.
#[derive(Clone, Copy, Debug)]
pub struct WaterInfo {
    pub entity: u32,
    /// `true` for a sphere sea, `false` for a box pool.
    pub sea: bool,
    /// World-space centre.
    pub center: [f64; 3],
    /// Sea: radius. Pool: unused.
    pub radius: f64,
    /// Pool: half-extents. Sea: unused.
    pub half: [f64; 3],
    /// Pool: orientation as a quaternion (xyzw). Sea: identity.
    pub rot: [f64; 4],
    pub density: f32,
    pub frozen: bool,
}

impl WaterInfo {
    /// Metres below this volume's surface at world point `p`; 0 outside. The
    /// same rule the solver uses — one definition of "in the water", so a
    /// script's swim state can never disagree with the physics that floats it.
    pub fn depth_at(&self, p: [f64; 3]) -> f64 {
        if self.frozen {
            return 0.0;
        }
        let d = glam::DVec3::from(p) - glam::DVec3::from(self.center);
        if self.sea {
            (self.radius - d.length()).max(0.0)
        } else {
            let q = glam::DQuat::from_xyzw(self.rot[0], self.rot[1], self.rot[2], self.rot[3]);
            let local = q.inverse() * d;
            let h = glam::DVec3::from(self.half);
            if local.x.abs() > h.x || local.z.abs() > h.z || local.y < -h.y {
                return 0.0;
            }
            (h.y - local.y).max(0.0)
        }
    }
}

/// The shared state the `water.*` calls read and write.
#[derive(Clone)]
pub(crate) struct WaterShared {
    /// Every volume in the scene, refreshed by the driver before scripts run.
    pub volumes: Rc<RefCell<Vec<WaterInfo>>>,
    /// `water.setFrozen(node, bool)` requests, drained by the driver.
    pub freeze: Rc<RefCell<Vec<(u32, bool)>>>,
}

/// The deepest volume containing `p`, and how deep — the same "innermost wins"
/// rule the solver uses, so a tank inside an ocean answers as the tank on both
/// sides.
fn deepest(vols: &[WaterInfo], p: [f64; 3]) -> Option<(&WaterInfo, f64)> {
    let mut best: Option<(&WaterInfo, f64)> = None;
    for v in vols {
        let d = v.depth_at(p);
        if d > 0.0 && best.is_none_or(|(_, bd)| d > bd) {
            best = Some((v, d));
        }
    }
    best
}

/// Read a `(x, y, z)` triple or a single vec3/node from the argument list.
fn point_of(a: &[Value]) -> Option<[f64; 3]> {
    if let Some(v) = a.first().and_then(crate::math_api::vec3_of) {
        return Some([v.x, v.y, v.z]);
    }
    let num = |v: Option<&Value>| -> Option<f64> {
        match v {
            Some(Value::Number(n)) => Some(*n),
            Some(Value::Integer(i)) => Some(*i as f64),
            _ => None,
        }
    };
    Some([num(a.first())?, num(a.get(1))?, num(a.get(2))?])
}

pub(crate) fn install_water_api(lua: &Lua, shared: WaterShared) {
    let Ok(t) = lua.create_table() else { return };

    // water.depthAt(x, y, z) / water.depthAt(vec3) → metres below the surface,
    // 0 in air. The one number everything else is derived from.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let Some(p) = point_of(&a) else {
                return Err(mlua::Error::RuntimeError(
                    "water.depthAt(x, y, z) — or a vec3, or a node".into(),
                ));
            };
            Ok(deepest(&s.volumes.borrow(), p).map(|(_, d)| d).unwrap_or(0.0))
        }) {
            let _ = t.set("depthAt", f);
        }
    }

    // water.at(x, y, z) → nil in air, else a table describing the water there.
    // Separate from `depthAt` because the cheap question is asked every frame by
    // every character and the detailed one is not.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let Some(p) = point_of(&a) else {
                return Err(mlua::Error::RuntimeError(
                    "water.at(x, y, z) — or a vec3, or a node".into(),
                ));
            };
            let vols = s.volumes.borrow();
            let Some((v, d)) = deepest(&vols, p) else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            out.set("depth", d)?;
            out.set("density", v.density as f64)?;
            out.set("frozen", v.frozen)?;
            out.set("node", crate::env::new_node_handle(lua, v.entity)?)?;
            // "Up" out of the water: radial on a sea, the pool's own +Y. What a
            // swim controller pushes along, and it is NOT −gravity in a tilted
            // tank.
            let up = if v.sea {
                (glam::DVec3::from(p) - glam::DVec3::from(v.center))
                    .try_normalize()
                    .unwrap_or(glam::DVec3::Y)
            } else {
                glam::DQuat::from_xyzw(v.rot[0], v.rot[1], v.rot[2], v.rot[3]) * glam::DVec3::Y
            };
            out.set("up", crate::math_api::LuaVec3(up))?;
            Ok(Value::Table(out))
        }) {
            let _ = t.set("at", f);
        }
    }

    // water.isUnderwater(x, y, z) → the yes/no a script asks most.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let Some(p) = point_of(&a) else {
                return Err(mlua::Error::RuntimeError(
                    "water.isUnderwater(x, y, z) — or a vec3, or a node".into(),
                ));
            };
            Ok(deepest(&s.volumes.borrow(), p).is_some())
        }) {
            let _ = t.set("isUnderwater", f);
        }
    }

    // water.setFrozen(node, frozen) — freezing is a STATE, not a second system.
    // A world that thaws is the same node with a flag flipped, and the physics
    // and the look both follow from it.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (node, on): (Value, bool)| {
            let eid = match &node {
                Value::Table(t) => t.raw_get::<u32>("__id").ok(),
                Value::Integer(i) => Some(*i as u32),
                Value::Number(n) => Some(*n as u32),
                _ => None,
            };
            let Some(eid) = eid else {
                return Err(mlua::Error::RuntimeError(
                    "water.setFrozen(node, frozen) — the first argument is a water node".into(),
                ));
            };
            s.freeze.borrow_mut().push((eid, on));
            Ok(())
        }) {
            let _ = t.set("setFrozen", f);
        }
    }

    // water.volumes() → every body of water in the scene, as nodes. What a
    // climate system iterates when it wants to know where the seas are.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (i, v) in s.volumes.borrow().iter().enumerate() {
                out.set(i + 1, crate::env::new_node_handle(lua, v.entity)?)?;
            }
            Ok(out)
        }) {
            let _ = t.set("volumes", f);
        }
    }

    let _ = lua.globals().set("water", t);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sea() -> WaterInfo {
        WaterInfo {
            entity: 1,
            sea: true,
            center: [0.0; 3],
            radius: 100.0,
            half: [0.0; 3],
            rot: [0.0, 0.0, 0.0, 1.0],
            density: 1000.0,
            frozen: false,
        }
    }

    fn pool() -> WaterInfo {
        WaterInfo {
            entity: 2,
            sea: false,
            center: [0.0, 0.0, 0.0],
            radius: 0.0,
            half: [5.0, 2.0, 5.0],
            rot: [0.0, 0.0, 0.0, 1.0],
            density: 1000.0,
            frozen: false,
        }
    }

    /// The script-side answer must be the SAME rule the solver uses. A game
    /// whose swim state disagreed with the physics floating it is the exact
    /// bug this API exists to remove — `climate.lua` re-deriving a sea radius
    /// it had to keep in step with the sphere the generator drew.
    #[test]
    fn depth_matches_the_solvers_rule() {
        let v = sea();
        assert_eq!(v.depth_at([0.0, 150.0, 0.0]), 0.0);
        assert_eq!(v.depth_at([0.0, 100.0, 0.0]), 0.0, "the surface is not under it");
        assert!((v.depth_at([0.0, 90.0, 0.0]) - 10.0).abs() < 1e-9);
    }

    /// A pool's sides are walls, in Lua as in the solver.
    #[test]
    fn a_pool_has_edges() {
        let v = pool();
        assert!(v.depth_at([0.0, 0.0, 0.0]) > 0.0);
        assert_eq!(v.depth_at([40.0, 0.0, 0.0]), 0.0, "beside it, same height");
        assert_eq!(v.depth_at([0.0, 3.0, 0.0]), 0.0, "above the surface");
    }

    /// Overlapping volumes resolve by depth, not by scene order.
    #[test]
    fn the_deepest_volume_answers() {
        let mut tank = pool();
        tank.half = [2.0, 3.0, 2.0];
        tank.density = 1300.0;
        let p = [0.0, -1.0, 0.0];
        let a = vec![tank, sea()];
        let b = vec![sea(), tank];
        assert_eq!(
            deepest(&a, p).map(|(v, _)| v.entity),
            deepest(&b, p).map(|(v, _)| v.entity),
            "order changed the answer"
        );
    }

    /// A frozen sea has no inside. A swim controller must not think the player
    /// is treading water on an ice sheet.
    #[test]
    fn a_frozen_sea_is_not_water() {
        let mut v = sea();
        v.frozen = true;
        assert_eq!(v.depth_at([0.0, 50.0, 0.0]), 0.0);
        assert!(deepest(&[v], [0.0, 50.0, 0.0]).is_none());
    }
}
