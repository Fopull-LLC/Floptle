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
use std::collections::HashMap;
use std::rc::Rc;

use floptle_nav::{AgentId, AgentParams, AgentState, Crowd, QueryFilter};
use mlua::{Lua, UserData, UserDataFields, UserDataMethods, Value};

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

/// Keys `nav.agent(node, opts)` and `agent:set{...}` read — the registry
/// entries that make a typo an error instead of a silent default, which is
/// this engine's most-filed bug shape (`floptle/0082`).
pub(crate) const AGENT_KEYS: &[&str] = &[
    "radius",
    "speed",
    "accel",
    "arrive",
    "slow",
    "avoid",
    "priority",
    "separation",
    "repath",
    "giveUpAfter",
    "drive",
    "filter",
];

/// Keys inside the `filter = {...}` sub-table.
pub(crate) const FILTER_KEYS: &[&str] = &["avoid", "cost"];

/// How an agent's movement reaches the node it belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Drive {
    /// Feed a physics body if the node has one, and move the transform if it
    /// does not. **What almost everything wants**, and the only reason it is not
    /// simply the behaviour is that saying so in the Inspector is better than
    /// having people discover it.
    #[default]
    Auto,
    /// Move the node's transform. Nothing collides; the navmesh is the collision.
    Transform,
    /// Write the node's velocity and let the physics sim carry it. Slopes,
    /// gravity and pushing each other come free; walls are enforced twice.
    Velocity,
    /// Steer, and leave the node alone. The script reads `agent.velocity` and
    /// does whatever it likes with it — a vehicle with a turning circle, an
    /// animation-driven character, a boat.
    None,
}

/// One agent's tie to the scene.
///
/// The crowd works in the navmesh's own frame and the scene works in world
/// coordinates, and this is where the two meet. **Targets are kept in world
/// space**, not converted once and stored: a rebake can move the mesh's anchor,
/// and an order half-translated into a frame that no longer exists is a unit
/// walking confidently to the wrong place.
pub struct Bound {
    pub entity: u32,
    pub drive: Drive,
    /// Where it was told to go, in world space, or `None` for "stand still".
    pub target: Option<[f64; 3]>,
    /// The last world position the host wrote, so a script can ask an agent
    /// where it is without the answer depending on a mesh being loaded.
    pub pos: [f64; 3],
    /// A pending `agent:teleport(...)`, in world space. The host writes it to
    /// the NODE and skips this frame's scene read-back — without that, the
    /// read-back immediately puts the agent right back where it was and a
    /// teleport is just a `stop()` wearing a hat.
    pub teleport: Option<[f64; 3]>,
    /// The filter as it was written, by name. Re-resolved against the mesh's
    /// area list whenever a bake arrives, because a name is the identity and an
    /// index is not.
    pub avoid: Vec<String>,
    pub costs: Vec<(String, f32)>,
}

/// Every agent in the scene, and what each one is attached to.
#[derive(Default)]
pub struct AgentWorld {
    pub crowd: Crowd,
    pub bound: HashMap<AgentId, Bound>,
}

impl AgentWorld {
    /// Work every agent's named filter out against this mesh's areas.
    ///
    /// An area a filter names that the bake does not have is **ignored rather
    /// than guessed at**: excluding an area that is not there would be a filter
    /// that does nothing, and picking the nearest name would be a filter that
    /// does something nobody asked for. The editor reports the mismatch after a
    /// bake, where there is room to say which name.
    pub fn resolve_filters(&mut self, mesh: Option<&floptle_nav::NavMesh>) {
        for (id, b) in &self.bound {
            let mut f = QueryFilter::default();
            if let Some(mesh) = mesh {
                let index = |name: &str| {
                    mesh.areas.iter().position(|a| a.name.eq_ignore_ascii_case(name)).map(|i| i as u8)
                };
                for name in &b.avoid {
                    if let Some(i) = index(name) {
                        f.exclude(i);
                    }
                }
                for (name, c) in &b.costs {
                    if let Some(i) = index(name) {
                        f.set_cost(i, *c);
                    }
                }
            }
            if let Some(a) = self.crowd.agent_mut(*id) {
                a.params.filter = f;
            }
        }
    }
}

