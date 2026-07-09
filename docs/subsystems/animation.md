# Animation (`floptle-anim`)

Skeletal clips from Blender, driven by a **small** state machine, blended into a
final pose, with timeline **events** that fire gameplay. Built for dreamlike
adventure and snappy combat — *not* Unreal-tier graph sprawl.

> Asset path: [`../decisions/0006-asset-pipeline-gltf.md`](../decisions/0006-asset-pipeline-gltf.md).
> Scripting: [`../decisions/0003-scripting-lua.md`](../decisions/0003-scripting-lua.md) ·
> Editor: [`../decisions/0004-editor-egui.md`](../decisions/0004-editor-egui.md).
> Siblings: [`./scene-and-nodes.md`](./scene-and-nodes.md) ·
> [`./particles-vfx.md`](./particles-vfx.md) · [`./physics.md`](./physics.md).
> Where it sits: [`../ARCHITECTURE.md`](../ARCHITECTURE.md).

`floptle-anim` depends on `floptle-core` (math · ECS · node facade · time) and feeds
poses to `floptle-render`. The design bet: a developer making surreal adventures and
flashy fights wants **clean simple states** (Idle/Run/Jump) plus **fast attack and
movement** states, and explicitly *does not* want a 200-node blackboard graph. So we
ship a lean state machine + layered blending, and stop there.

## Clips from Blender (glTF)

Animations import from glTF (ADR-0006) alongside the mesh. A clip is **sampled**
skeletal animation: per-joint TRS keyframe channels over a skeleton (the joint
hierarchy from the glTF skin).

```rust
struct Skeleton {
    joints: Vec<Joint>,          // parent index + inverse-bind matrix, topologically sorted
}
struct Joint { name: String, parent: Option<u16>, inverse_bind: Mat4 }

struct AnimClip {
    name: String,                // "Run", "Attack_360Slash"
    duration: f32,
    channels: Vec<Channel>,      // one per (joint, property)
    events: Vec<NotifyEvent>,    // timeline events (below)
    looping: bool,
}
struct Channel { joint: u16, prop: Prop /*T|R|S*/, keys: Sampler /* time→value */ }
```

**Sampling → skinning.** Each frame, for the active state(s) we sample channels at
the clip's local time (interpolating keys), compose local TRS into local matrices,
walk the joint hierarchy once to get **global** joint transforms, then multiply by
each joint's `inverse_bind` to get **skinning matrices** uploaded as a joint palette:

```
local_pose[j]  = sample(clip, j, t)                 // TRS → Mat4
global[j]      = global[parent[j]] * local_pose[j]  // single ordered pass
skin[j]        = global[j] * inverse_bind[j]        // → GPU joint palette
```

Vertex skinning (4-weight linear blend) runs in the vertex shader; `floptle-anim`
only produces the palette. See [`./renderer.md`](./renderer.md).

## State machine (lightweight)

The core authoring object. A per-character graph of **states** (each playing a clip
or a blend), with **transitions** guarded by **parameters** that scripts and input
drive. Deliberately flat: states + transitions + a parameter table, no sub-graphs,
no nested blackboards.

```rust
struct StateMachine {
    params: Vec<Param>,          // shared scratch the conditions read
    states: Vec<State>,
    any: Vec<Transition>,        // "Any State" transitions (e.g. -> Hit, -> Attack)
    entry: StateId,
}
enum Param { Bool(bool), Float(f32), Trigger(bool) }   // Trigger auto-resets when consumed

struct State {
    name: String,
    motion: Motion,              // Clip(id) | Blend1D{param, clips} | Additive{base, layer}
    speed: f32,                  // playback scale (e.g. tie Run speed to velocity)
    transitions: Vec<Transition>,
}
struct Transition {
    to: StateId,
    conditions: Vec<Cond>,       // ALL must hold: param op value
    duration: f32,               // crossfade seconds (0.05–0.12 for combat)
    exit_time: Option<f32>,      // normalized; gate on clip progress (attacks)
    interruptible: bool,         // can a new transition cut in mid-blend?
}
enum Cond { Bool(ParamId, bool), FloatCmp(ParamId, Op, f32), Trigger(ParamId) }
```

```
        ┌────────┐  speed>0.1   ┌────────┐  Jump(trig)  ┌────────┐
        │  Idle  │─────────────►│  Run   │─────────────►│  Jump  │
        └────────┘◄─────────────└────────┘◄─────────────└────────┘
            ▲  speed<0.1            ▲   grounded & vy<=0
            │                       │
      ──────┴───────────────────────┴──── Any State ──► Attack ──► (exit_time) Idle/Run
                          Attack(trig)
```

`Idle/Run` blends along a 1D `speed` param (`Blend1D`); attacks are `Clip` states off
**Any State** with short `duration` and an `exit_time` so they play out then return.
That covers the brief without graph sprawl.

## Blending

Two mechanisms, both cheap, composed into one final pose per frame:

- **Crossfade** between states on a transition: linearly interpolate the two sampled
  poses (per-joint TRS, `nlerp` on rotation) over `duration`. Short durations
  (~0.06–0.10s) keep combat snappy; longer for dreamy locomotion.
- **Additive layers**: a layer stores a clip as a *delta* from a reference pose and
  adds it on top of the base. The flagship use is **movement (base) + attack
  (upper-body additive)** so the character can swing while running.

```
 base layer:   Run ───────────┐
 add layer:    UpperBodySlash ─┼─► additive add (masked to spine+arms) ─► final pose
 (mask: joints the layer affects; unmasked joints keep the base unchanged)
```

