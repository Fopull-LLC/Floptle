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
/// them; and the frame is a hitch at the 5 ms threshold the script set,
/// carrying the same samples.
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
    let casts_line: f32 = r.samples.iter().filter(|s| s.func == "casts").map(|s| s.ms).sum();
    // An engine call never interrupts, so a sample after it carries its whole
    // time: the casts' line is charged at least the casts themselves.
    assert!(
        casts_line >= cast.ms * 0.8,
        "the casts' line was charged {casts_line:.2} ms of {:.2} ms of casts: {:?}",
        cast.ms,
        r.samples
    );
    let h = prof.take_hitches();
    assert_eq!(h.len(), 1, "the script's 5 ms threshold makes the slow frame a hitch: {h:?}");
    assert_eq!(h[0].samples.first().map(|s| (s.func.as_str(), s.line)), Some(("grind", 3)));
    assert_eq!(h[0].calls.first().map(|c| c.call), Some("spherecast"));
    drop(prof);
    let _ = std::fs::remove_dir_all(&dir);
}
