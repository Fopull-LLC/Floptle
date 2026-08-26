//! The Lua `terrain.*` API — runtime terrain editing and queries (Terrain 2.0 P6).
//!
//! Writes (`sculpt`/`dig`/`paint`/`paintTexture`) QUEUE ops; the editor drains them
//! after the script pass and applies each to the authority `ChunkField`, the sim's
//! collider copy (so collision changes the same tick), the chunk remesh queue, and
//! the shadow-proxy region — the exact pipeline an editor brush dab takes. Reads
//! (`query`/`height`) run immediately against the LENT sim colliders, the same loan
//! `raycast(...)` uses, so they see the world as of this frame.
//!
//! Multiplayer note (documented in scripting.md): ops apply on the machine that runs
//! them. Until replicated terrain lands, run terrain edits SERVER-side and mirror
//! them to clients with an RPC that repeats the same call — the ops are deterministic
//! by construction (same call, same field result).

use std::cell::RefCell;
use std::rc::Rc;

use floptle_core::math::Vec3;
use mlua::{Lua, Table};

/// One queued terrain write, in WORLD coordinates (scripts speak world; the editor
/// converts into each terrain's local frame when applying).
#[derive(Clone, Debug)]
pub struct TerrainOp {
    pub pos: [f64; 3],
    pub radius: f32,
    pub strength: f32,
    pub mode: TerrainOpMode,
    /// Ties this op to the yield report it produces when it lands. Ops are
    /// QUEUED and applied after the script pass, so a dig cannot return what it
    /// removed — the edit has not happened yet. It returns this instead, and the
    /// measured report arrives through `terrain.yields()` (floptle/0037).
    pub id: u64,
}

