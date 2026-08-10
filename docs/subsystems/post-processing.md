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

## Two formats, and one crossing (v0.43)

`Gpu::scene_format()` is **not** `Gpu::surface_format()`. Every scene-space pass
— raster, raymarch, particles, lines, the grid, debug triangles, world-space UI,
the 2D light composite, live render targets — and the whole post chain's scratch
render in the scene format, which is `Rgba16Float` whenever a window is driving
it. The display is 8-bit sRGB.

**Exactly one pass crosses between them: the terminal `fs_finish`**, which is
where the tonemap lives. That is what makes an exposure mean something and what
lets bloom tell a lit wall from a light bulb: an 8-bit target stores both as
white, and no pass downstream can ever separate them again. Clipping is also not
neutral — each channel clips on its own, so a colour whose red saturates first
slides toward yellow and then white, which is why blown highlights in an
untonemapped renderer go strange colours rather than simply going bright.

Consequences worth knowing before you touch this:

- **The chain always runs, and always ends in `fs_finish`**, even with every
  effect off. There is no "nothing is on, copy it straight to the window"
  shortcut any more: that would hand an sRGB surface a floating-point image and
  skip the one pass that knows how to land it. The fast path is a `finish` with
  identity parameters.
- **The scene always renders into `post.input_view()`** — the surface path, the
  docked Game view, and both Inspector previews. A preview owns its own
  `PostStack` for exactly this reason (`PreviewTarget::post`), which also means a
  material preview lands on screen through the same path the game does.
- **`Gpu::headless()` keeps the scene at the surface format**, so the forty
  render probes that read their target back as packed RGBA8 are untouched.
  `Gpu::headless_hdr()` is the opt-in, and `hdr_probe` is the one that uses it —
  it builds every scene pass against an HDR device (a colour target that
  disagrees with its texture is a validation error at DRAW time, in somebody's
  editor, on whichever scene contains that one pass) and asserts that a 4×-white
  emitter reaches the tonemap as 4× rather than as white.
- **The UI backdrop capture is pre-tonemap**, so anything over white clamps on
  the way into the (8-bit) backdrop texture. Frosted glass is a blur of what is
  behind it, not a measurement.

## Depth of field (v0.44)

Eight settings, and the interesting half is about what a lens actually does.

| Setting | What it does |
|---|---|
| **focus distance** | the distance from the camera that is sharp. 0 = off. |
| **follow** | a NODE to keep in focus — the focus distance becomes the camera's distance to it, every frame. |
| **far range** | how far beyond the focus distance stays sharp. |
| **near range** | how far in front does. 0 = half the far range, which is what the effect used to hardcode. |
| **max blur** | the widest the blur gets, in pixels. |
| **blades** | 0 is a round iris; 3+ gives the polygonal bokeh of a real lens. |
| **blade angle** | turns the polygon. |
| **highlight bokeh** | how much brighter-than-white pixels dominate the blur. |
| **samples** | taps in the kernel. 0 = the default 16. |
| **show the focus band** | a tuning view: cool in front of focus, warm behind. |

**Near and far are two ranges** because they are not the same thing — a lens goes
soft on the near side much sooner than on the far side, and more to the point
they are the two numbers people reach for (a portrait wants the foreground gone
and the background readable). `dof_near_range` at 0 means "half the far range",
which is the old single-number behaviour, resolved in `post.rs` so the shader
never sees the sentinel.

**Highlight bokeh only exists because the chain is scene-referred.** The weight
is `1 + boost × max(peak − 1, 0)`: it asks how far past *white* a tap is, which
is a meaningful question here and was not one before the frame went
floating-point. Without it, averaging a specular glint with its dark neighbours
turns bokeh into grey mush.

**Follow-a-node is resolved per VIEWPORT**, in `shading::dof_focus_distance`,
not inside `post_process_uniforms`. It is the one post setting that depends on
where the camera *is*, and the editor renders the same scene from several — fold
it into the settings and the Scene view shows the game camera's focus while you
fly around, which reads as the effect being broken. A name that matches nothing
falls back to the authored distance rather than to zero.

The extra knobs ride the DoF pass's own spare `b`/`c` lanes rather than growing
`PostParams` (each pass writes its own uniform, so `b` and `c` are free here) —
spelled out in `run_with` because the next person reaching for a spare lane needs
to know these are spoken for.

Probe: `dof_probe` is four control pairs — change one knob, assert the one thing
it should change. The sharp one is that with the near and far ranges *equal*, two
cards four units either side of focus blur to the same pixel count, which no
single-range implementation and no crossed pair can produce.

## Motion blur (v0.49)

