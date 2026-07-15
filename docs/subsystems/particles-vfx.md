# Particles & VFX (`floptle-vfx`)

> **Superseded in part** by [../particle-system-proposal.md](../particle-system-proposal.md)
> (2026-07-03): tracks absorb groups, emit events become draggable clips + bursts +
> automation lanes, mesh particles and field integration are added. This doc stays as
> the original design record until the implementation lands and it is rewritten.

Timeline-driven particle authoring: name an effect, give it a lifetime, drop
particle groups onto a video-editor-style timeline, and shape every property
with a constant or a hand-drawn curve.

> Reads on: [ADR-0008 Object pooling](../decisions/0008-object-pooling.md) ·
> [Shaders](./shaders.md) · [Editor](./editor.md). Crate: `floptle-vfx`
> (depends on `floptle-core`, `floptle-render`).

## Why this exists

Other engines bury particle authoring under scavenger-hunt panels — you spelunk
through twenty collapsible modules to fade an alpha. Floptle's bet is the
opposite: **a timeline you already understand** (like a video editor) plus
**progressive disclosure** — every knob exists, but you only see the ones you
reach for. The flagship workflow is "make `360Slash` in two minutes," not "find
the right module."

Three rules govern the whole design:

1. **The timeline is the truth.** An effect *is* a lifetime plus events on a
   timeline. Emission, groups, and previews all live there.
2. **Every property is a value-or-curve.** A single union type. Constant by
   default; a hover-corner graph icon promotes it to a drawn curve. Nothing else
   changes in the inspector.
3. **Pools by default.** Effects and their particles come from engine pools
   (ADR-0008). Spawning never churns the heap.

Out of scope (deliberately — see end): node-graph "VEX"-style per-particle
programming and GPU-compute fluid sims. Floptle stays the simple, fast,
timeline-driven flow.

## Data model

Everything authored serializes to RON under `vfx/*.ron`. Four nested types:
`ParticleEffect` → `[ParticleGroup]` + `Timeline` → `[Track]` of events;
properties are `ValueOrCurve`; curves are keyframe `Curve`s.

### ParticleEffect

The reusable, named unit the designer spawns in-game.

```rust
struct ParticleEffect {
    name: String,            // "360Slash" — the spawn key
    lifetime: f32,           // seconds the whole effect runs (one loop period)
    playback: Playback,      // Looping | OneShot
    end: EndBehavior,        // what happens when lifetime elapses (OneShot only)
    groups: Vec<ParticleGroup>,
    timeline: Timeline,      // lifetime-long; holds emit + (future) marker events
}

enum Playback { Looping, OneShot }

enum EndBehavior {
    Destroy,   // return effect + live particles to the pool (default for OneShot)
    Persist,   // stop emitting; let existing particles live out their own lifetime
}
```

`EndBehavior` is **only shown in the inspector when `playback == OneShot`**. For
`Looping` the lifetime simply restarts at `t = 0` (step 10), so a persist/destroy
choice would be meaningless and is hidden.

### ParticleGroup

A bundle of particles that share appearance and behavior — "Crescents," "Smoke."
A group owns its look, its per-particle spawn state, and how each property
evolves over a *particle's own* lifetime.

```rust
struct ParticleGroup {
    name: String,                  // "Crescents"; namespaces its timeline track
    texture: AssetId,              // sprite/atlas for the billboard
    material: MaterialRef,         // shader-IR material (see ./shaders.md)
    blend: BlendMode,              // Alpha | Additive | Premultiplied
    particle_lifetime: ValueOrCurve, // seconds EACH particle lives (curve = vary by emit time)

    emit: EmitBehavior,            // how/when this group spawns particles
    shape: EmitShape,              // where particles are born (point/cone/sphere/edge)
    count_per_emit: ValueOrCurve,  // particles spawned per emit event

    // Per-particle properties. Each is constant OR a curve over the PARTICLE's life.
    velocity: ValueOrCurve,        // Vec3; initial dir+speed, optionally curved over life
    rotation: ValueOrCurve,        // radians; spin over life
    size:     ValueOrCurve,        // scale over life
    color:    ValueOrCurve,        // RGBA; color-over-time / fade lives here
    // (gravity, drag, etc. are additional ValueOrCurve fields, surfaced on demand)
}
```

