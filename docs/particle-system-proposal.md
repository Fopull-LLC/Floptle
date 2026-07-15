# Floptle Particle System — Design Proposal

**Status:** proposal / decision document
**Author:** synthesis of `FloptileParticleSystemPlanning.docx` (Ty, 2026-07), the 2026-06 pre-spec
(`docs/subsystems/particles-vfx.md`), and three deep research passes over the live workspace
**Scope:** the timeline-driven particle/VFX system — asset model, runtime sim, render pass,
editor, Lua API — and how it leans into what only Floptle can do
**Grounded against:** the live workspace as of 2026-07-03 — the `floptle-vfx` stub, the raster
instancing path, the shared SDF field (`field.wgsl`), the retro-res post chain, the anim dope
sheet, and the component/asset/scripting checklists cited inline

---

## 1. Executive summary

Floptle's particle system is **a video-editor timeline, not a settings panel**. A
`ParticleSystem` component on a node references a **ParticleEffect** asset. The effect is a
container of **tracks** — each track is one visual layer ("Crescents", "Smoke") with its own
look. On the timeline, a track carries **clips** (ranged emission spans you drag, trim, and
split — start late, stop, start again), **bursts** (hand-placed instant emits), and
**automation lanes** (curves drawn over the track, DAW-style, modulating properties across the
effect's life). You arrange the effect the way you'd cut a video, watching it live, instead of
excavating twenty collapsible modules.

The big decisions, made and defended below:

1. **Track = group. One concept, not two.** The old pre-spec had a `groups: Vec<ParticleGroup>`
   list *plus* a timeline whose tracks referenced groups by index. The planning doc is clear
   that "the particles are just the individual tracks" — so the track *owns* its look, its
   per-particle curves, and its lane on the timeline. One selectable, draggable, copyable thing.

2. **Two time domains, one rule.** Curves exist over **effect time** (automation lanes on the
   timeline — what a particle is *born* as, and when) and over **particle life** (`[0..1]`
   normalized — what happens to it as it *ages*). The rule that keeps this learnable:
   **automation shapes birth; life-curves shape aging.** `size` can have both — automation sets
   the birth size across the effect ("crescents get smaller as the slash decays"), the life
   curve shapes each particle's pop-in/taper. They multiply.

3. **One data model, two sim backends: CPU reference first, GPU compute committed.** The
   timeline, curves, and birth semantics are backend-agnostic. A structure-of-arrays CPU sim
   ships first — it is the correctness reference, it proves the editor vision fast, and it
   handles tens of thousands of particles in well under a millisecond. A **GPU compute
   backend is a committed roadmap phase (§8 phase 5), not an escape hatch**: the engine has
   zero compute infrastructure today, so the CPU phase buys time to build it right while five
   architectural commitments (§4.4) guarantee the swap is a backend change, not a rewrite.
   The split that keeps the timeline honest at any scale: **births on CPU** (timeline logic,
   automation sampling, seeds — exact and tiny), **aging on GPU** (the embarrassingly
   parallel part). Scrubbing stays exact — and a GPU re-sim from `t=0` is trivial at counts
   where the CPU would sweat.

4. **Billboards get a new instanced pass; mesh particles ride the existing raster path.** The
   planning doc wants both "2d tex billboard" and "3d mesh" particles. Billboard quads are a
   small new pipeline modeled line-for-line on `raster.rs`'s per-instance vertex buffer
   pattern. Mesh particles don't need a renderer at all — they append `InstanceRaw` entries
   into the raster pass's existing buckets and inherit full lit shading, SDF sun shadows, AO,
   and the transparent pipeline for free.

5. **Particles draw before post and the retro upscale — so they are automatically pixelated,
   AO'd, and bloomed with the world.** The pass slots between the grid and the post chain in
   `render_frame.rs`, into the same color/depth targets. In retro mode that means particles
   render at the retro internal res and inherit per-retro-pixel post like everything else. No
   other engine's particles match a retro aesthetic without manual work; ours can't *not* match.

6. **Field-aware particles are the signature feature.** Every fragment in the engine can
   already sample the fused SDF field — `sun_shadow`, `sdf_ao`, `map_d` in the shared
   `field.wgsl`, bound at raster `group(2)` via `Raymarch::field_bind()`. Billboard particles
   opt into the same bind: smoke genuinely darkens under the terrain's sun shadow, dust
   receives SDF AO in crevices. The GPU backend phase adds field *collision* — the distance
   atlas already lives on the GPU, so particles bouncing off sculpted terrain is a texture
   fetch in the sim kernel — and the surreal stretch is **matter particles** that inject into
   the raymarched field itself and smin-fuse with blobs (§9).

Crate layout: everything simulation + data lives in the existing **`floptle-vfx`** stub (its
planned modules — `effect` / `timeline` / `group` / `curve` / `sim` — already match this
design; rename `group` → `track`). The billboard pass lives in `floptle-render` beside
`raster.rs`. RON DTOs live in `floptle-scene` mirroring `anim.rs`. `floptle-editor` and
`floptle-runtime` already depend on `floptle-vfx`, so no workspace rewiring is needed.

---

## 2. The vision, sharpened

The planning doc's bet: **arranging beats configuring.** You don't set "start delay: 0.35" in
a field — you *drag the clip* until it looks right while the effect loops in the viewport.
The mental model is a DAW / video editor, and the mapping is exact:

| DAW / video editor | Floptle Particle System |
|---|---|
| Project | `ParticleEffect` asset (lifetime, loop/one-shot) |
| Track | `Track` — one visual layer with its own look |
| Clip on a track | Emission span — the track emits while the playhead is inside |
| One-shot sample / hit marker | `Burst` — instant emit of N particles at a time |
| Automation lane under a track | Curve modulating a birth property over effect time |
| Plugin on the track | The track's look: texture/mesh, blend, render mode |
| Bounce/render | It just plays in the scene — the timeline *is* the runtime data |

What this buys artistically:

- **Timing is spatial.** "Smoke puffs a beat after the slash" is a clip sitting a little to the
  right — visible, draggable, obvious. "Start, stop, then start again" is two clips on one
  track. No engine-standard `delay`/`duration` number pairs.
- **Multiple automations, multiple tracks, one view.** Expand a track and its automation lanes
  stack under it like Ableton. Rate swells, tint shifts, and size decay are curves you *see
  next to each other*, aligned in time against every other track.
- **The playhead is the debugger.** Scrub anywhere; the deterministic sim shows the exact state
  of every particle at that instant. Drag a tangent, watch the slash sharpen live.

The pre-spec's best ideas survive intact: value-or-curve as the single property affordance
(hover-corner graph icon → sparkline, §6.4), the create-new-effect wizard, pooling by default
(ADR-0008), progressive disclosure ("every knob exists, but you only see the ones you reach
for"), and the explicit out-of-scope list (no node-graph per-particle VEX, no fluid solvers).

---

## 3. Data model

Authored as RON, one effect per file, extension **`.vfx.ron`** (suffix-classified like
`.anim.ron` / `.actl.ron`; the asset key is the project-relative path minus extension, per
`asset_key()` in `floptle-editor/src/anim.rs`). Runtime types in `floptle-vfx`; serde DTOs in
`floptle-scene/src/vfx.rs` with defaults on every field (the back-compat discipline of
`NodeDoc`).

```rust
// floptle-vfx — runtime model (DTO mirrors omitted)

pub struct ParticleEffect {
    pub name: String,            // display name; the asset key is the file path
    pub lifetime: f32,           // seconds — the timeline's length / one loop period
    pub playback: Playback,      // Looping | OneShot
    pub end: EndBehavior,        // OneShot only: Destroy | Persist (already in the stub)
    pub tracks: Vec<Track>,
    pub seed: u32,               // effect-level base seed (re-rollable in the editor)
}

pub enum Playback { Looping, OneShot }

/// One visual layer AND its timeline lane. The unit you select, drag, mute, copy.
pub struct Track {
    pub name: String,            // "Crescents"
    pub enabled: bool,           // mute button on the track header
    pub look: Look,
    pub space: Space,            // Local (follows the node) | World (trails stay behind)

    // ---- timeline content (effect-time domain) ----
    pub clips: Vec<Clip>,        // ranged emission spans; disjoint, sorted
    pub bursts: Vec<Burst>,      // hand-placed instant emits (diamonds)
    pub automation: Vec<Lane>,   // DAW-style lanes over effect time

    // ---- emission ----
    pub rate: f32,               // particles/sec while inside a clip (lane-modulatable)
    pub shape: EmitShape,        // Point | Cone | Sphere | Edge | Ring
    pub particle_lifetime: f32,  // seconds each particle lives
    pub lifetime_jitter: f32,    // 0..1 fraction of deterministic per-particle variance
    pub max_alive: Option<u32>,  // pool capacity override (derived when None)

    // ---- per-particle properties (birth value × life curve) ----
    pub velocity: ValueOrCurve,  // Vec3 — initial dir·speed; curve = over particle life
    pub size:     ValueOrCurve,  // f32
    pub rotation: ValueOrCurve,  // radians (billboard spin / mesh axis spin)
    pub color:    ValueOrCurve,  // Rgba — color-shift AND fade in one curve
    pub gravity:  f32,           // 0 = weightless, 1 = full scene gravity
    pub drag:     f32,
    // (spread/jitter per property, flipbook frames, etc. surface progressively)
}

pub struct Clip  { pub start: f32, pub end: f32 }       // the draggable span
pub struct Burst { pub t: f32, pub count: u32 }         // the draggable diamond

/// An automation lane: one curve over EFFECT time, targeting a birth-domain parameter.
pub struct Lane { pub target: LaneTarget, pub curve: Curve }

pub enum LaneTarget {
    Rate,          // multiplies Track::rate      — swells, ramps
    Count,         // multiplies burst counts
    Speed,         // multiplies birth |velocity|
    Size,          // multiplies birth size
    Tint,          // multiplies birth color (Rgba curve → gradient lane)
    ShapeScale,    // scales the emit shape (cone radius growing over the effect)
}

pub struct Look {
    pub render: RenderMode,
    pub blend: Blend,          // Alpha | Additive
    pub lit: bool,             // DEFAULT false. On: full scene lighting per particle —
                               // sun + point lights + field sun-shadow + SDF AO (§5.1)
    pub cast_shadows: bool,    // DEFAULT false. On: the track's live cloud casts into the
                               // field shadow march via an aggregate proxy (§5.3)
}

pub enum RenderMode {
    Billboard { texture: Option<String>, orient: Orient },   // project-relative path
    Mesh      { asset_path: String },                        // instanced through raster
}

pub enum Orient { FaceCamera, VelocityStretch { stretch: f32 }, AxisY }

pub enum EmitShape {
    Point,
    Cone { angle: f32, radius: f32 },
    Sphere { radius: f32, shell: bool },
    Edge { length: f32 },                // slash arcs
    Ring { radius: f32 },
}

pub enum Space { Local, World }
```

### 3.1 `ValueOrCurve` and `Curve` (unchanged from the pre-spec — it was right)

```rust
pub enum ValueOrCurve { Const(Value), Curve(Curve) }     // Const is the default
pub enum Value { F32(f32), Vec3(Vec3), Rgba([f32; 4]) }

pub struct Curve { pub keys: Vec<Key>, pub extrapolate: Extrapolate }
pub struct Key {
    pub t: f32,              // life curves: 0..1 normalized; lanes: seconds on the timeline
    pub v: Value,
    pub interp: Interp,      // Constant | Linear | Bezier (toward the NEXT key)
    pub in_tan: f32,
    pub out_tan: f32,
}
pub enum Interp { Constant, Linear, Bezier }
pub enum Extrapolate { Clamp, Repeat }
```

This is a **net-new curve module** — nothing reusable exists (`floptle-core::math` is 28 lines:
`lerp`/`smoothing`/`remap`; `floptle-anim::Interp` is Step/Linear only, no tangents). It lives
in `floptle-vfx::curve` first; if the animation editor later grows a graph editor, promote to
`floptle-core::curve`.

### 3.2 Emission semantics (the correctness core)

- **Clips gate, rate flows.** While the playhead is inside a clip, the track accumulates
  `rate × lane(Rate)(t) × dt` and emits on integer crossings. The accumulator resets at each
  clip start, so emission phase is deterministic regardless of frame rate.
- **Bursts fire on crossing**, half-open `(prev_t, t]` — never missed at low FPS, never
  double-fired, exactly the anim `ClipEvent` discipline.
- **Loop wrap** (`Playback::Looping`): playhead wraps `lifetime → 0` and re-arms clips/bursts.
  Live particles are *not* killed — they age out on their own `particle_lifetime`.
- **Birth snapshot**: at emit, a particle samples every automation lane at the *current effect
  time* and every `ValueOrCurve` at *its own* `t=0`, rolls its jitter from its seed, and stores
  the results. From then on only life curves apply. A particle is fully determined by its birth
  record — the property that makes scrubbing exact (§4.2).

---

## 4. Runtime & simulation

### 4.1 SoA layout, pooled per (instance, track)

```rust
// floptle-vfx::sim — one per live track per effect instance, capacity-allocated once
pub struct TrackParticles {
    pos:   Vec<Vec3>,     vel:  Vec<Vec3>,
    age:   Vec<f32>,      life: Vec<f32>,
    seed:  Vec<u32>,
    birth_size: Vec<f32>, birth_color: Vec<[f32; 4]>, birth_rot: Vec<f32>,
    count: usize,         // live; arrays never shrink, retire = swap_remove
}
```

Capacity is derived (`rate × max particle_lifetime + Σ burst counts`, clamped by `max_alive`)
and allocated when the instance spawns — **zero allocation during play**, the ADR-0008
contract. `floptle-core::pool` doesn't exist yet; the effect-instance pool starts as a private
free-list inside `floptle-vfx` and is promoted to core when a second consumer appears.

Per-frame, per instance: advance playhead → fire clips/bursts (births) → integrate
(`age += dt; vel += gravity·g·dt − drag·vel·dt; pos += vel·dt`; sample life curves at
`age/life` for size/rotation/color/velocity-over-life) → retire `age ≥ life` by swap-remove →
write instance data into the persistent per-batch GPU buffer. Curves are **baked to 64-sample
LUTs** at asset load/edit (curve eval = one lerp between table entries — branch-light, and the
LUT bake is where bezier cost goes to die).

### 4.2 Determinism (what makes the timeline honest)

- Per-particle RNG: `hash(effect.seed, instance_seed, track_index, emit_counter)` — a counter,
  not a global RNG. Same effect + same seed = same particles, forever.
- **Editor scrub = re-sim from `t=0`** at a fixed internal step (1/120 s, matching physics'
  step discipline). Forward scrubs advance incrementally; backward scrubs re-sim — at ≤ a few
  thousand particles and a few hundred steps this is well under a frame.
- **In-game playback** advances with variable `dt` forward-only (no scrubbing in game, no
  re-sim cost). The two modes share one `advance(dt)`.
- Determinism also gives the **visual test harness** golden images: a `vfx_probe` example
  renders the same effect at fixed times to PNGs, and `cmp` catches regressions bit-exactly —
  the same verify-by-render workflow used for shadows/SSAO/retro.

### 4.3 Play-loop insertion & spaces

The editor tick order is **scripts → animation → physics** (`play_step`,
`render_frame.rs:2025`). Particles tick **after physics**: emitter node transforms are final
for the frame, and `Space::World` tracks want post-physics positions.

- `Space::Local`: particles store node-local offsets; the node's world transform applies at
  render. Attached fire follows the torch.
- `Space::World`: particles anchor to the **effect instance's spawn point as an f64 anchor** and
  store f32 offsets from it — trails stay behind, and the floating origin (ADR-0015) can't
  smear them: rendering is `(anchor − camera.world_position) + offset`, exactly the
  anchored-collider pattern physics already proved at 1e7.

Edit-mode preview uses the same instances driven by the particle tab's playhead instead of
play time (the `preview_pose` precedent).

### 4.4 GPU-ready by construction (the five commitments)

The GPU compute backend (§8 phase 5) must be a backend swap, not a rewrite. These decisions
are made **now**, in the CPU implementation, so nothing blocks it later:

1. **Curves are LUTs.** A 64-sample table is one row of a texture (or a storage-buffer slice).
   The CPU samples it with a lerp; a compute shader samples the identical data. No bezier
   evaluation ever happens in the hot loop on either backend.
2. **Births on CPU, always.** Emission is timeline logic — clips, bursts, automation lanes,
   counter-hashed seeds — and its output per frame is small (a handful of birth records) even
   when *alive* counts are huge. Keeping it CPU-side means both backends share one exact,
   testable emission implementation, and the timeline semantics (§3.2) never fork.
3. **The SoA layout is the storage-buffer layout.** `TrackParticles` arrays are
   std430-compatible from day one (vec4-aligned fields); "upload" is a memcpy, and the GPU
   backend aliases the same buffers instead of copying.
4. **The sim's output IS the instance buffer.** The CPU sim ends each frame writing the
   per-batch instance data; the GPU sim ends each frame with a compute pass writing the same
   buffer on-device. The render pipelines (§5) are identical for both backends and never know
   which sim ran.
5. **RNG is a pure integer hash of (seed, counter)** — bit-exact in Rust and WGSL, no state
   to migrate. Cross-backend float integration may drift a hair over hundreds of steps;
   births (the artistic timing) never do.

The one honest divergence risk — an effect looking different in-editor (CPU) vs in-game
(GPU) — is retired by the endgame: once the GPU backend exists, the *editor preview runs it
too*, and the CPU sim remains as the reference for tests and headless runs.

---

## 5. Rendering

### 5.1 The billboard pass (`floptle-render/src/particles.rs`, new)

Modeled on `raster.rs`'s instancing: one shared unit quad, per-instance vertex buffer
(`VertexStepMode::Instance`) carrying `pos (camera-relative f32×3), size×2, rotation, color,
flags`, grown by `next_power_of_two` and rewritten once per frame. Bind groups follow the
raster convention:

- `group(0)` — globals uniform: `view_proj` + camera right/up basis (billboards face the
  camera by construction; view matrix has no translation, camera is the origin).
- `group(1)` — the track's texture + sampler (resolved through the existing
  `texture_registry: HashMap<String, TexId>`).
- `group(2)` — **optional** `Raymarch::field_bind()`, with `field.wgsl` concatenated onto the
  particle shader exactly as `raster.rs:215` does. `lit` tracks evaluate the raster lighting
  model at the particle center — directional sun + the 16 point lights from `Globals`, with a
  soft spherical normal — and multiply `sun_shadow(view_pos, n, pix)` and
  `sdf_ao(view_pos, n)` into the tint: smoke darkens under terrain shadow, dust shades in
  crevices, sparks pick up the nearest point light. Unlit tracks (the default) skip all of it
  — classic crisp VFX, zero lighting cost.

Pipeline state mirrors the raster transparent pipeline: `depth_compare: Less`,
`depth_write: false`, drawn after all opaque work. One instanced draw per
`(texture, blend, lit, cast)` batch. **Additive batches draw unsorted** (order-independent);
**Alpha batches CPU-sort back-to-front** by camera depth within the batch (thousands of
particles sort in microseconds; the raster pass's "no sort" gap does not repeat here).

### 5.2 Mesh particles = raster instances

`RenderMode::Mesh` tracks build `InstanceRaw` entries (model matrix from particle
pos/rot/size, color+alpha from the tint) and submit them through the raster pass's existing
bucketing — same pipelines, same `group(2)` field bind, so mesh particles are lit, sun-shadowed,
SDF-AO'd, and alpha-split exactly like scene meshes. No new shader. (They do inherit raster's
unsorted-transparency approximation; acceptable for v1, revisit with the sort infrastructure
above if it shows.)

### 5.3 Shadow casting (per-track opt-in, off by default)

The shadow system is field-first: meshes cast by registering **proxy occluders** into the
shadow march (`prox_a/b/rot`, `MAX_SHADOW_PROXIES = 32`, `field.wgsl:299`). Thousands of
particles can't each take a slot — so a `cast_shadows: true` track registers **one aggregate
ellipsoid proxy fitted to its live cloud** (center + extents from the SoA bounds, updated per
frame). That reads exactly right for the cases that matter — a smoke column darkening the
ground, a debris cloud dimming a doorway — at the cost of one proxy slot. The 32-slot budget
is shared with mesh-collider proxies; bump `MAX_SHADOW_PROXIES` if real scenes crowd it.
Mesh-particle tracks use the same aggregate (per-particle proxies would blow the budget), and
**matter particles (§9) cast for free** — they're *in* the field.

### 5.4 Pass placement — retro/post coherence for free

Insert between the grid pass and post in `render()` (`render_frame.rs`, after `:1664`), drawing
into the **same color/depth views** the scene used. Consequences, all desirable:

- Depth-tested against meshes *and* raymarched matter (shared `Depth32Float`).
- Captured by SSAO/bloom/vignette; in retro mode the whole pass runs at the retro internal res
  **before the upscale**, so particles pixelate with the world and get per-retro-pixel post —
  the 20c8291 invariant extends to particles with zero extra work.
- Replicate the call in `render_world_into` (`render_frame.rs:2750`) so the split-view Game
  viewport shows them (respecting its shared-buffer re-upload ordering).

**HDR caveat:** the scene composites in 8-bit sRGB (no `Rgba16Float` target exists despite
`renderer.md`'s claim), so additive stacking clips at 1.0. Bloom's bright-pass still blooms
near-white particles. An HDR scene target is an optimization-phase item — noted, not blocking.

---

## 6. Editor

### 6.1 Extract `timeline.rs` first (shared with the anim dope sheet)

The anim editor's `timeline_ui` (`anim_ui.rs:1405`) already solved ruler drawing, playhead
scrubbing, fps-snap, zoom, and draggable timeline objects — but as a private method over the
anim DTOs. Extract the genuinely generic pieces into `floptle-editor/src/timeline.rs`:

- `TimelineView { left_px, px_per_s, duration }` with `x_to_time`/`time_to_x`,
- `draw_ruler` (already a free function at `anim_ui.rs:1358`), the scrub-strip interaction,
  and the snap formula (currently copy-pasted three times inside `anim_ui.rs`).

Refactor the dope sheet onto these (pure code motion, separate commit per house rules), then
build the particle editor on the same helpers. Two timeline editors, one ruler.

### 6.2 The Particles tab

A new `EditorTab::Particles` following the ~30-line dock pattern (`dock.rs:7` enum + title +
`default_dock` placement beside Animation; dispatch in `main.rs:432`; focus block copying
`render_frame.rs:2426-2433`; opened from the Inspector component's "✎ Edit effect" button via
a new `cmd.open_particle_editor = Some(key)`).

Layout (matching the planning doc's sketch, transport conventions copied from the anim tab):

```
 ┌─ transport: ⏮ ⏵ ⏸ ⏹ ⟲loop │ 0.21 / 0.60 s │ seed ⟳ │ zoom ──●── │ snap ▾ ─────────┐
 ├───────────────┬───────────────────────────────────────────────│───────────────────┤
 │ ▣ Crescents ▾ │      ▐████████████████████▌        ▐██████▌   │    ← clips (drag   │
 │   ⊳ bursts    │   ◆        ◆                                  │       body/edges)  │
 │   ∿ Rate      │   ╱▔▔▔▔╲__________                            │    ← automation    │
 │   ∿ Size      │   ▁▁▁▂▃▅▆▇█▇▆▅▃▂▁▁                            │       lanes        │
 ├───────────────┼───────────────────────────────────────────────│───────────────────┤
 │ ▣ Smoke       │              ▐██████████████▌  ◆              │                    │
 └───────────────┴───────────────────────────────────────────────┴───────────────────┘
   track headers      timeline canvas                          playhead │  side panel →
```

- **Track rows**: header (enable/mute `▣`, name, look icon) + lane. `+ Track` adds one; drag
  to reorder; right-click → duplicate/delete. Selection highlights the row.
- **Clips**: drag body = move, drag edges = trim, `alt`-drag = split, right-click = delete;
  double-click empty lane = new clip. Snap honors the fps grid.
- **Bursts**: diamonds, dragged like the anim event flags (`anim_ui.rs:1484` is the template).
- **Automation lanes**: `▾` on the header expands lanes under the track (`∿ Rate`, `∿ Size`…).
  `+ lane` picks a `LaneTarget`. Each lane is a mini curve strip drawn in-lane; clicking opens
  the full curve editor on it. Multiple lanes across multiple tracks stay visible at once —
  the planning doc's explicit requirement.
- **Side panel** (right, the `graph_side_panel` precedent): the selected track's properties —
  look, blend, lit, space, shape, rate, lifetime, and every `ValueOrCurve` field. Selecting a
  clip/burst shows its start/end/count for numeric entry.
- **Playhead** scrubs the deterministic preview live in the Scene viewport on the selected
  node (and auto-plays on loop while the tab is focused — "seeing where it looks best in
  realtime").

### 6.3 The curve editor

Net-new widget (nothing exists workspace-wide): keys as draggable nodes, click-empty = add,
right-click = delete, per-key `Constant|Linear|Bezier` with draggable tangent handles,
auto-fit value axis with manual lock, domain label "particle life 0–1" or "effect time (s)"
depending on what opened it. Color curves render a **gradient strip** above an alpha lane —
color-shift and fade in one editor. One widget serves life curves *and* automation lanes.

### 6.4 The value-or-curve affordance (verbatim from the pre-spec)

Every property renders as its plain editor (drag-float, Vec3 triple, color swatch). Hovering
reveals a corner **graph icon**; clicking promotes `Const → Curve` (seeded flat at the current
value) and swaps the field to a **sparkline**; right-click → "Make constant" demotes, taking
the value at `t=0`. One helper (`ui.value_or_curve(label, &mut field)`) owns the whole
affordance so the inspector code stays uniform.

### 6.5 New-effect flow

Add Component → Particle System → pick an existing `.vfx.ron` or **New effect…** → the
four-field wizard (name, lifetime, Looping/OneShot, end behavior — hidden when Looping) →
writes a minimal asset, attaches the component, opens the tab with the empty lifetime-long
timeline. Also creatable from the Assets browser (the `AnimNew` pattern).

---

## 7. Engine integration (the checklists, condensed)

**Component** — `ParticleSystem { asset: String, play_on_start: bool }` in
`floptle-core/src/matter.rs` beside `AnimController` (L101, the exact pattern). `NodeDoc`
gains `particles: Option<ParticleSystemDoc>` with serde defaults (`floptle-scene/src/lib.rs`,
mirror `anim_controller` at L68 / `spawn_into` L860 / `to_doc` L931). Inspector section via
`component_header` + `ComponentClip::ParticleSystem` for copy/paste (`inspector.rs:22,47`);
`Add` enum entries + `EditorCmd` fields (`add_particles`, `open_particle_editor`) applied in
`apply_frame_commands` with `record()` for undo.

**Asset** — DTOs + `VFX_EXT = ".vfx.ron"` + load/save in `floptle-scene/src/vfx.rs` (mirror
`anim.rs`); `is_vfx` classifier + icon in `assets.rs`; a `VfxSystem` registry on `Editor`
(mirror `AnimSystem`: `rescan` on project open at `main.rs:886`, key = path minus ext, doc →
runtime compile with LUT bake). Texture pickers reuse `collect_texture_paths`; mesh pickers
reuse `collect_model_paths`.

**Play mode** — instances (re)built in `toggle_play` (like `sim`), ticked in `play_step` after
physics; live-tweak follows the physics dual pattern: per-frame `sync` for scalar edits
(rate, color, curves — cheap, since curves re-bake on edit), full instance rebuild on
structural change (track add/remove, asset swap). Stop restores via the existing snapshot.

**Lua** — the animator-bridge pattern (`api.rs:535-763`), not the field-mirror: a
`node:vfx()` handle whose metatable queues `VfxCmd`s (`play()`, `stop()`, `burst(track?,
count?)`, `seek(t)`, `setParam(name, v)`) drained in the play loop before the particle tick,
plus a read mirror (`isPlaying`, `time`, `alive`). Later phase: the pre-spec's pooled global
(`vfx.play("360Slash", node.transform)`, `vfx.play_at("Hit", pos)`) for effects not tied to a
node — instances already support that (editor previews aren't node-bound either). EmmyLua
`---@class VfxHandle` in `lua_support.rs`, docs page `docs/particles.md`, IDE hovers in
`ide.rs` — the full scripting-docs bar established by the RigidBody work.

---

## 8. What ships when — phased roadmap

Vertical slices, each independently verifiable (build clean, clippy zero, tests, probe PNGs):

1. **Core runtime + billboard render.** `floptle-vfx` data model + curve module + LUT bake +
   deterministic SoA sim; RON DTOs; billboard pass (Alpha + Additive, FaceCamera); component +
   play-on-start; pass wired into both viewports. Verified by `vfx_probe` golden PNGs of a
   hand-authored `.vfx.ron` at fixed times, plus unit tests (emission counts across clip
   boundaries/loop wrap, scrub-equals-forward-sim determinism, LUT-vs-analytic curve error).
2. **The timeline editor.** `timeline.rs` extraction (+ dope-sheet refactor as its own
   commit), Particles tab: tracks, clips, bursts, side panel, scrub-driven live preview,
   wizard, asset browser integration. This is the milestone where the *vision* is testable —
   arrange a slash effect end-to-end without touching RON.
3. **Curves everywhere.** Curve editor widget, value-or-curve affordance + sparklines,
   automation lanes with in-lane strips, gradient color editing, seed re-roll.
4. **The Floptle leans.** Mesh particles through raster; per-track `lit` + `cast_shadows`
   opt-ins (aggregate proxy); `Orient::VelocityStretch`; `Space::World` trails with f64
   anchors; `node:vfx()` Lua handle + EmmyLua/docs/IDE bar.
5. **The GPU compute backend** — the engine's first compute pass. Storage-buffer aging/
   integration (births stay CPU, §4.4), dead-flag compaction, GPU alpha sort (bitonic) or
   coarse-bucket fallback, backend auto-selected per track by `max_alive` threshold (small
   emitters skip dispatch overhead). This phase also lands **field collision the right way**:
   the fused SDF field already lives on the GPU as the 3D distance atlas, so per-particle
   `map_d` sampling (die/bounce/slide on `< 0`) is a texture fetch in the sim kernel —
   cheaper and more exact than any CPU port of it. Editor preview switches to the GPU
   backend once it exists (§4.4).
6. **Polish + depth** (post-playtest, priority by feel): pooled `vfx.play` global + prewarm,
   flipbook atlases, timeline marker events firing script callbacks (the `ClipEvent`
   pattern), soft particles (needs a depth pre-copy), **matter particles** (§9), HDR target.

---

## 9. Uniquely Floptle (why this design leans into *this* engine)

- **Retro-coherent by construction** (§5.3) — chunky particles with per-retro-pixel AO/bloom.
- **Field-shadowed billboards** (§5.1) — the shared-field trick meshes already use, extended
  to smoke and dust for nearly free.
- **Field collision** — the fused SDF field is already resident on the GPU as a 3D distance
  atlas, so once the compute backend lands (§8 phase 5), particles bouncing off *sculpted
  terrain* is a texture fetch per particle in the sim kernel — no baked collision proxies,
  no CPU broadphase, nothing authored.
- **Matter particles (stretch, capped, gloriously weird)** — a small array of live particle
  spheres (≤ 32, the `MAX_SHADOW_PROXIES` discipline) injected into `map_d` and smin-fused
  with the field: droplets that *merge into* terrain, goo that drips off blobs and becomes
  blob. No mainstream engine can express this; Floptle's raymarcher already almost can.

## 10. The optimization contract

This engine is for a wide variety of games and for other developers — the ceiling is set by
the GPU backend, not the CPU reference.

- **CPU backend (phases 1–4):** 10k live particles typical, 50k stress, sim < 0.5 ms, zero
  steady-state allocation. The means: SoA + swap-remove; capacity preallocation + pooling;
  curve LUTs; counter-hashed RNG (no state); one persistent instance buffer per batch, one
  `write_buffer` per frame; batch keys kept coarse `(texture, blend, lit, cast)`.
- **GPU backend (phase 5):** 250k+ live particles at negligible frame cost — integration,
  life-curve sampling (LUT fetches), field collision, and instance-data writes all stay
  on-device; the CPU touches only births and playheads. No readback in the frame loop.
- **Why CPU-first is sequencing, not a performance position:** the GPU backend is the
  engine's first compute pass (pipelines, storage buffers, compaction — real infrastructure),
  and building it against a proven, deterministic reference sim with an editor already
  exercising every semantic is how it lands correct. The §4.4 commitments exist precisely so
  none of the CPU work is throwaway.

The named-stage render decomposition gives per-pass timing hooks; the `vfx_probe` goldens
hold both backends to the same pictures.

## 11. Relation to the 2026-06 pre-spec

`docs/subsystems/particles-vfx.md` remains the record of the original design; this proposal
supersedes its data model (groups+tracks → unified tracks; emit-events → clips + bursts +
automation lanes), adds mesh particles, render modes, spaces, field integration, and grounds
everything against the code as it exists (the pre-spec's shader-IR `.flsl` materials and "VFX
workspace" predate the current Material component and egui_dock reality). Rewrite the
subsystem doc to match once Phase 2 lands.

## 12. Decisions log + remaining questions

**Resolved 2026-07-03 (Ty):**

- **Billboard lighting & shadow casting: per-track opt-ins, both OFF by default.** `lit`
  turns on full scene lighting (sun + points + field shadow + AO); `cast_shadows` registers
  the track's aggregate proxy. The dev flips them on per track when the effect wants it.
- **Performance ceiling: as high as the hardware allows.** The engine targets many games and
  outside developers — so the GPU compute backend is a **committed phase (§8 phase 5)**, the
  architecture is GPU-ready from day one (§4.4), and the CPU sim's role is reference +
  editor-proving, not the ceiling.

**Still open:**

1. **`vfx.play` pooled global vs component-only** — fine to ship component-attached first,
   with the global following in phase 6?
2. **Within phase 5, collision vs raw scale** — when the compute backend lands, is
   field collision or maximum particle count the first thing your games will reach for?
