//! What a scripted frame costs, on whichever VM this was built with.
//!
//! The bench gate for ADR-0028: `vm-luau` must not be worse than `vm-luajit` on
//! a real game's shape. Run it twice and compare — the numbers are directly
//! comparable because the workload is identical and neither run touches a GPU:
//!
//! ```text
//! cargo run -p floptle-script --release --example vm_bench
//! scripts/vm.sh luau run -p floptle-script --release --example vm_bench
//! ```
//!
//! **Release matters.** A debug interpreter measures the debug interpreter, and
//! the two VMs are not slowed down by `-O0` in the same proportion — LuaJIT's
//! cost is mostly in its own compiled code, Luau's is mostly in Rust-side
//! binding calls that a debug build inflates. A debug comparison is not a
//! comparison.
//!
//! ## What it measures, and why this shape
//!
//! The spike that chose Luau measured 1M isolated vector operations. That is
//! the right microbenchmark and it is not a frame: a frame also pays for the
//! host's per-frame work — walking the scene mirror, stamping node tables,
//! draining writes — and the VM swap changes the ratio between the two. So this
//! measures **whole frames of a scripted scene**, at three shapes, because a
//! single number would hide which half moved:
//!
//! * **vectors** — every node doing vector maths through its handle each frame
//!   (`node.pos`, arithmetic, write back). The hot path the whole decision
//!   rests on, and the one card `floptle/0176` is about.
//! * **scalars** — the same node count doing a short piece of arithmetic on
//!   plain numbers, the way ordinary game code does. Note that a loop this
//!   short is **below LuaJIT's hotloop threshold (56 by default)**, so LuaJIT
//!   interprets it — which is exactly what happens to most real game code, and
//!   is why this shape is kept separate from the one below rather than being
//!   called "LuaJIT's best case".
//! * **numeric** — a loop long enough to be JIT-compiled. This is LuaJIT's
//!   genuine home ground, and the shape the scoping spike measured LuaJIT
//!   winning (14 ms against 35 ms on a 10M-iteration microbenchmark). It is
//!   here so that a bench which only ever measured LuaJIT's interpreter cannot
//!   report a false all-clear.
//! * **idle** — the same nodes, scripts attached, no hook body doing work. The
//!   floor: what the host charges for a scripted node that does nothing. It is
//!   what a pool of parked objects costs, and it should not move with the VM at
//!   all.
//!
//! ## Reading the output
//!
//! **p95, not mean.** A collector pause is exactly the thing this migration is
//! meant to remove, and averaging hides it — `floptle/0176` reports 2415 frames
//! of ~5100 over 8 ms, which is a tail, not a mean. The max is printed for the
//! same reason.
//!
//! ## What it measured, 2026-09-01 (frame p95, ms, release)
//!
//! | shape | luajit | luau | luau + codegen |
//! | --- | --- | --- | --- |
//! | vectors | 6.39 | 4.86 | 4.92 |
//! | scalars | 5.42 | 4.17 | 4.10 |
//! | numeric | 5.59 | 7.38 | **6.37** |
//! | idle | 5.10 | 4.04 | 4.09 |
//!
//! Luau wins every shape except `numeric`, where LuaJIT stays 1.14× ahead even
//! with Luau's code generator on. That row is the migration's known trade and
//! it is meant to be visible here rather than argued about elsewhere.
//!
//! ## …and what a real game measured, same day
//!
//! This probe is synthetic on purpose — four shapes, so a regression can be
//! attributed to one. It is not the gate. `scripts/scene-bench.sh` is, and it
//! ran Solar's system scene and a Forgery-shaped first-person scene through
//! `floptle run --timing` on a release build of each VM, interleaved:
//!
//! | scene | luajit | luau |
//! | --- | --- | --- |
//! | Solar `system` | 4.81 | **4.15** |
//! | Forgery `first` | 13.99 | **12.38** |
//!
//! Luau by 1.16x and 1.13x, with every interleaved pair going the same way. The
//! `numeric` row above is real and it does not show up in either game, which is
//! what that row is for: 800,000 loop iterations per frame is a stress case, not
//! a script anybody writes.
//!
//! **The mistake this probe already made once**, since it is the easy one to
//! repeat: the first `scalars` shape looped 40 times, which is under LuaJIT's
//! default hotloop threshold of 56 — so LuaJIT never compiled it, the probe was
//! timing its interpreter, and the result said Luau won everything. If you add
//! a shape meant to represent compiled code, make the loop long enough to
//! become hot, or it measures the opposite of what it claims.

use std::time::Instant;