The pose pipeline each frame: `evaluate active state(s)` → `crossfade if mid-transition`
→ `apply additive layers (masked)` → `compose skinning palette`.

## Property tracks (shipped)

Beside the three transform lanes (translation/rotation/scale), a channel can
carry **property tracks** that animate an arbitrary component field — the same
`(component, field)` addressing Lua's `getcomponent` uses. A key value is either
a **number** (opacity, colors, light intensity, slider value…) or a **string**
(a path/text field). The headline case is a UI element's `image` **swapping its
texture frame-by-frame** — sprite animation — but any exposed field works.

```rust
// floptle-anim
enum PropValue { Float(f32), Text(String) }
struct PropertyTrack { component: String, field: String,
                       times: Vec<f32>, values: Vec<PropValue>, interp: Interp }
```

Numeric lanes lerp (or step); **string lanes always step** — you don't blend two
textures. Serialized per channel as `properties: [ (component, field, times,
values, step) ]` in the `.anim.ron`, omitted when empty (back-compatible). At
apply time the sampled values flow through `floptle_script::apply_component_field`
(numbers) and `apply_component_field_str` (paths/text) — the exact setters Lua
uses, so scripts and animation poke fields identically. Authored in the
Animating tab's **▦ Property tracks** section: pick node · component · field,
then key values at the playhead (image fields get a texture picker). Property
values snap to the top active state (they don't cross-fade like transforms).

## Notify / animation events

A clip carries **events on its timeline** — the bridge from animation to gameplay.
This is how "on frame 7, spawn the 360Slash VFX" works.

```rust
struct NotifyEvent {
    time: f32,           // seconds into the clip (frame 7 @ 24fps = 0.29s)
    kind: NotifyKind,
}
enum NotifyKind {
    Vfx { effect: String, socket: Option<String> },  // -> vfx.play (./particles-vfx.md)
    Sound { id: String },                             // footstep SFX
    Window { name: String, end: f32 },                // e.g. hitbox-active [start,end]
    Script { name: String },                          // -> on_event(name, ...) in Lua
}
```

When playback time crosses an event's `time` (and once per loop, edge-triggered to
avoid double-fires across frame jitter), the engine dispatches it:

- `Vfx` → `vfx.play(effect, socket_transform)` ([`./particles-vfx.md`](./particles-vfx.md)).
- `Sound` → audio one-shot.
- `Window` → opens/closes a named gameplay window (hitbox active, i-frames); the
  collider system reads it ([`./physics.md`](./physics.md)).
- `Script` → calls `on_event(name, payload)` on the owning node's script (ADR-0003).

```
 Attack_360Slash clip:  0 ───────●───────────■═══════■──────── duration
                              Sound:swing   Window "hitbox" [0.25,0.40]
                                  └ frame 7: Vfx "360Slash" (socket "hand_r")
```

## Root motion (optional, simple)

A clip can be flagged `root_motion`: instead of the root joint animating in place,
its horizontal delta per frame is **extracted** and handed to the character
controller as displacement, so a lunge attack actually moves the character. Default
**off** (in-place); the state machine just plays poses. Kept intentionally minimal —
no motion warping, no curve remapping.

## Editor UX

A dark/retro ([ADR-0004](../decisions/0004-editor-egui.md)) **Animation workspace**,
two custom egui widgets:

```
┌ State Machine ───────────────────────┐ ┌ Clip: Attack_360Slash ───────────────┐
│   (Idle)══(Run)══►(Jump)             │ │ ├─────●──────────■═════■────────────┤ │
│      ╲      ╱                         │ │ 0   Sound      Window      0.6s     │ │
│       (Attack)  ◄ Any                 │ │ [+Vfx] [+Sound] [+Window] [+Script] │ │
│  params: speed▮ Jump⚡ Attack⚡        │ │ drag markers · double-click to edit  │ │
└──────────────────────────────────────┘ └──────────────────────────────────────┘
```

- **State-machine graph:** drag states, draw transitions, click a transition to edit
  conditions/duration/exit-time. The parameter list lives in a side panel.
- **Clip event track:** a timeline (shares the VFX timeline widget feel) with
  draggable notify markers; pick `Vfx`/`Sound`/`Window`/`Script` and configure inline.
- **Live preview:** scrub the playhead; the viewport plays the selected clip/state.

Everything serializes to **RON** (`anim/<character>.ron` for the state machine; clip
events ride alongside the imported glТF in `anim/*.events.ron`), diffable like the
rest of the project.

## Scripting API (Lua)

A curated `anim` table (ARCHITECTURE §7). Scripts drive parameters and react to
events; they don't poke joints.

```lua
anim.play(self, "Jump")                    -- snap to a state
anim.crossfade(self, "Attack", 0.08)       -- blend over 0.08s (snappy)
anim.set_bool(self, "grounded", true)
anim.set_float(self, "speed", v.length())  -- drives the Idle<->Run blend
anim.trigger(self, "Attack")               -- one-shot trigger param

function on_event(name, p)                  -- notify -> script (NotifyKind::Script)
    if name == "hitbox_on" then self:enable_hitbox(true) end
end
```

## Out of scope

- **Full IK solvers** (two-bone/foot-lock/look-at, FABRIK). Maybe one tiny foot-lock
  later; no general solver.
- **Motion matching** and large pose databases.
- **Mocap retargeting pipelines** — author/clean in Blender, import via glTF.
- **Ragdoll / physical animation** — could arrive later via the *optional* rigid-body
  dynamics in [`./physics.md`](./physics.md), driven by a ragdoll skeleton; not at
  launch, and not the anim crate's problem.
