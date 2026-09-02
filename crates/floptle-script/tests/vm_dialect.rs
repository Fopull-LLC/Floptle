//! The dialect-delta corpus: one case per known difference between the VMs.
//!
//! ADR-0028 moves the engine from LuaJIT to Luau. This file is where every
//! difference anybody finds between them gets written down as an executable
//! statement, and it runs under **both** — `cargo test -p floptle-script
//! --test vm_dialect` and `scripts/vm.sh luau test -p floptle-script --test
//! vm_dialect`.
//!
//! Two rules, and they are what make this file worth having:
//!
//! 1. **Every case asserts in both directions.** A `#[cfg]`-skipped test says
//!    nothing about the VM that skipped it, which is precisely the VM anybody
//!    reading this file is worried about. Where the behaviour genuinely differs,
//!    the difference itself is the assertion, keyed on a constant the engine
//!    already exposes.
//! 2. **Numbers are measured here, not quoted from a doc.** Every "Luau's limit
//!    is N" sentence written anywhere else in this repo should be traceable to a
//!    test below that found N by trying it. The plan this work follows guessed
//!    Luau's upvalue ceiling was 200; it is not — there isn't one.
//!
//! What this file is *not*: a test of the engine's API. That is the ordinary
//! suite, which the harness runs under both VMs. This is only the places where
//! the two Luas themselves disagree.

use mlua::FromLua;

use floptle_script::load_error::UPVALUE_LIMIT;
use floptle_script::vm::VM_NAME;

/// A chunk with `n` file-scope locals and one function closing over all of
/// them, returning their sum. This is the exact shape that cost two releases in
/// `vessel_controller.lua`: one more `local` at the top of a long file.
fn closes_over(n: usize) -> String {
    let mut s = String::new();
    for i in 0..n {
        s.push_str(&format!("local v{i} = {i}\n"));
    }
    s.push_str("local function f()\n  local t = 0\n");
    for i in 0..n {
        s.push_str(&format!("  t = t + v{i}\n"));
    }
    s.push_str("  return t\nend\nreturn f()\n");
    s
}

/// The sum `closes_over(n)` must produce if it ran at all — 0 + 1 + … + (n-1).
fn expected_sum(n: usize) -> i64 {
    (n * n.saturating_sub(1) / 2) as i64
}

/// Run a chunk, and say whether it produced the right answer or refused.
fn eval_sum(lua: &mlua::Lua, n: usize) -> Result<i64, String> {
    match lua.load(closes_over(n)).eval::<mlua::Value>() {
        // `as_i64` / `as_f64` are each strict about the variant, and the two VMs
        // return this sum as different variants — so ask for the conversion.
        Ok(v) => f64::from_lua(v.clone(), lua)
            .map(|f| f as i64)
            .map_err(|_| format!("returned {v:?}, which is not a number")),
        Err(e) => Err(e.to_string().lines().next().unwrap_or("").to_string()),
    }
}

/// **The upvalue ceiling is whatever this VM actually enforces**, and
/// [`UPVALUE_LIMIT`] is the engine's claim about it.
///
/// This is the test that sets that constant. LuaJIT refuses a function closing
/// over more than 60 file-scope locals; Luau refuses nothing here, and returns
/// the right sum at every size tried. The engine warns, squiggles and rewrites
/// the loader's error off `UPVALUE_LIMIT`, so a wrong number there is a warning
/// about a wall in the wrong place — or, worse, silence in front of a real one.
#[test]
fn the_engines_upvalue_ceiling_is_the_one_the_vm_enforces() {
    let lua = mlua::Lua::new();

    // Under any VM, a small function closing over a handful of locals runs.
    assert_eq!(eval_sum(&lua, 10), Ok(expected_sum(10)), "[{VM_NAME}] ten upvalues");

    match UPVALUE_LIMIT {
        Some(limit) => {
            assert_eq!(
                eval_sum(&lua, limit),
                Ok(expected_sum(limit)),
                "[{VM_NAME}] exactly {limit} upvalues is still legal — an off-by-one here \
                 makes every warning message one out"
            );
            let over = eval_sum(&lua, limit + 1);
            let err = over.expect_err(&format!(
                "[{VM_NAME}] {} upvalues must be refused — UPVALUE_LIMIT says {limit}, and the \
                 engine warns, lints and explains off that number",
                limit + 1
            ));
            assert!(
                err.contains("upvalues"),
                "[{VM_NAME}] the refusal must be the upvalue one, not some other error: {err}"
            );
        }
        None => {
            // No ceiling claimed. Prove it well past anywhere a limit would be:
            // 200 is what the plan for this work guessed Luau's limit was, 255
            // is the usual C-side maximum, and 4096 is far past any script.
            for n in [61, 200, 255, 256, 1000, 4096] {
                assert_eq!(
                    eval_sum(&lua, n),
                    Ok(expected_sum(n)),
                    "[{VM_NAME}] UPVALUE_LIMIT is None, so {n} upvalues must compile AND run — \
                     compiling alone would not prove it, since a truncated closure still compiles"
                );
            }
        }
    }
}

