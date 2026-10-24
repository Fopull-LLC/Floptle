//! The Lua `terrain.*` API — runtime terrain editing and queries (Terrain 2.0 P6).
//!
//! Writes (`sculpt`/`dig`/`paint`/`paintTexture`) queue ops; the editor drains them
//! after the script pass and applies each to the authority `ChunkField`, the sim's
//! collider copy (so collision changes the same tick), the chunk remesh queue, and
//! the shadow-proxy region — the exact pipeline an editor brush dab takes. Reads
//! (`query`/`height`) run immediately against the lent sim colliders, the same loan
//! `raycast(...)` uses, so they see the world as of this frame.
//!
//! Multiplayer note (documented in scripting.md): ops apply on the machine that runs
//! them. Until replicated terrain lands, run terrain edits server-side and mirror
//! them to clients with an RPC that repeats the same call — the ops are deterministic
//! by construction (same call, same field result).

use std::cell::RefCell;
use std::rc::Rc;

use floptle_core::math::Vec3;
use mlua::{IntoLuaMulti, Lua, Table, Value};

/// One queued terrain write, in world coordinates (scripts speak world; the editor
/// converts into each terrain's local frame when applying).
#[derive(Clone, Debug)]
pub struct TerrainOp {
    pub pos: [f64; 3],
    pub radius: f32,
    pub strength: f32,
    pub mode: TerrainOpMode,
    /// Ties this op to the yield report it produces when it lands. Ops are
    /// Queued and applied after the script pass, so a dig cannot return what it
    /// removed — the edit has not happened yet. It returns this instead, and the
    /// measured report arrives through `terrain.yields()`.
    pub id: u64,
    /// `flatten`'s plane normal in world space, when the script chose one.
    /// `None` lets the engine decide: the radial up on a planet, `+Y` otherwise.
    pub normal: Option<[f64; 3]>,
}

impl TerrainOp {
    /// True if this op moves the surface rather than only its colour.
    ///
    /// Anything standing on the ground — a scattered tree, a placed building —
    /// only has to be re-settled when the ground itself moved. Repainting a
    /// hillside must not cost a re-resolve of every prop on it.
    pub fn affects_geometry(&self) -> bool {
        !matches!(
            self.mode,
            TerrainOpMode::Paint(_) | TerrainOpMode::PaintTexture(_)
        )
    }
}

/// The op-id counter and the inbox measured reports land in — the two halves of
/// "a queued edit tells you what it did" — and the planet questions answered
/// off the script thread.
pub(crate) struct TerrainReceipts {
    pub yields: Rc<RefCell<Vec<TerrainYield>>>,
    pub next_op_id: Rc<std::cell::Cell<u64>>,
    pub planet: Rc<RefCell<PlanetQueries>>,
    /// Edits the engine has taken and not applied yet: it applies a few
    /// milliseconds of them a frame, in order.
    pub queued: Rc<std::cell::Cell<usize>>,
    /// The generator version a planet whose options name none is made by
    /// (the project's; see `ScriptHost::set_terrain_generator`).
    pub generator: Rc<std::cell::Cell<u32>>,
    /// `terrain.impostorAt` calls since the engine last took them: a body's
    /// terrain id and its rule, `None` to go back to the default.
    pub impostors: ImpostorCalls,
    /// Edits refused because the queue was full, since the host was made:
    /// what `terrain.capacity()` exists to keep at zero, and what
    /// `perf.counts().terrainDropped` reports.
    pub dropped: Rc<std::cell::Cell<u64>>,
}

/// Ground the world has not made yet: a terrain body that is generating,
/// streaming in, or put away cold, as a world-space sphere that holds all of
/// its ground. Published by the engine each frame; `terrain.ready`,
/// `terrain.raycast` and `terrain.query`'s second answer read it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnmadeRegion {
    pub center: [f64; 3],
    pub radius: f64,
}

impl UnmadeRegion {
    /// Does the sphere `p`, `r` reach into this region?
    fn touches(&self, p: [f64; 3], r: f64) -> bool {
        let d = (0..3).map(|k| (p[k] - self.center[k]).powi(2)).sum::<f64>().sqrt();
        d <= self.radius + r.max(0.0)
    }

    /// Where a ray from `o` along the unit `d` first enters this region, if it
    /// does within `max`: 0 from inside.
    fn entry(&self, o: [f64; 3], d: [f64; 3], max: f64) -> Option<f64> {
        let oc = [o[0] - self.center[0], o[1] - self.center[1], o[2] - self.center[2]];
        let c = oc.iter().map(|v| v * v).sum::<f64>() - self.radius * self.radius;
        if c <= 0.0 {
            return Some(0.0);
        }
        let b = oc[0] * d[0] + oc[1] * d[1] + oc[2] * d[2];
        let disc = b * b - c;
        if b >= 0.0 || disc < 0.0 {
            return None;
        }
        let t = -b - disc.sqrt();
        (t <= max).then_some(t.max(0.0))
    }
}

/// Edits refused for a full queue since the host was made — the number
/// `perf.counts().terrainDropped` reads. Zero in a state with no terrain API.
pub(crate) fn dropped_count(lua: &Lua) -> u64 {
    lua.app_data_ref::<TerrainDrops>().map_or(0, |d| d.0.get())
}

/// The drop counter, lent to the state so `perf.counts` can read it without
/// the terrain API and the profiler having to know about each other.
struct TerrainDrops(Rc<std::cell::Cell<u64>>);

/// `terrain.impostorAt` calls waiting for the engine: a terrain id and its
/// rule, `None` for the default.
pub(crate) type ImpostorCalls = Rc<RefCell<Vec<(u32, Option<ImpostorRule>)>>>;

/// When a planet stops drawing its ground and draws as one shaded ball.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImpostorRule {
    /// Farther than this many of its radii from the camera.
    Radii(f64),
    /// Smaller on screen than this many pixels across.
    Pixels(f64),
}

/// What a planet question comes back with.
enum PlanetAnswer {
    Lakes(Vec<floptle_field::procgen::Lake>, f32),
    Channels(Vec<floptle_field::relief::Channel>, f32),
}

/// `terrain.planetLakes` and `terrain.planetRivers` asked with a callback: the
/// work runs on a thread of its own, and each callback waits here for its
/// answer. A large world's lakes take seconds to find, past the script
/// watchdog; asked this way the script thread pays for reading the opts.
#[derive(Default)]
pub(crate) struct PlanetQueries {
    waiting: Vec<(std::sync::mpsc::Receiver<PlanetAnswer>, mlua::RegistryKey)>,
}

impl PlanetQueries {
    /// Forget every callback still waiting: its answer lands on nothing.
    pub(crate) fn cancel_all(&mut self) {
        self.waiting.clear();
    }

    fn ask(&mut self, lua: &Lua, cb: mlua::Function, work: impl FnOnce() -> PlanetAnswer + Send + 'static) -> mlua::Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        #[cfg(not(target_arch = "wasm32"))]
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        // A page has no threads: it answers now, and the callback still runs
        // in a later frame pass, as it does everywhere else.
        #[cfg(target_arch = "wasm32")]
        let _ = tx.send(work());
        self.waiting.push((rx, lua.create_registry_value(cb)?));
        Ok(())
    }
}