use floptle_core::{ScriptInst, Scripts, Transform, World};
use floptle_script::vm::{VM_HAS_CODEGEN, VM_NAME};
use floptle_script::ScriptHost;

/// Timed frames per shape. Long enough that one scheduler hiccup cannot move
/// p95, short enough that the whole probe is a few seconds per VM.
const FRAMES: usize = 600;
/// Frames run before the clock starts: `start` hooks, handle caching, and the
/// first allocation of every Lua table the run then reuses. Also long enough
/// for LuaJIT to have compiled the loops it is going to compile — timing its
/// interpreter warming up would flatter Luau for no reason.
const WARMUP: usize = 120;
/// Scripted nodes. A real scene's order of magnitude (Solar's main scene is
/// 359 nodes); enough that per-node cost dominates the fixed per-frame cost.
const NODES: usize = 400;

/// The three workloads. Each is a whole script, so the host runs them exactly
/// as it runs a game's.
const SHAPES: &[(&str, &str)] = &[
    (
        "vectors",
        // The hot path: read a vector off the handle, do arithmetic that
        // allocates on both VMs today, write it back.
        r#"
function update(node, dt)
  local p = node.pos
  local v = vec3(dt, dt * 2, dt * 3)
  local q = (p + v) * 0.5
  node.pos = vec3(q.x, q.y, q.z)
end
"#,
    ),
    (
        "scalars",
        // No vectors, no allocation. Deliberately SHORT — 40 iterations is
        // under LuaJIT's hotloop threshold, so this is the interpreter on both
        // VMs, and it is what most game code actually looks like.
        r#"
function update(node, dt)
  local t = 0
  for i = 1, 40 do
    t = t + i * dt
  end
  node.y = t
end
"#,
    ),
    (
        "numeric",
        // Long enough to be JIT-compiled: LuaJIT's real strength, and the one
        // place the scoping spike measured it winning. If Luau is going to lose
        // anywhere, it is here.
        r#"
function update(node, dt)
  local t = 0
  for i = 1, 2000 do
    t = t + i * dt
  end
  node.y = t
end
"#,
    ),
    (
        "idle",
        // A hook that exists and does nothing. The host's own floor.
        r#"
function update(node, dt)
end
"#,
    ),
];

fn main() {
    let dir = std::env::temp_dir().join(format!("floptle_vm_bench_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);

    println!("vm_bench — {NODES} scripted nodes, {FRAMES} frames after {WARMUP} warm-up");
    println!("  vm: {VM_NAME}  codegen: {VM_HAS_CODEGEN}");
    if cfg!(debug_assertions) {
        println!("  !! DEBUG BUILD — these numbers do not compare across VMs. Use --release.");
    }
    println!();
    println!("  {:<9} {:>9} {:>9} {:>9} {:>9}", "shape", "mean", "p50", "p95", "max");

    for (name, body) in SHAPES {
        std::fs::write(dir.join(format!("{name}.lua")), body).expect("write the script");
        let mut world = world_of(name, NODES);
        let mut host = ScriptHost::new();

        for _ in 0..WARMUP {
            host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        }
        // A load failure here would time an empty frame and report a wonderful
        // number for a script that never ran.
        assert!(host.errors().is_empty(), "[{VM_NAME}] {name}: {:?}", host.errors());

        let mut ms: Vec<f64> = Vec::with_capacity(FRAMES);
        for _ in 0..FRAMES {
            let t = Instant::now();
            host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        assert!(host.errors().is_empty(), "[{VM_NAME}] {name} raised: {:?}", host.errors());

        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        ms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN in a duration"));
        println!(
            "  {:<9} {:>8.3} {:>8.3} {:>8.3} {:>8.3}",
            name,
            mean,
            pct(&ms, 0.50),
            pct(&ms, 0.95),
            ms[ms.len() - 1]
        );
    }

    println!();
    println!("  Compare against the other VM's run of this same probe. The gate is that");
    println!("  `vectors` p95 is not worse under luau — see ADR-0028. A regression on a");
    println!("  real game is a stop-and-report, not a number to explain away.");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `p`th percentile of an already-sorted slice, nearest-rank.
fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f64 * p).ceil() as usize).saturating_sub(1);
    sorted[i.min(sorted.len() - 1)]
}

/// A world of `n` nodes, each running the named script.
fn world_of(kind: &str, n: usize) -> World {
    let mut world = World::default();
    for _ in 0..n {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(
            e,
            Scripts(vec![ScriptInst {
                kind: kind.into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
            }]),
        );
    }
    world
}
