# Floptle — Renderer (`floptle-render`)

> The otherworldly renderer: a render graph whose default toolbox is SDFs and
> raymarching, not just triangles. See [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §4,
> [`../decisions/0002-render-backend-wgpu.md`](../decisions/0002-render-backend-wgpu.md),
> the shader IR in [`./shaders.md`](./shaders.md), materials in
> [`./materials-and-textures.md`](./materials-and-textures.md), and SDF physics in
> [`./physics.md`](./physics.md).

The job here is the [VISION](../VISION.md) reaction — *"I've never seen anything
like this, it's from another dimension"* — surreal, dreamlike, and willing to
break the laws of light and geometry. We render fractals as math, fly the camera
*inside* them, morph their geometry in real time, and run a post stack that
treats physical correctness as optional.

## 1. Render graph

`floptle-render` owns a small **render graph**. Each pass declares the resources
it **reads** and **writes**; the graph topologically orders passes, allocates a
transient resource pool, and **aliases** transient targets whose lifetimes don't
overlap (a half-res blur target can reuse the memory of a finished feedback
buffer). Backends are wgpu (Vulkan/Metal/DX12/GL) — see ADR-0002.

```rust
struct PassDesc {
    name: &'static str,
    reads:  Vec<ResId>,          // textures / buffers consumed
    writes: Vec<ResId>,          // targets produced (transient unless persistent)
    kind:   PassKind,            // Raster | Compute | Fullscreen
    run:    fn(&mut PassCtx),    // records draw/dispatch into the encoder
}

struct ResDesc {
    id:        ResId,
    format:    Format,           // Rgba16Float for HDR scene, etc.
    size:      SizeSpec,         // Full | Half | Fixed(w,h)
    persist:   bool,             // true = survives across frames (feedback history)
}
```

```
declared passes ─▶ build DAG from reads/writes ─▶ topo sort
                                                     │
                          alias non-overlapping transients (pool)
                                                     │
                          for pass in order: barrier? → run(ctx) → submit
```

Rules: a resource is **transient** by default (graph owns its memory and may
alias it); marking `persist` keeps it stable across frames, which is what
feedback/echo passes need (read last frame, write this frame, ping-pong). The
graph is rebuilt cheaply per frame so passes can be toggled by the active
post-effect set without restructuring code.

## 2. Pass stack

The signature look is assembled from four stages, in graph order:

```
                 ┌──────────────────────────────────────────────┐
 scene ─────────▶│ (1) RASTER     triangles → HDR color + depth  │
                 └───────────────┬──────────────────────────────┘
                                 │  (shared depth buffer)
                 ┌───────────────┴──────────────────────────────┐
 fractals ──────▶│ (2) RAYMARCH   SDF fields, depth-tested vs    │
                 │                raster depth, writes HDR+depth  │
                 └───────────────┬──────────────────────────────┘
                                 │
                 ┌───────────────┴──────────────────────────────┐
 materials ─────▶│ (3) BIND       compiled WGSL from shader IR    │
                 │                drives both raster & raymarch    │
                 └───────────────┬──────────────────────────────┘
                                 │
                 ┌───────────────┴──────────────────────────────┐
 looks ─────────▶│ (4) POST       reality-bending screen passes   │
                 └──────────────────────────────────────────────┘
```

1. **Raster pass** — ordinary triangle meshes: Blender glTF imports
   ([ADR-0006](../decisions/0006-asset-pipeline-gltf.md)) and the scene-builder's
   procedural shapes. Standard forward draw into an HDR `Rgba16Float` target with
   a depth buffer. Material/shader from the IR.
2. **SDF / raymarch pass** — the headline. Fractals and impossible/volumetric
   geometry rendered as math (§3). Shares the raster depth buffer so raymarched
   and rasterized geometry **interpenetrate correctly**.
3. **Material / shader binding** — every drawable references a compiled shader
   (WGSL from [`./shaders.md`](./shaders.md), validated by naga) plus its param
   block; raymarch SDFs are themselves authored in the same IR.
4. **Reality-bending post stack** (§5) — the screen-space passes that break
   lighting/physics norms for the dreamlike, nostalgic-underneath look.

## 3. Raymarching (the headline)

We render fractals and volumes by **sphere marching** a signed distance / distance
estimator function `f(p, t)`. The same `f` is what `floptle-physics` collides
against ([ADR-0012](../decisions/0012-physics-sdf-first.md)) — one field, drawn
and collided.

**Distance-estimator fractals.** Stdlib SDF nodes provide the canonical set:
Mandelbulb, Mandelbox, Menger sponge, Kleinian / IFS, plus boolean & smooth-min
combinators and domain warps so authored shaders can compose new ones.

**Sphere-marching loop** (per pixel, in the fullscreen WGSL):

```wgsl
var t = near;                      // start distance along ray
for (var i = 0u; i < MAX_STEPS; i++) {
    let p = ro + rd * t;           // ro = ray origin (camera), rd = direction
    let d = map(p, time);          // distance estimate to nearest surface
    if (d < EPS * t) { hit = true; break; }   // pixel-relative epsilon
    t += d * STEP_RELAX;           // relax factor < 1.0 for thin/fractal detail
    if (t > far) { break; }        // early-out: escaped the bounds / far plane
}
```

**Cheap normals from the SDF gradient** — central differences, no stored mesh:

```wgsl
fn normal(p: vec3<f32>) -> vec3<f32> {
    let e = vec2(EPS, 0.0);
    return normalize(vec3(
        map(p+e.xyy,time) - map(p-e.xyy,time),
        map(p+e.yxy,time) - map(p-e.yxy,time),
        map(p+e.yyx,time) - map(p-e.yyx,time)));
}
```

**Soft shadows & AO from the field** — soft shadows by marching toward the light
and tracking the smallest `d/t` ratio (penumbra); ambient occlusion by sampling
`map()` a few steps along the normal and accumulating the deficit. Both are pure
SDF tricks: no shadow maps, no GI bake. *(Both shipped 2026-07-02: AO as the
scene PostProcess node's `SDF (true)` mode — see
[`./post-processing.md`](./post-processing.md) — and sun shadows from the
Lighting node, with meshes receiving/casting via the shared field module —
see [`./shadows.md`](./shadows.md).)*

**Going inside.** "Fly inside a fractal" just means the camera origin `ro` lives
*within* the field. We march from `near` (a small positive start, not the camera
plane) so we don't immediately self-hit; when `f(ro) < 0` we're inside a solid
lobe and the loop marches outward to the inner surface. No special geometry — the
math is the same whether you're outside looking in or tumbling through a lobe.

**Bounded volumes vs fullscreen.** Two modes:
- **Fullscreen** — the field *is* the world (fly-through fractal scenes).
- **Bounded SDF volume** — a node with an OBB; the raymarch runs only for pixels
  whose ray intersects the box, depth-tested against the raster scene. This is how
  a single impossible object sits inside an otherwise triangle world cheaply.

**Time-morphing parameters.** `f(p, t)` takes `time` and per-field params (fold
limits, power, IFS transforms). Driving those from curves/uniforms makes one
fractal **melt into another** in real time — Mandelbox power sweeping, Kleinian
inversions drifting — patterns shifting into each other. These params are plain
shader uniforms, animatable from scripts or VFX curves.

**Compositing with raster.** The raymarch pass converts hit distance `t` to a
clip-space depth and writes it to the **shared depth buffer**, so a rasterized
mesh can occlude (or be occluded by) the fractal per-pixel. HDR color accumulates
into the same scene target. One depth buffer, two geometry models, correct
interleaving.

## 4. Dynamic / morphing meshes

Triangle meshes can morph too — "shifting vertices and patterns." Two paths:

- **Vertex-shader displacement** — cheap, stateless: displace by noise/SDF in the
  vertex stage of the material. Good for waves, breathing surfaces, flow fields.
- **Compute displacement** — a compute pass writes into the vertex buffer
  (or a parallel deformed-position buffer) before the raster pass reads it; used
  when displacement is stateful, needs neighbor data, or feeds physics.

**GPU buffer strategy:** keep an immutable **rest-pose** vertex buffer and a
**deformed** buffer the compute pass writes each frame; the raster pass binds the
deformed buffer. Double-buffer when a frame needs last-frame positions (velocity,
trails). Pooled allocations (ADR-0008) avoid per-frame churn.

## 5. Reality-bending post stack

Each is a graph pass (mostly fullscreen). They intentionally break physical
light transport — that's the point. The deepest layer is *nostalgic*: palette
cycling and retro quantization evoke old demos and 8-bit dreams.

```
HDR scene ─▶ feedback/echo ─▶ domain-warp ─▶ color-transport ─▶ chroma/temporal ─▶ palette ─▶ dither ─▶ present
              (persist hist)   (space-melt)   (non-physical)     (warps)          (cycle)    (quantize)
```

- **Frame feedback / echo trails** — blend a *persistent* history target with the
  current frame (ping-pong); decay + warp the history for motion echoes and
  infinite-tunnel feedback. Needs `persist` resources.
- **Non-physical color transport** — move/refract color along screen-space fields
  that obey no real optics (color flows uphill, splits by luminance, leaks across
  edges).
- **Palette cycling** — index colors through a small LUT and rotate the LUT over
  time. The classic nostalgic effect; also drives posterized dream looks.
- **Domain-warp / space-melt** — distort UVs by layered noise so the whole image
  ripples, melts, or breathes.
- **Chromatic & temporal warps** — per-channel offset/aberration; temporal warp
  samples the feedback history at an offset for smearing and ghost-time effects.
- **Dithering / retro quantization** — ordered/blue-noise dither + bit-depth
  reduction for the retro, banded, demoscene finish.

All are data-driven post nodes (also authored in the shader IR), composable in
any order from a per-scene/per-camera post chain.

## 6. Performance posture

"Hyperoptimized" is a requirement, not a hope (see [ARCHITECTURE](../ARCHITECTURE.md) §9).

- **Raymarch step budget** — hard `MAX_STEPS` cap per pass; `STEP_RELAX` and a
  pixel-relative epsilon (`EPS * t`) trade detail for speed; early-out on far/escape.
- **Half-res + upscale** — heavy raymarch and feedback passes run at half (or
  quarter) resolution into a transient target, then upscale (bilinear or a small
  edge-aware filter). The graph's aliasing keeps these targets cheap.
- **Bounded volumes** — prefer OBB-bounded raymarch over fullscreen when the
  impossible object is local; skips marching for off-object pixels.
- **In-engine frame profiler** — per-pass GPU timestamps surfaced in the editor;
  the step-count heatmap shows which pixels burn the raymarch budget. We *measure*
  lightweight, we don't assume it.
- **Cross-platform** — one wgpu path for Metal/Vulkan/DX12/GL. Watch
  workgroup-size and `Rgba16Float` storage-image support across backends; keep a
  half-res fallback for weaker GPUs.

## 6b. Frame pacing, and telling it apart from being slow (v0.53.1)

**A frame rate is two numbers pretending to be one.** How long a frame takes to
build, and how often the display accepts one. They are usually close enough that
nobody separates them — until they are not, and then every diagnosis goes wrong.

The engine presented with `PresentMode::Fifo`, hardcoded. Fifo is the right
default: every frame shown, in order, at the display's cadence, so what the
simulation sampled matches what reaches the glass, and frame times are
predictable. `Mailbox` was tried and rejected because the frames that reach the
glass sampled the world at moments unrelated to when they appear, which reads as
movement judder that comes and goes with the window mode.

What that reasoning missed is that **Fifo does not always do what it says.** On
at least one ordinary Wayland setup, a window that does nothing but clear itself
blue presents at a flat **20.0 fps on a 60 Hz display** — every third refresh —
while the same window under `Mailbox` or `Immediate` runs at thirteen thousand.
That is not a load; it is the presentation path. With the mode hardcoded, a
project on such a machine had no way to escape it and no way to tell it apart
from an engine that was simply slow. A real scene measured 8 ms of work per frame
and presented at 20 fps, which reads exactly like a renderer in trouble.

So:

- **`Vsync` is a project setting** (`ProjectConfigDoc::vsync`, Project Settings ⏵
  Rendering ⏵ Frame pacing): `On` = Fifo and still the default, `Adaptive` =
  Mailbox, `Off` = Immediate. A mode the surface does not support falls back to
  Fifo — every surface supports Fifo — and `Gpu::set_vsync` **returns the mode it
  actually applied**, which the editor prints, so a fallback is visible rather
  than being mistaken for a setting that did not help.
- **The window title reports the frame's own cost beside the rate**: `20 fps
  (8.2 ms/frame)`. The two together answer the question an fps number alone
  cannot. Blocking in `acquire()` is measured and excluded, because waiting for
  the display is not the same thing as being slow.

`examples/present_probe.rs` is the tool that settled it and is kept for the next
time: forty lines of winit and wgpu that clear a window and report the rate. If
it reads 60 and the editor reads 20, the editor is doing something; if both read
20, the display path is.

## 6c. Where the time goes, per pass (v0.54.0)

Frame pacing above answers "is the engine slow or is the display pacing me". It
does not answer "which part of the frame is slow", and until v0.54.0 nothing
did. The only method available was to switch a feature off and look at the
number again — slow, and under vsync useless, because every answer comes back
quantised to the same value.

`gpu_timer.rs` measures the frame **on the GPU**, and that distinction is the
whole point: the CPU records commands and moves on, so timing the recording
measures how fast the encoder ran, which is nearly always fast and nearly never
the answer. `write_timestamp` puts a marker in the command stream instead.

- **Requested, never required.** `timing_features(&adapter)` asks for
  `TIMESTAMP_QUERY` + `TIMESTAMP_QUERY_INSIDE_ENCODERS` only where the adapter
  offers them; `GpuTimer::new` then asks the **device**, because the two can
  disagree and the device is what decides whether the call is legal. No timer
  means the panel says so — it does not report zeros.
- **A mark is its own submission.** Every pass in `floptle-render` creates and
  submits its own encoder, so there is no shared one to bracket. A mark carries a
  single command, which is cheap and needs no change inside any pass.
- **Nothing is submitted while the panel is shut.** `gpu_timing_open` gates it
  all, so a profiler nobody is reading costs nothing.
- **n labels need n+1 marks.** A label names the region that *follows* it;
  `GpuTimer::end` writes the closing one. Off by one and every cost is reported
  against its neighbour's name, which is worse than no profiler because the
  answer gets acted on.
- **Results arrive a frame or two late**, deliberately. Blocking for a readback
  would make the profiler the most expensive thing in the frame.

`Window ⏵ ⏱ Frame timing` in the editor; `FLOPTLE_GPU_TIMING=1` opens it at
startup and repeats the numbers to stdout, which is the form a measurement needs
when the person reading it is not the person at the window.

**What it found first.** Pointed at a Backrooms-style interior reported as
running at 20–32 fps, it named `opaque + lighting` as 6.7 ms of an 8.3 ms frame
in one reading. Ablation inside that pass then put 6.0 ms of it in the volumetric
fog, and 4.45 ms of *that* in light injection — `fog_inscatter` was calling
`area_terms` (for a rect emitter: four quaternion rotations and an edge integral)
per lamp, per step, per pixel, and using two numbers out of it. There is no
surface in mid-air, so there is no `ndl` to integrate and no mirror direction to
find a representative point for. See `field.wgsl`'s `fog_emitter`, `fog_extent`
and `fog_noise_stride`. That pass now costs 2.7 ms.

The general lesson is the one the vsync investigation already taught in a
different form: **an ablation is only as good as its resolution.** Toggling
features under vsync gave six identical readings. The profiler gave the answer in
one frame.

## 7. Out of scope

We are lightweight — **not Unreal, not photoreal**. Explicitly *not* doing:

- PBR film realism / physically accurate material response.
- Lightmapping or baked global illumination.
- Ray-traced reflections/GI for *realism* (we raymarch for *strangeness*, not mirrors).
- A megapass deferred renderer with the full G-buffer feature soup.

If an effect serves correctness over wonder, it doesn't belong here.