pub type AgentsShared = Rc<RefCell<AgentWorld>>;

/// A script's handle on one agent.
///
/// Holds an id rather than the agent itself, so a handle kept in a Lua variable
/// after the unit died answers "no" to everything instead of pointing at
/// whoever was created next.
pub struct LuaAgent {
    id: AgentId,
    world: AgentsShared,
    mesh: NavShared,
}

impl LuaAgent {
    fn with<T>(&self, f: impl FnOnce(&floptle_nav::Agent, &Bound) -> T) -> Option<T> {
        let w = self.world.borrow();
        let a = w.crowd.agent(self.id)?;
        let b = w.bound.get(&self.id)?;
        Some(f(a, b))
    }
}

fn state_name(s: AgentState) -> &'static str {
    match s {
        AgentState::Idle => "idle",
        AgentState::Moving => "moving",
        AgentState::Arrived => "arrived",
        AgentState::Blocked => "blocked",
        AgentState::Crossing => "crossing",
    }
}

impl UserData for LuaAgent {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        // "idle" | "moving" | "arrived" | "blocked" | "crossing", and "gone" for
        // a handle whose agent has been destroyed — a state rather than an error,
        // because a script holding one is usually mid-cleanup.
        fields.add_field_method_get("state", |_, a| {
            Ok(a.with(|ag, _| state_name(ag.state())).unwrap_or("gone").to_string())
        });
        fields.add_field_method_get("moving", |_, a| {
            Ok(a.with(|ag, _| ag.state() == AgentState::Moving || ag.state() == AgentState::Crossing)
                .unwrap_or(false))
        });
        fields.add_field_method_get("arrived", |_, a| {
            Ok(a.with(|ag, _| ag.arrived()).unwrap_or(false))
        });
        fields.add_field_method_get("blocked", |_, a| {
            Ok(a.with(|ag, _| ag.state() == AgentState::Blocked).unwrap_or(false))
        });
        // Whether the route in hand actually reaches the order. False while
        // walking to the nearest reachable point instead.
        fields.add_field_method_get("complete", |_, a| {
            Ok(a.with(|ag, _| ag.route_complete()).unwrap_or(false))
        });
        // How far there is left to walk, along the route — not the straight line.
        fields.add_field_method_get("remaining", |_, a| {
            Ok(a.with(|ag, _| ag.distance_left() as f64).unwrap_or(0.0))
        });
        fields.add_field_method_get("velocity", |_, a| {
            let v = a.with(|ag, _| ag.vel).unwrap_or([0.0; 3]);
            Ok(LuaVec3(glam::DVec3::new(v[0] as f64, v[1] as f64, v[2] as f64)))
        });
        fields.add_field_method_get("speed", |_, a| {
            let v = a.with(|ag, _| ag.vel).unwrap_or([0.0; 3]);
            Ok((v[0] * v[0] + v[2] * v[2]).sqrt() as f64)
        });
        fields.add_field_method_get("pos", |_, a| {
            let p = a.with(|_, b| b.pos).unwrap_or([0.0; 3]);
            Ok(LuaVec3(glam::DVec3::new(p[0], p[1], p[2])))
        });
        fields.add_field_method_get("target", |_, a| {
            Ok(a.with(|_, b| b.target)
                .flatten()
                .map(|t| LuaVec3(glam::DVec3::new(t[0], t[1], t[2]))))
        });
        // The link being crossed right now, by name — nil the rest of the time.
        // This is the hook for "play the climb animation".
        fields.add_field_method_get("link", |_, a| {
            let Some(ride) = a.with(|ag, _| ag.crossing()).flatten() else { return Ok(None) };
            let guard = a.mesh.borrow();
            Ok(guard
                .as_ref()
                .and_then(|m| m.off_links.iter().find(|l| l.id == ride.link))
                .map(|l| l.name.clone()))
        });
        // How far across it is, 0 to 1 — what an animation is driven by.
        fields.add_field_method_get("linkProgress", |_, a| {
            Ok(a.with(|ag, _| ag.crossing().map(|r| r.progress as f64)).flatten())
        });
        fields.add_field_method_get("alive", |_, a| Ok(a.with(|_, _| true).unwrap_or(false)));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // agent:moveTo(point | node) — the whole API, most days.
        methods.add_method("moveTo", |_, a, target: Value| {
            let Some(p) = vec3_of(&target) else {
                return Err(mlua::Error::RuntimeError(
                    "agent:moveTo takes a vec3 (or anything with x/y/z)".into(),
                ));
            };
            let mut w = a.world.borrow_mut();
            if let Some(b) = w.bound.get_mut(&a.id) {
                b.target = Some([p.x, p.y, p.z]);
            }
            Ok(())
        });
        methods.add_method("stop", |_, a, ()| {
            let mut w = a.world.borrow_mut();
            if let Some(b) = w.bound.get_mut(&a.id) {
                b.target = None;
            }
            if let Some(ag) = w.crowd.agent_mut(a.id) {
                ag.stop();
            }
            Ok(())
        });
        // Put it somewhere without walking there — a spawn, a teleport, a
        // cutscene. Whatever it was doing is forgotten. (The host moves the
        // node too; with `drive = "none"` move the node yourself instead.)
        methods.add_method("teleport", |_, a, to: Value| {
            let Some(p) = vec3_of(&to) else {
                return Err(mlua::Error::RuntimeError("agent:teleport takes a vec3".into()));
            };
            let mut w = a.world.borrow_mut();
            if let Some(b) = w.bound.get_mut(&a.id) {
                b.pos = [p.x, p.y, p.z];
                b.target = None;
                b.teleport = Some([p.x, p.y, p.z]);
            }
            let guard = a.mesh.borrow();
            let local = match guard.as_ref() {
                Some(m) => m.to_local([p.x, p.y, p.z]),
                None => [p.x as f32, p.y as f32, p.z as f32],
            };
            if let Some(ag) = w.crowd.agent_mut(a.id) {
                ag.teleport(local);
                ag.stop();
            }
            Ok(())
        });
        // Change how it walks, mid-game. Anything left out is left alone.
        methods.add_method("set", |_, a, opts: mlua::Table| {
            crate::opts::check_keys(&opts, AGENT_KEYS, "agent:set")?;
            let mut w = a.world.borrow_mut();
            let mut params = match w.crowd.agent(a.id) {
                Some(ag) => ag.params,
                None => return Ok(()),
            };
            read_params(&opts, &mut params);
            if let Some(ag) = w.crowd.agent_mut(a.id) {
                ag.params = params;
            }
            let mut filter_changed = false;
            if let Some(b) = w.bound.get_mut(&a.id) {
                filter_changed = read_filter(&opts, b, "agent:set")?;
                if let Some(d) = opts.get::<Option<String>>("drive").ok().flatten() {
                    b.drive = drive_of(&d);
                }
            }
            // Names only mean something against the mesh's area list — resolve
            // NOW, or the new filter waits for the next bake that never comes
            // in a shipped game.
            if filter_changed {
                let guard = a.mesh.borrow();
                w.resolve_filters(guard.as_ref());
            }
            Ok(())
        });
        // The corners still to walk, in world space — for drawing a route while
        // working out why a unit went the way it did.
        methods.add_method("corners", |lua, a, ()| {
            let w = a.world.borrow();
            let out = lua.create_table()?;
            // Through the mesh's anchor, like every other point this module
            // hands out — the corridor itself lives in bake-local space.
            let guard = a.mesh.borrow();
            if let Some(ag) = w.crowd.agent(a.id) {
                for (i, p) in ag.remaining().iter().enumerate() {
                    let v = match guard.as_ref() {
                        Some(m) => world_vec(m, *p),
                        None => LuaVec3(glam::DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64)),
                    };
                    out.set(i + 1, v)?;
                }
            }
            Ok(out)
        });
        // Take it out of the crowd. Not required — an agent whose node is gone
        // is dropped on the next frame — but the right thing to call from a
        // script's own teardown.
        methods.add_method("destroy", |_, a, ()| {
            let mut w = a.world.borrow_mut();
            w.crowd.remove(a.id);
            w.bound.remove(&a.id);
            Ok(())
        });
    }
}

