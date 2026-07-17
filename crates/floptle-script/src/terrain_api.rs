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
use mlua::Lua;

/// One queued terrain write, in WORLD coordinates (scripts speak world; the editor
/// converts into each terrain's local frame when applying).
#[derive(Clone, Debug)]
pub struct TerrainOp {
    pub pos: [f64; 3],
    pub radius: f32,
    pub strength: f32,
    pub mode: TerrainOpMode,
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

pub(crate) fn install_terrain_api(
    lua: &Lua,
    ops: Rc<RefCell<Vec<TerrainOp>>>,
    colliders: Rc<RefCell<Vec<floptle_physics::AnchoredCollider>>>,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
) {
    let Ok(t) = lua.create_table() else { return };

    // Shared push-with-cap helper.
    let push = {
        let ops = ops.clone();
        let logs = logs.clone();
        move |op: TerrainOp| {
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
                return;
            }
            q.push(op);
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
            push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                mode,
            });
            Ok(())
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
            push(TerrainOp {
                pos: [x, y, z],
                radius: (radius as f32).clamp(0.05, MAX_RADIUS),
                strength: strength.unwrap_or(1.0).clamp(0.0, 1.0) as f32,
                mode: TerrainOpMode::Lower,
            });
            Ok(())
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
            });
            Ok(())
        }) {
            let _ = t.set("paintTexture", f);
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
