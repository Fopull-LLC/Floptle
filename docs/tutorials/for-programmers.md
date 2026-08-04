# Floptle for programmers

The model, the tick, and the six things that aren't like the engine you came from.

**for programmers** · about 20 minutes · 9 steps

> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and ticks each one off as your project starts to match it.

You've shipped software. You don't need `dt` explained. What you need is the
model — what a node actually is, what runs when, where state is supposed to
live, and which of your habits from the last engine will quietly cost you an
afternoon here.

No project to build. Read it front to back in about twenty minutes, then use the
**⚙ API** page for specifics; it lists every name the engine exposes.

The last step has something to type, because a model you haven't run is a model
you're guessing at.

## 1. What's on disk

A project is a directory of plain text. No database, no binary blob, no import
step you can't see.

    project.ron        settings: title, entry scene, layers, mixer graph
    scenes/*.ron       node trees
    scripts/*.lua      behaviour
    prefabs/*.prefab.ron  saved subtrees
    materials/  models/  textures/  audio/  vfx/  ui/
    .floptle/          editor-side extras (Lua definitions, tutorial progress)

Everything is RON — Rust's object notation, which reads like JSON that grew up.
It diffs and merges as well as text can, so it belongs in version control
exactly as it is.

Two consequences worth internalising:

- **The editor is not the source of truth; the files are.** External edits are
  picked up, and anything the editor can do to a project, `floptle --new`,
  `--migrate` and `--export` can do headlessly from CI.
- **A `.glb` you drop in is referenced, not absorbed.** Re-export it over the
  same path and the scene picks it up.

## 2. Nodes, components, and what a script attaches to

Underneath there's an ECS: entities with component columns. Above it there's a
**node tree**, which is what the editor and the scripts talk to. Think of the
node as a facade — it's the thing with a name, a transform, a parent, and a set
of components.

    Node = Name + Transform + Parent? + Matter + [components…]

`Matter` is the mutually-exclusive one — a node is a Primitive **or** a Mesh
**or** a Camera **or** a Light **or** a UI element, never two. Everything else
is additive: Rigidbody, Material, Scripts, Tags, Layer, Networked.

A **script** is a component holding `(kind, enabled, params)`. `kind` is the
file stem, so `scripts/patrol.lua` attaches as `patrol`. The same file attached
to forty nodes is forty independent instances: each gets its own environment,
its own file-scope locals, its own `params`.

That last point is the one to hold on to. A file-scope `local` in a Lua script is
**per-instance state**, not per-file. It behaves the way a struct field would in
a language with objects, and it is why almost none of these scripts define a
class.

## 3. The tick, and which hook to use

Three per-node hooks, and choosing between them correctly is most of the
frame-timing bugs you'll never have:

- **`fixedUpdate(node, dt)`** — 60 Hz, constant `dt`, the same clock physics
  steps on. Movement, gameplay, anything that decides where things go. Frame
  rate cannot change the outcome.
- **`update(node, dt)`** — once per drawn frame, variable `dt`. Cosmetics, HUD,
  input polling that isn't authoritative.
- **`lateUpdate(node, dt)`** — after physics **and** after the interpolated
  transform writeback. The camera pass. Anything that follows something else
  belongs here; do it in `update` and you are following last frame's pose,
  which is a `velocity × dt` lag that reads as jitter.

Plus `start(node)` once at play, and the event hooks: `onCollisionEnter/Stay/Exit`,
`onTriggerEnter/Stay/Exit`, and the UI hooks (`clicked`, `changed`, `dragStart`,
…). All take the node first — always, including `start`.

Alongside that there's a scheduler on the game clock: `after(s, fn)`,
`every(s, fn)`, `tween(s, fn, ease)`, each returning a handle with `:cancel()`.
They pause when the game pauses, which hand-rolled countdowns in `update` do
not.

## 4. Params: the Inspector as a two-way binding

defaults = {
      --@range 0 20 --@units m/s
      speed = 4.5,
      --@options patrol|chase|flee
      mode = "patrol",
      target = noderef(),
      body = componentref("RigidBody"),
      brain = scriptref("health"),
    }

`defaults` declares the instance's tunables; the Inspector generates a row per
entry, in declaration order, with the widget the value's *type* implies — number,
string, bool, colour, dropdown. `--@` annotations describe the row and are inert
at runtime.

Three things that aren't obvious:

- **It is two-way.** Writing `params.speed` from Lua persists across frames,
  shows live in the Inspector during play, and is readable by other scripts
  through a handle. Stop reverts it to the authored value.
- **The ref types eliminate `find()`.** `noderef` gives you a node handle
  wired in the editor; `componentref` and `scriptref` bind straight to a
  component or another script on that node. Renaming a node doesn't break any
  of them, and you can see the wiring in the Inspector rather than inferring it
  from string literals.
- **Undeclared keys are frame-local.** If it isn't in `defaults`, it doesn't
  round-trip.

## 5. Talking between scripts

In rough order of how much you should prefer them:

- **`scriptref` / `noderef` params** — wired in the editor. No search, no string
  literal, survives renames.
- **`findScript("kind")`** — a handle to the first script of that kind
  anywhere. The manager pattern: one `inventory`, one `gameState`, asked by
  everyone. `findScripts` gets all of them.
- **`find("Name")` / `findTagged("tag")`** — by name, or by group. Cache the
  result in `start`; calling `find` every frame is a scene walk you're paying
  for repeatedly.

A **script handle** proxies the target's environment: `h.someState` reads its
variable, `h.someFn()` calls its function, `h.node` is the node it's on, and
`h.valid` says whether it still exists. Note the dot — these are plain
functions, so a colon would pass the handle as a phantom first argument.

The convention for publishing state is a file-scope assignment with **no**
`local`. Locals are private to the file; a bare `over = false` is deliberately
the script's public surface. The linter knows the difference and won't flag it.

## 6. Lua's one real hazard, and the lints

Lua has a defining trap: **every undeclared name is a global that reads `nil`.**

    local speed = 4
    sped = speed * dt   -- compiles, runs, does nothing, says nothing

Nothing raises. Combine it with hot reload and you can lose an afternoon to a
script that "should work". The editor lints for exactly this, plus:

- **unused local** — usually a half-finished rename.
- **upvalue pressure** — LuaJIT allows 60 upvalues per function, and every
  file-scope `local` is an upvalue of every function below it. The real error
  ("too many upvalues") names no fix, so this warns at 50 with one.
- **hook signature** — `function update(dt)` binds the *node* to `dt`. From the
  outside that's a script that does nothing at all.
- **raw key polls** — `input.pressed("space")` where a named action would work.
  It runs; it just can't be rebound, never reaches a gamepad, and reads neutral
  on a networked predicted node.

All warnings, never blocking, `--@nolint` to silence a line or a file. The
runtime is **LuaJIT** (5.1 semantics plus `goto`): `math.atan2` and `#t`, no
integer division operator, no `goto continue` idiom needed.

## 7. Six things that will surprise you

The list of habits that don't transfer, collected from the projects built in
this engine:

- **Forward is -Z.** +X right, +Y up. Every direction bug traces back here once.
- **Scripts can create nodes but not colliders.** `createNode` and
  `n:setPrimitive(...)` build geometry at runtime; anything that must be solid
  is authored once as a prefab and `spawn`ed. Procedural *levels* are prefabs
  placed by script, not geometry conjured by it.
- **A parent link in a scene file is positional.** Inserting a node in the
  middle of a `.ron` by hand shifts every later index. Let the editor do it.
- **`save.*` is capped at about a kilobyte per key.** Deliberately. Store the
  fact, not the world.
- **Play mode never mutates your scene.** Everything reverts on Stop, params
  included. It's a sandbox — but it also means "I fixed it while playing" is a
  thing you have to redo.
- **Named actions, always.** `input.action("Fire")` over a key code. Gamepads,
  rebinding, and multiplayer prediction all fall out of it for free; raw key
  polls read neutral on a predicted node, which is a bug that only appears once
  a second player joins.

## 8. Determinism, if you're going near multiplayer

Skip this until it matters, but know it exists — retrofitting it is much worse
than allowing for it.

Networking is **rollback**: clients predict forward from local input, the server
is authoritative, and mispredictions re-simulate from the last agreed state.
Re-simulation means the same inputs must produce the same outputs, every time,
on every machine.

What that costs you in practice:

- Gameplay in `fixedUpdate`. Anything driven by frame time cannot re-simulate.
- `rng(seed)` for anything that matters — a deterministic stream. `math.random`
  is fine for a spark, not for loot.
- Read input through named actions. Raw key polls are not part of the input
  command that gets replayed, so they read neutral during prediction.
- `net.isServer()` / `net.isMine(node)` to decide who's allowed to act, and
  `net.spawn` rather than `spawn` for anything replicated.

`docs/multiplayer.md` is the long version, and there's a referee mode that
replays a recorded match and reports the first frame where two machines
disagreed.

## 9. Run one

Enough reading. Create the script below, put it on any node, and press Play.

It's forty seconds of work and it makes four of the abstractions above concrete
at once: per-instance state, the difference between the two clocks, two-way
params, and the scheduler.

Watch the Console. `update` and `fixedUpdate` tick at different rates and the
gap between them is your frame rate; drag **Rate** in the Inspector while it
runs and the log responds immediately.

### Then

- **⚙ API** in the Scripting tab, or `docs/lua-api.md` — every name, grouped,
  searchable, with worked examples.
- The scripts every project is seeded with, in `scripts/` — `third_person`,
  `fighter`, the `rts_*` trio and `web_login` are reference implementations,
  and they are written to be read.
- `docs/scripting.md` for the long form, `docs/ARCHITECTURE.md` for what's
  under the node tree.

`scripts/probe.lua`

```lua
-- A throwaway probe: watch the two clocks, and prove params are live.

defaults = {
  --@desc How often to report, in seconds.
  --@range 0.25 5 --@units s
  rate = 1.0,
}

-- File-scope locals are PER INSTANCE. Two nodes running this script keep two
-- separate pairs of counters.
local frames = 0
local ticks = 0

function start(node)
  log(node.name .. ": start")

  -- The scheduler runs on the game clock, so this pauses when the game does.
  every(params.rate, function()
    log(string.format("%s: %d frames, %d ticks in the last %.1fs",
      node.name, frames, ticks, params.rate))
    frames = 0
    ticks = 0
  end)
end

function update(node, dt)
  frames = frames + 1
end

-- Constant 60 Hz, whatever the frame rate is doing. This is the clock gameplay
-- belongs on.
function fixedUpdate(node, dt)
  ticks = ticks + 1
end
```

*Done when: scripts/probe.lua exists.*

