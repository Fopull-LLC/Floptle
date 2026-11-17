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

/// Every way a script or the engine can write a table, each of which must
/// quietly disarm an armed table rather than raise — see the third patch in
/// FLOPTLE-PATCHES.md. `t` is the armed table, `{a = 1, 10, 20, 30}`; each
/// case runs inside a loop so native code generation, when it is built,
/// compiles it rather than leaving it to the interpreter.
const ARMED_WRITES: &[(&str, &str)] = &[
    ("an existing field", "t.a = 2"),
    ("an existing field, same value", "t.a = 1"),
    ("a new field", "t.fresh = true"),
    ("a computed key", "local k = 'a' .. '' t[k] = 3"),
    ("an existing array slot", "t[2] = 5"),
    ("a new array slot", "t[4] = 40"),
    ("a slot by a constant index", "t[1] = 0"),
    ("a compound assignment", "t.a += 1"),
    ("clearing a field", "t.a = nil"),
    ("rawset", "rawset(t, 'a', 9)"),
    ("rawset of a new key", "rawset(t, 'other', 9)"),
    ("table.insert at the end", "table.insert(t, 99)"),
    ("table.insert at a position", "table.insert(t, 1, 99)"),
    ("table.remove", "table.remove(t)"),
    ("table.remove at a position", "table.remove(t, 1)"),
    ("table.sort", "table.sort(t, function(a, b) return a > b end)"),
    ("table.clear", "table.clear(t)"),
    ("table.move into it", "table.move({7, 8}, 1, 2, 1, t)"),
    ("table.move within it", "table.move(t, 1, 2, 2)"),
    ("setmetatable", "setmetatable(t, {})"),
    ("setmetatable to nil", "setmetatable(t, nil)"),
    ("a write from a coroutine", "coroutine.wrap(function() t.a = 4 end)()"),
    ("a write from a yielded coroutine", "local co = coroutine.create(function() coroutine.yield() t.a = 4 end) coroutine.resume(co) coroutine.resume(co)"),
    ("a write under pcall", "pcall(function() t.a = 5 end)"),
    ("a write in a closure", "local function f(x) x.a = 6 end f(t)"),
];

/// Reads, each of which must leave the table armed.
const ARMED_READS: &[(&str, &str)] = &[
    ("a field read", "local _ = t.a"),
    ("an absent field read", "local _ = t.nothing"),
    ("an array read", "local _ = t[1]"),
    ("rawget", "local _ = rawget(t, 'a')"),
    ("pairs", "for _ in pairs(t) do end"),
    ("ipairs", "for _ in ipairs(t) do end"),
    ("generalised iteration", "for _ in t do end"),
    ("next", "local _ = next(t)"),
    ("the length", "local _ = #t"),
    ("table.clone", "local c = table.clone(t) c.a = 7"),
    ("table.concat", "local _ = table.concat(t, ',')"),
    ("table.find", "local _ = table.find(t, 20)"),
    ("table.unpack", "local _ = table.unpack(t)"),
    ("getmetatable", "local _ = getmetatable(t)"),
    ("tostring", "local _ = tostring(t)"),
    ("type", "local _ = type(t)"),
    ("table.isfrozen", "assert(table.isfrozen(t) == false, 'an armed table read as frozen')"),
    ("a write to another table", "local u = {} u.a = 1 rawset(u, 'b', 2) setmetatable(u, {})"),
];

fn armed_table(lua: &mlua::Lua) -> mlua::Table {
    let t: mlua::Table = lua.load("return {a = 1, 10, 20, 30}").eval().unwrap();
    crate::env::arm(&t);
    assert!(crate::env::is_armed(&t), "a fresh table did not arm");
    lua.globals().raw_set("t", &t).unwrap();
    t
}

