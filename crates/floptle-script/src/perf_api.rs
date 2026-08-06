//! The Lua `perf` table — a game reading its own frame cost (`floptle/0077`).
//!
//! The point of this existing at all is that "the engine is slow" was the only
//! report a game could make. Four separate tickets came in that way and every one
//! turned out to be a number the game could have read itself: a component lookup
//! that was a linear scan, `findScript` doing the same, a scatter field asking for
//! 117,000 props, and terrain priority ignoring world distance.
//!
//! So this is not a debugging aid bolted to the editor. It is readable from Lua on
//! purpose, so a project can assert its own budget in a smoke test and find out
//! from CI rather than from a player.
//!
//! ```lua
//! function start(node)
//!   perf.enable(true)
//! end
//!
//! function update(node, dt)
//!   -- The number worth watching is the WORST recent frame, not the mean: a 40 ms
//!   -- hitch once a second is under a millisecond of average.
//!   if perf.worstMs("scripts") > 6 then
//!     log("slow pass — " .. perf.slowestScript())
//!   end
//! end
//! ```
//!
//! # Reading while it is off is an ERROR, not a zero
//!
//! Collection costs nothing when nothing is looking, which is the only way a
//! profiler stays switched on. But that makes "off" and "free" the same shape, and
//! a smoke test asserting `perf.ms("scripts") < 4` would then pass by measuring
//! nothing. So every getter raises while collection is off and says to call
//! `perf.enable(true)`. Same reasoning as `floptle/0082`, applied to this task's
//! own API.

use mlua::Lua;

use floptle_core::profile::Bucket;

use crate::SharedProfile;

/// Install the `perf` global.
pub fn install(lua: &Lua, profile: &SharedProfile) -> mlua::Result<()> {
    let t = lua.create_table()?;

    // perf.enable(on) — start or stop collecting. Off is the default, and off is
    // free. Stopping CLEARS the history: a stale mean from before a fix looks
    // exactly like a fix that did not work.
    let p = profile.clone();
    t.set(
        "enable",
        lua.create_function(move |_, on: Option<bool>| {
            p.borrow_mut().enable(on.unwrap_or(true));
            Ok(())
        })?,
    )?;

    // perf.enabled() -> bool
    let p = profile.clone();
    t.set("enabled", lua.create_function(move |_, ()| Ok(p.borrow().enabled()))?)?;

    // perf.ms(bucket) -> the rolling average, in milliseconds.
    let p = profile.clone();
    t.set(
        "ms",
        lua.create_function(move |_, name: String| {
            Ok(cost(&p, &name, "ms")?.ms as f64)
        })?,
    )?;

    // perf.worstMs(bucket) -> the worst single frame in the last second.
    //
    // The one to reach for. A spike is what anybody is ever chasing and a mean
    // hides it, so this is deliberately as easy to call as `ms`.
    let p = profile.clone();
    t.set(
        "worstMs",
        lua.create_function(move |_, name: String| {
            Ok(cost(&p, &name, "worstMs")?.worst_ms as f64)
        })?,
    )?;

    // perf.scriptMs(kind) / perf.scriptWorstMs(kind) — one script's own cost, by
    // FILE NAME. "Which of my scripts is doing this" is the question; a total for
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
            for (i, (name, c)) in prof.scripts().into_iter().enumerate() {
                let row = lua.create_table()?;
                row.set("name", name)?;
                row.set("ms", c.ms as f64)?;
                row.set("worstMs", c.worst_ms as f64)?;
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

    // perf.counts() -> { nodes=, culled=, instances=, draws=, chunks=, props=,
    // particles= }
    //
    // Free to keep, and three of the four misdiagnosed tickets were answerable
    // from one of these alone — `0071` was a report of 117,000 props with no way
    // to see the number.
    let p = profile.clone();
    t.set(
        "counts",
        lua.create_function(move |lua, ()| {
            let c = p.borrow().counts();
            let out = lua.create_table()?;
            out.set("nodes", c.nodes)?;
            out.set("culled", c.culled)?;
            out.set("instances", c.instances)?;
            out.set("draws", c.draws)?;
            out.set("chunks", c.chunks)?;
            out.set("props", c.props)?;
            out.set("particles", c.particles)?;
            // The capped resources, and what each cap actually cut. A ceiling a
            // game cannot see is one it discovers as "my seventeenth torch does
            // nothing" (`floptle/0114`, `floptle/0116`), so the pair is the
            // point: one number for the cost, one for what was refused.
            out.set("effects", c.effects)?;
            out.set("effectsDropped", c.effects_dropped)?;
            out.set("lights", c.lights)?;
            out.set("lightsDropped", c.lights_dropped)?;
            out.set("voices", c.voices)?;
            // What 2D lighting costs this frame: flat surfaces rasterized a
            // second time into its G-buffer (`floptle/0122`). Zero when no light
            // can reach them, which is the answer a 2D game most wants to be
            // able to check.
            out.set("flat2d", c.flat2d)?;
            Ok(out)
        })?,
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

/// Resolve a bucket name and read its cost, refusing clearly on both failures.
fn cost(
    p: &SharedProfile,
    name: &str,
    call: &str,
) -> mlua::Result<floptle_core::profile::Cost> {
    let prof = p.borrow();
    require_on(&prof, call)?;
    // An unrecognised bucket names the whole set rather than answering zero — the
    // property, the value, and what is accepted (`floptle/0082`).
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

/// The calls that answer while collection is OFF, because they are meaningful
/// then: the switch itself, whether it is on, the bucket names, and the counts
/// (which are free to keep and obviously not a measurement when zero).
///
/// Everything else refuses, and the test below enumerates the real `perf` table
/// against this list — so a getter added later cannot quietly default to
/// answering zero without somebody deciding it should.
#[cfg(test)]
const READABLE_WHILE_OFF: &[&str] = &["enable", "enabled", "buckets", "counts"];

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::profile::{Counts, FrameProfile};

    fn host() -> (Lua, SharedProfile) {
        let lua = Lua::new();
        let p: SharedProfile = std::rc::Rc::new(std::cell::RefCell::new(FrameProfile::default()));
        install(&lua, &p).expect("install perf");
        (lua, p)
    }

    /// Reading a time while collection is off RAISES, and the message says how to
    /// turn it on.
    ///
    /// This is the whole design decision. A zero would let
    /// `assert(perf.ms("scripts") < 4)` pass in a smoke test that measured
    /// nothing, which is the exact shape of bug this engine has shipped 32 times.
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
        // EVERY getter refuses, enumerated from the `perf` table itself rather
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
            prof.record_script("vessel_controller", 5.0);
            prof.record_script("pulsate", 0.25);
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
        // The scripts bucket is the rows added up.
        assert!(lua.load("return perf.worstMs('scripts')").eval::<f64>().unwrap() > 5.0);
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
