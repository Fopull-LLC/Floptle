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

use crate::opts::check_keys;

/// A scatter source a script declared, plus the ids it has removed.
pub type Sources = Rc<RefCell<Vec<floptle_core::scatter::ScatterSource>>>;

/// Read `key` from an options table as a number, falling back to `d`.
fn num(t: &Table, key: &str, d: f64) -> f64 {
    t.get::<Option<f64>>(key).ok().flatten().unwrap_or(d)
}

fn dvec3(t: &Table, key: &str) -> Option<glam::DVec3> {
    t.get::<Value>(key).ok().as_ref().and_then(crate::math_api::vec3_of)
}

/// Every key `scatter.create` reads. Anything else is refused.
///
/// **Why refusing is worth a breaking change.** A `collide` option was parsed,
/// defaulted, stored — and read by nothing, for two releases. A game that asked
/// for solid props got no error, no warning and props it walked straight
/// through, and the only reason that could happen is that an unknown key was
/// silently dropped (`floptle/0066`). A typo'd `perchunk` had exactly the same
/// failure: the default, forever, with nothing to see.
const CREATE_KEYS: &[&str] = &[
    "asset", "lod", "range", "seed", "center", "radius", "halfX", "halfZ", "align", "perChunk",
    "chunk", "scaleMin", "scaleMax", "fade", "density", "densityRows", "parent",
];

/// Chunks resident at once, above which a source is reported rather than left
/// to be discovered as "the engine is slow" (`floptle/0071`).
///
/// Not a limit — a game may genuinely want a big field, and refusing one would
/// be the engine deciding a game's look. This is the number at which the reason
/// stops being obvious. A walkable body wants tens of chunks; the field that
/// prompted this had 4,489, and the two numbers that did it read as a look.
const LOUD_CHUNKS: u64 = 400;

