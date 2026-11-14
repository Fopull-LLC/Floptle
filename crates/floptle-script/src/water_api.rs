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

use floptle_core::water::{DeclaredWater, HeightGrid, Lay, Surface, Swell, Wave};
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
    /// Ids whose surface `water.setLevel` / `setLevels` moved, for the
    /// driver to hand the solver before the next tick.
    pub changed: Rc<RefCell<Vec<u32>>>,
    /// Holds, releases and water added to tiled sims, for the driver.
    pub sim_cmds: Rc<RefCell<Vec<floptle_core::water::SimCmd>>>,
    /// What each tiled sim's held groups have given since `water.exchanged`
    /// last asked, m³, by volume and group.
    pub exchanged: Rc<RefCell<std::collections::HashMap<u32, std::collections::BTreeMap<u16, f64>>>>,
    /// What they gave to fill the ground under newly held points since
    /// `water.filled` last asked, the same way.
    pub filled: Rc<RefCell<std::collections::HashMap<u32, std::collections::BTreeMap<u16, f64>>>>,
    /// Water entries and exits since `water.splashes` last asked, oldest
    /// first, at most 64.
    pub splashes: Rc<RefCell<std::collections::VecDeque<floptle_physics::splash::Splash>>>,
    /// The physics clock and when each node was last in water.
    pub wet: Rc<RefCell<(f64, std::collections::HashMap<u32, f64>)>>,
    /// Each body of water's slam (`water.setSlam`), by the water's entity.
    pub slam: Rc<RefCell<std::collections::HashMap<u32, f32>>>,
    /// The nodes that are round worlds, by name, refreshed by the driver
    /// before scripts run: a ribbon declared on one lies round it.
    pub worlds: Rc<RefCell<std::collections::HashSet<String>>>,
    pub next_id: Rc<std::cell::Cell<u32>>,
    /// The level, lent for the frame, so `water.surface` can tell which of a
    /// lake's sphere lies in its basin and which runs under the ground.
    pub colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    pub sim_origin: Rc<RefCell<glam::DVec3>>,
    /// What `water.surface` needs beyond the declared water itself.
    pub surfaces: SurfaceBook,
    /// The waves on each Water Volume node that has any, by entity,
    /// refreshed by the driver with `volumes`.
    pub swells: Rc<RefCell<std::collections::HashMap<u32, NodeSwell>>>,
    /// The simulation's clock every swell is read at, seconds: `simTime`.
    pub clock: Rc<std::cell::Cell<f64>>,
    /// `water.setSwell(node, waves)` requests, drained by the driver.
    pub swell_writes: Rc<RefCell<SwellWrites>>,
    /// `water.bindSwell(node, water)`: the water whose swell a shader on a
    /// node reads, by the node's entity.
    pub swell_binds: Rc<RefCell<std::collections::HashMap<u32, SwellSource>>>,
}

/// Waves scripts asked to be written to Water Volume nodes, by entity.
pub type SwellWrites = Vec<(u32, Vec<Wave>)>;

/// The waves on a Water Volume node, mirrored for scripts.
#[derive(Clone, Debug)]
pub struct NodeSwell {
    pub swell: std::sync::Arc<Swell>,
    /// The node's rotation: the frame the waves are laid in.
    pub rot: glam::DQuat,
}

/// A body of water named by a script: a Water Volume node, or water it
/// declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwellSource {
    Node(u32),
    Declared(u32),
}

impl WaterInfo {
    /// The swell over world point `p` on this volume, with the up there, or
    /// `None` outside a pool's walls.
    fn swell_over(&self, sw: &NodeSwell, p: glam::DVec3, t: f64) -> Option<(floptle_core::swell::Over, glam::DVec3, glam::DQuat)> {
        let d = p - glam::DVec3::from(self.center);
        if self.sea {
            let o = sw.swell.over(sw.rot.inverse() * d, Lay::Round { radius: self.radius }, t);
            Some((o, d.try_normalize().unwrap_or(glam::DVec3::Y), sw.rot))
        } else {
            let q = glam::DQuat::from_xyzw(self.rot[0], self.rot[1], self.rot[2], self.rot[3]);
            let local = q.inverse() * d;
            let h = glam::DVec3::from(self.half);
            if local.x.abs() > h.x || local.z.abs() > h.z || local.y < -h.y {
                return None;
            }
            Some((sw.swell.over(local, Lay::Flat { level: h.y }, t), q * glam::DVec3::Y, q))
        }
    }

    /// What a swell on this volume is laid on, and the turn from its frame
    /// to the world's.
    fn lay(&self, sw: &NodeSwell) -> (Lay, glam::DQuat) {
        if self.sea {
            (Lay::Round { radius: self.radius }, sw.rot)
        } else {
            (Lay::Flat { level: self.half[1] }, glam::DQuat::from_xyzw(self.rot[0], self.rot[1], self.rot[2], self.rot[3]))
        }
    }
}

/// What `water.surface` and `water.surfaceChanged` keep between calls: the
/// engine's own report on each simulated grid, and what each surface looked
/// like when a script last took it.
#[derive(Clone, Default)]
pub struct SurfaceBook {
    /// Per declared-water id, refreshed by the driver as the sim steps.
    pub(crate) reports: Rc<RefCell<std::collections::HashMap<u32, floptle_core::water::SimReport>>>,
    /// Per declared-water id, [`surface_stamp`] at the last `water.surface`.
    pub(crate) taken: Rc<RefCell<std::collections::HashMap<u32, u64>>>,
}

impl SurfaceBook {
    /// The driver's report on simulated water `id` this frame.
    pub fn set_report(&self, id: u32, report: floptle_core::water::SimReport) {
        self.reports.borrow_mut().insert(id, report);
    }

    /// Has the driver reported on simulated water `id` yet?
    pub fn has_report(&self, id: u32) -> bool {
        self.reports.borrow().contains_key(&id)
    }

    /// Forget water that is gone.
    pub fn retain(&self, live: &dyn Fn(u32) -> bool) {
        self.reports.borrow_mut().retain(|id, _| live(*id));
        self.taken.borrow_mut().retain(|id, _| live(*id));
    }
}

/// A number that changes when a surface does: its levels and flow for a
/// grid, its sphere for a lake. What `water.surfaceChanged` compares, so it
/// answers for water the engine moves, water a script re-levels and water
/// that never moves alike.
fn surface_stamp(surface: &Surface) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    match surface {
        Surface::Lake { radius, axis, reach } => {
            for v in [*radius, axis.x, axis.y, axis.z, *reach] {
                h.write_u64(v.to_bits());
            }
        }
        Surface::Grid(g) => {
            for l in &g.levels {
                h.write_u32(l.to_bits());
            }
            for f in &g.flow {
                h.write_u32(f[0].to_bits());
                h.write_u32(f[1].to_bits());
            }
        }
        // A tile's serial moves when its surface has, by more than the sim's
        // `stillDepth`.
        Surface::Tiles(t) => {
            let mut tiles: Vec<(u32, u64)> = t.tiles.iter().map(|(k, l)| (*k, l.serial)).collect();
            tiles.sort_unstable();
            for (k, serial) in tiles {
                h.write_u32(k);
                h.write_u64(serial);
            }
        }
        // A course never changes once declared; only its lift does.
        Surface::Ribbon { course, lift } => {
            h.write_u64(course.stamp);
            h.write_u64(lift.to_bits());
        }
    }
    h.finish()
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
    /// Out of the surface over the point: `up`, leant with a wave's face.
    normal: glam::DVec3,
    /// Metres the surface over the point stands above its still level.
    wave: f64,
}

/// The deepest water containing `p` across both kinds, by the same
/// "innermost wins" rule the solver uses. Water with a swell answers for
/// its moving surface, at the simulation's clock.
fn deepest_any(s: &WaterShared, p: [f64; 3]) -> Option<Hit> {
    let swells = s.swells.borrow();
    let still_up = |v: &WaterInfo| {
        if v.sea {
            (glam::DVec3::from(p) - glam::DVec3::from(v.center)).try_normalize().unwrap_or(glam::DVec3::Y)
        } else {
            glam::DQuat::from_xyzw(v.rot[0], v.rot[1], v.rot[2], v.rot[3]) * glam::DVec3::Y
        }
    };
    let still = |v: &WaterInfo, depth: f64| {
        let up = still_up(v);
        Hit { depth, density: v.density, frozen: v.frozen, node: Some(v.entity), volume: None, up, flow: glam::DVec3::ZERO, normal: up, wave: 0.0 }
    };
    let mut best = if swells.is_empty() {
        // No node has waves: the still answer, as it always was.
        deepest(&s.volumes.borrow(), p).map(|(v, d)| still(v, d))
    } else {
        let mut best: Option<Hit> = None;
        for v in s.volumes.borrow().iter() {
            let hit = match swells.get(&v.entity) {
                Some(sw) if !v.frozen => {
                    let Some((o, up, rot)) = v.swell_over(sw, glam::DVec3::from(p), s.clock.get()) else { continue };
                    Hit {
                        depth: (-o.above).max(0.0),
                        density: v.density,
                        frozen: v.frozen,
                        node: Some(v.entity),
                        volume: None,
                        up,
                        flow: rot * o.flow,
                        normal: rot * o.normal,
                        wave: o.height,
                    }
                }
                _ => still(v, v.depth_at(p)),
            };
            if hit.depth > 0.0 && best.as_ref().is_none_or(|b| hit.depth > b.depth) {
                best = Some(hit);
            }
        }
        best
    };
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
                normal: at.normal,
                wave: at.wave,
            });
        }
    }
    best
}

/// Every key one wave of a `waves` list reads.
pub(crate) const WAVE_KEYS: &[&str] = &["amplitude", "wavelength", "direction", "steepness", "speed", "phase"];

/// A `waves` list as the waves it names: `{ { amplitude = 1, wavelength = 40,
/// direction = {1, 0}, steepness = 0.6, speed = nil, phase = 0 }, ... }`.
/// `nil` and an empty list are no waves.
fn waves_from(v: &Value, call: &str) -> mlua::Result<Vec<Wave>> {
    let err = |m: String| mlua::Error::RuntimeError(format!("{call}: {m}"));
    let list = match v {
        Value::Nil => return Ok(Vec::new()),
        Value::Table(t) => t,
        _ => return Err(err("`waves` is a list of waves, each { amplitude, wavelength, direction, steepness }".into())),
    };
    let n = list.raw_len();
    if n > floptle_core::swell::MAX_WAVES {
        return Err(err(format!("`waves` has {n} waves; a body of water carries at most {}", floptle_core::swell::MAX_WAVES)));
    }
    let mut out = Vec::with_capacity(n);
    for i in 1..=n {
        let Value::Table(w) = list.raw_get::<Value>(i)? else {
            return Err(err(format!("wave {i} is a table: {{ amplitude, wavelength, direction, steepness }}")));
        };
        crate::opts::check_keys(&w, WAVE_KEYS, &format!("{call} waves[{i}]"))?;
        let num = |k: &str, or: f64| -> mlua::Result<f64> {
            match w.get::<Value>(k)? {
                Value::Nil => Ok(or),
                Value::Number(x) if x.is_finite() => Ok(x),
                Value::Integer(x) => Ok(x as f64),
                _ => Err(err(format!("wave {i}'s `{k}` is a number"))),
            }
        };
        let dflt = Wave::default();
        let amplitude = num("amplitude", dflt.amplitude as f64)?;
        let wavelength = num("wavelength", dflt.wavelength as f64)?;
        let steepness = num("steepness", dflt.steepness as f64)?;
        if amplitude < 0.0 || wavelength <= 0.0 || steepness < 0.0 {
            return Err(err(format!(
                "wave {i}: `amplitude` and `steepness` are 0 or more and `wavelength` is more than 0 metres"
            )));
        }
        let direction = match w.get::<Value>("direction")? {
            Value::Nil => dflt.direction,
            Value::Table(d) if d.raw_len() == 2 => [d.raw_get::<f32>(1)?, d.raw_get::<f32>(2)?],
            other => match crate::math_api::vec3_of(&other) {
                Some(d) => [d.x as f32, d.z as f32],
                None => return Err(err(format!("wave {i}'s `direction` is {{x, z}} along the water, or a vec3 whose x and z are read"))),
            },
        };
        if direction == [0.0, 0.0] || !direction[0].is_finite() || !direction[1].is_finite() {
            return Err(err(format!("wave {i}'s `direction` has no length")));
        }
        let speed = match w.get::<Value>("speed")? {
            Value::Nil => None,
            _ => Some(num("speed", 0.0)? as f32),
        };
        out.push(Wave {
            amplitude: amplitude as f32,
            wavelength: wavelength as f32,
            direction,
            steepness: steepness as f32,
            speed,
            phase: num("phase", 0.0)? as f32,
        });
    }
    Ok(out)
}

