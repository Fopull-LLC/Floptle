//! `water.*` — what a script asks about the wet parts of the world.
//!
//! The engine owns the volume, the buoyancy and the drag. What a game still has
//! to decide is everything *meaningful*: whether the player is swimming or
//! drowning, whether the engine floods, whether a gauge reads red, whether the
//! music ducks. All of those are the same question with different answers —
//! "how deep is this point" — so that is the one thing this module exports, and
//! everything else is derived from it in Lua where it belongs.
//!
//! Without it a game re-derives the geometry: a `seaDepth(x,y,z)` of its own
//! against a sea radius it has to keep in step with the sphere the generator
//! drew. Swimming still belongs to the game — a character is one capsule —
//! but it does not have to agree with the renderer by hand.

use std::cell::RefCell;
use std::rc::Rc;

use floptle_core::water::{DeclaredWater, HeightGrid, Surface};
use mlua::{Lua, Table, Value};

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
    /// Water scripts declared with `water.addVolume`. The driver keeps each
    /// one's frame on its anchor and hands them to the solver.
    pub declared: Rc<RefCell<Vec<DeclaredWater>>>,
    pub next_id: Rc<std::cell::Cell<u32>>,
    /// The level, lent for the frame, so `water.surface` can tell which of a
    /// lake's sphere lies in its basin and which runs under the ground.
    pub colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    pub sim_origin: Rc<RefCell<glam::DVec3>>,
}

/// What `water.at` says about the deepest water at a point: a scene volume or
/// one a script declared.
struct Hit {
    depth: f64,
    density: f32,
    frozen: bool,
    node: Option<u32>,
    volume: Option<u32>,
    up: glam::DVec3,
    flow: glam::DVec3,
}

/// The deepest water containing `p` across both kinds, by the same
/// "innermost wins" rule the solver uses.
fn deepest_any(s: &WaterShared, p: [f64; 3]) -> Option<Hit> {
    let mut best = deepest(&s.volumes.borrow(), p).map(|(v, d)| Hit {
        depth: d,
        density: v.density,
        frozen: v.frozen,
        node: Some(v.entity),
        volume: None,
        up: if v.sea {
            (glam::DVec3::from(p) - glam::DVec3::from(v.center)).try_normalize().unwrap_or(glam::DVec3::Y)
        } else {
            glam::DQuat::from_xyzw(v.rot[0], v.rot[1], v.rot[2], v.rot[3]) * glam::DVec3::Y
        },
        flow: glam::DVec3::ZERO,
    });
    for w in s.declared.borrow().iter() {
        let at = w.at(glam::DVec3::from(p));
        if at.depth > 0.0 && best.as_ref().is_none_or(|b| at.depth > b.depth) {
            best = Some(Hit {
                depth: at.depth,
                density: w.density,
                frozen: false,
                node: None,
                volume: Some(w.id),
                up: at.up,
                flow: at.flow,
            });
        }
    }
    best
}

/// A declared water's surface as triangles in its anchor's frame: what
/// `water.surface` hands to `mesh.build`.
pub(crate) struct SurfaceMesh {
    pub positions: Vec<glam::DVec3>,
    pub normals: Vec<glam::DVec3>,
    /// Metres across the surface, so a shader tiles and flows in metres.
    pub uvs: Vec<[f64; 2]>,
    /// 0-based, three per triangle.
    pub indices: Vec<u32>,
}

/// [`surface_mesh_in`] with no ground: a lake's whole disc.
#[cfg(test)]
pub(crate) fn surface_mesh(surface: &Surface, cells: usize) -> SurfaceMesh {
    surface_mesh_in(surface, cells, &|_| false)
}

