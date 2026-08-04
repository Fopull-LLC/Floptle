//! What a 2D game pays on the LUA side for N sprites — the measurement
//! `floptle/0059` asks for, and the other half of `sprite2d_probe`.
//!
//! The 0058 probe measured the GPU and found no difference between a pool of
//! scene-node quads and one `SpriteBatch`: 1400 quads is 1400 quads however
//! they are gathered, and that is still true. The cost a bullet-hell actually
//! hits is on this side of the interpreter boundary — per-sprite property
//! writes through mlua, plus the scene mirror that every one of those nodes is
//! walked into once a frame, whether or not anything touched it.
//!
//! Three shapes, so the number separates the two costs a pool has:
//!
//! * **written** — N quad nodes, each getting `x`/`y`/`z`/`scale`/`roll` and a
//!   sheet cell every frame. What a game writes when it pools scene nodes.
//! * **parked** — the same N nodes, and a script that touches none of them.
//!   Pools grow on demand and never shrink, so this is what last wave's bullets
//!   keep charging after they are invisible.
//! * **batched** — one `SpriteBatch` node, N `b:draw(...)` calls, no pool.
//!
//! Run it:
//!
//! ```text
//! cargo run -p floptle-script --release --example sprite2d_lua_probe
//! ```
//!
//! Release matters — a debug interpreter measures the debug interpreter.

use std::path::Path;
use std::time::Instant;

use floptle_core::{Entity, Material, Matter, Name, ScriptInst, Scripts, Shape, Transform, World};
use floptle_script::ScriptHost;

/// Frames per timed run, after the warm-up. Long enough to swamp a stray
/// scheduler hiccup, short enough that the whole probe is a few seconds.
const FRAMES: usize = 120;
/// Frames run before the clock starts: script `start`, handle caching, and the
/// first allocation of every Lua table the run will reuse.
const WARMUP: usize = 10;

fn main() {
    let dir = std::env::temp_dir().join(format!("floptle_2d_lua_probe_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    println!("\nthe Lua side of N sprites — ms per frame, and the frame budget it eats\n");
    for n in [1500usize, 5500] {
        let written = time(&dir, Shape2d::Written, n);
        let parked = time(&dir, Shape2d::Parked, n);
        let batched = time(&dir, Shape2d::Batched, n);
        println!("  {n} sprites");
        row("pooled nodes, written every frame", written);
        row("pooled nodes, parked and untouched", parked);
        row("one sprite batch", batched);
        println!(
            "                    writing the pool costs {:.1}x the batch, \
             and merely HAVING it costs {:.1}x",
            written / batched.max(1e-9),
            parked / batched.max(1e-9),
        );
        println!();
    }
    println!(
        "A 60 fps frame is 16.7 ms and the game does not get all of it: physics,\n\
         rendering and the rest of the scripts are in there too.\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn row(label: &str, ms: f64) {
    // The share of a 60 fps frame is the number that decides a design, so it is
    // printed next to the milliseconds rather than left as an exercise.
    println!("    {label:<38} {ms:6.2} ms   ({:.0}% of a 60 fps frame)", ms / 16.667 * 100.0);
}

#[derive(Clone, Copy, PartialEq)]
enum Shape2d {
    Written,
    Parked,
    Batched,
}

/// Mean milliseconds per `ScriptHost::run` — the whole Lua pass: mirror the
/// scene, run every `update`, flush the queued writes back into the ECS.
fn time(dir: &Path, shape: Shape2d, n: usize) -> f64 {
    write_script(dir, shape, n);
    let (mut world, _) = build_world(shape, n);
    let mut host = ScriptHost::new();
    for f in 0..WARMUP {
        host.run(&mut world, dir, 1.0 / 60.0, f as f32 / 60.0);
    }
    assert!(host.errors().is_empty(), "script errors: {:?}", host.errors());
    let t0 = Instant::now();
    for f in 0..FRAMES {
        host.run(&mut world, dir, 1.0 / 60.0, (WARMUP + f) as f32 / 60.0);
    }
    t0.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64
}

/// The driver node, plus the pool it drives (nothing, for a batch).
fn build_world(shape: Shape2d, n: usize) -> (World, Entity) {
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(driver, Name("Driver".into()));
    world.insert(driver, Scripts(vec![ScriptInst {
        kind: "probe".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
    }]));
    match shape {
        Shape2d::Batched => {
            world.insert(driver, Matter::SpriteBatch { size: 1.0 });
            world.insert(driver, sheet_material());
        }
        Shape2d::Written | Shape2d::Parked => {
            for i in 0..n {
                let e = world.spawn();
                world.insert(e, Transform::IDENTITY);
                world.insert(e, Name(format!("b{i}")));
                // A 2D game's sprite IS a plane with a spritesheet on it.
                world.insert(e, Matter::Primitive { shape: Shape::Plane, color: [1.0; 3] });
                world.insert(e, sheet_material());
            }
        }
    }
    (world, driver)
}

fn sheet_material() -> Material {
    Material {
        texture: Some("textures/Grass.png".into()),
        sheet_cols: 4,
        sheet_rows: 4,
        ..Default::default()
    }
}

/// The Lua each shape runs. Deliberately the same arithmetic in all three —
/// what differs is only how the result reaches the engine.
fn write_script(dir: &Path, shape: Shape2d, n: usize) {
    let src = match shape {
        // Cache the handles in `start` the way a pool does — charging `find`
        // to the per-frame cost would measure the wrong thing.
        Shape2d::Written => format!(
            "\
local pool = {{}}
function start(node)
  for i = 1, {n} do pool[i] = find('b' .. (i - 1)) end
end
function update(node, dt)
  local t = node.time or 0
  for i = 1, {n} do
    local s = pool[i]
    if s then
      s.x = (i % 40) * 1.1 + t
      s.y = (i / 40) * 1.1
      s.z = 0
      s.scale = 1.0
      s.roll = t
      s.cell = i % 16
    end
  end
end
"
        ),
        // The pool exists and is iterated by the engine; the game has moved on.
        Shape2d::Parked => "function update(node, dt)\nend\n".to_string(),
        Shape2d::Batched => format!(
            "\
function update(node, dt)
  local b = node:sprites()
  local t = node.time or 0
  for i = 1, {n} do
    b:draw((i % 40) * 1.1 + t, (i / 40) * 1.1, 0, 1.0, t, i % 16)
  end
end
"
        ),
    };
    std::fs::write(dir.join("probe.lua"), src).expect("write probe.lua");
}
