//! A river declared as a ribbon (`water.addVolume{ shape = "ribbon" }`): its
//! options, and its surface as a strip of triangles.
//!
//! The course itself, and every question asked of it, live in
//! `floptle_core::water_ribbon`. What is here is the script's side: reading
//! the points a game wrote, and laying the strip `water.surface` hands to
//! `mesh.build` across the same stations the physics floats things on.

use floptle_core::water::Surface;
use floptle_core::water_ribbon::{Bed, Ribbon, RibbonPoint};
use mlua::{Table, Value};

use crate::water_api::{surface_color, SurfaceLook, SurfaceMesh};

/// Columns of triangles from bank to bank when `water.surface` is not told.
pub(crate) const CELLS_ACROSS: usize = 4;

/// The `addVolume` options of a ribbon as its surface.
///
/// `halfWidth`, `depth` and `current` (a number) on the volume are what a
/// point that gives none of its own takes, so a river of one width is
/// written once. `on_world` says its parent is a round world: it then lies
/// round the parent's centre, and flat otherwise, unless `flat` says which.
pub(crate) fn ribbon_from(opts: &Table, call: &str, on_world: bool) -> mlua::Result<Surface> {
    let err = |m: &str| mlua::Error::RuntimeError(format!("{call}: {m}"));
    let num = |t: &Table, k: &str, what: &str| -> mlua::Result<Option<f64>> {
        match t.get::<Value>(k)? {
            Value::Nil => Ok(None),
            Value::Number(n) => Ok(Some(n)),
            Value::Integer(n) => Ok(Some(n as f64)),
            _ => Err(err(what)),
        }
    };
    let half_width = num(opts, "halfWidth", "`halfWidth` is metres from the middle of the channel to either bank")?;
    let depth = num(opts, "depth", "`depth` is metres from the surface to the bed in the middle of the channel")?;
    // A vec3 `current` is a current everywhere on top, as for every shape.
    let current = match opts.get::<Value>("current")? {
        Value::Number(n) => Some(n),
        Value::Integer(n) => Some(n as f64),
        _ => None,
    };
    let flat = crate::opts::opt_bool(opts, call, "flat")?.unwrap_or(!on_world);
    let straight = match crate::opts::opt_str(opts, call, "curve")?.as_deref() {
        None | Some("smooth") => false,
        Some("straight") => true,
        Some(other) => return Err(err(&format!("`curve` is \"smooth\" (the default: a curve through the points) or \"straight\", not \"{other}\""))),
    };
    let bed = match crate::opts::opt_str(opts, call, "bed")?.as_deref() {
        None | Some("u") => Bed::Round,
        Some("flat") => Bed::Flat,
        Some(other) => return Err(err(&format!("`bed` is \"u\" (the default: shallowing to the banks) or \"flat\", not \"{other}\""))),
    };
    const POINT: &str = "`points` is a list of { pos = vec3, halfWidth = m, depth = m, current = m/s } down the river";
    let list: Table = match opts.get::<Value>("points")? {
        Value::Table(t) => t,
        _ => return Err(err(&format!("a ribbon needs `points`: {POINT}"))),
    };
    let mut points = Vec::with_capacity(list.raw_len());
    for (i, entry) in list.sequence_values::<Value>().enumerate() {
        let Value::Table(e) = entry? else { return Err(err(POINT)) };
        crate::opts::check_keys(&e, &["pos", "halfWidth", "depth", "current"], "water.addVolume points")?;
        let pos = crate::math_api::vec3_of(&e.get::<Value>("pos")?)
            .ok_or_else(|| err(&format!("point {}'s `pos` is a vec3 on the water's surface, in the parent's frame", i + 1)))?;
        let half_width = num(&e, "halfWidth", POINT)?
            .or(half_width)
            .ok_or_else(|| err(&format!("point {} has no `halfWidth`, and the ribbon gives none for every point", i + 1)))?;
        points.push(RibbonPoint {
            pos,
            half_width,
            depth: num(&e, "depth", POINT)?.or(depth).unwrap_or(f64::INFINITY),
            current: num(&e, "current", POINT)?.or(current).unwrap_or(0.0),
        });
    }
    // A bed is the whole river's or nobody's: half a river with no bottom
    // has no depth to ease towards.
    if points.iter().any(|p| p.depth.is_infinite()) && points.iter().any(|p| p.depth.is_finite()) {
        return Err(err("give every point a `depth` (or the ribbon one for all of them), or none of them"));
    }
    let course = Ribbon::with(points, flat, bed, straight).map_err(|m| err(&m))?;
    Ok(Surface::Ribbon { course: std::sync::Arc::new(course), lift: 0.0 })
}

