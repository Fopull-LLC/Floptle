# Global illumination — the bounce

*Status: shipped in v0.45.0 (baked irradiance volumes). Per-surface lightmaps and
a real-time SDFGI mode are both still ahead; the render graph is shaped so they
land as additional sources of the same term.*

Direct light tells a surface about the sun. Everything a real room actually looks
like past that is light that already bounced — off a red wall, off a bright floor
— and no amount of material work invents it. Before this, the engine's answer was
a single flat ambient colour, which lifts the inside of a sealed box exactly as
much as it lifts an open field.

A **Light Probes** node is a box. Baking renders the scene from a lattice of
points inside it and keeps, at each one, the light arriving from every direction.
Inside the box that replaces the flat ambient; outside, the flat ambient carries
on exactly as before. A scene with no volume renders precisely the frames it
rendered in v0.44.

---

## The one design decision

**A probe is a camera.** Each one renders the scene six times, once per cube
face, through the same `render_world_into` the Game view uses.

The obvious alternative is to trace rays against the SDF field, which the engine
already marches for shadows and ambient occlusion. It was rejected for two
reasons. The field holds only what has been voxelised into it, and it holds
*distance*, not colour — and bouncing grey light off a red wall is most of the
way to no bounce at all. Rendering the scene instead means everything that can be
seen contributes what it actually looks like: meshes with their textures, terrain
with its splats, tilemaps, sculpted matter, custom `.flsl` materials, emissive
surfaces, and the sky.

It also means there is no second gather to keep in step with the first. This
codebase has drifted a duplicated scene walk four separate times; a bake that
renders through the existing path cannot drift from it.

The cost is that a bake is thousands of real frames, so it runs a slice at a time
across the editor's own frames. The window stays live, the progress bar moves,
and Cancel works.

## What a probe stores

Spherical harmonics, first two bands: a constant plus one direction, per colour
channel. Twelve floats.

Band 1 is what makes the bounce *directional* — "the light here comes mostly from
over there, and it is warm". Band 2 would cost nine coefficients per channel for
a sharpening a grid this coarse cannot honestly claim to know, and it rings
negative around bright sources, which in an ambient term reads as a black smear.

Alongside the light, each probe records the **closest surface it can see**. That
one number is what tells a probe floating in a room apart from one buried inside
a wall, and it is the whole basis of the leak test.

## Sampling

Eight surrounding probes, trilinear, times two weights:

- **Facing.** `(dot(n, toProbe) · 0.5 + 0.5)²`. A probe behind the surface being
  shaded cannot be lighting it. Squared and softened rather than cut off, so a
  wall sliding past a probe plane does not pop.
- **Validity.** A probe with no clearance around it is inside geometry and
  contributes nothing. The threshold is the volume's `leak` setting, in multiples
  of the probe spacing; 0 turns the test off.

Plus a **surface offset**: the shading point steps along its own normal by a
fraction of a cell before looking anything up. A shading point sits exactly *on*
the geometry, which is the one place where "which side of this wall am I on" is
genuinely ambiguous.

Together these are what stop the lit room next door glowing faintly through the
wall — the artefact everybody recognises from an irradiance volume.

## Where the code is

| Piece | Where |
| --- | --- |
| The arithmetic — SH, the grid, cube-face integration, the leak weighting, the `.fgi` file | `crates/floptle-gi/src/lib.rs` (pure CPU, unit-tested) |
| The bake — probe cameras, chunking, readback, multi-bounce | `crates/floptle-editor/src/gi_bake.rs` |
| The probe texture + the four uniform lanes | `crates/floptle-render/src/gi.rs` |
| The sampler every surface shades through | `gi_bounce` / `gi_ambient` in `crates/floptle-render/src/field.wgsl` |
| The node | `Matter::LightProbes` (`floptle-core`), `MatterDoc::LightProbes` (`floptle-scene`) |

`gi_bounce` is a hand transliteration of `BakedGi::sample`. The Rust side has the
tests that say what "does not leak" means; `examples/gi_probe.rs` renders the same
situation through the shader and compares the pixel to what the Rust says, which
is the only thing that keeps the two honest.

## Storage

Four `Rgba32Float` texels per probe, side by side along x in one 3D texture. One
binding instead of four, and no filtering is given up: the eight-probe blend
applies its own weights, and hardware trilinear cannot apply a leak test.

The bake itself lives in a `.fgi` beside the scene — a build artefact of a few
hundred kilobytes, and a `.ron` is a thing people read and merge. It is keyed off
the scene's real relative path, not its stem, so two scenes named `main.ron` in
different folders do not overwrite each other.

## Multi-bounce

The same bake, run again, with the answer from last time turned on. Bounce 1
gathers direct light coming off surfaces once; each further bounce re-renders
every probe with the previous result applied, and costs the same again.

## Knobs, and which need a re-bake

| | Effect | Re-bake? |
| --- | --- | --- |
| size, spacing | the lattice | yes |
| bounces, bake detail, skip layers | how it is gathered | yes |
| intensity | how much of it to apply | **no** — an upload |
| leak rejection | how hard to reject buried probes | **no** |
| surface offset | the normal step before the lookup | **no** |
| enabled | master switch | **no** |

The last four are resolved into the probe texels as they are uploaded, so they
cost a shading point nothing per pixel and change the picture the moment you drag
the slider.

## Looking at it

- **Show only the bounce** switches every direct light off, so what is left on
  screen is exactly what was baked. Implemented in one place — `key_light` in
  `field.wgsl` — which is why meshes, terrain, blobs, field shapes and `.flsl`
  materials all go dark together.
- **Show the probes** draws each one in the colour it baked, and draws the ones
  the leak test has rejected hollow. A grid too coarse for the room, a volume
  nudged half out of the level, and a row of probes buried in the floor are all
  invisible in the final picture and obvious here.

## From a script

```lua
local gi = find("GI"):getcomponent("LightProbes")
gi.intensity = 0                 -- the flashback: no bounce at all
gi.enabled   = true
```

Only the live knobs are exposed. `bounces`, `bake detail` and `spacing` describe
how to bake, and a script cannot start a bake, so offering them would be offering
a lie.

## Not yet

- **Per-surface lightmaps.** A grid cannot represent a shadow sharper than its
  own spacing. Contact detail is what SSAO and contact shadows are for; a real
  lightmap needs every surface unwrapped into a shared atlas, which is a pipeline
  of its own.
- **More than one volume per scene.** The second is ignored, deliberately and
  quietly, rather than fighting the first over the same uniform slots.
- **Real-time GI.** The field is most of an SDFGI data structure already, and the
  sampler above is the seam it would plug into — `gi_ambient` is the only thing
  the rest of the renderer knows about.