/// Triangulate `surface`. A lake is a grid of `cells` across laid on its
/// sphere by arc length out past its farthest shore; a heightfield is its own
/// grid, triangles only where its points are wet. Of a lake only its basin is
/// kept: the points `under`
/// says are not below the ground, reached from the lake's centre without
/// crossing ground, and the shore triangles around them. A lake's sphere
/// carries on past its spill point over lower ground outside the basin, and
/// drawing it there put water where there is none.
pub(crate) fn surface_mesh_in(surface: &Surface, cells: usize, under: &dyn Fn(glam::DVec3) -> bool) -> SurfaceMesh {
    let mut m = SurfaceMesh { positions: Vec::new(), normals: Vec::new(), uvs: Vec::new(), indices: Vec::new() };
    match surface {
        Surface::Lake { radius, axis, reach } => {
            let a = axis.try_normalize().unwrap_or(glam::DVec3::Y);
            let e1 = a.any_orthonormal_vector();
            let e2 = e1.cross(a);
            // A little past the farthest shore, so the shore itself is under water.
            let r = reach * 1.05 + 0.5;
            let n = cells;
            let mut index = vec![u32::MAX; (n + 1) * (n + 1)];
            for j in 0..=n {
                for i in 0..=n {
                    let u = (i as f64 / n as f64 * 2.0 - 1.0) * r;
                    let v = (j as f64 / n as f64 * 2.0 - 1.0) * r;
                    let arc = (u * u + v * v).sqrt();
                    if arc > r + r / n as f64 {
                        continue;
                    }
                    // Out along the surface by arc length: the disc lies on
                    // the sphere, not on a plane through its centre.
                    let theta = arc / radius.max(1e-6);
                    let side = if arc > 1e-12 { (e1 * u + e2 * v) / arc } else { e1 };
                    let dir = a * theta.cos() + side * theta.sin();
                    index[j * (n + 1) + i] = m.positions.len() as u32;
                    m.positions.push(dir * *radius);
                    m.normals.push(dir);
                    m.uvs.push([u, v]);
                }
            }
            // The basin: flood from the centre across points not under ground.
            let side = n + 1;
            let wet: Vec<bool> = index.iter().map(|&k| k != u32::MAX && !under(m.positions[k as usize])).collect();
            let mut reached = vec![false; index.len()];
            let centre = (n / 2) * side + n / 2;
            let mut todo = if wet[centre] { vec![centre] } else { Vec::new() };
            while let Some(c) = todo.pop() {
                if reached[c] {
                    continue;
                }
                reached[c] = true;
                let (i, j) = (c % side, c / side);
                for (di, dj) in [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)] {
                    let (a, b) = (i as i64 + di, j as i64 + dj);
                    if a >= 0 && b >= 0 && (a as usize) < side && (b as usize) < side {
                        let k = b as usize * side + a as usize;
                        if wet[k] && !reached[k] {
                            todo.push(k);
                        }
                    }
                }
            }
            {
                // Keep what the basin reaches and the ring of points round it,
                // so the shoreline runs under the ground rather than short of it.
                let keep: Vec<bool> = (0..index.len())
                    .map(|c| {
                        let (i, j) = (c % side, c / side);
                        reached[c]
                            || [(-1i64, 0i64), (1, 0), (0, -1), (0, 1)].iter().any(|(di, dj)| {
                                let (a, b) = (i as i64 + di, j as i64 + dj);
                                a >= 0 && b >= 0 && (a as usize) < side && (b as usize) < side && reached[b as usize * side + a as usize]
                            })
                    })
                    .collect();
                for (c, k) in index.iter_mut().enumerate() {
                    if !keep[c] {
                        *k = u32::MAX;
                    }
                }
            }
            push_quads(&mut m.indices, &index, n + 1, n + 1);
            compact(&mut m);
        }
        Surface::Grid(g) => {
            let mut index = vec![u32::MAX; g.columns * g.rows];
            for j in 0..g.rows {
                for i in 0..g.columns {
                    let l = g.levels[j * g.columns + i];
                    if !l.is_finite() {
                        continue;
                    }
                    let (x, z) = (g.x0 + i as f64 * g.cell, g.z0 + j as f64 * g.cell);
                    index[j * g.columns + i] = m.positions.len() as u32;
                    m.positions.push(glam::DVec3::new(x, l as f64, z));
                    m.normals.push(glam::DVec3::Y);
                    m.uvs.push([x, z]);
                }
            }
            push_quads(&mut m.indices, &index, g.columns, g.rows);
        }
    }
    m
}

