//! When a fixed tick costs more than the time it simulates, the game slows
//! down instead of falling to a few frames a second, and says so once.
//!
//! Driven the way a window drives the editor: each frame is handed the wall
//! time the previous one took. That feedback is what turns an expensive tick
//! into a spiral — a long frame banks more ticks, which make the next frame
//! longer — so a test that stepped a fixed `dt` could not see it at all.

use floptle_core::time::Instant;

use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform};

/// A project whose one script burns `ms` of wall time in every `fixedUpdate`.
fn heavy_project(root: &std::path::Path, ms: f64) {
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("scripts/heavy.lua"),
        format!(
            "function fixedUpdate(node, dt)\n\
               local t = os.clock()\n\
               while os.clock() - t < {} do end\n\
             end\n",
            ms / 1000.0
        ),
    )
    .unwrap();
}

/// Frames rendered in `seconds` of wall time, and what the Console said.
fn play_for(dir: &std::path::Path, policy: floptle_scene::TickOverloadDoc, seconds: f64) -> (usize, Vec<String>) {
    let mut ed = crate::Editor { project_root: dir.to_path_buf(), ..Default::default() };
    ed.project.tick_overload = policy;
    let e = ed.world.spawn();
    ed.world.insert(e, Transform::IDENTITY);
    ed.world.insert(e, Name("Heavy".into()));
    ed.world.insert(e, Matter::Empty);
    ed.world.insert(
        e,
        Scripts(vec![ScriptInst { kind: "heavy".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new() }]),
    );
    ed.toggle_play();
    assert!(ed.playing, "did not enter play");
    let start = Instant::now();
    let mut last = 1.0f32 / 60.0;
    let mut frames = 0;
    // The first second is detection; count the frames after it.
    let mut counted = 0;
    while start.elapsed().as_secs_f64() < seconds {
        let t0 = Instant::now();
        ed.play_step(last, true);
        ed.drain_script_logs();
        ed.script_host.profile().borrow_mut().end_frame();
        last = t0.elapsed().as_secs_f32().min(0.25);
        frames += 1;
        if start.elapsed().as_secs_f64() > 1.0 {
            counted += 1;
        }
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(frames > 0);
    let said = ed.console.entries.iter().map(|e| e.msg.clone()).collect();
    (counted, said)
}

/// **An overloaded tick plays in slow motion, not at 2 fps.** Thirty
/// milliseconds a tick against a 16.7 ms slice: catching up runs the maximum
/// ticks every frame and settles near 4 fps; slow motion runs one a frame and
/// stays above 15. The Console says it once, with the numbers.
#[test]
fn an_overloaded_tick_plays_in_slow_motion_and_says_so_once() {
    let dir = std::env::temp_dir().join(format!("floptle_overload_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    heavy_project(&dir, 30.0);
    let secs = 3.5;
    let (slow, said) = play_for(&dir, floptle_scene::TickOverloadDoc::SlowMotion, secs);
    let (catch, _) = play_for(&dir, floptle_scene::TickOverloadDoc::CatchUp, secs);
    let fps = |n: usize| n as f64 / (secs - 1.0);
    assert!(fps(slow) >= 15.0, "slow motion ran at {:.1} fps", fps(slow));
    assert!(fps(catch) < fps(slow) / 2.0, "catch-up {:.1} fps vs slow motion {:.1}: no spiral to escape?", fps(catch), fps(slow));
    let warnings: Vec<&String> = said.iter().filter(|m| m.contains("more than its")).collect();
    assert_eq!(warnings.len(), 1, "said {} times: {said:#?}", warnings.len());
    let w = warnings[0];
    assert!(w.contains("16.7 ms slice") && w.contains("slow motion") && w.contains("scripts"), "{w}");
    let _ = std::fs::remove_dir_all(&dir);
}
