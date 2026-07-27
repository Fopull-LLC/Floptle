# Input & action mapping (`floptle-input`)

Bind a named **Action** to many inputs at once — any keyboard key, any mouse
button, sticks/triggers/buttons on a gamepad — then decide what the action *does*
in a node script. Devices are an implementation detail; gameplay code talks to
actions and axes, never raw keys.

**Status: shipped.** See [ADR-0024](../decisions/0024-input-action-map.md) for the
four decisions taken during implementation — chiefly that the fighting-game
layer (buffering, motion inputs, SOCD) moved *into* the engine, and that the
netcode wire now carries actions instead of keys.

> Reads on: [ADR-0024 Input action map](../decisions/0024-input-action-map.md) ·
> [ADR-0003 Lua scripting](../decisions/0003-scripting-lua.md) ·
> [ADR-0005 ECS/Node facade](../decisions/0005-scene-model-ecs-node-hybrid.md).
> Sits beside: [`./scene-and-nodes.md`](./scene-and-nodes.md) (scripts that read
> input) · [`./ui.md`](./ui.md) and [`./camera-and-dialogue.md`](./camera-and-dialogue.md)
> (which own input *contexts*). Where it runs: [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3.
> Crate `floptle-input` depends on `floptle-core` plus `gilrs` behind the `pads`
> feature; neither `winit` nor `gilrs` appears in its public model.

## Where it sits in the frame

Devices are pumped first, right after the winit poll and before any script step
(ARCHITECTURE §3), into a `RawInput`. Resolution then happens **twice, in two
domains**:

```
winit events ┐                       ┌─▶ resolve_frame ─▶ ActionState ─▶ update / lateUpdate
gilrs pump   ├─▶ RawInput ──────────>┤        (per rendered frame)
mouse motion ┘   (levels + banked    └─▶ resolve_tick  ─▶ ActionState ─▶ fixedUpdate
                  edges)                     (per fixed tick)  └─▶ History (buffers, motions)
```

`update` runs per rendered frame and `fixedUpdate` per fixed tick; they advance
at different rates, so a single runtime would let whichever ran first consume
the other's edges — press Jump on a frame between two ticks and `fixedUpdate`
would never see it. Each domain therefore keeps its own edge state and hold
timers.

`RawInput` carries two kinds of information for that reason: **levels** (what is
held right now) and **banked edges** (presses since the last window closed). The
tick domain needs the latter, because a key tapped between two ticks is already
back up by the time the tick samples.

**Only the tick domain feeds history.** Motion windows are counted in ticks so a
player on 144 Hz and one on 60 Hz get identical leniency, and a rollback replay
reproduces the same answer.

Nothing writes input state mid-pass, so two scripts reading `"Jump"` in the same
pass see the same answer.

## Devices & sources

One flat `Source` enum covers everything bindable. "ALL key types / ALL mouse
buttons" is the point: we mirror winit's full key set and button range, we don't
curate a subset.

```rust
enum Source {
    Key(Key),                   // every physical key (letters, F-keys, mods, numpad…)
    Mouse(MouseButton),         // Left/Right/Middle/Back/Forward
    MouseAxis(MouseAxis),       // MotionX, MotionY, ScrollX, ScrollY (relative deltas)
    Pad { id: PadId, ctrl: PadControl },   // see below; id resolves which controller
}

enum PadControl {
    Button(PadButton),          // South/East/West/North, bumpers, dpad, stick clicks, start…
    Axis(PadAxis),              // LeftStickX/Y, RightStickX/Y, LeftZ/RightZ (triggers)
}

enum PadId { Any, Slot(u8) }    // Any = "any connected pad"; Slot(n) = local player n
```

These are the crate's **own** enums, not winit's or gilrs's — that's what keeps
every rule below testable with no window and no controller. The editor
translates `winit::KeyCode` at the boundary; `pads.rs` is the only module that
mentions `gilrs`.

- **Keyboard** — `Key` mirrors the *physical* key (layout-independent), so a
  binding to the `W` position is stable across QWERTY/AZERTY. Its `script_name`
  matches the legacy `input.key("w")` strings byte for byte, so the raw and
  action views of the keyboard never disagree.
- **Mouse** — buttons *and* relative motion/scroll are first-class `Source`s, so
  "look" (mouse motion) and "zoom" (scroll) bind exactly like a stick axis.
- **Gamepad** — `gilrs` gives normalised buttons/sticks/triggers and **hot-plug**.
  Slots are claimed by device **UUID**, not by position in the connected list: a
  pad that drops out keeps its slot and reclaims it on replug, so P1's battery
  dying doesn't promote P2 onto P1's character. An unplugged slot reads fully
  neutral rather than freezing its last pose. `PadId::Any` is the single-player
  default and resolves to the reading player's own pad first.
- An **analog** source bound to a *digital* action fires past its `threshold`
  (0.5 by default) — that's how a trigger becomes a button.

## The action map

The action map is the whole public model: **digital Actions** and **analog Axes**,
each fed by a list of bindings. Add as many bindings as you like; *any* of them
fires the action.

```rust
struct InputMap {
    actions: Vec<Action>,       // digital: pressed/released/held
    axes1:   Vec<Axis1>,        // 1D analog: triggers, mouse wheel, A/D
    axes2:   Vec<Axis2>,        // 2D analog: WASD, a stick, mouse motion
    motions: Vec<Motion>,       // fighting-game direction sequences
    players: u8,                // local player count (slots)
}

struct Action {
    name: String,               // "Jump"
    bindings: Vec<Binding>,     // ANY of these triggers it (OR)
}

struct Binding {
    source: Source,
    modifiers: Vec<Source>,     // optional chord, e.g. [Key(ControlLeft)] for Ctrl+S
    threshold: f32,             // where an analog source counts as pressed
}
```

**Digital state per action**, recomputed each resolve from the OR of its
bindings (a binding with modifiers is "active" only while all modifiers are held):

```
pressed       held last frame? + held now?   → true while down
just_pressed  edge: up → down this frame
just_released edge: down → up this frame
held_secs     f32, seconds continuously down  (for hold-to-charge in script)
```

### Axes — 1D and 2D, with deadzone & sensitivity

An axis composes sources into a value. Digital sources contribute ±1 (so WASD
*is* a 2D axis); analog sources pass through with per-binding deadzone, curve, and
sensitivity. This is how "Move" works identically on stick and keyboard.

```rust
struct Axis2 {
    name: String,               // "Move"
    bindings: Vec<Axis2Binding>,// any contributes; engine picks the dominant source
}

enum Axis2Binding {
    Stick { id: PadId, x: PadAxis, y: PadAxis, deadzone: f32, sensitivity: f32, curve: Curve },
    Keys  { up: Source, down: Source, left: Source, right: Source },   // WASD-style
    Mouse { sensitivity: f32 },                                         // motion deltas
}
```

- **Deadzone** is radial for sticks (kills drift near center), per-binding so a
  worn stick can be tightened without touching the keyboard binding.
- **Sensitivity / response curve** scales magnitude (linear default; an optional
  `expo` curve for fine aim). Output is clamped to the unit disk for `Axis2`.
- **Dominant source:** when both a stick and WASD are bound to one axis, the
  larger-magnitude source this frame wins **whole** — no fighting if the player
  bumps both, and no summing that would let a brushed key deaden the stick. An
  exact tie keeps the earlier binding.
- **Gate** — a mouse/analog binding can require a source to be held (`gate:
  [Mouse(Right)]`). This is what lets one `Look` axis serve both devices
  honestly: the mouse contributes only while you drag, so a free cursor never
  spins the view, while a right-stick binding on the *same* axis stays live at
  all times because a stick recentres itself.
- **Rate** — a mouse binding reports **pixels per second** by default rather
  than pixels-this-frame. A stick reports a position the game integrates into a
  turn rate; a mouse reports a displacement that already *is* the turn. Dividing
  by the frame time makes them the same kind of quantity, so a script writes
  `yaw = yaw - lookX * dt` once and it is correct — and frame-rate independent —
  on both.
- **SOCD** decides what a *digital* axis does when opposing directions are held
  at once — see the fighter layer below.

### RON input map — `<project>/input.ron`

The whole thing is RON (ARCHITECTURE §8), diffable and hand/AI-editable. It gets
its own file rather than a `project.ron` field because it's the one project
asset a shipped settings menu overlays at runtime, and rebinds stay diffable
that way. A **missing** file is not an error — the project simply has no actions
yet and raw-key scripts keep working. A file that exists but won't parse *is*
an error: silently substituting an empty map would unbind the whole game and
read as a hardware fault.

The real top-level name is `InputMap`, and it carries `motions`, `players` and
per-axis `socd` alongside what the original sketch below shows:

```ron
InputMap(
    actions: [ Action(name: "Punch", bindings: [
        Binding(source: Key(KeyJ)),
        Binding(source: Pad(id: Slot(0), ctrl: Button(West))),
    ]) ],
    axes2: [ Axis2(name: "Move", socd: LastWins, bindings: [
        Keys(up: Key(KeyW), down: Key(KeyS), left: Key(KeyA), right: Key(KeyD)),
        Stick(id: Any, x: LeftStickX, y: LeftStickY, deadzone: 0.15),
    ]) ],
    motions: [ Motion(name: "qcf", dirs: [2, 3, 6], window: 12) ],
    players: 2,
)
```

The original sketch, still accurate for actions and axes:

```ron
InputMap(
    actions: [
        Action(name: "Jump",   bindings: [
            Binding(source: Key(Space)),
            Binding(source: Pad(id: Any, ctrl: Button(South))),   // A / ✕
        ]),
        Action(name: "Fire",   bindings: [
            Binding(source: MouseButton(Left)),
            Binding(source: Pad(id: Any, ctrl: Axis(RightZ))),    // right trigger as button (threshold)
        ]),
        Action(name: "Save",   bindings: [
            Binding(source: Key(KeyS), modifiers: [Key(ControlLeft)]),  // Ctrl+S chord
        ]),
    ],
    axes1: [
        Axis1(name: "Zoom", bindings: [
            Axis1Binding(Mouse(MouseAxis(ScrollY), sensitivity: 1.0)),
            Axis1Binding(Keys(plus: Pad(Any, Axis(RightZ)), minus: Pad(Any, Axis(LeftZ)))),
        ]),
    ],
    axes2: [
        Axis2(name: "Move", bindings: [
            Keys(up: Key(KeyW), down: Key(KeyS), left: Key(KeyA), right: Key(KeyD)),
            Stick(id: Any, x: LeftStickX, y: LeftStickY, deadzone: 0.15, sensitivity: 1.0),
        ]),
        Axis2(name: "Look", bindings: [
            Mouse(sensitivity: 0.08),
            Stick(id: Any, x: RightStickX, y: RightStickY, deadzone: 0.12, sensitivity: 1.0),
        ]),
    ],
)
```

`"Move"` reads identical to a script whether the player uses WASD or the left
stick — that is the entire value proposition.

## Input contexts

A **context** is a named, prioritized layer of action enablement that can
*consume* input so lower layers don't see it. This is how dialogue eats the world's
input without the player script knowing.

```rust
struct Context {
    name: String,               // "gameplay" | "menu" | "dialogue"
    priority: i32,              // higher wins; resolved top-down
    enabled: Vec<String>,       // actions/axes this layer cares about
    mode: ConsumeMode,          // Passthrough | Consume
}

enum ConsumeMode { Passthrough, Consume }   // Consume = swallow handled inputs
```

Resolution each `update`: contexts sort by priority desc; for each input event,
the highest-priority **enabled** context claims it. A `Consume` context (a modal
menu, an active dialogue) blocks lower contexts entirely; a `Passthrough` overlay
(a HUD that only listens for `Pause`) claims its own actions and lets the rest
fall through.

```
 priority ▼            sees input?
  dialogue  (Consume)  ████  ← active: eats "Advance"/"Skip", blocks below
  menu      (Consume)  ░░░░  ← inactive
  gameplay  (Pass)     ░░░░  ← gets nothing while dialogue is up
```

Contexts are pushed/popped by systems and scripts: opening dialogue pushes the
`dialogue` context (see [`./camera-and-dialogue.md`](./camera-and-dialogue.md));
the UI system pushes `menu` when a modal opens (see [`./ui.md`](./ui.md)). The
stack is plain data — no callbacks needed to "give back" input.

## Runtime rebinding (press-to-bind)

For a settings menu: ask the input system to capture the **next** input and write
it onto a binding. Useful, small, not a whole subsystem.

```rust
input.start_rebind(action: "Jump", slot: 0, filter: BindFilter::AnyButton);
// next frame a button/key is captured → input.pending_rebind() == Some(Source::…)
// confirm → writes Source onto Action."Jump".bindings[0]; cancel on Esc/timeout.
```

`BindFilter` (`AnyButton`, `KeyboardOnly`, `PadOnly`, `AxisOnly`) keeps a "press
a key" prompt from grabbing stray stick drift. Rebinds serialize back to the same
RON map, so player settings are just an overlay map merged over `input.ron`.

## Scripting API (Lua)

The curated `input` table (ARCHITECTURE §7) is the only thing gameplay touches —
it reads the snapshot, never devices. Action **behavior** is defined in the node
script, per the developer's ask:

The shipped API is flat camelCase functions on the existing `input` table, so
the raw polls and the action layer coexist and a project can migrate one call at
a time.

```lua
-- a player controller node script
function update(node, dt)
    -- digital action: any of its bindings (Space OR gamepad South)
    if input.justPressed("Jump") then node.vy = JUMP_SPEED end

    -- 2D axis: WASD or left stick, deadzoned and normalised for us
    local mx, my = input.axis2("Move")
    node.pos = node.pos + vec3(mx, 0, -my) * SPEED * dt

    -- 1D axis: trigger / scroll
    zoom = zoom - input.axis1("Zoom") * dt

    -- charge: held duration is tracked for you
    if input.heldSecs("Fire") > 0.5 then charge() end
end
```

| Lua call                              | Returns                                   |
|---------------------------------------|-------------------------------------------|
| `input.action(n)`                     | bool — held                               |
| `input.justPressed(n)`                | bool — down-edge this frame/tick          |
| `input.justReleased(n)`               | bool — up-edge this frame/tick            |
| `input.heldSecs(n)`                   | number — seconds continuously held        |
| `input.axis1(n)`                      | number in `[-1,1]`                        |
| `input.axis2(n)`                      | `x, y` in the unit disk (two returns)     |
| `input.buffered(n, ticks)` / `input.consume(n, ticks)` | the input buffer         |
| `input.motion(n [, window])`          | bool — a motion just completed            |
| `input.dir()` / `input.dirHeldTicks(d)` | numpad direction, and how long held     |
| `input.setFacing(f)` / `input.facing()` | mirror directions after a cross-up      |
| `input.player(n)`                     | the same table, bound to local player `n` |
| `input.pushContext(n, opts)` / `input.popContext(n)` | manage the context stack   |
| `input.actions()` / `input.bindingsOf(n)` | drive an in-game controls screen      |
| `input.startRebind(n [, filter])` / `pendingRebind()` / `commitRebind()` / `cancelRebind()` | press-to-bind from a settings menu |

Which **domain** a call reads is decided by the pass it runs in, not by the
call: `fixedUpdate` sees the tick domain (the one with history), `update` and
`lateUpdate` see the frame domain. A predicted node's `update` runs on the tick
clock, so it reads tick input too — that's what keeps client and server
resolving the same edges.

## Editor UX — the action-map editor

A single panel in the editor (egui, dark/retro theme — ADR-0004), deliberately
**not** a property-soup inspector. One row per action/axis; bindings are chips.

```
 ┌─ Project Settings ▸ Input ────────────────────────── input.ron ── [⟲] ┐
 │ ACTIONS                                                                │
 │  Jump      [⌨ Space] [🎮 South]                            [＋] [🗑]  │
 │  Punch     [⌨ J    ] [🎮 West ]                            [＋] [🗑]  │
 │  Taunt   ⚪ ⚠ unbound                                      [＋] [🗑]  │
 │ USED IN SCRIPTS, NOT IN THE MAP                                        │
 │  ⚠ Block                            fighter.lua:42  action     [add]   │
 │ AXES 2D                                                                │
 │  Move      [⌨ WASD] [🎮 L-Stick dz0.15]        SOCD: Neutral ⏷        │
 │ MOTIONS                                                                │
 │  qcf  qcb  dp  rdp  hcf  hcb  dd  ff  bb  chargeF  chargeU             │
 ├─ LIVE ─────────────────────────────────────────────────────────────────┤
 │ ● Jump  ○ Punch  ○ Taunt     Move: (+0.82, -0.11)     P1 🎮 Xbox pad   │
 └────────────────────────────────────────────────────────────────────────┘
```

- **The action list is scanned out of your scripts.** Every action, axis and
  motion the project's Lua references, deduped, with the first `file:line` on
  hover. An entry a script uses but the map doesn't define is flagged with ⚠ and
  one click adds it — that's a control which silently does nothing, and it's the
  failure worth surfacing above all others. `⚪` marks the reverse: bound, but
  nothing references it.
- **Add a binding** two ways. `＋` arms **press-to-bind** — the fast path when
  the device is in your hand, through the same code path a shipped game's
  settings menu uses; Escape always cancels. `▾` opens a **picker** listing
  every key, mouse button, pad button and pad axis, which needs **no hardware
  connected at all**: laying out controller bindings on a laptop, or adding
  P2's while only one pad is plugged in, is entirely normal. Click a chip to
  remove it.
- **add missing starter bindings** fills gaps only — it binds an entry that was
  created from the ⚠ warning (those land *unbound* by design) and adds any
  starter action the map lacks, while leaving your own actions, bindings and
  SOCD choices exactly as they are.
- **Live tester** lights up as you mash, *without* entering Play. It resolves
  the real devices independently of gameplay input, which is deliberate: you
  edit bindings with the game view unfocused, which is exactly when gameplay
  input is neutral.
- **Still polling raw keys** lists every legacy `input.key(...)` call site — the
  migration worklist, and the set of calls that read neutral under prediction.
- The window is a **constant** width and the list scrolls at a fixed height, so
  adding actions never resizes it under you.

## The fighter layer — buffering, motions, SOCD

Originally listed as out of scope; **now shipped**, because the pieces need a
per-tick history ring that a script can't reconstruct correctly under rollback
(ADR-0024 §1). `floptle-input::history` keeps 180 ticks per player.

Everything here reads the **tick** domain — call it from `fixedUpdate`. Motion
windows are counted in ticks so a player on 144 Hz and one on 60 Hz get
identical leniency, and a replay reproduces the same answer.

```lua
-- an input buffer: a punch pressed a few frames early still lands
if input.buffered("Punch", 4) then
    input.consume("Punch", 4)      -- spend it, so it fires ONCE
    punch()
end

-- a special: motion first, so qcf+P never comes out as a bare punch
if input.motion("qcf") and input.buffered("Punch", 4) then ... end

input.dir()               -- numpad notation, 1-9 (5 = neutral, 6 = forward)
input.dirHeldTicks(4)     -- build your own charge/leniency rules
input.setFacing(-1)       -- mirror directions after a cross-up
```

**Numpad notation**, from the character's point of view:

```
  7 8 9        up-back    up     up-forward
  4 5 6   =    back     neutral    forward
  1 2 3        dn-back   down   dn-forward
```

`setFacing` is how "forward" stays meaningful after the characters swap sides:
directions are mirrored before they reach the history, so `motion("qcf")` keeps
meaning *toward the opponent*. The engine has no opinion about who faces where.

Seeded motions: `qcf` `qcb` `dp` `rdp` `hcf` `hcb` `dd` `ff` `bb` `chargeF`
`chargeU`. Edit their directions and windows in `input.ron`.

**SOCD** — what happens when opposing directions are held at once, which a
leverless controller does trivially — is per-axis in the map: `Neutral` (both
cancel; the tournament standard, and the default), `LastWins` (the newer
direction takes over with no neutral frame, so a player can pivot), or
`Positive`/`Negative` priority.

What stays out: the **move list**. Which motion plus which button makes which
attack is the game's business — see `assets/scripts/fighter.lua` for a worked
example including local versus off a single script.

## The shipped default scripts

`freelook`, `first_person`, `third_person`, `third_person_camera`, `character`
and `sword` are all written against the starter names, so **every one of them
plays on a keyboard, on a gamepad, or on both at once** with nothing to
configure. A new project seeds `input.ron` alongside them.

Two tests guard that, because a drifting name here means a brand-new project
whose camera silently cannot move:

- every action/axis/motion the shipped scripts reference exists in
  `InputMap::starter()` (`input_scan.rs`), and
- none of them polls a raw device any more.

Notable conversions:

| Was | Now |
|---|---|
| `input.key("w")` … four keys, hand-normalised | `input.axis2("Move")` |
| `input.button(1)` + `input.mouse_delta()` | `input.axis2("Look")`, gated + rate-converted |
| `input.pressed("space")` | `input.justPressed("Jump")` |
| `input.key("shift")` | `input.action("Sprint")` |
| `input.scroll()` | `input.axis1("Zoom")` |
| double-tap W/A/S/D to run | double-tap detected on the **Move axis** leaving rest — so a stick flick arms it exactly like tapping a key |

`third_person_camera` reads `LookFree` instead of `Look` while it owns the
cursor (shift lock, first person): same bindings, minus the hold-to-drag gate,
because there is no free pointer left to protect.

## Local multiplayer

`input.player(n)` returns the whole API bound to another local player (1-based).
Both characters run the *same* script; the slot rides in as an ordinary param:

```lua
defaults = { player = 1 }
local me = input.player(params.player)
if me.justPressed("Punch") then ... end
```

Pad slots are claimed by device **UUID**, so a player whose battery dies
mid-match returns to their own character on replug instead of everyone being
renumbered. Set the player count in Project Settings → Input.

## Multiplayer: actions ride the wire

`NetInput` carries an action bitmask plus axis values — no keys. A pad player
and a keyboard player who press "Jump" produce byte-identical commands, so the
one-script prediction model holds across devices.

Two consequences worth knowing:

- Raw `input.key/pressed/released/button/clicked` read **neutral** on a
  `Predicted` node (client, server and replay alike). An unmigrated controller
  visibly does nothing rather than quietly desyncing. The Input settings list
  every raw-key call site so the migration has a worklist.
- Actions are indexed by their **position** in `input.ron`, so the handshake
  compares `InputMap::hash()` and refuses a mismatched peer. Personal rebinds
  don't affect the hash — rebinding never locks you out of a session.

## Out of scope

- **A combo / move-list DSL.** The engine gives you edges, hold times, buffers
  and motions; which of those makes which attack is the game's.
- **Full input recording / replay** (deterministic capture for demos/TAS).
  The fixed-step sim makes this *possible* later, but it's not a launch feature.
- **On-screen virtual touch controls** — a UI-system concern if mobile ever lands
  (see [`./ui.md`](./ui.md)), not part of the action map.