/// A ribbon's surface as a strip: a row of vertices from bank to bank at
/// every station of its course, `across` columns of triangles wide.
///
/// The layout depends only on the course, so the vertex count never changes
/// and the result always rewrites a mesh in place. `uvs` are metres: u down
/// the channel from the first point, so a shader scrolls along the flow, and
/// v across it from the middle. Colours are a sheet's: r = the depth over
/// the bed (shallowing to the banks under a U bed), g and b = the flow along
/// the frame's x and z, a = foam.
pub(crate) fn ribbon_mesh(m: &mut SurfaceMesh, course: &Ribbon, lift: f64, across: usize, look: &SurfaceLook) {
    // An even count, so the deepest line of the channel has vertices on it.
    let across = across.clamp(2, 64).next_multiple_of(2);
    let side = across + 1;
    for s in &course.stations {
        for c in 0..side {
            let x = c as f64 / across as f64 * 2.0 - 1.0;
            let out = s.pos + if x < 0.0 { s.banks[0] * -x } else { s.banks[1] * x };
            // On a round world the banks lie on the world, not on the flat
            // of the channel's middle.
            let up = course.up_at(out);
            let p = if course.flat { out + up * lift } else { up * (s.level + lift) };
            m.positions.push(p);
            m.normals.push(up);
            m.uvs.push([s.along, x * s.half_width]);
            if let Some(look) = &look.colors {
                let bed = course.bed_at(s.depth, (x * s.half_width).abs(), s.half_width);
                // A river with no bed is as deep as the ground under it.
                let depth = if bed.is_finite() { bed } else { (look.depth)(p) };
                let flow = s.tangent * s.current + look.current.as_dvec3();
                let foam = floptle_core::water::foam(depth as f32, flow.length() as f32);
                m.colors.push(surface_color(depth, [flow.x, flow.z], foam as f64));
            }
        }
    }
    for r in 0..course.stations.len() - 1 {
        for c in 0..across {
            let at = |dr: usize, dc: usize| ((r + dr) * side + c + dc) as u32;
            // Wound to face up: across, then downstream.
            m.indices.extend([at(0, 0), at(0, 1), at(1, 0), at(0, 1), at(1, 1), at(1, 0)]);
        }
    }
    m.index_capacity = m.indices.len();
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use glam::DVec3;
    use mlua::Lua;

    use super::*;
    use crate::water_api::{install_water_api, SurfaceBook, WaterShared};

    fn host() -> (Lua, WaterShared) {
        let lua = Lua::new();
        crate::math_api::install(&lua).unwrap();
        let shared = WaterShared {
            volumes: Rc::default(),
            freeze: Rc::default(),
            declared: Rc::default(),
            changed: Rc::default(),
            sim_cmds: Rc::default(),
            exchanged: Rc::default(),
            filled: Rc::default(),
            splashes: Rc::default(),
            wet: Rc::default(),
            slam: Rc::default(),
            worlds: Rc::default(),
            next_id: Rc::default(),
            colliders: Rc::default(),
            sim_origin: Rc::default(),
            surfaces: SurfaceBook::default(),
        };
        install_water_api(&lua, shared.clone());
        (lua, shared)
    }

    /// A river that runs 40 m east, turns a right angle and runs 40 m south,
    /// 3 m to each bank and 2 m deep, at 1.5 m/s.
    const BEND: &str = r#"river = water.addVolume{ shape = "ribbon", flat = true, halfWidth = 3, depth = 2, current = 1.5,
        points = { { pos = vec3(0, 10, 0) }, { pos = vec3(40, 10, 0) }, { pos = vec3(40, 10, 40) } } }"#;

    /// **A ribbon is water `water.at` answers for, between its banks.** In
    /// the channel it is the river's id, its depth below the eased surface
    /// and its current down the channel; past either bank, past either end,
    /// above the surface and under the bed there is no water; and a river
    /// narrower and shallower at one end than the other is each in between.
    #[test]
    fn a_ribbon_is_water_between_its_banks() {
        let (lua, shared) = host();
        lua.load(
            r#"id = water.addVolume{ shape = "ribbon", flat = true, points = {
                   { pos = vec3(0, 10, 0), halfWidth = 2, depth = 1, current = 0.5 },
                   { pos = vec3(100, 8, 0), halfWidth = 6, depth = 3, current = 1.5 } } }
               local w = water.at(50, 8.5, 0)
               volume, depth, flow, up = w.volume, w.depth, w.flow, w.up
               same = water.depthAt(vec3(50, 8.5, 0))
               flowAt = water.flowAt(50, 8.5, 0)
               inBank, outBank = water.isUnderwater(50, 8.9, 3.9), water.at(50, 8.9, 4.1)
               narrow, wide = water.at(2, 9.9, 2.5), water.isUnderwater(98, 8, 5.5)
               above, ends = water.at(50, 9.2, 0), water.at(-1, 9.9, 0) or water.at(101, 7.9, 0)
               -- The bed is 2 m down in the middle here and shallows to the banks.
               bed, shoal = water.at(50, 6.2, 0), water.at(50, 7.4, 3.9)
               inAir = water.flowAt(50, 12, 0)"#,
        )
        .exec()
        .unwrap();
        let g = lua.globals();
        let v = |k: &str| g.get::<crate::math_api::LuaVec3>(k).unwrap().0;
        assert_eq!(g.get::<u32>("volume").unwrap(), g.get::<u32>("id").unwrap());
        let depth = g.get::<f64>("depth").unwrap();
        assert!((depth - 0.5).abs() < 1e-9, "the surface is at 9 halfway down: {depth} m deep at 8.5");
        assert_eq!(g.get::<f64>("same").unwrap(), depth);
        let flow = v("flow");
        assert!((flow.length() - 1.0).abs() < 1e-9 && flow.x > 0.99 && flow.z.abs() < 1e-9, "{flow:?}");
        assert_eq!(v("flowAt"), flow);
        assert_eq!(v("up"), DVec3::Y);
        assert!(g.get::<bool>("inBank").unwrap(), "3.9 m off the middle of a channel 4 m to its bank");
        assert!(g.get::<mlua::Value>("outBank").unwrap().is_nil(), "past the bank");
        assert!(g.get::<mlua::Value>("narrow").unwrap().is_nil(), "the narrow end is 2 m to its banks");
        assert!(g.get::<bool>("wide").unwrap(), "the wide end is 6 m to its banks");
        assert!(g.get::<mlua::Value>("above").unwrap().is_nil(), "above the surface");
        assert!(g.get::<mlua::Value>("ends").unwrap().is_nil(), "past an end");
        assert!(g.get::<mlua::Value>("bed").unwrap().is_nil(), "2.8 m down in a channel 2 m deep");
        assert!(g.get::<mlua::Value>("shoal").unwrap().is_nil(), "1.6 m down at the bank, where it is centimetres deep");
        assert_eq!(v("inAir"), DVec3::ZERO);
        assert_eq!(shared.declared.borrow()[0].floor, f64::INFINITY, "a ribbon's depth is its channel's, not a floor");
    }

    fn mesh_of(shared: &WaterShared, across: usize, look: &SurfaceLook) -> SurfaceMesh {
        crate::water_api::surface_mesh_with(&shared.declared.borrow()[0].surface, across, &|_| false, look)
    }

    /// **The strip that is drawn is the river that floats things.** Round a
    /// right-angle bend, every point of every triangle is water just under
    /// the surface, a point just outside any edge of the strip is not, and
    /// over a grid of points across the whole bend, in the strip and in the
    /// water are the same thing.
    #[test]
    fn the_strip_covers_exactly_where_the_water_is() {
        strip_is_the_water(BEND);
    }

    /// **A straight river's strip turns its corners whole too.** The same
    /// bend in straight lines: the outside of the corner is a fan on the
    /// round, the inside banks meet at a point, no triangle faces down, and
    /// in the strip and in the water are still the same thing.
    #[test]
    fn a_straight_rivers_strip_covers_exactly_where_the_water_is() {
        strip_is_the_water(&BEND.replace("flat = true", "flat = true, curve = \"straight\""));
    }

    fn strip_is_the_water(declare: &str) {
        let (lua, shared) = host();
        lua.load(declare).exec().unwrap();
        let m = mesh_of(&shared, 4, &SurfaceLook::default());
        let d = shared.declared.borrow();
        let wet = |p: DVec3| d[0].at(p - DVec3::Y * 0.05).depth > 0.0;
        let tris: Vec<[DVec3; 3]> = m.indices.chunks(3).map(|t| [0, 1, 2].map(|k| m.positions[t[k] as usize])).collect();
        assert!(tris.len() >= 2 * 4 * 12, "{} triangles", tris.len());
        for t in &tris {
            let c = (t[0] + t[1] + t[2]) / 3.0;
            assert!(wet(c), "a triangle's middle is dry at {c:?}");
            for corner in t {
                let p = c + (*corner - c) * 0.98;
                assert!(wet(p), "a triangle's corner is dry at {p:?}");
            }
            // Facing up.
            assert!((t[1] - t[0]).cross(t[2] - t[0]).y > -1e-9, "a triangle faces down at {c:?}");
        }
        let inside = |p: DVec3| {
            tris.iter().any(|t| {
                // A triangle with no area (where a corner's banks meet) holds nothing.
                if (t[1] - t[0]).cross(t[2] - t[0]).length() < 1e-9 {
                    return false;
                }
                let s = |a: DVec3, b: DVec3| (b.x - a.x) * (p.z - a.z) - (b.z - a.z) * (p.x - a.x);
                let (x, y, z) = (s(t[0], t[1]), s(t[1], t[2]), s(t[2], t[0]));
                (x >= 0.0 && y >= 0.0 && z >= 0.0) || (x <= 0.0 && y <= 0.0 && z <= 0.0)
            })
        };
        let Surface::Ribbon { course, .. } = &d[0].surface else { unreachable!() };
        let (mut both, mut neither) = (0, 0);
        for i in 0..=250 {
            for j in 0..=250 {
                let p = DVec3::new(-5.0 + i as f64 * 0.2, 10.0, -5.0 + j as f64 * 0.2);
                // Within 3 cm of a bank the strip's straight edges and the
                // curve they follow may differ.
                let bank = course.stations.windows(2).filter(|s| s[0].pos != s[1].pos).map(|s| {
                    let (a, b) = (s[0].pos, s[1].pos);
                    let t = ((p - a).dot(b - a) / (b - a).length_squared()).clamp(0.0, 1.0);
                    ((p - a - (b - a) * t).length() - 3.0).abs()
                });
                // On the line an end is cut square along, either answer is right.
                let ends = p.x.abs().min((p.z - 40.0).abs());
                if bank.fold(f64::INFINITY, f64::min) < 0.03 || ends < 1e-6 {
                    continue;
                }
                assert_eq!(inside(p), wet(p), "the strip and the water disagree at {p:?}");
                if wet(p) { both += 1 } else { neither += 1 }
            }
        }
        assert!(both > 9_000 && neither > 40_000, "{both} wet and {neither} dry");
    }

    /// **The strip carries what a river shader reads.** Its `uvs` are metres
    /// down the channel and across it; its colours are the depth over the
    /// bed (shallowing to nothing at the banks) and the flow down the
    /// channel, which turns with the bend; and its layout never changes, so
    /// it goes to `mesh.build` once and `mesh.update` after.
    #[test]
    fn the_strip_carries_metres_depth_and_flow() {
        let (lua, shared) = host();
        lua.load(BEND).exec().unwrap();
        let depth = |_: DVec3| 16.0;
        let look = SurfaceLook {
            fixed: false,
            colors: Some(crate::water_api::SurfaceColors { report: None, depth: &depth, current: glam::Vec3::ZERO }),
        };
        let m = mesh_of(&shared, 4, &look);
        let rows = m.positions.len() / 5;
        assert_eq!((m.positions.len(), m.uvs.len(), m.colors.len()), (rows * 5, rows * 5, rows * 5));
        assert_eq!(m.indices.len(), (rows - 1) * 4 * 6);
        assert_eq!(m.index_capacity, m.indices.len());
        // Across the first row: v from −3 to 3 metres, u = 0.
        assert_eq!(m.uvs[..5], [[0.0, -3.0], [0.0, -1.5], [0.0, 0.0], [0.0, 1.5], [0.0, 3.0]]);
        // Down the middle u is metres along the curve, to its whole length.
        let mut along = 0.0;
        for r in 1..rows {
            along += (m.positions[r * 5 + 2] - m.positions[(r - 1) * 5 + 2]).length();
            assert!((m.uvs[r * 5 + 2][0] - along).abs() < 1e-9 && m.uvs[r * 5 + 2][0] > m.uvs[(r - 1) * 5 + 2][0]);
        }
        assert!(along > 80.0 && along < 90.0, "the bend is {along} m long");
        // Depth: 2 m in the middle, three quarters of it halfway out, none at the bank.
        let near = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(near(m.colors[2][0], 2.0 / 16.0) && near(m.colors[1][0], 1.5 / 16.0) && near(m.colors[0][0], 0.0), "{:?}", &m.colors[..5]);
        // Flow: 1.5 m/s east at the start (g), south at the end (b).
        let about = |a: f32, b: f32| (a - b).abs() < 0.02;
        assert!(about(m.colors[2][1], 0.5 + 1.5 / 8.0) && about(m.colors[2][2], 0.5), "{:?}", m.colors[2]);
        let end = m.colors[(rows - 1) * 5 + 2];
        assert!(about(end[1], 0.5) && about(end[2], 0.5 + 1.5 / 8.0), "{end:?}");

        // From Lua: built once, updated after, as lists or as buffers.
        let book: Rc<RefCell<crate::mesh_api::MeshBook>> = Rc::default();
        crate::mesh_api::install(&lua, book.clone()).unwrap();
        lua.load(
            r#"local s = water.surface(river, { fixed = true, colors = true })
               verts, cap, dynamic = #s.positions // 3, s.indexCapacity, s.dynamic
               sheet = mesh.build(s)
               wide = #water.surface(river, { cells = 8 }).positions // 3"#,
        )
        .exec()
        .unwrap();
        let built = book.borrow_mut().take_requests().remove(0);
        assert_eq!((built.positions.len(), built.index_capacity), (rows * 5, (rows - 1) * 4 * 6));
        lua.load(
            r#"water.setLevel(river, 11)
               local s2 = water.surface(river, { fixed = true, colors = true, buffers = true })
               mesh.update(sheet, s2)"#,
        )
        .exec()
        .unwrap();
        let g = lua.globals();
        assert_eq!(g.get::<usize>("verts").unwrap(), rows * 5);
        assert_eq!(g.get::<usize>("wide").unwrap(), rows * 9, "`cells` counts from bank to bank");
        assert_eq!(g.get::<usize>("cap").unwrap(), (rows - 1) * 4 * 6);
        assert!(g.get::<bool>("dynamic").unwrap());
        let u = book.borrow_mut().take_updates().remove(0);
        let v = u.vertices.unwrap();
        assert_eq!(v.positions.len(), rows * 5, "the update changed the vertex count");
        assert!(v.positions.iter().all(|p| p[1] == 11.0), "the raised river's vertices are not at 11");
        assert_eq!(book.borrow().live.len(), 1);
    }

    /// **`water.setLevel` raises a whole river.** Its level is its first
    /// point's height; setting it moves every point by the same amount, so a
    /// river that falls 2 m along its length still does, floats things 1 m
    /// higher, says its surface changed and tells the driver.
    #[test]
    fn set_level_raises_every_point_of_a_ribbon() {
        let (lua, shared) = host();
        lua.load(
            r#"id = water.addVolume{ shape = "ribbon", flat = true, halfWidth = 3, depth = 2, points = {
                   { pos = vec3(0, 10, 0) }, { pos = vec3(100, 8, 0) } } }
               water.surface(id)
               level, unchanged = water.levelOf(id), water.surfaceChanged(id)
               head, foot, dry = water.depthAt(1, 9.5, 0), water.depthAt(99, 7.5, 0), water.depthAt(50, 9.5, 0)
               ok = water.setLevel(id, 11)
               changed, raised = water.surfaceChanged(id), water.levelOf(id)
               head2, foot2, wet = water.depthAt(1, 9.5, 0), water.depthAt(99, 7.5, 0), water.depthAt(50, 9.5, 0)
               y = water.surface(id).positions[2]
               taken = water.surfaceChanged(id)"#,
        )
        .exec()
        .unwrap();
        let g = lua.globals();
        let n = |k: &str| g.get::<f64>(k).unwrap();
        assert_eq!((n("level"), n("raised")), (10.0, 11.0));
        assert!(!g.get::<bool>("unchanged").unwrap() && g.get::<bool>("changed").unwrap() && !g.get::<bool>("taken").unwrap());
        assert!(g.get::<bool>("ok").unwrap());
        assert!((n("head2") - n("head") - 1.0).abs() < 1e-9 && (n("foot2") - n("foot") - 1.0).abs() < 1e-9, "each end rose 1 m");
        assert!(n("dry") == 0.0 && (n("wet") - 0.5).abs() < 1e-9, "halfway down, 9.5 was above a surface at 9 and is 0.5 m under one at 10");
        assert_eq!(n("y"), 11.0);
        assert_eq!(*shared.changed.borrow(), vec![g.get::<u32>("id").unwrap()]);
    }

    /// **A ribbon that cannot be a river says why.** No points, one point,
    /// a point with no width, a misspelt key, half a bed, a river on a
    /// round world laid at its parent's centre: each is refused by name.
    #[test]
    fn a_misdeclared_ribbon_is_refused_by_name() {
        let (lua, shared) = host();
        for (opts, want) in [
            (r#"shape = "ribbon""#, "points"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, points = { { pos = vec3(0, 0, 0) } }"#, "2 to"),
            (r#"shape = "ribbon", flat = true, points = { { pos = vec3(0, 0, 0) }, { pos = vec3(9, 0, 0) } }"#, "halfWidth"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, points = { { pos = vec3(0, 0, 0), width = 3 }, { pos = vec3(9, 0, 0) } }"#, "width"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, points = { { pos = vec3(0, 0, 0), depth = 1 }, { pos = vec3(9, 0, 0) } }"#, "depth"),
            (r#"shape = "ribbon", flat = false, halfWidth = 2, points = { { pos = vec3(0, 0, 0) }, { pos = vec3(9, 0, 0) } }"#, "flat = true"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, bed = "v", points = { { pos = vec3(0, 0, 0) }, { pos = vec3(9, 0, 0) } }"#, "bed"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, points = { { pos = vec3(0, 0, 0) }, { pos = vec3(0, 5, 0) } }"#, "above the other"),
            (r#"shape = "ribbon", flat = true, halfWidth = 2, curve = "wavy", points = { { pos = vec3(0, 0, 0) }, { pos = vec3(9, 0, 0) } }"#, "curve"),
            (r#"shape = "river""#, "ribbon"),
        ] {
            let e = lua.load(format!("water.addVolume{{ {opts} }}")).exec().unwrap_err().to_string();
            assert!(e.contains(want), "`{opts}` said: {e}");
        }
        assert!(shared.declared.borrow().is_empty());
        // A flat bed is the same depth to the bank; with no depth there is no bed.
        let (flat, open): (bool, bool) = lua
            .load(
                r#"water.addVolume{ shape = "ribbon", flat = true, halfWidth = 4, depth = 2, bed = "flat",
                                    points = { { pos = vec3(0, 0, 0) }, { pos = vec3(20, 0, 0) } } }
                   water.addVolume{ shape = "ribbon", flat = true, halfWidth = 4,
                                    points = { { pos = vec3(0, 0, 50) }, { pos = vec3(20, 0, 50) } } }
                   return water.isUnderwater(10, -1.9, 3.9), water.isUnderwater(10, -300, 50)"#,
            )
            .eval()
            .unwrap();
        assert!(flat && open, "{flat} {open}");
    }

    /// **A ribbon lies flat unless it is declared on a world.** With no
    /// parent, or on a node that is not a world (a ship, a building), up is
    /// the parent's +Y and its points may lie anywhere, the origin included;
    /// on a node that is a world it lies round the world's centre; and
    /// `flat = true` or `flat = false` says which whatever it lies on.
    #[test]
    fn a_ribbon_is_flat_unless_its_parent_is_a_world() {
        let (lua, shared) = host();
        shared.worlds.borrow_mut().insert("Planet".into());
        lua.load(
            r#"local low = { { pos = vec3(0, 0, 0) }, { pos = vec3(60, 0, 0) } }
               local high = { { pos = vec3(0, 500, 0) }, { pos = vec3(60, 500, 0) } }
               local function river(parent, flat, points)
                 return water.addVolume{ shape = "ribbon", parent = parent, flat = flat, halfWidth = 3, depth = 2, points = points }
               end
               river(nil, nil, low)          -- 1: no parent
               river("Ship", nil, low)       -- 2: a parent that is not a world
               river("Planet", nil, high)    -- 3: a world
               river("Planet", true, high)   -- 4: a world, told flat
               river(nil, false, high)       -- 5: no parent, told round
               up = water.at(30, -1, 0).up"#,
        )
        .exec()
        .unwrap();
        let flat: Vec<bool> = shared
            .declared
            .borrow()
            .iter()
            .map(|w| match &w.surface {
                Surface::Ribbon { course, .. } => course.flat,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(flat, [true, true, false, true, false], "no parent, a ship, a world, a world told flat, no parent told round");
        assert_eq!(lua.globals().get::<crate::math_api::LuaVec3>("up").unwrap().0, DVec3::Y);
        // Round the world, up at the river's far end leans out along the
        // radial; flat, it is the parent's +Y there as everywhere.
        let d = shared.declared.borrow();
        let up = |i: usize| match &d[i].surface {
            Surface::Ribbon { course, .. } => course.stations.last().unwrap().up,
            _ => unreachable!(),
        };
        assert!(up(2).x > 0.1 && up(3) == DVec3::Y, "{:?} {:?}", up(2), up(3));
    }

    /// **On a round world a ribbon lies on the world.** Its points are in
    /// its parent's frame, about the parent's centre: up is the radial, the
    /// level `water.levelOf` reads is a radius, and its strip's normals
    /// point out from the centre.
    #[test]
    fn a_ribbon_on_a_round_world_is_radial() {
        let (lua, shared) = host();
        shared.worlds.borrow_mut().insert("Planet".into());
        let (depth, level, up): (f64, f64, crate::math_api::LuaVec3) = lua
            .load(
                r#"local function on(a, r) return vec3(math.sin(a) * r, math.cos(a) * r, 0) end
                   id = water.addVolume{ shape = "ribbon", parent = "Planet", halfWidth = 4, depth = 3, current = 2,
                                         points = { { pos = on(0, 500) }, { pos = on(0.2, 500) }, { pos = on(0.4, 499) } } }
                   local w = water.at(on(0.1, 498.5))
                   return w.depth, water.levelOf(id), w.up"#,
            )
            .eval()
            .unwrap();
        assert!((depth - 1.5).abs() < 1e-6 && level == 500.0, "{depth} {level}");
        assert!((up.0 - DVec3::new(0.1f64.sin(), 0.1f64.cos(), 0.0)).length() < 1e-9);
        let m = mesh_of(&shared, 4, &SurfaceLook::default());
        for (p, n) in m.positions.iter().zip(&m.normals) {
            assert!(p.normalize().dot(*n) > 0.999999 && p.length() > 498.99 && p.length() < 500.001, "{p:?} {n:?}");
        }
        assert_eq!(shared.declared.borrow()[0].anchor.as_deref(), Some("Planet"));
    }
}