/// The second wall, and the one nothing in the engine ever named.
///
/// LuaJIT stops at 200 locals in one scope as well as 60 upvalues in one
/// function, and `explain` has a branch for it. Luau has neither. Recorded
/// because a script that trips this one gets a *different* message, and the
/// difference is invisible until somebody writes a very long function.
#[test]
fn the_local_variable_wall_is_lower_than_it_looks_on_luajit() {
    let lua = mlua::Lua::new();
    let mut src = String::new();
    for i in 0..254 {
        src.push_str(&format!("local v{i} = {i}\n"));
    }
    src.push_str("return v253\n");
    let got = lua.load(src).eval::<mlua::Value>();

    if UPVALUE_LIMIT.is_some() {
        let err = got.expect_err("[luajit] 254 locals in one scope is past the 200 limit");
        let err = err.to_string();
        assert!(
            err.contains("local variables") || err.contains("main function has more than"),
            "[{VM_NAME}] a different refusal than expected: {err}"
        );
    } else {
        let v = got.unwrap_or_else(|e| panic!("[{VM_NAME}] 254 locals must be fine: {e}"));
        assert_eq!(v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)), Some(253));
    }
}

/// **A Lua integer is not always 64 bits**, and the engine has to know it.
///
/// `mlua::Integer` is `i64` under LuaJIT and `i32` under Luau, because Luau's
/// `lua_Integer` is a C `int`. Anything the engine converts through it that can
/// exceed 2^31 has to be carried some other way — `f64` holds every `u32`
/// exactly and is the same on both VMs, which is what `api::tile_cell` uses
/// after `cell = 4294967295` (the engine's own "empty square" constant) was
/// found saturating to 2147483647 and painting that tile instead.
#[test]
fn the_width_of_a_lua_integer_is_a_property_of_the_vm() {
    let lua = mlua::Lua::new();
    let big: mlua::Value = lua.load("return 4294967295").eval().expect("a whole number");

    // Whatever it arrives as, the value converts to an f64 on both VMs. That is
    // the property the engine relies on. (`Value::as_f64` is strict — it answers
    // `None` for an `Integer` — so the conversion is asked for, not inspected.)
    let as_f64: f64 = lua.load("return 4294967295").eval().expect("converts to f64");
    assert_eq!(as_f64, 4294967295.0, "[{VM_NAME}] u32::MAX must survive the trip");

    // And it is representable as a Lua integer on exactly the VMs whose integer
    // is wide enough — which is the fact worth pinning, because the tempting
    // `as mlua::Integer` cast is a SATURATION, not a failure, and says nothing.
    // Widened to `i128` so this is a real cast under both VMs — `i64::from` on
    // an `i64` is a no-op the linter rightly objects to.
    let fits = mlua::Integer::MAX as i128 >= 4294967295;
    assert_eq!(
        matches!(big, mlua::Value::Integer(_)),
        fits,
        "[{VM_NAME}] mlua::Integer::MAX is {}, so u32::MAX {} arrive as an Integer",
        mlua::Integer::MAX,
        if fits { "should" } else { "should not" }
    );
}

/// `setfenv` / `getfenv` survive into Luau, and the engine's sandbox leans on
/// them. Asserted rather than believed: the plan for this work said "it KEEPS
/// `setfenv`/`getfenv` (5.1 heritage — verify in the harness, don't trust this
/// sentence)", and this is that verification.
#[test]
fn the_five_one_environment_functions_are_still_there() {
    let lua = mlua::Lua::new();
    for name in ["setfenv", "getfenv", "unpack", "newproxy"] {
        let present: bool = lua
            .load(format!("return type({name}) == 'function'"))
            .eval()
            .unwrap_or(false);
        assert!(present, "[{VM_NAME}] `{name}` is missing, and the host relies on 5.1 shape");
    }
}