/// Hand every finished planet question to its callback. Runs in the frame
/// pass, like every other reply a script waits for.
pub(crate) fn drain_planet_queries(lua: &Lua, state: &Rc<RefCell<PlanetQueries>>, logs: &Rc<RefCell<Vec<crate::ScriptLog>>>) {
    // Collect with the borrow held, call with it released: a callback that
    // asks again re-borrows the state.
    let ready: Vec<(PlanetAnswer, mlua::RegistryKey)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let mut ready = Vec::new();
        let mut still = Vec::new();
        for (rx, key) in std::mem::take(&mut s.waiting) {
            match rx.try_recv() {
                Ok(answer) => ready.push((answer, key)),
                Err(std::sync::mpsc::TryRecvError::Empty) => still.push((rx, key)),
                // The work panicked: there is no answer to wait for.
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {}
            }
        }
        s.waiting = still;
        ready
    };
    for (answer, key) in ready {
        let Ok(cb) = lua.registry_value::<mlua::Function>(&key) else { continue };
        let _ = lua.remove_registry_value(key);
        let (what, table) = match answer {
            PlanetAnswer::Lakes(lakes, radius) => ("terrain.planetLakes", lakes_table(lua, &lakes, radius)),
            PlanetAnswer::Channels(channels, radius) => ("terrain.planetRivers", channels_table(lua, &channels, radius)),
        };
        if let Err(e) = table.and_then(|t| cb.call::<()>(t)) {
            logs.borrow_mut().push(crate::ScriptLog {
                level: crate::LogLevel::Error,
                msg: format!("{what} callback: {e}"),
                source: None,
            });
        }
    }
}

/// `terrain.planetLakes`' answer as Lua sees it.
fn lakes_table(lua: &Lua, lakes: &[floptle_field::procgen::Lake], radius: f32) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for (i, l) in lakes.iter().enumerate() {
        let row = lua.create_table()?;
        row.set("level", l.level)?;
        row.set("center", crate::math_api::LuaVec3((l.centre * (radius + l.level)).as_dvec3()))?;
        row.set("reach", l.reach)?;
        row.set("area", l.area)?;
        row.set("depth", l.depth)?;
        out.raw_set(i + 1, row)?;
    }
    Ok(out)
}

/// `terrain.planetRivers`' answer as Lua sees it.
fn channels_table(lua: &Lua, channels: &[floptle_field::relief::Channel], radius: f32) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for (i, c) in channels.iter().enumerate() {
        let ch = lua.create_table()?;
        ch.set("river", c.river)?;
        ch.set("depth", c.depth)?;
        let pts = lua.create_table_with_capacity(c.points.len() * 3, 0)?;
        for (k, p) in c.points.iter().enumerate() {
            let p = *p * radius;
            pts.raw_set(k * 3 + 1, p.x)?;
            pts.raw_set(k * 3 + 2, p.y)?;
            pts.raw_set(k * 3 + 3, p.z)?;
        }
        ch.set("points", pts)?;
        ch.set("widths", lua.create_sequence_from(c.widths.iter().copied())?)?;
        out.raw_set(i + 1, ch)?;
    }
    Ok(out)
}

/// What one applied op actually moved, in world cubic units.
#[derive(Clone, Debug, Default)]
pub struct TerrainYield {
    pub id: u64,
    pub removed: f64,
    pub added: f64,
    /// `removed` split by texture-palette slot.
    pub slots: Vec<(u8, f64)>,
    /// The part of `removed` that carried no palette slot.
    pub untextured: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TerrainOpMode {
    Raise,
    Lower,
    Smooth,
    Flatten,
    Paint([f32; 3]),
    PaintTexture(u8),
}

/// The point terrain detail follows in place of the render camera — see
/// `terrain.lodAnchor`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LodAnchor {
    /// A node, followed wherever it goes (entity index).
    Node(u32),
    /// A fixed world point.
    Point([f64; 3]),
}

/// Per-op safety caps: a runaway loop must not freeze the frame. Radius is
/// clamped; the engine lands a few milliseconds of edits a frame and the rest
/// wait, in order, for the frames after; past [`MAX_PENDING_OPS`] waiting, an
/// op is refused: its verb answers the receipt 0, the refusal is counted in
/// `perf.counts().terrainDropped`, and the first one is warned about.
/// `terrain.capacity()` is the room left, for a game pacing a big batch.
const MAX_RADIUS: f32 = 64.0;
/// What `terrain.sculpt`'s options table reads.
pub(crate) const SCULPT_KEYS: &[&str] = &["normal"];
pub const MAX_PENDING_OPS: usize = 4096;
/// The milliseconds of script terrain edits landed a frame unless a game sets
/// another with `terrain.editBudget`.
pub const TERRAIN_EDIT_BUDGET_MS: f64 = 4.0;

/// The streaming-related shared slots (`terrain.saveDir` / `warm` / `flush`) —
/// bundled so the install signature stays sane as the API grows.
pub(crate) struct TerrainStreamShared {
    pub save_dir: Rc<RefCell<Option<String>>>,
    /// `terrain.lodAnchor(...)`: what drives terrain detail instead of the
    /// render camera.
    pub lod_anchor: Rc<RefCell<Option<LodAnchor>>>,
    /// `terrain.editBudget(ms)`: the milliseconds of script terrain edits the
    /// engine lands a frame, when a game has asked for other than the default.
    pub edit_budget: Rc<std::cell::Cell<Option<f64>>>,
    pub warm: Rc<RefCell<Vec<String>>>,
    pub flush: Rc<RefCell<bool>>,
    /// Mirror of "the background terrain worker has something to do" — a field
    /// being generated, or one streaming in. Published so a game that creates
    /// worlds on demand can wait its turn.
    pub busy: Rc<std::cell::Cell<bool>>,
    /// The ground not made yet, as the engine last published it.
    pub unmade: Rc<RefCell<Vec<UnmadeRegion>>>,
    /// `terrain.prepare` asks since the engine last took them.
    pub prepare: PrepareCalls,
    /// What `terrain.deleteSaveDir` resolves its (validated, relative) path
    /// against — see [`save_dir_path`].
    pub(crate) paths: crate::paths::Paths,
}

/// Where a `terrain.saveDir` path is on disk. A save slot is the player's
/// progress, so a plain path sits beside the `save.*` slots — the project in
/// development, the per-user data folder in an exported build — and a
/// `user://` path is under the player's own folder.
pub(crate) fn save_dir_path(paths: &crate::paths::Paths, sd: &str) -> std::path::PathBuf {
    match sd.strip_prefix(crate::paths::USER_PREFIX) {
        Some(rest) => paths.user_dir().join(rest),
        None => paths.runtime_base().join(sd),
    }
}

/// A save-slot path a script may name: relative, no `..`, optionally
/// `user://`.
fn valid_save_dir(p: &str) -> bool {
    let rel = p.strip_prefix(crate::paths::USER_PREFIX).unwrap_or(p);
    !rel.is_empty()
        && !std::path::Path::new(rel).is_absolute()
        && !rel.split(['/', '\\']).any(|c| c == ".." || c.is_empty())
        && floptle_vfs::contain(std::path::Path::new(""), rel).is_some()
}

/// Does any terrain hold stand-in ground (a coarse copy, or rock not generated
/// yet) within `r` of world point `p`? Its chunks there are pending: what a
/// query reads from them is provisional.
fn stand_in_near(cols: &[floptle_physics::AnchoredCollider], p: glam::DVec3, r: f64) -> bool {
    cols.iter().any(|c| {
        let Some(t) = c.shape.chunk_terrain() else { return false };
        if t.field.pending_count() == 0 {
            return false;
        }
        let s = t.scale.max(1e-6);
        let local = (t.rot.inverse() * (p - c.anchor).as_vec3()) / s;
        // A read samples a voxel either side of the point.
        let reach = Vec3::splat(r as f32 / s + t.field.voxel() * 2.0);
        !t.field.pending_in_box(local - reach, local + reach).is_empty()
    })
}

/// `terrain.prepare` asks, waiting for the engine: world-space spheres whose
/// stand-in ground should be made exactly.
pub(crate) type PrepareCalls = Rc<RefCell<Vec<([f64; 3], f64)>>>;

