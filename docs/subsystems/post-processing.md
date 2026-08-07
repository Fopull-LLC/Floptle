# Floptle — Post-Processing (the per-scene PostProcess node)

Post-processing is tuned **per scene, not per project**: every scene carries a
mandatory **✨ Post Processing** node (self-healed on load, exactly like the
Skybox and the default GravityVolume), so a dreamy overworld and a harsh
interior can each own their look. The node's `enabled` box gates the whole
chain; each effect then has its own switch and knobs.

> Implemented 2026-07-02. Render side: `crates/floptle-render/src/post.rs`
> (`PostStack`), `post.wgsl`, `ssao.wgsl`; SDF AO lives in `raymarch.wgsl`
> (`sdf_ao`). Scene side: `Matter::PostProcess` / `MatterDoc::PostProcess`.

## The node

| Setting | What it does |
|---|---|
| `enabled` | Master switch for the whole chain. |
| **Ambient occlusion** | `Off` / `Screen space` (default) / `SDF (true)` + strength, radius (m). |
| **Bloom** | threshold, intensity — bright-pass → half-res Gaussian → additive. |
| **Vignette** | strength, radius — radial corner darkening (last pass). |

- **Mandatory:** deleting it is refused (Console explains); copy/duplicate skip
  it so a scene never has two. Its *values* copy/paste through the Type header's
  ⎘/📋 like any component — that's how you carry a look between scenes.
- **Migration:** scenes saved before the node existed self-heal a default one on
  load, and the editor copies any **legacy project-wide bloom/vignette settings**
  (old `project.ron` fields) onto it once — old projects keep their tuned look.
  The legacy fields are still *read* from `project.ron` but never written again.
- Project Settings keeps only true project-wide rendering (retro pixelization,
  SDF matter toggle).

## The two ambient-occlusion modes

**Screen space (SSAO)** — the default (cheap, and it shades *everything*:
raster meshes and raymarched matter alike, since both write real depth).
A half-res pass reconstructs view-space position + normal from the depth
buffer (`ssao.wgsl`; nearer-neighbor differencing so silhouettes stay clean),
gathers a 16-tap golden-angle hemisphere with a hard range-check falloff
(no halos on far geometry), blurs, and multiplies the scene. In **retro mode**
it samples the low-res retro depth, so the AO goes chunky with the pixels —
which is the point.

**SDF ("true")** — iq's exponentially-weighted occlusion sampled from the
*real fused distance field* (volumes + blobs) along the surface normal
(`sdf_ao` in the shared `field.wgsl`). No screen-space artifacts, correct
behind-the-camera occlusion — and since 2026-07-02 **everything receives it**:
the raster pass binds the same field (see [`./shadows.md`](./shadows.md) §2),
so a mesh resting on terrain gets true contact darkening. Only SDF matter
*occludes*, though — meshes aren't in the field, so they don't self-shade or
shade each other in this mode (SSAO does). It never darkens emissive.
This is the Tier-0 AO promised in [`./light.md`](./light.md) §2.

Both modes share `ao_strength` (how dark) and `ao_radius` (reach in meters).

## Render plumbing (for the next effect you add)

The `PostStack` chain: scene renders into `input_view()`, then
**SSAO ⊗ → bloom → colour filter → vignette → out**, each a one-triangle pass
ping-ponging between full-res targets (`scene`/`ping`/`pong`). SSAO needs an
[`SsaoFrame`] (depth view + projection) — depth textures now carry
`TEXTURE_BINDING`. The split Game viewport runs its own `PostStack` so the node
applies there too; the editor gathers the node once per frame
(`post_process_uniforms`).

**Posterize is not in that chain**, and where it sits is load-bearing rather
than incidental (`floptle/0127`). It is its own pass — `Raster::quantize_palette`,
[`crate::palette`] — run by the caller after the raster and raymarch passes and
immediately *before* `light2d_pass`. Posterize quantizes the **palette**, the
set of values the art is allowed to be; a light is a multiplier on the palette
rather than a member of it. Quantizing the finished frame made them one setting,
and then no configuration was right: hard rings, a stipple, or no palette at all.

The rule that falls out, and the one to keep when you add an effect:
**quantize the palette; everything light-shaped comes after.** SSAO, bloom and
the vignette are all downstream of the quantize now, which is why a vignette is
a smooth darkening rather than rings in the corners — it was banding for exactly
the reason a light was.

`PostSettings::any()` still counts posterize even though the chain no longer
applies it. That is deliberate: it forces the scene into a post target, and the
palette pass has to be able to *read* the frame it quantizes. A swapchain
texture cannot be sampled.

Adding an effect = a `fs_*` entry in `post.wgsl` + a pipeline + a
`PostSettings` field + sliders in the node's Inspector arm. Headless probes:
`ssao_probe`, `sdf_ao_probe`, `post_probe` (bloom/vignette),
`posterize_chroma_probe` (the quantizer keeps a warm ramp's hue),
`light2d_smooth_probe` (a light adds no step the art does not already have,
dithered and undithered).

## Not yet

- Lua control of the node (`scene.post.*`) — the node is inspector-only so far.
- Bilateral (depth-aware) AO upsample; tonemap/color-grade/fog/CRT effects —
  the proof-of-concept shaders in `crates/floptle-proof/src/present.wgsl` are
  ready templates.
