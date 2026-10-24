//! The Lua `perf` table: a game reading its own frame cost.
//!
//! "The engine is slow" is the report a game makes when it cannot read its
//! own numbers, and it is usually a number the game could have read itself: a
//! scatter field asking for 117,000 props, a `findScript` in a loop. Readable
//! from Lua, a project asserts its own budget in a smoke test and finds out
//! from CI rather than from a player.
//!
//! ```lua
//! function start(node)
//!   perf.enable(true)
//! end
//!
//! function update(node, dt)
//!   -- The number worth watching is the worst recent frame, not the mean: a 40 ms
//!   -- hitch once a second is under a millisecond of average.
//!   if perf.worstMs("scripts") > 6 then
//!     log("slow pass — " .. perf.slowestScript())
//!   end
//! end
//! ```
//!
//! Reading while it is off is an error, not a zero. Collection costs nothing
//! when nothing is looking, which makes "off" and "free" the same shape, and a
//! smoke test asserting `perf.ms("scripts") < 4` would pass by measuring
//! nothing. So every getter raises while collection is off and says to call
//! `perf.enable(true)`.

use std::rc::Rc;

use mlua::{Lua, Table};

use floptle_core::profile::{Bucket, Counts, EngineCall, Sample};

use crate::sampler::Sampler;
use crate::SharedProfile;

/// The sampler's interval when a script does not name one: four samples a
/// millisecond of script time.
const DEFAULT_SAMPLE_MS: f32 = 0.25;