pub(crate) fn install_terrain_api(
    lua: &Lua,
    ops: Rc<RefCell<Vec<TerrainOp>>>,
    generates: Rc<RefCell<Vec<(u32, floptle_field::procgen::PlanetFill)>>>,
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
    stream: TerrainStreamShared,
    receipts: TerrainReceipts,
) {
    let TerrainStreamShared { save_dir, lod_anchor, edit_budget, warm, flush, busy, unmade, prepare, paths } = stream;
    let TerrainReceipts { yields, next_op_id, planet, queued, generator, impostors, dropped } = receipts;
    lua.set_app_data(TerrainDrops(dropped.clone()));
    let Ok(t) = lua.create_table() else { return };

    // terrain.deleteSaveDir(path) — delete a save slot's persisted terrain from
    // disk ("delete this save" UIs, paired with save.deleteSlot). Deliberately
    // narrow, not a generic rm -rf: the path must be relative with no "..",
    // must not be the active saveDir (clear it first), and only terrain files
    // (.cfield/.tfield/.meta) inside that one directory are removed — the
    // directory itself (and an emptied parent) goes only once it's empty.
    // Returns the number of files removed.
    {
        let sd = save_dir.clone();
        let paths = paths.clone();
        if let Ok(f) = lua.create_function(move |_, path: String| {
            if !valid_save_dir(&path) {
                return Err(mlua::Error::RuntimeError(format!(
                    "terrain.deleteSaveDir(\"{path}\"): needs a relative project path with no \"..\""
                )));
            }
            if sd.borrow().as_deref() == Some(path.as_str()) {
                return Err(mlua::Error::RuntimeError(format!(
                    "terrain.deleteSaveDir(\"{path}\"): that is the ACTIVE saveDir — \
                     clear it (terrain.saveDir(\"\")) before deleting the slot"
                )));
            }
            let dir = save_dir_path(&paths, &path);
            let Ok(entries) = floptle_vfs::read_dir(&dir) else { return Ok(0) };
            let mut removed = 0u32;
            for entry in entries {
                let p = entry.path();
                let terrain_file = p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                    matches!(x, "cfield" | "tfield" | "meta")
                });
                if terrain_file && floptle_vfs::remove_file(&p).is_ok() {
                    removed += 1;
                }
            }
            // Tidy up: the dir if now empty, then its parent if that emptied too
            // (a game's saves/<slot>/terrain layout leaves saves/<slot> behind).
            if floptle_vfs::remove_dir(&dir).is_ok()
                && let Some(parent) = dir.parent()
            {
                let _ = floptle_vfs::remove_dir(parent);
            }
            Ok(removed)
        }) {
            let _ = t.set("deleteSaveDir", f);
        }
    }

    // terrain.flush() — write every edited resident field to the save slot now
    // (terrain.saveDir must be set). Call at checkpoints and on exit-to-menu so
    // a slot always reloads from fast files instead of regenerating; streaming
    // already flushes on its own when bodies stream out.
    {
        let fl = flush.clone();
        if let Ok(f) = lua.create_function(move |_, ()| {
            *fl.borrow_mut() = true;
            Ok(())
        }) {
            let _ = t.set("flush", f);
        }
    }

    // terrain.warm(bodyName) — keep that body's terrain resident this frame
    // regardless of where the ship/player physically is: it loads if cold and
    // never streams out. Immediate mode (call it every frame while you care) —
    // the map calls it for its tab-focused planet so focusing a far world
    // streams its real terrain in while everything else stays a cheap sphere.
    {
        let w = warm.clone();
        if let Ok(f) = lua.create_function(move |_, name: String| {
            let mut w = w.borrow_mut();
            if w.len() < 32 && !w.contains(&name) {
                w.push(name);
            }
            Ok(())
        }) {
            let _ = t.set("warm", f);
        }
    }

    // terrain.lodAnchor(node | vec3 | nil) — what terrain detail, the far-body
    // impostor switch and far-vessel freezing follow, in place of the render
    // camera. A map view that pulls the camera thousands of units off leaves
    // the ground under the anchor exactly as it was. nil hands it back to the
    // camera. Returns the anchor it replaced (a node handle, a vec3, or nil).
    {
        let la = lod_anchor.clone();
        if let Ok(f) = lua.create_function(move |lua, v: mlua::MultiValue| {
            let prev = *la.borrow();
            if let Some(v) = v.into_iter().next() {
                let next = match &v {
                    Value::Nil => None,
                    Value::Table(t) if t.raw_get::<u32>("__id").is_ok() => Some(LodAnchor::Node(t.raw_get("__id")?)),
                    other => {
                        let p = crate::math_api::vec3_of(other).ok_or_else(|| {
                            mlua::Error::runtime("terrain.lodAnchor takes a node, a vec3 or nil")
                        })?;
                        Some(LodAnchor::Point([p.x, p.y, p.z]))
                    }
                };
                *la.borrow_mut() = next;
            }
            Ok(match prev {
                None => Value::Nil,
                Some(LodAnchor::Node(id)) => Value::Table(crate::env::new_node_handle(lua, id)?),
                Some(LodAnchor::Point(p)) => mlua::IntoLua::into_lua(crate::math_api::LuaVec3(glam::DVec3::from(p)), lua)?,
            })
        }) {
            let _ = t.set("lodAnchor", f);
        }
    }

    // terrain.editBudget([ms]) — how many milliseconds of a frame the engine
    // spends landing script terrain edits; the rest wait, in order, for the
    // frames after. A loading screen that covers the view can raise it to land
    // a big batch sooner, and lower it again (or pass nil for the default) as
    // the view is shown. Clamped to 0.5..1000 ms. Returns the budget it
    // replaced, in ms; with no argument it only reads it.
    {
        let eb = edit_budget.clone();
        if let Ok(f) = lua.create_function(move |_, v: mlua::MultiValue| {
            let prev = eb.get().unwrap_or(TERRAIN_EDIT_BUDGET_MS);
            if let Some(v) = v.into_iter().next() {
                match v {
                    Value::Nil => eb.set(None),
                    Value::Integer(n) => eb.set(Some((n as f64).clamp(0.5, 1000.0))),
                    Value::Number(n) if n.is_finite() => eb.set(Some(n.clamp(0.5, 1000.0))),
                    _ => return Err(mlua::Error::runtime("terrain.editBudget takes milliseconds or nil")),
                }
            }
            Ok(prev)
        }) {
            let _ = t.set("editBudget", f);
        }
    }

    // terrain.busy() — is the background terrain worker already occupied?
    //
    // For a game that generates its world as the player travels. Terrain fills
    // and streams share one background budget, so a game that queues more of
    // them whenever it feels like it queues them behind the ground somebody is
    // standing on. Asking first is how on-demand generation stays smooth: build
    // one thing, wait until this goes quiet, build the next.
    //
    // True while any field is generating or streaming in.
    {
        let b = busy.clone();
        if let Ok(f) = lua.create_function(move |_, ()| Ok(b.get())) {
            let _ = t.set("busy", f);
        }
    }

    // terrain.pending() — how many of this game's terrain edits (dig, sculpt,
    // flatten, paint) have not landed yet. Edits are applied a few
    // milliseconds' worth a frame, in the order they were made, so a batch of
    // a thousand flattens spreads over frames instead of stopping one; zero
    // means every edit made so far is in the ground, and a query reads it.
    {
        let (ops, queued) = (ops.clone(), queued.clone());
        if let Ok(f) = lua.create_function(move |_, ()| Ok(ops.borrow().len() + queued.get())) {
            let _ = t.set("pending", f);
        }
    }

    // terrain.capacity() — how many more edits the queue takes right now
    // before it refuses them. A game sending a big batch sends at most this
    // many a frame and the rest on the frames after; nothing it sends that
    // way is ever refused.
    {
        let (ops, queued) = (ops.clone(), queued.clone());
        if let Ok(f) = lua.create_function(move |_, ()| {
            Ok(MAX_PENDING_OPS.saturating_sub(ops.borrow().len() + queued.get()))
        }) {
            let _ = t.set("capacity", f);
        }
    }

    // terrain.ready(x, y, z [, r]) — is the ground within `r` of the point
    // made, exactly and for good? False where a terrain body that reaches it
    // is still generating, streaming in, or put away (what `query`, `height`
    // and `raycast` read there is not that body's ground at all), and false
    // where the ground there is still a stand-in: far ground a streamed world
    // holds as a coarse copy, and deep rock not generated yet, both made
    // exactly only when something comes near or `terrain.prepare` asks. An
    // answer read where this is true does not change until the ground is
    // edited; one read where it is false may. True over open space no terrain
    // reaches.
    {
        let unmade = unmade.clone();
        let cols = colliders.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z, r): (f64, f64, f64, Option<f64>)| {
            let r = r.unwrap_or(0.0).max(0.0);
            if unmade.borrow().iter().any(|u| u.touches([x, y, z], r)) {
                return Ok(false);
            }
            Ok(!stand_in_near(&cols.borrow(), glam::DVec3::new(x, y, z), r))
        }) {
            let _ = t.set("ready", f);
        }
    }

    // terrain.prepare(x, y, z, r) — make the stand-in ground within `r` of
    // the point exactly, off the frame: the far ground of a streamed world and
    // deep rock not generated yet. For a game about to plan from queries
    // somewhere nobody is standing; `terrain.ready` turns true once it has
    // landed. r is capped at 256.
    {
        let prepare = prepare.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z, r): (f64, f64, f64, f64)| {
            let mut q = prepare.borrow_mut();
            if q.len() < 256 && r.is_finite() && r >= 0.0 {
                q.push(([x, y, z], r.min(256.0)));
            }
            Ok(())
        }) {
            let _ = t.set("prepare", f);
        }
    }

    // terrain.impostorAt(id, { radii = n } | { pixels = n }) — when the planet
    // with terrain id `id` stops drawing its ground and draws as one shaded
    // ball: past `radii` of its radii from the camera, or once it is smaller
    // on screen than `pixels` across. With no rule it goes back to the
    // default, sixty radii.
    {
        let impostors = impostors.clone();
        if let Ok(f) = lua.create_function(move |_, (id, opts): (u32, Option<Table>)| {
            const CALL: &str = "terrain.impostorAt";
            let rule = match &opts {
                None => None,
                Some(t) => {
                    crate::opts::check_keys(t, &["radii", "pixels"], CALL)?;
                    let radii = t.raw_get::<Option<f64>>("radii")?;
                    let pixels = t.raw_get::<Option<f64>>("pixels")?;
                    match (radii, pixels) {
                        (Some(r), None) if r.is_finite() && r >= 0.0 => Some(ImpostorRule::Radii(r)),
                        (None, Some(p)) if p.is_finite() && p > 0.0 => Some(ImpostorRule::Pixels(p)),
                        (Some(_), Some(_)) => {
                            return Err(mlua::Error::runtime(format!("{CALL}: give radii or pixels, not both")));
                        }
                        (None, None) => None,
                        _ => {
                            return Err(mlua::Error::runtime(format!(
                                "{CALL}: radii is 0 or more, pixels more than 0"
                            )));
                        }
                    }
                }
            };
            impostors.borrow_mut().push((id, rule));
            Ok(())
        }) {
            let _ = t.set("impostorAt", f);
        }
    }

    // terrain.saveDir(path) / terrain.saveDir() — set (or read) the game's
    // Save-slot directory for player-edited terrain (relative to the project
    // root, e.g. "saves/slot1/terrain"). While set, the streaming system loads
    // a body's field from here first (before the project file or its genspec)
    // and writes edited fields back here on evict/stop — so a player's digs
    // persist per save slot without ever touching the authored project data.
    // Pass "" to clear. G2 galaxy streaming (docs/subsystems/large-world-space.md).
    {
        let sd = save_dir.clone();
        if let Ok(f) = lua.create_function(move |_, path: Option<String>| {
            match path {
                Some(p) => {
                    // The same rule as `deleteSaveDir`, for the same reason:
                    // edited fields are written here, and a directory outside
                    // the project is not a save slot.
                    if !p.is_empty() && !valid_save_dir(&p) {
                        return Err(mlua::Error::RuntimeError(format!(
                            "terrain.saveDir(\"{p}\"): needs a relative project path with no \"..\""
                        )));
                    }
                    *sd.borrow_mut() = if p.is_empty() { None } else { Some(p) };
                    Ok(None)
                }
                None => Ok(sd.borrow().clone()),
            }
        }) {
            let _ = t.set("saveDir", f);
        }
    }

    // Shared push-with-cap helper. Stamps the op's id and hands it back, so
    // every edit verb can return the receipt its yield will arrive under.
    let push = {
        let ops = ops.clone();
        let logs = logs.clone();
        let ids = next_op_id.clone();
        let waiting = queued.clone();
        let warned = Rc::new(std::cell::Cell::new(false));
        let dropped = dropped.clone();
        move |mut op: TerrainOp| -> u64 {
            let mut q = ops.borrow_mut();
            if q.len() + waiting.get() >= MAX_PENDING_OPS {
                dropped.set(dropped.get() + 1);
                if !warned.replace(true) {
                    logs.borrow_mut().push(crate::ScriptLog {
                        level: crate::LogLevel::Warn,
                        msg: format!(
                            "terrain: {MAX_PENDING_OPS} edits are already waiting to land \
                             (a few milliseconds of them land a frame) — this one and any more \
                             are refused until they catch up. Each refused edit answers 0 and is \
                             counted in perf.counts().terrainDropped; send at most \
                             terrain.capacity() a frame to never lose one"
                        ),
                        source: None,
                    });
                }
                // A dropped op yields nothing; 0 is the "no receipt" id.
                return 0;
            }
            op.id = ids.get().wrapping_add(1);
            ids.set(op.id);
            let id = op.id;
            q.push(op);
            id
        }
    };

    // terrain.sculpt(x,y,z, radius, strength?, mode?, opts?) — mode: "raise"
    // (default), "lower"/"dig", "smooth", "flatten". opts.normal is the plane
    // flatten levels to. Queued; lands this tick.
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, Option<f64>, Option<String>, Option<Table>);
        if let Ok(f) = lua.create_function(move |_, (x, y, z, radius, strength, mode, opts): Args| {
            let mut normal = None;
            if let Some(o) = &opts {
                crate::opts::check_keys(o, SCULPT_KEYS, "terrain.sculpt")?;
                let v: Value = o.get("normal")?;
                if !v.is_nil() {
                    let n = crate::math_api::vec3_of(&v)
                        .filter(|n| n.length_squared() > 1e-12 && n.is_finite())
                        .ok_or_else(|| {
                            mlua::Error::runtime("terrain.sculpt: `normal` must be a non-zero vec3 or {x,y,z}")
                        })?
                        .normalize();
                    normal = Some([n.x, n.y, n.z]);
                }
            }
            let mode = match mode.as_deref().unwrap_or("raise") {
                "raise" => TerrainOpMode::Raise,
                "lower" | "dig" => TerrainOpMode::Lower,
                "smooth" => TerrainOpMode::Smooth,
                "flatten" => TerrainOpMode::Flatten,
                other => {
                    return Err(mlua::Error::RuntimeError(format!(
                        "terrain.sculpt: unknown mode '{other}' (raise, lower/dig, smooth, flatten)"
                    )))
                }
            };
            Ok(push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                mode,
                id: 0,
                normal,
            }))
        }) {
            let _ = t.set("sculpt", f);
        }
    }

    // terrain.dig(x,y,z, radius, strength?) — sugar for sculpt(..., "lower"): the
    // verb every digging game actually wants.
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, Option<f64>);
        if let Ok(f) = lua.create_function(move |_, (x, y, z, radius, strength): Args| {
            Ok(push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                mode: TerrainOpMode::Lower,
                id: 0,
                normal: None,
            }))
        }) {
            let _ = t.set("dig", f);
        }
    }

    // terrain.paint(x,y,z, radius, r,g,b, strength?) — recolor the surface.
    // Answers the edit's receipt, as sculpt does: 0 when the queue was full and
    // the edit was refused.
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, f64, f64, f64, Option<f64>);
        if let Ok(f) =
            lua.create_function(move |_, (x, y, z, radius, r, g, b, strength): Args| {
                Ok(push(TerrainOp {
                    pos: [x, y, z],
                    radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                    strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                    mode: TerrainOpMode::Paint([r as f32, g as f32, b as f32]),
                    id: 0,
                    normal: None,
                }))
            })
        {
            let _ = t.set("paint", f);
        }
    }

    // terrain.paintTexture(x,y,z, radius, slot) — paint a palette texture slot
    // (1-based, matching the Terrain tab's palette; 0 clears back to flat color).
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, f64);
        if let Ok(f) = lua.create_function(move |_, (x, y, z, radius, slot): Args| {
            Ok(push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: 1.0,
                mode: TerrainOpMode::PaintTexture((slot.max(0.0) as u8).min(32)),
                id: 0,
                normal: None,
            }))
        }) {
            let _ = t.set("paintTexture", f);
        }
    }

    // terrain.generatePlanet(id, opts) — replace terrain volume `id`'s whole
    // field with a generated planet (floptle_field::procgen::PlanetFill; runs
    // on an editor background thread — heavyweight, seconds per body). Every
    // knob is optional; layer paints are {slot=…, color={r,g,b}}:
    //   terrain.generatePlanet(2, { seed=41, radius=180, relief=9,
    //     bumpFreq=4.5, caveDepth=60, coreR=12, corePaint={slot=6,color={1,.8,.6}},
    //     craters=12, craterMin=0.12, craterMax=0.26, craterDust={slot=11,color=…},
    //     surfaceA={slot=1,color=…}, surfaceB={slot=2,color=…},
    //     patchBias=0.45, patchThr=0.08,
    //     subsoil={slot=3,color=…}, subsoilDepth=2.4,
    //     strata={slot=4,color=…}, strataDepth=9,
    //     deep={slot=5,color=…},
    //     pockets={slot=7,color=…,threshold=0.46,minDepth=6},
    //     seam={slot=6,color=…,minDepth=20,center=0.32,width=0.045},
    //     iceCaps={lat=0.75,slot=12,color=…} })
    {
        let q = generates.clone();
        let logs2 = logs.clone();
        let busy2 = busy.clone();
        let generator = generator.clone();
        if let Ok(f) = lua.create_function(move |_, (id, opts): (u32, Option<Table>)| {
            let mut fill = planet_fill_from_table(opts.as_ref())?;
            fill.generator.get_or_insert(generator.get());
            let mut q = q.borrow_mut();
            if q.len() >= 16 {
                logs2.borrow_mut().push(crate::ScriptLog {
                    level: crate::LogLevel::Warn,
                    msg: "terrain.generatePlanet: too many generations queued (16 max)"
                        .into(),
                    source: None,
                });
                return Ok(());
            }
            q.push((id, fill));
            // **Busy from this instant**, not from the next publish.
            // The consumer is a game that queues a world and
            // then asks whether it may queue the next one — a caller told "idle"
            // about work it just asked for queues it twice, and the second one
            // goes in behind the ground a player is standing on. The editor's
            // per-frame publish owns the flag from the real job state after
            // this, and is what lowers it again.
            //
            // Only the queueing calls raise it. `terrain.warm` does
            // not: it is immediate-mode, called every frame for as long as a
            // caller cares about a body, so raising it there would pin the flag
            // true for the whole time the map has a planet focused.
            busy2.set(true);
            Ok(())
        }) {
            let _ = t.set("generatePlanet", f);
        }
    }

    // terrain.planetRivers(opts [, callback]) → the rivers and ravines
    // `generatePlanet` with the same opts carves, without generating: { {river=,
    // depth=, points={x1,y1,z1, ...}, widths={...}}, ... }. Points are
    // planet-local, on the sphere of the planet's radius; the ground there is
    // the carved bed. Given a callback, the work runs off the script thread and
    // the callback gets the answer in a later frame.
    {
        let planet = planet.clone();
        let generator = generator.clone();
        if let Ok(f) = lua.create_function(move |lua, (opts, cb): (Option<Table>, Option<mlua::Function>)| {
            let mut fill = planet_fill_from_table(opts.as_ref())?;
            // The rivers of the world this project makes from these options.
            fill.generator.get_or_insert(generator.get());
            match cb {
                Some(cb) => {
                    planet.borrow_mut().ask(lua, cb, move || {
                        PlanetAnswer::Channels(floptle_field::relief::channels_of(&fill), fill.radius)
                    })?;
                    Ok(Value::Nil)
                }
                None => Ok(Value::Table(channels_table(lua, &floptle_field::relief::channels_of(&fill), fill.radius)?)),
            }
        }) {
            let _ = t.set("planetRivers", f);
        }
    }

    // terrain.planetLakes(opts [, { cell, minDepth, seaLevel }] [, callback]) →
    // the lakes the planet's basins hold, without generating it: { {level=,
    // center=, reach=, area=, depth=}, ... }, largest first. `center` is
    // planet-local, on the lake's surface. Given a callback, the work runs off
    // the script thread and the callback gets the answer in a later frame.
    {
        let planet = planet.clone();
        let generator = generator.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let mut args = args.into_iter();
            let opts = match args.next() {
                None | Some(Value::Nil) => None,
                Some(Value::Table(t)) => Some(t),
                Some(_) => return Err(mlua::Error::runtime("terrain.planetLakes: opts must be a table")),
            };
            let (query, cb) = match (args.next(), args.next()) {
                (Some(Value::Function(f)), _) => (None, Some(f)),
                (Some(Value::Table(q)), Some(Value::Function(f))) => (Some(q), Some(f)),
                (Some(Value::Table(q)), None | Some(Value::Nil)) => (Some(q), None),
                (None | Some(Value::Nil), Some(Value::Function(f))) => (None, Some(f)),
                (None | Some(Value::Nil), None | Some(Value::Nil)) => (None, None),
                _ => return Err(mlua::Error::runtime("terrain.planetLakes(opts [, query] [, callback]): query is a table and callback a function")),
            };
            let mut fill = planet_fill_from_table(opts.as_ref())?;
            fill.generator.get_or_insert(generator.get());
            if let Some(q) = &query {
                crate::opts::check_keys(q, &["cell", "minDepth", "seaLevel"], "terrain.planetLakes")?;
            }
            let get = |k: &str| query.as_ref().and_then(|q| q.get::<Option<f32>>(k).ok().flatten());
            let sea = get("seaLevel").unwrap_or(fill.rivers.as_ref().map_or(f32::NEG_INFINITY, |r| r.sea_level));
            let (cell, min_depth) = (get("cell").unwrap_or(3.0), get("minDepth").unwrap_or(0.5));
            match cb {
                Some(cb) => {
                    planet.borrow_mut().ask(lua, cb, move || {
                        PlanetAnswer::Lakes(floptle_field::procgen::lakes_of(&fill, cell, min_depth, sea), fill.radius)
                    })?;
                    Ok(Value::Nil)
                }
                None => Ok(Value::Table(lakes_table(lua, &floptle_field::procgen::lakes_of(&fill, cell, min_depth, sea), fill.radius)?)),
            }
        }) {
            let _ = t.set("planetLakes", f);
        }
    }

    // terrain.yields() → the reports for edits that have landed since the last
    // call, drained. An op is queued and applied after the script pass, so the
    // report for a dab arrives on the following frame; the id `dig` returned
    // ties the two together.
    {
        let ys = yields.clone();
        if let Ok(f) = lua.create_function(move |lua, ()| {
            let out = lua.create_table()?;
            for (i, y) in ys.borrow_mut().drain(..).enumerate() {
                let r = lua.create_table()?;
                r.set("id", y.id)?;
                r.set("removed", y.removed)?;
                r.set("added", y.added)?;
                r.set("untextured", y.untextured)?;
                let slots = lua.create_table()?;
                for (slot, vol) in &y.slots {
                    slots.set(*slot, *vol)?;
                }
                r.set("slots", slots)?;
                out.set(i + 1, r)?;
            }
            Ok(out)
        }) {
            let _ = t.set("yields", f);
        }
    }

    // terrain.slotAt(x,y,z) → the texture-palette slot at a world point, or nil
    // where the field is untextured. The material half of the question
    // `terrain.query` answers the distance half of: survey before you cut, and
    // let a footstep know what it is standing on.
    {
        let cols = colliders.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let mut best: Option<(f32, Option<u8>)> = None;
            for c in cols.borrow().iter() {
                let Some(t) = c.shape.chunk_terrain() else { continue };
                let s = t.scale.max(1e-6);
                let local = (t.rot.inverse()
                    * Vec3::new(
                        (x - c.anchor.x) as f32,
                        (y - c.anchor.y) as f32,
                        (z - c.anchor.z) as f32,
                    ))
                    / s;
                // Nearest terrain wins, the same rule an op uses to choose which
                // field it lands on — so a survey and the dig that follows it
                // cannot disagree about which terrain they meant.
                let d = t.field.d(local).abs() * s;
                if best.as_ref().is_none_or(|b| d < b.0) {
                    best = Some((d, t.field.slot_at(local)));
                }
            }
            Ok(best.and_then(|b| b.1))
        }) {
            let _ = t.set("slotAt", f);
        }
    }

    // terrain.query(x,y,z) → signed distance to the nearest terrain surface
    // (negative = inside rock), or nil when the scene has no terrain. World coords.
    // The number is all it answers, as it always has: whether the ground there
    // is made yet is `terrain.ready`'s question, kept out of this one so a
    // `math.min(a, terrain.query(...))` never grows an argument.
    {
        let cols = colliders.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let mut best: Option<f32> = None;
            for c in cols.borrow().iter() {
                let Some(t) = c.shape.chunk_terrain() else { continue };
                // Through the node's full frame (rotation + uniform scale), and
                // back to world distance by the scale.
                let s = t.scale.max(1e-6);
                let local = (t.rot.inverse()
                    * Vec3::new(
                        (x - c.anchor.x) as f32,
                        (y - c.anchor.y) as f32,
                        (z - c.anchor.z) as f32,
                    ))
                    / s;
                let d = t.field.d(local) * s;
                best = Some(match best {
                    Some(b) => b.min(d),
                    None => d,
                });
            }
            Ok(best.map(|d| d as f64))
        }) {
            let _ = t.set("query", f);
        }
    }

    // terrain.raycast(origin, dir [, maxDist]) → the first terrain surface
    // along the ray, read from the ground's own field rather than from
    // collision meshes that may not be built yet: { pos =, normal =,
    // distance = }. A miss answers nil and why: nil, "air" when the ray
    // reached maxDist (default 1000) through open space, and nil, "unknown",
    // distance when it reached ground the world has not made yet first — the
    // distance along the ray where it did.
    {
        let cols = colliders.clone();
        let unmade = unmade.clone();
        if let Ok(f) = lua.create_function(
            move |lua, (o, d, max): (crate::math_api::LuaVec3, crate::math_api::LuaVec3, Option<f64>)| {
                let max = max.unwrap_or(1000.0).clamp(0.0, 1.0e6);
                let Some(dir) = d.0.try_normalize() else {
                    return Err(mlua::Error::runtime("terrain.raycast: dir must be a non-zero vector"));
                };
                let o = o.0;
                let mut best: Option<(f64, glam::DVec3, glam::DVec3)> = None;
                for c in cols.borrow().iter() {
                    let Some(t) = c.shape.chunk_terrain() else { continue };
                    let s = t.scale.max(1e-6);
                    let inv = t.rot.inverse();
                    let start = (inv * (o - c.anchor).as_vec3()) / s;
                    let ld = inv * dir.as_vec3();
                    let Some(hit) = t.field.raycast(start, ld, (max / s as f64) as f32) else { continue };
                    let world = c.anchor + (t.rot * (hit * s)).as_dvec3();
                    let dist = (world - o).length();
                    if best.as_ref().is_none_or(|b| dist < b.0) {
                        let n = (t.rot * t.field.grad(hit)).as_dvec3();
                        best = Some((dist, world, n));
                    }
                }
                let reach = best.as_ref().map_or(max, |b| b.0);
                let entry = unmade
                    .borrow()
                    .iter()
                    .filter_map(|u| u.entry([o.x, o.y, o.z], [dir.x, dir.y, dir.z], reach))
                    .fold(None, |a: Option<f64>, t| Some(a.map_or(t, |a| a.min(t))));
                if let Some(at) = entry {
                    return (Value::Nil, "unknown", at).into_lua_multi(lua);
                }
                match best {
                    None => (Value::Nil, "air").into_lua_multi(lua),
                    Some((dist, pos, n)) => {
                        let hit = lua.create_table()?;
                        hit.set("pos", crate::math_api::LuaVec3(pos))?;
                        hit.set("normal", crate::math_api::LuaVec3(n))?;
                        hit.set("distance", dist)?;
                        // False where the ground hit is a stand-in: the hit
                        // may move when it is made exactly (`terrain.ready`).
                        hit.set("exact", !stand_in_near(&cols.borrow(), pos, 0.0))?;
                        hit.into_lua_multi(lua)
                    }
                }
            },
        ) {
            let _ = t.set("raycast", f);
        }
    }

    // terrain.height(x, z) → the world-space Y of the highest terrain surface under
    // (x, z), or nil if no terrain is hit. Casts down each terrain's own field from
    // just above its content bounds — chunk-accurate and cheap.
    {
        let cols = colliders.clone();
        if let Ok(f) = lua.create_function(move |_, (x, z): (f64, f64)| {
            let mut best: Option<f64> = None;
            for c in cols.borrow().iter() {
                let Some(t) = c.shape.chunk_terrain() else { continue };
                let Some((lo, hi)) = t.field.bounds() else { continue };
                // Cast the world-down ray in the field's local frame (the node
                // may be rotated/scaled); reconstruct the hit's world Y through
                // the same frame.
                let s = t.scale.max(1e-6);
                let inv = t.rot.inverse();
                let down = (inv * Vec3::NEG_Y).normalize_or_zero();
                // Farthest content point from the anchor bounds the start height:
                // above that is above everything, no matter how the node is rotated.
                let bound_r = Vec3::new(
                    lo.x.abs().max(hi.x.abs()),
                    lo.y.abs().max(hi.y.abs()),
                    lo.z.abs().max(hi.z.abs()),
                )
                .length();
                let px = Vec3::new((x - c.anchor.x) as f32, 0.0, (z - c.anchor.z) as f32);
                let start_w = px + Vec3::Y * (bound_r * s + 2.0);
                let start = (inv * start_w) / s;
                if let Some(hit) = t.field.raycast(start, down, 2.0 * bound_r + 8.0 / s) {
                    let hw = t.rot * (hit * s);
                    let wy = c.anchor.y + hw.y as f64;
                    best = Some(match best {
                        Some(b) if b >= wy => b,
                        _ => wy,
                    });
                }
            }
            Ok(best)
        }) {
            let _ = t.set("height", f);
        }
    }

    let _ = lua.globals().set("terrain", t);
}

