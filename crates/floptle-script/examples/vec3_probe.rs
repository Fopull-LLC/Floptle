//! Probe: can Luau's native `vector` carry the engine's vec3 method surface?
//!
//! Phase 3 of the Luau migration (ADR-0028) wants a per-project `fast` vec3
//! backed by `mlua::Value::Vector` — immutable f32×3, zero GC — behind the
//! **same** documented surface as today's `exact` f64 userdata. Field reads and
//! arithmetic are VM-native and were never in doubt. The open question was
//! METHODS: `v:length()` needs an `__index` on the vector *type*, and there is
//! no obvious way to reach it.
//!
//! Two things make that question worth a probe rather than an afternoon of
//! reading:
//!
//! * `mlua::Lua::set_type_metatable` looks like the answer and **is not** — it
//!   is bounded by a private `LuaType` trait implemented for `bool`, `Number`,
//!   `String`, `Table`, `Function` and `Thread`, and NOT for `Vector`.
//! * Luau does give vectors a metatable already, with `__index` as a C
//!   function — but it is **readonly**, so the obvious Lua-side
//!   `getmetatable(v).__index = …` raises. That is what step 2 below shows, and
//!   it is the wrong turn this probe exists to save somebody.
//!
//! The route that works is step 2b, and it needs no `unsafe` and no raw FFI:
//! take the metatable from Rust, `set_readonly(false)`, wrap `__index` with a
//! methods table that falls back to Luau's own function, and re-lock it.
//!
//! **Why the fallback is kept — corrected.** The first reading of this probe
//! concluded that the fallback is what resolves `.x`/`.y`/`.z`. It is not: the
//! VM resolves components itself, ahead of any metatable, and they keep working
//! with `__index` deleted outright (step 2c measures exactly that). What the
//! fallback actually preserves is the RAISE on an unknown key — drop it and a
//! misspelled component reads as a silent `nil`. That is still a good reason to
//! wrap rather than replace, but it is a different one, and the guard for it is
//! the `a misspelled component` entry in `math_api`'s divergence list.
//!
//! Run: `cargo run -p floptle-script --example vec3_probe`
//!
//! There is no vector type under `vm-luajit` at all — `mlua::Vector` does not
//! exist to compile against — so the body below is Luau-only and the other
//! build says so and exits. That is also why the whole thing is `#[cfg]`-split
//! rather than branching on [`floptle_script::vm::VM_NAME`] at runtime.

#[cfg(feature = "vm-luajit")]
fn main() {
    println!("VM = {}", floptle_script::vm::VM_NAME);
    println!(
        "\nLuaJIT has no native vector type — `mlua::Vector` is compiled out entirely.\n\
         Nothing here to measure; `fast` vec3 is a Luau-only backing by construction,\n\
         which is itself one of Phase 3's constraints (ADR-0028)."
    );
}

#[cfg(feature = "vm-luau")]
fn main() {
    let lua = mlua::Lua::new();
    println!("VM = {}", floptle_script::vm::VM_NAME);

    println!("\n1. what the VM gives a vector before we touch anything");
    for (label, src) in [
        ("type(vector)", "return type(vector)"),
        ("type(vector.create)", "return type(vector and vector.create)"),
        ("type of a vector value", "return type(vector.create(1,2,3))"),
        (
            "native .x/.y/.z",
            "local v = vector.create(1,2,3) return ('%s,%s,%s'):format(v.x, v.y, v.z)",
        ),
        ("native arithmetic", "return tostring(vector.create(1,2,3) + vector.create(4,5,6))"),
        ("getmetatable(v)", "return type(getmetatable(vector.create(1,2,3)))"),
        (
            "its __index",
            "local mt = getmetatable(vector.create(1,2,3)) return mt and type(mt.__index) or 'no mt'",
        ),
    ] {
        say(label, lua.load(src).eval());
    }

    println!("\n2. the obvious Lua-side route — expected to RAISE");
    say(
        "getmetatable(v).__index = f",
        lua.load(
            "local mt = getmetatable(vector.create(1,2,3))\n\
             mt.__index = function() end\n\
             return 'modified'",
        )
        .eval(),
    );

    println!("\n2b. from Rust: unlock, WRAP, re-lock");
    say("methods + fields + arithmetic", attach_from_rust(&lua));

    println!("\n2c. what the fallback is really for: delete __index entirely");
    say("components / unknown key", without_index(&lua));

    println!("\n3. a Rust-built Vector crossing into Lua");
    say(
        "Value::Vector -> Lua",
        lua.load("return function(v) return ('%s %s'):format(type(v), tostring(v.x)) end")
            .eval::<mlua::Function>()
            .and_then(|f| f.call(mlua::Value::Vector(mlua::Vector::new(7.0, 8.0, 9.0)))),
    );
}

