use super::*;

/// The probe: a known slow function and a known run of engine calls, both
/// in one script's `update`, with the profiler and sampler switched on from
/// the script itself on its first frame.
const HOT: &str = "local function grind(n)\n\
  local x = 0\n\
  for i = 1, n do x = x + i % 7 end return x\n\
end\n\
local function casts()\n\
  for i = 1, 300 do spherecast(vec3(0, 5, 0), vec3(0, -1, 0), 0.5, 10) end\n\
end\n\
local frame = 0\n\
function update(node, dt)\n\
  frame = frame + 1\n\
  if frame == 1 then perf.enable(true) perf.sampleScripts(true, 0.05) perf.hitchThreshold(5) return end\n\
  grind(4000000)\n\
  casts()\n\
end\n";

/// **A slow frame names the function, the line and the engine calls it was
/// spent on.** One script spends four million iterations (tens of ms) on
/// line 3 of `grind` and
/// casts 300 spheres from line 6. The frame's record names `grind`:3 as the
/// hottest line and charges `grind` within 10% of the frame's measured CPU
/// time less its casts; the casts are charged to the script under `spherecast`, 300 of
/// them, and their line is charged; and the frame is a hitch at the 5 ms
/// threshold the script set, carrying the same samples.
///
/// Every time here is compared with another measured in the same frame, so a
/// loaded machine stretches both sides alike.
#[test]
fn a_slow_frame_names_its_function_line_and_engine_calls() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-sampler-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "hot", HOT);
    let (mut world, _) = world_with_script("hot");
    let mut host = ScriptHost::new();
    host.set_colliders(
        vec![floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)))],
        glam::DVec3::ZERO,
    );
    let mut run_cpu = 0.0f32;
    for i in 1..=2 {
        let c0 = crate::budget::thread_cpu();
        host.run(&mut world, &dir, 0.016, 0.016 * i as f32);
        if let (Some(a), Some(b)) = (c0, crate::budget::thread_cpu()) {
            run_cpu = b.saturating_sub(a).as_secs_f32() * 1000.0;
        }
        host.profile().borrow_mut().end_frame();
        assert!(host.errors().is_empty(), "frame {i}: {:?}", host.errors());
    }
    let mut prof = host.profile().borrow_mut();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    let script_ms = r.scripts.iter().find(|(n, _)| n == "hot").map(|(_, ms)| *ms).expect("hot ran");
    let cast = r.calls.iter().find(|c| c.script == "hot" && c.call == "spherecast").expect("casts charged to hot");
    assert_eq!(cast.n, 300, "{:?}", r.calls);
    let top = &r.samples[0];
    assert_eq!((top.script.as_str(), top.func.as_str(), top.line), ("hot", "grind", 3), "{:?}", r.samples);
    // The sampler charges this thread's CPU time, so a loaded machine taking
    // the core away mid-spin lands on nobody; the frame's own CPU time is what
    // it is held to. Where the platform has no thread clock it charges the
    // wall, and so is held to the script's wall time.
    let grind: f32 = r.samples.iter().filter(|s| s.func == "grind").map(|s| s.ms).sum();
    let measured = if run_cpu > 0.0 { run_cpu } else { script_ms };
    let expect = measured - cast.ms;
    assert!(
        (grind - expect).abs() <= expect * 0.1,
        "grind sampled {grind:.2} ms against {expect:.2} ms measured ({measured:.2} less {:.2} of casts)",
        cast.ms
    );
    // The line that makes the calls is named and charged. Whether a long call's
    // whole time lands on it is `sampler.rs`'s own guard, which times the call
    // in the clock the sampler charges. Here the casts are timed on the wall and
    // the samples in CPU time, so a moment the OS took the core away mid-sweep
    // (6.5 ms of wall against 1.4 ms charged, once, in a loaded suite) would
    // compare two different clocks.
    let casts_line: f32 = r.samples.iter().filter(|s| s.func == "casts" && s.line == 6).map(|s| s.ms).sum();
    assert!(casts_line > 0.0, "the casts' line 6 was never charged: {:?}", r.samples);
    let h = prof.take_hitches();
    assert_eq!(h.len(), 1, "the script's 5 ms threshold makes the slow frame a hitch: {h:?}");
    assert_eq!(h[0].samples.first().map(|s| (s.func.as_str(), s.line)), Some(("grind", 3)));
    assert_eq!(h[0].calls.first().map(|c| c.call), Some("spherecast"));
    drop(prof);
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The script bucket's time outside every hook is named.** A timer callback
/// that spins for 20 ms runs in no hook, so no script's figure carries it, and
/// it used to sit in the bucket as time nobody could see. It is charged to
/// `scriptsTimers` now, and the rest of the pass is in its own named parts:
/// the work list, the write flush, and the engine between the hooks.
#[test]
fn script_time_outside_the_hooks_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-parts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "timed",
        "function start(node)\n  \
           after(0.001, function() local t = os.clock() while os.clock() - t < 0.02 do end end)\n\
         end\n\
         function fixedUpdate(node, dt) end\n",
    );
    let (mut world, _) = world_with_script("timed");
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    host.profile().borrow_mut().end_frame();
    host.profile().borrow_mut().begin_frame();
    host.run_fixed(&mut world, 0.1, 0.1);
    host.profile().borrow_mut().end_frame();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let prof = host.profile().borrow();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    let part = |name: &str| r.parts.iter().find(|(p, _)| *p == name).map(|(_, ms)| *ms);
    let timers = part("scriptsTimers").unwrap_or(0.0);
    assert!(timers >= 15.0, "the 20 ms timer callback was charged {timers:.2} ms to scriptsTimers: {:?}", r.parts);
    for name in ["scriptsWorkList", "scriptsFlush", "scriptsBetweenHooks"] {
        assert!(part(name).is_some(), "{name} is not named: {:?}", r.parts);
    }
    let hooks: f32 = r.scripts.iter().map(|(_, ms)| *ms).sum();
    // Against the timer's own time rather than a fixed few milliseconds: a
    // loaded machine stretches both, and one at load 16 once stretched the
    // hooks past 5 ms. A spin charged to a hook reads the whole 20 ms there.
    assert!(hooks < 0.5 * timers, "the timer's spin was charged to a hook ({hooks:.2} ms against the timer's {timers:.2})");
}

