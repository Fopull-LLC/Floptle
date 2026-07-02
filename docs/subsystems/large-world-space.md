# Floptle — Large-world space (`floptle-core`)

Default-on, behind-the-scenes coordinate space: the world moves around the player,
the player never actually moves — so a galaxy-scale or infinitely-deep world renders
and simulates with **zero jitter and zero developer work**.

> Decision & rationale: [`../decisions/0015-large-world-space.md`](../decisions/0015-large-world-space.md).
> Reads-with: the authoring model [`./scene-and-nodes.md`](./scene-and-nodes.md)
> (`Transform` lives in `floptle-core`), the GPU upload path
> [`./renderer.md`](./renderer.md), the SDF sim [`./physics.md`](./physics.md), and
> the gravity-aligned player frame [`./gravity-and-density.md`](./gravity-and-density.md).
> Future peers-share-a-space: [`./networking-future.md`](./networking-future.md).

The headline: **the developer writes ordinary world-space code.** You place a node at
`(0, 0, 0)` or at a coordinate twelve light-years out; you read and set
`self.transform.pos` exactly the same way. Everything below — camera-relative upload,
floating origin, `f64` transforms, reference frames — is engine plumbing the game
never has to think about. Most engines make you architect around this; we own the
renderer *and* the physics, so we bake it in instead.

## 1. The precision problem, concretely

A 32-bit float is `1 sign · 8 exponent · 23 fraction` bits → 24 bits of effective
mantissa, ≈ **7.2 significant decimal digits**. The catch is that the *step* between
representable values scales with magnitude: the gap is `2^(exp-23)`. Concretely the
absolute resolution near a value `x` is roughly `x · 2^-23 ≈ x · 1.2e-7`:

```
 |x| (meters)     ULP / smallest step        what it means on screen
 ────────────     ───────────────────        ──────────────────────
 1                ~6e-8 m   (60 nm)           perfect
 1 000            ~6e-5 m   (0.06 mm)         fine
 65 536           ~0.004 m  (4 mm)            sub-pixel wobble begins
 1 000 000        ~0.06 m   (6 cm)            visible jitter, cracks
 16 000 000       ~1 m                        snapping; geometry tears
```

So somewhere around **10^5–10^6 units** the representable gap crosses a meter and
positions *snap* to a lattice. Vertices that should be still vibrate frame to frame
(the camera transform rounds differently each frame), normals flicker, z-fighting
appears, and a smooth fly-through turns to gravel. **GPUs force `f32`** — wgpu/WGSL
math, vertex attributes, and depth are 32-bit; emulated double on the GPU is slow and
unsupported on most of our backends (ADR-0002). Physics degrades the same way:
penetration depth and `∇f` finite differences (see [`./physics.md`](./physics.md))
are *differences of large nearly-equal numbers*, where `f32` cancellation eats every
useful bit.

