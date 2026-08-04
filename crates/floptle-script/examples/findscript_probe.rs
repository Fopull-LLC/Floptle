//! What cross-script wiring costs — the measurement `floptle/0063` asks for.
//!
//! `find(name)` has had an index for a long time. `findScript`, `findScripts`
//! and `findTagged` walked the whole scene and string-compared their way
//! through it, on every call — so the cost of one script asking for another
//! scaled with the size of the scene, and a project that wires panels to
//! singletons pays it per panel per frame. One real project measured 126
//! full-scene scans a frame, none of them carelessly written: the alternative
//! (an Inspector wire) does not exist for a singleton sixteen panels want, for
//! "is any craft being flown", or for anything spawned at runtime.
//!
//! Two scene sizes, because the shape of the curve is the finding — a cost that
//! grows with the scene looks fine at the size you develop at.
//!
//! ```text
//! cargo run -p floptle-script --release --example findscript_probe
//! ```

use std::path::Path;
use std::time::Instant;

use floptle_core::{Name, ScriptInst, Scripts, Tags, Transform, World};
use floptle_script::ScriptHost;

const FRAMES: usize = 120;
const WARMUP: usize = 10;
/// Lookups each frame, spread over the panels doing them — the shape of a real
/// frame, where many small scripts each ask for the manager they need.
const CALLS: usize = 120;

fn main() {
    let dir = std::env::temp_dir().join(format!("floptle_find_probe_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    println!("\ncross-script lookups: what {CALLS} of them a frame cost\n");
    for nodes in [500usize, 5000] {
        // The same scene twice, with and without the lookups, because the Lua
        // pass also mirrors the scene every frame and that cost is linear in
        // node count whatever the lookups do. The DIFFERENCE is the answer.
        let idle = time(&dir, nodes, 0);
        let busy = time(&dir, nodes, CALLS);
        let each = (busy - idle) * 1000.0 / CALLS as f64;
        println!("  {nodes:>5} nodes   idle {idle:6.2} ms   +{CALLS} lookups {busy:6.2} ms");
        println!("                lookups cost {:6.2} ms  ({each:.2} us each)", busy - idle);
    }
    println!(
        "\nThe finding is whether the per-lookup number is the SAME in both\n\
         scenes. A lookup that walks the scene costs ten times as much in a\n\
         scene ten times as big; one that reads an index does not care.\n"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Mean ms per `ScriptHost::run` — the whole Lua pass, mirror included.
fn time(dir: &Path, nodes: usize, calls: usize) -> f64 {
    write_script(dir, calls);
    let mut world = build_world(nodes);
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

/// One driver, one singleton manager, and `nodes` worth of ordinary scenery —
/// the manager LAST, which is the honest case: a scan finds it only after
/// walking everything else, and where it happens to sit is not something a
/// scene author thinks about.
fn build_world(nodes: usize) -> World {
    let mut world = World::default();
    let driver = world.spawn();
    world.insert(driver, Transform::IDENTITY);
    world.insert(driver, Name("Driver".into()));
    world.insert(driver, Scripts(vec![inst("probe")]));

    for i in 0..nodes {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Name(format!("prop{i}")));
        // Scenery carries tags, and every tenth one a script, so the scan has
        // real strings to compare rather than empty slots to skip. Not one
        // script EACH: a live Lua environment per instance is held by the host,
        // and a few thousand of those hit a limit of their own (see the note in
        // the ledger) which is not what this probe is measuring.
        world.insert(e, Tags(vec!["scenery".into()]));
        if i % 10 == 0 {
            world.insert(e, Scripts(vec![inst("prop")]));
        }
    }

    let manager = world.spawn();
    world.insert(manager, Transform::IDENTITY);
    world.insert(manager, Name("Company".into()));
    world.insert(manager, Scripts(vec![inst("company")]));
    world.insert(manager, Tags(vec!["manager".into()]));
    world
}

fn inst(kind: &str) -> ScriptInst {
    ScriptInst {
        kind: kind.into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
    }
}

fn write_script(dir: &Path, calls: usize) {
    std::fs::write(
        dir.join("prop.lua"),
        "-- scenery: exists to be walked past\nfunction update(node, dt)\nend\n",
    )
    .expect("write prop.lua");
    std::fs::write(dir.join("company.lua"), "balance = 0\n").expect("write company.lua");
    std::fs::write(
        dir.join("probe.lua"),
        format!(
            "\
function update(node, dt)
  for i = 1, {calls} do
    local c = findScript('company')
    local all = findScripts('company')
    local m = findTagged('manager')
  end
end
"
        ),
    )
    .expect("write probe.lua");
}
