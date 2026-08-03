//! Shape queries — `overlapSphere`, `spherecast`, `capsulecast` (roadmap B2).
//!
//! `raycast` answers "what is along this line". A combat game asks a different
//! question — "what is inside this volume" — and until now had to fake it with
//! a fan of rays, which misses anything thinner than the fan and cannot report
//! penetration depth at all.
//!
//! Two things make these belong here rather than in a game:
//!
//! * **They are nearly free in this engine.** Every collider already answers a
//!   signed distance, so an overlap is `d(center) < radius` and a swept sphere
//!   is the ray march with the radius subtracted. No new geometry kernels.
//! * **Lag compensation.** Inside `net.rewind` the lent hulls ARE the rewound
//!   ones, so an overlap sees the world as the attacker saw it. The netcode
//!   design promised rewound overlaps (§7) and only `raycast` had ever kept it;
//!   a game cannot fix that from outside, because it cannot rewind anything.
//!
//! Every query takes the same `{ ignore = node, layers = "Ground" | {...} }`
//! options table as `raycast`, parsed by the same code, so learning one teaches
//! the others.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// The shared state a query needs: the lent collider/hull sets, the sim origin
/// (scripts speak world, the sim runs origin-relative — ADR-0015), the running
/// script's own entity, and the project's layer table.
#[derive(Clone)]
pub(crate) struct QueryShared {
    pub colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    pub hulls: Rc<RefCell<Vec<floptle_physics::BodyHull>>>,
    pub sim_origin: Rc<RefCell<glam::DVec3>>,
    pub current: Rc<RefCell<Option<(u32, String)>>>,
    pub layers: Rc<RefCell<floptle_core::Layers>>,
}

/// Parse the shared options table into (bodies to skip, layer mask).
///
/// Identical in meaning to `raycast`'s, deliberately: an unknown layer name is
/// an error rather than a silent everything-misses, which is the failure mode
/// that costs an afternoon.
fn query_opts(
    who: &str,
    opts: Option<&Value>,
    shared: &QueryShared,
) -> mlua::Result<(Vec<u32>, u32)> {
    let mut exclude: Vec<u32> = Vec::with_capacity(2);
    let mut mask = !0u32;
    // Your own body never counts as a hit — a hitbox centred on you must not
    // report you, the same rule the ray uses.
    if let Some((eid, _)) = shared.current.borrow().as_ref() {
        exclude.push(*eid);
    }
    match opts {
        Some(Value::Table(t)) => {
            if let Ok(eid) = t.raw_get::<u32>("__id") {
                exclude.push(eid);
            } else {
                if let Ok(ig) = t.get::<Table>("ignore")
                    && let Ok(eid) = ig.raw_get::<u32>("__id")
                {
                    exclude.push(eid);
                }
                let names: Vec<String> = match t.get::<Value>("layers") {
                    Ok(Value::String(s)) => vec![s.to_string_lossy().to_string()],
                    Ok(Value::Table(list)) => list.sequence_values::<String>().flatten().collect(),
                    _ => Vec::new(),
                };
                if !names.is_empty() {
                    let lt = shared.layers.borrow();
                    mask = 0;
                    for n in &names {
                        match lt.index_of(n) {
                            Some(i) => mask |= 1u32 << i,
                            None => {
                                return Err(mlua::Error::RuntimeError(format!(
                                    "{who}: no layer named '{n}' (project layers: {})",
                                    lt.names.join(", ")
                                )));
                            }
                        }
                    }
                }
            }
        }
        Some(Value::Integer(id)) => exclude.push(*id as u32),
        Some(Value::Number(id)) => exclude.push(*id as u32),
        _ => {}
    }
    Ok((exclude, mask))
}

/// Build the Lua table one hit becomes — the same field names `raycast`
/// returns, so a script that handles one handles the other.
fn hit_table(
    lua: &Lua,
    h: &floptle_physics::ShapeHit,
    origin: glam::DVec3,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("x", origin.x + h.point[0] as f64)?;
    t.set("y", origin.y + h.point[1] as f64)?;
    t.set("z", origin.z + h.point[2] as f64)?;
    t.set("nx", h.normal[0] as f64)?;
    t.set("ny", h.normal[1] as f64)?;
    t.set("nz", h.normal[2] as f64)?;
    t.set("distance", h.distance as f64)?;
    if let Some(eid) = h.eid {
        t.set("node", crate::env::new_node_handle(lua, eid)?)?;
    }
    Ok(t)
}