/// Parse the Lua `generatePlanet`/`setTerrainGen` opts table into a
/// [`floptle_field::procgen::PlanetFill`] — one parser for both the immediate
/// generation queue and the on-node genspec (G2), so their vocabularies can
/// never drift apart. Every field optional; camelCase keys.
/// Every key a planet-generation options table reads. The seven
/// paint layers all take `{ slot =, color = }`; `pockets`, `seam` and `iceCaps`
/// have their own inner keys, checked separately below.
pub(crate) const PLANET_KEYS: &[&str] = &[
    "seed", "radius", "voxel", "relief", "bumpFreq", "caveDepth", "coreR", "craters",
    "craterMin", "craterMax", "patchBias", "patchThr", "subsoilDepth", "strataDepth",
    "corePaint", "craterDust", "surfaceA", "surfaceB", "subsoil", "strata", "deep", "pockets",
    "seam", "iceCaps", "mountains", "warp", "plateau", "terrace", "overhangs", "ravines", "rivers",
    "biomes", "moistureScale", "deferCaves", "generator", "smallBody",
];

/// A small body's keys.
const SMALL_BODY_KEYS: &[&str] = &["shape", "elongation", "lumpiness", "neck", "lumps"];

/// The relief layers' inner keys.
const MOUNTAIN_KEYS: &[&str] = &["height", "scale", "sharpness", "coverage", "octaves", "latitude", "latitudeEdge"];
const WARP_KEYS: &[&str] = &["strength", "scale", "latitude", "latitudeEdge"];
const PLATEAU_KEYS: &[&str] = &["height", "smooth", "latitude", "latitudeEdge"];
const TERRACE_KEYS: &[&str] = &["step", "sharpness", "latitude", "latitudeEdge"];
const OVERHANG_KEYS: &[&str] = &["depth", "scale", "strength"];
const RAVINE_KEYS: &[&str] = &["count", "depth", "width", "length", "meander"];
const RIVER_KEYS: &[&str] = &["count", "depth", "width", "seaLevel", "meander"];
const BIOME_KEYS: &[&str] = &["slot", "color", "latitude", "altitude", "slope", "moisture", "nearRiver"];

