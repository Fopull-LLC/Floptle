//! Where the ceiling on script instances actually is (`floptle/0069`).
//!
//! A scene of a few thousand scripted nodes used to panic:
//!
//! ```text
//! cannot create a Lua reference, out of auxiliary stack space (used 7999 slots)
//! ```
//!
//! The ledger's hypothesis was that the host's `envs` map — one live
//! `mlua::Table` per instance — was spending a bounded resource that the
//! `RegistryKey` beside it does not. This probe is the confirmation the task
//! asks for BEFORE the rewrite: hold N of each, and see which one falls over.
//!
//! It was, twice over: the host held a live `Table` per instance in `envs` AND
//! another as each instance's cached `node` table, so the ceiling landed around
//! four thousand instances. Both are registry keys now.
//!
//! Part two runs the actual `ScriptHost` over a scene of N scripted nodes, which
//! is the number a game cares about:
//!
//! ```text
//!    nodes    before          after
//!    1,000    ok              ok
//!    5,000    PANIC           ok
//!   10,000    PANIC           ok
//!   20,000    PANIC           ok
//! ```
//!
//! The frame TIME at those sizes is a separate matter and this probe reports it
//! without fixing it: 20,000 scripted nodes is still far too slow to play, for
//! reasons that have nothing to do with the ref stack. What changed is that the
//! failure is now a frame rate you can see and measure instead of a panic that
//! takes the editor and your unsaved scene with it.
//!
//! ```text
//! cargo run -p floptle-script --release --example auxstack_probe
//! ```

use floptle_core::{Name, ScriptInst, Scripts, Transform, World};
use floptle_script::ScriptHost;

fn main() {
    println!("\nwhat a held Lua value costs, by how it is held\n");

    // --- 1: the two ways to hold the same table, side by side ---
    //
    // Caught, because `create_table` does not return an error when the ref
    // stack is full — it PANICS inside mlua, which is the whole complaint. The
    // workspace builds with unwinding panics on purpose (see the note in the
    // root Cargo.toml), so this probe can report the failure instead of being
    // it.
    const WANT: usize = 20_000;
    for as_key in [false, true] {
        let what = if as_key { "RegistryKey" } else { "Table" };
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let lua = mlua::Lua::new();
            let mut tables: Vec<mlua::Table> = Vec::new();
            let mut keys: Vec<mlua::RegistryKey> = Vec::new();
            for _ in 0..WANT {
                let t = lua.create_table().expect("create_table");
                if as_key {
                    keys.push(lua.create_registry_value(t).expect("registry"));
                } else {
                    tables.push(t);
                }
            }
            keys.len() + tables.len()
        }));
        match outcome {
            Ok(n) => println!("  {what:<12} held {n} of them, no trouble"),
            Err(_) => println!("  {what:<12} PANICKED before {WANT} — see the message above"),
        }
    }

    // --- 2: the number a game asks — scripted nodes in one scene ---
    let dir = std::env::temp_dir().join(format!("floptle_aux_probe_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("prop.lua"), "function update(node, dt)\nend\n").expect("write");

    println!("\nand the same thing through the real host — one script per node\n");
    for nodes in [1_000usize, 5_000, 10_000, 20_000] {
        let mut world = World::default();
        for i in 0..nodes {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, Name(format!("prop{i}")));
            world.insert(e, Scripts(vec![ScriptInst {
                kind: "prop".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
            }]));
        }
        let mut host = ScriptHost::new();
        let t0 = std::time::Instant::now();
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
        host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        let errs = host.errors().len();
        println!("  {nodes:>6} scripted nodes   two frames in {ms:6.1} ms   {errs} error(s)");
        if errs > 0 {
            println!("      first: {}", host.errors()[0]);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    println!();
}