/// Install `overlapSphere`, `overlapBox`, `spherecast` and `capsulecast`.
pub(crate) fn install_shape_api(
    lua: &Lua,
    shared: QueryShared,
) {
    // overlapSphere(center, radius [, opts]) → list of hits, nearest-surface
    // first. Reports BOTH static geometry and body hulls; `hit.node` is the
    // node where there is one.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let Some(c) = a.first().and_then(crate::math_api::vec3_of) else {
                return Err(mlua::Error::RuntimeError(
                    "overlapSphere(center, radius [, opts]) — center is a vec3 (or a node)".into(),
                ));
            };
            let radius = match a.get(1) {
                Some(Value::Number(n)) => *n,
                Some(Value::Integer(i)) => *i as f64,
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "overlapSphere(center, radius [, opts]) — radius is a number".into(),
                    ));
                }
            };
            let (exclude, mask) = query_opts("overlapSphere", a.get(2), &s)?;
            let origin = *s.sim_origin.borrow();
            let p = (glam::DVec3::new(c.x, c.y, c.z) - origin).as_vec3();
            let r = radius as f32;
            let mut hits =
                floptle_physics::overlap_sphere_colliders(&s.colliders.borrow(), p, r, mask);
            hits.extend(floptle_physics::overlap_sphere_hulls(
                &s.hulls.borrow(),
                p,
                r,
                &exclude,
                mask,
            ));
            // Deepest overlap first: the thing most inside the volume is the
            // thing a hit most wants to resolve against.
            hits.sort_by(|a, b| b.distance.total_cmp(&a.distance));
            let out = lua.create_table()?;
            for (i, h) in hits.iter().enumerate() {
                out.set(i + 1, hit_table(lua, h, origin)?)?;
            }
            Ok(out)
        }) {
            let _ = lua.globals().set("overlapSphere", f);
        }
    }

    // spherecast(origin, dir, radius, max [, opts]) → the first hit, or nil.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let num = |v: Option<&Value>| -> Option<f64> {
                match v {
                    Some(Value::Number(n)) => Some(*n),
                    Some(Value::Integer(i)) => Some(*i as f64),
                    _ => None,
                }
            };
            let (Some(o), Some(d), Some(radius), Some(max)) = (
                a.first().and_then(crate::math_api::vec3_of),
                a.get(1).and_then(crate::math_api::vec3_of),
                num(a.get(2)),
                num(a.get(3)),
            ) else {
                return Err(mlua::Error::RuntimeError(
                    "spherecast(origin, dir, radius, max [, opts]) — origin and dir are vec3s \
                     (or a node), radius and max are numbers"
                        .into(),
                ));
            };
            let (exclude, mask) = query_opts("spherecast", a.get(4), &s)?;
            let sim = *s.sim_origin.borrow();
            let p = (glam::DVec3::new(o.x, o.y, o.z) - sim).as_vec3();
            let dir = glam::Vec3::new(d.x as f32, d.y as f32, d.z as f32);
            let hit = floptle_physics::spherecast(
                &s.colliders.borrow(),
                &s.hulls.borrow(),
                p,
                dir,
                radius as f32,
                max as f32,
                &exclude,
                mask,
            );
            match hit {
                Some(h) => Ok(Value::Table(hit_table(lua, &h, sim)?)),
                None => Ok(Value::Nil),
            }
        }) {
            let _ = lua.globals().set("spherecast", f);
        }
    }

    // capsulecast(origin, dir, radius, halfHeight, max [, opts]) → first hit or
    // nil. The player-shaped sweep: "can I actually move there", asked with the
    // shape that will be moving.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let num = |v: Option<&Value>| -> Option<f64> {
                match v {
                    Some(Value::Number(n)) => Some(*n),
                    Some(Value::Integer(i)) => Some(*i as f64),
                    _ => None,
                }
            };
            let (Some(o), Some(d), Some(radius), Some(half), Some(max)) = (
                a.first().and_then(crate::math_api::vec3_of),
                a.get(1).and_then(crate::math_api::vec3_of),
                num(a.get(2)),
                num(a.get(3)),
                num(a.get(4)),
            ) else {
                return Err(mlua::Error::RuntimeError(
                    "capsulecast(origin, dir, radius, halfHeight, max [, opts])".into(),
                ));
            };
            let (exclude, mask) = query_opts("capsulecast", a.get(5), &s)?;
            let sim = *s.sim_origin.borrow();
            let p = (glam::DVec3::new(o.x, o.y, o.z) - sim).as_vec3();
            let dir = glam::Vec3::new(d.x as f32, d.y as f32, d.z as f32);
            // Upright along the capsule's own axis; the solver keeps a capsule
            // body aligned to −gravity, so a cast agrees with the move.
            let up = s
                .hulls
                .borrow()
                .iter()
                .find(|h| s.current.borrow().as_ref().is_some_and(|(e, _)| *e == h.eid))
                .map(|h| h.up)
                .unwrap_or(glam::Vec3::Y);
            let hit = floptle_physics::capsulecast(
                &s.colliders.borrow(),
                &s.hulls.borrow(),
                p,
                dir,
                radius as f32,
                half as f32,
                up,
                max as f32,
                &exclude,
                mask,
            );
            match hit {
                Some(h) => Ok(Value::Table(hit_table(lua, &h, sim)?)),
                None => Ok(Value::Nil),
            }
        }) {
            let _ = lua.globals().set("capsulecast", f);
        }
    }
}