/// Install the `perf` global.
pub fn install(lua: &Lua, profile: &SharedProfile, sampler: &Rc<Sampler>) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // perf.enable(on) — start or stop collecting. Off is the default, and off is
    // free. Stopping clears the history: a stale mean from before a fix looks
    // exactly like a fix that did not work.
    let p = profile.clone();
    let smp = sampler.clone();
    t.set(
        "enable",
        lua.create_function(move |_, on: Option<bool>| {
            let on = on.unwrap_or(true);
            p.borrow_mut().enable(on);
            if !on {
                smp.set(None);
            }
            Ok(())
        })?,
    )?;

    // perf.frames(n) -> the last n frames (at most 300), oldest first, each
    // exactly as it was: no smoothing, so a probe needs no `update` of its own
    // in every frame to read them.
    let p = profile.clone();
    t.set(
        "frames",
        lua.create_function(move |lua, n: Option<usize>| {
            let prof = p.borrow();
            require_on(&prof, "frames")?;
            let n = n.unwrap_or(floptle_core::profile::FRAME_RING);
            let list = lua.create_table()?;
            for (i, r) in prof.recent_frames(n).enumerate() {
                let row = lua.create_table()?;
                row.set("frame", r.frame)?;
                row.set("ms", r.ms)?;
                let buckets = lua.create_table()?;
                for (b, ms) in &r.buckets {
                    buckets.set(b.name(), *ms)?;
                }
                row.set("buckets", buckets)?;
                row.set("parts", parts_table(lua, &r.parts)?)?;
                row.set("scripts", named_ms(lua, &r.scripts)?)?;
                row.set("calls", calls_table(lua, &r.calls)?)?;
                row.set("samples", samples_table(lua, &r.samples)?)?;
                row.set("bodies", named_ms(lua, &r.bodies)?)?;
                row.set("counts", counts_table(lua, &r.counts)?)?;
                if let Some(why) = r.paced {
                    row.set("paced", why)?;
                }
                list.raw_set(i + 1, row)?;
            }
            Ok(list)
        })?,
    )?;

    // perf.hitchThreshold([ms]) -> the frame time a hitch starts at, after
    // setting it when given: 50 by default, 33 to catch what makes a 30 fps
    // game miss, 20 for 60.
    let p = profile.clone();
    t.set(
        "hitchThreshold",
        lua.create_function(move |_, ms: Option<f32>| {
            if let Some(ms) = ms {
                p.borrow_mut().set_hitch_threshold(ms);
            }
            Ok(p.borrow().hitch_threshold())
        })?,
    )?;

    // perf.sampleScripts(on [, intervalMs]) — the script sampler: which line of
    // which script the time went to, kept with every frame and every hitch.
    let p = profile.clone();
    let smp = sampler.clone();
    t.set(
        "sampleScripts",
        lua.create_function(move |_, (on, interval): (Option<bool>, Option<f32>)| {
            require_on(&p.borrow(), "sampleScripts")?;
            let on = on.unwrap_or(true);
            smp.set(on.then(|| interval.unwrap_or(DEFAULT_SAMPLE_MS)));
            Ok(smp.on())
        })?,
    )?;

    // perf.samples() -> every sampled line since the last call, hottest first:
    // { {script=, fn=, line=, n=, ms=}, … }.
    let p = profile.clone();
    t.set(
        "samples",
        lua.create_function(move |lua, ()| {
            require_on(&p.borrow(), "samples")?;
            let all = p.borrow_mut().take_samples();
            samples_table(lua, &all)
        })?,
    )?;

    // perf.bodies(n) -> the costliest physics assemblies and bodies last
    // frame, by node name: { {name=, ms=}, … }.
    let p = profile.clone();
    t.set(
        "bodies",
        lua.create_function(move |lua, n: Option<usize>| {
            let prof = p.borrow();
            require_on(&prof, "bodies")?;
            let rows = prof.last_bodies();
            named_ms(lua, &rows[..rows.len().min(n.unwrap_or(rows.len()))])
        })?,
    )?;

    // perf.pacing() -> how frames reach the display and what held the last
    // one, so a frame spent waiting on vsync or a compositor does not read
    // as the game being slow.
    let p = profile.clone();
    t.set(
        "pacing",
        lua.create_function(move |lua, ()| {
            let prof = p.borrow();
            require_on(&prof, "pacing")?;
            let pc = prof.pacing();
            let out = lua.create_table()?;
            out.set("presentMode", if pc.setup.present_mode.is_empty() { "none" } else { pc.setup.present_mode })?;
            out.set("vsync", pc.setup.vsync)?;
            out.set("refreshHz", pc.setup.refresh_hz)?;
            out.set("frameCap", pc.setup.frame_cap)?;
            out.set("frameMs", pc.frame_ms)?;
            out.set("waitMs", pc.wait_ms)?;
            if let Some(g) = pc.gpu_ms {
                out.set("gpuMs", g)?;
            }
            if let Some(why) = pc.limited_by {
                out.set("limitedBy", why)?;
            }
            out.set("pacedFrames", pc.paced_frames)?;
            out.set("frames", pc.frames)?;
            Ok(out)
        })?,
    )?;

    // perf.enabled() -> bool
    let p = profile.clone();
    t.set("enabled", lua.create_function(move |_, ()| Ok(p.borrow().enabled()))?)?;

    // perf.gpu() -> { total = ms, passes = { {name=, ms=}, ... } } for the last
    // frame the GPU finished timing, or nil when nothing has been timed (a
    // device without timestamp queries, or the first frames after enabling).
    // What a quality governor wants: the GPU's own answer, not a CPU guess.
    let p = profile.clone();
    t.set(
        "gpu",
        lua.create_function(move |lua, ()| {
            let prof = p.borrow();
            require_on(&prof, "gpu")?;
            let Some((passes, total)) = prof.gpu() else { return Ok(mlua::Value::Nil) };
            let out = lua.create_table()?;
            out.set("total", *total as f64)?;
            let list = lua.create_table_with_capacity(passes.len(), 0)?;
            for (i, (name, ms)) in passes.iter().enumerate() {
                let row = lua.create_table()?;
                row.set("name", name.as_str())?;
                row.set("ms", *ms as f64)?;
                list.raw_set(i + 1, row)?;
            }
            out.set("passes", list)?;
            Ok(mlua::Value::Table(out))
        })?,
    )?;

    // perf.gpuMs(pass) -> one pass's GPU time last frame ("opaque + lighting",
    // "post", …, or "total"), nil when it was not timed or did not run.
    let p = profile.clone();
    t.set(
        "gpuMs",
        lua.create_function(move |_, name: String| {
            let prof = p.borrow();
            require_on(&prof, "gpuMs")?;
            let Some((passes, total)) = prof.gpu() else { return Ok(None) };
            if name == "total" {
                return Ok(Some(*total as f64));
            }
            Ok(passes.iter().filter(|(n, _)| *n == name).map(|(_, ms)| *ms as f64).reduce(|a, b| a + b))
        })?,
    )?;

    // perf.ms(bucket) -> the rolling average, in milliseconds.
    let p = profile.clone();
    t.set(
        "ms",
        lua.create_function(move |_, name: String| {
            Ok(cost(&p, &name, "ms")?.ms as f64)
        })?,
    )?;

    // perf.parts() -> { name = ms, … } for the last finished frame: the named
    // pieces of work inside the buckets (`scatterSettle`, `terrainEdits`, …),
    // every frame rather than only in a hitch.
    let p = profile.clone();
    t.set(
        "parts",
        lua.create_function(move |lua, ()| {
            require_on(&p.borrow(), "parts")?;
            let out = lua.create_table()?;
            for (name, ms) in p.borrow().last_parts() {
                out.set(*name, *ms as f64)?;
            }
            Ok(out)
        })?,
    )?;

    // perf.worstMs(bucket) -> the worst single frame in the last second.
    //
    // The one to reach for. A spike is what anybody is ever chasing and a mean
    // hides it, so this is as easy to call as `ms`.
    let p = profile.clone();
    t.set(
        "worstMs",
        lua.create_function(move |_, name: String| {
            Ok(cost(&p, &name, "worstMs")?.worst_ms as f64)
        })?,
    )?;

    // perf.scriptMs(kind) / perf.scriptWorstMs(kind) — one script's own cost, by
    // file name. "Which of my scripts is doing this" is the question; a total for
    // "scripts" does not answer it.
    let p = profile.clone();
    t.set(
        "scriptMs",
        lua.create_function(move |_, kind: String| {
            let prof = p.borrow();
            require_on(&prof, "scriptMs")?;
            Ok(prof.script(&kind).map(|c| c.ms as f64).unwrap_or(0.0))
        })?,
    )?;
    let p = profile.clone();
    t.set(
        "scriptWorstMs",
        lua.create_function(move |_, kind: String| {
            let prof = p.borrow();
            require_on(&prof, "scriptWorstMs")?;
            Ok(prof.script(&kind).map(|c| c.worst_ms as f64).unwrap_or(0.0))
        })?,
    )?;

    // perf.scripts() -> { {name=, ms=, worstMs=}, … }, most expensive first.
    let p = profile.clone();
    t.set(
        "scripts",
        lua.create_function(move |lua, ()| {
            let prof = p.borrow();
            require_on(&prof, "scripts")?;
            let out = lua.create_table()?;
            let calls = prof.last_calls();
            for (i, (name, c)) in prof.scripts().into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("ms", c.ms as f64)?;
                row.set("worstMs", c.worst_ms as f64)?;
                // Last frame's engine calls made from this script, costliest
                // first, and their total: the part of its time that was the
                // engine working on its behalf.
                let mine: Vec<EngineCall> = calls.iter().filter(|c| c.script == name).cloned().collect();
                row.set("engineMs", mine.iter().map(|c| c.ms).sum::<f32>())?;
                row.set("calls", calls_table(lua, &mine)?)?;
                row.set("name", name)?;
                out.raw_set(i + 1, row)?;
            }
            Ok(out)
        })?,
    )?;

    // perf.slowestScript() -> the name, or nil if nothing has run yet.
    //
    // The one-liner a game actually writes in an assertion message, so it exists
    // rather than being three lines of sorting in every project.
    let p = profile.clone();
    t.set(
        "slowestScript",
        lua.create_function(move |_, ()| {
            let prof = p.borrow();
            require_on(&prof, "slowestScript")?;
            Ok(prof.scripts().into_iter().next().map(|(n, _)| n))
        })?,
    )?;

    // perf.counts() -> { nodes=, culled=, instances=, draws=, unbatched=,
    // unbatchedBy=, chunks=, props=, particles= }
    //
    // Free to keep, and three of the four misdiagnosed tickets were answerable
    // from one of these alone — `0071` was a report of 117,000 props with no way
    // to see the number.
    let p = profile.clone();
    t.set(
        "counts",
        lua.create_function(move |lua, ()| {
            let c = p.borrow().counts();
            let out = counts_table(lua, &c)?;
            // The script mirror, last frame: whole rebuilds (O(scene) each)
            // and single nodes re-read because a component of theirs changed.
            // Rebuilds climbing every frame is something structural changing
            // every frame, and `perf.mirrorCause()` names it.
            let m = p.borrow().mirror_work();
            out.set("mirrorRebuilds", m.rebuilds)?;
            out.set("mirrorRefreshed", m.refreshed)?;
            // Animators last frame, and how many culling skipped (hidden, off
            // screen, switched off, or between a reduced-rate animator's frames).
            let (animators, culled) = p.borrow().anim_counts();
            out.set("animators", animators)?;
            out.set("animatorsCulled", culled)?;
            // Rays scripts cast last frame (raycast + raycastMany) and what
            // they took, which is part of `scripts`, not beside it.
            let (rays, ray_ms) = p.borrow().rays();
            out.set("rays", rays)?;
            out.set("rayMs", ray_ms)?;
            // Sweeps the same way (spherecast, capsulecast, boxcast): a chase
            // camera keeping clear of a craft casts a dozen a frame.
            let (sweeps, sweep_ms) = p.borrow().sweeps();
            out.set("sweeps", sweeps)?;
            out.set("sweepMs", sweep_ms)?;
            // Decals in the world (`decals.add`) and the triangles they were
            // laid as: one batch per picture, so this is geometry, not draws.
            let (decals, decal_tris) = p.borrow().decal_counts();
            out.set("decals", decals)?;
            out.set("decalTris", decal_tris)?;
            // Terrain edits refused because the queue was full, since the
            // start: zero for a game that paces with `terrain.capacity()`.
            out.set("terrainDropped", crate::terrain_api::dropped_count(lua))?;
            Ok(out)
        })?,
    )?;

    // perf.hitches() -> every frame since the last call whose buckets added up
    // to the hitch threshold (50 ms unless `perf.hitchThreshold` says otherwise)
    // or more: { {frame=, ms=, top=, buckets={terrain=157.4, ...}}, ... },
    // oldest first. The window behind `worstMs` is sixty frames, so a probe
    // that reads every few seconds had already lost the spike it was after.
    let p = profile.clone();
    t.set(
        "hitches",
        lua.create_function(move |lua, ()| {
            require_on(&p.borrow(), "hitches")?;
            let list = lua.create_table()?;
            for (i, h) in p.borrow_mut().take_hitches().into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("frame", h.frame)?;
                row.set("ms", h.ms)?;
                if let Some((b, _)) = h.buckets.first() {
                    row.set("top", b.name())?;
                }
                let buckets = lua.create_table()?;
                for (b, ms) in &h.buckets {
                    buckets.set(b.name(), *ms)?;
                }
                row.set("buckets", buckets)?;
                let parts = lua.create_table()?;
                for (name, ms) in &h.parts {
                    parts.set(*name, *ms)?;
                }
                row.set("parts", parts)?;
                // The files the frame was waiting on, slowest first:
                // { {path=, part=, ms=}, … }.
                let assets = lua.create_table()?;
                for (j, (path, part, ms)) in h.assets.iter().enumerate() {
                    let a = lua.create_table()?;
                    a.set("path", path.as_str())?;
                    a.set("part", *part)?;
                    a.set("ms", *ms)?;
                    assets.raw_set(j + 1, a)?;
                }
                row.set("assets", assets)?;
                row.set("scripts", named_ms(lua, &h.scripts)?)?;
                row.set("calls", calls_table(lua, &h.calls)?)?;
                row.set("samples", samples_table(lua, &h.samples)?)?;
                row.set("bodies", named_ms(lua, &h.bodies)?)?;
                list.raw_set(i + 1, row)?;
            }
            Ok(list)
        })?,
    )?;

    // perf.overloaded() -> true while the fixed tick costs more than the time
    // it simulates: this machine can't run the game in real time, and (unless
    // the project says CatchUp) it is playing in slow motion. For a loading
    // screen that would otherwise wait forever for calm frames.
    let p = profile.clone();
    t.set("overloaded", lua.create_function(move |_, ()| Ok(p.borrow().overloaded()))?)?;

    // perf.mirrorCause() -> why the script mirror last rebuilt from scratch,
    // naming the node and what changed on it; nil if it never has.
    //
    // A rebuild costs time proportional to the whole scene and shows up in the
    // `mirror` bucket, not under any script — this is how to find who caused it.
    let p = profile.clone();
    t.set(
        "mirrorCause",
        lua.create_function(move |_, ()| Ok(p.borrow().mirror_cause().map(str::to_owned)))?,
    )?;

    // perf.accountedMs() -> the buckets added up.
    //
    // Named "accounted", not "total": vsync, the OS and the GPU finishing are all
    // outside every bucket, and a number that claimed to be the frame time
    // without being it would be worse than not offering one.
    let p = profile.clone();
    t.set(
        "accountedMs",
        lua.create_function(move |_, ()| {
            let prof = p.borrow();
            require_on(&prof, "accountedMs")?;
            Ok(prof.accounted_ms().unwrap_or(0.0) as f64)
        })?,
    )?;

    // perf.buckets() -> the names, so a script can iterate them without a list of
    // its own that could go stale.
    let names = lua.create_table()?;
    for (i, b) in Bucket::ALL.into_iter().enumerate() {
        names.raw_set(i + 1, b.name())?;
    }
    t.set("buckets", lua.create_function(move |_, ()| Ok(names.clone()))?)?;

    lua.globals().set("perf", t)
}

