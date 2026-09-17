//! Which Lua the engine runs: Luau, everywhere. Desktop and browser share one
//! dialect to document and one set of behaviour to test, and Luau's collector
//! is the faster one on real games (Solar's system scene 4.81 → 4.15 ms, a
//! first-person scene 13.99 → 12.38 ms, frame p95; `scripts/scene-bench.sh`).
//!
//! There is no LuaJIT build: it exposed `io` to every script it ran. What
//! remains of the switch is the `vm-luau` feature name, which every dependent
//! forwards, and the checks below.
//!
//! `mlua` links exactly one Lua. Cargo features are additive, so a crate that
//! depends on this one and names a second `mlua/...` VM feature, or a browser
//! build that reaches `mlua/luau` through a target-specific dependency while
//! the desktop switch says something else, puts two Luas in the graph.
//! `mlua-sys`'s build script then fails before this crate compiles at all:
//!
//! ```text
//! error: You can enable only one of the features: lua54, lua53, lua52, lua51, luajit, luajit52, luau
//! ```
//!
//! It says the same for no VM as for two, and names none of the features you
//! wrote. The `compile_error!` below states the invariant as a backstop; the
//! guard that fires first is `tests/vm_wiring.rs`, which reads every crate
//! manifest in the workspace and fails on a dependency that would bring a
//! second VM in. `scripts/vm.sh luau <cargo args>` is the short way to say the
//! whole thing.

#[cfg(not(feature = "vm-luau"))]
compile_error!(
    "floptle-script: no script VM selected. Enable `vm-luau` (the default — Luau,\n\
     and the only VM the engine embeds since v0.89.0).\n\
     `--no-default-features` on its own leaves the engine with no Lua at all."
);

/// The name of the VM this build embeds, for logs, version output and the
/// diff harness's report headers.
///
/// A string rather than an enum on purpose: every consumer of it is printing
/// it, and a `match` over two variants that both stringify is ceremony. It is
/// lowercase and stable — the harness keys its per-VM snapshots on it.
pub const VM_NAME: &str = "luau";

/// Whether this build has a code generator behind the interpreter.
///
/// Luau does only with `vm-luau-codegen`, and never on wasm. Reported rather
/// than acted on: it is the first thing to check when a benchmark comes back
/// slower than the last one.
pub const VM_HAS_CODEGEN: bool = cfg!(all(feature = "vm-luau-codegen", not(target_arch = "wasm32")));

/// Fill in what Luau is missing so the documented Lua surface is the same
/// here. Call it on a fresh state before anything else touches the globals.
///
/// `bit`: [`editor-scripting.md`] promises packages a bit library, and a game
/// script has the whole standard library in scope. Luau calls it `bit32`, with
/// the same operations and different results: `bit32.bnot(0)` is 4294967295
/// where `bit.bnot(0)` is -1, signed 32-bit. A plain alias would change a
/// package's hash, so every result goes back through `tobit`.
///
/// Not shimmed, because a stand-in would be worse than the absence:
/// `loadstring`/`load` (a sandbox that compiles a string is not one),
/// `io`/`os.getenv` (a shipped game must not reach the machine), `goto`
/// (syntax; no library can add it), and `debug.getinfo`. `tests/vm_dialect.rs`
/// records each one.
///
/// [`editor-scripting.md`]: https://github.com/Fopull-LLC/Floptle/blob/main/docs/editor-scripting.md
pub fn install_compat(lua: &mlua::Lua) -> mlua::Result<()> {
    lua.load(BIT_COMPAT_LUA).set_name("=[floptle vm compat]").exec()
}

/// The `bit` library, rebuilt on `bit32` with LuaJIT's signed results.
///
/// Public so a test can read the same source the host runs, rather than a copy
/// of it that could drift.
pub const BIT_COMPAT_LUA: &str = r#"
-- LuaJIT's `bit`, on top of Luau's `bit32`.
--
-- The whole point is the LAST step of each function: bit32 works in unsigned
-- 32-bit and LuaJIT's bit works in signed 32-bit, so `bnot(0)` is 4294967295
-- there and -1 here. Every result is normalised, or a package's hash changes
-- value across a VM it never asked about.
local b32 = bit32
local function tobit(x)
  x = b32.band(x, 0xFFFFFFFF)
  if x >= 0x80000000 then x = x - 0x100000000 end
  return x
end
local function wrap1(f) return function(a) return tobit(f(a)) end end
local function wrapv(f) return function(...) return tobit(f(...)) end end
local function wrapshift(f) return function(a, n) return tobit(f(b32.band(a, 0xFFFFFFFF), n)) end end

bit = {
  tobit   = tobit,
  bnot    = wrap1(b32.bnot),
  band    = wrapv(b32.band),
  bor     = wrapv(b32.bor),
  bxor    = wrapv(b32.bxor),
  lshift  = wrapshift(b32.lshift),
  rshift  = wrapshift(b32.rshift),
  arshift = wrapshift(b32.arshift),
  rol     = wrapshift(b32.lrotate),
  ror     = wrapshift(b32.rrotate),
  tohex   = function(x, n)
    -- LuaJIT: default 8 digits, lower case; a negative n means upper case.
    n = n or 8
    local upper = false
    if n < 0 then upper = true; n = -n end
    local s = string.format("%0" .. n .. "x", b32.band(x, 0xFFFFFFFF))
    if #s > n then s = s:sub(#s - n + 1) end
    return upper and s:upper() or s
  end,
  bswap = function(x)
    x = b32.band(x, 0xFFFFFFFF)
    return tobit(b32.bor(
      b32.lshift(b32.band(x, 0xFF), 24),
      b32.lshift(b32.band(x, 0xFF00), 8),
      b32.rshift(b32.band(x, 0xFF0000), 8),
      b32.rshift(b32.band(x, 0xFF000000), 24)
    ))
  end,
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_knows_which_vm_it_embeds() {
        assert_eq!(VM_NAME, "luau");
        assert!(VM_NAME.chars().all(|c| c.is_ascii_lowercase()), "{VM_NAME} is a snapshot key");
    }

    /// The compatibility layer installs on a bare state without raising.
    ///
    /// Cheap, and it is the failure that would be worst: `ScriptHost::new`
    /// panics on it deliberately, because a shipped script silently losing a
    /// documented library is the outcome this migration promised not to
    /// produce. Better to find it here.
    #[test]
    fn the_compatibility_layer_installs_on_a_fresh_state() {
        let lua = mlua::Lua::new();
        install_compat(&lua).expect("compat layer installs");
        let kind: String = lua
            .load("return type(bit) .. ' ' .. type(bit and bit.band)")
            .eval()
            .expect("read it back");
        assert_eq!(kind, "table function", "[{}] `bit` must be there either way", VM_NAME);
    }
}