fn drive_of(name: &str) -> Drive {
    match name.to_ascii_lowercase().as_str() {
        "transform" | "move" => Drive::Transform,
        "velocity" | "physics" | "body" => Drive::Velocity,
        "none" | "steer" | "off" => Drive::None,
        _ => Drive::Auto,
    }
}

fn read_params(opts: &mlua::Table, p: &mut AgentParams) {
    let num = |key: &str| opts.get::<Option<f64>>(key).ok().flatten().map(|v| v as f32);
    if let Some(v) = num("radius") {
        p.radius = v.max(0.0);
    }
    if let Some(v) = num("speed") {
        p.speed = v.max(0.0);
    }
    if let Some(v) = num("accel") {
        p.accel = v.max(0.0);
    }
    if let Some(v) = num("arrive") {
        p.arrive = v.max(0.0);
    }
    if let Some(v) = num("slow") {
        p.slow = v.max(0.0);
    }
    if let Some(v) = num("priority") {
        p.priority = v;
    }
    if let Some(v) = num("separation") {
        p.separation = v.clamp(0.0, 4.0);
    }
    if let Some(v) = num("repath") {
        p.repath = v.max(0.0);
    }
    if let Some(v) = num("giveUpAfter") {
        p.stuck_after = v.max(0.0);
    }
    if let Ok(Some(v)) = opts.get::<Option<bool>>("avoid") {
        p.avoid = v;
    }
}