/// An error is a RESULT here, not a crash: two of the questions above are
/// "does this raise?", and the answer is the whole point.
#[cfg(feature = "vm-luau")]
fn say(label: &str, r: mlua::Result<String>) {
    match r {
        Ok(v) => println!("  {label:<34} {v}"),
        Err(e) => {
            let msg = e.to_string();
            println!("  {label:<34} raised: {}", msg.lines().next().unwrap_or(&msg));
        }
    }
}

/// Remove the metatable's `__index` altogether and see what actually stops
/// working. Run last, because it leaves the state without one.
///
/// This is the step that corrected the probe's first conclusion: components
/// still read, and it is the unknown-key ERROR that is lost.
#[cfg(feature = "vm-luau")]
fn without_index(lua: &mlua::Lua) -> mlua::Result<String> {
    let mt: mlua::Table = lua.load("return getmetatable(vector.create(1,2,3))").eval()?;
    mt.set_readonly(false);
    mt.set("__index", mlua::Value::Nil)?;
    mt.set_readonly(true);
    lua.load(
        "local v = vector.create(3, 4, 0)\n\
         local okx, x = pcall(function() return v.x end)\n\
         local okn, n = pcall(function() return v.nope end)\n\
         return ('.x=%s(%s)  .nope=%s(%s)'):format(\n\
           tostring(okx), tostring(x), tostring(okn), tostring(n))",
    )
    .eval()
}

/// The route Phase 3 will actually take, in miniature: one method, installed
/// over Luau's own `__index` rather than in place of it.
#[cfg(feature = "vm-luau")]
fn attach_from_rust(lua: &mlua::Lua) -> mlua::Result<String> {
    let mt: mlua::Table = lua.load("return getmetatable(vector.create(1,2,3))").eval()?;
    println!("  {:<34} {}", "metatable is_readonly", mt.is_readonly());
    mt.set_readonly(false);

    // Keep Luau's own `__index` as the fallback. Not for the components — step
    // 2c shows the VM resolves those without any metatable at all — but for the
    // unknown-key raise: replace this outright and `v.nope` answers nil instead
    // of saying so.
    let previous: Option<mlua::Function> = match mt.get::<mlua::Value>("__index")? {
        mlua::Value::Function(f) => Some(f),
        _ => None,
    };

    let methods = lua.create_table()?;
    methods.set(
        "length",
        lua.create_function(|_, v: mlua::Vector| {
            Ok(glam::DVec3::new(v.x().into(), v.y().into(), v.z().into()).length())
        })?,
    )?;

    mt.set(
        "__index",
        lua.create_function(move |_, (this, key): (mlua::Value, mlua::Value)| {
            if let mlua::Value::String(ref k) = key {
                let found: mlua::Value = methods.get(k.to_str()?.to_owned())?;
                if found != mlua::Value::Nil {
                    return Ok(found);
                }
            }
            match previous {
                Some(ref f) => f.call((this, key)),
                None => Ok(mlua::Value::Nil),
            }
        })?,
    )?;
    mt.set_readonly(true);

    lua.load(
        "local v = vector.create(3, 4, 0)\n\
         local okm, m = pcall(function() return v:length() end)\n\
         local okf, f = pcall(function() return v.x end)\n\
         local oka, a = pcall(function() return tostring(v + vector.create(1, 1, 1)) end)\n\
         return ('method=%s(%s)  field=%s(%s)  arithmetic=%s(%s)')\n\
           :format(tostring(okm), tostring(m), tostring(okf), tostring(f), tostring(oka), tostring(a))",
    )
    .eval()
}