/// **A file written on the frame is named as such.** `assets.writeText`
/// without `async` holds the frame while the disk writes. On a busy disk
/// that was seconds, and it read as script time. Its time is charged to the
/// `fileWriteOnFrame` part now.
#[test]
fn a_file_written_on_the_frame_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-filewrite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "writer",
        "function update(node, dt)\n  \
           local ok, why = assets.writeText(\"user://perf_note.txt\", string.rep(\"x\", 4096))\n  \
           if not ok then error(why) end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("writer");
    let mut host = ScriptHost::new();
    host.set_project_root(dir.clone());
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    host.profile().borrow_mut().end_frame();
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let prof = host.profile().borrow();
    let r = prof.recent_frames(1).next().expect("a frame").clone();
    assert!(r.parts.iter().any(|(p, _)| *p == "fileWriteOnFrame"), "the on-frame write is not named: {:?}", r.parts);
}

/// **The collector's work between frames is named, and it collects.** A
/// script leaves a few MB of garbage; stepping the collector between frames
/// (`step_gc`, what the engine does each frame) records `scriptsGc`, and
/// stepped through two whole cycles, the heap is smaller than it was.
#[test]
fn the_collector_steps_between_frames_and_is_named() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-gc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "litter",
        "function update(node, dt)\n  \
           local junk = {}\n  \
           for i = 1, 60000 do junk[i] = { i, tostring(i) } end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("litter");
    let mut host = ScriptHost::new();
    host.profile().borrow_mut().enable(true);
    host.profile().borrow_mut().begin_frame();
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    let before = host.lua_used_memory();
    host.step_gc(0.5);
    host.profile().borrow_mut().end_frame();
    let parts = host.profile().borrow().recent_frames(1).next().expect("a frame").parts.clone();
    assert!(parts.iter().any(|(n, ms)| *n == "scriptsGc" && *ms > 0.0), "the collector's step is not named: {parts:?}");
    // Two whole cycles: the first to finish may be one that began before
    // the junk was garbage, and frees none of it.
    let (mut cycles, mut steps) = (0, 0);
    while cycles < 2 && steps < 5000 {
        if host.step_gc(5.0) {
            cycles += 1;
        }
        steps += 1;
    }
    let after = host.lua_used_memory();
    assert!(after < before, "stepping the collector to the end of a cycle freed nothing: {before} -> {after} bytes");
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The collector's work while scripts run is counted per frame, and the
/// engine's own paced steps are not.** A script that leaves megabytes of
/// garbage every frame makes Luau take collector steps inside its hook; the
/// frame's `gcMs` is above zero, within the hook's own time, and readable from
/// Lua with `mainCpuMs` beside it. A frame where no script runs but the
/// engine steps the collector between frames (`step_gc`, the `scriptsGc`
/// part) reads zero.
#[test]
fn collector_time_inside_hooks_is_counted_and_paced_steps_are_not() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-gcms-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "litter",
        "local frame = 0\n\
         function update(node, dt)\n  \
           frame = frame + 1\n  \
           if frame == 1 then perf.enable(true) return end\n  \
           local junk = {}\n  \
           for i = 1, 60000 do junk[i] = { i, tostring(i) } end\n  \
           if frame == 3 then\n    \
             local f = perf.frames(1)[1]\n    \
             assert(type(f.gcMs) == 'number', 'gcMs is not a number')\n    \
             assert(f.gcMs > 0, 'a littering frame read no collector time')\n    \
             assert(f.mainCpuMs == nil or f.mainCpuMs >= 0, 'mainCpuMs is not a time')\n  \
           end\n\
         end\n",
    );
    let (mut world, _) = world_with_script("litter");
    let mut host = ScriptHost::new();
    for i in 1..=3 {
        host.profile().borrow_mut().begin_frame();
        host.run(&mut world, &dir, 0.016, 0.016 * i as f32);
        assert!(host.errors().is_empty(), "frame {i}: {:?}", host.errors());
        host.profile().borrow_mut().end_frame();
    }
    let littered = host.profile().borrow().recent_frames(1).next().expect("a frame").clone();
    assert!(littered.gc_ms > 0.0, "a frame that littered megabytes read no collector time");
    let hook = littered.scripts.iter().find(|(n, _)| n == "litter").map(|(_, ms)| *ms).expect("litter ran");
    assert!(littered.gc_ms <= hook, "collector time {} is more than the hook it fell in, {hook}", littered.gc_ms);
    assert_eq!(littered.main_cpu_ms.is_some(), crate::budget::thread_cpu().is_some());
    host.profile().borrow_mut().begin_frame();
    host.step_gc(2.0);
    host.profile().borrow_mut().end_frame();
    let paced = host.profile().borrow().recent_frames(1).next().expect("a frame").clone();
    assert!(paced.part("scriptsGc") > 0.0, "the paced step did not run: {:?}", paced.parts);
    assert_eq!(paced.gc_ms, 0.0, "the engine's paced collector steps were counted as the scripts'");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A world of `n` nodes running `kind`, then `busy` more running `"busy"`.
