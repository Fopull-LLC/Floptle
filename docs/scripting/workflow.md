# Working in the editor

The in-engine IDE, the profiler, the gotchas, and why some calls refuse.

Part of the [scripting guide](../scripting.md) · [every call, as a reference](../lua-api.md)

## Contents

- [14. The in-engine IDE](#14-the-in-engine-ide)
- [15. Tips & gotchas](#15-tips-gotchas)
- [28. Where the frame went: `perf.*`](#28-where-the-frame-went-perf)
- [29. Options that refuse — and why an error is the kind answer](#29-options-that-refuse-and-why-an-error-is-the-kind-answer)

---

## 14. The in-engine IDE

Double-click a `.lua` in Assets (or use the Inspector's Scripting section) to
open it in the **Scripting** tab — a small but real code editor:

- **Find & replace** — `Ctrl+F` finds (seeded from your selection), `Ctrl+H`
  adds a replace row, `Enter` / `Shift+Enter` or `F3` / `Shift+F3` step
  through matches (the current one is outlined), `Aa` toggles match case, and
  **⌕ all scripts** lists every matching line across the whole project.
  Typing in the find field never yanks focus back into the code.
- **Line editing** — with nothing selected, `Ctrl+C` / `Ctrl+X` copy / cut the
  whole current line. `Ctrl+D` duplicates, `Ctrl+Shift+K` deletes,
  `Alt+Up/Down` moves the selected lines, `Ctrl+/` toggles `--` comments over
  the selection, and `Tab` / `Shift+Tab` indent / outdent a multi-line
  selection. `Enter` auto-indents (one level deeper after `then`/`do`/`function`).
- **Navigation** — `Ctrl+G` goes to a line, `Ctrl+B` (or right-click) jumps to
  a definition, right-click also finds all references. The Console's
  double-click-to-source lands here too.
- **Saving** — `Ctrl+S` saves, `Ctrl+Shift+S` saves all; closing a tab with
  unsaved changes asks first, and pressing **Play auto-saves** open edits so
  the run always matches what you see.
- **Completion & docs** — the popup opens **by itself only after `.` or `:`**,
  where you're asking what fields something has; `Ctrl+Space` summons it anywhere
  else. `↑`/`↓` choose, **`Enter` accepts**, `Esc` hides it until the token
  changes — and **`Tab` always indents**, so completion can never eat a
  keystroke you aimed at your code. It understands member access on **any
  variable** — `rb.fri` offers `friction`, `anim:pl` offers `play`, and
  `params.` offers this script's own `defaults` keys. The highlighted entry
  shows its doc *and a usage example* in the popup; hovering an API name in
  code shows the same. The **§ Docs** page has a search box over the whole
  guide + API reference, with worked examples under the common entries.
- **Formatting** — `Alt+Shift+F` (or the **⚏ Format** button, or tick **on
  save**) re-indents the file by block depth and tidies whitespace. It changes
  **nothing else** — no re-flowed expressions, no realigned comments, no moved
  code — and it's idempotent, so format-on-save can't produce a second diff.
  `--@noformat` exempts a file; a line ending in `--@keep` keeps its own
  indentation.
- **Warnings** — a `⚠ n warnings` strip under the editor expands into a
  clickable list of the mistakes Lua can't report:
  - **an undeclared assignment** — `sped = speed * dt` compiles, writes a
    global, reads `nil` forever, and says nothing. The warning names it and
    suggests the local you meant. Globals assigned at **file scope** are
    deliberate publications (§8) and are never flagged.
  - **an unused local** — usually a half-finished rename. Prefix with `_` to
    keep it quiet.
  - **upvalue pressure** — on a build whose Lua caps upvalues per function, every
    file-scope `local` is one, and you get a warning before the cap that names
    the fix (group related state into one table). LuaJIT's cap is **60**; Luau,
    which the engine runs by default, has none, so on a stock build this warning
    does not appear at all.
  - **a hook that forgot the node** — `function update(dt)` looks right and is
    wrong: every lifecycle hook is called with the **node first** (§3), so `dt`
    is bound to the node and the first arithmetic on it raises, every frame,
    before anything visible has happened. From the outside that isn't an error,
    it's a script that does nothing at all.

  `--@nolint` silences a line; on its own line it silences the file.

### Shortcuts

| | | | |
|---|---|---|---|
| `Ctrl+S` | save | `Ctrl+Shift+S` | save all |
| `Ctrl+F` | find | `Ctrl+H` | find & replace |
| `F3` / `Shift+F3` | next / prev match | `Ctrl+G` | go to line |
| `Ctrl+C` / `Ctrl+X` | copy / cut line | `Ctrl+D` | duplicate line |
| `Ctrl+Shift+K` | delete line | `Alt+↑` / `Alt+↓` | move line(s) |
| `Ctrl+/` | toggle comment | `Tab` / `Shift+Tab` | indent / outdent |
| `Ctrl+B` or `F12` | go to definition | `Shift+F12` | find references |
| `Alt+Shift+F` | format document | `Ctrl+Space` | suggest |
| `Ctrl+W` | close tab | | |

The same list lives on the tab's **§ Docs** page.

---

## 15. Tips & gotchas

- **Run, then Play:** scripts only execute while the game is playing (F1). Stop
  restores the scene to its pre-Play state.
- **Drive bodies by velocity, not position.** Setting `node.x/y/z` on a Rigidbody
  node fights the physics step; set `node.vx/vy/vz` instead.
- **Edges vs. held:** use `input.pressed` / `input.clicked` for one-shot actions
  (jump, fire) and `input.key` / `input.button` for held movement.
- **Errors** appear at the top of the Scripting tab and in the Console, with the
  script name + line — double-click to jump to the source.
- **Hot-reload:** just save. The script re-runs in a fresh environment, so avoid
  relying on state surviving a reload mid-Play.
- **A loop without an exit stops the script, not the editor.** A pass into your
  scripts may run for **2 seconds** (500 ms on a dedicated server, see
  `--script-budget-ms`); past that the script that overran is stopped with an
  error naming the budget, and it is not called again until you edit its file.
  Everything else keeps running. The scripts share **512 MB** between them —
  a table that only grows hits "not enough memory" in its own call, and the
  rest of the game goes on.
- **A path is relative to the project and stays inside it.** `assets.getFile`,
  `assets.getContents`, `node.model =`, a texture, a clip, a scene, a script
  name on a node: an absolute path or a `..` that would leave the project
  resolves to nothing, and the Console says which rule refused it.
  `assets.getContents` lists at most 20 000 files and says so when it stops.

---

## 28. Where the frame went: `perf.*`

Until **0.33.0** the engine kept a smoothed FPS number and nothing else. No
attribution — not per script, not per subsystem, not per draw. So when a game got
slow, the author's only available move was to file an engine ticket. That is not a
hypothetical; it happened four times:

| filed as | actually was |
|---|---|
| "a crowded scene is unplayable" | component lookup was a linear scan |
| "cross-script wiring is slow" | `findScript` was a linear scan |
| "currently unplayable" | a scatter field asked for 117,000 props |
| "I can see through unloaded terrain" | mesh priority ignored world distance |

Every one cost a round trip through the engine to discover a number the game
could have read itself. **Three of the four were answerable from a count alone.**

```lua
function start(node)
  perf.enable(true)
end

function update(node, dt)
  -- The WORST recent frame, not the average. A 40 ms hitch once a second adds
  -- under a millisecond to a 60-frame mean, so the mean hides the thing you are
  -- chasing.
  if perf.worstMs("scripts") > 6 then
    log("slow pass — the worst is " .. perf.slowestScript())
  end
end
```

The buckets, in frame order: `scripts` `mirror` `physics` `terrain` `scatter`
`particles` `audio` `animation` `ui` `render`. `perf.buckets()` returns exactly
that list, so a loop over it can never name one that does not exist.

`scripts` is the **whole** of every script pass — the per-instance setup, the
reference params, the write flush, and the hooks themselves. `mirror` is the
ECS → Lua sync each pass runs before it calls anything, which is nested inside
a pass and subtracted out of `scripts` so the two do not count it twice. Until
0.84.2 `scripts` was the hook time alone, which on one shipped game accounted
5.3 ms of a 12.9 ms step and hid an optimisation pass for a release.

### Per script, by name

`perf.ms("scripts")` tells you the pass is expensive. It does not tell you which
of your scripts is doing it, which is the actual question — so
`perf.scripts()` returns a row per script **most expensive first**, and
`perf.slowestScript()` is the one-liner you put in an assertion message.

```lua
for _, s in ipairs(perf.scripts()) do
  print(string.format("%-24s %5.2f ms  (worst %5.2f)", s.name, s.ms, s.worstMs))
end
```

Names are script file names, because that is what you call them.

**What the number is, and what it is not.** It is a wall clock around the call,
so it includes everything that happened while your script was on the CPU —
including things that were not your script. A garbage collection, or the
operating system taking the core away for a moment, lands on whichever script
happened to be running, and shows up as a large `worstMs` for a script that did
nothing unusual.

These are a **breakdown of part of** `perf.ms("scripts")`, not all of it: the
bucket is the whole pass and these are the time inside a hook. The difference is
what the engine spends reaching your hooks, and if it is large the thing to look
at is how many scripted nodes the scene has rather than any one script.

Two consequences worth knowing before you go optimising:

* A script with **no hook for that pass** is not listed at all. It used to be,
  and a stall inside its empty span read as a 12–21 ms "peak" against a file
  with no `update` in it.
* `ms` (the average) is the number to act on. A single high `worstMs` is at
  least as likely to be a collection as a slow script — look for one that is
  high *consistently*, or that moves when you change the script.

If the collector is what you are chasing, start with
`floptle run --frames 600 --alloc`: it says how much Lua heap a frame allocates
and which scripts it comes from. Vectors are one source — under `exact` every
`vec3` a script builds is an object, so `a + b`, `v:normalized()`, `node.pos`
and `node.worldPos` each allocate, while scalar reads (`node.x`, `v:length()`,
`v:dot(o)`) allocate nothing — but they are not always the biggest one: on one
shipped game they were 2% of the frame's allocation, and the rest was tables
and strings the scripts built themselves. Let the readout name the script
before rewriting anything.

### Counts

```lua
local c = perf.counts()
-- nodes, culled, instances, draws, chunks, props, particles
assert(c.props < 20000, "the forest is asking for too much")
```

Counts are free to keep, so `perf.counts()` works even while collection is off.

### It is off by default, and reading it while off is an ERROR

A profiler that is itself a frame cost gets turned off, and then it does not
exist. So nothing is collected until `perf.enable(true)` — either from a script,
or by opening the editor's **⏱** panel.

But that makes "off" and "free" the same shape, and
`assert(perf.ms("scripts") < 4)` would then pass in a smoke test that measured
nothing. So every timing getter **raises** while collection is off and tells you
to call `perf.enable(true)`. Same reasoning as the `pin = "topCenter"` fix: a
wrong answer that looks like a right one is worse than an error.

An unknown bucket name raises too, and names every accepted value.

### In the editor

The **⏱** button in the play toolbar opens the same numbers, per bucket and per
script, with the worst column coloured. Opening it starts collection; closing it
stops — unless a script asked for it, in which case it stays on so a game's own
budget check keeps working.

`accountedMs()` is the buckets added up. It is called *accounted* and not *total*
because vsync, the OS and the GPU finishing are outside every bucket; a number
claiming to be the frame time without being it would be worse than not offering
one.

---

## 29. Options that refuse — and why an error is the kind answer

Every options table in this engine is **closed**. A key it does not read is an
error, not a shrug:

```lua
scatter.create{ asset = "trees/pine.glb", perchunk = 6 }
-- scatter.create: no option called `perchunk` (did you mean `perChunk`?)

node:setCamera{ target = "minimap", width = 0 }
-- node:setCamera: `width = 0` is outside 8 – 4096

audio.play("engine.ogg", { mode = "spacial" })
-- audio.play: `mode = "spacial"` is not a name I know — it takes spatial, 3d,
-- distance, flat, 2d
```

Every message names three things: **the property**, **the value it got**, and
**what it accepts**. Any one of the three missing sends you back to re-reading
your own file, which is the part that costs an afternoon.

### Why this is worth a breaking change

43% of every bug ever filed against this engine — 32 of 74 — was one shape: the
engine answered something it did not understand instead of refusing it.

* `scatter.create{ collide = true }` was parsed, stored, and read by nothing.
  For two releases. A game asked for solid props and got props you walk through.
* `pin = "topCenter"` meant top-**left**. Silently. Four HUD elements in one
  corner and a report that read like a layout bug.
* A typo'd `perchunk` took the default, forever, with nothing to see.
* `scene.load(name, { addative = true })` **destroyed** the running scene instead
  of layering onto it — `additive = false` is what a misspelling reads as.

Every one of those was found by somebody playing, and each was fixed on its own.
The counter-argument — that refusing is a breaking change — was settled the first
time: a game that asked for solid props and got props it walked through was worse
off than a game that got an error. **Silence is not compatibility.**

### What keeps it true

One list per call, in the code, read by both the check and the write — so a key
that is accepted is a key that does something. One shared parser per enum, so the
list an error offers is the list the engine acts on. And two tests: one calls
every registered options table with a bogus key and fails if it is accepted, the
other scans the source and fails when a **new** options table appears that
neither checks its keys nor is excused in writing.

So this does not decay back. That is the actual deliverable — not the 32 fixes,
which were already made one at a time.
