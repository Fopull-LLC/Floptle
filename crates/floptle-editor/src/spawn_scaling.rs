//! What it costs a script to build a level — the measurement `floptle/0138` asks
//! for, and the guard that keeps the answer.
//!
//! A streamer, a procedural dungeon, a voxel game and a destructible building
//! are all the same thing: a script that spawns and destroys nodes in bursts.
//! Four separate paths in that round trip were `O(scene)` **per node**, which
//! makes building one chunk `O(scene × chunk)` and the whole level quadratic in
//! how much of itself is already loaded. None of them was a hot inner loop —
//! each was a `world.query::<…>()` where a lookup would do, and every one of
//! them is invisible in a hand-built scene, because a hand-built scene spawns a
//! bullet at a time.
//!
//! ## The measurement
//!
//! `cargo test -p floptle-editor --bin floptle spawn_scaling -- --nocapture`
//! prints ms to build and to tear down a **1,000-node chunk** at scene sizes of
//! 1k / 4k / 8k, for the path as it is and for the path as it was.
//!
//! ## The guard
//!
//! Like `floptle-core/tests/scaling.rs`, the assertion is on a **ratio and not a
//! duration**: a millisecond threshold on a shared runner is a coin flip. The
//! chunk is a fixed size, so its cost must not depend on the size of the scene
//! it lands in — four times the scene, roughly the same time. Anything that
//! scans the scene per node reads four times as long instead, and that is the
//! difference the ceiling below is set to catch.

use std::time::Instant;

use floptle_core::{Matter, Name, ScriptInst, Scripts, Transform, World};

/// Nodes per chunk. Floprooms builds ~800; a round thousand is the ledger's ask.
const CHUNK: usize = 1_000;

/// Write the project the measurement runs in: one prefab, and one script that
/// asks for a chunk of it.
///
/// **The spawns carry a callback**, which is the whole point. A callback-free
/// spawn is the *workaround* the game had to invent — bake the model and the
/// rotation into 624 generated prefabs so that nothing ever needs one — and
/// measuring the workaround would measure the wrong thing.
fn write_project(root: &std::path::Path, with_callback: bool) {
    std::fs::create_dir_all(root.join("prefabs")).unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("prefabs/prop.prefab.ron"),
        r#"[
    (
        name: "prop",
        transform: (
            translation: (0.0, 0.0, 0.0),
            rotation: (0.0, 0.0, 0.0, 1.0),
            scale: (1.0, 1.0, 1.0),
        ),
        matter: Mesh(
            asset_path: "models/prop.glb",
        ),
    ),
]
"#,
    )
    .unwrap();
    let cb = if with_callback { ", function(n) n.y = 1 end" } else { "" };
    std::fs::write(
        root.join("scripts/builder.lua"),
        format!(
            "local done = false\n\
             function update(node, dt)\n\
               if done then return end\n\
               done = true\n\
               for i = 1, {CHUNK} do\n\
                 spawn('prop', vec3(i, 0, 0){cb})\n\
               end\n\
             end\n"
        ),
    )
    .unwrap();
}