/// **A write of any kind disarms an armed table, and no read does.** The
/// engine arms each kept `node` table after it stamps it, and takes "still
/// armed" to mean nothing has written to it since, so it can hand the table
/// back without reading it. A write that slipped past the patch would be a
/// write the engine never drains; one that raised would break a script that
/// works today. Every case at every optimisation level, since fast calls and
/// inline caches each take their own path to a table write.
#[test]
fn every_write_disarms_an_armed_table_and_no_read_does() {
    let mut wrong = Vec::new();
    for level in 0..=2 {
        for (what, src) in ARMED_WRITES {
            let lua = mlua::Lua::new();
            lua.set_compiler(mlua::Compiler::new().set_optimization_level(level));
            let t = armed_table(&lua);
            match lua.load(format!("for _ = 1, 1 do {src} end")).exec() {
                Err(e) => wrong.push(format!("{what} at level {level} raised: {e}")),
                Ok(()) if crate::env::is_armed(&t) => wrong.push(format!("{what} at level {level} left the table armed")),
                Ok(()) => {}
            }
        }
        for (what, src) in ARMED_READS {
            let lua = mlua::Lua::new();
            lua.set_compiler(mlua::Compiler::new().set_optimization_level(level));
            let t = armed_table(&lua);
            match lua.load(format!("for _ = 1, 1 do {src} end")).exec() {
                Err(e) => wrong.push(format!("{what} at level {level} raised: {e}")),
                Ok(()) if !crate::env::is_armed(&t) => wrong.push(format!("{what} at level {level} disarmed the table")),
                Ok(()) => {}
            }
        }
    }
    // The engine's own writes, from Rust.
    type HostWrite = fn(&mlua::Lua, &mlua::Table) -> mlua::Result<()>;
    let host: &[(&str, HostWrite)] = &[
        ("Table::raw_set", |_, t| t.raw_set("a", 2)),
        ("Table::set", |_, t| t.set("a", 2)),
        ("Table::raw_insert", |_, t| t.raw_insert(1, 5)),
        ("Table::raw_remove", |_, t| t.raw_remove(1)),
        ("Table::clear", |_, t| t.clear()),
        ("Table::set_metatable", |lua, t| {
            t.set_metatable(Some(lua.create_table()?));
            Ok(())
        }),
    ];
    for (what, write) in host {
        let lua = mlua::Lua::new();
        let t = armed_table(&lua);
        assert!(!t.is_readonly(), "an armed table read as readonly to the host");
        match write(&lua, &t) {
            Err(e) => wrong.push(format!("{what} raised: {e}")),
            Ok(()) if crate::env::is_armed(&t) => wrong.push(format!("{what} left the table armed")),
            Ok(()) => {}
        }
    }
    // A write through a Rust `__newindex` that sets the key raw — the node
    // metatable's road for `node.pos = v`. The metatable is set before the
    // table is armed, as the engine's is.
    {
        let lua = mlua::Lua::new();
        let t: mlua::Table = lua.load("return {}").eval().unwrap();
        let mt = lua.create_table().unwrap();
        mt.raw_set(
            "__newindex",
            lua.create_function(|_, (t, _k, v): (mlua::Table, String, f64)| t.raw_set("x", v)).unwrap(),
        )
        .unwrap();
        t.set_metatable(Some(mt));
        crate::env::arm(&t);
        lua.globals().raw_set("t", &t).unwrap();
        match lua.load("for _ = 1, 1 do t.pos = 3 end").exec() {
            Err(e) => wrong.push(format!("a Rust __newindex that sets raw raised: {e}")),
            Ok(()) if crate::env::is_armed(&t) => wrong.push("a Rust __newindex that sets raw left the table armed".into()),
            Ok(()) => {}
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// **Freezing and arming keep their meanings apart.** `table.freeze` on an
/// armed table freezes it (and a write then raises, as on any frozen table);
/// arming a frozen table leaves it frozen; and an armed table reads as not
/// frozen to `table.isfrozen` and to the host.
#[test]
fn freezing_and_arming_a_table_keep_their_meanings() {
    let lua = mlua::Lua::new();
    let t = armed_table(&lua);
    let (before, after): (bool, bool) =
        lua.load("local b = table.isfrozen(t) table.freeze(t) return b, table.isfrozen(t)").eval().unwrap();
    assert!(!before && after, "table.freeze on an armed table: frozen before {before}, after {after}");
    assert!(!crate::env::is_armed(&t), "a frozen table still reads as armed");
    assert!(t.is_readonly(), "a frozen table does not read as readonly to the host");
    assert!(lua.load("t.a = 2").exec().is_err(), "a write to a table frozen while armed went through");
    crate::env::arm(&t);
    assert!(!crate::env::is_armed(&t) && t.is_readonly(), "arming a frozen table unfroze it");
    assert!(lua.load("t.a = 2").exec().is_err(), "a write to a frozen table went through after arming it");
    // Armed again after a write disarmed it, it works the same way again.
    let u = armed_table(&lua);
    lua.load("t.a = 2").exec().unwrap();
    assert!(!crate::env::is_armed(&u));
    crate::env::arm(&u);
    assert!(crate::env::is_armed(&u), "a table did not arm a second time");
    lua.load("t.b = 3").exec().unwrap();
    assert!(!crate::env::is_armed(&u), "a table armed a second time missed a write");
}