/// The inner keys of a paint layer, and of the three special sub-tables.
const PAINT_KEYS: &[&str] = &["slot", "color"];
const POCKET_KEYS: &[&str] = &["slot", "color", "threshold", "minDepth"];
const SEAM_KEYS: &[&str] = &["slot", "color", "minDepth", "center", "width"];
const ICECAP_KEYS: &[&str] = &["lat", "slot", "color"];

pub(crate) fn planet_fill_from_table(
    opts: Option<&Table>,
) -> mlua::Result<floptle_field::procgen::PlanetFill> {
    use crate::opts::check_keys;
    use floptle_field::procgen::{GlowPockets, LayerPaint, PlanetFill, SeamSpec};
    // A planet is generated once, on a background thread, and cached to disk.
    // A misspelled `relif` therefore produces a world that is wrong and stays
    // wrong across restarts, with nothing anywhere saying which key did it.
    const CALL: &str = "terrain.generatePlanet";
    // No version unless the options name one: the caller decides what an
    // unnamed one means (the project's), and an on-node genspec written
    // without one reads back as the same text.
    let mut fill = PlanetFill { generator: None, ..PlanetFill::default() };
    {
        if let Some(t) = opts {
                check_keys(t, PLANET_KEYS, CALL)?;
                for k in ["corePaint", "craterDust", "surfaceA", "surfaceB", "subsoil", "strata", "deep"] {
                    if let Some(v) = t.raw_get::<Option<Table>>(k).ok().flatten() {
                        check_keys(&v, PAINT_KEYS, &format!("{CALL} {k}"))?;
                    }
                }
                for (k, keys) in
                    [("pockets", POCKET_KEYS), ("seam", SEAM_KEYS), ("iceCaps", ICECAP_KEYS)]
                {
                    if let Some(v) = t.raw_get::<Option<Table>>(k).ok().flatten() {
                        check_keys(&v, keys, &format!("{CALL} {k}"))?;
                    }
                }
                let gf = |k: &str| t.raw_get::<Option<f64>>(k).ok().flatten();
                let gc = |v: &Table| -> Option<[f32; 3]> {
                    let c: Table = v.raw_get::<Option<Table>>("color").ok().flatten()?;
                    Some([
                        c.raw_get::<Option<f64>>(1).ok().flatten().unwrap_or(1.0) as f32,
                        c.raw_get::<Option<f64>>(2).ok().flatten().unwrap_or(1.0) as f32,
                        c.raw_get::<Option<f64>>(3).ok().flatten().unwrap_or(1.0) as f32,
                    ])
                };
                let gp = |k: &str, cur: LayerPaint| -> LayerPaint {
                    match t.raw_get::<Option<Table>>(k).ok().flatten() {
                        Some(v) => LayerPaint {
                            slot: v
                                .raw_get::<Option<u32>>("slot")
                                .ok()
                                .flatten()
                                .map(|s| s as u8)
                                .unwrap_or(cur.slot),
                            color: gc(&v).unwrap_or(cur.color),
                        },
                        None => cur,
                    }
                };
                if let Some(v) = gf("seed") { fill.seed = v as u32; }
                if let Some(v) = gf("radius") { fill.radius = v as f32; }
                if let Some(v) = gf("voxel") { fill.voxel = v as f32; }
                if let Some(v) = gf("relief") { fill.relief = v as f32; }
                if let Some(v) = gf("bumpFreq") { fill.bump_freq = v as f32; }
                if let Some(v) = gf("caveDepth") { fill.cave_depth = v as f32; }
                if let Some(v) = gf("coreR") { fill.core_r = v as f32; }
                if let Some(v) = gf("craters") { fill.craters = v as u32; }
                if let Some(v) = gf("craterMin") { fill.crater_min = v as f32; }
                if let Some(v) = gf("craterMax") { fill.crater_max = v as f32; }
                if let Some(v) = gf("patchBias") { fill.patch_bias = v as f32; }
                if let Some(v) = gf("patchThr") { fill.patch_thr = v as f32; }
                if let Some(v) = gf("subsoilDepth") { fill.subsoil_depth = v as f32; }
                if let Some(v) = gf("strataDepth") { fill.strata_depth = v as f32; }
                fill.core_paint = gp("corePaint", fill.core_paint);
                fill.crater_dust = gp("craterDust", fill.crater_dust);
                fill.surface_a = gp("surfaceA", fill.surface_a);
                fill.surface_b = gp("surfaceB", fill.surface_b);
                fill.subsoil = gp("subsoil", fill.subsoil);
                fill.strata = gp("strata", fill.strata);
                fill.deep = gp("deep", fill.deep);
                if let Some(v) = t.raw_get::<Option<Table>>("pockets").ok().flatten() {
                    fill.pockets = Some(GlowPockets {
                        paint: LayerPaint {
                            slot: v.raw_get::<Option<u32>>("slot").ok().flatten().unwrap_or(7)
                                as u8,
                            color: gc(&v).unwrap_or([0.72, 0.65, 0.85]),
                        },
                        threshold: v
                            .raw_get::<Option<f64>>("threshold")
                            .ok()
                            .flatten()
                            .unwrap_or(0.46) as f32,
                        min_depth: v
                            .raw_get::<Option<f64>>("minDepth")
                            .ok()
                            .flatten()
                            .unwrap_or(6.0) as f32,
                    });
                }
                if let Some(v) = t.raw_get::<Option<Table>>("seam").ok().flatten() {
                    fill.seam = Some(SeamSpec {
                        paint: LayerPaint {
                            slot: v.raw_get::<Option<u32>>("slot").ok().flatten().unwrap_or(6)
                                as u8,
                            color: gc(&v).unwrap_or([0.95, 0.85, 0.72]),
                        },
                        min_depth: v
                            .raw_get::<Option<f64>>("minDepth")
                            .ok()
                            .flatten()
                            .unwrap_or(20.0) as f32,
                        center: v.raw_get::<Option<f64>>("center").ok().flatten().unwrap_or(0.32)
                            as f32,
                        width: v.raw_get::<Option<f64>>("width").ok().flatten().unwrap_or(0.045)
                            as f32,
                    });
                }
                if let Some(v) = t.raw_get::<Option<Table>>("iceCaps").ok().flatten() {
                    fill.ice_caps = Some((
                        v.raw_get::<Option<f64>>("lat").ok().flatten().unwrap_or(0.75) as f32,
                        LayerPaint {
                            slot: v.raw_get::<Option<u32>>("slot").ok().flatten().unwrap_or(12)
                                as u8,
                            color: gc(&v).unwrap_or([0.85, 0.92, 0.98]),
                        },
                    ));
                }
                relief_layers_from_table(t, &mut fill, CALL)?;
        }
    }
    Ok(fill)
}

