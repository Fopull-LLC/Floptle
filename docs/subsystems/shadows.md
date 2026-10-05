# Floptle — Shadows (field-marched sun shadows)

**Status: IMPLEMENTED · 2026-07-02** — field-first SDF sun shadows, per scene,
on everything: terrain/blobs cast from the field itself, raster meshes *receive*
by marching the same field and *cast* two ways — static level meshes as baked
shadow-only occluder volumes (true silhouettes, dark interiors), dynamic bodies
as collider-shape proxies. The style range spans razor-hard PS1 to dreamy-soft
modern from one dial set on the Lighting node. (Deferred: point-light shadows,
bent shadow rays — see §6.)

> Reads-with: [`./renderer.md`](./renderer.md) §3 (the march that carries the
> shadow ray), [`./light.md`](./light.md) (this is Tier 0's "SDF soft shadows";
> Tier 2 later *bends* the same ray), [`./post-processing.md`](./post-processing.md)
> (SDF AO — the sibling effect, same shared field module).

## 1. Why field-first (the design call)

**Shadow mapping** (render depth from the light, compare) is what general
engines ship because they must serve arbitrary triangle soups: hard 1-tap →
PCF → PCSS for softness, plus CSM cascades for big worlds — 2–4 extra scene
renders per frame and a permanent seam/shimmer maintenance tax.

Floptle's renderer already marches **one fused SDF field** (terrain volumes +
blobs), so the field *is* the shadow system: march from each shaded point
toward the sun tracking iq's `min(k·d/t)` and you get **analytically soft
shadows** — no shadow maps, no cascades, no resolution, no shimmer, and
large-world-safe for free (the field is camera-relative, ADR-0015). All
cross-shadowing (hills into valleys, blobs onto terrain, terrain onto meshes)
falls out of the one `map_d()`. This also keeps light.md's Tier 2 *bent shadow
rays* reachable — the shadow ray is already a field march; a shadow-map
pipeline would have to be thrown away to get there.

**What we gave up** (still true): pixel-exact silhouettes of complex *dynamic*
meshes — a windmill's blades shadow as their collider box, not as blades.
(Static level meshes are NOT in this bucket: they bake real occluder volumes,
§2.) If a game someday needs a hero-caster silhouette, a single non-cascaded
shadow map can be folded into the same visibility term *then*.

## 2. How it works

Everything lives in **`crates/floptle-render/src/field.wgsl`** — the shared
distance-field module concatenated onto *both* passes' shaders (WGSL
module-scope declarations are order-independent):

- `light_vis(p, n, l)` marches the fused field **plus the proxy occluders**
  from the surface point toward the sun, tracking `vis = min(vis, k·d/t)` —
  the single `k` sweeps hard (≈64) → soft (≈2). Acne control: the ray starts
  lifted off the surface by ~1.6 voxels (scaled up when the sun grazes the
  surface, or noisy walls stripe), and the penumbra term only accumulates once
  the ray clears the start surface's own noise floor (hard hits count from the
  first step).
- `sun_shadow(p, n, pix)` wraps it in the style pipeline: optional quantize
  into N bands (+ optional Bayer 4×4 dither between bands at pixel `pix`),
  then the result multiplies the sun toward
  `mix(vec3(1), tint, strength·(1−vis))`.
- The shadow term multiplies the **directional diffuse + specular only** —
  ambient and point lights are the unshadowed fill (so full shadow is never
  pitch black), emissive is untouched, and `unlit` matter ignores shadows
  entirely. Both shading paths (raymarch terrain/blob branches, raster mesh
  fragments) apply it identically.

**Meshes receive:** the raster pipeline binds the raymarch pass's own globals
buffer + distance atlas at group(2) (`Raymarch::field_bind`), so each mesh
fragment marches the very field the raymarch pass draws. The raymarch pass
draws (or on frames with nothing to raymarch, `upload_globals`-es) first, so
the buffer always holds the frame's data. Standalone raster callers (asset
previews, probes) pass no field and get a zeroed fallback — every field branch
skips, zero cost.

**Meshes cast — two paths, picked by what the node already authors:**

