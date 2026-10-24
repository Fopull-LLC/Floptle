//! What a script pays to read vectors and walk children, per call.
//!
//! A loop over a thousand parts reads three components of a position per
//! part, and looks parts up by id; when each of those is a host call, a
//! per-part loop inside a per-part loop goes quadratic in engine calls. This
//! prints the per-call cost of each, on a node with a thousand children:
//!
//! ```text
//! cargo run -p floptle-script --example script_access_probe
//! ```
//!
//! * **read** — one `.x`/`.y`/`.z` on an `exact` vec3, ns.
//! * **make** — one `vec3(x, y, z)`, ns.
//! * **arith** — one `+` or `*` between vectors (or a vector and a number), ns.
//! * **pos1000** — every child's `.pos` read and its three components summed, ms.
//! * **children** — one `node:children()` on the thousand-child node, ms.
//! * **byId1000** — a thousand `node:child(id)` lookups, ms.

use floptle_core::{Name, Parent, ScriptInst, Scripts, Transform, World};
use floptle_script::ScriptHost;

const SCRIPT: &str = r##"
local done = false
function update(node, dt)
  if done then return end
  done = true
  local v, s = vec3(1, 2, 3), 0
  local t0 = os.clock()
  for i = 1, 300000 do s = s + v.x + v.y + v.z end
  log(string.format("read %.1f ns", (os.clock() - t0) * 1e9 / 9e5))
  t0 = os.clock()
  for i = 1, 100000 do local p = vec3(i, 0, 0) end
  log(string.format("make %.1f ns", (os.clock() - t0) * 1e9 / 1e5))
  local a, b = vec3(1, 2, 3), vec3(4, 5, 6)
  t0 = os.clock()
  for i = 1, 100000 do local c = a + b * 2 end
  log(string.format("arith %.1f ns", (os.clock() - t0) * 1e9 / 2e5))
  local kids = node:children()
  t0 = os.clock()
  for i = 1, #kids do local p = kids[i].pos; s = s + p.x + p.y + p.z end
  log(string.format("pos1000 %.3f ms", (os.clock() - t0) * 1e3))
  t0 = os.clock()
  for k = 1, 20 do kids = node:children() end
  log(string.format("children %.3f ms", (os.clock() - t0) * 1e3 / 20))
  local ok = pcall(function() return node:child(kids[1].id) end)
  if ok then
    t0 = os.clock()
    for i = 1, #kids do node:child(kids[i].id) end
    log(string.format("byId1000 %.3f ms", (os.clock() - t0) * 1e3))
  end
end
"##;

fn main() {
    let dir = std::env::temp_dir().join(format!("floptle_access_probe_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("probe.lua"), SCRIPT).expect("write the script");
    let mut world = World::default();
    let root = world.spawn();
    world.insert(root, Transform::IDENTITY);
    world.insert(root, Name("Craft".into()));
    world.insert(
        root,
        Scripts(vec![ScriptInst {
            kind: "probe".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    for i in 0..1000 {
        let e = world.spawn();
        world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(i as f64, 0.0, 0.0)));
        world.insert(e, Name(format!("part{i}")));
        world.insert(e, Parent(root));
    }
    let mut host = ScriptHost::new();
    host.set_playing(true);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    assert!(host.errors().is_empty(), "{:?}", host.errors());
    if cfg!(debug_assertions) {
        println!("(debug build: this crate at opt-level 1, mlua and Luau at 3)");
    }
    for l in host.drain_logs() {
        println!("  {}", l.msg);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