Key distinction: **effect lifetime vs particle lifetime.** The effect's
`lifetime` drives the *timeline* (when groups emit). A *particle's* lifetime
(`particle_lifetime`) is the domain of that group's property curves — a `size`
curve runs from 0→1 over *one particle's* life, independent of where on the
effect timeline it was born.

### EmitShape

```rust
enum EmitShape {
    Point,
    Cone   { angle: f32, radius: f32 },
    Sphere { radius: f32 },
    Edge   { length: f32 },        // a line — handy for slash arcs
}
```

### Timeline & EmitBehavior

The timeline is lifetime-long and holds one **track per group**. Each track
carries that group's `Emit` events. Two ways to author them (step 8):

- **Constant rate** → the engine **auto-draws** evenly spaced emit nodes named
  `GroupName/Emit` (e.g. `Crescents/Emit`). Change the rate → nodes redraw.
  Toggle it off → nodes vanish. **Defaults to nothing** (no emission until you
  ask for it).
- **Manual** → you hand-place individual `Emit` events anywhere on the track.
  When the playhead crosses one during playback, that group **fires** an
  emission (spawns `count_per_emit` particles).

Both modes can coexist on a track (a steady rate plus a hand-placed burst).

```rust
struct Timeline {
    duration: f32,                 // == effect.lifetime
    tracks: Vec<Track>,            // one per group, in group order
}

struct Track {
    group: GroupId,
    events: Vec<Emit>,             // sorted by t
}

struct Emit {
    t: f32,                        // seconds along the timeline
    source: EmitSource,            // Auto (rate-generated) | Manual (hand-placed)
}

enum EmitSource { Auto, Manual }

enum EmitBehavior {
    None,                          // DEFAULT — group emits nothing on its own
    Rate { every: f32 },          // "once every 0.2s" → auto nodes across lifetime
    Manual,                       // only the hand-placed Emit events fire
}
```

`Emit { source: Auto }` events are **derived**, not hand-edited: they're
regenerated from `EmitBehavior::Rate` whenever the rate changes and are skipped
by RON serialization (only `Rate { every }` is stored). Manual events are stored
verbatim.

### ValueOrCurve — the property union

The heart of the inspector. One type the inspector renders identically
everywhere, with the hover-corner graph affordance:

```rust
enum ValueOrCurve {
    Const(Value),                  // single constant — the default
    Curve(Curve),                  // drawn over the particle's lifetime [0..1]
}

enum Value { F32(f32), Vec3(Vec3), Rgba([f32; 4]) }
```

### Curve

```rust
struct Curve {
    keys: Vec<Key>,                // the "nodes" you draw
    extrapolate: Extrapolate,      // Clamp | Repeat (before first / after last key)
}

struct Key {
    t: f32,                        // 0..1 along the particle's normalized lifetime
    v: Value,
    interp: Interp,                // how we reach the NEXT key
    in_tan: f32,                   // tangents (Bezier handles)
    out_tan: f32,
}

enum Interp { Constant, Linear, Bezier }
```

A scalar field (`size`, `rotation`, alpha) uses `Value::F32`; `color` uses
`Value::Rgba` (RGB and A interpolate together, giving color-shift *and* fade in
one curve); `velocity` uses `Value::Vec3`.

### RON example — `vfx/360Slash.ron`

A 360 slash: a fast band of glowing `Crescents`, plus a subtle `Smoke` burst.