/// Luau drops `io` and most of `os`, and that is *correct* for a shipped game —
/// but the engine must not be relying on either.
///
/// Stated as a difference rather than a requirement: the assertion is that the
/// two VMs disagree here in the known direction, so that a future Luau build
/// which quietly gained `io` (a sandbox regression — a shipped game must not be
/// able to open files) fails this test rather than passing silently.
#[test]
fn a_shipped_game_cannot_reach_the_filesystem_through_luau() {
    let lua = mlua::Lua::new();
    let has_io: bool = lua.load("return type(io) == 'table'").eval().unwrap_or(false);
    if UPVALUE_LIMIT.is_some() {
        assert!(has_io, "[{VM_NAME}] LuaJIT ships `io`; the dev host's test scripts use it");
    } else {
        assert!(
            !has_io,
            "[{VM_NAME}] Luau must NOT expose `io` — a game script that can open a file is a \
             sandbox hole, and six dev-only test scripts were ported off it rather than \
             shimming it back in"
        );
    }
}

/// **The standard library each VM actually offers**, recorded rather than
/// remembered.
///
/// This is the table Phase 2 has to act on: the day Luau becomes the default,
/// every `nil` in its column that the docs promise becomes a wrong sentence in
/// a shipped guide, and every name a script uses becomes a crash. Recording it
/// as an assertion means the list cannot quietly go stale between now and then
/// — a Luau upgrade that adds or drops one fails here, loudly, instead of being
/// discovered by a game.
///
/// Every entry was measured on this machine, both VMs, at the time of writing.
/// Nothing here is quoted from Luau's documentation.
///
/// The good news it records: **no first-party `.lua` file in this repository
/// uses any name that differs** — not `bit.*`, not `loadstring`, not `goto`,
/// not `debug.getinfo`, not `os.getenv`. The migration surface for shipped
/// scripts really is empty, which is the claim the whole plan rests on.
#[test]
fn the_standard_library_is_the_one_this_vm_actually_has() {
    let lua = mlua::Lua::new();
    let kind = |expr: &str| -> String {
        lua.load(format!(
            "local ok, v = pcall(function() return {expr} end) \
             if not ok then return 'raises' end return type(v)"
        ))
        .eval()
        .unwrap_or_else(|e| format!("<{e}>"))
    };

    // (name, what LuaJIT has, what Luau has)
    let table = [
        // Bit twiddling moved house. `editor-scripting.md` names `bit` today.
        ("bit", "table", "nil"),
        ("bit32", "nil", "table"),
        // Runtime code loading is gone from Luau ENTIRELY — by design, since a
        // sandbox that can compile a string is not a sandbox.
        ("loadstring", "function", "nil"),
        ("load", "function", "nil"),
        ("string.dump", "function", "nil"),
        // The environment is not a game's to read.
        ("os.getenv", "function", "nil"),
        // …but time is, and both keep it.
        ("os.time", "function", "function"),
        ("os.clock", "function", "function"),
        ("os.date", "function", "function"),
        // `debug` is reduced, and this is the pair the Console cares about:
        // a traceback survives, `getinfo` does not.
        ("debug.traceback", "raises", "function"),
        ("debug.getinfo", "raises", "nil"),
        // 5.1 shape both sides.
        ("coroutine", "table", "table"),
        ("select", "function", "function"),
        ("table.getn", "function", "function"),
        ("table.move", "function", "function"),
        ("math.atan2", "function", "function"),
        ("math.fmod", "function", "function"),
        ("math.pow", "function", "function"),
        ("collectgarbage", "function", "function"),
        // What Luau ADDS, and Phase 3 spends: a native vector type.
        ("vector", "nil", "table"),
        ("buffer", "nil", "table"),
        ("utf8", "nil", "table"),
        ("rawlen", "nil", "function"),
        ("table.create", "nil", "function"),
        ("table.find", "nil", "function"),
        ("string.split", "nil", "function"),
        ("math.round", "nil", "function"),
    ];

    let luau = UPVALUE_LIMIT.is_none();
    let mut wrong = Vec::new();
    for (name, on_luajit, on_luau) in table {
        let want = if luau { on_luau } else { on_luajit };
        let got = kind(name);
        if got != want {
            wrong.push(format!("  {name}: recorded {want}, found {got}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "[{VM_NAME}] the recorded standard library no longer matches this VM:\n{}\n\n\
         Update the table with what was MEASURED, and check whether a doc page or a shipped \
         script promised the name that moved.",
        wrong.join("\n")
    );
}

/// **`goto` is a LuaJIT extension and Luau does not have it.**
///
/// Worth its own case because `docs/tutorials/for-programmers.md` tells the
/// reader the runtime is "5.1 semantics plus `goto`" — a promise that stops
/// being true when the default flips, and one no compile error would catch,
/// because it is a sentence in a guide rather than a call in the API.
///
/// Compiled directly rather than through `loadstring`: Luau has no
/// `loadstring`, so routing the check through it would report "no goto" for
/// entirely the wrong reason. That mistake was made once while writing this.
#[test]
fn goto_is_a_luajit_extension_and_the_docs_promise_it() {
    let lua = mlua::Lua::new();
    let compiles = lua
        .load("for i = 1, 2 do goto cont ::cont:: end return 1")
        .into_function()
        .is_ok();
    assert_eq!(
        compiles,
        UPVALUE_LIMIT.is_some(),
        "[{VM_NAME}] `goto` compiling here disagrees with what the docs say this VM is. \
         If Luau has gained `goto`, say so in docs/tutorials/for-programmers.md; if LuaJIT \
         has lost it, something is very wrong."
    );
}

/// **The `bit` library a script sees answers the same numbers on both VMs.**
///
/// `docs/editor-scripting.md` promises third-party packages a bit library, and
/// a game script has the whole standard library in scope — so `bit.band` is
/// reachable from code this project did not write and cannot edit. Luau spells
/// it `bit32`, and `vm::install_compat` rebuilds `bit` on top of it.
///
/// The reason this is a values test and not a presence test: `bit32` answers
/// **unsigned** where LuaJIT's `bit` answers **signed 32-bit**. Aliasing one to
/// the other would leave every call working, every test passing, and a
/// package's hash quietly computing a different number on the other VM. So each
/// case below is a number, and the expected number is the same on both sides.
#[test]
fn the_bit_library_answers_the_same_numbers_on_both_vms() {
    let lua = mlua::Lua::new();
    floptle_script::vm::install_compat(&lua).expect("compat layer installs");

    // (expression, what LuaJIT's `bit` answers)
    for (expr, want) in [
        ("bit.band(0xF0, 0x3C)", "48"),
        ("bit.tobit(0xFFFFFFFF)", "-1"),
        ("bit.bnot(0)", "-1"),
        ("bit.bor(1, 2, 4)", "7"),
        ("bit.bxor(0xFF, 0x0F)", "240"),
        ("bit.lshift(1, 31)", "-2147483648"),
        ("bit.rshift(-1, 28)", "15"),
        ("bit.arshift(-16, 2)", "-4"),
        ("bit.rol(0x12345678, 8)", "878082066"),
        ("bit.bswap(0x12345678)", "2018915346"),
        ("bit.tohex(255)", "000000ff"),
        ("bit.tohex(255, 4)", "00ff"),
        ("bit.tohex(255, -8)", "000000FF"),
    ] {
        let got: String = lua
            .load(format!("return tostring({expr})"))
            .eval()
            .unwrap_or_else(|e| panic!("[{VM_NAME}] {expr} raised: {e}"));
        assert_eq!(
            got, want,
            "[{VM_NAME}] {expr} — the two VMs must agree on the VALUE, not just on the \
             function existing. A package hashing a scene gets this number."
        );
    }
}

/// **Luau's runtime errors do not name the thing that was nil, and LuaJIT's
/// do.** This is the largest user-visible regression the port has found, and it
/// is recorded here rather than absorbed.
///
/// `node.postion.x` — one transposed letter, the commonest mistake anybody
/// makes in a game script — reports as `attempt to index field 'postion'` on
/// LuaJIT and as `attempt to index nil with 'x'` on Luau. The second says
/// nothing a reader can act on, and it is the **same sentence** a missing
/// global and a nil local produce, so three different bugs are one message.
///
/// The engine already rewrote *load* errors into its own voice
/// (`load_error::explain`, `floptle/0086`), for exactly this reason. This
/// finding is what `runtime_error::explain` was built for, and
/// `a_typo_in_a_script_names_the_typo_whichever_vm_is_running` below is the
/// closing half: what a script's author actually reads is now identical on both
/// VMs, and names the typo neither VM names on its own.
///
/// This test stays because it pins the raw material that rewrite is built on.
/// It does not judge which VM is better — it records what each says underneath,
/// so a Luau upgrade that improves these messages is noticed rather than
/// silently making the rewrite unnecessary, and so the rewrite is never
/// rebuilt against a phrasing that has moved.
#[test]
fn what_a_runtime_error_names_is_a_property_of_the_vm() {
    let lua = mlua::Lua::new();
    let first_line = |src: &str| -> String {
        lua.load(src)
            .set_name("probe")
            .exec()
            .expect_err("must raise")
            .to_string()
            .lines()
            .next()
            .unwrap_or("")
            .to_string()
    };

    // (source, what LuaJIT names, what Luau names)
    let cases = [
        ("local node = {}\nreturn node.postion.x\n", "field 'postion'", "index nil with 'x'"),
        ("return missingGlobal.x\n", "global 'missingGlobal'", "index nil with 'x'"),
        ("local t\nreturn t.x\n", "local 't'", "index nil with 'x'"),
        ("local node = {}\nreturn node.speed + 1\n", "field 'speed'", "on nil and number"),
        ("local node = {}\nreturn node.mve()\n", "call field 'mve'", "call a nil value"),
    ];

    let luau = UPVALUE_LIMIT.is_none();
    for (src, on_luajit, on_luau) in cases {
        let want = if luau { on_luau } else { on_luajit };
        let got = first_line(src);
        assert!(
            got.contains(want),
            "[{VM_NAME}] expected the message to contain {want:?}, got:\n  {got}\n\n\
             If this VM's messages have changed, update the recorded text — and check whether \
             a host-side rewrite is now needed, or no longer is."
        );
        // Whatever else it does, it must always say WHERE.
        assert!(got.contains(":2:") || got.contains(":1:"), "[{VM_NAME}] no line number: {got}");
    }

    // The part that makes this a regression rather than a difference: on Luau
    // these three distinct mistakes are one indistinguishable sentence.
    if luau {
        let a = first_line("local node = {}\nreturn node.postion.x\n");
        let b = first_line("return missingGlobal.x\n");
        let c = first_line("local t\nreturn t.x\n");
        let strip = |s: &str| s.split("]:").nth(1).unwrap_or(s)[1..].to_string();
        assert_eq!(strip(&a), strip(&c), "[{VM_NAME}] a typo and a nil local read alike");
        assert_eq!(
            strip(&b),
            strip(&c),
            "[{VM_NAME}] a missing global and a nil local read alike — this is the finding"
        );
    }
}

/// **End to end: a real script, a real host, and the SAME message either way.**
///
/// The unit tests in `runtime_error` prove the rewrite; this proves it is
/// actually reached, from a script on disk through `ScriptHost::run` to the
/// string a Console line carries. It is the case that matters, because the
/// whole finding was that a player-visible message changed with the VM.
///
/// Asserted as content rather than as an exact string on purpose: the VM's own
/// first line still differs (it is kept, deliberately — it is what a reader
/// searches for), and what has to converge is the sentence the engine adds.
#[test]
fn a_typo_in_a_script_names_the_typo_whichever_vm_is_running() {
    let dir = std::env::temp_dir().join(format!("floptle_vm_rt_err_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(
        dir.join("mover.lua"),
        "function update(node, dt)\n  node.y = node.postion.x\nend\n",
    )
    .expect("write the script");

    let mut world = floptle_core::World::default();
    let e = world.spawn();
    world.insert(e, floptle_core::transform::Transform::IDENTITY);
    world.insert(
        e,
        floptle_core::Scripts(vec![floptle_core::ScriptInst {
            kind: "mover".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );

    let mut host = floptle_script::ScriptHost::new();
    host.run(&mut world, &dir, 0.1, 0.1);

    let errs = host.errors().to_vec();
    let msg = errs
        .iter()
        .find(|e| e.contains("mover"))
        .unwrap_or_else(|| panic!("[{VM_NAME}] the runtime error must be reported: {errs:?}"));

    // The finding, closed: the typo is named, on both VMs.
    assert!(
        msg.contains("`node.postion` is nil"),
        "[{VM_NAME}] the message must name what was nil, not what was read from it:\n{msg}"
    );
    assert!(
        msg.contains("`.x` was read from it"),
        "[{VM_NAME}] …and the other half too:\n{msg}"
    );
    // …and the offending statement is quoted, so nobody has to go and look.
    assert!(
        msg.contains("node.y = node.postion.x"),
        "[{VM_NAME}] the source line must be quoted:\n{msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