/// The counts a frame produced, as `perf.counts()` and each `perf.frames`
/// record give them.
fn counts_table(lua: &Lua, c: &Counts) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    out.set("nodes", c.nodes)?;
    out.set("culled", c.culled)?;
    out.set("instances", c.instances)?;
    out.set("draws", c.draws)?;
    // Draws that carried one instance, and why each did not join a
    // batch: a `draws` far above the meshes on screen is the symptom,
    // and this table names the cause.
    out.set("unbatched", c.unbatched.total())?;
    let by = lua.create_table()?;
    for (why, n) in c.unbatched.reasons() {
        by.set(why, n)?;
    }
    out.set("unbatchedBy", by)?;
    out.set("chunks", c.chunks)?;
    out.set("props", c.props)?;
    out.set("particles", c.particles)?;
    // The capped resources, and what each cap actually cut. A ceiling a
    // game cannot see is one it discovers as "my seventeenth torch does
    // nothing", so the pair is the
    // point: one number for the cost, one for what was refused.
    out.set("effects", c.effects)?;
    out.set("effectsDropped", c.effects_dropped)?;
    out.set("lights", c.lights)?;
    out.set("lightsDropped", c.lights_dropped)?;
    out.set("voices", c.voices)?;
    // What 2D lighting costs this frame: flat surfaces rasterized a
    // second time into its G-buffer. Zero when no light
    // can reach them, which is the answer a 2D game most wants to be
    // able to check.
    out.set("flat2d", c.flat2d)?;
    // What the shadow march tests along each ray: collider proxies
    // (32 at most, the nearest) and baked volumes. A room built from
    // many boxes costs lighting time per box, and this is the number
    // that says so.
    out.set("shadowProxies", c.shadow_proxies)?;
    out.set("shadowProxiesDropped", c.shadow_proxies_dropped)?;
    out.set("shadowVolumes", c.shadow_volumes)?;
    Ok(out)
}