```ron
ParticleEffect(
    name: "360Slash",
    lifetime: 0.6,
    playback: OneShot,
    end: Destroy,
    groups: [
        ParticleGroup(
            name: "Crescents",
            texture: "assets/vfx/crescent_slash.png",
            material: Shader("shaders/slash_glow.flsl"),
            blend: Additive,
            particle_lifetime: Const(F32(0.35)),
            emit:  Rate(every: 0.05),          // dense band over the swing
            shape: Edge(length: 1.4),
            count_per_emit: Const(F32(3.0)),
            velocity: Curve(Curve(            // fast out, then decel
                keys: [
                    Key(t: 0.0, v: Vec3((0.0, 0.0, 9.0)), interp: Bezier, in_tan: 0.0, out_tan: -8.0),
                    Key(t: 1.0, v: Vec3((0.0, 0.0, 1.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
            rotation: Const(F32(0.0)),
            size: Curve(Curve(                // pop in, taper out
                keys: [
                    Key(t: 0.0, v: F32(0.2), interp: Bezier, in_tan: 0.0, out_tan: 4.0),
                    Key(t: 0.3, v: F32(1.0), interp: Bezier, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 1.0, v: F32(0.0), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
            color: Curve(Curve(               // white-hot → cyan → fade alpha
                keys: [
                    Key(t: 0.0, v: Rgba((1.0, 1.0, 1.0, 1.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 0.5, v: Rgba((0.3, 0.9, 1.0, 1.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 1.0, v: Rgba((0.1, 0.4, 0.8, 0.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
        ),
        ParticleGroup(
            name: "Smoke",
            texture: "assets/vfx/soft_cloud.png",
            material: Shader("shaders/soft_smoke.flsl"),
            blend: Alpha,
            particle_lifetime: Const(F32(0.9)),
            emit:  Manual,                    // one hand-placed burst, see timeline
            shape: Cone(angle: 35.0, radius: 0.3),
            count_per_emit: Const(F32(12.0)),
            velocity: Const(Vec3((0.0, 0.6, 0.0))),
            rotation: Curve(Curve(
                keys: [
                    Key(t: 0.0, v: F32(0.0), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 1.0, v: F32(1.2), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
            size: Curve(Curve(                // grow as it dissipates
                keys: [
                    Key(t: 0.0, v: F32(0.3), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 1.0, v: F32(1.6), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
            color: Curve(Curve(               // grey, fade out
                keys: [
                    Key(t: 0.0, v: Rgba((0.7, 0.7, 0.75, 0.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 0.2, v: Rgba((0.6, 0.6, 0.65, 0.5)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                    Key(t: 1.0, v: Rgba((0.5, 0.5, 0.55, 0.0)), interp: Linear, in_tan: 0.0, out_tan: 0.0),
                ],
                extrapolate: Clamp,
            )),
        ),
    ],
    timeline: Timeline(
        duration: 0.6,
        tracks: [
            // Crescents: rate-driven → Auto emits are derived, not stored here.
            Track(group: 0, events: []),
            // Smoke: one hand-placed burst a beat after the swing starts.
            Track(group: 1, events: [ Emit(t: 0.12, source: Manual) ]),
        ],
    ),
)
```

## The value-or-curve affordance

Precise editor behavior for **every** property field (step 7):

- A property renders as its plain editor (a drag-float, a Vec3 triple, a color
  swatch). This is the `Const` case and the default.