/// The relief layers of a planet's opts: `mountains`, `warp`, `plateau`,
/// `terrace`, `overhangs`, `ravines`, `rivers` (each a table whose keys all
/// default), `biomes` (a list of rules) and `moistureScale`.
fn relief_layers_from_table(t: &Table, fill: &mut floptle_field::procgen::PlanetFill, call: &str) -> mlua::Result<()> {
    use crate::opts::check_keys;
    use floptle_field::procgen as pg;
    let sub = |k: &str, keys: &[&str]| -> mlua::Result<Option<Table>> {
        let v = t.raw_get::<Option<Table>>(k).ok().flatten();
        if let Some(v) = &v {
            check_keys(v, keys, &format!("{call} {k}"))?;
        }
        Ok(v)
    };
    let num = |v: &Table, k: &str, d: f32| v.raw_get::<Option<f64>>(k).ok().flatten().map_or(d, |x| x as f32);
    let range = |v: &Table, k: &str| -> Option<[f32; 2]> {
        let r: Table = v.raw_get::<Option<Table>>(k).ok().flatten()?;
        Some([r.raw_get::<Option<f64>>(1).ok().flatten()? as f32, r.raw_get::<Option<f64>>(2).ok().flatten()? as f32])
    };
    if let Some(v) = sub("mountains", MOUNTAIN_KEYS)? {
        let d = pg::Mountains::default();
        fill.mountains = Some(pg::Mountains {
            height: num(&v, "height", d.height),
            scale: num(&v, "scale", d.scale),
            sharpness: num(&v, "sharpness", d.sharpness),
            coverage: num(&v, "coverage", d.coverage),
            octaves: num(&v, "octaves", d.octaves as f32).max(1.0) as u32,
            latitude: range(&v, "latitude"),
            latitude_edge: num(&v, "latitudeEdge", d.latitude_edge),
        });
    }
    if let Some(v) = sub("warp", WARP_KEYS)? {
        fill.warp = Some(pg::Warp {
            strength: num(&v, "strength", 0.15),
            scale: num(&v, "scale", 2.0),
            latitude: range(&v, "latitude"),
            latitude_edge: num(&v, "latitudeEdge", 5.0),
        });
    }
    if let Some(v) = sub("plateau", PLATEAU_KEYS)? {
        let d = pg::Plateau::default();
        fill.plateau = Some(pg::Plateau {
            height: num(&v, "height", d.height),
            smooth: num(&v, "smooth", d.smooth),
            latitude: range(&v, "latitude"),
            latitude_edge: num(&v, "latitudeEdge", d.latitude_edge),
        });
    }
    if let Some(v) = sub("terrace", TERRACE_KEYS)? {
        let d = pg::Terrace::default();
        fill.terrace = Some(pg::Terrace {
            step: num(&v, "step", d.step),
            sharpness: num(&v, "sharpness", d.sharpness),
            latitude: range(&v, "latitude"),
            latitude_edge: num(&v, "latitudeEdge", d.latitude_edge),
        });
    }
    if let Some(v) = sub("overhangs", OVERHANG_KEYS)? {
        let d = pg::Overhangs::default();
        fill.overhangs = Some(pg::Overhangs {
            depth: num(&v, "depth", d.depth),
            scale: num(&v, "scale", d.scale),
            strength: num(&v, "strength", d.strength),
        });
    }
    if let Some(v) = sub("ravines", RAVINE_KEYS)? {
        let d = pg::Ravines::default();
        fill.ravines = Some(pg::Ravines {
            count: num(&v, "count", d.count as f32).max(0.0) as u32,
            depth: num(&v, "depth", d.depth),
            width: num(&v, "width", d.width),
            length: num(&v, "length", d.length),
            meander: num(&v, "meander", d.meander),
        });
    }
    if let Some(v) = sub("rivers", RIVER_KEYS)? {
        let d = pg::Rivers::default();
        fill.rivers = Some(pg::Rivers {
            count: num(&v, "count", d.count as f32).max(0.0) as u32,
            depth: num(&v, "depth", d.depth),
            width: num(&v, "width", d.width),
            sea_level: num(&v, "seaLevel", d.sea_level),
            meander: num(&v, "meander", d.meander),
        });
    }
    if let Some(list) = t.raw_get::<Option<Table>>("biomes").ok().flatten() {
        for (i, rule) in list.sequence_values::<Table>().enumerate() {
            let rule = rule.map_err(|_| mlua::Error::runtime(format!("{call} biomes[{}] is not a table", i + 1)))?;
            check_keys(&rule, BIOME_KEYS, &format!("{call} biomes[{}]", i + 1))?;
            let color = rule.raw_get::<Option<Table>>("color").ok().flatten().map(|c| {
                [1, 2, 3].map(|k| c.raw_get::<Option<f64>>(k).ok().flatten().unwrap_or(1.0) as f32)
            });
            fill.biomes.push(pg::Biome {
                paint: pg::LayerPaint {
                    slot: rule.raw_get::<Option<u32>>("slot").ok().flatten().unwrap_or(1) as u8,
                    color: color.unwrap_or([1.0; 3]),
                },
                latitude: range(&rule, "latitude"),
                altitude: range(&rule, "altitude"),
                slope: range(&rule, "slope"),
                moisture: range(&rule, "moisture"),
                near_river: rule.raw_get::<Option<f64>>("nearRiver").ok().flatten().map(|x| x as f32),
            });
        }
    }
    if let Some(v) = t.raw_get::<Option<f64>>("moistureScale").ok().flatten() {
        fill.moisture_scale = v as f32;
    }
    if let Some(v) = t.raw_get::<Option<bool>>("deferCaves").ok().flatten() {
        fill.defer_caves = v;
    }
    // An irregular small body instead of a ball.
    if let Some(b) = t.raw_get::<Option<Table>>("smallBody").ok().flatten() {
        crate::opts::check_keys(&b, SMALL_BODY_KEYS, "terrain.generatePlanet smallBody")?;
        use floptle_field::procgen::{SmallBody, SmallBodyShape};
        let mut body = SmallBody::default();
        if let Some(name) = b.raw_get::<Option<String>>("shape")? {
            body.shape = match name.as_str() {
                "potato" => SmallBodyShape::Potato,
                "binary" => SmallBodyShape::ContactBinary,
                "rubble" => SmallBodyShape::Rubble,
                other => {
                    return Err(mlua::Error::runtime(format!(
                        "terrain.generatePlanet: smallBody shape \"{other}\" is not one of \"potato\", \"binary\", \"rubble\""
                    )));
                }
            };
        }
        if let Some(v) = b.raw_get::<Option<f64>>("elongation")? {
            body.elongation = v as f32;
        }
        if let Some(v) = b.raw_get::<Option<f64>>("lumpiness")? {
            body.lumpiness = v as f32;
        }
        if let Some(v) = b.raw_get::<Option<f64>>("neck")? {
            body.neck = v as f32;
        }
        if let Some(v) = b.raw_get::<Option<u32>>("lumps")? {
            body.lumps = v;
        }
        fill.small_body = Some(body);
    }
    // Which generator's rules to make the world by. Named, it is kept with
    // the world; left out, the world is made by the project's.
    if let Some(v) = t.raw_get::<Option<i64>>("generator").ok().flatten() {
        let now = floptle_field::procgen::GENERATOR_VERSION;
        if !(0..=i64::from(now)).contains(&v) {
            return Err(mlua::Error::runtime(format!(
                "terrain.generatePlanet: generator = {v}, but this engine's planet generator is version {now} (0 to {now})"
            )));
        }
        fill.generator = Some(v as u32);
    }
    Ok(())
}

