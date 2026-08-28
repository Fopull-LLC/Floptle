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

// ---------------------------------------------------------------------------
// Re-measuring one chunk of a level, against re-measuring the level
// ---------------------------------------------------------------------------

/// The Nav Mesh node a ring of this size wants.
fn nav_node_matter(span: f32) -> floptle_core::Matter {
    floptle_core::Matter::NavMesh {
        id: 1,
        half_extents: [span, 8.0, span],
        auto_bounds: true,
        layers: Vec::new(),
        agent_radius: 0.4,
        agent_height: 2.0,
        max_slope: 45.0,
        step_height: 0.4,
        cell_size: 0.5,
        enabled: true,
        auto_rebake: false,
        // This is a bake-throughput benchmark, and link generation is a
        // separate cost with its own guard. Off, so the number this measures
        // stays the number it measured before.
        max_drop: 0.0,
        max_jump: 0.0,
        min_region_area: 1.0,
    }
}

/// A ring of `chunks × chunks` chunks, each a floor slab and some props.
///
/// Deliberately built out of primitives and mesh nodes with static box bodies —
/// the two things a streamed level is actually made of, and the two the gather
/// can bound without opening a file.
fn streamed_ring(ed: &mut crate::Editor, chunks: usize) {
    use floptle_core::{BodyKind, BodyMode, Matter, RigidBody, Shape, Transform};
    const CHUNK_M: f32 = 32.0;
    for cz in 0..chunks {
        for cx in 0..chunks {
            let ox = cx as f32 * CHUNK_M;
            let oz = cz as f32 * CHUNK_M;
            // The floor.
            let f = ed.world.spawn();
            ed.world.insert(
                f,
                Transform {
                    translation: floptle_core::math::DVec3::new(
                        (ox + CHUNK_M * 0.5) as f64,
                        0.0,
                        (oz + CHUNK_M * 0.5) as f64,
                    ),
                    ..Transform::IDENTITY
                },
            );
            ed.world.insert(f, Matter::Primitive { shape: Shape::Plane, color: [0.5; 3] });
            ed.world.insert(f, floptle_core::Collidable);
            let t = ed.world.get_mut::<Transform>(f).expect("just inserted");
            t.scale = floptle_core::math::Vec3::new(CHUNK_M / 0.7, 1.0, CHUNK_M / 0.7);

            // …and the props standing on it.
            for i in 0..24 {
                let p = ed.world.spawn();
                let a = i as f32 * 0.7;
                ed.world.insert(
                    p,
                    Transform {
                        translation: floptle_core::math::DVec3::new(
                            (ox + 4.0 + (a.cos() * 12.0 + 12.0)) as f64,
                            0.5,
                            (oz + 4.0 + (a.sin() * 12.0 + 12.0)) as f64,
                        ),
                        ..Transform::IDENTITY
                    },
                );
                ed.world.insert(p, Matter::Mesh { asset_path: "models/prop.glb".into() });
                ed.world.insert(
                    p,
                    RigidBody {
                        kind: BodyKind::Box,
                        mode: BodyMode::Static,
                        half_extents: [0.5, 0.5, 0.5],
                        ..Default::default()
                    },
                );
            }
        }
    }
}

/// The measurement `floptle/0140` asks for: what it costs to re-measure one
/// 32 m chunk of a ring, against re-measuring the ring.
///
/// `cargo test -p floptle-editor --bin floptle rebake_a_chunk -- --nocapture`
#[test]
fn rebake_a_chunk_against_rebaking_the_level() {
    use floptle_core::Transform;
    let dir = temp("rebake");
    println!("\n  one 32 m chunk re-measured, in a ring of:\n");
    println!("    ring        nodes    whole level     one chunk");
    for chunks in [2usize, 3, 4] {
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        // The navmesh node, sized to hold the whole ring.
        let span = chunks as f32 * 32.0;
        let nav = ed.world.spawn();
        ed.world.insert(nav, Transform::IDENTITY);
        ed.world.insert(
            nav,
            nav_node_matter(span),
        );
        streamed_ring(&mut ed, chunks);
        let nodes = ed.world.query::<Transform>().count();

        // A whole-level bake, on this thread, the way the watcher would ask for
        // one. Timed as the gather plus the bake, which is what a rebake is.
        let t0 = Instant::now();
        ed.bake_nav();
        // The bake is handed to a worker; wait for it the way the editor does.
        while ed.nav_baked.is_none() {
            ed.poll_nav_bake();
            std::thread::yield_now();
        }
        let whole = t0.elapsed().as_secs_f64() * 1000.0;

        // …and one chunk of it, re-measured.
        let t1 = Instant::now();
        ed.rebake_region(
            floptle_core::math::DVec3::new(16.0, 0.0, 16.0),
            floptle_core::math::Vec3::new(32.0, 16.0, 32.0),
        )
        .expect("the chunk re-measures");
        let one = t1.elapsed().as_secs_f64() * 1000.0;

        println!("  {chunks}x{chunks}  {nodes:>9}  {whole:>10.1}m  {one:>10.1}m");
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!(
        "\n  The whole-level column grows with the ring, because it is measuring\n  \
         the ring. The chunk column should not, because a chunk is a chunk\n  \
         whatever is around it.\n"
    );
}

/// **The guard.** Re-measuring a fixed box must not cost more in a bigger level.
#[test]
fn re_measuring_a_chunk_does_not_cost_more_in_a_bigger_level() {
    use floptle_core::Transform;
    let dir = temp("rebake-guard");
    let cost = |chunks: usize| -> f64 {
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let span = chunks as f32 * 32.0;
        let nav = ed.world.spawn();
        ed.world.insert(nav, Transform::IDENTITY);
        ed.world.insert(
            nav,
            nav_node_matter(span),
        );
        streamed_ring(&mut ed, chunks);
        ed.bake_nav();
        while ed.nav_baked.is_none() {
            ed.poll_nav_bake();
            std::thread::yield_now();
        }
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = Instant::now();
            ed.rebake_region(
                floptle_core::math::DVec3::new(16.0, 0.0, 16.0),
                floptle_core::math::Vec3::new(32.0, 16.0, 32.0),
            )
            .expect("re-measures");
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
        }
        best
    };
    // Interleaved, for the reason `floptle-core/tests/scaling.rs` gives.
    let (mut small, mut large) = (f64::MAX, f64::MAX);
    for _ in 0..2 {
        small = small.min(cost(2));
        large = large.min(cost(4));
    }
    let _ = std::fs::remove_dir_all(&dir);
    let ratio = large / small;
    println!("  chunk rebake {ratio:.2}x for 4x the level ({large:.1} ms vs {small:.1} ms)");
    assert!(
        ratio < 3.0,
        "re-measuring one 32 m chunk cost {ratio:.1}x as much in a level four times the size \
         ({large:.1} ms vs {small:.1} ms) — the regional bake is reading more than its box"
    );
}