/// Drop the vertices no triangle uses, renumbering the rest.
fn compact(m: &mut SurfaceMesh) {
    let mut used = vec![u32::MAX; m.positions.len()];
    let mut out = SurfaceMesh { positions: Vec::new(), normals: Vec::new(), uvs: Vec::new(), indices: Vec::new() };
    for &i in &m.indices {
        let i = i as usize;
        if used[i] == u32::MAX {
            used[i] = out.positions.len() as u32;
            out.positions.push(m.positions[i]);
            out.normals.push(m.normals[i]);
            out.uvs.push(m.uvs[i]);
        }
        out.indices.push(used[i]);
    }
    *m = out;
}

/// Two triangles per grid cell, wound to face +Y in the grid's frame, each
/// only where its three corners exist.
fn push_quads(out: &mut Vec<u32>, index: &[u32], columns: usize, rows: usize) {
    for j in 0..rows.saturating_sub(1) {
        for i in 0..columns.saturating_sub(1) {
            let at = |di: usize, dj: usize| index[(j + dj) * columns + i + di];
            let (a, b, c, d) = (at(0, 0), at(1, 0), at(0, 1), at(1, 1));
            let wet = |k: u32| k != u32::MAX;
            // A cell with one dry corner keeps the triangle of the other
            // three, so a shore steps by half cells rather than whole ones.
            let tris: &[[u32; 3]] = match (wet(a), wet(b), wet(c), wet(d)) {
                (true, true, true, true) => &[[a, c, b], [b, c, d]],
                (false, true, true, true) => &[[b, c, d]],
                (true, false, true, true) => &[[a, c, d]],
                (true, true, false, true) => &[[a, d, b]],
                (true, true, true, false) => &[[a, c, b]],
                _ => &[],
            };
            out.extend(tris.iter().flatten());
        }
    }
}

/// Every key `water.addVolume` reads.
pub(crate) const VOLUME_KEYS: &[&str] = &[
    "shape", "parent", "center", "radius", "reach", "origin", "cell", "columns", "rows", "levels", "flow",
    "density", "drag", "angularDrag", "current", "depth", "tint", "visibility", "underwater", "level",
];

/// The most points a simulated volume may have: a few hundred metres of lake
/// at a couple of metres a point, stepped every frame.
pub const MAX_SIM_POINTS: usize = 128 * 128;

