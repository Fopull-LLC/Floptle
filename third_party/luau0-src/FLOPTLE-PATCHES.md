# luau0-src, patched for Floptle

This is `luau0-src 0.12.3+luau663` from crates.io (MIT; Luau is MIT too, see
`LICENSE`), vendored so the engine can carry a fix to Luau's compiler. The
workspace's `[patch.crates-io]` points mlua at this copy. Nothing else differs
from the published crate.

## The patch

`luau/Compiler/src/Compiler.cpp`, `resolveAssignConflicts`. Every change is
marked `FLOPTLE PATCH`.

In a multiple assignment, PUC Lua and LuaJIT evaluate every value on the right
before they assign any target. Luau evaluates a value straight into its local's
register, and it checks for a later value reading that local only syntactically.
So a function on the right that reads the local through a closure sees the new
value:

```lua
local U = 1
local function W(x) return U * 10 + x end
local R, Z
R, U, Z = W(1), W(2), W(3)   -- Luau: 11 12 123, Lua and LuaJIT: 11 12 13
```

Luau's own comment cites the manual's latitude for this: the order is
guaranteed only for accesses inside the statement itself. Scripts written
against Lua's behaviour still break, silently. Solar's docking port axis came
out 8.8° off.

The patch handles it like this. Once any local has been assigned, a later value
that can run code (anything but a local, a constant, a function literal or
`...`) first marks every assigned local as conflicting. Those locals are then
assigned through a temporary after all the values are in. The cost is one
`MOVE` per local target, and only in multiple assignments.

## A second, small patch: `build.rs`

The published crate has no build script. Cargo therefore fingerprints it by its
Rust sources only, so an edit to the C++ under `luau/` silently rebuilt nothing:
the old compiler stayed linked. `build.rs` watches `luau/`, and `Cargo.toml`
names it (`build = "build.rs"`). Keep both when re-vendoring.

## Upgrading mlua or Luau

Luau 745 still has this. When you upgrade:
1. Re-vendor the new `luau0-src`.
2. Re-apply the `FLOPTLE PATCH` blocks and `build.rs`.
3. Keep the `[patch.crates-io]` version in step.

The guard `a_multiple_assignment_evaluates_every_value_before_assigning` in
`floptle-script` fails if the patch is lost.