fn scripted_crowd(kind: &str, n: usize, busy: usize) -> World {
    let mut world = World::default();
    for i in 0..n + busy {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(glam::DVec3::new(i as f64, 0.0, 0.0)));
        world.insert(
            e,
            Scripts(vec![floptle_core::ScriptInst {
                kind: if i < n { kind.into() } else { "busy".into() },
                enabled: true,
                params: vec![("speed".into(), 2.0)],
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
    }
    world
}

/// One frame the way the editor runs it — the frame pass, a tick, the late
/// pass — each timed into the scripts bucket net of the mirror, as
/// `timed_script_pass` does, so `scriptsUnnamed` is what it is in a game.
fn profiled_frame(host: &mut ScriptHost, world: &mut World, dir: &Path, f: usize) -> floptle_core::profile::FrameRecord {
    use floptle_core::profile::{Bucket, Span};
    let t = f as f32 / 60.0;
    host.profile().borrow_mut().begin_frame();
    for pass in 0..3 {
        let mirror0 = host.profile().borrow().frame_total(Bucket::Mirror);
        let span = Span::new();
        match pass {
            0 => host.run(world, dir, 1.0 / 60.0, t),
            1 => host.run_fixed(world, 1.0 / 60.0, t),
            _ => host.run_late(world, 1.0 / 60.0, t),
        }
        let ms = span.ms();
        let mut p = host.profile().borrow_mut();
        let mirror = p.frame_total(Bucket::Mirror) - mirror0;
        p.record(Bucket::Scripts, (ms - mirror).max(0.0));
    }
    host.profile().borrow_mut().end_frame();
    host.profile().borrow().recent_frames(1).next().expect("a frame").clone()
}

/// **An instance with no hook for a pass costs next to nothing in it.**
/// The script pass's time outside every hook — `scriptsSeed`,
/// `scriptsHookless`, `scriptsBetweenHooks` and whatever is left in
/// `scriptsUnnamed` — for 2,000 instances of a script that does nothing per
/// frame, beside 50 running an `update`, over the frame pass, a tick and the
/// late pass. Two such scripts: a library with no hooks at all, and one whose
/// only hook is `start`.
///
/// Held to the `update` instances' own hook time measured in the same frames,
/// per instance, so a loaded machine stretches both sides alike (best of
/// three runs, since a burst of load can land on either side). Both cost more
/// than twice what a busy instance did: every enabled instance was seeded
/// every frame (two `String` keys, two registry fetches, a fresh `node`
/// handle and registry slot), walked the whole hook road three passes a frame
/// for hooks it does not have, and had its `node` table drained and
/// re-stamped each time. Watched failing at 2.2 times (11.4 µs an idle
/// instance against 5.2 µs a busy one, `scriptsSeed` 3.1 ms and
/// `scriptsUnnamed` 18.6 ms a frame).
///
/// The library must cost under 30% of a busy instance (0.13 after). The
/// `start`-only script is held to 1.3, its own measured bound: `start` handed
/// it its `node` table, which a callback may still write through, so the
/// table is checked every pass for as long as the instance lives — about
/// 0.8 µs a pass at rest (1.06 after; 1.45 with the at-rest check off, which
/// this bound also catches).
#[test]
fn an_instance_without_a_hook_costs_next_to_nothing_outside_the_hooks() {
    let dir = std::env::temp_dir().join(format!("floptle-perf-hookless-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(&dir, "busy", "function update(node, dt) node.y = node.y + dt * params.speed end\n");
    write_script(
        &dir,
        "lib",
        "defaults = { speed = 1 }\nlocal count = 0\nfunction bump() count = count + 1 return count end\n",
    );
    write_script(&dir, "starter", "defaults = { speed = 1 }\nfunction start(node) node.x = node.x + 0 end\n");
    let (idle, busy, frames) = (2000usize, 50usize, 60usize);
    for (kind, bound) in [("lib", 0.3f32), ("starter", 1.3)] {
        let mut tries = String::new();
        let mut passed = false;
        for _ in 0..3 {
            let mut world = scripted_crowd(kind, idle, busy);
            let mut host = ScriptHost::new();
            host.profile().borrow_mut().enable(true);
            for f in 0..5 {
                profiled_frame(&mut host, &mut world, &dir, f);
            }
            assert!(host.errors().is_empty(), "{:?}", host.errors());
            let (mut seed, mut unnamed, mut hookless, mut between, mut hooks) = (0.0f32, 0.0f32, 0.0f32, 0.0f32, 0.0f32);
            for f in 5..5 + frames {
                let r = profiled_frame(&mut host, &mut world, &dir, f);
                seed += r.part("scriptsSeed");
                unnamed += r.part("scriptsUnnamed");
                hookless += r.part("scriptsHookless");
                between += r.part("scriptsBetweenHooks");
                hooks += r.scripts.iter().filter(|(n, _)| n == "busy").map(|(_, ms)| *ms).sum::<f32>();
            }
            assert!(hooks > 0.0, "the busy instances never ran");
            let per_idle = (seed + unnamed + hookless + between) / idle as f32;
            let per_busy = hooks / busy as f32;
            let ratio = per_idle / per_busy;
            let f = frames as f32;
            tries += &format!(
                "\n  {ratio:.2}: an idle instance {:.3} us, a busy one {:.3} us a frame; per frame scriptsSeed {:.3} ms, \
                 scriptsUnnamed {:.3}, scriptsHookless {:.3}, scriptsBetweenHooks {:.3}",
                per_idle / f * 1e3,
                per_busy / f * 1e3,
                seed / f,
                unnamed / f,
                hookless / f,
                between / f,
            );
            if ratio < bound {
                passed = true;
                break;
            }
        }
        assert!(passed, "2,000 idle `{kind}` instances cost more than {bound} of a busy one outside the hooks:{tries}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