Two settings on the node, and one idea.

| Setting | What it does |
|---|---|
| **shutter** | how much of the frame's camera motion is smeared, as a fraction of the step between frames. 0 = off. 0.5 is the 180° shutter a film camera has; 1 leaves it open for the whole frame. |
| **samples** | taps along the streak. 0 = the default 12. Too few and a fast pan bands into separate copies of the picture. |

**The streak is reconstructed, not rendered.** Take a pixel's depth, put it back
in the world, and ask where that same point was in the *previous* frame's
picture; the difference is how far it travelled across the screen, and smearing
along it is the blur. Two matrices reach the shader for it —
`motion_inv_view_proj` (clip → camera-relative world, this frame) and
`motion_prev_view_proj` (camera-relative world → the previous frame's clip) —
and they are per-frame camera facts rather than artist settings, which is why
they live on `PostSettings` beside `time` instead of in a second struct threaded
through every call site.

**The world is camera-relative** (ADR-0015), so the previous view-projection
cannot be used as it was taken: a point standing still in the world has different
relative coordinates in each frame. `shading::motion_frame` shifts it by how far
the camera itself moved, which is what turns "where was this pixel" into a
question about the scene rather than about the origin. Getting this wrong smears
a still camera, which is why the probe's first check is that a still camera
leaves the frame *byte-identical*.

**With no history the previous matrix IS the current one**, so every pixel
reports zero motion and the frame stays sharp. That is the right answer for the
first frame after a load, a scene switch, or a camera cut — and the only safe
one, since the alternative is one frame smeared by wherever the camera used to
be looking.

**The Game view gets it; the Scene view does not.** The Scene view is a tool, and
placing a prop while the camera is still coasting is not something to have to
fight. The Scene view already shows every other effect in the chain, so this is a
deliberate exception rather than an oversight.

**Two things it does not do**, both structural rather than bugs:

1. **Object motion.** A car crossing a locked-off shot is a point standing still
   in the world as far as this is concerned, so it stays sharp. That half needs a
   velocity buffer — a second render target written by *every* draw path in the
   engine, including skinned meshes, instanced meshes, tilemaps, sprite batches,
   particles and the raymarched field — plus the previous frame's transform and
   bone palette for each. That is a change to every draw path in the renderer,
   and it is exactly the shape of change this codebase has got wrong four times
   (see `two-gathers-must-agree`). It is a release of its own, not a corner of
   this one.
2. **Reach outside a moving surface.** This is a *gather*: each pixel collects
   along its own velocity, so a fast-moving surface softens within its own
   footprint rather than throwing light onto what is behind it. At a large depth
   step — a near railing against a far valley — the railing smears and the valley
   does not receive the smear. Fixing that means dilating velocity across screen
   tiles first, which is another two full-screen passes.

Neither shows up in the case motion blur is mostly for. A **pan** moves every
pixel by the same amount whatever its depth, so a rotating camera blurs the whole
frame evenly and correctly.

It runs directly after depth of field, for the same reason DoF runs first: both
are about the SCENE rather than the picture, and both need the frame's own depth
to still describe what is in the frame. After rather than before, because a lens
defocuses light and *then* the shutter smears what the lens produced — swap them
and you get sharp streaks through a blurred image, which reads as a fault.

The streak ceiling is in **pixels** and scales with the frame (5% of its height,
clamped to 8..96, set in `motion_frame`): the same uv length is twice the streak
on a 2160-tall picture as on a 1080-tall one, so a fixed uv cap would be a
different look at every window size.

Probe: `motion_probe` is six control pairs. Its subject is an edge between two
surfaces at the *same depth* rather than a card on black — because a gather
softens within a footprint, a smeared card comes back the same size, and a width
measurement would read "no blur" on a pass that is working perfectly.

## Screen shaders — your own passes (v0.44)

The chain above is a fixed list of effects the engine happens to ship. **Screen
shaders** are the other half: a `stage post` `.flsl` gets the finished frame and
returns a new colour for every pixel, and the PostProcess node carries an
**ordered list** of them.

```
shader inkOutline {
  stage post
  uniform thickness: float = 1 range(0.5, 4)

  let px = screenTexel() * thickness
  let bend = abs(sceneDepth(uv - vec2(px.x, 0)) + sceneDepth(uv + vec2(px.x, 0))
                 - sceneDepth() * 2) / max(sceneDepth(), 0.01)
  output color = vec4(mix(sceneColor().rgb, vec3(0), saturate(bend * 8)), 1)
}
```

Four ops, `screen` category, available **only** in `stage post`:

| Op | What it gives you |
|---|---|
| `sceneColor(uv?)` | the frame so far, in **real light** (upstream of the tonemap, so a bright light really does read above 1) |
| `sceneDepth(uv?)` | distance from the camera in world units; the sky reads `1e6`, so a silhouette is the largest step in the frame |
| `sceneNormal(uv?)` | the surface normal in **view space**, reconstructed from depth — so meshes, terrain, SDF matter and tilemaps all have one, with nothing to author |
| `screenTexel()` | one pixel, in uv. Follows the **retro** resolution, so an effect written in texels stays one pixel wide instead of being magnified by the upscale |

The optional `uv` is the whole point. Defaulting to this pixel makes a colour
grade a one-liner; passing another pixel's is what makes an edge detect, a blur
and a warp expressible at all — and no varying could offer that, which is why
these are ops and not inputs.

**Where they run:** after depth of field and the denoise, **before** the colour
grade, the lens and the grain. Each part is a decision. After DoF, because focus
belongs to the scene and a pass should see the picture the camera actually took.
After the denoise, because the denoise wants raw sampling noise. Before the
grade, because whatever a pass draws is *art* — an ink outline should be graded
and vignetted like everything else in the frame, not stencilled onto the finished
picture — and it keeps the pass upstream of the lens distortion, so the depth it
reads still lines up with the pixels it is reading.

**Plumbing.** `PostShaders` (post.rs) owns the pipelines and the ordered slot
list; `PostStack::run_with` executes it. The registry is deliberately *not* owned
by a `PostStack`: a running editor holds several chains (the surface, the docked
Game view, each Inspector preview) and a scene's screen shaders belong to the
scene, not to one of its viewports.

Three bind groups, all built from `post::layouts` so an authored pass and a
built-in one cannot drift: group(0) is the chain's own `{ frame, sampler, params }`
(which is why a custom pass ping-pongs between the chain's scratch targets with
no bind group of its own), group(1) is the depth buffer plus the inverse
projection, group(2) is the shader's uniforms. The WGSL contract lives in
`floptle-shader/src/post_prelude.wgsl` — and unlike `TEST_PRELUDE` it is the
**real** module text, not a mirror, so there is nothing to keep in step.

Without a frame's depth (a 2D project, a viewport that renders none) the pass
gets a 1×1 texture cleared to far: `sceneDepth` reads sky everywhere and an
outline quietly finds no edges, rather than reading uninitialised memory.

Alpha is forced to 1 on the way out. A post pass *replaces* the pixel — it writes
into the chain's next scratch target — so a shader that forgot about alpha would
otherwise hand the rest of the chain a transparent frame that every later pass
reads as black.

Two are shipped in `shaders/examples/`: **`inkOutline.flsl`** (the comic-book
look — depth *bend* plus a normal fold) and **`crtScanlines.flsl`** (twelve lines,
the one to read first). From Lua:
`node:setScreenShader("inkOutline", false)` and
`node:setShaderParam("inkOutline.thickness", 2)`.

Probe: `post_shader_probe` renders the same scene with and without the outline
and measures three regions worked out from the control image — full coverage on
the silhouettes, and **zero** ink on a curved sphere's interior or on a plane
raked steeply away from the camera. That last one is what the second-derivative
measure (`dl + dr - 2*d0`, zero on any plane at any angle) buys; a detector
thresholding a plain depth difference inks the floor of every scene.

## Render plumbing (for the next effect you add)

The `PostStack` chain: scene renders into `input_view()`, then
**SSAO ⊗ → bloom → DoF → denoise → screen shaders → grade → lens → sharpen →
finish(tonemap, colour filter, vignette, grain)**, each a one-triangle pass ping-ponging between
full-res targets (`scene`/`ping`/`pong`). SSAO needs an [`SsaoFrame`] (depth view
+ projection) — depth textures now carry `TEXTURE_BINDING`. The split Game
viewport runs its own `PostStack` so the node applies there too; the editor
gathers the node once per frame (`post_process_uniforms`).

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

- A screen shader cannot declare texture slots yet: the frame, its depth and its
  normals are bound, and nothing else. A ramp LUT is the obvious next one.
- Bilateral (depth-aware) AO upsample. (Fog and CRT effects are no longer a
  gap: they are screen shaders — see the section above, and `crtScanlines.flsl`.)
- **Auto-exposure.** The pipeline can carry it now (the scene is scene-referred
  all the way to the tonemap, which is the hard part), but a light meter that
  moves on its own is a decision about a scene, not a default, and it wants an
  artist-facing shape before it wants an implementation.
- **Per-object motion blur** — the camera half shipped in v0.49; the object half
  needs a velocity buffer written by every draw path (see the section above).
