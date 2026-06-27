# Input & action mapping (`floptle-input`)

Bind a named **Action** to many inputs at once — any keyboard key, any mouse
button, sticks/triggers/buttons on a gamepad — then decide what the action *does*
in a node script. Devices are an implementation detail; gameplay code talks to
actions and axes, never raw keys.

> Reads on: [ADR-0003 Lua scripting](../decisions/0003-scripting-lua.md) ·
> [ADR-0005 ECS/Node facade](../decisions/0005-scene-model-ecs-node-hybrid.md).
> Sits beside: [`./scene-and-nodes.md`](./scene-and-nodes.md) (scripts that read
> input) · [`./ui.md`](./ui.md) and [`./camera-and-dialogue.md`](./camera-and-dialogue.md)
> (which own input *contexts*). Where it runs: [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §3.
> Crate `floptle-input` depends only on `floptle-core`.

## Where it sits in the frame

`input.update` runs first, right after the winit poll and before any script step
(ARCHITECTURE §3). It drains this frame's OS/gamepad events, advances every
action's state machine, and exposes an immutable snapshot that scripts read all
frame. Nothing else writes input state mid-frame, so two scripts reading
`"Jump"` see the same answer.

```
winit events ┐
gilrs events ├─▶ input.update ─▶ [resolve bindings per context] ─▶ ActionState snapshot
mouse motion ┘                                                         │
                                              scripts.on_fixed_update / on_update read it
```

## Devices & sources

One flat `Source` enum covers everything bindable. "ALL key types / ALL mouse
buttons" is the point: we wrap winit's full key set and button range, we don't
curate a subset.

```rust
enum Source {
    Key(KeyCode),               // every winit physical key (letters, F-keys, mods, numpad…)
    MouseButton(MouseButton),   // Left/Right/Middle + Back/Forward + Other(u16)
    MouseAxis(MouseAxis),       // MotionX, MotionY, ScrollX, ScrollY (relative deltas)
    Pad { id: PadId, ctrl: PadControl },   // see below; id resolves which controller
}

enum PadControl {
    Button(gilrs::Button),      // South/East/West/North, bumpers, dpad, stick clicks, start…
    Axis(gilrs::Axis),          // LeftStickX/Y, RightStickX/Y, LeftZ/RightZ (triggers)
}

enum PadId { Any, Slot(u8) }    // Any = "any connected pad"; Slot(n) = local player n
```

- **Keyboard** — `KeyCode` is winit's *physical* key (layout-independent), so a
  binding to the `W` position is stable across QWERTY/AZERTY. (A future option
  flag can switch a binding to logical/character mapping for text-y rebinds.)
- **Mouse** — buttons *and* relative motion/scroll are first-class `Source`s, so
  "look" (mouse motion) and "zoom" (scroll) bind exactly like a stick axis.
- **Gamepad** — `gilrs` gives normalized buttons/sticks/triggers and **hot-plug**
  events. We map gilrs ids → stable `Slot`s as pads connect/disconnect; a slot
  that drops out goes inert (its actions read released) and reattaches on replug
  without rebinding. `PadId::Any` is the single-player default.

## The action map

The action map is the whole public model: **digital Actions** and **analog Axes**,
each fed by a list of bindings. Add as many bindings as you like; *any* of them
fires the action.

```rust
struct ActionMap {
    actions: Vec<Action>,       // digital: pressed/released/held
    axes1:   Vec<Axis1>,        // 1D analog: triggers, mouse wheel, A/D
    axes2:   Vec<Axis2>,        // 2D analog: WASD, a stick, mouse motion
}

struct Action {
    name: String,               // "Jump"
    bindings: Vec<Binding>,     // ANY of these triggers it (OR)
}

struct Binding {
    source: Source,
    modifiers: Vec<Source>,     // optional chord, e.g. [Key(LCtrl)] for Ctrl+S
}
```

**Digital state per action**, recomputed each `input.update` from the OR of its
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
    Stick { id: PadId, x: gilrs::Axis, y: gilrs::Axis, deadzone: f32, sensitivity: f32 },
    Keys  { up: Source, down: Source, left: Source, right: Source },   // WASD-style
    Mouse { sensitivity: f32 },                                         // motion deltas
}
```

- **Deadzone** is radial for sticks (kills drift near center), per-binding so a
  worn stick can be tightened without touching the keyboard binding.
- **Sensitivity / response curve** scales magnitude (linear default; an optional
  `expo` curve for fine aim). Output is clamped to the unit disk for `Axis2`.
- **Dominant source:** when both a stick and WASD are bound to one axis, the
  larger-magnitude source this frame wins — no fighting if the player bumps both.

### RON input map — `input/default.ron`

The whole thing is RON (ARCHITECTURE §8), diffable and hand/AI-editable.

```ron
ActionMap(
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
RON map, so player settings are just an overlay map merged over `default.ron`.

## Scripting API (Lua)

The curated `input` table (ARCHITECTURE §7) is the only thing gameplay touches —
it reads the snapshot, never devices. Action **behavior** is defined in the node
script, per the developer's ask:

```lua
-- a player controller node script
function on_update(dt)
    -- digital action: any of its bindings (Space OR gamepad South)
    if input:action("Jump"):just_pressed() then
        self.velocity.y = JUMP_SPEED
    end

    -- 2D axis: WASD or left stick, deadzoned & normalized for us
    local mv = input:axis2("Move")          -- {x, y} in the unit disk
    self.node:translate(mv.x * SPEED * dt, 0, mv.y * SPEED * dt)

    -- 1D axis: trigger / scroll
    camera.fov = camera.fov - input:axis1("Zoom") * dt

    -- charge-jump: held duration is tracked for you
    if input:action("Fire"):held_secs() > 0.5 then charge() end
end
```

| Lua call                          | Returns                                       |
|-----------------------------------|-----------------------------------------------|
| `input:action(n):pressed()`       | bool — held this frame                        |
| `input:action(n):just_pressed()`  | bool — down-edge this frame                   |
| `input:action(n):just_released()` | bool — up-edge this frame                     |
| `input:action(n):held_secs()`     | f32 — seconds continuously held               |
| `input:axis1(n)`                  | f32 in `[-1,1]`                               |
| `input:axis2(n)`                  | `{x, y}` in the unit disk                     |
| `input:push_context(name)` / `:pop_context(name)` | manage the context stack       |

Combos and buffers are intentionally *not* here — a script builds them atop
`just_pressed` + `held_secs` (see Out of scope).

## Editor UX — the action-map editor

A single panel in the editor (egui, dark/retro theme — ADR-0004), deliberately
**not** a property-soup inspector. One row per action/axis; bindings are chips.

```
 ┌─ Input Map ─────────────────────────── default.ron ──── [ + Action ][ + Axis ]┐
 │ ACTIONS                                                                        │
 │  Jump      [⌨ Space] [🎮 South] [+]                                            │
 │  Fire      [🖱 LMB ] [🎮 R-Trig] [+]                                           │
 │  Save      [⌨ Ctrl+S]                  [+]                                     │
 │ AXES 2D                                                                        │
 │  Move      [⌨ WASD ] [🎮 L-Stick dz .15] [+]                                   │
 │  Look      [🖱 Motion ×.08] [🎮 R-Stick] [+]                                   │
 │ AXES 1D                                                                        │
 │  Zoom      [🖱 Wheel] [🎮 Triggers] [+]                                        │
 └────────────────────────────────────────────────────────────────────────────────┘
```

- **Add a binding** → click `[+]`, then **press the input** (same press-to-bind
  path as runtime rebinding) — keyboard, mouse, or controller. No dropdown of 200
  key names unless you click the chip to pick manually.
- **A chip** shows device icon + label; click to edit deadzone/sensitivity in a
  tiny popover (not inline columns). Right-click → remove.
- **Live tester** strip at the bottom lights up actions/axes as you mash inputs,
  so you confirm a binding without entering play mode.
- **Contexts** are a small side list; you tag which actions belong to which
  context, nothing more.

## Out of scope

- **Combo / input-buffer systems** (fighting-game motions, leniency windows).
  The action layer exposes clean edges and hold-times; a game builds combos in
  script atop them. Baking a combo DSL into the engine fights "lightweight."
- **Full input recording / replay** (deterministic input capture for demos/TAS).
  The fixed-step sim makes this *possible* later, but it's not a launch feature.
- **On-screen virtual touch controls** — a UI-system concern if mobile ever lands
  (see [`./ui.md`](./ui.md)), not part of the action map.
