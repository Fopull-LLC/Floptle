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
SDF tricks: no shadow maps, no GI bake. *(The AO half shipped 2026-07-02 as the
scene PostProcess node's `SDF (true)` mode — see
[`./post-processing.md`](./post-processing.md); shadows are planned in
[`./shadows.md`](./shadows.md).)*

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

## 7. Out of scope

We are lightweight — **not Unreal, not photoreal**. Explicitly *not* doing:

- PBR film realism / physically accurate material response.
- Lightmapping or baked global illumination.
- Ray-traced reflections/GI for *realism* (we raymarch for *strangeness*, not mirrors).
- A megapass deferred renderer with the full G-buffer feature soup.

If an effect serves correctness over wonder, it doesn't belong here.