**Why `f64` helps but isn't the whole answer.** A double has 52 fraction bits → 53
effective → **≈15.95 significant decimal digits**. That buys sub-millimeter precision
out to ~tens of AU (`1e12`–`1e13` m): a solar system is comfortable. But a galaxy is
~10^21 m across, and `1e21 · 2^-52 ≈ 2.2e5` — a **220 km** step. Flat `f64` *cannot*
hold a galaxy at meter precision. The fix is not "more bits everywhere," it's
**hierarchy**: keep precision *where the player is*, and represent the rest as
high-precision offsets between nested frames. (`f128`/fixed-point are off the table —
no hardware, and they don't solve the GPU side anyway.)

## 2. The layered solution (all default-on, all transparent)

Four layers, each independently always-on. They compose: 1+2+3 land near-term and
already give you a flawless solar system; layer 4 extends the same idea to a galaxy.

### Layer 1 — Camera-relative rendering (always on)

We never upload absolute world positions to the GPU. We upload positions **relative
to the camera**, with the subtraction done in `f64` and the result cast to `f32`
*last*:

```
p_view = (p_world_f64 - cam_world_f64)   // f64 subtract — full precision
view_f32 = (proj_f32) * (R_cam_f32) * vec3_to_f32(p_view)
```

The GPU only ever sees small numbers (the visible scene is a few km across no matter
where it sits in the world), so vertex math, depth, and interpolation are all in
`f32`'s sweet spot. This is the **renderer's default upload path**
([`./renderer.md`](./renderer.md)): the per-draw model matrix is a *model-view*
matrix built host-side in `f64` and downcast, never a model matrix times a giant view
matrix on the GPU. It kills GPU jitter at *any* world scale, and it is ~free — one
`f64` vector subtract per object (or per instance batch) that we already do when
culling. For raymarched fields the ray origin `ro` is likewise expressed
camera-relative before entering WGSL.

### Layer 2 — Floating origin (always on)

Camera-relative upload fixes the *GPU*. Floating origin fixes the *CPU sim*: we keep
the active simulation near `(0,0,0)` so physics and gameplay never integrate large
coordinates. When the anchor camera drifts past a threshold (a few km), we **rebase**:

```
if cam.translation.length() > origin.threshold {
    let shift = cam.translation;              // DVec3, the offset to remove
    for t in active_transforms { t.translation -= shift; }
    origin.world_offset += shift;             // remember where (0,0,0) really is
}
```

- **What rebases:** the `translation` of every *active* (player-frame) entity, and the
  `world_offset` record on the `FloatingOrigin` resource.
- **What must NOT rebase:** velocity, acceleration, force, momentum, and any *relative*
  constraint (joints, contact offsets, surface-velocity from `∂f/∂t`). These are
  **translation-invariant** — `(p - shift)' = v` because the shift is constant — so a
  rebase is invisible to the integrator. This is the property that makes it safe.
- **When:** at a defined quiet point **between fixed steps** (see §5), so a physics
  step never spans a rebase and never sees a big coordinate. The sim is identical
  before and after, just re-centered.

`world_offset` is `DVec3` and accumulates the true distance from the universe origin;
add it back only when you need an absolute coordinate (serialization, frame math).

> **As implemented (2026-07-02).** The shipped design inverts the sketch above in
> one important way: instead of rebasing *ECS translations*, the rebase lives
> **inside the physics sim**. `PhysicsWorld.origin` (`DVec3`) anchors an
> origin-relative frame — bodies, contacts, gravity centers and ray origins are
> all small `f32` numbers near that origin; each static collider is baked
> relative to its own `f64` anchor and re-offset (`anchor − origin`, computed in
> `f64`) on every rebase, so repeated rebases accumulate **zero** error into
> geometry. The ECS keeps stable, absolute `f64` translations at all times, which
> means **scripts and gameplay code never observe a rebase** — a Lua variable
> holding a world position is still valid afterward. The trigger is the active
> camera drifting > `FloatingOrigin.threshold` (4 096 m) from `origin`; the new
> origin is the camera position rounded to whole units (whole-number shifts are
> exact in `f32`). Sim → ECS writeback also **interpolates** between the last two
> fixed steps by the accumulator fraction, so rendered motion is smooth at any
> frame rate. **Terrain** (2026-07-02, same day): per-volume fields are node-local,
> and there is **no combined field at all anymore** — every terrain volume is
> uploaded into one shared 3D atlas at its NATIVE voxel resolution and the shader
> folds them with the same smin the old CPU combine used (fields continued as air
> outside their boxes — the near-zero-shell lesson — and the slab-edge taper
> applied once, post-fold, against the *union* of the boxes so interior seams stay
> seamless while true outer faces still slope to air). Placement is per-volume:
> node `f64` anchor + local center composed in `f64`, then camera-relative, read
> fresh every frame — moving a terrain costs zero GPU work. Physics likewise
> anchors each volume as its own collider at native resolution. So neither
> collision nor rendering degrades with distance from the origin OR with distance
> *between* volumes; the only capacity limits are explicit and surfaced (16
> volumes per scene / the device's 3D-texture size, warned in the Console, never
> silent). Visual regression proof: `terrain_far_probe` (two-volume scene at 1e7
> units) is bit-identical to `terrain_blend_probe` (same scene at the origin).

### Layer 3 — `f64` authoritative transforms

`Transform.translation` is **`DVec3`** (glam's `f64` vec) — rotation and scale stay
`f32` (an `f32` quaternion is fine to nanoradians; nobody is 10^6 units of *rotation*
from origin). Each frame we derive a small **camera-relative `f32` render transform**
from the `f64` translation + the `f32` rotation/scale. Authoritative state is `f64`;
the GPU-facing state is the cheap `f32` projection of it.

### Layer 4 — Hierarchical reference frames (galaxy and beyond)

For scales past flat `f64` we nest frames. A **frame tree** — `galaxy → star system →
planet → local` — where each frame stores a high-precision **offset from its parent**.
Only the player's *local* frame is simulated at full precision near the origin;
sibling and ancestor frames are **composed camera-relative at render time** by walking
the offsets in `f64` and downcasting at the end (Layer 1, generalized):

```
            ┌─ galaxy ───────────────── offset_from_parent = 0
            │   ├─ system "Sol"  ─────── DVec3 (light-years, f64)
            │   │    ├─ planet "Floo" ── DVec3 (AU-scale, f64)
            │   │    │    └─ LOCAL  ◀──── the player; simulated at origin
            │   │    └─ planet "Bar" ──  DVec3
            │   └─ system "Vega" ──────  DVec3
 p_render = downcast_f32( Σ offsets(local..camera_frame) + (p_local - cam_local) )
```

Each entity carries a `frame: FrameId` and a local `DVec3`. To render anything you sum
the chain of `f64` offsets from its frame up to the camera's frame, add the
camera-relative local delta, then cast. The whole galaxy is representable because no
single number ever has to hold galaxy-scale *and* meter precision at once — the scale
lives in the offset chain, the precision lives in the local coordinate.

## 3. Data structures (Rust-ish)

```rust
// floptle-core — the authoritative transform (see ./scene-and-nodes.md catalog)
struct Transform {
    translation: DVec3,    // f64 authoritative world (within its frame)
    rotation:    Quat,     // f32 — plenty precise
    scale:       Vec3,     // f32
    frame:       FrameId,  // which reference frame (FrameId::ROOT until galaxy-scale)
}

// The single moving-origin record. One per active sim (ECS resource).
struct FloatingOrigin {
    world_offset: DVec3,   // accumulated true distance of (0,0,0) from universe origin
    anchor:       Entity,  // the camera that drags the origin (usually the active cam)
    threshold:    f64,     // rebase when anchor strays past this (meters), e.g. 4_096.0
}

// A node in the frame tree (galaxy → system → body → local). Layer 4.
struct ReferenceFrame {
    id:               FrameId,
    parent:           Option<FrameId>,
    offset_from_parent: DVec3,   // f64 high-precision offset
    rotation:         Quat,      // optional frame orientation (e.g. gravity-aligned)
}

// Derived each frame, consumed by the GPU. Small numbers only. Never serialized.
struct RenderTransform {
    model_view: Mat4,   // f32, camera-relative — ready to multiply by projection
}
```

**How the renderer consumes them** — once per frame, after the sim settles:

```rust
// for each visible entity, in floptle-render's upload step
let rel: DVec3 = frame_chain_offset(t.frame, cam.frame)   // Σ f64 offsets (Layer 4)
               + (t.translation - cam.translation);        // f64 camera-relative
let mv = Mat4::from_scale_rotation_translation(
            t.scale, t.rotation, rel.as_vec3());            // downcast LAST
render.push(RenderTransform { model_view: cam_view_rot * mv });
```

**How physics consumes them** — `floptle-physics` reads `translation` directly and,
because Layer 2 keeps the active frame near origin, the `f32` SDF math
(`distance`, `∇f`, sweeps) already operates on small numbers. It samples
`f(p_local, t)` in *local* coordinates; the field never sees the world offset (see §4).

## 4. Integration — it must "just work"

**Scene / Node model ([`./scene-and-nodes.md`](./scene-and-nodes.md)).** Authoring is
ordinary world space. The inspector shows whatever coordinate the author typed; the
node tree, prefabs, and scripts are unchanged. Under the hood the `Transform` is `f64`
and (at galaxy scale) frame-tagged. **RON serialization** stores the `f64` translation
and, when present, the frame — so a saved scene is exact, not a rounded `f32`:

```ron
// far-flung station, authored in plain world coords; frame defaults to Root
Node( id: 42, name: "RelayStation", parent: None,
    transform: (
        pos:   (1.4960e11, 0.0, -3.84e8),   // f64 — ~1 AU out, exact
        rot:   (0, 0, 0, 1),
        scale: (1, 1, 1),
        frame: "Sol/Floo",                  // omitted ⇒ Root (most scenes)
    ),
    components: [ MeshRenderer( mesh: "models/relay.glb#Mesh" ) ],
),
```

```
┌ Inspector ─────────────────────────────────┐
│ RelayStation                                │
│ Transform   pos[1.4960e11  0  -3.84e8]  (f64)│   ← author edits world coords
│ Frame       Sol/Floo                        │   ← auto for galaxy scenes; else Root
└─────────────────────────────────────────────┘
```

**SDF / procedural worlds.** Fractals and SDFs evaluate in **local coordinates plus a
frame offset**, so the field functions are scale/offset-aware: to march or collide a
distant body you transform the sample point into that body's local space first, then
evaluate `f(p_local, t)`. "Infinitely deep" zoom works the same way — descending into a
fractal is a *frame* with a shrinking scale and an `f64` offset, so the local
coordinates the field sees stay `O(1)` no matter how deep you go. The renderer and
physics share that one offset-aware field ([`./physics.md`](./physics.md),
[`./renderer.md`](./renderer.md)).

**Determinism & the fixed-step loop.** The deterministic fixed-timestep loop is
preserved. Rebasing is a pure translation applied at one **defined point — after the
last fixed step of a frame, before render interpolation** — never mid-step, so two
runs from the same inputs rebase at the same instant and produce bit-identical state.
The rebase is order-independent (it touches each entity's translation by the same
constant).

**Gravity-aligned player frame ([`./gravity-and-density.md`](./gravity-and-density.md)).**
The player's local frame can double as the **gravity-aligned** frame: `ReferenceFrame.
rotation` orients "down" to `g(p)` so walking up a fractal wall or around a small body
keeps a stable up-vector for the camera and controller. Origin-anchoring and
gravity-alignment are the same frame — one rebase keeps both the position *and* the
orientation reference local.

**Networking (deferred).** A future [`floptle-net`](./networking-future.md) must have
peers **agree on `world_offset` and the frame tree** so everyone shares one space;
since the authoritative sim is already `f64` + frames + deterministic fixed-step, the
seam is clean. Out of scope now — noted so we don't paint over it.

## 5. Performance & correctness

- **Camera-relative upload is ~free.** One `f64` subtract per object (or per instance
  batch), folded into the cull/upload pass we already run. No extra GPU cost — the GPU
  sees the same `f32` matrices it always would.
- **Rebase cost is bounded.** A rebase touches only **active (player-frame) entities'
  translations** — a linear sweep of one `DVec3` field, cache-friendly, typically a few
  thousand entities. Distant frames are *not* touched (their offsets are unchanged).
- **Avoid hitches:** pick a threshold large enough that rebases are rare (km-scale) but
  small enough that coordinates stay modest; **amortize** by spreading a very large
  rebase across the entities over a couple of frames *only if* needed (usually
  unnecessary). Because it happens at the quiet inter-step point, it never stalls a
  physics solve.
- **Frame composition cost (Layer 4)** is an `f64` offset walk up the (shallow) frame
  tree per visible frame, not per entity — cache the per-frame camera-relative offset
  once and reuse it for every entity in that frame.
- **Determinism / repro maintained** end-to-end: `f64` authoritative state, a
  translation-invariant rebase at a fixed point, deterministic iteration order. Same
  inputs → same trajectory → same render, on a platform, bit-for-bit.

## 6. Authoring / UX — essentially nothing

**Zero required developer work** is the whole point. There is no API to call, no
component to add, no "enable large coordinates" flag, no rule about staying near the
origin. You write world-space code; the engine keeps you precise. Minimal knobs exist
for the rare case:

- `FloatingOrigin.threshold` — rebase distance (default a few km).
- **Origin anchor** — which camera drags the origin (defaults to the active camera);
  set per-camera when a cutscene/security cam shouldn't move the world.
- A **debug overlay** (editor + runtime toggle) showing live `world_offset`, the active
  `FrameId`, distance-to-rebase, and last-rebase time:

```
┌ Large-World ───────────────────────────┐
│ origin offset  (1.50e11, 0.0, -3.8e8)   │
│ active frame   Sol/Floo  (depth 3)      │
│ to rebase      1.9 km / 4.0 km          │
│ last rebase    312 ms ago               │
└─────────────────────────────────────────┘
```

## 7. Near-term vs Future

- **Near-term (lands with the Phase-1 camera + fixed-step loop):** camera-relative
  rendering, floating origin, and `f64` authoritative transforms — Layers 1–3. This
  alone delivers a jitter-free solar-system-scale world, which covers nearly every
  scene we'll author first.
- **Later:** the full **hierarchical reference-frame tree** (Layer 4) for galaxy scale
  and infinite fractal depth. The data structures above leave the seam in (every
  `Transform` already carries a `FrameId`, defaulting to `Root`), so turning it on is
  additive, not a rewrite.

## 8. Out of scope

- **Relativistic effects** — no length contraction, time dilation, light-travel delay,
  or relativistic aberration. We solve **numerical precision and jitter**, not
  astrophysical realism. "Simulate a galaxy" here means *represent and render it
  without floating-point error*, not model its physics.
- **Per-object adaptive precision / arbitrary-precision arithmetic** — `f64` + frames
  is the model; we don't chase `f128` or bignum.
- **Multi-scene streaming / chunk loading** — an orthogonal runtime concern
  ([`./scene-and-nodes.md`](./scene-and-nodes.md) "out of scope"), not part of the
  coordinate system.