- **Hover the property's corner** → a small **graph icon** fades in at the
  top-right of the field. It is invisible until hovered — never thrown in your
  face (the engine's progressive-disclosure rule).
- **Click the graph icon** → the field promotes to `Curve` and opens the curve
  editor (below), seeded with two keys (start = current constant, end = same).
  The inline editor now shows a tiny **sparkline** of the curve instead of a
  single value; clicking the sparkline reopens the editor.
- **Right-click the sparkline → "Make constant"** demotes back to `Const`, taking
  the curve's value at `t = 0`.

The inspector code is uniform: it calls `ui.value_or_curve(label, &mut field)`
and that helper owns the hover icon, promotion/demotion, and sparkline. Because
the curve domain is always the **particle's normalized lifetime `[0,1]`**, the
same widget drives size, rotation, alpha, color-shift, and velocity with no
special cases.

## Timeline semantics

```
 t=0.00                         playhead │                         t=0.60 (lifetime)
 ┌────────────────────────────────────────────────────────────────────────────────┐
 │ ▶  ◼  ⟲loop   ⏮  ⏭     [ 0.21s / 0.60s ]            zoom ──●────  □ snap         │
 ├──────────────┬─────────────────────────────────────────────────────────────────┤
 │ Crescents    │ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮ ▮      (Auto: rate 0.05s — greyed, derived)│
 │ ⊳ Emit       │                              │                                   │
 ├──────────────┼──────────────────────────────│───────────────────────────────────┤
 │ Smoke        │            ◆                  │      (Manual emit @ 0.12s)         │
 │ ⊳ Emit       │            └ drag me          │                                   │
 └──────────────┴──────────────────────────────│───────────────────────────────────┘
                                                ▲ playhead — scrub to preview live
```

- **Playhead** advances at real time during playback; the live preview viewport
  simulates the effect deterministically from `t = 0` so scrubbing is exact.
- **Looping restart:** for `Playback::Looping`, when the playhead reaches
  `lifetime` it wraps to `0` and re-emits. Live particles are *not* killed at the
  wrap — they finish their own `particle_lifetime`.
- **Emit firing:** each frame the sim advances the playhead and fires every
  `Emit` event the playhead **crossed** this step (half-open interval
  `(prev_t, t]`), so no emit is missed at low frame rates and none double-fires.
  Firing an event spawns `count_per_emit` particles into that group, sampling
  initial properties from each `ValueOrCurve` at the particle's `t = 0`.
- **Auto vs Manual on one track:** Auto nodes are drawn greyed/locked (regenerate
  from the rate); Manual nodes are solid diamonds you drag. Dragging an Auto node
  does nothing; editing the rate redraws them.
- **Scrubbing/preview:** dragging the playhead re-simulates to that time (cheap —
  the sim is deterministic and pooled). `snap` quantizes drops to a grid.
- **Per-group tracks** keep authoring legible: "what does Smoke do?" is one row.

## The curve / graph editor

Opens over the property when you click its graph icon. Domain is the particle's
normalized lifetime `[0,1]`; range auto-fits the values (a fade is `[0,1]`,
rotation might be `[0, 2π]`). You **draw nodes** and tug tangents.

```
   size  (over particle lifetime)                         [ Linear ▾ ] [ fit ] [ × ]
 1.0 ┤            ●╮                                        click empty space = add node
     │          ╱   ╲___                                    drag node = move
 0.8 ┤        ╱        ╲___                                 drag handle ╴╴ = tangent
     │      ╱              ╲__                              right-click node = delete
 0.6 ┤    ╱                   ╲__
     │  ╱                        ╲___
 0.4 ┤╱                              ╲__
     │                                  ╲___
 0.2 ●  ◀ in-tangent                       ╲────────────●
     │                                                  ▲ out (alpha 0 = faded out)
 0.0 ┼────┬────┬────┬────┬────┬────┬────┬────┬────┬────┼
    0.0  0.1  0.2  0.3  0.4  0.5  0.6  0.7  0.8  0.9  1.0   ← normalized lifetime
```

- **Add / move / delete nodes** with click / drag / right-click.
- **Tangents:** selecting a `Bezier` node exposes draggable handles (`in_tan`,
  `out_tan`); `Linear` and `Constant` (stepped) modes need none.
- **Range fit** auto-scales the vertical axis; a manual range lock is available.
- **Color curves** swap the value-axis for a **gradient strip** above the graph —
  you place color stops and drag alpha as a normal curve underneath, so a single
  editor handles color-shift + fade.
- Live preview updates as you drag (the viewport re-simulates), so you *see* the
  slash sharpen while you pull a tangent.

## Runtime & simulation

Data-oriented and GPU-friendly. A spawned effect is an **instance**; each group's
live particles live in **structure-of-arrays (SoA)** buffers for tight,
cache-friendly updates and direct upload.

```rust
struct GroupParticles {        // SoA — one set of arrays per live group instance
    pos:    Vec<Vec3>,
    vel:    Vec<Vec3>,
    age:    Vec<f32>,          // seconds since birth
    life:   Vec<f32>,         // this particle's total lifetime
    seed:   Vec<u32>,         // per-particle RNG for variation
    count:  usize,            // live particles; arrays are pool-sized capacity
}
```

Per frame, the VFX sim (variable timestep, after anim — see ARCHITECTURE §3):

1. **Advance playheads** of all live effect instances; fire crossed `Emit`
   events → append births to the relevant `GroupParticles` (from the pool, no
   alloc).
2. **Integrate**: `age += dt`; sample each group property's `ValueOrCurve` at
   `age/life` to derive `size/rotation/color/velocity`; integrate position.
   Curves are evaluated from a flattened key table (branch-light, SIMD-friendly).
3. **Retire** particles where `age >= life` by swap-remove (keeps arrays dense).
4. **Build draw batches**: one batch per (group material, blend, texture). The
   billboards' instance data (pos, size, rotation, color) is written to a
   per-batch instance buffer.

**Mapping to the renderer:** each batch is a single instanced draw of a quad,
fed to `floptle-render` with the group's **shader-IR material** (compiled to WGSL
per ./shaders.md). Surreal looks — feedback, color transport, SDF warps — come
free because particle materials are ordinary IR materials.