/// `{ name = ms, … }`.
fn parts_table(lua: &Lua, parts: &[(&'static str, f32)]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (name, ms) in parts {
        t.set(*name, *ms)?;
    }
    Ok(t)
}

/// `{ {name=, ms=}, … }`, in the order given.
fn named_ms(lua: &Lua, rows: &[(String, f32)]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(rows.len(), 0)?;
    for (i, (name, ms)) in rows.iter().enumerate() {
        let r = lua.create_table()?;
        r.set("name", name.as_str())?;
        r.set("ms", *ms)?;
        t.raw_set(i + 1, r)?;
    }
    Ok(t)
}

/// `{ {script=, call=, n=, ms=}, … }`, in the order given.
fn calls_table(lua: &Lua, calls: &[EngineCall]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(calls.len(), 0)?;
    for (i, c) in calls.iter().enumerate() {
        let r = lua.create_table()?;
        r.set("script", c.script.as_str())?;
        r.set("call", c.call)?;
        r.set("n", c.n)?;
        r.set("ms", c.ms)?;
        t.raw_set(i + 1, r)?;
    }
    Ok(t)
}

/// `{ {script=, fn=, line=, n=, ms=}, … }`, in the order given.
fn samples_table(lua: &Lua, samples: &[Sample]) -> mlua::Result<Table> {
    let t = lua.create_table_with_capacity(samples.len(), 0)?;
    for (i, x) in samples.iter().enumerate() {
        let r = lua.create_table()?;
        r.set("script", x.script.as_str())?;
        r.set("fn", x.func.as_str())?;
        r.set("line", x.line)?;
        r.set("n", x.n)?;
        r.set("ms", x.ms)?;
        t.raw_set(i + 1, r)?;
    }
    Ok(t)
}

/// Resolve a bucket name and read its cost, refusing clearly on both failures.
fn cost(
    p: &SharedProfile,
    name: &str,
    call: &str,
) -> mlua::Result<floptle_core::profile::Cost> {
    let prof = p.borrow();
    require_on(&prof, call)?;
    // An unrecognised bucket names the whole set rather than answering zero — the
    // property, the value, and what is accepted.
    let Some(bucket) = Bucket::from_name(name) else {
        let all: Vec<&str> = Bucket::ALL.iter().map(|b| b.name()).collect();
        return Err(mlua::Error::runtime(format!(
            "perf.{call}: \"{name}\" is not a bucket — accepted: {}",
            all.join(", ")
        )));
    };
    // `None` here means enabled but no frame has completed yet, which is a real
    // answer and not an error: ask again next frame.
    Ok(prof.bucket(bucket).unwrap_or_default())
}

/// Refuse to answer while collection is off.
fn require_on(
    prof: &floptle_core::profile::FrameProfile,
    call: &str,
) -> mlua::Result<()> {
    if prof.enabled() {
        return Ok(());
    }
    Err(mlua::Error::runtime(format!(
        "perf.{call}: nothing is being measured — call perf.enable(true) first \
         (collection is off by default because a profiler that costs a frame gets \
         turned off). Returning 0 here would make a budget assertion pass on no data."
    )))
}

/// The calls that answer while collection is off, because they are meaningful
/// then: the switch itself, whether it is on, the bucket names, and the counts
/// (which are free to keep and obviously not a measurement when zero).
///
/// Everything else refuses, and the test below enumerates the real `perf` table
/// against this list — so a getter added later cannot quietly default to
/// answering zero without somebody deciding it should.
#[cfg(test)]
const READABLE_WHILE_OFF: &[&str] =
    &["enable", "enabled", "buckets", "counts", "mirrorCause", "overloaded", "hitchThreshold"];

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::profile::{Counts, FrameProfile};

    fn host() -> (Lua, SharedProfile) {
        let lua = Lua::new();
        let p: SharedProfile = std::rc::Rc::new(std::cell::RefCell::new(FrameProfile::default()));
        let sampler = Sampler::new(p.clone());
        let budget = crate::budget::Budget::new();
        crate::sampler::install(&lua, &budget, &sampler);
        install(&lua, &p, &sampler).expect("install perf");
        (lua, p)
    }

    /// Reading a time while collection is off raises, and the message says how to
    /// turn it on.
    ///
    /// This is the whole design decision. A zero would let
    /// `assert(perf.ms("scripts") < 4)` pass in a smoke test that measured
    /// nothing, which is the exact shape of bug this engine has shipped 32 times.
    /// **The GPU's own pass times reach a script.** Nil until a frame
    /// has been timed (a device without timestamp queries never is), then each
    /// pass by name and the total, as the renderer published them.
    #[test]
    fn gpu_pass_times_read_back_by_name_and_nil_until_measured() {
        let (lua, p) = host();
        p.borrow_mut().enable(true);
        assert!(lua.load("return perf.gpu() == nil and perf.gpuMs('post') == nil").eval::<bool>().unwrap());
        p.borrow_mut().set_gpu(vec![("opaque + lighting".into(), 7.25), ("post".into(), 1.5)], 9.0);
        let (opaque, total, n, missing): (f64, f64, i64, bool) = lua
            .load("local g = perf.gpu() return perf.gpuMs('opaque + lighting'), perf.gpuMs('total'), #g.passes, perf.gpuMs('glass') == nil")
            .eval()
            .unwrap();
        assert_eq!((opaque, total, n, missing), (7.25, 9.0, 2, true));
    }

    #[test]
    fn reading_a_time_while_off_refuses_and_says_how_to_turn_it_on() {
        let (lua, _p) = host();
        let err = lua
            .load("return perf.ms('scripts')")
            .eval::<f64>()
            .expect_err("must not answer");
        let msg = err.to_string();
        assert!(msg.contains("perf.enable(true)"), "no remedy in: {msg}");
        assert!(msg.contains("on no data"), "does not say why not zero: {msg}");
        // every getter refuses, enumerated from the `perf` table itself rather
        // than a hand-kept list — so a call added later cannot quietly default
        // to answering zero, which is the one thing this whole design is about.
        let names: Vec<String> = lua
            .load("local n = {} for k in pairs(perf) do n[#n+1] = k end return n")
            .eval()
            .unwrap();
        let mut checked = 0;
        for name in &names {
            if READABLE_WHILE_OFF.contains(&name.as_str()) {
                continue;
            }
            let call = if name.starts_with("script") && name != "scripts" {
                format!("perf.{name}('x')")
            } else if name == "ms" || name == "worstMs" {
                format!("perf.{name}('render')")
            } else {
                format!("perf.{name}()")
            };
            assert!(
                lua.load(format!("return {call}")).eval::<mlua::Value>().is_err(),
                "{call} answered while collection was off — a budget assertion \
                 would pass on no data. Either make it refuse, or add it to \
                 READABLE_WHILE_OFF with a reason."
            );
            checked += 1;
        }
        assert!(checked >= 6, "only {checked} getters were checked, expected the whole table");
        // …and the ones that are meaningful while off still work, so a script can
        // ask whether it is on without handling an error.
        assert!(!lua.load("return perf.enabled()").eval::<bool>().unwrap());
        assert!(lua.load("return #perf.buckets()").eval::<i64>().unwrap() > 0);
        assert!(lua.load("return perf.counts().nodes").eval::<i64>().is_ok());
    }

    /// A misspelled bucket names every accepted value.
    #[test]
    fn an_unknown_bucket_lists_what_is_accepted() {
        let (lua, p) = host();
        p.borrow_mut().enable(true);
        let err = lua.load("return perf.ms('scripting')").eval::<f64>().unwrap_err().to_string();
        assert!(err.contains("scripting"), "does not quote the value: {err}");
        assert!(err.contains("accepted"), "no accepted list: {err}");
        assert!(err.contains("scripts"), "does not name the real bucket: {err}");
    }

    /// Enabled, the numbers come through — per bucket, per script, and sorted.
    #[test]
    fn a_script_can_read_its_own_cost_by_name() {
        let (lua, p) = host();
        lua.load("perf.enable(true)").exec().unwrap();
        {
            let mut prof = p.borrow_mut();
            // A pass records the bucket; the hooks inside it record themselves.
            // Two calls since 0.84.2 — `Scripts` is the whole pass and the rows
            // are the part of it that was inside a hook, so 8.0 is
            // more than the 5.25 the rows come to.
            prof.record(Bucket::Scripts, 8.0);
            prof.record_script("vessel_controller", 5.0);
            prof.record_script("pulsate", 0.25);
            prof.record(Bucket::Mirror, 1.5);
            prof.record(Bucket::Render, 3.0);
            prof.set_counts(Counts { nodes: 5500, culled: 2700, props: 117_000, ..Default::default() });
            prof.end_frame();
        }
        assert!(lua.load("return perf.enabled()").eval::<bool>().unwrap());
        assert_eq!(
            lua.load("return perf.slowestScript()").eval::<String>().unwrap(),
            "vessel_controller"
        );
        assert!(lua.load("return perf.scriptWorstMs('vessel_controller')").eval::<f64>().unwrap() > 4.0);
        assert!(lua.load("return perf.worstMs('render')").eval::<f64>().unwrap() > 2.0);
        // The scripts bucket is the whole pass — more than the rows under it,
        // which are hook time. A reader that saw 5.25 here would be reading the
        // rows added up, which is what this stopped being.
        let scripts = lua.load("return perf.worstMs('scripts')").eval::<f64>().unwrap();
        assert!((scripts - 8.0).abs() < 1e-3, "scripts bucket reads {scripts}, not the pass");
        // …and the mirror is its own line, reachable by name like any other.
        assert!((lua.load("return perf.worstMs('mirror')").eval::<f64>().unwrap() - 1.5).abs() < 1e-3);
        // Rows are most-expensive-first, which is the order the question is asked.
        assert_eq!(
            lua.load("return perf.scripts()[1].name").eval::<String>().unwrap(),
            "vessel_controller"
        );
        // The counts that would have diagnosed 0071 and 0075 in one line each.
        assert_eq!(lua.load("return perf.counts().props").eval::<i64>().unwrap(), 117_000);
        assert_eq!(lua.load("return perf.counts().culled").eval::<i64>().unwrap(), 2700);
    }

    /// A script nobody has run reads zero rather than raising — it genuinely cost
    /// nothing, which is different from nothing having been measured.
    #[test]
    fn a_script_that_never_ran_costs_zero_rather_than_erroring() {
        let (lua, p) = host();
        p.borrow_mut().enable(true);
        p.borrow_mut().end_frame();
        assert_eq!(lua.load("return perf.scriptMs('never_written')").eval::<f64>().unwrap(), 0.0);
        assert_eq!(lua.load("return perf.slowestScript()").eval::<Option<String>>().unwrap(), None);
    }

    /// The bucket list a script iterates is the same set `perf.ms` accepts, so a
    /// loop over `perf.buckets()` cannot hit the unknown-bucket error.
    #[test]
    fn every_name_perf_offers_is_a_name_perf_accepts() {
        let (lua, p) = host();
        p.borrow_mut().enable(true);
        p.borrow_mut().end_frame();
        lua.load(
            "for _, b in ipairs(perf.buckets()) do
               local _ = perf.ms(b) + perf.worstMs(b)
             end",
        )
        .exec()
        .expect("every offered bucket must be readable");
    }
}