/// `filter = { avoid = {"water"}, cost = { mud = 4 } }`, kept by name.
/// Returns whether a filter table was present (so the caller re-resolves).
fn read_filter(opts: &mlua::Table, b: &mut Bound, call: &str) -> mlua::Result<bool> {
    let Ok(Some(f)) = opts.get::<Option<mlua::Table>>("filter") else { return Ok(false) };
    crate::opts::check_keys(&f, FILTER_KEYS, call)?;
    if let Ok(Some(list)) = f.get::<Option<mlua::Table>>("avoid") {
        b.avoid = list.sequence_values::<String>().flatten().collect();
    }
    if let Ok(Some(costs)) = f.get::<Option<mlua::Table>>("cost") {
        b.costs = costs
            .pairs::<String, f64>()
            .flatten()
            .map(|(k, v)| (k, v as f32))
            .collect();
        // Deterministic, because a table's pairs are not.
        b.costs.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok(true)
}

pub(crate) fn install_nav_api(
    lua: &Lua,
    mesh: NavShared,
    agents: AgentsShared,
    scene: Rc<RefCell<crate::SceneMirror>>,
) {
    let Ok(t) = lua.create_table() else { return };

    // nav.agent(node[, opts]) -> agent
    //
    // The one call this whole module exists for. Everything else answers
    // questions about the navmesh; this walks something along it.
    let m = mesh.clone();
    let w = agents.clone();
    let scene = scene.clone();
    if let Ok(f) = lua.create_function(move |lua, (node, opts): (Value, Option<mlua::Table>)| {
        let Value::Table(handle) = &node else {
            return Err(mlua::Error::RuntimeError(
                "nav.agent(node[, opts]) takes a node — pass the `node` your script was given"
                    .into(),
            ));
        };
        let Ok(entity) = handle.raw_get::<u32>("__id") else {
            return Err(mlua::Error::RuntimeError(
                "nav.agent(node[, opts]): that is not a node handle".into(),
            ));
        };

        let mut params = AgentParams::default();
        // A mesh baked for a wider character than the agent thinks it is would
        // let it stand somewhere it does not fit, so the bake's radius is the
        // starting point rather than a guess.
        if let Some(mesh) = m.borrow().as_ref() {
            params.radius = mesh.settings.agent_radius.max(0.05);
        }
        let mut bound = Bound {
            entity,
            drive: Drive::Auto,
            target: None,
            pos: [0.0; 3],
            teleport: None,
            avoid: Vec::new(),
            costs: Vec::new(),
        };
        if let Some(opts) = &opts {
            crate::opts::check_keys(opts, AGENT_KEYS, "nav.agent")?;
            read_params(opts, &mut params);
            read_filter(opts, &mut bound, "nav.agent")?;
            if let Some(d) = opts.get::<Option<String>>("drive").ok().flatten() {
                bound.drive = drive_of(&d);
            }
        }

        // Start where the node is, so an agent asked about before its first step
        // answers about the right place.
        let world_pos = {
            let s = scene.borrow();
            crate::api::world_transform_of_handle(&s, handle, entity).translation
        };
        bound.pos = [world_pos.x, world_pos.y, world_pos.z];
        let local = match m.borrow().as_ref() {
            Some(mesh) => mesh.to_local(bound.pos),
            None => [world_pos.x as f32, world_pos.y as f32, world_pos.z as f32],
        };

        let mut world = w.borrow_mut();
        let id = world.crowd.add(params, local);
        world.bound.insert(id, bound);
        drop(world);
        {
            let guard = m.borrow();
            w.borrow_mut().resolve_filters(guard.as_ref());
        }
        lua.create_userdata(LuaAgent { id, world: w.clone(), mesh: m.clone() })
    }) {
        let _ = t.set("agent", f);
    }

    // nav.agents() -> how many there are. One number, for a HUD or a test.
    let w = agents.clone();
    if let Ok(f) = lua.create_function(move |_, ()| Ok(w.borrow().crowd.len())) {
        let _ = t.set("agents", f);
    }

    // nav.budget([n]) -> how many path searches the crowd may run per frame.
    //
    // Raising it makes a hundred units react to one order in the same frame at
    // the cost of that frame; lowering it spreads the thinking further. Read it
    // with no argument.
    let w = agents.clone();
    if let Ok(f) = lua.create_function(move |_, n: Option<usize>| {
        let mut world = w.borrow_mut();
        if let Some(n) = n {
            world.crowd.paths_per_step = n.max(1);
        }
        Ok(world.crowd.paths_per_step)
    }) {
        let _ = t.set("budget", f);
    }

    // nav.link(name | id[, open]) -> is it open, or nil if there is no such link
    //
    // The door. Closing one makes every route that used it repath, with nothing
    // rebaked and nothing else to remember.
    let m = mesh.clone();
    let w = agents.clone();
    if let Ok(f) = lua.create_function(move |_, (which, open): (Value, Option<bool>)| {
        let mut guard = m.borrow_mut();
        let Some(mesh) = guard.as_mut() else { return Ok(None) };
        let found = match &which {
            Value::Integer(i) => mesh.off_links.iter_mut().find(|l| l.id == *i as u32),
            Value::Number(n) => mesh.off_links.iter_mut().find(|l| l.id == *n as u32),
            Value::String(s) => {
                let name = s.to_str()?.to_string();
                mesh.off_links.iter_mut().find(|l| l.name == name)
            }
            _ => None,
        };
        let Some(link) = found else { return Ok(None) };
        if let Some(open) = open {
            let changed = link.enabled != open;
            link.enabled = open;
            if changed {
                w.borrow_mut().crowd.navmesh_changed();
            }
        }
        Ok(Some(link.enabled))
    }) {
        let _ = t.set("link", f);
    }

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
        install_nav_api(
            &lua,
            shared.clone(),
            Rc::new(RefCell::new(AgentWorld::default())),
            Rc::new(RefCell::new(crate::SceneMirror::default())),
        );
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
        install_nav_api(
            &lua,
            Rc::new(RefCell::new(None)),
            Rc::new(RefCell::new(AgentWorld::default())),
            Rc::new(RefCell::new(crate::SceneMirror::default())),
        );
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