**Sorting & blending:** `Additive` batches are order-independent and drawn first
(no sort). `Alpha` batches are **depth-sorted back-to-front** per camera before
upload. Soft particles (depth-fade against the scene buffer) are a material
feature, not a sim concern.

**Pooling (ADR-0008):** both layers come from pools and never churn the heap.

- An **effect-instance pool** keyed by effect name: `vfx.play` *takes* an
  instance (pre-warmed for common effects); on finish it *returns* and resets.
- Per-group **particle capacity** is pool-backed SoA; births reuse retired slots.
- `OneShot` + `EndBehavior::Destroy` auto-returns the instance the frame its last
  particle retires. `Persist` returns once particles drain. `Looping` returns
  only on explicit `stop`.

Threading: the sim is a per-instance, embarrassingly-parallel job; it starts
main-thread but the SoA layout lets it move to the job pool unchanged later.

## Scripting API (Lua)

Spawn stored effects anywhere (step 11). Curated `vfx` table (per ARCHITECTURE
§7), pool-backed:

```lua
-- one-shot at a transform; auto-returns to the pool when finished
vfx.play("360Slash", node.transform)

-- attach to a node so it follows (e.g. a trail on the sword)
local h = vfx.attach("360Slash", sword)

-- looping handle you control
local fire = vfx.play("Campfire", torch.transform, { loop = true })
fire:stop()        -- ends emission; particles live out, then instance returns
fire:set_param("rate", 0.1)   -- tweak an exposed group param at runtime

-- fire-and-forget at a world point
vfx.play_at("Hit", hit_position)
```

`play` returns immediately with a lightweight handle. For one-shots you can
ignore it — the engine returns the instance to its pool automatically on finish.
Handles are safe after auto-return (calls become no-ops), so scripts can't touch
freed instances.

## Editor integration

Lives in the editor's **VFX workspace** (see ./editor.md), dark/retro theme. Two
panes plus the shared timeline:

- **Effect inspector** (left): effect-level fields (name, lifetime, playback,
  end behavior) and the group list. Selecting a group swaps in its properties,
  the **Emit Behavior** section, and the value-or-curve fields.
- **Timeline** (bottom): the per-group tracks described above; the dock that
  every other pane previews against.
- **Preview viewport** (right): live, deterministic simulation that scrubs with
  the playhead.

### Create-new-effect wizard (steps 1–4)

A four-step modal that mirrors the developer's flow exactly:

```
 ┌─ New Particle Effect ─────────────────────────────────┐
 │ 1. Name         [ 360Slash                          ] │
 │ 2. Lifetime     [ 0.60 ] s                            │
 │ 3. Emission     ( ) Looping    (•) One-time play      │
 │ 4. End behavior (•) Destroy    ( ) Persist            │  ← hidden if Looping
 │                                        [ Cancel ][ Create ] │
 └───────────────────────────────────────────────────────┘
```

On **Create**, the editor writes a minimal `vfx/<name>.ron` (no groups, empty
timeline of `duration == lifetime`) and opens the workspace with the empty
lifetime-long timeline ready (step 4). From there you **Add Group** ("Crescents,"
then "Smoke"), set each group's Emit Behavior, and draw curves. Saving round-trips
to RON; the asset database assigns a stable id and hot-reloads spawned instances.

## Out of scope

To keep the flow simple and fast, Floptle deliberately does **not** ship:

- **Node-graph / "VEX"-style per-particle programming.** Behavior is timeline
  events + value-or-curve properties, not a per-particle scripting graph.
  (Custom *visual* weirdness still comes from shader-IR materials, ADR-0007.)
- **GPU-compute fluid/smoke simulation.** "Smoke" here is billboard particles
  with curves, not a Navier–Stokes solver. The sim stays the lightweight,
  deterministic, pooled, timeline-driven system described above.
```
