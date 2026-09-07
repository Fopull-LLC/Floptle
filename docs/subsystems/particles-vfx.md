# Particles & VFX (`floptle-vfx`)

Timeline-driven particle authoring: name an effect, give it a lifetime, lay
tracks down a video-editor timeline, and shape every property with a constant, a
random range, or a hand-drawn curve. The **✱ Particles** tab is where you do it,
and it plays live while you drag.

> Reads on: [Shaders](./shaders.md) · [Editor](./editor.md) ·
> [Object pooling](./object-pooling.md). Crate: `floptle-vfx` (depends on
> `floptle-core`, `floptle-render`). Scripting: `node:particles()` in
> [lua-api.md](../lua-api.md#particles--effects-from-script).

## Why this exists

Other engines bury particle authoring under scavenger-hunt panels — you spelunk
through twenty collapsible modules to fade an alpha. Floptle's bet is the
opposite: **a timeline you already understand**, plus a curve editor, plus a
preview that never stops running. Everything an effect does is visible as
something you can see on the timeline and drag.

## The data model

An effect serializes to RON under `vfx/*.vfx.ron`. There are two levels, not
four: an **effect** owns **tracks**, and a track owns both its look and its lane
on the timeline.

### `ParticleEffect`

```rust
struct ParticleEffect {
    name: String,             // "OreBreak" — the spawn key
    lifetime: f32,            // seconds the timeline runs (one loop period)
    playback: Playback,       // Looping | OneShot
    end: EndBehavior,         // Destroy | Persist — OneShot only
    tracks: Vec<Track>,
    seed: u32,                // instances offset it, so two campfires don't march in step
    gravity_mode: GravityMode,// WorldDown | Field
}
```

`end` is shown in the Inspector only for `OneShot`; a `Looping` effect simply
restarts at `t = 0`, so a persist/destroy choice would mean nothing.

`gravity_mode` is the one field with no counterpart in other engines. `WorldDown`
is the constant every engine assumes; `Field` reads the live scene gravity field
instead, so sparks struck on a small moon fall toward *it* — see
[gravity-and-density.md](./gravity-and-density.md).

### `Track` — one visual layer *and* its timeline lane

**Track and group are one concept.** A track owns its look, its emission, its
per-particle curves and its position on the timeline: one thing you select, drag,
mute and copy. It is the unit the whole tab is built around.

```rust
struct Track {
    name: String,
    enabled: bool,            // the mute button
    look: Look,               // render mode, blend, orientation, flipbook, lighting
    space: Space,             // Local (rides the emitter) | World (stays where it was born)

    clips: Vec<Clip>,         // ranged emission spans on the timeline
    automation: Vec<Lane>,    // curves over EFFECT time

    shape: EmitShape,
    max_alive: Option<u32>,   // pool capacity; derived from the clips when None

    // Per-particle: a birth value, times a curve over that particle's own life.
    velocity: ValueOrCurve,   // emitter-space; +Y is "along the emit direction"
    size: ValueOrCurve,
    rotation: ValueOrCurve,         // radians (billboards use roll only)
    angular_velocity: ValueOrCurve, // radians/sec, integrated over age
    color: ValueOrCurve,            // RGBA
    gravity: f32,             // 0 = weightless, 1 = full
    drag: f32,
    inherit_velocity: f32,    // how much of the emitter's motion a newborn keeps
    forces: Vec<Force>,
    trail: Option<Trail>,
    // Beam tracks only:
    segments: u32, beam_end: Vec3,
    wave_amplitude: f32, wave_frequency: f32, scroll: f32,
}
```

**Two time domains, one rule: automation shapes birth, life-curves shape ageing.**
An `automation` lane runs over *effect* time and multiplies what a particle is
born as. A `ValueOrCurve` on a property runs over *one particle's* life, `[0..1]`,
and shapes what happens to it as it ages. `size` can carry both, and they
multiply — the effect's crescents get smaller as the slash decays, while each
crescent still pops in and tapers.

`inherit_velocity` is the fix for World-space trails on something fast: smoke off
a moving vessel used to be left behind in space. At 1 a newborn fully keeps up
with the emitter, then drifts as drag bleeds it off. It only means anything for
`Space::World` — a `Local` track rides the node already.

### Emission — clips, not a rate field

There is no track-level rate or lifetime. **A clip is the emission.** Its length
*is* the particle lifetime, and its `emit` says how it spawns:

```rust
struct Clip { start: f32, end: f32, lifetime_jitter: f32, emit: Emit }

enum Emit {
    Rate  { rate: f32 },      // a continuous stream across the clip
    Burst { count: u32, count_jitter: f32,
            pulses: u32, interval: f32, interval_jitter: f32 },
}
```

`pulses = 1` is a single burst. More than one is a pulse train — the first at the
clip's start, each following one `interval` later, with jitter so a chain of
explosions does not tick like a metronome. A track can hold several clips, so
"start late, stop, start again" is dragging two clips rather than keyframing a
rate to zero.

### Shapes, forces, trails

```rust
enum EmitShape {
    Point,
    Cone   { angle: f32, radius: f32 },   // within `angle` of +Y, born on a disc
    Sphere { radius: f32, shell: bool },  // emit direction is radial
    Edge   { length: f32 },               // a line along X — slash arcs
    Ring   { radius: f32 },               // a circle in XZ, radially outward
}

enum Force {                              // added to velocity each step
    Directional { dir: Vec3, strength: f32 },          // wind, updraft
    Point       { center: Vec3, strength: f32 },       // a gravity well (or a push)
    Vortex      { center: Vec3, axis: Vec3, strength: f32 },
    Turbulence  { frequency: f32, strength: f32 },     // value-noise wander
}
```

`forces` is empty by default and costs nothing when it is. A `Trail` is a
per-particle ribbon on billboard tracks, spanning `time` seconds of history at
`width` world units, optionally tapering to nothing (`fade`); a track with no
trail records no history and pays for none.

### Look

`RenderMode` decides what a track draws:

- **`Billboard { texture }`** — a textured quad, oriented by `Look::orient`
  (camera-facing by default, or aligned to velocity for speed lines).
  `aspect` sets width:height so one size curve can drive non-square quads;
  `stretch` lengthens a velocity-aligned quad along its motion.
- **`Mesh { asset_path }`** — instanced geometry through the raster pass. Debris
  that is actually shaped like debris.
- **`Beam { texture }`** — a single camera-facing ribbon from the effect origin to
  `beam_end`, subdivided into `segments` quads. A beam track has no particles at
  all: its width and colour are its `size` and `color` sampled at the *effect's*
  time, and `wave_amplitude` / `wave_frequency` / `scroll` animate it. Lasers,
  tethers, a mining beam.

`blend` is `Alpha`, `Additive`, `Premultiplied`, `Screen` or `Multiply`.
`flipbook` plays a sprite sheet over a particle's life. `lit` puts a particle
through the full scene lighting — sun, point lights, field shadow, AO — and
`cast_shadows` lets the track's live cloud cast into the field shadow march.

### Automation lanes

A `Lane` is a curve over effect time against one `LaneTarget`: `Rate`, `Count`,
`Speed`, `Size`, `Tint`, `ShapeScale` or `Aspect`. Each multiplies the
corresponding birth value, so a cone can widen across the effect (`ShapeScale`),
round sparks can stretch into streaks partway through (`Aspect`), and a swell
into a die-off is one drawn curve on `Rate`.

## A real effect

`OreBreak` — a one-shot burst of shards, thrown outward off a sphere shell:

```ron
(
    name: "OreBreak",
    lifetime: 0.85,
    playback: OneShot,
    end: Destroy,
    tracks: [
        (
            name: "Shards",
            render: Billboard(texture: Some("textures/VFXTEX/Shards/1.png")),
            blend: Alpha,
            orient: Velocity,
            stretch: 1.6,
            space: World,
            clips: [(
                start: 0.0, end: 0.08,
                emit: Some(Burst(count: 16, count_jitter: 0.3, pulses: 1,
                                 interval: 0.0, interval_jitter: 0.0)),
            )],
            automation: [],
            shape: Sphere(radius: 0.18, shell: true),
            velocity: Range(Vec3((-4.5, 2.0, -4.5)), Vec3((4.5, 7.5, 4.5))),
            size: Curve((keys: [ /* 0.34 held, then tapering to 0 */ ])),
        ),
    ],
)
```

`Range` is the third spelling of a property, beside a constant and a curve: each
particle draws its own value between the two ends. It is what keeps sixteen
shards from being one shard drawn sixteen times.

## Runtime

An effect is **compiled** before it runs — every curve baked to a LUT, every
derived value precomputed — into a `CompiledEffect` of `CompiledTrack`s. The sim
is structure-of-arrays per track, and every track has a hard `capacity`, either
authored as `max_alive` or derived from the clips. That ceiling is why a busy
frame cannot ask for unbounded work: `perf.counts()` reports `particles`,
`effects` and `effectsDropped`, and a non-zero `effectsDropped` means a ceiling
refused something this frame — a number rather than a screenshot.

Instances are pooled (ADR-0008). `seed` is offset per instance, so two of the
same effect in one scene do not run in lockstep.

## From a script

An effect is a **Particle System** component on a node, and
`node:particles()` is the handle:

```lua
local fx = node:particles()
fx:restart()                     -- re-fire a one-shot burst on every hit
fx:setIntensity(throttle)        -- live emission scale off a control input
fx:setBeamEnd(target:pos())      -- aim every Beam track, in WORLD space
if fx:isPlaying() then … end     -- and :alive(), :asset(), :play(), :stop()
```

`setBeamEnd` takes a world point and converts it to effect-local itself, so a
beam keeps tracking its target as the emitter moves.

## Editor integration

The **✱ Particles** tab is the timeline: tracks down the left, clips and bursts
you drag along each lane, automation lanes underneath, and the curve editor for
any property. The preview runs continuously and re-compiles as you edit, so
there is no "apply" step between a change and seeing it.

A **Particle System** component on a node references an effect by key and can
play on start; `node:particles()` is the same instance the tab previews.
