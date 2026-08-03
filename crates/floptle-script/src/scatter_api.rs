//! `scatter.*` — declare thousands of props instead of building them
//! (`floptle/0036`).
//!
//! The division of labour is the point. A game's generator keeps deciding
//! **what grows where** — it rolls the species, reads the climate, picks the
//! palette — and hands the engine a prototype and a rule. The engine decides
//! where each instance stands and draws them all, GPU-instanced, with no scene
//! node anywhere in it.
//!
//! That is the opposite of what a script could do before, which was
//! `createNode` per trunk segment. A plant was 4–14 nodes, so a "forest" was
//! ninety plants inside a moving bubble — and the engine growing its own plant
//! generator instead would have been both worse and less general.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// A scatter source a script declared, plus the ids it has removed.
pub type Sources = Rc<RefCell<Vec<floptle_core::scatter::ScatterSource>>>;

/// Read `key` from an options table as a number, falling back to `d`.
fn num(t: &Table, key: &str, d: f64) -> f64 {
    t.get::<Option<f64>>(key).ok().flatten().unwrap_or(d)
}

fn dvec3(t: &Table, key: &str) -> Option<glam::DVec3> {
    t.get::<Value>(key).ok().as_ref().and_then(crate::math_api::vec3_of)
}

pub(crate) fn install_scatter_api(lua: &Lua, sources: Sources, next_id: Rc<std::cell::Cell<u32>>) {
    use floptle_core::scatter::{Align, Band, Proxy, Region, ScatterSource};
    let Ok(t) = lua.create_table() else { return };

    // scatter.create{...} → a source id. Every knob has a default so the
    // shortest useful call is `scatter.create{ asset = "tree.glb" }`.
    {
        let s = sources.clone();
        let ids = next_id.clone();
        if let Ok(f) = lua.create_function(move |_, opts: Table| {
            // LOD bands: `lod = { {asset, distance}, ... }`, nearest first.
            // A single `asset` is the one-band shorthand.
            let mut bands: Vec<Band> = Vec::new();
            if let Ok(Some(list)) = opts.get::<Option<Table>>("lod") {
                for v in list.sequence_values::<Table>().flatten() {
                    let asset: String = v.get("asset").unwrap_or_default();
                    if asset.is_empty() {
                        continue;
                    }
                    bands.push(Band { asset, distance: num(&v, "distance", 100.0) as f32 });
                }
            }
            if bands.is_empty() {
                let asset: String = opts.get("asset").unwrap_or_default();
                if asset.is_empty() {
                    return Err(mlua::Error::RuntimeError(
                        "scatter.create{...} needs an `asset` (a mesh path) or a `lod` list"
                            .into(),
                    ));
                }
                bands.push(Band { asset, distance: num(&opts, "range", 120.0) as f32 });
            }
            // Bands must be sorted by distance or `band_at` picks the wrong
            // one — and a caller listing them far-to-near is an easy mistake
            // whose only symptom is the far mesh drawn up close.
            bands.sort_by(|a, b| a.distance.total_cmp(&b.distance));

            let region = match dvec3(&opts, "center") {
                Some(center) if opts.contains_key("radius").unwrap_or(false) => {
                    Region::Sphere { center, radius: num(&opts, "radius", 100.0) }
                }
                center => Region::Ground {
                    center: center.unwrap_or(glam::DVec3::ZERO),
                    half: [num(&opts, "halfX", 500.0), num(&opts, "halfZ", 500.0)],
                },
            };
            let align = match opts.get::<Option<String>>("align").ok().flatten().as_deref() {
                Some("world") | Some("up") => Align::World,
                _ => Align::Surface,
            };
            let collide = opts
                .get::<Option<Table>>("collide")
                .ok()
                .flatten()
                .map(|c| Proxy {
                    radius: num(&c, "radius", 0.4) as f32,
                    height: num(&c, "height", 2.0) as f32,
                })
                .or_else(|| {
                    matches!(opts.get::<Option<bool>>("collide"), Ok(Some(true)))
                        .then_some(Proxy { radius: 0.4, height: 2.0 })
                });
            let id = ids.get().wrapping_add(1).max(1);
            ids.set(id);
            s.borrow_mut().push(ScatterSource {
                id,
                seed: num(&opts, "seed", 1.0) as u64,
                region,
                per_chunk: num(&opts, "perChunk", 24.0).clamp(0.0, 4096.0) as u32,
                chunk: num(&opts, "chunk", 16.0).max(0.5),
                align,
                scale: (
                    num(&opts, "scaleMin", 0.85) as f32,
                    num(&opts, "scaleMax", 1.25) as f32,
                ),
                bands,
                fade: num(&opts, "fade", 8.0) as f32,
                collide,
                removed: Default::default(),
            });
            Ok(id)
        }) {
            let _ = t.set("create", f);
        }
    }

    // scatter.destroy(id) — the whole source goes.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let mut v = s.borrow_mut();
            let before = v.len();
            v.retain(|src| src.id != id);
            Ok(v.len() != before)
        }) {
            let _ = t.set("destroy", f);
        }
    }

    // scatter.remove(sourceId, instanceId) — ONE prop, permanently.
    //
    // By id rather than by position, which is what makes it survive a
    // stream-out and back in: an id is derived from (seed, chunk, index), and a
    // position is a float that came from a chain of arithmetic.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |_, (id, inst): (u32, mlua::Integer)| {
            let mut v = s.borrow_mut();
            let Some(src) = v.iter_mut().find(|s| s.id == id) else { return Ok(false) };
            Ok(src.removed.insert(inst as u64))
        }) {
            let _ = t.set("remove", f);
        }
    }

    // scatter.restore(sourceId [, instanceId]) — put one back, or all of them.
    // What "the forest regrows after fifteen minutes" is, without the game
    // having to remember what it cut.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |_, (id, inst): (u32, Option<mlua::Integer>)| {
            let mut v = s.borrow_mut();
            let Some(src) = v.iter_mut().find(|s| s.id == id) else { return Ok(0u32) };
            match inst {
                Some(i) => Ok(u32::from(src.removed.remove(&(i as u64)))),
                None => {
                    let n = src.removed.len() as u32;
                    src.removed.clear();
                    Ok(n)
                }
            }
        }) {
            let _ = t.set("restore", f);
        }
    }

    // scatter.removed(sourceId) → the ids this source has lost. A game that
    // wants permanence stores THIS (a handful of numbers), not every plant it
    // ever saw — `save.*` values are capped at about a kilobyte each, which is
    // what made "every plant you ever cut" unstorable in the first place.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let out = lua.create_table()?;
            if let Some(src) = s.borrow().iter().find(|s| s.id == id) {
                // Sorted, so the saved list is stable rather than reordering
                // itself every session and looking like a change.
                let mut ids: Vec<u64> = src.removed.iter().copied().collect();
                ids.sort_unstable();
                for (i, v) in ids.iter().enumerate() {
                    out.set(i + 1, *v as mlua::Integer)?;
                }
            }
            Ok(out)
        }) {
            let _ = t.set("removed", f);
        }
    }

    // scatter.near(sourceId, point, radius) → the instances around a point,
    // nearest first. What a harvest verb aims with, and what a "is there room
    // to build here" check reads.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let id = match a.first() {
                Some(Value::Integer(i)) => *i as u32,
                Some(Value::Number(n)) => *n as u32,
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "scatter.near(sourceId, point, radius)".into(),
                    ));
                }
            };
            let Some(p) = a.get(1).and_then(crate::math_api::vec3_of) else {
                return Err(mlua::Error::RuntimeError(
                    "scatter.near(sourceId, point, radius) — point is a vec3 (or a node)".into(),
                ));
            };
            let r = match a.get(2) {
                Some(Value::Number(n)) => *n,
                Some(Value::Integer(i)) => *i as f64,
                _ => 5.0,
            };
            let out = lua.create_table()?;
            let vols = s.borrow();
            let Some(src) = vols.iter().find(|s| s.id == id) else { return Ok(out) };
            let mut found: Vec<(f64, floptle_core::scatter::Instance)> = Vec::new();
            for key in floptle_core::scatter::chunks_near(src, p, r) {
                for i in floptle_core::scatter::chunk_instances(src, key) {
                    let d = (i.pos - p).length();
                    if d <= r {
                        found.push((d, i));
                    }
                }
            }
            found.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (n, (d, i)) in found.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("id", i.id as mlua::Integer)?;
                row.set("distance", *d)?;
                row.set("pos", crate::math_api::LuaVec3(i.pos))?;
                row.set("scale", i.scale as f64)?;
                row.set("param", i.param as f64)?;
                out.set(n + 1, row)?;
            }
            Ok(out)
        }) {
            let _ = t.set("near", f);
        }
    }

    let _ = lua.globals().set("scatter", t);
}
