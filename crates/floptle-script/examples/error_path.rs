//! Does a Lua error raised from a Rust callback survive in a RELEASE build?
//!
//! `cargo test` silently ignores a profile's `panic` setting (tests need unwind), so the
//! unit tests cannot answer this — the abort in floptle/0025 only reproduces in a real
//! release binary. This is that binary.
//!
//!     cargo run --release -p floptle-script --example error_path
//!
//! Exit 0 = the error surfaced as a script error. A SIGABRT means a Lua error unwinding
//! out of a Rust callback is killing the process.

use std::io::Write;

use floptle_core::transform::Transform;
use floptle_core::{Matter, ScriptInst, Scripts, World};
use floptle_script::ScriptHost;

fn main() {
    let dir = std::env::temp_dir().join("floptle-error-path");
    std::fs::create_dir_all(&dir).unwrap();
    let mut f = std::fs::File::create(dir.join("boom.lua")).unwrap();
    // Two shapes of failure: an error raised by a Rust callback, and a plain Lua error.
    f.write_all(
        b"function update(node, dt)\n\
            local ok, err = pcall(function() node:setMaterial{ emissive = { nope = 1 } } end)\n\
            print('pcall caught: ' .. tostring(ok) .. ' ' .. tostring(err))\n\
            node:setMaterial{ emissive = { nope = 1 } }\n\
          end\n",
    )
    .unwrap();
    drop(f);

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Matter::Empty);
    world.insert(
        e,
        Scripts(vec![ScriptInst {
            kind: "boom".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    println!("running the hook…");
    host.run(&mut world, &dir, 0.016, 0.0);
    for log in host.drain_logs() {
        println!("  log: {}", log.msg);
    }
    let errs = host.errors();
    println!("errors: {errs:?}");
    assert!(!errs.is_empty(), "the bad shape should have been recorded as a script error");
    println!("SURVIVED — the process is still here.");
}