/// The water a script names: a Water Volume node, or the id of water it
/// declared.
fn source_of(v: &Value) -> Option<SwellSource> {
    match v {
        Value::Table(t) => t.raw_get::<u32>("__id").ok().map(SwellSource::Node),
        Value::Integer(i) => Some(SwellSource::Declared(*i as u32)),
        Value::Number(n) => Some(SwellSource::Declared(*n as u32)),
        _ => None,
    }
}

/// The swell on the water a script names, what it is laid on at world point
/// `p`, where its frame is and how that frame is turned.
fn swell_frame(s: &WaterShared, src: SwellSource, p: glam::DVec3) -> Option<(std::sync::Arc<Swell>, Lay, glam::DVec3, glam::DQuat, f64)> {
    match src {
        SwellSource::Node(e) => {
            let vols = s.volumes.borrow();
            let v = vols.iter().find(|v| v.entity == e)?;
            let swells = s.swells.borrow();
            let sw = swells.get(&e)?;
            let (lay, rot) = v.lay(sw);
            Some((sw.swell.clone(), lay, glam::DVec3::from(v.center), rot, s.clock.get()))
        }
        SwellSource::Declared(id) => {
            let d = s.declared.borrow();
            let w = d.iter().find(|w| w.id == id)?;
            let r = w.frame.rot;
            let rot = glam::DQuat::from_xyzw(r.x as f64, r.y as f64, r.z as f64, r.w as f64);
            Some((w.swell.clone()?, w.lay_at(p)?, w.frame.origin, rot, w.time))
        }
    }
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
    /// Per vertex, when asked for ([`SurfaceLook`]): r = the water's depth
    /// over its bed (metres / 16), g and b = its flow along the frame's x
    /// and z (m/s / 4, about 0.5, so 0.5 is still), a = foam, each 0..1.
    pub colors: Vec<[f32; 4]>,
    /// The most indices this surface can ever have: every cell wet.
    pub index_capacity: usize,
}

/// How deep colours reach: a depth of this many metres or more is r = 1.
pub(crate) const COLOR_DEPTH: f64 = 16.0;
/// How fast colours reach: a flow of this many m/s along an axis is the end
/// of the g or b lane.
pub(crate) const COLOR_FLOW: f64 = 4.0;

/// What `water.surface` is asked for beyond the triangles.
#[derive(Default)]
pub(crate) struct SurfaceLook<'a> {
    /// Every point of the surface's grid is a vertex, wet or dry, in the
    /// same order every time: the vertex count never changes, so the result
    /// rewrites a mesh in place (`mesh.update`). Triangles are still only
    /// where it is wet.
    pub fixed: bool,
    /// Per-vertex colours, from these.
    pub colors: Option<SurfaceColors<'a>>,
}

/// Where a surface's per-vertex colours come from.
pub(crate) struct SurfaceColors<'a> {
    /// The engine's simulation of the grid, when it runs one.
    pub report: Option<&'a floptle_core::water::SimReport>,
    /// The water's depth at a point of the surface in its frame, for water
    /// no simulation reports on.
    pub depth: &'a dyn Fn(glam::DVec3) -> f64,
    /// The current everywhere in it, on top of a grid's own flow.
    pub current: glam::Vec3,
}

pub(crate) fn surface_color(depth: f64, flow: [f64; 2], foam: f64) -> [f32; 4] {
    let lane = |v: f64| (0.5 + (v / COLOR_FLOW).clamp(-1.0, 1.0) * 0.5) as f32;
    [(depth / COLOR_DEPTH).clamp(0.0, 1.0) as f32, lane(flow[0]), lane(flow[1]), foam.clamp(0.0, 1.0) as f32]
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
#[cfg(test)]
pub(crate) fn surface_mesh_in(surface: &Surface, cells: usize, under: &dyn Fn(glam::DVec3) -> bool) -> SurfaceMesh {
    surface_mesh_with(surface, cells, under, &SurfaceLook::default())
}

/// [`surface_mesh_in`], with what `look` asks for: a fixed vertex layout,
/// per-vertex colours.
pub(crate) fn surface_mesh_with(
    surface: &Surface,
    cells: usize,
    under: &dyn Fn(glam::DVec3) -> bool,
    look: &SurfaceLook,
) -> SurfaceMesh {
    let mut m = SurfaceMesh {
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
        colors: Vec::new(),
        index_capacity: 0,
    };
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
                    if let Some(c) = &look.colors {
                        let depth = (c.depth)(dir * *radius);
                        m.colors.push(surface_color(depth, [c.current.x as f64, c.current.z as f64], 0.0));
                    }
                }
            }
            m.index_capacity = n * n * 6;
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
            // A fixed layout keeps every point of the disc, used or not.
            if !look.fixed {
                compact(&mut m);
            }
        }
        Surface::Grid(g) => {
            let mut index = vec![u32::MAX; g.columns * g.rows];
            let report = look.colors.as_ref().and_then(|c| c.report);
            // Where a dry point of a fixed layout sits: on the bed when the
            // sim says where that is, else level with the water there is, so
            // the mesh's bounds stay the water's.
            let wet_mean = {
                let (sum, n) = g.levels.iter().filter(|l| l.is_finite()).fold((0.0f64, 0u32), |(s, n), l| (s + *l as f64, n + 1));
                if n > 0 { sum / n as f64 } else { 0.0 }
            };
            for j in 0..g.rows {
                for i in 0..g.columns {
                    let k = j * g.columns + i;
                    let l = g.levels[k];
                    let wet = l.is_finite();
                    if !wet && !look.fixed {
                        continue;
                    }
                    let ground = report.and_then(|r| r.ground.get(k)).copied().filter(|v| v.is_finite());
                    let y = if wet { l as f64 } else { ground.map_or(wet_mean, f64::from) };
                    let (x, z) = (g.x0 + i as f64 * g.cell, g.z0 + j as f64 * g.cell);
                    if wet {
                        index[k] = m.positions.len() as u32;
                    }
                    m.positions.push(glam::DVec3::new(x, y, z));
                    m.normals.push(glam::DVec3::Y);
                    m.uvs.push([x, z]);
                    if let Some(c) = &look.colors {
                        let depth = match (wet, ground) {
                            (false, _) => 0.0,
                            (true, Some(ground)) => (l - ground).max(0.0) as f64,
                            (true, None) => (c.depth)(glam::DVec3::new(x, y, z)),
                        };
                        let own = g.flow.get(k).copied().unwrap_or([0.0; 2]);
                        let flow = if wet { [(own[0] + c.current.x) as f64, (own[1] + c.current.z) as f64] } else { [0.0; 2] };
                        let foam = if wet { report.and_then(|r| r.foam.get(k)).copied().unwrap_or(0.0) } else { 0.0 };
                        m.colors.push(surface_color(depth, flow, foam as f64));
                    }
                }
            }
            push_quads(&mut m.indices, &index, g.columns, g.rows);
            m.index_capacity = g.columns.saturating_sub(1) * g.rows.saturating_sub(1) * 6;
        }
        Surface::Tiles(t) => tiles_mesh(&mut m, t, None, look),
        Surface::Ribbon { course, lift } => crate::water_ribbon::ribbon_mesh(&mut m, course, *lift, cells, look),
    }
    m
}

/// The wet cells of tiled water as triangles, every tile or one: a vertex
/// at each wet cell's centre at its level, and the triangles between them,
/// laid on the patch so a round world's water is round. A tile's mesh takes
/// in the row and column of the tiles past it, so the tiles meet.
pub(crate) fn tiles_mesh(m: &mut SurfaceMesh, t: &floptle_core::water::TiledSurface, only: Option<u32>, look: &SurfaceLook) {
    use floptle_core::water::TILE;
    let p = &t.patch;
    let mut keys: Vec<u32> = t.tiles.keys().copied().filter(|k| only.is_none_or(|o| o == *k)).collect();
    keys.sort_unstable();
    for key in keys {
        let (ti, tj) = ((key & 0xffff) as usize, (key >> 16) as usize);
        let side = TILE + 1;
        let mut index = vec![u32::MAX; side * side];
        for y in 0..side {
            for x in 0..side {
                let (i, j) = (ti * TILE + x, tj * TILE + y);
                let Some((level, depth, flow)) = t.cell(i as i64, j as i64) else { continue };
                let (gx, gz) = (i as f64, j as f64);
                index[y * side + x] = m.positions.len() as u32;
                let up = p.up_at(gx, gz);
                m.positions.push(p.point(gx, gz, level as f64));
                m.normals.push(up);
                if let Some(c) = &look.colors {
                    // The cell's flow runs along the patch's columns and
                    // rows; a colour carries it along the frame's x and z.
                    let (e1, e2) = p.along(up);
                    let along = e1 * flow[0] as f64 + e2 * flow[1] as f64;
                    let speed = (flow[0] * flow[0] + flow[1] * flow[1]).sqrt();
                    let foam = floptle_core::water::foam(depth, speed) as f64;
                    let current = c.current.as_dvec3();
                    m.colors.push(surface_color(depth as f64, [along.x + current.x, along.z + current.z], foam));
                }
                m.uvs.push([(gx - (p.columns.max(1) - 1) as f64 * 0.5) * p.cell, (gz - (p.rows.max(1) - 1) as f64 * 0.5) * p.cell]);
            }
        }
        push_quads(&mut m.indices, &index, side, side);
    }
    compact(m);
}