pub(crate) fn install_scatter_api(
    lua: &Lua,
    sources: Sources,
    next_id: Rc<std::cell::Cell<u32>>,
    logs: Rc<RefCell<Vec<crate::ScriptLog>>>,
) {
    use floptle_core::scatter::{Align, Band, Region, ScatterSource};
    let Ok(t) = lua.create_table() else { return };

    // scatter.create{...} → a source id. Every knob has a default so the
    // shortest useful call is `scatter.create{ asset = "tree.glb" }`.
    {
        let s = sources.clone();
        let ids = next_id.clone();
        let lg = logs.clone();
        if let Ok(f) = lua.create_function(move |_, opts: Table| {
            check_keys(&opts, CREATE_KEYS, "scatter.create")?;
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
            // `density`: a rule evaluated ONCE, here, and kept as its answer.
            // A function is sampled over the region; a flat array is taken as
            // given. Either way nothing calls back into Lua while chunks build,
            // which is what keeps placement a pure function of the seed.
            let sphere = matches!(region, Region::Sphere { .. });
            let rows = num(&opts, "densityRows", 64.0).clamp(2.0, 512.0) as u32;
            let density = match opts.get::<Value>("density") {
                Ok(Value::Function(f)) => {
                    let cols = if sphere { rows * 2 } else { rows };
                    let mut data = Vec::with_capacity((rows * cols) as usize);
                    for r in 0..rows {
                        for c in 0..cols {
                            let u = c as f64 / (cols.max(2) - 1) as f64;
                            let v = r as f64 / (rows.max(2) - 1) as f64;
                            // The point the game is being asked about, in WORLD
                            // space — a climate model is written against places,
                            // not against grid indices.
                            let p = match region {
                                Region::Ground { center, half } => glam::DVec3::new(
                                    center.x + (u - 0.5) * 2.0 * half[0],
                                    center.y,
                                    center.z + (v - 0.5) * 2.0 * half[1],
                                ),
                                Region::Sphere { center, radius } => {
                                    let lon = (u - 0.5) * std::f64::consts::TAU;
                                    let lat = v * std::f64::consts::PI;
                                    center
                                        + glam::DVec3::new(
                                            lat.sin() * lon.cos(),
                                            lat.cos(),
                                            lat.sin() * lon.sin(),
                                        ) * radius
                                }
                            };
                            let d: f64 = f.call((p.x, p.y, p.z)).unwrap_or(1.0);
                            data.push(d.clamp(0.0, 1.0) as f32);
                        }
                    }
                    Some(floptle_core::scatter::Density { rows, data })
                }
                Ok(Value::Table(t)) => {
                    let data: Vec<f32> = (1..=t.raw_len())
                        .map(|i| t.raw_get::<f64>(i).unwrap_or(1.0).clamp(0.0, 1.0) as f32)
                        .collect();
                    // A grid handed over directly has to say how wide it is, or
                    // it is a list of numbers with no shape.
                    let cols = if sphere { rows * 2 } else { rows };
                    if data.len() < (rows * cols) as usize {
                        return Err(mlua::Error::RuntimeError(format!(
                            "scatter.create: `density` has {} values but densityRows = {rows} \
                             needs {} ({rows} x {cols}{})",
                            data.len(),
                            rows * cols,
                            if sphere { ", doubled for a sphere's longitude" } else { "" }
                        )));
                    }
                    Some(floptle_core::scatter::Density { rows, data })
                }
                _ => None,
            };
            let id = ids.get().wrapping_add(1).max(1);
            ids.set(id);
            let src = ScatterSource {
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
                density,
                removed: Default::default(),
                // The node this region rides (`floptle/0073`). Without it a
                // region is pinned to the world, and every prop on a planet
                // that orbits is left behind by its own planet within seconds.
                anchor: opts
                    .get::<Option<String>>("parent")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
                frame: Default::default(),
            };
            // Say what this costs, HERE, while the two numbers that decided it
            // are still on screen (`floptle/0071`). The alternative is what
            // happened: a day of "the engine is slow", and a clamp written in
            // Lua from reading engine source.
            let cost = floptle_core::scatter::cost(&src);
            if cost.chunks > LOUD_CHUNKS {
                let far = src.range();
                lg.borrow_mut().push(crate::ScriptLog {
                    level: crate::LogLevel::Warn,
                    msg: format!(
                        "scatter.create: source {id} is resident in {} chunks ({} props) every \
                         frame. That is set by the outermost `lod` distance ({far:.0}) against \
                         `chunk` ({:.0}) — cost grows with (lod/chunk)^2, so halving the \
                         distance or doubling the chunk quarters it.",
                        cost.chunks, cost.props, src.chunk
                    ),
                    source: None,
                });
            }
            s.borrow_mut().push(src);
            Ok(id)
        }) {
            let _ = t.set("create", f);
        }
    }

    // scatter.cost(id) → { chunks, props, far, chunkSize } — what this source
    // asks for every frame, before it is asked for (`floptle/0071`).
    //
    // The knobs read as a look. `lod`'s outermost distance is really the budget:
    // it sets how many chunks stay resident, as a sweep whose side grows with
    // it, walked every frame. A game that wants to tune within a budget can now
    // read the number instead of restating the engine's own arithmetic in Lua.
    {
        let s = sources.clone();
        if let Ok(f) = lua.create_function(move |lua, id: u32| {
            let out = lua.create_table()?;
            let v = s.borrow();
            let Some(src) = v.iter().find(|s| s.id == id) else { return Ok(out) };
            let cost = floptle_core::scatter::cost(src);
            out.set("chunks", cost.chunks as mlua::Integer)?;
            out.set("props", cost.props as mlua::Integer)?;
            out.set("far", src.range() as f64)?;
            out.set("chunkSize", src.chunk)?;
            out.set("perChunk", src.per_chunk as mlua::Integer)?;
            Ok(out)
        }) {
            let _ = t.set("cost", f);
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
            // The caller asks in WORLD space; the region lives in its anchor's
            // frame (`floptle/0073`). Convert once, query locally, and hand back
            // world positions — a game should never have to know the difference.
            let pl = src.frame.to_local(p);
            let mut found: Vec<(f64, floptle_core::scatter::Instance)> = Vec::new();
            for key in floptle_core::scatter::chunks_near(src, pl, r) {
                for mut i in floptle_core::scatter::chunk_instances(src, key) {
                    let d = (i.pos - pl).length();
                    if d <= r {
                        i.pos = src.frame.to_world(i.pos);
                        i.up = src.frame.dir_to_world(i.up);
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

#[cfg(test)]
mod tests {
    use super::*;

    type Logs = Rc<RefCell<Vec<crate::ScriptLog>>>;

    fn host() -> (Lua, Sources) {
        let (lua, sources, _) = host_with_logs();
        (lua, sources)
    }

    fn host_with_logs() -> (Lua, Sources, Logs) {
        let lua = Lua::new();
        let sources: Sources = Rc::new(RefCell::new(Vec::new()));
        let logs: Logs = Rc::new(RefCell::new(Vec::new()));
        install_scatter_api(
            &lua,
            sources.clone(),
            Rc::new(std::cell::Cell::new(0)),
            logs.clone(),
        );
        (lua, sources, logs)
    }

    /// A field whose cost is not obvious says so AT DECLARE TIME, while the two
    /// numbers that decided it are still on screen (`floptle/0071`).
    ///
    /// The configuration below is the one that shipped and froze a game. Its
    /// knobs read as a look; nothing in the API, the docs or the Console said
    /// that `lod`'s outermost distance was really the budget, and the fix was
    /// eventually written in Lua by reading engine source.
    #[test]
    fn a_field_that_costs_a_frame_says_so_when_it_is_declared() {
        let (lua, _, logs) = host_with_logs();
        lua.load(
            r#"scatter.create{ asset = "rock.glb", halfX = 5000, halfZ = 5000,
                               range = 700, chunk = 22, perChunk = 26 }"#,
        )
        .exec()
        .expect("a big field is allowed — it is reported, not refused");
        let said = logs.borrow();
        let warn = said
            .iter()
            .find(|l| l.level == crate::LogLevel::Warn)
            .unwrap_or_else(|| panic!("nothing was said about it: {:?}", said.len()));
        // It has to name the numbers. "Your scatter is expensive" sends someone
        // back to the same three knobs with no more information than before.
        for want in ["chunks", "props", "700", "22"] {
            assert!(warn.msg.contains(want), "the warning never mentions {want}: {}", warn.msg);
        }
    }

    /// …and an ordinary field says nothing at all. A warning every game trips
    /// over is a warning every game learns to scroll past.
    #[test]
    fn a_field_of_a_sane_size_is_not_warned_about() {
        let (lua, _, logs) = host_with_logs();
        lua.load(
            r#"scatter.create{ asset = "rock.glb", center = { x = 0, y = 0, z = 0 },
                               radius = 107, lod = { { asset = "rock.glb", distance = 190 } },
                               chunk = 34, perChunk = 14 }"#,
        )
        .exec()
        .unwrap();
        assert!(
            logs.borrow().iter().all(|l| l.level != crate::LogLevel::Warn),
            "warned about a field that is fine: {:?}",
            logs.borrow()
        );
    }

    /// A game can read the number instead of restating the engine's arithmetic
    /// in Lua — which is what the game that hit this had to do.
    #[test]
    fn a_script_can_ask_what_a_source_costs() {
        let (lua, _, _) = host_with_logs();
        let cost: Table = lua
            .load(
                r#"local id = scatter.create{ asset = "rock.glb", halfX = 5000, halfZ = 5000,
                                              range = 200, chunk = 20, perChunk = 10 }
                   return scatter.cost(id)"#,
            )
            .eval()
            .unwrap();
        let chunks: i64 = cost.get("chunks").unwrap();
        let props: i64 = cost.get("props").unwrap();
        assert!(chunks > 0, "no chunk count");
        assert_eq!(props, chunks * 10, "props is chunks x perChunk");
        assert_eq!(cost.get::<f64>("far").unwrap(), 200.0);
        assert_eq!(cost.get::<f64>("chunkSize").unwrap(), 20.0);
        // An id nobody declared is an empty table, not an error — a query is
        // not a place to blow up.
        let none: Table = lua.load("return scatter.cost(9999)").eval().unwrap();
        assert!(none.get::<Option<i64>>("chunks").unwrap().is_none());
    }

    /// The test that would have caught `collide`: every option the Lua surface
    /// accepts has to be one the engine reads.
    ///
    /// `CREATE_KEYS` is that list, and it is the same list the parser consults,
    /// so the two cannot drift. What this pins down is the BEHAVIOUR — an
    /// option outside it is refused rather than shrugged at, which is the only
    /// reason a dead option could hide for two releases.
    #[test]
    fn an_option_the_engine_does_not_read_is_refused() {
        let (lua, sources) = host();
        let err = lua
            .load(r#"return scatter.create{ asset = "tree.glb", collide = true }"#)
            .exec()
            .expect_err("a dead option must not be accepted");
        let msg = err.to_string();
        assert!(msg.contains("collide"), "it names the key: {msg}");
        assert!(sources.borrow().is_empty(), "and nothing was declared");

        // A typo had the same silent failure — the default, forever.
        let err = lua
            .load(r#"return scatter.create{ asset = "tree.glb", perchunk = 40 }"#)
            .exec()
            .expect_err("a typo must not be accepted");
        assert!(
            err.to_string().contains("perChunk"),
            "…and it suggests the real one: {err}"
        );
    }

    /// …and every key it DOES list still works, so the check cannot quietly
    /// become "refuse everything".
    #[test]
    fn every_option_the_list_names_is_accepted() {
        let (lua, sources) = host();
        let src = format!(
            "scatter.create{{ {} }}",
            CREATE_KEYS
                .iter()
                .map(|k| match *k {
                    "asset" => "asset = \"tree.glb\"".to_string(),
                    "lod" => "lod = {}".to_string(),
                    "align" => "align = \"world\"".to_string(),
                    "center" => "center = { x = 0, y = 0, z = 0 }".to_string(),
                    other => format!("{other} = 1"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        );
        lua.load(&src).exec().unwrap_or_else(|e| panic!("{src}\n{e}"));
        assert_eq!(sources.borrow().len(), 1, "the source was declared");
    }
}