/// A scene of `n` ordinary nodes to spawn into, plus the builder that fills it.
fn scenery(world: &mut World, n: usize) {
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(driver, Name("Builder".into()));
    world.insert(driver, Matter::Empty);
    world.insert(
        driver,
        Scripts(vec![ScriptInst {
            kind: "builder".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    for i in 0..n {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Name(format!("scenery{i}")));
        world.insert(e, Matter::Empty);
    }
}

/// ms to build a chunk and ms to tear it down, in a scene that already holds
/// `n` nodes.
fn build_and_tear(dir: &std::path::Path, n: usize, with_callback: bool) -> (f64, f64) {
    write_project(dir, with_callback);
    let mut ed = crate::Editor { project_root: dir.to_path_buf(), ..Default::default() };
    scenery(&mut ed.world, n);

    // One Lua pass to queue the chunk. Not timed: it mirrors the scene once,
    // which is by design and is the same work in every arm — what is being
    // measured is what the DRAIN costs.
    let scripts = dir.join("scripts");
    ed.script_host.run(&mut ed.world, &scripts, 1.0 / 60.0, 0.0);
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());

    let before: std::collections::HashSet<u32> =
        ed.world.query::<Transform>().map(|(e, _)| e.index()).collect();
    let t0 = Instant::now();
    ed.apply_script_spawns();
    let build = t0.elapsed().as_secs_f64() * 1000.0;

    // Tear the chunk down a node at a time — the case the game had to work
    // around by parenting every chunk to one node so that unloading it could be
    // a single `destroy`. That workaround should not be the price of `spawn`.
    let fresh: Vec<u32> = ed
        .world
        .query::<Transform>()
        .map(|(e, _)| e.index())
        .filter(|id| !before.contains(id))
        .collect();
    // **A benchmark that measures nothing reads exactly like a fast one.** If the
    // prefab failed to resolve, or the script failed to load, every number here
    // would be a convincing zero.
    assert_eq!(fresh.len(), CHUNK, "the chunk did not spawn");
    let t1 = Instant::now();
    ed.apply_destroys(fresh);
    let tear = t1.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(ed.world.query::<Transform>().count(), n + 1, "the chunk did not go away");

    (build, tear)
}

fn temp(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "flspawn-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The table `floptle/0138` asks for, printed rather than asserted.
///
/// Kept as a test rather than an example because the spawn path lives in the
/// editor, and the editor is a binary — there is nowhere else it can be run
/// from. It asserts nothing about time; the guard below does that.
#[test]
fn the_cost_of_building_a_chunk_against_the_size_of_the_scene() {
    let dir = temp("table");
    println!("\n{CHUNK}-node chunk, into a scene that already holds:\n");
    println!("    scene     build      tear");
    for n in [1_000usize, 4_000, 8_000] {
        let (build, tear) = build_and_tear(&dir, n, true);
        println!("  {n:>7}  {build:8.1}m {tear:8.1}m");
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\nThe finding is the SHAPE of the build column. A chunk is the same\n\
         thousand nodes every time, so a path that walks the scene per node\n\
         costs eight times as much in the last row as in the first, and one\n\
         that does not costs the same.\n"
    );
}

/// **The guard.** A fixed-size chunk must cost the same whatever it lands in.
///
/// Measured at 2k and 8k — four times the scene — and taking the best of three,
/// because scheduler noise only ever adds time. The ceiling is generous: the
/// signal being caught is a factor of four, and anything under two is noise.
#[test]
fn building_a_chunk_does_not_cost_more_in_a_bigger_scene() {
    let dir = temp("guard");
    let best = |n: usize| {
        (0..3)
            .map(|_| build_and_tear(&dir, n, true))
            .fold((f64::MAX, f64::MAX), |a, b| (a.0.min(b.0), a.1.min(b.1)))
    };
    // Interleaved, and both sizes measured in one alternating pass, for the
    // reason `floptle-core/tests/scaling.rs` spells out: a cold core runs the
    // first size at base clock and the second boosted, and the ratio is then
    // measuring the CPU rather than the code.
    let (mut small, mut large) = ((f64::MAX, f64::MAX), (f64::MAX, f64::MAX));
    for _ in 0..2 {
        let s = best(2_000);
        let l = best(8_000);
        small = (small.0.min(s.0), small.1.min(s.1));
        large = (large.0.min(l.0), large.1.min(l.1));
    }
    let _ = std::fs::remove_dir_all(&dir);

    let build = large.0 / small.0;
    let tear = large.1 / small.1;
    println!("  build {:.2}x   tear {:.2}x  (4x the scene)", build, tear);
    assert!(
        build < 2.5,
        "building a {CHUNK}-node chunk cost {build:.1}x as much in a scene 4x the size \
         ({:.1} ms vs {:.1} ms). A fixed chunk should cost a fixed amount; something in \
         the spawn path is walking the scene per node again.",
        large.0,
        small.0
    );
    assert!(
        tear < 2.5,
        "tearing a {CHUNK}-node chunk down cost {tear:.1}x as much in a scene 4x the size \
         ({:.1} ms vs {:.1} ms).",
        large.1,
        small.1
    );
}
