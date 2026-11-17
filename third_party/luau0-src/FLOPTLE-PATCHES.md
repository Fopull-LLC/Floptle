# luau0-src, patched for Floptle

This is `luau0-src 0.12.3+luau663` from crates.io (MIT; Luau is MIT too, see
`LICENSE`), vendored so the engine can carry a fix to Luau's compiler and a small
addition to its VM. The workspace's `[patch.crates-io]` points mlua at this
copy. Nothing else differs from the published crate.

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

## A third patch: armed tables

`luau/VM`. Every change is marked `FLOPTLE PATCH`.

The engine keeps one `node` table per script instance, and before each pass
it has to know whether anything wrote to that table since it last stamped the
transform into it: a script may keep the table (`me = node`) and write it from
a timer or another script's call, and that write must reach the world. Reading
the table back to find out cost about 0.8 µs a table every pass, most of a
frame's script overhead in a scene of mostly idle scripts.

Luau already tests one byte, `LuaTable::readonly`, on every path that writes a
table, because `table.freeze` relies on it. The patch gives that byte a third
value:

- `0`: an ordinary table. `1`: frozen, as before. `2`: **armed**.
- An armed table is an ordinary writable table to every script. The first
  write of any kind clears the byte and goes on: a field or slot, `rawset`,
  `setmetatable`, `table.insert`, `remove`, `sort`, `clear`, `move`, or a raw
  set from the host. So "still armed" means "nothing has written to it since
  it was armed".
- The inline fast paths in `lvmexecute.cpp`, the fast calls in
  `lbuiltins.cpp` and native code generation already treat any nonzero byte
  as "take the slow path", so they need no change and an armed table costs
  them nothing until it is written.

The changes:

1. `ldebug.h`, `ldebug.cpp`: `luaG_readonlywrite(L, t)`. On an armed table it
   disarms and returns; on a frozen one it raises "attempt to modify a
   readonly table" as before.
2. Every place that raised on a readonly table now calls it instead:
   `lvmutils.cpp` (`luaV_settable`), `lapi.cpp` (`lua_rawsetfield`,
   `lua_rawset`, `lua_rawseti`, `lua_setmetatable`, `lua_cleartable`) and
   `ltablib.cpp` (`moveelements`, `table.move`, `table.sort`, `table.clear`).
3. `lapi.cpp`, `lua_getreadonly`: true only for a frozen table, so
   `table.isfrozen`, `table.freeze`'s "already frozen" check and mlua's
   `Table::is_readonly` all see an armed table as not frozen. `table.freeze`
   on an armed table freezes it.
4. `lua.h`, `lapi.cpp`: `lua_floptle_arm(p)` (0 becomes 2; a frozen table
   stays frozen) and `lua_floptle_armed(p)`. `p` is the table's address from
   `lua_topointer`; Luau's collector never moves an object, so the address is
   good for as long as the caller keeps the table alive.

mlua 0.10.5 decides "readonly" through `lua_getreadonly` in both
`Table::raw_set` and `Table::set_metatable`, so the engine's own writes go
through and disarm the table like any other.

Two guards in `floptle-script`:
- `every_write_disarms_an_armed_table_and_no_read_does` sweeps every write
  path (at each optimisation level) and the reads that must leave the table
  armed.
- `no_readonly_error_is_raised_outside_the_armed_table_helper` reads these
  sources and fails if any call to `luaG_readonlyerror` is left outside
  `luaG_readonlywrite`. A re-vendored Luau that adds a new raise site fails it
  until the site is converted.

## Upgrading mlua or Luau

Luau 745 still evaluates multiple assignments its own way. When you upgrade:
1. Re-vendor the new `luau0-src`.
2. Re-apply the `FLOPTLE PATCH` blocks and `build.rs`.
3. Convert any new `luaG_readonlyerror` raise site to `luaG_readonlywrite`.
4. Check that mlua still reads `lua_getreadonly` before its raw writes.
5. Keep the `[patch.crates-io]` version in step.

The guard `a_multiple_assignment_evaluates_every_value_before_assigning` in
`floptle-script` fails if the compiler patch is lost; the two above fail if
the armed-table patch is.