impl TerrainOp {
    /// True if this op moves the SURFACE rather than only its colour.
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
/// "a queued edit tells you what it did".
pub(crate) struct TerrainReceipts {
    pub yields: Rc<RefCell<Vec<TerrainYield>>>,
    pub next_op_id: Rc<std::cell::Cell<u64>>,
}

/// What one applied op actually moved, in WORLD cubic units.
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

/// Per-op safety caps: a runaway loop must not freeze the frame. Radius is clamped;
/// ops past the per-frame cap are dropped with a warning (once per play).
const MAX_RADIUS: f32 = 64.0;
const MAX_OPS_PER_FRAME: usize = 64;

/// The streaming-related shared slots (`terrain.saveDir` / `warm` / `flush`) —
/// bundled so the install signature stays sane as the API grows.
pub(crate) struct TerrainStreamShared {
    pub save_dir: Rc<RefCell<Option<String>>>,
    pub warm: Rc<RefCell<Vec<String>>>,
    pub flush: Rc<RefCell<bool>>,
    /// Mirror of "the background terrain worker has something to do" — a field
    /// being generated, or one streaming in. Published so a game that creates
    /// worlds ON DEMAND can wait its turn.
    pub busy: Rc<std::cell::Cell<bool>>,
    /// Project root — `terrain.deleteSaveDir` resolves its (validated,
    /// relative) path against this.
    pub root: Rc<RefCell<std::path::PathBuf>>,
}

pub(crate) fn install_terrain_api(
    lua: &Lua,
    ops: Rc<RefCell<Vec<TerrainOp>>>,
    generates: Rc<RefCell<Vec<(u32, floptle_field::procgen::PlanetFill)>>>,
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
    stream: TerrainStreamShared,
    receipts: TerrainReceipts,
) {
    let TerrainStreamShared { save_dir, warm, flush, busy, root } = stream;
    let TerrainReceipts { yields, next_op_id } = receipts;
    let Ok(t) = lua.create_table() else { return };

    // terrain.deleteSaveDir(path) — delete a save slot's PERSISTED TERRAIN from
    // disk ("delete this save" UIs, paired with save.deleteSlot). Deliberately
    // narrow, not a generic rm -rf: the path must be relative with no "..",
    // must not be the ACTIVE saveDir (clear it first), and only terrain files
    // (.cfield/.tfield/.meta) inside that one directory are removed — the
    // directory itself (and an emptied parent) goes only once it's empty.
    // Returns the number of files removed.
    {
        let sd = save_dir.clone();
        let root = root.clone();
        if let Ok(f) = lua.create_function(move |_, path: String| {
            let bad = path.is_empty()
                || std::path::Path::new(&path).is_absolute()
                || path.split(['/', '\\']).any(|c| c == ".." || c.is_empty());
            if bad {
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
            let dir = root.borrow().join(&path);
            let Ok(entries) = std::fs::read_dir(&dir) else { return Ok(0) };
            let mut removed = 0u32;
            for entry in entries.flatten() {
                let p = entry.path();
                let terrain_file = p.extension().and_then(|x| x.to_str()).is_some_and(|x| {
                    matches!(x, "cfield" | "tfield" | "meta")
                });
                if terrain_file && std::fs::remove_file(&p).is_ok() {
                    removed += 1;
                }
            }
            // Tidy up: the dir if now empty, then ITS parent if that emptied too
            // (a game's saves/<slot>/terrain layout leaves saves/<slot> behind).
            if std::fs::remove_dir(&dir).is_ok()
                && let Some(parent) = dir.parent()
            {
                let _ = std::fs::remove_dir(parent);
            }
            Ok(removed)
        }) {
            let _ = t.set("deleteSaveDir", f);
        }
    }

    // terrain.flush() — write every EDITED resident field to the save slot NOW
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

    // terrain.warm(bodyName) — keep that body's terrain RESIDENT this frame
    // regardless of where the ship/player physically is: it loads if cold and
    // never streams out. Immediate mode (call it every frame while you care) —
    // the map calls it for its TAB-focused planet so focusing a far world
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

    // terrain.busy() — is the background terrain worker already occupied?
    //
    // For a game that GENERATES its world as the player travels. Terrain fills
    // and streams share one background budget, so a game that queues more of
    // them whenever it feels like it queues them behind the ground somebody is
    // standing on. Asking first is how on-demand generation stays smooth: build
    // one thing, wait until this goes quiet, build the next.
    //
    // True while any field is generating OR streaming in.
    {
        let b = busy.clone();
        if let Ok(f) = lua.create_function(move |_, ()| Ok(b.get())) {
            let _ = t.set("busy", f);
        }
    }

    // terrain.saveDir(path) / terrain.saveDir() — set (or read) the game's
    // SAVE-SLOT directory for player-edited terrain (relative to the project
    // root, e.g. "saves/slot1/terrain"). While set, the streaming system loads
    // a body's field from here FIRST (before the project file or its genspec)
    // and writes edited fields back here on evict/stop — so a player's digs
    // persist per save slot without ever touching the authored project data.
    // Pass "" to clear. G2 galaxy streaming (docs/subsystems/large-world-space.md).
    {
        let sd = save_dir.clone();
        if let Ok(f) = lua.create_function(move |_, path: Option<String>| {
            match path {
                Some(p) => {
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
        move |mut op: TerrainOp| -> u64 {
            op.id = ids.get().wrapping_add(1);
            ids.set(op.id);
            let id = op.id;
            let mut q = ops.borrow_mut();
            if q.len() >= MAX_OPS_PER_FRAME {
                if q.len() == MAX_OPS_PER_FRAME {
                    logs.borrow_mut().push(crate::ScriptLog {
                        level: crate::LogLevel::Warn,
                        msg: format!(
                            "terrain: more than {MAX_OPS_PER_FRAME} edits in one frame — extra ops dropped"
                        ),
                        source: None,
                    });
                    q.push(op); // sentinel push so the warning fires once
                }
                // A dropped op yields nothing; 0 is the "no receipt" id.
                return 0;
            }
            q.push(op);
            id
        }
    };

    // terrain.sculpt(x,y,z, radius, strength?, mode?) — mode: "raise" (default),
    // "lower"/"dig", "smooth", "flatten". Queued; lands this tick.
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, Option<f64>, Option<String>);
        if let Ok(f) = lua.create_function(move |_, (x, y, z, radius, strength, mode): Args| {
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
            }))
        }) {
            let _ = t.set("dig", f);
        }
    }

    // terrain.paint(x,y,z, radius, r,g,b, strength?) — recolor the surface.
    {
        let push = push.clone();
        type Args = (f64, f64, f64, f64, f64, f64, f64, Option<f64>);
        if let Ok(f) =
            lua.create_function(move |_, (x, y, z, radius, r, g, b, strength): Args| {
                push(TerrainOp {
                    pos: [x, y, z],
                    radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                    strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                    mode: TerrainOpMode::Paint([r as f32, g as f32, b as f32]),
                    id: 0,
                });
                Ok(())
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
            push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: 1.0,
                mode: TerrainOpMode::PaintTexture((slot.max(0.0) as u8).min(32)),
                id: 0,
            });
            Ok(())
        }) {
            let _ = t.set("paintTexture", f);
        }
    }

    // terrain.generatePlanet(id, opts) — REPLACE terrain volume `id`'s whole
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
        if let Ok(f) = lua.create_function(move |_, (id, opts): (u32, Option<Table>)| {
            let fill = planet_fill_from_table(opts.as_ref())?;
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
            // **Busy from this instant**, not from the next publish
            // (`floptle/0158`). The consumer is a game that queues a world and
            // then asks whether it may queue the next one — a caller told "idle"
            // about work it just asked for queues it twice, and the second one
            // goes in behind the ground a player is standing on. The editor's
            // per-frame publish owns the flag from the real job state after
            // this, and is what lowers it again.
            //
            // Only the QUEUEING calls raise it. `terrain.warm` deliberately does
            // not: it is immediate-mode, called every frame for as long as a
            // caller cares about a body, so raising it there would pin the flag
            // true for the whole time the map has a planet focused.
            busy2.set(true);
            Ok(())
        }) {
            let _ = t.set("generatePlanet", f);
        }
    }

    // terrain.yields() → the reports for edits that have LANDED since the last
    // call, drained. An op is queued and applied after the script pass, so the
    // report for a dab arrives on the following frame; the id `dig` returned
    // ties the two together (floptle/0037).
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
    {
        let cols = colliders.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let mut best: Option<f32> = None;
            for c in cols.borrow().iter() {
                let Some(t) = c.shape.chunk_terrain() else { continue };
                // Through the node's full frame (rotation + uniform scale), and
                // back to WORLD distance by the scale.
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
                // Cast the WORLD-down ray in the field's local frame (the node
                // may be rotated/scaled); reconstruct the hit's world Y through
                // the same frame.
                let s = t.scale.max(1e-6);
                let inv = t.rot.inverse();
                let down = (inv * Vec3::NEG_Y).normalize_or_zero();
                // Farthest content point from the anchor bounds the start height:
                // above THAT is above everything, no matter how the node is rotated.
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
/// [`floptle_field::procgen::PlanetFill`] — one parser for BOTH the immediate
/// generation queue and the on-node genspec (G2), so their vocabularies can
/// never drift apart. Every field optional; camelCase keys.
/// Every key a planet-generation options table reads (`floptle/0082`). The seven
/// paint layers all take `{ slot =, color = }`; `pockets`, `seam` and `iceCaps`
/// have their own inner keys, checked separately below.
pub(crate) const PLANET_KEYS: &[&str] = &[
    "seed", "radius", "voxel", "relief", "bumpFreq", "caveDepth", "coreR", "craters",
    "craterMin", "craterMax", "patchBias", "patchThr", "subsoilDepth", "strataDepth",
    "corePaint", "craterDust", "surfaceA", "surfaceB", "subsoil", "strata", "deep", "pockets",
    "seam", "iceCaps",
];

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
    // A planet is generated ONCE, on a background thread, and cached to disk.
    // A misspelled `relif` therefore produces a world that is wrong and STAYS
    // wrong across restarts, with nothing anywhere saying which key did it.
    const CALL: &str = "terrain.generatePlanet";
    let mut fill = PlanetFill::default();
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
        }
    }
    Ok(fill)
}