1. **Static level meshes → baked occluder volumes.** A `Matter::Mesh` node with
   a `Collidable`/`MeshCollider` (and no RigidBody) is baked once by
   `floptle_field::bake_occluder` — a fast unsigned distance field (surface
   voxelization + chamfer transform, milliseconds-to-subsecond even for whole
   maps; logged to the Console) — and uploaded into the same 3D atlas as the
   terrains, flagged **shadow-only** (`vol_center.w = 2`). The shadow march
   folds it in; the drawn field, AO, collision and the selection mask all skip
   it. The mesh therefore casts with its **true silhouette**: building
   interiors go dark under their own roofs, and the map shadows the terrain
   around it. Bakes are cached by (asset, rotation, scale) — *moving* a map
   never rebakes (the volume anchors on the node's f64 translation per frame);
   re-orienting or rescaling one rebakes once.

   **Copies share a bake.** Seven copies of one building at the same rotation
   and scale are one bake in the atlas, and each copy draws from it at its own
   place. The atlas holds up to 64 distinct bakes; a 128-voxel bake costs about
   12 MB of VRAM (6 bytes a voxel).

   **The nearest ones cast.** The shader folds in up to 32 volumes at once
   (`shadow_volumes` in `project.ron` lowers that, for a game short on GPU:
   each one adds a little to every shadow ray that passes near it). Terrains
   take theirs first. The rest go to the collidable meshes nearest the camera
   that frame, so the shadows follow the player around a big level. A mesh
   whose **Casts shadows** is off is never baked and takes none of the budget,
   so switching it off on small props spends the budget on the buildings.

   **A turn or resize is baked once it settles.** A bake takes around 100 ms,
   so a node a script turns or resizes every frame keeps its last bake (it
   still follows the node's position) and is baked again once it has held one
   shape for 20 frames. A shape it already has a bake for is taken at once.

   **Out of video memory, the atlas shrinks.** When the GPU refuses the atlas
   (another program holding most of its memory), bakes are dropped from the end
   until one fits, down to none. Whatever is dropped casts no distance-field
   shadow or AO. The world still draws and the sun shadow map still works. The
   Console says so once, with the size that was refused.
2. **Dynamic meshes → proxy occluders.** The editor harvests up to 32 cheap
   analytic stand-ins per frame (`collect_shadow_proxies`): a `RigidBody`
   casts its body shape (sphere / capsule / oriented box), and a static
   `Collidable` *primitive* casts the shape the physics build gives it
   (Cube → 0.7·scale box, Sphere → 0.85·max-scale, Capsule → 0.5-sized).
   A capsule character casting a soft capsule shadow *is* the retro
   blob-shadow look. A proxy containing the ray start is skipped, so a mesh
   never blanket-shadows itself from inside its own capsule.

**How the march ENDS is half the answer**, and the two endings mean opposite
things. A ray that runs out of steps mid-span spent its whole life hugging
matter, so it is occluded rather than lit-by-default; a ray that breaks out
because the field went empty has nothing in the way and is lit. Both leave the
march short of the last relevant bound, so *how far it got* cannot tell them
apart — `field_vis` records **why** it stopped instead. Reading the distance
alone painted scanlines of full shadow across open ground in a scene with no
terrain, no blobs and no baked volumes, which is to say across the floor of an
ordinary game scene (fixed in v0.62.1). The same case has a second exit: when
the only occluder the relevance sweep found is the fragment's *own* proxy, the
march is skipped entirely and the ray is lit, because there is nothing left in
it to march.

Blobs/terrain never need either — they're in the field itself. Both paths are
folded into the shadow march only (never the drawn surface or AO), hidden
(`Visible(false)`) nodes don't cast, and every collider node has a
**casts shadows** checkbox in the Inspector (`CastShadow(false)` opt-out —
casting is the default, per-node opt-out serializes only when off; toggles
apply instantly, no rebake).

## 3. The knobs (Lighting node, per scene)

| Inspector | `Light` field | Range / meaning | Style it unlocks |
|---|---|---|---|
| sun shadows | `shadows` | on/off (default **on**) | — |
| softness | `shadow_softness` | 0 hard … 1 soft (log-maps to `k` 64…2) | PS1-hard ↔ modern-soft |
| strength | `shadow_strength` | 0..1 — how dark full shadow gets (default 1) | airy ↔ deep |
| tint | `shadow_tint` | RGB — shadows darken *toward this color* | purple dusk, sepia, horror green |
| quantize | `shadow_quantize` | smooth / 2–4 bands | posterized toon/retro penumbra |
| dither | `shadow_dither` | Bayer-pattern the quantized penumbra | the PS1 edge; pairs with retro mode |
| distance | `shadow_distance` | max march distance (perf fence) | open-world haze |
| steps | `shadow_steps` | most steps a ray takes, 8–64 (default 64) | a low quality setting that keeps shadows |

Serialized in `SceneDoc.lighting` (`LightDoc`, serde defaults — pre-shadow
scenes load with the defaults above and just start casting).

**Recipes** — same shader, different uniforms:
- **PS1:** softness 0.2 + quantize 2 + dither on + retro 240p project mode.
- **N64 blob:** softness 0.9 — proxies read as soft blobs under characters.
- **Modern cozy:** softness 0.7, strength 0.6, warm tint.
- **Toon:** softness 0.5 + quantize 3, no dither.

## 4. Render plumbing (for whoever touches it next)

- Uniforms ride `RaymarchGlobals` (appended at the end, layout-compatible):
  `shadow_params` [on, k, strength, max-dist], `shadow_tint` [rgb, quantize],
  `shadow_extra` [dither], `prox_count` / `prox_a` / `prox_b` / `prox_rot`
  (see `MAX_SHADOW_PROXIES`). The editor gathers them in `shadow_uniforms` +
  `collect_shadow_proxies` at every render site (surface, camera preview,
  split Game viewport).
- `field.wgsl` also owns the `Globals` struct and all distance-only field
  machinery (`map_d`, blob/volume distances, `field_eps`, SDF AO) — the
  raymarch pass keeps only the color-carrying surface path, and its hot march
  loop samples `map_d` (one color fetch per ray, at the hit).
- Volume slots carry a role flag (`vol_center.w`: 0 absent, 1 render,
  2 shadow-only). The editor bakes/caches occluders in
  `refresh_mesh_occluders`, uploads each distinct bake once AFTER the
  terrains in the same `set_volumes` atlas, and places instances per frame in
  `fill_terrain_volumes`, nearest the camera first. An instance names the
  atlas region it samples with `vol_atlas[i].w = region + 1`; zero means its
  own slot in upload order, which is what every other caller relies on.
- Probes: `shadow_probe` renders the whole matrix (off / soft / hard / retro /
  tint / full-with-AO) over a hill + shadowed cube (receive) + capsule (proxy
  cast) + blob + an invisible occluder slab with a cube "indoors" beneath it
  (the level-mesh path); `terrain_far_probe` stays bit-identical with shadows
  off. **`mesh_only_shadow_probe` is the one with NO field at all** — a plane
  and a character, both casting through proxies — because every other probe
  here builds a terrain first, and a scene whose field is empty took a
  different path through the march that nothing rendered. It asserts in both
  directions: open ground is evenly lit **row by row** (a whole-region average
  would pass a striped floor), and the caster is still casting (or deleting the
  shadow pass would pass the first check perfectly).

## 5. Performance posture

Decision D (full-res first, measure, then optimize — renderer.md §6): the
march runs per shaded fragment, at most `shadow_steps` steps (default 64), and
is gated hard — it never runs when shadows are off, on sun-averted fragments
(`n·l ≤ 0`), on unlit matter, or past `shadow_distance`; empty scenes break out
after one sample. At retro internal resolutions the cost is trivial.

The 32 proxy slots go to the casters nearest the camera, skipping switched-off
bodies and bodies whose meshes are all hidden (a body with no mesh under it at
all is a stand-in and still casts). They used to go to the first 32 in scene
order. A game drops a body from the march with `node.castShadow = false`.

Measured 2026-09-29 on a mesh level at 1080p (RTX 4060), the player in a
trench with enemies awake: shadows are 4.7 ms of a 7.3 ms opaque + lighting
pass at 64 steps and 4.1 ms at 20 — most rays end at their last relevant bound
long before the cap, so the step budget trims rather than halves. The enemy
proxies were 0.2 ms of it there. The next lever is still the SSAO-style
half-res + depth-aware upsample path.

**Per step, only the proxies the ray is near.** The march walks the relevance
mask's set bits (`firstTrailingBit`) instead of all 32 slots. Under a lattice
of 32 box colliders at 720p that took opaque + lighting from 2.3 ms to
0.95 ms, with the picture bit-identical; 2 casters cost 0.25 ms before and
after. Two things that did not help, kept here so nobody tries them again:
holding each proxy's entry and exit along the ray in two 32-entry arrays was
slower everywhere (private arrays in a shader this size cost more than the
tests they skip), and skipping a proxy whose bounding sphere is farther than
the current distance saved 4%. `perf.counts()` reports `shadowProxies`,
`shadowProxiesDropped` and `shadowVolumes`.

## 5b. Contact shadows (v0.48.0)

The field march knows about terrain, blobs, baked level meshes and collider
**proxies** — and a proxy is a box or a capsule, so a moving mesh casts a box's
shadow. The place that reads worst is the contact between a foot and the floor,
which is exactly where a capsule is least like a character.

Contact shadows close that from the other end: a short **screen-space** trace
along the light ray, tested against the opaque depth prepass the renderer already
produces. Anything on screen occludes with its true silhouette — skinned,
morphing, tilemapped, whatever it is made of — with no proxy, no bake, and no
second gather of the scene.

```
        marched field shadow  ──────────────────────────────►  long range, true
                                                               shapes for anything
                                                               IN the field
        contact trace  ──►  short range, true shapes for
                            anything ON SCREEN
```

The two combine with `min` **before** the styling, so a contact shadow takes the
same tint, strength and posterize the marched one does — two shadow terms with
two different looks would read as two shadows.

**What it cannot do**, and these are the shape of the technique rather than
bugs:

- shadow from something off the edge of the frame, or hidden behind something
  else — there is no depth for what was never drawn
- reach far. Turning the reach up widens the "is this the same object" window
  (see below), which is what starts smearing distant geometry over things in
  front of it

**The one judgement call.** Depth alone cannot distinguish *"I am inside a thick
pillar"* from *"I am in front of a wall on the far side of the room"*. The
tolerance is tied to the reach, which is short by design: a trace that only looks
35 cm ahead can afford to believe that anything within 35 cm behind what it
crossed is the same object — and that is what lets a solid pillar cast rather
than being written off as scenery. Tie the start bias to the reach instead and
the knob stops being monotonic: a longer trace lifts its own start over the thing
it was meant to find, and finds **less**.

Knobs on the Lighting node (`contact shadows`, `reach`, `strength`, `steps`), and
in Lua as `contactShadows`, `contactLength`, `contactStrength`, `contactSteps`.
Off by default — it costs a trace per lit fragment, and a scene that never asked
should not start paying.

Verified by `contact_shadow_probe`, whose caster deliberately has **no proxy and
no volume**: the field has nothing to say about it, so anything that appears
under it came from the trace. Control pairs on/off, strength 0 back to the
control frame, and a longer reach giving a longer shadow — not merely a darker
one.

## 5c. Local shadows — one lamp at a time (v0.52.0)

Everything above is about the SUN. Every placeable light in the engine was
unshadowed fill: a torch in a doorway lit the room behind the door exactly as
brightly as the one it was standing in, which is the most conspicuous way local
lighting can be wrong.

`point_vis` in `field.wgsl` is a screen-space trace, built on the same depth
prepass contact shadows read — **not** the field march the sun uses, and for a
concrete reason. The sun is one light, infinitely far away, bounded by a distance
the scene sets. A lamp is one of sixteen, sits inside the level, and the thing
that has to block it is almost always ordinary polygon geometry: a wall, a crate,
a character. None of that is in the SDF field, which knows terrain, blobs and
collider proxies. A room is not in the field at all.

Two details that are not arbitrary:

- **The march ends AT THE LIGHT**, not at a tuned reach. A lamp two metres away
  and one twenty metres away need completely different distances, and any single
  number would be wrong for one of them. The step count is fixed, so a distant
  lamp simply samples more coarsely. It also stops at the light's own `range`:
  past that the lamp contributes nothing, so what is out there cannot matter.
- **The occluder-thickness window scales with the STEP**, not with the reach. A
  lamp across a room takes long steps, and a window sized for contact shadows'
  short trace would let the ray tunnel between two samples that both sit inside
  the same wall.

The visibility multiplies BOTH the diffuse and the specular halves. Shadowing
only the diffuse leaves a highlight floating in the dark, which is the giveaway
that a shadow is being faked.

**Per lamp, off by default** (`PointLight::shadows`, the "casts shadows"
checkbox, `shadows` in Lua). It costs a march per lit pixel per casting light,
and most lamps in a level have nothing to be blocked by — a strip under a
counter, a glow inside a sign, a fill light placed exactly so. The ones worth
paying for are the ones a player can walk around.

### The other half: occluders the camera cannot see (v0.53.0)

A screen-space trace has one structural limit — it reads the depth prepass, so it
only knows about surfaces that were *drawn*. Turn away from a wall and the shadow
it was casting stops existing, which is the most alarming way a shadow can
behave.

So a lamp now marches the field as well, through `point_field_vis`, and
`point_vis` is the `min` of the two. The division of labour is exact:

| | sees | at the resolution of |
|---|---|---|
| screen-space trace | only what is in frame | the real silhouette, every railing and bolt |
| field march | everything, in frame or not | a collider proxy or a baked occluder volume |

Neither alone is a local shadow. Together, a wall blocks a torch whether you are
looking at the wall or not.

The march itself is `field_vis`, split out of `light_vis` so the sun and a lamp
run **one** implementation over the same set — terrain, blobs, Field Shapes, the
baked occluder volume a static collider mesh gets, and every collider proxy. Two
copies of a 64-step SDF march would be two copies that drift.

Three things that fall out of it rather than being chosen:

- **Softness comes from the lamp's own size.** `k` in the k·d/t estimator IS the
  reciprocal of the emitter's apparent half-angle, so `dist / radius` is the
  physically correct value and needs no knob: a wide sphere lamp close up casts a
  soft edge, a bare point casts a hard one.
- **One steps knob drives both halves.** The field march gets twice the
  screen-space trace's step count because it covers the whole distance to the
  light rather than a short trace, but "how carefully does this lamp look" is
  still one number.
- **Proxies are now collected when the sun's shadows are OFF.** They were gated
  on the sun's switch, which was correct when the sun was the only thing that
  read them. A scene with sun shadows off and a torch casting would otherwise get
  an empty proxy list and a torch that shines through every crate in the room —
  precisely the silent-nothing shape this engine keeps finding in itself.

**What it still cannot do**: shadow from geometry the *field* does not know
about either. A mesh with no collider is in neither list, and a lamp will shine
through it when it is off screen. Giving it a collider — or any `Collidable`
primitive standing in for it — is the fix, and is the same rule the sun follows.

Verified by `offscreen_shadow_probe`, whose occluder is **never drawn**: it
exists only as a shadow proxy, well outside the frustum, so the depth buffer
cannot know it is there and any darkness on the floor came from the field march.
Both controls — the proxy without the flag, and the flag without the proxy —
read identically lit, so the shadow needs both halves and neither alone can
explain it.

These work in **every** view of the game: the window, a fullscreen Game tab and a
docked Game panel alike. That was not true when they landed — the offscreen
render path ran no depth prepass, so a docked panel showed a visibly different
picture from the same game fullscreen. Both paths run and bind it now, and
`tests/offscreen_draws_the_same_world.rs` fails if either stops.

Verified by `point_shadow_probe`: a floor, a wall, a lamp on one side, and no sun
or ambient at all — so every lit pixel came from the lamp and the wall's shadow
is the only thing that can take it away. It checks that the far side goes dark,
that the near side does **not** (a lamp getting dimmer is not a lamp getting
blocked), and that without the flag both sides are lit.

## 5d. The sun shadow map

`Light::shadow_map` (the Lighting panel's **shadow map**, `env.shadowMap`)
swaps the sun's field march for a shadow map. Off by default.

**Why.** The march costs pixels × the casters in each ray's reach × steps,
so a dense level pays most exactly where it has the most buildings. It also
only sees what physics sees: colliders, their proxies and terrain. A drawn
mesh that doesn't collide never cast, and neither did map-tool geometry,
because the occluder bakes only collect models. The map renders what's drawn,
once from the sun, and each pixel does twelve filtered lookups. The cost
stays flat however many casters there are.

**Measured** on freeflier's city (RTX 4060, 1080p, softness 0.35): the
opaque + lighting pass fell from 9.5 ms to 4.1 ms, and the map itself costs
0.65 ms. The whole GPU frame went from 12.9 ms to 8.0 ms.

**How.**

- `raster.rs` `sun_shadow_pass` draws the depth prepass's own opaque draw
  list, skinned parts included, into a 2048² depth map, using the prepass
  pipelines and a view from `sun_shadow_matrix`. The camera culls that list,
  so `gather.rs` `shadow_map_casters` adds the static models, map meshes and
  primitives it culled that still fall in the map's box: the roof over a
  camera looking at a wall. Those go to the map only. A rigged model out of
  view is left out.
- The map is an orthographic box along the sun, centred a third of
  `shadow_distance` ahead of the camera, reaching 0.85 of it around the
  camera and 300 m toward the sun. It's snapped to whole texels so edges
  don't crawl as the camera moves.
- The pass binds a group(0) variant with a 1×1 stand-in at the map's slot,
  because a texture can't be sampled and drawn to in one pass.
- `raster.wgsl` `sun_map_vis` offsets the point along its normal by 1.5
  texels, biases the depth by two texels, and takes a rotated 12-tap disc of
  hardware 2×2 compares. Softness scales the disc from 1.5 to 7.5 texels. The
  last 4% of the map fades to sun.
- `field.wgsl` `sun_shadow` asks `sun_map_vis` first and marches only when
  it answers −1. The raymarch module's stub always answers −1, so raymarched
  matter keeps the march.

**Watch out:** that hook has to be an `if`, not `select`. WGSL's `select`
evaluates both sides, and the first cut ran the march and the map on every
pixel: 16 ms instead of 4.

**Limits:** one cascade, so detail is set by `shadow_distance` (about 10 cm a
texel at 120). Blobs that are only a distance field cast only in the march.
In stars mode the map is drawn toward star 0, the brightest at the camera
(`star_uniforms` sorts them), and only that star reads it; the others march.
Over a shadow distance the direction to a star thousands of metres away
barely turns, so one direction serves. Cached static casters and more
cascades are the next levers.

## 6. Not yet

- **Point-light shadows from geometry with no collider.** §5c now covers
  off-screen occluders through the field, but a mesh that is in neither the depth
  buffer nor the proxy list is in nothing. A shadow map per casting lamp would
  close it completely and costs a whole-scene render per light per frame; a
  cheaper step is auto-proxying visible static meshes that have no collider.
- **Bent shadow rays** — arrives with light.md Tier 2 (the ray is already a
  field march, so nothing here blocks it).
- **Cascaded shadow maps.** Considered and deliberately not taken. CSM buys one
  thing this engine lacks — a moving mesh's exact silhouette at *long* range —
  and charges 2–4 extra whole-scene renders per frame, a cascade seam to
  maintain, and (worst here) a **second scene gather** to keep in step with the
  first, which is a mistake this codebase has already made four separate times.
  The near half of that gap is now covered by contact shadows at a fraction of
  the cost. If the far half ever matters, a **per-hero shadow map** — one small
  map for the one character that needs it, folded into the same visibility term —
  buys most of it for a fraction of CSM's machinery, and is the recommended next
  step over cascades.
- **Lua control** — the Lighting node's shadow fields aren't scripted yet
  (same gap as the PostProcess node; do both together).

Sources consulted while designing: iq's soft-shadow article (`min(k·d/t)` +
Sebastian Aaltonen's improved estimator), RTSDF (real-time SDF generation for
soft shadows), classic retro techniques (blob shadows / geometry-baked
shadows, polycount retro-3D FAQ, N64 homebrew writeups).