/// A `water.addVolume` options table as the water it declares.
fn declared_from(opts: &Table, id: u32) -> mlua::Result<DeclaredWater> {
    const CALL: &str = "water.addVolume";
    crate::opts::check_keys(opts, VOLUME_KEYS, CALL)?;
    let err = |m: &str| mlua::Error::RuntimeError(format!("{CALL}: {m}"));
    let num = |k: &str| opts.get::<Option<f64>>(k).ok().flatten();
    let vec = |k: &str| opts.get::<Value>(k).ok().as_ref().and_then(crate::math_api::vec3_of);
    let mut sim_level = None;
    let shape: String = opts.get::<Option<String>>("shape")?.unwrap_or_else(|| "lake".into());
    let surface = match shape.as_str() {
        "lake" => {
            let center = vec("center").ok_or_else(|| {
                err("a lake needs `center`, a point on its surface in the parent's frame (terrain.planetLakes gives one)")
            })?;
            let radius = num("radius").unwrap_or(center.length());
            let reach = num("reach").ok_or_else(|| err("a lake needs `reach`, how far its farthest shore is from `center`, in metres"))?;
            if radius <= 0.0 {
                return Err(err("a lake's surface is `radius` metres from its parent's centre, and that must be more than 0"));
            }
            Surface::Lake { radius, axis: center.try_normalize().unwrap_or(glam::DVec3::Y), reach: reach.max(0.0) }
        }
        // Water the engine moves: a heightfield whose levels the engine finds
        // by filling the ground under it to `level`, then flows.
        "sim" => {
            let columns = num("columns").ok_or_else(|| err("a sim needs `columns` and `rows`"))? as usize;
            let rows = num("rows").ok_or_else(|| err("a sim needs `columns` and `rows`"))? as usize;
            let cell = num("cell").ok_or_else(|| err("a sim needs `cell`, the metres between points"))?;
            if columns < 2 || rows < 2 || cell <= 0.0 || columns * rows > MAX_SIM_POINTS {
                return Err(err(&format!(
                    "a sim is at least 2 × 2 points and at most {MAX_SIM_POINTS}, `cell` more than 0 metres apart"
                )));
            }
            let level = num("level").ok_or_else(|| err("a sim needs `level`, the height it is filled to in its parent's frame"))?;
            sim_level = Some(level as f32);
            let origin = vec("origin").unwrap_or(glam::DVec3::ZERO);
            let levels = vec![level as f32; columns * rows];
            Surface::Grid(std::sync::Arc::new(HeightGrid { x0: origin.x, z0: origin.z, cell, columns, rows, levels, flow: Vec::new() }))
        }
        "heightfield" => {
            let columns = num("columns").ok_or_else(|| err("a heightfield needs `columns` and `rows`"))? as usize;
            let rows = num("rows").ok_or_else(|| err("a heightfield needs `columns` and `rows`"))? as usize;
            let cell = num("cell").ok_or_else(|| err("a heightfield needs `cell`, the metres between points"))?;
            if columns < 2 || rows < 2 || cell <= 0.0 {
                return Err(err("a heightfield is at least 2 × 2 points, `cell` more than 0 metres apart"));
            }
            let levels_t: Table = opts.get("levels").map_err(|_| err("a heightfield needs `levels`, one per point"))?;
            let mut levels = Vec::with_capacity(columns * rows);
            for i in 1..=columns * rows {
                // `false` (or a missing number) is dry ground.
                levels.push(match levels_t.raw_get::<Value>(i)? {
                    Value::Number(n) => n as f32,
                    Value::Integer(n) => n as f32,
                    _ => f32::NAN,
                });
            }
            if levels_t.raw_len() != columns * rows {
                return Err(err(&format!("`levels` has {} entries for {columns} × {rows} points", levels_t.raw_len())));
            }
            let mut flow = Vec::new();
            if let Ok(Some(f)) = opts.get::<Option<Table>>("flow") {
                if f.raw_len() != columns * rows * 2 {
                    return Err(err(&format!("`flow` is x, z per point: {} numbers for {} points", f.raw_len(), columns * rows)));
                }
                for i in 0..columns * rows {
                    flow.push([f.raw_get::<f64>(i * 2 + 1)? as f32, f.raw_get::<f64>(i * 2 + 2)? as f32]);
                }
            }
            let origin = vec("origin").unwrap_or(glam::DVec3::ZERO);
            Surface::Grid(std::sync::Arc::new(HeightGrid { x0: origin.x, z0: origin.z, cell, columns, rows, levels, flow }))
        }
        other => return Err(err(&format!("`shape` is \"lake\", \"heightfield\" or \"sim\", not \"{other}\""))),
    };
    let anchor = match opts.get::<Value>("parent")? {
        Value::Nil => None,
        Value::String(n) => Some(n.to_str()?.to_string()),
        _ => return Err(err("`parent` is the name of the node the water lies on")),
    };
    Ok(DeclaredWater {
        id,
        anchor,
        frame: floptle_core::scatter::Frame::IDENTITY,
        surface,
        density: num("density").unwrap_or(1000.0).max(1e-3) as f32,
        drag: num("drag").unwrap_or(1.0).max(0.0) as f32,
        angular_drag: num("angularDrag").unwrap_or(1.0).max(0.0) as f32,
        current: vec("current").unwrap_or(glam::DVec3::ZERO).as_vec3(),
        floor: match num("depth") {
            Some(d) if d > 0.0 => d,
            Some(_) => return Err(err("`depth` is how far below the surface the bed lies, in metres, more than 0")),
            None => f64::INFINITY,
        },
        look: look_from(opts, CALL)?,
        simulated: sim_level,
    })
}

