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
canvas (these become real Fopull art by Phase 10, [ROADMAP](../ROADMAP.md),
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

## 7. Out of scope

We are lightweight — **not a PBR authoring suite, not Substance.**

- **Full PBR authoring** (metallic/roughness/clearcoat/sheen/anisotropy layer
  stacks). We import glTF PBR as a *seed* ([`./asset-pipeline.md`](./asset-pipeline.md))
  and expose the knobs a shader chooses to — no film-grade material model.
- **Substance-style procedural texture graphs.** Procedural *looks* are the
  shader IR's job ([`./shaders.md`](./shaders.md)) — noise/warp/color nodes make
  generated surfaces; we don't bake a separate node-based texture authoring tool.
- **Per-texel painting / texture baking** in-editor — that's Blender's job.

If a material feature serves photoreal correctness over fast iteration, it
doesn't belong here.
