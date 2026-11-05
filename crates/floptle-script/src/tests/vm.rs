//! **A multiple assignment evaluates every value before it assigns any target**,
//! as Lua and LuaJIT do. Stock Luau evaluated each value straight into its
//! local, so a later value that read that local through a closure saw the new
//! value. Solar's docking-port axis came out 8.8° off that way (0641). The fix
//! lives in the vendored Luau compiler (`third_party/luau0-src`, see
//! FLOPTLE-PATCHES.md), and these guards fail if an upgrade loses it.

use super::*;

/// Each case returns what it computed, then what Lua says it should be.
const CASES: &[(&str, &str)] = &[
    // The 0641 shape: a closure reads a local the statement assigned earlier.
    (
        "closure reads an earlier target",
        "local U = 1\nlocal function W(x) return U * 10 + x end\nlocal R, Z\nR, U, Z = W(1), W(2), W(3)\nreturn R .. ' ' .. U .. ' ' .. Z, '11 12 13'",
    ),
    // The same inside a function, the shape a script's update has.
    (
        "inside a function",
        "local function f()\n  local U = 1\n  local function W(x) return U * 10 + x end\n  local R, Z\n  R, U, Z = W(1), W(2), W(3)\n  return R .. ' ' .. U .. ' ' .. Z\nend\nreturn f(), '11 12 13'",
    ),
    // A function compiled AFTER the statement captures the local: no compile-time
    // capture flag could have seen it.
    (
        "captured by a later function",
        "local U = 1\nlocal R, Z\nlocal function step() R, U, Z = W(1), W(2), W(3) end\nfunction W(x) return U * 10 + x end\nstep()\nreturn R .. ' ' .. U .. ' ' .. Z, '11 12 13'",
    ),
    // A metamethod reads it while a value is indexed.
    (
        "a metamethod reads an earlier target",
        "local U = 1\nlocal t = setmetatable({}, { __index = function(_, k) return U * 10 + k end })\nlocal R, Z\nR, U, Z = t[1], t[2], t[3]\nreturn R .. ' ' .. U .. ' ' .. Z, '11 12 13'",
    ),
    // A value past the last target still runs after the targets are set.
    (
        "a trailing value",
        "local U = 1\nlocal seen\nlocal function W() seen = U return 0 end\nlocal R\nR, U = 5, 7, W()\nreturn seen .. ' ' .. R .. ' ' .. U, '1 5 7'",
    ),
    // What already worked has to keep working: a swap, the manual's example,
    // and a call filling several targets.
    ("a swap", "local a, b = 1, 2\na, b = b, a\nreturn a .. ' ' .. b, '2 1'"),
    ("the manual's example", "local i, a = 3, {}\ni, a[i] = i + 1, 20\nreturn tostring(a[3]) .. ' ' .. tostring(a[4]), '20 nil'"),
    ("a call fills the rest", "local function f() return 2, 3 end\nlocal x, y, z\nx, y, z = 1, f()\nreturn x .. ' ' .. y .. ' ' .. z, '1 2 3'"),
];

#[test]
fn a_multiple_assignment_evaluates_every_value_before_assigning() {
    let mut wrong = Vec::new();
    for level in 0..=2 {
        for (what, src) in CASES {
            let lua = mlua::Lua::new();
            lua.set_compiler(mlua::Compiler::new().set_optimization_level(level));
            let (got, want): (String, String) = lua.load(*src).eval().unwrap_or_else(|e| panic!("{what}: {e}"));
            if got != want {
                wrong.push(format!("{what} at optimisation level {level}: got {got}, Lua gives {want}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The same through a node script, the way a game runs it.
#[test]
fn a_node_script_sees_values_evaluated_before_assignment() {
    let dir = std::env::temp_dir().join(format!("floptle_script_test_multi_assign_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "dock",
        "function update(node, dt)\n  local U = 1\n  local function W(x) return U * 10 + x end\n  local R, Z\n  R, U, Z = W(1), W(2), W(3)\n  node.x = Z\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "dock".into(),
        enabled: true,
        params: Vec::new(),
        refs: Vec::new(),
        strs: Vec::new(),
        pinned_bools: None,
    }]));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0, 1.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let x = world.get::<Transform>(e).unwrap().translation.x;
    assert_eq!(x, 13.0, "the third value read the local after it was assigned");
    let _ = std::fs::remove_dir_all(&dir);
}