/// Drop the vertices no triangle uses, renumbering the rest.
fn compact(m: &mut SurfaceMesh) {
    let mut used = vec![u32::MAX; m.positions.len()];
    let mut out = SurfaceMesh {
        positions: Vec::new(),
        normals: Vec::new(),
        uvs: Vec::new(),
        indices: Vec::new(),
        colors: Vec::new(),
        index_capacity: m.index_capacity,
    };
    for &i in &m.indices {
        let i = i as usize;
        if used[i] == u32::MAX {
            used[i] = out.positions.len() as u32;
            out.positions.push(m.positions[i]);
            out.normals.push(m.normals[i]);
            out.uvs.push(m.uvs[i]);
            if let Some(c) = m.colors.get(i) {
                out.colors.push(*c);
            }
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
    "roughness", "springs", "holdEdges", "size", "depths", "held", "solver", "flat", "budget", "state", "minDepth", "soak", "soakBelow", "stillDepth",
    "points", "halfWidth", "bed", "curve",
    "waves",
];

/// The most cells a tiled sim may span, held or not: 4,096 a side.
pub const MAX_TILED_SIDE: usize = 4096;

/// The most points a simulated volume may have: a few hundred metres of lake
/// at a couple of metres a point, stepped every frame.
pub const MAX_SIM_POINTS: usize = 128 * 128;

/// A `water.addVolume` options table as the water it declares.
fn declared_from(opts: &Table, id: u32, worlds: &std::collections::HashSet<String>) -> mlua::Result<DeclaredWater> {
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
        // Water the engine moves, stored in tiles only where it is, on a
        // patch of a round world about `center` (or of a plane, `flat`).
        "sim" if !matches!(opts.get::<Value>("center")?, Value::Nil) => {
            let (surface, spec) = tiled_from(opts, CALL)?;
            sim_level = Some(spec);
            surface
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
            let origin = vec("origin").unwrap_or(glam::DVec3::ZERO);
            let roughness = num("roughness").unwrap_or(0.0);
            if !(0.0..=1.0).contains(&roughness) {
                return Err(err("`roughness` is the bed's Manning roughness, 0 to 1 (0.035 is a natural river bed)"));
            }
            // Springs: a river's head, at the point nearest where each is put.
            let mut springs = Vec::new();
            if let Some(list) = opts.get::<Option<Table>>("springs").map_err(|_| err("`springs` is a list of { pos = vec3, rate = m³/s }"))? {
                for entry in list.sequence_values::<Table>() {
                    let e = entry.map_err(|_| err("`springs` is a list of { pos = vec3, rate = m³/s }"))?;
                    crate::opts::check_keys(&e, &["pos", "rate"], "water.addVolume springs")?;
                    let p = crate::math_api::vec3_of(&e.get::<Value>("pos")?)
                        .ok_or_else(|| err("a spring's `pos` is a vec3 in the parent's frame"))?;
                    let rate: f64 = e.get::<Option<f64>>("rate")?.ok_or_else(|| err("a spring needs `rate`, m³/s"))?;
                    let (i, j) = (((p.x - origin.x) / cell).round(), ((p.z - origin.z) / cell).round());
                    if i < 0.0 || j < 0.0 || i >= columns as f64 || j >= rows as f64 || rate.is_nan() || rate < 0.0 {
                        return Err(err("a spring is a `pos` inside the sim's grid with a `rate` of 0 or more m³/s"));
                    }
                    springs.push((j as usize * columns + i as usize, rate as f32));
                }
            }
            let hold_edges = opts.get::<Option<bool>>("holdEdges").map_err(|_| err("`holdEdges` is true or false"))?.unwrap_or(false);
            sim_level = Some(floptle_core::water::SimSpec {
                level: level as f32,
                roughness: roughness as f32,
                springs,
                hold_edges,
                falls: Vec::new(),
                tiled: None,
            });
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
        // A river: a line of points down its channel.
        "ribbon" => {
            // It lies round a world it is declared on, and flat anywhere
            // else, unless it says which itself.
            let on_world = matches!(opts.get::<Value>("parent")?, Value::String(n) if worlds.contains(&*n.to_str()?));
            crate::water_ribbon::ribbon_from(opts, CALL, on_world)?
        }
        other => return Err(err(&format!("`shape` is \"lake\", \"heightfield\", \"sim\" or \"ribbon\", not \"{other}\""))),
    };
    let anchor = match opts.get::<Value>("parent")? {
        Value::Nil => None,
        Value::String(n) => Some(n.to_str()?.to_string()),
        _ => return Err(err("`parent` is the name of the node the water lies on")),
    };
    let waves = waves_from(&opts.get::<Value>("waves")?, CALL)?;
    let mut water = DeclaredWater {
        id,
        anchor,
        frame: floptle_core::scatter::Frame::IDENTITY,
        surface,
        density: num("density").unwrap_or(1000.0).max(1e-3) as f32,
        drag: num("drag").unwrap_or(1.0).max(0.0) as f32,
        angular_drag: num("angularDrag").unwrap_or(1.0).max(0.0) as f32,
        current: vec("current").unwrap_or(glam::DVec3::ZERO).as_vec3(),
        floor: match num("depth") {
            // A ribbon's `depth` is its channel's, shaped from bank to bank.
            _ if shape == "ribbon" => f64::INFINITY,
            Some(d) if d > 0.0 => d,
            Some(_) => return Err(err("`depth` is how far below the surface the bed lies, in metres, more than 0")),
            None => f64::INFINITY,
        },
        look: look_from(opts, CALL)?,
        simulated: sim_level,
        swell: None,
        time: 0.0,
    };
    // A river's surface slopes and turns with its course: nothing to lay
    // waves on. Said here rather than left to float things on a flat river
    // a game believes has waves.
    if !waves.is_empty() && matches!(water.surface, Surface::Ribbon { .. }) {
        return Err(err("a ribbon carries no swell: `waves` are for a lake, a heightfield or a sim"));
    }
    water.swell = water.swell_of(&waves);
    Ok(water)
}

/// A cell of tiled water named by a script: a point in the parent's frame
/// (the cell under it), or `{i, j}`, its column and row from the patch's
/// corner, counted from 0.
fn cell_of(patch: &floptle_core::water::Patch, v: &Value) -> Option<[u32; 2]> {
    let (i, j) = if let Some(p) = crate::math_api::vec3_of(v) {
        let (gx, gz, _, _) = patch.locate(p)?;
        (gx.round(), gz.round())
    } else if let Value::Table(t) = v {
        (t.raw_get::<f64>(1).ok()?, t.raw_get::<f64>(2).ok()?)
    } else {
        return None;
    };
    (i >= 0.0 && j >= 0.0 && (i as usize) < patch.columns && (j as usize) < patch.rows).then_some([i as u32, j as u32])
}

/// A list of cells (or one point) a script named, every one on the patch.
fn cells_of(patch: &floptle_core::water::Patch, v: &Value, call: &str) -> mlua::Result<Vec<[u32; 2]>> {
    let err = || mlua::Error::RuntimeError(format!("{call}: `cells` is a list of points (vec3, in the parent's frame) or {{i, j}} cells on the sim"));
    if crate::math_api::vec3_of(v).is_some() {
        return Ok(vec![cell_of(patch, v).ok_or_else(err)?]);
    }
    let Value::Table(t) = v else { return Err(err()) };
    let mut out = Vec::new();
    for e in t.sequence_values::<Value>() {
        out.push(cell_of(patch, &e?).ok_or_else(err)?);
    }
    Ok(out)
}

/// The `addVolume` options of a tiled sim: its surface (empty until the
/// engine fills it) and how it starts.
fn tiled_from(opts: &Table, call: &str) -> mlua::Result<(Surface, floptle_core::water::SimSpec)> {
    use floptle_core::water::{HeldCell, Patch, SimSpec, TiledSpec, TiledSurface};
    let err = |m: &str| mlua::Error::RuntimeError(format!("{call}: {m}"));
    let num = |k: &str| opts.get::<Option<f64>>(k).ok().flatten();
    let center = crate::math_api::vec3_of(&opts.get::<Value>("center")?)
        .ok_or_else(|| err("a sim's `center` is a point on the ground in the parent's frame, where the patch is laid"))?;
    let cell = num("cell").ok_or_else(|| err("a sim needs `cell`, the metres between points"))?;
    if cell <= 0.0 {
        return Err(err("`cell` is more than 0 metres"));
    }
    let (columns, rows) = match num("size") {
        Some(size) => {
            let n = (size / cell).round() as usize + 1;
            (n, n)
        }
        None => (
            num("columns").ok_or_else(|| err("a sim needs `size` (metres across) or `columns` and `rows`"))? as usize,
            num("rows").ok_or_else(|| err("a sim needs `size` (metres across) or `columns` and `rows`"))? as usize,
        ),
    };
    if columns < 2 || rows < 2 || columns > MAX_TILED_SIDE || rows > MAX_TILED_SIDE {
        return Err(err(&format!("a sim with a `center` is 2 to {MAX_TILED_SIDE} points a side")));
    }
    let flat = opts.get::<Option<bool>>("flat").map_err(|_| err("`flat` is true or false"))?.unwrap_or(false);
    let patch = if flat { Patch::flat(center, cell, columns, rows) } else { Patch::sphere(center, cell, columns, rows) };
    let (fill, level) = match opts.get::<Value>("level")? {
        Value::Number(n) => (true, n),
        Value::Integer(n) => (true, n as f64),
        Value::Boolean(false) => (false, patch.base()),
        _ => return Err(err("a sim needs `level`: the height it is filled to (a radius on a round world), or false to start dry")),
    };
    let depths = match opts.get::<Option<Table>>("depths").map_err(|_| err("`depths` is metres per point"))? {
        None => None,
        Some(d) => {
            if d.raw_len() != columns * rows {
                return Err(err(&format!("`depths` has {} entries for {columns} × {rows} points", d.raw_len())));
            }
            let mut v = Vec::with_capacity(columns * rows);
            for i in 1..=columns * rows {
                v.push(match d.raw_get::<Value>(i)? {
                    Value::Number(n) => (n as f32).max(0.0),
                    Value::Integer(n) => (n as f32).max(0.0),
                    _ => 0.0,
                });
            }
            Some(std::sync::Arc::new(v))
        }
    };
    let roughness = num("roughness").unwrap_or(0.0);
    if !(0.0..=1.0).contains(&roughness) {
        return Err(err("`roughness` is the bed's Manning roughness, 0 to 1 (0.035 is a natural river bed)"));
    }
    let mut springs = Vec::new();
    if let Some(list) = opts.get::<Option<Table>>("springs").map_err(|_| err("`springs` is a list of { pos = vec3, rate = m³/s }"))? {
        for entry in list.sequence_values::<Table>() {
            let e = entry.map_err(|_| err("`springs` is a list of { pos = vec3, rate = m³/s }"))?;
            crate::opts::check_keys(&e, &["pos", "rate"], "water.addVolume springs")?;
            let c = cell_of(&patch, &e.get::<Value>("pos")?).ok_or_else(|| err("a spring's `pos` is a point on the sim, in the parent's frame"))?;
            let rate: f64 = e.get::<Option<f64>>("rate")?.ok_or_else(|| err("a spring needs `rate`, m³/s"))?;
            if rate.is_nan() || rate < 0.0 {
                return Err(err("a spring's `rate` is 0 or more m³/s"));
            }
            springs.push((c[1] as usize * columns + c[0] as usize, rate as f32));
        }
    }
    let mut held = Vec::new();
    if let Some(list) = opts.get::<Option<Table>>("held").map_err(|_| err("`held` is a list of { pos = vec3 or cell = {i, j}, level =, group = }"))? {
        for entry in list.sequence_values::<Table>() {
            let e = entry.map_err(|_| err("`held` is a list of { pos = vec3 or cell = {i, j}, level =, group = }"))?;
            crate::opts::check_keys(&e, &["pos", "cell", "level", "group"], "water.addVolume held")?;
            let at = match e.get::<Value>("pos")? {
                Value::Nil => e.get::<Value>("cell")?,
                p => p,
            };
            let c = cell_of(&patch, &at).ok_or_else(|| err("a held cell is a `pos` or a `cell` on the sim"))?;
            let l: f64 = e.get::<Option<f64>>("level")?.unwrap_or(level);
            let group: u16 = e.get::<Option<u16>>("group").map_err(|_| err("a held cell's `group` is a whole number, 0 to 65535"))?.unwrap_or(1);
            held.push(HeldCell { i: c[0], j: c[1], level: l as f32, group });
        }
    }
    let solver = opts.get::<Option<String>>("solver").map_err(|_| err("`solver` is \"swe\" or \"pipes\""))?;
    let pipes = match solver.as_deref() {
        None | Some("swe") => false,
        Some("pipes") => true,
        Some(other) => return Err(err(&format!("`solver` is \"swe\" (the default: water with momentum) or \"pipes\", not \"{other}\""))),
    };
    let budget_ms = num("budget").unwrap_or(1.0).max(0.05) as f32;
    let hold_edges = opts.get::<Option<bool>>("holdEdges").map_err(|_| err("`holdEdges` is true or false"))?.unwrap_or(false);
    // A saved state: what water.state handed back.
    let state = match opts.get::<Option<Table>>("state").map_err(|_| err("`state` is what water.state returned"))? {
        None => None,
        Some(t) => {
            let cells: Table = t.get("cells").map_err(|_| err("`state` is what water.state returned: { cells = { i, j, depth, ... } }"))?;
            let n = cells.raw_len();
            if !n.is_multiple_of(3) {
                return Err(err("a state's `cells` is column, row, depth for each wet point"));
            }
            let mut v = Vec::with_capacity(n / 3);
            for k in 0..n / 3 {
                let (i, j, d): (f64, f64, f64) = (cells.raw_get(k * 3 + 1)?, cells.raw_get(k * 3 + 2)?, cells.raw_get(k * 3 + 3)?);
                if i >= 0.0 && j >= 0.0 && (i as usize) < columns && (j as usize) < rows && d > 0.0 {
                    v.push((i as u32, j as u32, d as f32));
                }
            }
            Some(std::sync::Arc::new(v))
        }
    };
    let min_depth = num("minDepth").unwrap_or(1e-3).max(1e-5) as f32;
    let soak = num("soak").unwrap_or(0.0).max(0.0) as f32;
    let soak_below = num("soakBelow").unwrap_or(0.01).max(0.0) as f32;
    let still_depth = num("stillDepth").unwrap_or(5e-4);
    if !(0.0..=0.1).contains(&still_depth) {
        return Err(err("`stillDepth` is the surface change that counts, in metres: 0 to 0.1 (0.0005 by default)"));
    }
    let spec = SimSpec {
        level: level as f32,
        roughness: roughness as f32,
        springs,
        hold_edges,
        falls: Vec::new(),
        tiled: Some(TiledSpec {
            patch,
            fill: fill && depths.is_none() && state.is_none(),
            depths,
            held,
            pipes,
            budget_ms,
            hold_edges,
            state,
            min_depth,
            soak,
            soak_below,
            still_depth: still_depth as f32,
        }),
    };
    let surface = Surface::Tiles(std::sync::Arc::new(TiledSurface { patch, tiles: Default::default() }));
    Ok((surface, spec))
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

/// A declared water's level as `water.levelOf` reads it.
fn level_of(w: &DeclaredWater) -> Option<f64> {
    match &w.surface {
        Surface::Lake { radius, .. } => Some(*radius),
        Surface::Grid(g) => {
            let wet: Vec<f64> = g.levels.iter().filter(|l| l.is_finite()).map(|l| *l as f64).collect();
            (!wet.is_empty()).then(|| wet.iter().sum::<f64>() / wet.len() as f64)
        }
        Surface::Tiles(t) => t.mean_level(),
        Surface::Ribbon { course, lift } => Some(course.level() + lift),
    }
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
            // The surface over the point: which way it faces, and how far a
            // wave has lifted it from its still level.
            out.set("normal", crate::math_api::LuaVec3(h.normal))?;
            out.set("wave", h.wave)?;
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
            let mut w = declared_from(&opts, id, &s.worlds.borrow())?;
            w.time = s.clock.get();
            s.next_id.set(id);
            s.declared.borrow_mut().push(w);
            Ok(id)
        }) {
            let _ = t.set("addVolume", f);
        }
    }

    // water.setSwell(water, waves): the waves on a body of water, replacing
    // any it had; nil or {} for a still surface. `water` is a Water Volume
    // node or the id of declared water. The solver, `water.at`, splashes and
    // a shader's `waterSwell` all read the one list.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (water, waves): (Value, Value)| {
            const CALL: &str = "water.setSwell";
            let waves = waves_from(&waves, CALL)?;
            match source_of(&water) {
                Some(SwellSource::Node(e)) => {
                    s.swell_writes.borrow_mut().push((e, waves));
                    Ok(true)
                }
                Some(SwellSource::Declared(id)) => {
                    let mut d = s.declared.borrow_mut();
                    let Some(w) = d.iter_mut().find(|w| w.id == id) else { return Ok(false) };
                    if !waves.is_empty() && matches!(w.surface, Surface::Ribbon { .. }) {
                        return Err(mlua::Error::RuntimeError(format!(
                            "{CALL}: a ribbon carries no swell: waves are for a lake, a heightfield or a sim"
                        )));
                    }
                    w.swell = w.swell_of(&waves);
                    w.time = s.clock.get();
                    s.changed.borrow_mut().push(id);
                    Ok(true)
                }
                None => Err(mlua::Error::RuntimeError(format!(
                    "{CALL}(water, waves): `water` is a Water Volume node or the id water.addVolume gave"
                ))),
            }
        }) {
            let _ = t.set("setSwell", f);
        }
    }

    // water.swell(water) → the waves on it, as setSwell takes them, each with
    // the `speed` it travels at; nil for a still surface.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, water: Value| {
            let swell = match source_of(&water) {
                Some(SwellSource::Node(e)) => s.swells.borrow().get(&e).map(|sw| sw.swell.clone()),
                Some(SwellSource::Declared(id)) => s.declared.borrow().iter().find(|w| w.id == id).and_then(|w| w.swell.clone()),
                None => {
                    return Err(mlua::Error::RuntimeError(
                        "water.swell(water): `water` is a Water Volume node or the id water.addVolume gave".into(),
                    ));
                }
            };
            let Some(swell) = swell else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            for (i, w) in swell.waves().iter().enumerate() {
                let row = lua.create_table()?;
                row.set("amplitude", w.amplitude as f64)?;
                row.set("wavelength", w.wavelength as f64)?;
                let dir = lua.create_table()?;
                dir.raw_set(1, w.direction[0] as f64)?;
                dir.raw_set(2, w.direction[1] as f64)?;
                row.set("direction", dir)?;
                row.set("steepness", w.steepness as f64)?;
                row.set("speed", swell.speed(i))?;
                row.set("phase", w.phase as f64)?;
                out.raw_set(i + 1, row)?;
            }
            Ok(Value::Table(out))
        }) {
            let _ = t.set("swell", f);
        }
    }

    // water.swellAt(water, pos) → offset, normal: where the swell has moved
    // the point of the still surface under (or over) `pos`, and the
    // surface's normal there, both in the world. Exactly what a shader's
    // waterSwell gives the vertex at that point, for a script that places
    // foam, a buoy or a mesh of its own on the drawn surface. nil without a
    // swell.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, (water, pos): (Value, Value)| {
            use mlua::IntoLua;
            let (Some(src), Some(p)) = (source_of(&water), crate::math_api::vec3_of(&pos)) else {
                return Err(mlua::Error::RuntimeError(
                    "water.swellAt(water, pos): a Water Volume node or a declared water's id, and a vec3".into(),
                ));
            };
            let Some((swell, lay, origin, rot, time)) = swell_frame(&s, src, p) else {
                return Ok((Value::Nil, Value::Nil));
            };
            let (offset, normal) = swell.offset(rot.inverse() * (p - origin), lay, time);
            Ok((crate::math_api::LuaVec3(rot * offset).into_lua(lua)?, crate::math_api::LuaVec3(rot * normal).into_lua(lua)?))
        }) {
            let _ = t.set("swellAt", f);
        }
    }

    // water.bindSwell(node, water): the shader on `node` reads `water`'s
    // swell (waterSwell, waterSwellNormal). Without it a node reads the
    // Water Volume it is or is under, or the declared water on its parent
    // whose surface it is nearest. nil for `water` goes back to that.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (node, water): (Value, Value)| {
            let err = || {
                mlua::Error::RuntimeError(
                    "water.bindSwell(node, water): a node, and a Water Volume node or a declared water's id (nil to unbind)".into(),
                )
            };
            let Some(SwellSource::Node(e)) = source_of(&node).filter(|_| matches!(node, Value::Table(_))) else { return Err(err()) };
            match &water {
                Value::Nil => {
                    s.swell_binds.borrow_mut().remove(&e);
                }
                other => {
                    s.swell_binds.borrow_mut().insert(e, source_of(other).ok_or_else(err)?);
                }
            }
            Ok(())
        }) {
            let _ = t.set("bindSwell", f);
        }
    }

    // water.surface(id [, { cells = 48, colors = false, fixed = false,
    // buffers = false }]) → the arrays mesh.build takes, for the surface of
    // declared water in its parent's frame: so the lake that is drawn is the
    // lake `water.at` answers for, and a game writes no mesh code to fit one.
    //
    // `colors` adds what a water shader reads per vertex as `vertexColor`:
    // depth, flow and foam. `fixed` keeps every point of the grid as a
    // vertex so the count never changes, and the answer goes to mesh.update
    // as it stands. `buffers` hands the arrays over as buffers, which
    // mesh.build and mesh.update read in one copy.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, (id, opts): (u32, Option<Table>)| {
            const CALL: &str = "water.surface";
            if let Some(o) = &opts {
                crate::opts::check_keys(o, &["cells", "colors", "fixed", "buffers", "tile"], CALL)?;
            }
            let flag = |key: &str| -> mlua::Result<bool> {
                Ok(match &opts {
                    Some(o) => crate::opts::opt_bool(o, CALL, key)?.unwrap_or(false),
                    None => false,
                })
            };
            let (colors, fixed, buffers) = (flag("colors")?, flag("fixed")?, flag("buffers")?);
            let tile = opts.as_ref().and_then(|o| o.get::<Option<u32>>("tile").ok().flatten());
            let cells = opts.as_ref().and_then(|o| o.get::<Option<f64>>("cells").ok().flatten()).unwrap_or(48.0);
            let cells = (cells.round() as usize).clamp(2, 512);
            let declared = s.declared.borrow();
            let Some(w) = declared.iter().find(|w| w.id == id) else { return Ok(Value::Nil) };
            // Under the ground is a point the level has a surface above,
            // within the lake's own reach. With no level lent (a host with no
            // physics) nothing is, and the lake is its whole disc.
            let cols = s.colliders.borrow();
            let origin = *s.sim_origin.borrow();
            let up_at = |local: glam::DVec3| match &w.surface {
                Surface::Lake { .. } => (w.frame.to_world(local.normalize_or_zero()) - w.frame.origin).normalize_or_zero(),
                Surface::Grid(_) => (w.frame.to_world(glam::DVec3::Y) - w.frame.origin).normalize_or_zero(),
                Surface::Tiles(t) => {
                    let up = t.patch.locate(local).map_or(t.patch.up, |(_, _, _, up)| up);
                    (w.frame.to_world(up) - w.frame.origin).normalize_or_zero()
                }
                Surface::Ribbon { course, .. } => (w.frame.to_world(course.up_at(local)) - w.frame.origin).normalize_or_zero(),
            };
            let under = |local: glam::DVec3| -> bool {
                if cols.is_empty() {
                    return false;
                }
                let at = w.frame.to_world(local);
                let up = (w.frame.to_world(local.normalize_or_zero()) - w.frame.origin).normalize_or_zero();
                let height = match &w.surface {
                    Surface::Lake { reach, .. } => reach.max(10.0),
                    Surface::Grid(g) => (g.cell * g.columns.max(g.rows) as f64).max(10.0),
                    Surface::Tiles(t) => (t.patch.cell * t.patch.columns.max(t.patch.rows) as f64).max(10.0),
                    // A river is drawn where it was laid, ground or no.
                    Surface::Ribbon { .. } => return false,
                };
                let from = (at + up * height - origin).as_vec3();
                floptle_physics::raycast_colliders(&cols, from, -up.as_vec3(), height as f32 - 0.01, !0).is_some()
            };
            // The water's depth at a point of the surface, for water no sim
            // reports on: down to the ground the level has there, no deeper
            // than the water's own bed or than a colour can say.
            let reach = w.floor.min(COLOR_DEPTH);
            let depth = |local: glam::DVec3| -> f64 {
                if cols.is_empty() {
                    return reach;
                }
                let from = (w.frame.to_world(local) - origin).as_vec3();
                floptle_physics::raycast_colliders(&cols, from, -up_at(local).as_vec3(), reach as f32, !0)
                    .map_or(reach, |hit| f64::from(hit.distance))
            };
            let reports = s.surfaces.reports.borrow();
            let look = SurfaceLook {
                fixed,
                colors: colors.then(|| SurfaceColors { report: reports.get(&id), depth: &depth, current: w.current }),
            };
            if fixed && matches!(w.surface, Surface::Tiles(_)) {
                return Err(mlua::Error::runtime(
                    "water.surface: `fixed` is for a lake or a heightfield; a tiled sim's tiles are each a mesh of their own (`tile = key`)",
                ));
            }
            let m = match (&w.surface, tile) {
                // One tile of tiled water: what a game rebuilds when that
                // tile's serial moves.
                (Surface::Tiles(t), Some(key)) => {
                    let mut m = SurfaceMesh { positions: Vec::new(), normals: Vec::new(), uvs: Vec::new(), indices: Vec::new(), colors: Vec::new(), index_capacity: 0 };
                    tiles_mesh(&mut m, t, Some(key), &look);
                    m
                }
                // A river's `cells` count from bank to bank, a few by default.
                (Surface::Ribbon { .. }, _) => {
                    let across = opts.as_ref().and_then(|o| o.get::<Option<f64>>("cells").ok().flatten());
                    let across = across.map_or(crate::water_ribbon::CELLS_ACROSS, |c| c.round().clamp(2.0, 64.0) as usize);
                    surface_mesh_with(&w.surface, across, &under, &look)
                }
                _ => surface_mesh_with(&w.surface, cells, &under, &look),
            };
            s.surfaces.taken.borrow_mut().insert(id, surface_stamp(&w.surface));
            let out = lua.create_table()?;
            // A list of numbers, or the same numbers as a buffer of f32s.
            let floats = |vals: &mut dyn Iterator<Item = f64>| -> mlua::Result<Value> {
                if buffers {
                    let bytes: Vec<u8> = vals.flat_map(|v| (v as f32).to_le_bytes()).collect();
                    return Ok(Value::Buffer(lua.create_buffer(bytes)?));
                }
                let t = lua.create_table()?;
                for (i, v) in vals.enumerate() {
                    t.raw_set(i + 1, v)?;
                }
                Ok(Value::Table(t))
            };
            out.set("positions", floats(&mut m.positions.iter().flat_map(|p| p.to_array()))?)?;
            out.set("normals", floats(&mut m.normals.iter().flat_map(|n| n.to_array()))?)?;
            out.set("uvs", floats(&mut m.uvs.iter().flat_map(|u| *u))?)?;
            if colors {
                out.set("colors", floats(&mut m.colors.iter().flat_map(|c| c.map(f64::from)))?)?;
            }
            // A list numbers vertices from 1, a buffer of u32s from 0: what
            // mesh.build reads each as.
            if buffers {
                let bytes: Vec<u8> = m.indices.iter().flat_map(|i| i.to_le_bytes()).collect();
                out.set("indices", lua.create_buffer(bytes)?)?;
            } else {
                let t = lua.create_table()?;
                for (i, v) in m.indices.iter().enumerate() {
                    t.raw_set(i + 1, f64::from(*v) + 1.0)?;
                }
                out.set("indices", t)?;
            }
            if fixed {
                out.set("dynamic", true)?;
                out.set("indexCapacity", m.index_capacity)?;
            }
            Ok(Value::Table(out))
        }) {
            let _ = t.set("surface", f);
        }
    }

    // water.surfaceChanged(id) → has this water's surface changed since
    // `water.surface(id)` was last called? True before the first call, and
    // for an id that is not there; false over a lake that stands still, so a
    // game rebuilds or updates a sheet only when there is something new.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let declared = s.declared.borrow();
            let Some(w) = declared.iter().find(|w| w.id == id) else { return Ok(true) };
            Ok(s.surfaces.taken.borrow().get(&id) != Some(&surface_stamp(&w.surface)))
        }) {
            let _ = t.set("surfaceChanged", f);
        }
    }

    // water.falls([id]) → every place simulated water pours off a drop of
    // more than 1.5 m: { pos, dir, width, discharge, drop, volume }, in the
    // world. Where a game puts a fall sheet and its mist.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, id: Option<u32>| {
            let out = lua.create_table()?;
            let mut n = 0;
            for w in s.declared.borrow().iter().filter(|w| id.is_none_or(|i| i == w.id)) {
                let Some(sim) = &w.simulated else { continue };
                for fall in &sim.falls {
                    let row = lua.create_table()?;
                    row.set("pos", crate::math_api::LuaVec3(fall.pos))?;
                    row.set("dir", crate::math_api::LuaVec3(fall.dir.as_dvec3()))?;
                    row.set("width", fall.width)?;
                    row.set("discharge", fall.discharge)?;
                    row.set("drop", fall.drop)?;
                    row.set("volume", w.id)?;
                    n += 1;
                    out.raw_set(n, row)?;
                }
            }
            Ok(out)
        }) {
            let _ = t.set("falls", f);
        }
    }

    // water.setLevel(id, level): move a declared body of water's surface in
    // place, its id, look, density, drag, current and depth kept. A lake's
    // `level` is its surface sphere's radius; a heightfield's wet points all
    // go to `level`; a sim's held points (and the level it fills to, if it
    // has not filled yet) go to `level`. Cheap enough to call every frame.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, level): (u32, f64)| {
            let err = |m: &str| mlua::Error::RuntimeError(format!("water.setLevel: {m}"));
            if !level.is_finite() {
                return Err(err("`level` is a number: a lake's radius, a heightfield's or a sim's height"));
            }
            let mut d = s.declared.borrow_mut();
            let Some(w) = d.iter_mut().find(|w| w.id == id) else { return Ok(false) };
            match (&mut w.surface, &mut w.simulated) {
                (_, Some(sim)) => sim.level = level as f32,
                (Surface::Lake { radius, .. }, None) => {
                    if level <= 0.0 {
                        return Err(err("a lake's level is its surface's radius from its parent's centre, more than 0"));
                    }
                    *radius = level;
                }
                (Surface::Grid(g), None) => {
                    for l in std::sync::Arc::make_mut(g).levels.iter_mut().filter(|l| l.is_finite()) {
                        *l = level as f32;
                    }
                }
                // Tiled water is always a sim's.
                (Surface::Tiles(_), None) => return Ok(false),
                // A river keeps its slope: every point rises or falls by
                // what brings its first point to `level`.
                (Surface::Ribbon { course, lift }, None) => *lift = level - course.level(),
            }
            s.changed.borrow_mut().push(id);
            Ok(true)
        }) {
            let _ = t.set("setLevel", f);
        }
    }

    // water.setLevels(id, levels): a heightfield's levels, one per point as
    // `addVolume` took them (`false` for dry ground).
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, levels): (u32, Table)| {
            let err = |m: &str| mlua::Error::RuntimeError(format!("water.setLevels: {m}"));
            let mut d = s.declared.borrow_mut();
            let Some(w) = d.iter_mut().find(|w| w.id == id) else { return Ok(false) };
            let (Surface::Grid(g), None) = (&mut w.surface, &w.simulated) else {
                return Err(err("only a heightfield has a level per point; water.setLevel moves a lake or a sim"));
            };
            let g = std::sync::Arc::make_mut(g);
            if levels.raw_len() != g.levels.len() {
                return Err(err(&format!("`levels` has {} entries for {} points", levels.raw_len(), g.levels.len())));
            }
            for (i, l) in g.levels.iter_mut().enumerate() {
                *l = match levels.raw_get::<Value>(i + 1)? {
                    Value::Number(n) => n as f32,
                    Value::Integer(n) => n as f32,
                    _ => f32::NAN,
                };
            }
            s.changed.borrow_mut().push(id);
            Ok(true)
        }) {
            let _ = t.set("setLevels", f);
        }
    }

    // water.levelOf(id) → its level now: a lake's radius, the mean of a
    // heightfield's wet points, a sim's mean surface where it is wet. nil
    // when there is no such volume, or none of it is wet.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let d = s.declared.borrow();
            let Some(w) = d.iter().find(|w| w.id == id) else { return Ok(None) };
            Ok(level_of(w))
        }) {
            let _ = t.set("levelOf", f);
        }
    }

    // water.hold(id, cells, level [, group]): hold tiled sim cells' surface
    // at `level` (a radius on a round world): the sea at a coast, a lake at
    // its live level, a river's rows at their surface. What they give or
    // take is counted in `group` (1 by default) for water.exchanged.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, cells, level, group): (u32, Value, f64, Option<u16>)| {
            let d = s.declared.borrow();
            let Some(patch) = d.iter().find(|w| w.id == id).and_then(|w| w.simulated.as_ref()?.tiled.as_ref()).map(|t| t.patch) else {
                return Ok(false);
            };
            let cells = cells_of(&patch, &cells, "water.hold")?;
            if !level.is_finite() {
                return Err(mlua::Error::RuntimeError("water.hold: `level` is a number, a radius on a round world".into()));
            }
            s.sim_cmds.borrow_mut().push(floptle_core::water::SimCmd::Hold { id, cells, level: level as f32, group: group.unwrap_or(1) });
            Ok(true)
        }) {
            let _ = t.set("hold", f);
        }
    }

    // water.release(id, cells): let held cells go; the water there flows
    // again.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, cells): (u32, Value)| {
            let d = s.declared.borrow();
            let Some(patch) = d.iter().find(|w| w.id == id).and_then(|w| w.simulated.as_ref()?.tiled.as_ref()).map(|t| t.patch) else {
                return Ok(false);
            };
            let cells = cells_of(&patch, &cells, "water.release")?;
            s.sim_cmds.borrow_mut().push(floptle_core::water::SimCmd::Release { id, cells });
            Ok(true)
        }) {
            let _ = t.set("release", f);
        }
    }

    // water.exchanged(id) → { [group] = m³ }: what each group of held cells
    // gave the water (negative: took from it) since the last ask.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let out = lua.create_table()?;
            if let Some(groups) = s.exchanged.borrow_mut().remove(&id) {
                for (g, v) in groups {
                    out.raw_set(g, v)?;
                }
            }
            Ok(out)
        }) {
            let _ = t.set("exchanged", f);
        }
    }

    // water.filled(id) → { [group] = m³ }: what each group gave to fill the
    // ground under newly held cells, from when they were first held until
    // the group came to rest, since the last ask. Counted apart from
    // water.exchanged, so a lake started dry from held points does not read
    // as a lake draining.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let out = lua.create_table()?;
            if let Some(groups) = s.filled.borrow_mut().remove(&id) {
                for (g, v) in groups {
                    out.raw_set(g, v)?;
                }
            }
            Ok(out)
        }) {
            let _ = t.set("filled", f);
        }
    }

    // water.tiles(id) → { { key, serial, i, j }, ... }: a tiled sim's tiles
    // holding water, each with a serial that moves when its surface has
    // moved by more than `stillDepth`.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let d = s.declared.borrow();
            let Some(Surface::Tiles(tiles)) = d.iter().find(|w| w.id == id).map(|w| &w.surface) else { return Ok(Value::Nil) };
            let mut keys: Vec<&u32> = tiles.tiles.keys().collect();
            keys.sort_unstable();
            let out = lua.create_table()?;
            for (n, k) in keys.into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("key", *k)?;
                row.set("serial", tiles.tiles[k].serial)?;
                row.set("i", (k & 0xffff) as usize * floptle_core::water::TILE)?;
                row.set("j", (k >> 16) as usize * floptle_core::water::TILE)?;
                out.raw_set(n + 1, row)?;
            }
            Ok(Value::Table(out))
        }) {
            let _ = t.set("tiles", f);
        }
    }

    // water.state(id) → { columns, rows, cells = { i, j, depth, ... } }: a
    // tiled sim's wet points, for a save. addVolume{ state = ... } starts
    // from it.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let d = s.declared.borrow();
            let Some(Surface::Tiles(tiles)) = d.iter().find(|w| w.id == id).map(|w| &w.surface) else { return Ok(Value::Nil) };
            let out = lua.create_table()?;
            out.set("columns", tiles.patch.columns)?;
            out.set("rows", tiles.patch.rows)?;
            let cells = lua.create_table()?;
            let mut keys: Vec<&u32> = tiles.tiles.keys().collect();
            keys.sort_unstable();
            let mut n = 0;
            let side = floptle_core::water::TILE;
            for k in keys {
                let (ti, tj) = ((k & 0xffff) as usize, (k >> 16) as usize);
                for (c, depth) in tiles.tiles[k].depth.iter().enumerate() {
                    if *depth > 0.0 {
                        cells.raw_set(n + 1, ti * side + c % side)?;
                        cells.raw_set(n + 2, tj * side + c / side)?;
                        cells.raw_set(n + 3, *depth as f64)?;
                        n += 3;
                    }
                }
            }
            out.set("cells", cells)?;
            Ok(Value::Table(out))
        }) {
            let _ = t.set("state", f);
        }
    }

    // water.addWater(id, at, m3): add water to a tiled sim at a point (or an
    // {i, j} cell), or take it if negative: seepage, rain, a pump.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, at, volume): (u32, Value, f64)| {
            let d = s.declared.borrow();
            let Some(patch) = d.iter().find(|w| w.id == id).and_then(|w| w.simulated.as_ref()?.tiled.as_ref()).map(|t| t.patch) else {
                return Ok(false);
            };
            let cell = cell_of(&patch, &at)
                .ok_or_else(|| mlua::Error::RuntimeError("water.addWater: `at` is a point on the sim (vec3, in the parent's frame) or an {i, j} cell".into()))?;
            if volume.is_finite() && volume != 0.0 {
                s.sim_cmds.borrow_mut().push(floptle_core::water::SimCmd::AddWater { id, cell, volume });
            }
            Ok(true)
        }) {
            let _ = t.set("addWater", f);
        }
    }

    // water.setSpring(id, at, rate): a spring at a point of a tiled sim
    // giving `rate` m³/s, changed or (at 0) taken away while it runs.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, at, rate): (u32, Value, f64)| {
            let d = s.declared.borrow();
            let Some(patch) = d.iter().find(|w| w.id == id).and_then(|w| w.simulated.as_ref()?.tiled.as_ref()).map(|t| t.patch) else {
                return Ok(false);
            };
            let cell = cell_of(&patch, &at)
                .ok_or_else(|| mlua::Error::RuntimeError("water.setSpring: `at` is a point on the sim (vec3, in the parent's frame) or an {i, j} cell".into()))?;
            if !rate.is_finite() || rate < 0.0 {
                return Err(mlua::Error::RuntimeError("water.setSpring: `rate` is 0 or more m³/s (0 takes the spring away)".into()));
            }
            s.sim_cmds.borrow_mut().push(floptle_core::water::SimCmd::Spring { id, cell, rate: rate as f32 });
            Ok(true)
        }) {
            let _ = t.set("setSpring", f);
        }
    }

    // water.splashes() → every time something entered or left water since the
    // last call, oldest first (at most 64): { node, volume or water, pos,
    // normal, speed, mass, area, displaced, entering }. Where a game puts its
    // splash, its sound, its ripple and its damage.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (n, e) in s.splashes.borrow_mut().drain(..).enumerate() {
                let row = lua.create_table()?;
                row.set("node", crate::env::new_node_handle(lua, e.node)?)?;
                // Declared water is numbered down from the top; a Water
                // Volume node is itself.
                if e.water > u32::MAX / 2 {
                    row.set("volume", u32::MAX - e.water)?;
                } else {
                    row.set("water", crate::env::new_node_handle(lua, e.water)?)?;
                }
                row.set("pos", crate::math_api::LuaVec3(e.pos))?;
                row.set("normal", crate::math_api::LuaVec3(e.normal.as_dvec3()))?;
                row.set("speed", e.speed as f64)?;
                row.set("mass", e.mass as f64)?;
                row.set("area", e.area as f64)?;
                row.set("displaced", e.displaced as f64)?;
                row.set("entering", e.entering)?;
                out.raw_set(n + 1, row)?;
            }
            Ok(out)
        }) {
            let _ = t.set("splashes", f);
        }
    }

    // water.setSlam(volume, k): what something entering this water fast
    // meets: an impulse k·ρ·v²·area·dt against its motion while it goes
    // under, never more than stops it. `volume` is an addVolume id or a
    // Water Volume node; 0 (the default) is none.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (volume, k): (Value, f64)| {
            let key = match &volume {
                Value::Integer(i) => Some(u32::MAX - *i as u32),
                Value::Number(n) => Some(u32::MAX - *n as u32),
                Value::Table(t) => t.raw_get::<u32>("__id").ok(),
                _ => None,
            };
            let Some(key) = key else {
                return Err(mlua::Error::RuntimeError("water.setSlam(volume, k): `volume` is an id water.addVolume gave, or a Water Volume node".into()));
            };
            if !k.is_finite() || k < 0.0 {
                return Err(mlua::Error::RuntimeError("water.setSlam: `k` is 0 or more (1 is a hard slam)".into()));
            }
            let mut m = s.slam.borrow_mut();
            if k == 0.0 {
                m.remove(&key);
            } else {
                m.insert(key, k as f32);
            }
            Ok(())
        }) {
            let _ = t.set("setSlam", f);
        }
    }

    // water.wet(node) → seconds since the node (a body, an assembly or one of
    // its parts) was last in water: 0 while it is, nil if it never was.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, node: Value| {
            let eid = match &node {
                Value::Table(t) => t.raw_get::<u32>("__id").ok(),
                Value::Integer(i) => Some(*i as u32),
                _ => None,
            };
            let Some(eid) = eid else { return Err(mlua::Error::RuntimeError("water.wet(node): `node` is a node".into())) };
            let w = s.wet.borrow();
            Ok(w.1.get(&eid).map(|t| (w.0 - t).max(0.0)))
        }) {
            let _ = t.set("wet", f);
        }
    }

    // water.removeVolume(id) → true if there was one.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let mut d = s.declared.borrow_mut();
            let before = d.len();
            d.retain(|w| w.id != id);
            s.surfaces.retain(&|live| live != id);
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
            swells: Rc::default(),
            clock: Rc::default(),
            swell_writes: Rc::default(),
            swell_binds: Rc::default(),
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

    /// A sim takes a river bed's roughness, springs at the points nearest
    /// where they are put, and a held edge; a spring off the grid is refused.
    /// `water.falls` hands back what the engine reported, in the world.
    #[test]
    fn a_sim_takes_springs_and_reports_its_falls() {
        let (lua, shared) = host();
        let id: u32 = lua
            .load(
                r#"return water.addVolume{ shape = "sim", origin = vec3(-4, 0, -2), cell = 2, columns = 5, rows = 3, level = 0,
                                          roughness = 0.035, holdEdges = true, springs = { { pos = vec3(-4.4, 1, 1.8), rate = 2 } } }"#,
            )
            .eval()
            .unwrap();
        {
            let d = shared.declared.borrow();
            let sim = d[0].simulated.as_ref().expect("a sim");
            assert_eq!((sim.roughness, sim.hold_edges), (0.035, true));
            // (-4.4, 1.8) is nearest the point at column 0, row 2.
            assert_eq!(sim.springs, vec![(2 * 5, 2.0)]);
        }
        let off = lua.load(r#"water.addVolume{ shape = "sim", cell = 1, columns = 2, rows = 2, level = 0, springs = { { pos = vec3(9, 0, 9), rate = 1 } } }"#).exec();
        assert!(off.is_err_and(|e| e.to_string().contains("spring")), "a spring off the grid was taken");
        let fall = floptle_core::water::WaterFall { pos: glam::DVec3::new(1.0, 2.0, 3.0), dir: glam::Vec3::X, width: 2.0, discharge: 1.5, drop: 4.0 };
        shared.declared.borrow_mut()[0].simulated.as_mut().unwrap().falls = vec![fall];
        let (n, drop, x, vol): (usize, f64, f64, u32) = lua
            .load("local f = water.falls() return #f, f[1].drop, f[1].pos.x, f[1].volume")
            .eval()
            .unwrap();
        assert_eq!((n, drop, x, vol), (1, 4.0, 1.0, id));
        let none: usize = lua.load(format!("return #water.falls({})", id + 7)).eval().unwrap();
        assert_eq!(none, 0, "another volume's falls were handed back");
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

    /// A 3 × 2 grid a sim has run: level 1 over a bed that steps down, the
    /// last point of the first row dry, water moving along +x.
    fn run_grid() -> (Surface, floptle_core::water::SimReport) {
        let nan = f32::NAN;
        let grid = HeightGrid {
            x0: 0.0,
            z0: 0.0,
            cell: 1.0,
            columns: 3,
            rows: 2,
            levels: vec![1.0, 1.0, nan, 1.0, 1.0, 1.0],
            flow: vec![[2.0, 0.0], [2.0, 0.0], [0.0, 0.0], [-4.0, 1.0], [0.0, 0.0], [9.0, -9.0]],
        };
        let report = floptle_core::water::SimReport {
            ground: vec![0.0, -3.0, 1.5, -15.0, -40.0, 0.5],
            foam: vec![0.0, 0.25, 0.9, 1.0, 0.0, 0.5],
        };
        (Surface::Grid(std::sync::Arc::new(grid)), report)
    }

    /// **A simulated surface carries depth, flow and foam per vertex.** Red
    /// is the water's depth over its bed (16 m and deeper is 1), green and
    /// blue its flow along x and z about 0.5 (±4 m/s the ends), alpha the
    /// foam: what a water shader reads as `vertexColor` with no pass over
    /// the grid in Lua. The volume's own current is added to the flow.
    #[test]
    fn a_simulated_surface_carries_depth_flow_and_foam_per_vertex() {
        let (surface, report) = run_grid();
        let depth = |_: glam::DVec3| -> f64 { panic!("a sim's depth is its own, never a ray") };
        let look = |current: glam::Vec3| SurfaceLook {
            fixed: false,
            colors: Some(SurfaceColors { report: Some(&report), depth: &depth, current }),
        };
        let m = surface_mesh_with(&surface, 0, &|_| false, &look(glam::Vec3::ZERO));
        assert_eq!(m.positions.len(), 5, "the dry point is left out");
        assert_eq!(m.colors.len(), 5, "one colour a vertex");
        let near = |a: [f32; 4], b: [f32; 4]| a.iter().zip(&b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(near(m.colors[0], [1.0 / 16.0, 0.75, 0.5, 0.0]), "{:?}", m.colors[0]);
        assert!(near(m.colors[1], [4.0 / 16.0, 0.75, 0.5, 0.25]), "{:?}", m.colors[1]);
        // The second row: 16 m deep exactly, flowing back along x and across.
        assert!(near(m.colors[2], [1.0, 0.0, 0.625, 1.0]), "{:?}", m.colors[2]);
        assert!(near(m.colors[3], [1.0, 0.5, 0.5, 0.0]), "deeper than a colour can say is 1: {:?}", m.colors[3]);
        assert!(near(m.colors[4], [0.5 / 16.0, 1.0, 0.0, 0.5]), "a flow past the ends is held at them: {:?}", m.colors[4]);
        // No colours asked for: none made.
        assert!(surface_mesh(&surface, 0).colors.is_empty());
        // A current everywhere in the volume rides on the grid's own flow.
        let m = surface_mesh_with(&surface, 0, &|_| false, &look(glam::Vec3::new(-2.0, 7.0, 2.0)));
        assert!(near(m.colors[0], [1.0 / 16.0, 0.5, 0.75, 0.0]), "{:?}", m.colors[0]);
    }

    /// **A fixed layout never changes its vertex count.** Every point of the
    /// grid is a vertex, the dry one lying on the bed with no depth, no flow
    /// and no triangle through it; when it floods, only the triangles and
    /// the values change. That is what lets `mesh.update` rewrite the sheet.
    #[test]
    fn a_fixed_surface_keeps_every_point_wet_or_dry() {
        let (surface, report) = run_grid();
        let depth = |_: glam::DVec3| 0.0;
        let look = SurfaceLook {
            fixed: true,
            colors: Some(SurfaceColors { report: Some(&report), depth: &depth, current: glam::Vec3::ZERO }),
        };
        let m = surface_mesh_with(&surface, 0, &|_| false, &look);
        assert_eq!(m.positions.len(), 6, "every point of a 3 × 2 grid");
        assert_eq!(m.positions[2], glam::DVec3::new(2.0, 1.5, 0.0), "the dry point lies on the bed");
        assert_eq!(m.colors[2], [0.0, 0.5, 0.5, 0.0], "and carries nothing");
        assert_eq!(m.indices.len(), 3 * 3);
        assert!(m.indices.iter().all(|&i| i != 2), "a triangle runs through the dry point");
        assert_eq!(m.index_capacity, 2 * 6, "room for both cells, wet");

        // Flooded: the same six vertices, more triangles.
        let Surface::Grid(g) = &surface else { unreachable!() };
        let mut wet = (**g).clone();
        wet.levels[2] = 1.75;
        let m2 = surface_mesh_with(&Surface::Grid(std::sync::Arc::new(wet)), 0, &|_| false, &look);
        assert_eq!(m2.positions.len(), 6);
        assert_eq!(m2.indices.len(), 4 * 3);
        assert_eq!(m2.positions[2].y, 1.75);
        assert!((m2.colors[2][0] - 0.25 / 16.0).abs() < 1e-6);

        // A lake's fixed layout keeps its whole disc, basin or not.
        let lake = Surface::Lake { radius: 805.0, axis: glam::DVec3::Y, reach: 60.0 };
        let along = |p: glam::DVec3| p.normalize().dot(glam::DVec3::Y).clamp(-1.0, 1.0).acos() * 805.0;
        let rim = |p: glam::DVec3| (30.0..40.0).contains(&along(p));
        let fixed = SurfaceLook { fixed: true, colors: None };
        let (whole, basin) = (surface_mesh_with(&lake, 16, &|_| false, &fixed), surface_mesh_with(&lake, 16, &rim, &fixed));
        assert_eq!(whole.positions.len(), basin.positions.len());
        assert!(basin.indices.len() < whole.indices.len());
        assert_eq!(whole.index_capacity, 16 * 16 * 6);
    }

    /// **Water no sim reports on takes its depth from the ground under it.**
    /// The depth function is asked at each wet vertex; a lake's flow is the
    /// volume's current and it has no foam.
    #[test]
    fn standing_water_takes_its_depth_from_what_it_is_asked() {
        let lake = Surface::Lake { radius: 805.0, axis: glam::DVec3::Y, reach: 20.0 };
        let depth = |p: glam::DVec3| if p.x > 0.0 { 8.0 } else { 2.0 };
        let look = SurfaceLook {
            fixed: false,
            colors: Some(SurfaceColors { report: None, depth: &depth, current: glam::Vec3::new(1.0, 0.0, 0.0) }),
        };
        let m = surface_mesh_with(&lake, 8, &|_| false, &look);
        assert_eq!(m.colors.len(), m.positions.len());
        for (p, c) in m.positions.iter().zip(&m.colors) {
            let want = if p.x > 0.0 { 0.5 } else { 0.125 };
            assert_eq!(*c, [want, 0.625, 0.5, 0.0], "at {p:?}");
        }
    }

    /// **From Lua the sheet is built once and rewritten.** `fixed` marks the
    /// answer `dynamic` with room for every cell, so it goes to `mesh.build`
    /// and then to `mesh.update` as it stands, as lists or as buffers; and
    /// `water.surfaceChanged` says when there is something new to write.
    #[test]
    fn a_fixed_surface_builds_a_mesh_once_and_updates_it() {
        let (lua, shared) = host();
        let book: Rc<RefCell<crate::mesh_api::MeshBook>> = Rc::default();
        crate::mesh_api::install(&lua, book.clone()).unwrap();
        lua.load(
            r#"id = water.addVolume{ shape = "heightfield", origin = vec3(0, 0, 0), cell = 1, columns = 3, rows = 2,
                                      levels = { 1, 1, 0/0, 1, 1, 1 } }
               before = water.surfaceChanged(id)
               local s = water.surface(id, { fixed = true, colors = true })
               verts, tris, cap, lists = #s.positions // 3, #s.indices // 3, s.indexCapacity, type(s.positions) == "table"
               sheet = mesh.build(s)
               after = water.surfaceChanged(id)"#,
        )
        .exec()
        .unwrap();
        let g = lua.globals();
        assert_eq!(
            (g.get::<i64>("verts").unwrap(), g.get::<i64>("tris").unwrap(), g.get::<i64>("cap").unwrap(), g.get::<bool>("lists").unwrap()),
            (6, 3, 12, true)
        );
        assert!(g.get::<bool>("before").unwrap(), "a surface nobody has taken is new");
        assert!(!g.get::<bool>("after").unwrap(), "a surface just taken has not changed");
        let built = book.borrow_mut().take_requests().remove(0);
        assert_eq!((built.positions.len(), built.index_capacity, built.colors.is_some()), (6, 12, true));

        // The dry point floods (what the engine's sim does each frame).
        {
            let mut d = shared.declared.borrow_mut();
            let Surface::Grid(old) = &d[0].surface else { unreachable!() };
            let mut wet = (**old).clone();
            wet.levels[2] = 1.5;
            d[0].surface = Surface::Grid(std::sync::Arc::new(wet));
        }
        lua.load(
            r#"changed = water.surfaceChanged(id)
               local s = water.surface(id, { fixed = true, colors = true, buffers = true })
               bufs = type(s.positions) == "buffer" and type(s.indices) == "buffer" and type(s.colors) == "buffer"
               mesh.update(sheet, s)
               still = water.surfaceChanged(id)
               gone = water.surfaceChanged(999)"#,
        )
        .exec()
        .unwrap();
        assert!(g.get::<bool>("changed").unwrap(), "the flood went unnoticed");
        assert!(g.get::<bool>("bufs").unwrap(), "`buffers = true` gave lists");
        assert!(!g.get::<bool>("still").unwrap());
        assert!(g.get::<bool>("gone").unwrap(), "water that is not there has nothing to keep");
        let u = book.borrow_mut().take_updates().remove(0);
        assert_eq!(u.vertices.unwrap().positions[2], [2.0, 1.5, 0.0]);
        assert_eq!(u.indices.unwrap().len(), 4 * 3, "the flooded cell's triangles");
        assert_eq!(u.colors.unwrap().len(), 6);
        assert_eq!(book.borrow().live.len(), 1, "the update made a second mesh");

        let e = lua.load("water.surface(id, { colours = true })").exec().unwrap_err().to_string();
        assert!(e.contains("colours"), "{e}");
    }

    /// **`water.setLevel` moves a body of water's surface in place.** A
    /// lake takes a new radius, a heightfield's wet points all go to the
    /// level (its dry ones stay dry), `setLevels` sets them one by one, and
    /// `levelOf` reads it back; the id, the count of volumes and everything
    /// else about the water stay, and the driver is told which moved.
    #[test]
    fn set_level_moves_a_surface_in_place() {
        let (lua, shared) = host();
        let (lake, field, before, after, mean, n): (u32, u32, f64, f64, f64, i64) = lua
            .load(
                r#"local lake = water.addVolume{ shape = "lake", center = vec3(0, 805, 0), reach = 60, depth = 6 }
                   local field = water.addVolume{ shape = "heightfield", cell = 2, columns = 2, rows = 2, levels = { 3, 3, false, 1 } }
                   local before = water.depthAt(0, 803, 0)
                   assert(water.setLevel(lake, 806))
                   assert(water.setLevel(field, 5))
                   assert(not water.isUnderwater(0, 4, 2), "a dry point was wetted")
                   assert(water.setLevel(999, 3) == false)
                   return lake, field, before, water.depthAt(0, 803, 0), water.levelOf(field), #water.volumes()"#,
            )
            .eval()
            .unwrap();
        assert!((before - 2.0).abs() < 1e-9 && (after - 3.0).abs() < 1e-9, "{before} {after}");
        assert!((mean - 5.0).abs() < 1e-9, "{mean}");
        assert_eq!(n, 0);
        assert_eq!(shared.declared.borrow().len(), 2);
        assert_eq!(shared.declared.borrow()[0].id, lake);
        assert_eq!(shared.declared.borrow()[0].floor, 6.0, "the lake's bed went");
        let mut changed = shared.changed.borrow().clone();
        changed.sort_unstable();
        assert_eq!(changed, vec![lake, field]);
        let level: f64 = lua.load(format!("return water.levelOf({lake})")).eval().unwrap();
        assert_eq!(level, 806.0);
        let (one, dry): (f64, bool) = lua
            .load(format!(
                "water.setLevels({field}, {{ 2, false, 2, 2 }}) return water.depthAt(1, 1, 1), water.isUnderwater(2, 1.5, 0)"
            ))
            .eval()
            .unwrap();
        assert!((one - 1.0).abs() < 1e-9 && !dry, "{one} {dry}");
        let err = lua.load(format!("water.setLevels({field}, {{ 1, 2 }})")).exec().unwrap_err();
        assert!(err.to_string().contains("`levels` has 2 entries"), "{err}");
        let err = lua.load(format!("water.setLevels({lake}, {{ 1 }})")).exec().unwrap_err();
        assert!(err.to_string().contains("heightfield"), "{err}");
        let err = lua.load(format!("water.setLevel({lake}, -1)")).exec().unwrap_err();
        assert!(err.to_string().contains("radius"), "{err}");
    }

    /// **Moving a lake's level costs next to nothing**, so a game can do it
    /// every frame while a lake drains: a thousand calls among twenty
    /// volumes take well under the 0.02 ms a frame the card allows for one
    /// a frame.
    #[test]
    fn set_level_is_cheap() {
        let (lua, _) = host();
        let ms: f64 = lua
            .load(
                r#"local ids = {}
                   for i = 1, 20 do ids[i] = water.addVolume{ shape = "lake", center = vec3(0, 800 + i, 0), reach = 60 } end
                   local t = os.clock()
                   for k = 1, 1000 do water.setLevel(ids[20], 820 + k * 0.001) end
                   return (os.clock() - t) * 1000 / 1000"#,
            )
            .eval()
            .unwrap();
        assert!(ms < 0.02, "one setLevel took {ms} ms");
    }

    /// **A sim declared with a `center` lies on a round world, in tiles.**
    /// Its patch is laid at the centre point, its level is a radius, it can
    /// start dry, held points and springs land on the cells under them, and
    /// water.hold, water.release and water.exchanged reach the engine and
    /// come back; water.tiles and water.surface read its tiles.
    #[test]
    fn a_tiled_sim_is_declared_held_and_read_by_its_tiles() {
        let (lua, shared) = host();
        let id: u32 = lua
            .load(
                r#"return water.addVolume{ shape = "sim", parent = "Planet", center = vec3(0, 0, 800), cell = 1.25, size = 640,
                                          level = false, held = { { pos = vec3(0, 0, 800), level = 801, group = 4 } },
                                          springs = { { pos = vec3(10, 0, 800), rate = 2 } } }"#,
            )
            .eval()
            .unwrap();
        let patch = {
            let d = shared.declared.borrow();
            let sim = d[0].simulated.as_ref().unwrap();
            let t = sim.tiled.as_ref().expect("a tiled sim");
            assert!(!t.fill && !t.pipes);
            assert_eq!(t.still_depth, 5e-4, "the surface change that counts is half a millimetre unless `stillDepth` says");
            assert_eq!((t.patch.columns, t.patch.rows), (513, 513));
            assert_eq!(t.patch.radius, Some(800.0));
            assert_eq!(t.held, vec![floptle_core::water::HeldCell { i: 256, j: 256, level: 801.0, group: 4 }]);
            assert_eq!(sim.springs.len(), 1);
            assert!(matches!(d[0].surface, Surface::Tiles(_)));
            t.patch
        };
        let ok: bool = lua.load(format!("return water.hold({id}, {{ vec3(0, 0, 800), {{3, 4}} }}, 800.5) and water.release({id}, vec3(0, 0, 800))")).eval().unwrap();
        assert!(ok);
        let cmds = shared.sim_cmds.borrow().clone();
        assert_eq!(
            cmds,
            vec![
                floptle_core::water::SimCmd::Hold { id, cells: vec![[256, 256], [3, 4]], level: 800.5, group: 1 },
                floptle_core::water::SimCmd::Release { id, cells: vec![[256, 256]] },
            ]
        );
        let err = lua.load(format!("water.hold({id}, {{ vec3(0, 0, -800) }}, 1)")).exec().unwrap_err();
        assert!(err.to_string().contains("cells"), "{err}");
        shared.exchanged.borrow_mut().entry(id).or_default().insert(4, -2.5);
        let (given, again): (f64, bool) =
            lua.load(format!("local e = water.exchanged({id}) return e[4], next(water.exchanged({id})) == nil")).eval().unwrap();
        assert!(given == -2.5 && again, "{given} {again}");
        // What filled the ground under newly held points is counted apart.
        shared.filled.borrow_mut().entry(id).or_default().insert(4, 40.0);
        let (fill, exchanged, again): (f64, bool, bool) = lua
            .load(format!("local e = next(water.exchanged({id})) == nil local f = water.filled({id}) return f[4], e, next(water.filled({id})) == nil"))
            .eval()
            .unwrap();
        assert!(fill == 40.0 && exchanged && again, "{fill} {exchanged} {again}");
        // A tile of water as the engine hands it back.
        let mut level = vec![f32::NAN; floptle_core::water::TILE * floptle_core::water::TILE];
        for c in [0, 1, floptle_core::water::TILE, floptle_core::water::TILE + 1] {
            level[c] = 0.5;
        }
        let tile = floptle_core::water::TileLevels { depth: level.iter().map(|l| if l.is_finite() { 1.0 } else { 0.0 }).collect(), flow: vec![[0.0; 2]; level.len()], level, serial: 9 };
        let key = floptle_core::water::tile_key(8, 8);
        let tiles = floptle_core::water::TiledSurface { patch, tiles: [(key, std::sync::Arc::new(tile))].into_iter().collect() };
        shared.declared.borrow_mut()[0].surface = Surface::Tiles(std::sync::Arc::new(tiles));
        lua.globals().set("patchPoint", crate::math_api::LuaVec3(patch.point(256.4, 256.4, 0.2))).unwrap();
        let (n, serial, i, verts, tris, depth): (i64, i64, i64, i64, i64, f64) = lua
            .load(format!(
                r#"local t = water.tiles({id})
                   local m = water.surface({id}, {{ tile = t[1].key }})
                   return #t, t[1].serial, t[1].i, #m.positions // 3, #m.indices // 3, water.depthAt(patchPoint)"#
            ))
            .eval()
            .unwrap();
        assert_eq!((n, serial, i, verts, tris), (1, 9, 256, 4, 2), "the tile's mesh is its four wet points, two triangles");
        assert!((depth - 0.3).abs() < 1e-6, "{depth}");

        // A tile's colours carry its depth as a sheet's do; `fixed` is
        // refused (a tile is its own mesh); and the surface has changed
        // exactly when a tile's serial moves.
        let (n, r, g, a, still): (i64, f64, f64, f64, bool) = lua
            .load(format!(
                r#"local c = water.surface({id}, {{ tile = water.tiles({id})[1].key, colors = true }}).colors
                   return #c, c[1], c[2], c[4], water.surfaceChanged({id})"#
            ))
            .eval()
            .unwrap();
        assert_eq!(n, 4 * 4, "one colour a vertex");
        assert!((r - 1.0 / 16.0).abs() < 1e-6 && (g - 0.5).abs() < 1e-6 && a == 0.0, "{r} {g} {a}");
        assert!(!still, "a tile just taken has not changed");
        let err = lua.load(format!("water.surface({id}, {{ fixed = true }})")).exec().unwrap_err();
        assert!(err.to_string().contains("fixed"), "{err}");
        {
            let mut d = shared.declared.borrow_mut();
            let Surface::Tiles(t) = &mut d[0].surface else { unreachable!() };
            let t = std::sync::Arc::make_mut(t);
            std::sync::Arc::make_mut(t.tiles.get_mut(&key).unwrap()).serial += 1;
        }
        let moved: bool = lua.load(format!("return water.surfaceChanged({id})")).eval().unwrap();
        assert!(moved, "a tile's water moved and the surface says it did not");
    }

    /// **A tiled sim saves and restarts, and takes water and springs while
    /// it runs.** water.state hands back its wet points, addVolume{ state }
    /// starts from them, and water.addWater and water.setSpring reach the
    /// engine for the point under them.
    #[test]
    fn a_tiled_sims_state_round_trips_and_water_can_be_added() {
        let (lua, shared) = host();
        let id: u32 = lua
            .load(r#"return water.addVolume{ shape = "sim", center = vec3(0, 0, 0), flat = true, cell = 1, size = 63, level = false, soak = 0.002 }"#)
            .eval()
            .unwrap();
        let patch = shared.declared.borrow()[0].simulated.as_ref().unwrap().tiled.as_ref().unwrap().patch;
        let mut depth = vec![0.0f32; floptle_core::water::TILE * floptle_core::water::TILE];
        depth[33] = 0.7;
        let tile = floptle_core::water::TileLevels { level: vec![f32::NAN; depth.len()], flow: vec![[0.0; 2]; depth.len()], depth, serial: 1 };
        let tiles = floptle_core::water::TiledSurface { patch, tiles: [(floptle_core::water::tile_key(1, 0), std::sync::Arc::new(tile))].into_iter().collect() };
        shared.declared.borrow_mut()[0].surface = Surface::Tiles(std::sync::Arc::new(tiles));
        let again: u32 = lua
            .load(format!(
                r#"local s = water.state({id})
                   assert(s.columns == 64 and #s.cells == 3 and s.cells[1] == 33 and s.cells[2] == 1, "the state is " .. #s.cells)
                   assert(water.addWater({id}, vec3(0.2, 0, 0.1), 2.5) and water.setSpring({id}, {{ 5, 6 }}, 0))
                   return water.addVolume{{ shape = "sim", center = vec3(0, 0, 0), flat = true, cell = 1, size = 63, level = false, state = s }}"#
            ))
            .eval()
            .unwrap();
        let d = shared.declared.borrow();
        let t = d.iter().find(|w| w.id == again).unwrap().simulated.as_ref().unwrap().tiled.as_ref().unwrap();
        assert_eq!(t.state.as_deref(), Some(&vec![(33, 1, 0.7)]));
        assert!(!t.fill);
        assert_eq!(d[0].simulated.as_ref().unwrap().tiled.as_ref().unwrap().soak, 0.002);
        let cmds = shared.sim_cmds.borrow().clone();
        assert_eq!(
            cmds,
            vec![
                floptle_core::water::SimCmd::AddWater { id, cell: [32, 32], volume: 2.5 },
                floptle_core::water::SimCmd::Spring { id, cell: [5, 6], rate: 0.0 },
            ]
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

    /// A flat sea 400 m across at y = 0, declared with one 1.5 m wave
    /// running along +x, and the clock at `t`.
    fn swell_host(steepness: f64, t: f64) -> (Lua, WaterShared) {
        let (lua, shared) = host();
        shared.clock.set(t);
        lua.load(format!(
            r#"sea = water.addVolume{{ shape = "heightfield", origin = vec3(-200, 0, -200), cell = 400, columns = 2, rows = 2,
                 levels = {{ 0, 0, 0, 0 }},
                 waves = {{ {{ amplitude = 1.5, wavelength = 40, direction = {{ 1, 0 }}, steepness = {steepness} }} }} }}"#
        ))
        .exec()
        .unwrap();
        (lua, shared)
    }

    /// **`water.at` is the formula.** Under a rolling wave `A·sin(k·x − ω·t)`
    /// high, a point 5 m under the still level is `5 + A·sin(…)` deep, the
    /// surface stands that `wave` above its level, and `depthAt` and
    /// `isUnderwater` agree: a point between the still level and a crest is
    /// under water, one in a trough's air is not. The clock is the one the
    /// host sets with `simTime`.
    #[test]
    fn water_at_answers_at_the_moving_surface() {
        let t = 3.25;
        let (lua, shared) = swell_host(0.0, t);
        let k = std::f64::consts::TAU / 40.0;
        let omega = (9.81 * k).sqrt();
        for x in [-37.0, 0.0, 4.5, 10.0, 26.0] {
            let h = 1.5 * (k * x - omega * t).sin();
            let (depth, wave, plain, ny): (f64, f64, f64, f64) = lua
                .load(format!("local w = water.at(vec3({x}, -5, 2)); return w.depth, w.wave, water.depthAt({x}, -5, 2), w.normal.y"))
                .eval()
                .unwrap();
            assert!((depth - (5.0 + h)).abs() < 1e-9, "at x = {x}: {depth} deep, the formula says {}", 5.0 + h);
            assert!((wave - h).abs() < 1e-9 && (plain - depth).abs() < 1e-12);
            let slope = 1.5 * k * (k * x - omega * t).cos();
            assert!((ny - 1.0 / (1.0 + slope * slope).sqrt()).abs() < 1e-9, "the normal leans with the wave's face: {ny}");
            // Half way up (or down) to where the wave has the surface.
            let (wet, dry): (bool, bool) = lua
                .load(format!("return water.isUnderwater({x}, {}, 2), water.isUnderwater({x}, {}, 2)", h - 0.1, h + 0.1))
                .eval()
                .unwrap();
            assert!(wet && !dry, "at x = {x} the surface is at {h}");
        }
        // The clock moves it: the host's simTime is the swell's time.
        shared.clock.set(t + 1.0);
        for w in shared.declared.borrow_mut().iter_mut() {
            w.time = t + 1.0;
        }
        let later: f64 = lua.load("return water.depthAt(10, -5, 2)").eval().unwrap();
        assert!((later - (5.0 + 1.5 * (k * 10.0 - omega * (t + 1.0)).sin())).abs() < 1e-9, "{later}");
    }

    /// **A steep wave is the trochoid a shader draws.** `water.swellAt`
    /// moves a point of the still surface as `waterSwell` moves its vertex:
    /// `s·A·cos θ` along the wave and `A·sin θ` up. Asked the other way,
    /// `water.at` finds that moved point on the surface, and a point 2 m
    /// under it 2 m deep.
    #[test]
    fn swell_at_and_water_at_agree_on_a_steep_wave() {
        let t = 1.75;
        let (lua, _) = swell_host(0.8, t);
        let k = std::f64::consts::TAU / 40.0;
        let omega = (9.81 * k).sqrt();
        for x0 in [-30.0, 3.0, 10.0, 17.5] {
            let theta = k * x0 - omega * t;
            let (ox, oy, depth, wave): (f64, f64, f64, f64) = lua
                .load(format!(
                    "local off, n = water.swellAt(sea, vec3({x0}, 0, 7))
                     local w = water.at(vec3({x0}, 0, 7) + off - vec3(0, 2, 0))
                     return off.x, off.y, w.depth, w.wave"
                ))
                .eval()
                .unwrap();
            assert!((ox - 0.8 * 1.5 * theta.cos()).abs() < 1e-6 && (oy - 1.5 * theta.sin()).abs() < 1e-6, "the offset at {x0}: {ox}, {oy}");
            assert!((depth - 2.0).abs() < 1e-4, "2 m under the moved point is {depth} m deep");
            assert!((wave - oy).abs() < 1e-4);
        }
    }

    /// **`setSwell` gives, changes and takes away a swell, and `swell` reads
    /// it back.** Water declared without waves is the still surface it
    /// always was; a list that is not waves is refused by name.
    #[test]
    fn set_swell_changes_the_waves_and_no_waves_is_still_water() {
        let (lua, shared) = host();
        lua.load(
            r#"id = water.addVolume{ shape = "heightfield", origin = vec3(-50, 0, -50), cell = 100, columns = 2, rows = 2,
                 levels = { 0, 0, 0, 0 } }"#,
        )
        .exec()
        .unwrap();
        assert!(shared.declared.borrow()[0].swell.is_none(), "water declared without waves has a swell");
        let (still, none): (f64, bool) = lua.load("return water.depthAt(5, -3, 5), water.swell(id) == nil").eval().unwrap();
        assert_eq!(still, 3.0);
        assert!(none);
        let (ok, n, speed, depth): (bool, usize, f64, f64) = lua
            .load(
                "local ok = water.setSwell(id, { { amplitude = 1, wavelength = 20, steepness = 0, phase = math.pi / 2 },
                                                 { amplitude = 0.25, wavelength = 5, direction = vec3(0, 9, 2), steepness = 0, speed = 1.5 } })
                 local s = water.swell(id)
                 return ok, #s, s[2].speed, water.depthAt(0, -3, 0)",
            )
            .eval()
            .unwrap();
        assert!(ok && n == 2 && (speed - 1.5).abs() < 1e-6);
        // At the origin at time 0 the first wave is at its crest (phase π/2).
        assert!((depth - 4.0).abs() < 1e-6, "{depth}");
        assert_eq!(*shared.changed.borrow(), [1], "the solver is told the surface changed");
        let back: f64 = lua.load("water.setSwell(id, nil); return water.depthAt(0, -3, 0)").eval().unwrap();
        assert_eq!(back, 3.0, "a swell taken away leaves the still surface");
        assert!(shared.declared.borrow()[0].swell.is_none());
        let missing: bool = lua.load("return water.setSwell(99, {})").eval().unwrap();
        assert!(!missing);
        for (bad, says) in [
            ("{ { amplitude = 1, wavelenght = 20 } }", "wavelenght"),
            ("{ { 1, 20 } }", "waves[1]"),
            ("{ { amplitude = 1, wavelength = 0 } }", "more than 0"),
            ("{ { direction = { 0, 0 } } }", "no length"),
            ("5", "list of waves"),
        ] {
            let err = lua.load(format!("water.setSwell(id, {bad})")).exec().unwrap_err().to_string();
            assert!(err.contains(says), "`{bad}` said: {err}");
        }
        let many = (0..17).map(|_| "{}").collect::<Vec<_>>().join(", ");
        let err = lua.load(format!("water.setSwell(id, {{ {many} }})")).exec().unwrap_err().to_string();
        assert!(err.contains("at most 16"), "{err}");
    }

    /// **A Water Volume node's swell answers too, in its node's frame.** A
    /// sea of radius 100 with a wave along its +x: `water.at` over its +Y is
    /// the formula, and with the node turned the waves turn with it.
    #[test]
    fn a_sea_nodes_swell_is_read_in_the_nodes_frame() {
        let (lua, shared) = host();
        shared.volumes.borrow_mut().push(sea());
        let wave = Wave { amplitude: 0.5, wavelength: 25.0, direction: [1.0, 0.0], steepness: 0.0, speed: None, phase: 0.0 };
        let swell = std::sync::Arc::new(Swell::new(&[wave], glam::DVec3::Y).unwrap());
        shared.clock.set(2.0);
        let k = std::f64::consts::TAU / 25.0;
        let omega = (9.81 * k).sqrt();
        // 6 m along +x from the top of the sea, 1 m under its still level.
        let a: f64 = 6.0 / 100.0;
        let p = glam::DVec3::new(a.sin(), a.cos(), 0.0) * 99.0;
        let depth_with = |rot: glam::DQuat, p: glam::DVec3| -> f64 {
            shared.swells.borrow_mut().insert(1, NodeSwell { swell: swell.clone(), rot });
            lua.load(format!("return water.depthAt({}, {}, {})", p.x, p.y, p.z)).eval().unwrap()
        };
        let want = 1.0 + 0.5 * (k * 6.0 - omega * 2.0).sin();
        let plain = depth_with(glam::DQuat::IDENTITY, p);
        assert!((plain - want).abs() < 1e-6, "{plain} deep, the formula says {want}");
        let turn = glam::DQuat::from_rotation_y(1.1);
        assert!((depth_with(turn, turn * p) - want).abs() < 1e-6, "the swell did not turn with its node");
        // No swell on the node: the still sphere, exactly.
        shared.swells.borrow_mut().clear();
        let still: f64 = lua.load(format!("return water.depthAt({}, {}, {})", p.x, p.y, p.z)).eval().unwrap();
        assert_eq!(still, sea().depth_at(p.to_array()));
        let (ny, wave_h): (f64, f64) = lua.load("local w = water.at(0, 99, 0); return w.normal.y, w.wave").eval().unwrap();
        assert_eq!((ny, wave_h), (1.0, 0.0));
    }
}
