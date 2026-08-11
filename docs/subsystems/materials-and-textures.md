# Floptle — Materials & Textures (`floptle-assets` + `floptle-shader`)

> Assign a shader, tweak some knobs, drag a texture on, tile it — *without ever
> writing a shader to repeat a texture.* See the shader IR in
> [`./shaders.md`](./shaders.md), the asset database & import in
> [`./asset-pipeline.md`](./asset-pipeline.md), the editor's drag-on-object flow
> in [`./editor.md`](./editor.md), the renderer in [`./renderer.md`](./renderer.md),
> and [`../decisions/0007-shader-ir.md`](../decisions/0007-shader-ir.md).

The pain we're solving: in most engines, tiling a texture onto a non-UV-mapped
object means *writing a shader*. Floptle says no. Tiling, clamping, mirroring,
offsetting, and projecting are **sampler + UV-transform settings** on the
material — set them by dragging and clicking. Shaders are for *looks*; textures
and their tiling are *data*.

> **STATUS (2026-07-15): shipped, shaped by the code as it exists** (see
> [`./shaders.md`](./shaders.md)). The
> fixed-function `Material` component is permanent; `shader: Option<String>`
> is an OPTIONAL `.flsl` reference on it, with `shader_params` /
> `shader_textures` / per-slot `shader_tiling` maps. The **tiling block** is
> live on both paths: `Tiling::Uv { count, offset, rotation }` or
> `Tiling::Triplanar { scale, blend }` per BINDING (the base texture and each
> shader slot), with mode/fields rows in the Inspector; wrap (`Repeat / Clamp /
> Mirror` — this doc's mirror-on-alternate) and filtering stay per-texture
> settings in the Assets panel. Triplanar projects in OBJECT space (stable
> under the floating origin). §2's `material.ron` shape shipped as the
> `MaterialDoc` fields instead of a new asset kind.

## 1. Separation of concerns

Two distinct things, deliberately:

- **Texture** = an image *plus how it's sampled and tiled* across a surface
  (repeat/clamp/flip/count/offset/rotation, or triplanar projection). No shading
  decisions live here.
- **Material** = a **shader-IR reference** + its **params** + **texture
  bindings** (which texture goes in which slot). This is where color, lighting,
  and effects on the geometry+texture are decided.

```
TEXTURE  ── image + tiling/sampler/UV-transform ──┐
                                                   ├──▶ drawn surface
MATERIAL ── shader.flsl + params + tex bindings ──┘
```

A texture can be reused by many materials; a material can bind many textures.
The asset database tracks which-uses-which ([`./asset-pipeline.md`](./asset-pipeline.md) §2).

## 2. Material data model

A material is RON, like everything authored ([ARCHITECTURE](../ARCHITECTURE.md) §8).
It names a compiled shader, sets that shader's exposed params, and binds textures
to the shader's sampler slots — each binding carrying its own **tiling block**.

```rust
struct Material {
    name:    String,
    shader:  AssetRef,                  // → shaders/*.flsl (compiled to WGSL)
    params:  BTreeMap<String, ParamVal>,// uniform values the shader exposes
    textures: BTreeMap<String, TexBinding>, // slot name → texture + tiling
    blend:   BlendMode,                 // Opaque | AlphaBlend | Additive | ...
    cull:    CullMode,                  // Back | Front | None (impossible geo)
}

struct TexBinding {
    texture: AssetRef,                  // → assets/textures/*
    tiling:  Tiling,                    // §3 — the no-shader-needed part
    uv_set:  u8,                        // which mesh UV channel (usually 0)
}
```

### `material.ron` example

A tiled stone floor: one shader, a base color texture repeated 4×4 with mirrored
seams, plus a few lighting knobs the shader exposes.

```ron
Material(
    name: "stone_floor",
    shader: "shaders/lit_textured.flsl",
    params: {
        "tint":      Color((0.9, 0.9, 0.95, 1.0)),
        "roughness": Float(0.7),
        "emissive":  Float(0.0),
    },
    textures: {
        "albedo": (
            texture: "assets/textures/stone_albedo.png",
            uv_set: 0,
            tiling: Uv(Repeat(
                count:  (4.0, 4.0),     // 4×4 tiles across the surface
                offset: (0.0, 0.0),
                rotation: 0.0,          // degrees
                flip:   Mirror,         // mirror on alternate repeats — no seams
                clamp:  false,
            )),
        ),
    },
    blend: Opaque,
    cull:  Back,
)
```

> **SPRITESHEETS (shipped 2026-07-30).** A texture sliced into a `cols`×`rows`
> grid in its **asset settings** (the same grid a UI image reads) can be indexed
> by a Material: `sheet_cols` / `sheet_rows` / `cell` on the component, one cell
> drawn over the mesh's UVs, row-major from the top-left. Picking the texture in
> the Inspector inherits its grid, and a clickable cell grid appears under the
> texture row — the same widget the UI element inspector uses, so a sheet reads
> identically on a HUD image and on a character's face plane.
>
> **Animate it** by stepping `cell`: from Lua (`node:getcomponent("Material").cell
> = f`, or `node:setMaterial{ cell = f }`), or with a **stepped property track**
> in the Animation tab (✚ Property ▸ Material ▸ cell). Under the hood a sheet is
> the *cell's UV window* packed into the existing tiling lanes
> (`Material::effective_tiling`), so it costs no instance attribute (location 15
> is the last one), no shader variant, and a custom `.flsl`'s `baseTexture()` gets
> it for free. Consequences worth knowing:
>
> - A **sheet wins over a tiling block** — repeating or rotating one cell would
>   drag in its neighbours. The Inspector says so where the tiling rows were.
> - Set the texture's filter to **Pixelated** for pixel art; a smooth filter can
>   bleed half a texel from the neighbouring cell at the seam.
> - Cells clamp into the grid, and re-slicing the `.png` re-slices every material
>   using it (a now-missing cell falls back into range).
> - Raster surfaces only (meshes, primitives, map geometry) — blobs/SDF matter
>   have no UVs to window.

## 3. Tiling without a shader

Tiling is a `Tiling` value on each `TexBinding`. Two projection modes cover the
cases the developer hits, and **neither requires touching a shader** — the stdlib
`sample()` node honors them automatically ([`./shaders.md`](./shaders.md) §4).

```rust
enum Tiling {
    Uv(UvTransform),     // standard: tile across the mesh's UVs
    Triplanar(Triplanar),// project from 3 axes — for shapes with bad/no UVs
}

struct UvTransform {
    mode:     WrapMode,    // Repeat | ClampToBounds | MirrorRepeat
    count:    Vec2,        // repeats across the 0..1 UV span (e.g. 4×4)
    offset:   Vec2,        // scroll/shift the texture
    rotation: f32,         // degrees, around the UV center
    flip:     FlipMode,    // None | FlipX | FlipY | Mirror(on alternate repeats)
}

struct Triplanar {
    scale:    Vec3,        // world-space tile size per axis
    blend:    f32,         // sharpness of the axis blend at edges (0.5..8)
    offset:   Vec3,
}
```

**`WrapMode`** maps to wgpu sampler address modes plus our framing:

- `Repeat` — tile forever; `count` controls density.
- `ClampToBounds` — show the texture once, edges held to the surface bounds.
- `MirrorRepeat` — like repeat but each odd tile is flipped, hiding seams.

**`flip: Mirror`** is the "no visible seam" trick for organic textures — every
alternate repeat mirrors, so tile edges meet their own reflection.

### Triplanar — for the scene-builder's procedural shapes

The Cube/Sphere/Wedge/Stairs primitives ([`./editor.md`](./editor.md) §3) and
morphed meshes often have stretched or absent UVs. **Triplanar projection**
samples the texture three times — once per world axis (X/Y/Z) — and blends by the
surface normal. Result: clean, uniform tiling on *any* geometry with zero UV work.

```
        world-space stairs (UVs would stretch on the risers)
              │
        sample tex along  +X, +Y, +Z   ── weighted by |normal| ──▶ blended color
              │
        no UVs needed · consistent tile size in world units
```

Pick triplanar in the material editor with one toggle; set `scale` (world tile
size) and `blend` (edge sharpness). This is the default suggestion when a surface
reports poor UVs.

## 4. Built-in content (out of the box)

Floptle ships defaults so a new project is *immediately buildable* — no blank
canvas (these become real Fopull art before release,
replacing any OoT temps per [ADR-0010](../decisions/0010-temporary-assets.md)):

- **Built-in shaders** (`.flsl`): `unlit`, `lit_textured` (basic directional +
  ambient), `lit_color`, `triplanar_lit`, `emissive`, plus a couple surreal
  starters (`palette_cycle`, `space_melt`) showcasing the IR.
- **Built-in materials**: a neutral default (`default_grid`), `matte`, `metalish`,
  `glow` — each a thin binding over a built-in shader so it's a worked example.
- **Built-in textures**: grid/checker (the classic "is my UV right?" texture),
  noise, gradient ramps, and a few palette LUTs the color nodes use.

Every default is a normal asset you can copy and edit — they double as tutorials.

## 5. Data flow: material → pixels

```
material.ron ─▶ resolve shader (compiled WGSL, naga-validated)
            ├─▶ pack params into a uniform buffer
            └─▶ for each tex binding:
                  texture (GPU image + mips)  +  sampler(WrapMode)
                  + UV-transform / triplanar uniforms
                          │
                          ▼
               renderer binds pipeline + uniforms + textures ─▶ draw
```

The shader's `sample(slot, uv)` node reads the binding's tiling uniforms; param
changes are uniform writes (no recompile); swapping a *texture* re-points a bind
group. Material edits hot-reload live ([`./asset-pipeline.md`](./asset-pipeline.md) §2).

## 6. Editor UX — the Material Editor

A focused panel ([`./editor.md`](./editor.md) §2), live-previewed:

```
┌─ Material Editor ──────────────────────────────┐
│ Shader: [ lit_textured.flsl     ▼] [Open in VSCode]
│ ┌─ Params ─────────────┐  ┌─ Preview ────────┐ │
│ │ tint      ■ #E6E6F2  │  │   (sphere/quad/   │ │
│ │ roughness ▮▮▮▮▮▯ 0.7 │  │    your mesh)     │ │
│ │ emissive  ▯▯▯▯▯▯ 0.0 │  │   live wgpu       │ │
│ └──────────────────────┘  └──────────────────┘ │
│ ┌─ Textures ─────────────────────────────────┐ │
│ │ albedo  [stone_albedo.png] (drop here)     │ │
│ │   tiling: ( • Repeat  ○ Clamp  ○ Triplanar)│ │
│ │   count [4]×[4]  offset[0,0]  rot[0°]       │ │
│ │   flip: [Mirror ▼]                          │ │
│ └────────────────────────────────────────────┘ │
└────────────────────────────────────────────────┘
```

- **Assign a shader** from a dropdown of project + built-in `.flsl`; the param
  and texture-slot rows regenerate from the shader's exposed uniforms.
- **Drop a texture** onto a slot (from the Asset Browser) to bind it.
- **Tiling controls** sit right under each slot — radio for Repeat/Clamp/Triplanar,
  then count/offset/rotation/flip. Changes preview instantly.
- **Open in VSCode** jumps to the bound shader's `.flsl` ([ADR-0011](../decisions/0011-vscode-integration.md)).

### Drag-texture-onto-object-in-scene

The fast path the developer wants ([`./editor.md`](./editor.md) §3): drag a
texture from the Asset Browser straight onto a surface in the Scene View. Floptle:

1. Clones the object's current material (or makes one from `default_grid`).
2. Binds the dropped texture to the `albedo` slot.
3. **Auto-picks tiling**: good UVs → `Repeat` with a sane default count; poor/no
   UVs (procedural primitives) → `Triplanar`. A small popup lets you adjust
   count/flip immediately.

No dialog hunting, no shader writing — drop, see it tile, tweak.

## 6b. Surface maps, two lighting models, and the retro flags (v0.43)

> **STATUS (2026-08-09): shipped.** Fields on `Material` /`MaterialDoc`, rows in
> the Inspector, keys on `node:setMaterial{…}`, asserted by
> `crates/floptle-render/examples/pbr_probe.rs`.

### The maps

Four slots beside the base colour, each `None` by default and each with a
**neutral** 1×1 default bound when it is: a flat `(0.5, 0.5, 1)` normal and
white for the rest. A material that names no map therefore shades exactly as it
did before they existed, and there is no "is a map bound" flag anywhere that
could disagree with what is actually bound.

| slot | reads | means |
| --- | --- | --- |
| `normal_map` | RGB | tangent-space normal; `normal_strength` scales the tilt, **negative flips green** (the handedness fix) |
| `roughness_map` | **G** | × `roughness` |
| `metallic_map` | **B** | × `metallic` |
| `ao_map` | **R** | baked occlusion; × `occlusion_strength` |

The channels are not arbitrary: R/G/B is glTF's packed
occlusion-roughness-metallic layout, so **one image drops into all three
slots** and does the right thing.

Occlusion multiplies **ambient and indirect only, never the key light**.
Occlusion darkens light arriving from everywhere, not light arriving from one
place; applying it to everything is the usual mistake and reads as a surface
covered in grey smudges.

### No tangent attribute

The tangent frame is derived **per pixel from screen-space derivatives**
(`tangent_frame` in `raster.wgsl`), re-orthogonalised against the interpolated
normal so smooth shading still wins.

This is a decision, not a shortcut. The raster vertex stream is full at 16
attributes so there is no room for a tangent — but more to the point, most of
what this engine draws could never carry one: SDF terrain is re-extracted on
every sculpt dab, primitives and Model-tool meshes are generated, tilemaps are
rebuilt per frame. A per-pixel frame works on all of them, and on skinned
characters too, because it reads the position *after* skinning.

> **Gotcha, and it cost real time.** The published cotangent-frame derivation
> assumes screen +y points UP. Here it points DOWN. Under a downward y, `dpdy`
> of both position and UV come back with the opposite sign, which negates T and
> B *together* — every tangent-space normal ends up tilted the wrong way in both
> axes. Nothing looks broken: the surface is lit, the highlight moves, it is
> simply inside out. `pbr_probe` caught it as "the half tilted toward the light
> is the dark one".

### Two lighting models, neither a degraded version of the other

`Shading::Classic` is the Blinn-Phong that shipped from the start — a specular
colour, an exponent and a strength, all set by hand. `Shading::Physical` is
Cook-Torrance GGX with `roughness` and `metallic`: the highlight falls out of
the microsurface rather than being dialled in, a dielectric reflects white at
4%, and a metal has no diffuse at all and reflects its own colour.

A stylised surface wants the first and a realistic one wants the second, so
neither is simulated with the other's knobs. **A normal map, an occlusion map
and the retro flags apply under both** — they describe the surface, not the
shading model. So do rim and opacity, which are art direction rather than
physics. Only `roughness` and `metallic` are Physical-only.

`key_light_ggx` in `raster.wgsl` is deliberately the mirror image of
field.wgsl's `key_light`: same star loop, same `star_shadow` / `sun_shadow`
calls in the same places, so a Physical surface receives exactly the shadows a
Classic one does and the two can only differ in the BRDF.

### Reflections: the environment term

A Physical surface also reflects the **sky**. For a long time it did not, and
the absence was invisible in the code and glaring on screen: the GGX lobe was
correct and had nothing to put in it but the sun and the placed point lights,
so a metal at roughness 0 came out as a sun dot on black. Mirrors and crystal
balls were not hard to make, they were not expressible.

The sky is captured once per frame into an equirectangular map with a mip chain
(`env.rs`) and reaches shading through the SHARED field bind group, the same
route the baked GI takes. Capturing rather than calling the sky directly is what
makes all three sky sources — the solid vault, a skybox image and a `stage sky`
shader — arrive already resolved, and it produces the roughness chain a
reflection needs anyway.

`env_specular` in `raster.wgsl` reflects the view vector, samples the chain at a
level chosen by `sqrt(roughness)` (which spends more of the chain on the
polished end, where the eye reads the difference), and weights it with Karis'
analytic split-sum approximation — the alternative to shipping a BRDF lookup
table and a pass to bake it.

Two things follow that are worth knowing:

* **`Material::reflectivity` scales it**, defaulting to `1.0` — the honest
  amount. It is Physical-only, because the Classic model has no `f0` to weight
  an environment by.
* **For a metal, albedo IS `f0`.** A black metal reflects nothing, by
  definition and correctly. This is the trap `reflection_probe` was written
  against: an early version measured a black sphere, saw the analytic BRDF's
  grazing-sheen bias, and read as a passing test that proved nothing.

The mip chain is a box filter, not a true GGX prefilter — an approximation, and
a good one for skies, which are low-frequency. The case it gets exactly right is
the one that matters most: at roughness 0 a mirror samples level 0, which IS the
sky.

### Reflections of the scene (screen space)

The environment term above answers "what is the sky doing behind this surface".
It cannot answer "what is standing in front of it", and a floor that shows the
sky but not the table on it does not read as a floor. Screen-space reflections
are the other half, switched on per scene by `Light::reflections` (the Lighting
node — see [light](light.md)) rather than per material: whether a mirror
shows the room is a fact about the renderer, and every Physical surface with
some reflectivity picks it up at once.

`ssr_trace` in `raster.wgsl` marches the reflected ray against the **opaque depth
prepass**, uniformly in SCREEN space — equal world steps would crowd hundreds of
samples into the few pixels nearest the surface and then leap whole objects in
the distance, which is backwards for something read at screen resolution. The
world position per sample is recovered perspective-correctly, by interpolating
`1/w` alongside `position/w`. On a crossing it bisects five times, then reads the
colour from the scene history.

**It reads the PREVIOUS frame** (`ssr.rs`). Shading is forward: when a fragment
runs, most other pixels' colours do not exist yet and many belong to draws not
yet issued. So the choice is between a deferred renderer — a G-buffer written by
every one of the raster pass's pipeline variants, with the specular term moved
out of the forward shader entirely — and reflecting the frame that has finished.
The cost is one frame of lag in a reflection's CONTENTS; its geometry is this
frame's, because the march is against this frame's depth.

The history is captured after the scene composites and before post, so it holds
linear HDR with no tonemap, bloom or grade in it — a reflection is part of the
scene and must go through the tonemap WITH it. It is half resolution with a mip
chain, and roughness picks a level by `sqrt(roughness)`, the same index the sky
chain uses, so a surface reflecting some sky and some scene blurs both equally.

Three ways a screen-space hit is a lie, all faded rather than cut so the sky
takes over without a seam: the frame edge (no data past it), rays pointing back
at the camera (what they want is behind the viewer), and long rays (more scene
crossed that the camera cannot see around). **A miss falls back to the
environment map**, which is why the two features are complements rather than
alternatives — and why the fallback must never be black.

Because the world is camera-relative (ADR-0015) the stored view-projection is not
usable as taken: a rock that has not moved has different coordinates in each
frame. `SceneHistory::prev_view_proj` pre-translates by how far the camera went,
the same correction motion blur makes. Left out, every reflection slides whenever
the camera dollies.

**Every view has its own history**, because a history carries the camera it was
taken from and the resolution it was taken at. The window has one; a docked Game
panel has its own; a thumbnail or a GI bake has none and reflects the sky, which
is what it did before any of this existed. Sharing one would have each view
reprojecting the other's frame, which reads as the reflection tearing.

### Reflection probes: what a room reflects (v0.53.0)

Screen-space reflections answer "is what I am reflecting on screen?". The
environment map answers what is left, and until now it held the **sky**. Outdoors
that pair is nearly complete: a ray that leaves the frame leaves toward the
horizon, and the sky is genuinely what is out there. Indoors it is badly wrong —
a polished floor in a corridor shows a strip of the visible scene and then
daylight, through the ceiling, in a sealed room.

`Matter::ReflectionProbe` is the third answer. It captures the view from its own
position and every reflective surface inside its **box** falls back to that
instead of the sky.

**The capture is six 90° renders through `render_world_into`** — the same path
every other offscreen view uses, and the same conclusion the GI bake reached: a
probe is a camera. `reflect.rs` folds them into one equirectangular map per
probe, in a texture array with the same box-filter roughness chain the sky's map
has (`env::DOWN_WGSL`, shared).

**Equirectangular rather than a hardware cube map**, for three specific reasons:
`env_radiance`'s direction→uv formula is reused unchanged, the mip chain is the
one that already exists, and there are no cube-face seams to show up as a cross
on a mirror. The pole stretch an equirect map has instead is the one artefact no
reflection has ever been troubled by. The conversion's `face_of` is written as
the exact inverse of `floptle_gi::Face::texel_dir`, so the bake's cube
orientations and the probe's are one table rather than two that drift — and
`reflect.rs`'s unit tests check the round trip **without a GPU**, because a face
table off by one flip does not crash or look broken, it puts the wall on the left
onto the surfaces on the right.

**Parallax is the whole difference between this and a second sky.** An
environment map sampled by direction alone is a picture at infinity: it slides
with the camera and a reflected wall never lands on the wall. The reflected ray
is intersected with the probe's box first, and the sample direction taken from
the *probe* to that intersection — exact when the room really is a box, and
gracefully approximate when it is not, which is why the box is authored rather
than derived. The same box is the probe's region of influence, so one rectangle
says both "this is the room" and "these are its surfaces". Weights are 1 inside
the box and fade over `fade` metres **outside** it: a surface against a wall is
the one that most needs the room's reflection, so a weight tailing off toward the
wall would drop the probe exactly where it matters.

Up to `MAX_PROBES` (4) blend at once, normalised, with the leftover weight going
to the sky — so walking out of a doorway crosses back to the sky smoothly. More
probes than slots keeps the ones **nearest the camera**: dropping the one you are
standing in because it was added last would be indefensible.

**Nothing is written to disk.** A GI bake is minutes of work and hundreds of
kilobytes, so it earns a file; six renders at 256² is a fraction of a frame, which
makes a stored artefact all cost and no benefit — and a stored one has a failure
mode a live one cannot have: a capture that no longer matches the room, in a file,
with nothing to say so. Captures happen on load and whenever a probe is moved or
resized, **one probe per frame**, and the ⟳ button covers the changes a probe
cannot see (the room relit, the furniture moved).

Two things worth knowing if you touch it:

- A capture must not contain its **own** reflections, or each one folds the last
  one in and a room's reflections compound frame after frame — the same trap
  glass avoids by drawing into a capture that excludes it. `capturing_probes`
  zeroes the probe count for the six face renders.
- The targets are allocated in `Gpu::scene_format()`, **not** `config.format`.
  Windowed rendering runs in HDR while the surface is 8-bit sRGB, so asking the
  surface produces a raster pipeline that cannot be set and a validation error on
  the first capture.

Verified by `interior_reflection_probe`: a sealed box with a red wall, a blue
wall and a mirror ball, under a **green** sky that appears nowhere in the room.
Without a probe the ball is entirely green — the bug, reproduced. With one, the
left of the ball reads blue and the right reads red, which is the check that
catches a face table rotated or mirrored by one face. That the no-probe shot
reads green in the same windows is also what proves those windows are on the ball
and not on a wall.

### Glass: seeing through, bent

`Material::transmission` is how much light passes THROUGH a surface instead of
stopping at it, and it is a different thing from `alpha`. Alpha fades a surface
toward what is behind it — the surface gets weaker, and takes its highlight, its
reflection and its bright grazing edge with it. Transmission keeps the surface at
full strength and lets the scene behind arrive *refracted*: bent by `ior`,
blurred by `roughness` (the same mip chain reflections use), and tinted by the
material's own `color`. The diffuse term fades out as it rises, because light
that passes through a surface is not also scattering off it.

**Glass gets its own pass, and that is the whole design.** A surface cannot
sample a picture it is already in. Drawn with the rest of the scene, the only
picture available is the previous frame's — which has the glass composited into
it, so a green bottle would deepen its own tint every frame it stayed on screen.
So the renderer:

1. runs the opaque depth prepass **without** glass (glass in it would kill the
   fragments behind it by early-z, leaving a glass-shaped hole in the capture);
2. draws the scene **without** glass;
3. captures that into the scene-colour texture;
4. draws glass, with the capture bound as "what is behind" and the reprojection
   matrix set to the identity, because the picture is this frame's.

None of it happens unless something in view actually asks: `Raster::any_transmissive`
gates the capture and the extra pass, so a scene with no glass runs exactly the
passes it always did.

**The bend travels to the scene, not just through the material.** Refraction
displaces what you see in proportion to how far the ray travels after bending, so
the common approximation — march the material's own thickness and stop — gives a
marble on a table and a ball held against a distant wall the same negligible
shift. `refracted` in `raster.wgsl` reads the depth prepass for the distance to
whatever is behind and marches that as well, which is what makes a solid ball
behave as the lens it is and turn the background upside down. Glass being absent
from the prepass is what makes that reading the scene behind rather than the
glass itself.

#### Glass behind glass (v0.53.0)

One capture gives exactly one correct layer — the nearest. Anything behind it was
never in a picture anybody took, so a fish tank's back wall stopped existing the
moment you looked through its front one.

`Light::refraction_layers` (1..=4, default 2) is how many depths of glass a frame
resolves. Above 1 the glass is drawn **far to near** with the scene re-captured
between groups, so every pane samples a picture holding the panes behind it and
none of the panes in front — the same rule the whole pass exists for, applied one
level down. Each extra layer is one more capture and one more pass, and only
while something see-through is in view.

**Where the cuts go is the biggest gaps in the sorted depths**, not equal-sized
groups. `Raster::transmissive_cuts` sorts the glass instances by distance and
splits at the `layers - 1` largest jumps, which puts a tank's front and back
panes on opposite sides of a cut and leaves a row of bottles standing together in
one group, where their order does not matter anyway. Depth comes free from
ADR-0015: the view matrix has no translation, so an instance's model translation
already IS its position relative to the camera.

Two pieces of glass in the *same* group still show only the nearer one, so 4 is a
ceiling rather than a promise — past it a scene wants a renderer that sorts per
fragment, which is a different technique rather than a bigger number.

Verified by `glass_layers_probe`: a white card, a green pane, and a clear pane
over half of it. At two layers the overlap reads green (2.41 green share); at one
it reads white (1.00), because the pane behind vanished. Both controls — the
green pane alone and the bare card — read identically in each, so the difference
is the layering and nothing else.

The remaining limits are the ones screen space always has: what is off-screen or
hidden cannot be refracted (the sample falls back to the unbent view rather than
smearing an edge pixel around the rim).

Verified by `refraction_probe`, which puts a ball in front of two hard-edged
colour cards and checks that the view through it MOVES, that it stops moving at
`ior = 1`, that the ball is opaque at `transmission = 0`, and — the one that
catches the compounding bug — that two identical frames come out byte-identical.

### The retro flags

Four era-accurate artefacts, all off by default, each independent
(`floptle_core::Retro`):

- **`jitter`** — snap vertices to a screen grid of N steps, the way hardware
  with no fractional vertex coordinates did. Applied in NDC and scaled back by
  `w`; a vertex at the eye plane is left alone, because dividing there sends it
  to infinity and shows up as a triangle stretched over the whole screen.
- **`affine_uv`** — interpolate UVs without the perspective divide. Both UV
  varyings are always emitted and `surface_uv` picks between them per material,
  rather than compiling a shader variant, because the choice is per-instance and
  instances batch.
- **`vertex_lit`** — Gouraud. Computed in `vs` from the group(0) globals only,
  because group(2)'s field is fragment-visible: a vertex-lit surface receives no
  SDF shadow, no AO and no normal map. Hardware that shaded per vertex had none
  of those.
- **`dither_alpha`** — screen-door transparency. Stays in the **opaque** pass
  (`is_opaque` accounts for it), so it needs no sorting.

Two things to know about `jitter` in particular, because both read as bugs:

- It is a **snap, not an oscillation**. It runs in the vertex shader every
  frame, but a still surface under a still camera lands in the same cell every
  time and holds perfectly still. The wobble is what motion looks like through
  the grid. `retro_fog_probe` measures exactly this — the same pan produces
  fewer distinct frames through the grid than without it.
- Each **vertex** snaps on its own. A quad does not translate rigidly; its
  corners cross cell boundaries on different frames, which is where the era's
  characteristic warping comes from rather than a clean stepped slide.

### Project-wide, and the opt-out (v0.51)

The same four live on `ProjectConfigDoc` (`retro_jitter`, `retro_affine_uv`,
`retro_vertex_lit`, `retro_dither_alpha`) for a game whose whole look is of
that era — otherwise it has to be set on every material it owns and on every
material it imports next week. All default off, so an existing project loads to
exactly the look it has.

`Retro::under` is the one place the precedence rule is written: a material's own
jitter wins, `0` means "follow the project", the three switches are ORs, and
`exempt` takes nothing at all. It is folded in at `Raster::push_surface_extras`.

The fold **moves the neutral entry**: index 0 stops meaning "no artefacts" and
starts meaning "the project's artefacts, nothing of its own". That is what makes
it reach the draws that name no material — terrain chunks, tilemaps, map
geometry, an untinted primitive — without a gather having to remember to apply
it. Those all carry index 0 and always have.

It reaches raster surfaces only. SDF matter and terrain are raymarched and have
no vertices to snap.

The project-level jitter is offered as **named strengths derived from the
project's own `retro_height`** (`retro_jitter_presets`), not as a bare number.
The number counts grid steps, so bigger is subtler — the opposite of what a
strength slider implies — and the value that reads as authentic depends on how
many pixels the project renders, not on taste: hardware with no fractional
vertex coordinates snapped to ITS pixels. `retro_jitter_pixels` is
`retro_height / 2` (steps are counted across NDC, which spans 2), keyed on the
height because the width often follows the window and a look that changed on
resize would be the same problem somewhere else. Nothing finer than pixel-exact
is offered: a grid finer than the pixels it is drawn on snaps vertices to
positions the frame cannot show.

### Fog, per surface (v0.51)

`Material::fog` (default `true`) says whether the scene's fog reaches this
surface — both the distance ramp and the marched volumetric layer. It rides the
extras store as `EXT_NO_FOG`, stored **inverted** so the neutral entry's
all-zero flags still mean "fogged".

`surface_fog` in `raster.wgsl` is the single call site, used by all three
shading returns (unlit, vertex-lit, full). An opt-out that only held on one of
them would be worse than none: it would work in the frame somebody tested and
come back when the material was lit differently.

Aerial perspective from a `CelestialBody`'s atmosphere is deliberately still
applied — a separate effect with its own controls, and a planet seen from orbit
should still haze.

### Where the extra properties live

The instance stream is full at 16/16 attributes, so the PBR scalars and the
retro flags ride a **surface-extras storage buffer** on group(0) — the third
store on the `vpaint` pattern, indexed by `normal_mat[1].w >> 1`. Entry 0 is a
reserved neutral, so an instance that sets none of this reads it and shades as
before.

An indexed store and not a uniform on group(1), for a correctness reason rather
than a tidiness one: two untextured nodes of the same mesh share one group(1)
bind, so a roughness living there would give both of them whichever was bound
last. It also ends the attribute famine for good — every material property
invented from here on lands in this buffer behind one index, instead of being
bit-packed into a lane meant for something else.

## 7. Out of scope

We are lightweight — **not a PBR authoring suite, not Substance.**

- **Layered PBR authoring** — clearcoat, sheen, anisotropy, transmission,
  subsurface, material stacks. §6b ships the metal-rough base (roughness,
  metallic, normal, occlusion), which is the layer everything else is a
  refinement of; the refinements are not planned. We import glTF PBR as a *seed*
  ([`./asset-pipeline.md`](./asset-pipeline.md)) and expose the knobs a shader
  chooses to — no film-grade material model.
- **Substance-style procedural texture graphs.** Procedural *looks* are the
  shader IR's job ([`./shaders.md`](./shaders.md)) — noise/warp/color nodes make
  generated surfaces; we don't bake a separate node-based texture authoring tool.
- **Per-texel painting / texture baking** in-editor — that's Blender's job.

If a material feature serves photoreal correctness over fast iteration, it
doesn't belong here.