/// The underwater look an `addVolume` options table asks for: on with a Water
/// Volume node's defaults, `tint = {r, g, b}` and `visibility` (metres) to
/// change it, `underwater = false` for none.
fn look_from(opts: &Table, call: &str) -> mlua::Result<Option<floptle_core::water::UnderwaterLook>> {
    let err = |m: &str| mlua::Error::RuntimeError(format!("{call}: {m}"));
    if opts.get::<Option<bool>>("underwater").map_err(|_| err("`underwater` is true or false"))? == Some(false) {
        return Ok(None);
    }
    let mut look = floptle_core::water::UnderwaterLook::default();
    if let Some(t) = opts.get::<Option<Table>>("tint").map_err(|_| err("`tint` is {r, g, b} from 0 to 1"))? {
        for (i, c) in look.tint.iter_mut().enumerate() {
            *c = t.raw_get::<f32>(i + 1).map_err(|_| err("`tint` is {r, g, b} from 0 to 1"))?.clamp(0.0, 1.0);
        }
    }
    if let Some(v) = opts.get::<Option<f64>>("visibility")? {
        look.visibility = (v as f32).max(0.5);
    }
    Ok(Some(look))
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
            Ok(deepest_any(&s, p).map(|h| h.depth).unwrap_or(0.0))
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
            let Some(h) = deepest_any(&s, p) else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            out.set("depth", h.depth)?;
            out.set("density", h.density as f64)?;
            out.set("frozen", h.frozen)?;
            if let Some(e) = h.node {
                out.set("node", crate::env::new_node_handle(lua, e)?)?;
            }
            if let Some(id) = h.volume {
                out.set("volume", id)?;
            }
            // "Up" out of the water: radial on a sea or a planet's lake, the
            // pool's own +Y. What a swim controller pushes along, and it is
            // not −gravity in a tilted tank.
            out.set("up", crate::math_api::LuaVec3(h.up))?;
            out.set("flow", crate::math_api::LuaVec3(h.flow))?;
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
            Ok(deepest_any(&s, p).is_some())
        }) {
            let _ = t.set("isUnderwater", f);
        }
    }

    // water.setFrozen(node, frozen) — freezing is a state, not a second system.
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

    // water.flowAt(x, y, z) → vec3, the water's velocity there in m/s (zero
    // in still water and in air). What a swimmer is pushed by.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let Some(p) = point_of(&a) else {
                return Err(mlua::Error::RuntimeError("water.flowAt(x, y, z) — or a vec3, or a node".into()));
            };
            Ok(crate::math_api::LuaVec3(deepest_any(&s, p).map_or(glam::DVec3::ZERO, |h| h.flow)))
        }) {
            let _ = t.set("flowAt", f);
        }
    }

    // water.addVolume{ shape = "lake" | "heightfield", parent = "Planet", ... }
    // → an id. Water the physics floats things in and `water.at` answers for,
    // in the frame of the node it lies on.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, opts: Table| {
            let id = s.next_id.get().wrapping_add(1).max(1);
            let w = declared_from(&opts, id)?;
            s.next_id.set(id);
            s.declared.borrow_mut().push(w);
            Ok(id)
        }) {
            let _ = t.set("addVolume", f);
        }
    }

    // water.surface(id [, { cells = 48 }]) → the arrays mesh.build takes, for
    // the surface of declared water in its parent's frame: so the lake that
    // is drawn is the lake `water.at` answers for, and a game writes no mesh
    // code to fit one.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, (id, opts): (u32, Option<Table>)| {
            if let Some(o) = &opts {
                crate::opts::check_keys(o, &["cells"], "water.surface")?;
            }
            let cells = opts.and_then(|o| o.get::<Option<f64>>("cells").ok().flatten()).unwrap_or(48.0);
            let cells = (cells.round() as usize).clamp(2, 512);
            let declared = s.declared.borrow();
            let Some(w) = declared.iter().find(|w| w.id == id) else { return Ok(Value::Nil) };
            // Under the ground is a point the level has a surface above,
            // within the lake's own reach. With no level lent (a host with no
            // physics) nothing is, and the lake is its whole disc.
            let cols = s.colliders.borrow();
            let origin = *s.sim_origin.borrow();
            let under = |local: glam::DVec3| -> bool {
                if cols.is_empty() {
                    return false;
                }
                let at = w.frame.to_world(local);
                let up = (w.frame.to_world(local.normalize_or_zero()) - w.frame.origin).normalize_or_zero();
                let height = match &w.surface {
                    Surface::Lake { reach, .. } => reach.max(10.0),
                    Surface::Grid(g) => (g.cell * g.columns.max(g.rows) as f64).max(10.0),
                };
                let from = (at + up * height - origin).as_vec3();
                floptle_physics::raycast_colliders(&cols, from, -up.as_vec3(), height as f32 - 0.01, !0).is_some()
            };
            let m = surface_mesh_in(&w.surface, cells, &under);
            let out = lua.create_table()?;
            let flat = |vals: &mut dyn Iterator<Item = f64>| -> mlua::Result<Table> {
                let t = lua.create_table()?;
                for (i, v) in vals.enumerate() {
                    t.raw_set(i + 1, v)?;
                }
                Ok(t)
            };
            out.set("positions", flat(&mut m.positions.iter().flat_map(|p| p.to_array()))?)?;
            out.set("normals", flat(&mut m.normals.iter().flat_map(|n| n.to_array()))?)?;
            out.set("uvs", flat(&mut m.uvs.iter().flat_map(|u| *u))?)?;
            out.set("indices", flat(&mut m.indices.iter().map(|&i| f64::from(i) + 1.0))?)?;
            Ok(Value::Table(out))
        }) {
            let _ = t.set("surface", f);
        }
    }

    // water.removeVolume(id) → true if there was one.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let mut d = s.declared.borrow_mut();
            let before = d.len();
            d.retain(|w| w.id != id);
            Ok(d.len() != before)
        }) {
            let _ = t.set("removeVolume", f);
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

    /// The script-side answer must be the same rule the solver uses. A game
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

    fn host() -> (Lua, WaterShared) {
        let lua = Lua::new();
        crate::math_api::install(&lua).unwrap();
        let shared = WaterShared {
            volumes: Rc::default(),
            freeze: Rc::default(),
            declared: Rc::default(),
            next_id: Rc::default(),
            colliders: Rc::default(),
            sim_origin: Rc::default(),
        };
        install_water_api(&lua, shared.clone());
        (lua, shared)
    }

    /// **A script's lake is water `water.at` answers for, where the lake is.**
    /// Declared from what `terrain.planetLakes` says, in the planet's frame,
    /// its surface is the sphere through the lake's centre point, it ends at
    /// the farthest shore, and it is gone when removed.
    #[test]
    fn a_declared_lake_is_water_where_the_lake_is() {
        let (lua, shared) = host();
        let (under, depth, shore, gone): (bool, f64, bool, bool) = lua
            .load(
                r#"local id = water.addVolume{ shape = "lake", parent = "Planet", center = vec3(0, 805, 0), reach = 60,
                                               current = vec3(1, 0, 0) }
                   local w = water.at(vec3(0, 803, 0))
                   assert(w.volume == id and w.node == nil and w.flow.x == 1)
                   assert(water.flowAt(0, 803, 0).x == 1 and water.flowAt(0, 900, 0).x == 0)
                   local shore = water.isUnderwater(vec3(800 * math.sin(70 / 805), 800 * math.cos(70 / 805), 0))
                   return water.isUnderwater(0, 804, 0), w.depth, shore, water.removeVolume(id) and not water.isUnderwater(0, 804, 0)"#,
            )
            .eval()
            .unwrap();
        assert!(under && (depth - 2.0).abs() < 1e-9 && !shore && gone, "{under} {depth} {shore} {gone}");
        assert!(shared.declared.borrow().is_empty());
    }

    /// **A lake has a bed, and a look from inside.** `depth` (as
    /// `terrain.planetLakes` gives it) is how far down the water reaches, so a
    /// cave under the lake is dry; the underwater look is a Water Volume
    /// node's unless the volume says otherwise, and `underwater = false` is none.
    #[test]
    fn a_lake_has_a_bed_and_an_underwater_look() {
        let (lua, shared) = host();
        let (lake, cave): (bool, bool) = lua
            .load(
                r#"local id = water.addVolume{ shape = "lake", center = vec3(0, 805, 0), reach = 60, depth = 6,
                                               tint = {0.2, 0.1, 0.05}, visibility = 9 }
                   water.addVolume{ shape = "lake", center = vec3(0, -905, 0), reach = 60, underwater = false }
                   return water.isUnderwater(0, 801, 0), water.isUnderwater(0, 790, 0)"#,
            )
            .eval()
            .unwrap();
        assert!(lake && !cave, "in the lake {lake}, in the cave under it {cave}");
        let d = shared.declared.borrow();
        assert_eq!(d[0].look, Some(floptle_core::water::UnderwaterLook { tint: [0.2, 0.1, 0.05], visibility: 9.0 }));
        assert_eq!(d[1].look, None);
        let err = lua.load(r#"water.addVolume{ shape = "lake", center = vec3(0, 5, 0), reach = 1, depth = -1 }"#).exec().unwrap_err();
        assert!(err.to_string().contains("`depth`"), "{err}");
    }

    /// A heightfield is wet where its levels are, and a misshapen one is
    /// refused by name rather than drawn wrong.
    #[test]
    fn a_heightfield_is_wet_where_its_levels_are() {
        let (lua, _) = host();
        let (wet, dry): (f64, bool) = lua
            .load(
                r#"water.addVolume{ shape = "heightfield", cell = 2, columns = 2, rows = 2, levels = { 3, 3, false, false } }
                   return water.depthAt(1, 1, 0), water.isUnderwater(1, 1, 2)"#,
            )
            .eval()
            .unwrap();
        assert!((wet - 2.0).abs() < 1e-9 && !dry, "{wet} {dry}");
        let err = lua
            .load(r#"water.addVolume{ shape = "heightfield", cell = 2, columns = 3, rows = 2, levels = { 1, 2 } }"#)
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("`levels` has 2 entries"), "{err}");
        let err = lua.load(r#"water.addVolume{ shape = "lake", center = vec3(0, 5, 0), reach = 1, deep = 2 }"#).exec().unwrap_err();
        assert!(err.to_string().contains("deep"), "{err}");
    }

    /// **The surface a game draws is the surface `water.at` answers for.**
    /// Every point of a lake's mesh is at the lake's level, it reaches past
    /// the farthest shore, and it faces out of the planet; a heightfield's mesh
    /// is its wet points, with no triangle reaching dry ground.
    #[test]
    fn a_declared_surface_meshes_where_the_water_is() {
        let lake = Surface::Lake { radius: 805.0, axis: glam::DVec3::new(0.0, 1.0, 1.0), reach: 60.0 };
        let m = surface_mesh(&lake, 32);
        assert!(!m.indices.is_empty() && m.indices.len().is_multiple_of(3));
        let axis = glam::DVec3::new(0.0, 1.0, 1.0).normalize();
        let mut farthest: f64 = 0.0;
        for (p, n) in m.positions.iter().zip(&m.normals) {
            assert!((p.length() - 805.0).abs() < 1e-6, "a vertex off the lake's level: {}", p.length());
            assert!((n.normalize() - p.normalize()).length() < 1e-9);
            farthest = farthest.max(p.normalize().dot(axis).clamp(-1.0, 1.0).acos() * 805.0);
        }
        assert!(farthest > 60.0 && farthest < 70.0, "reaches {farthest} m from the centre");
        // Faces out: the first triangle's winding agrees with its normal.
        let [a, b, c] = [0, 1, 2].map(|k| m.positions[m.indices[k] as usize]);
        assert!((b - a).cross(c - a).dot(m.normals[m.indices[0] as usize]) > 0.0, "wound into the planet");

        let nan = f32::NAN;
        let grid = HeightGrid { x0: 0.0, z0: 0.0, cell: 1.0, columns: 3, rows: 2, levels: vec![1.0, 1.0, nan, 1.0, 1.0, 1.0], flow: Vec::new() };
        let m = surface_mesh(&Surface::Grid(std::sync::Arc::new(grid)), 0);
        assert_eq!(m.positions.len(), 5);
        assert_eq!(m.indices.len(), 3 * 3, "two in the wet cell, one in the half-dry one");
        let [a, b, c] = [0, 1, 2].map(|k| m.positions[m.indices[k] as usize]);
        assert!((b - a).cross(c - a).y > 0.0, "a heightfield faces +Y");
    }

    /// **A lake's surface stops at its shore.** Its sphere runs on past the
    /// spill point over lower ground outside the basin, and the disc drew water
    /// there. Only the part reached from the centre without crossing ground is
    /// drawn now, with one ring past it that runs under the shore.
    #[test]
    fn a_lakes_surface_stays_in_its_basin() {
        let lake = Surface::Lake { radius: 805.0, axis: glam::DVec3::Y, reach: 60.0 };
        let along = |p: glam::DVec3| p.normalize().dot(glam::DVec3::Y).clamp(-1.0, 1.0).acos() * 805.0;
        // The basin is 30 m across; a rim from 30 to 40 m stands above the
        // water, and past it lower ground the sphere would flood.
        let under = |p: glam::DVec3| (30.0..40.0).contains(&along(p));
        let whole = surface_mesh(&lake, 48);
        let basin = surface_mesh_in(&lake, 48, &under);
        let farthest = |m: &SurfaceMesh| m.positions.iter().map(|p| along(*p)).fold(0.0, f64::max);
        assert!(farthest(&whole) > 60.0, "the whole disc reaches {}", farthest(&whole));
        assert!(farthest(&basin) < 36.0, "the basin's surface reaches {} m, past its rim", farthest(&basin));
        assert!(farthest(&basin) > 29.0, "the basin's surface stops short of its shore at {}", farthest(&basin));
        assert!(basin.indices.iter().all(|&i| (i as usize) < basin.positions.len()));
    }

    /// It reaches Lua as the arrays `mesh.build` takes.
    #[test]
    fn a_declared_surface_reaches_lua_as_mesh_build_arrays() {
        let (lua, _) = host();
        let (verts, tris, nothing): (i64, i64, bool) = lua
            .load(
                r#"local id = water.addVolume{ shape = "lake", center = vec3(0, 805, 0), reach = 60 }
                   local s = water.surface(id, { cells = 16 })
                   return #s.positions // 3, #s.indices // 3, water.surface(999) == nil"#,
            )
            .eval()
            .unwrap();
        assert!(verts > 100 && tris > 100 && nothing, "{verts} {tris} {nothing}");
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
