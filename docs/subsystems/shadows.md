# Floptle — Shadows (field-marched sun shadows)

**Status: IMPLEMENTED · 2026-07-02** — field-first SDF sun shadows, per scene,
on everything: terrain/blobs cast from the field itself, raster meshes *receive*
by marching the same field and *cast* through collider-proxy occluders. The
style range spans razor-hard PS1 to dreamy-soft modern from one dial set on the
Lighting node. (Deferred: point-light shadows, bent shadow rays — see §6.)

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
meshes — a windmill's blades shadow as their collider box, not as blades. If a
game someday needs a hero-caster silhouette, a single non-cascaded shadow map
can be folded into the same visibility term *then*.

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

**Meshes cast — proxy occluders:** meshes aren't in the field, so the editor
harvests up to 32 cheap analytic stand-ins per frame (`collect_shadow_proxies`)
from what the scene already authors — **physics collider shapes**:

- a `RigidBody` casts its body shape (sphere / capsule / oriented box);
- a static `Collidable` primitive casts the same shape the physics build gives
  it (Cube → 0.7·scale box, Sphere → 0.85·max-scale, Capsule → 0.5-sized);
- trimesh colliders (Collidable **meshes**) have no cheap analytic stand-in —
  give such a node a RigidBody box if it should cast;
- blobs/terrain never need proxies (they're in the field itself).

A capsule character casting a soft capsule shadow *is* the retro blob-shadow
look — grounded but forgiving. Proxies are folded into the shadow march only
(never the drawn surface or AO), a proxy containing the ray start is skipped
(so a mesh doesn't blanket-shadow itself from inside its own capsule), hidden
(`Visible(false)`) nodes don't cast, and every collider node has a
**casts shadows** checkbox in the Inspector (`CastShadow(false)` opt-out —
casting is the default, per-node opt-out serializes only when off).

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
- Probes: `shadow_probe` renders the whole matrix (off / soft / hard / retro /
  tint / full-with-AO) over a hill + shadowed cube (receive) + sunny cube and
  capsule (proxy cast) + blob; `terrain_far_probe` stays bit-identical with
  shadows off.

## 5. Performance posture

Decision D (full-res first, measure, then optimize — renderer.md §6): the
march runs per shaded fragment, ≤64 steps, and is gated hard — it never runs
when shadows are off, on sun-averted fragments (`n·l ≤ 0`), on unlit matter,
or past `shadow_distance`; empty scenes break out after one sample. At retro
internal resolutions the cost is trivial. If a full-res scene ever burns here,
the SSAO-style half-res + blur-upsample path is the known next lever.

## 6. Not yet

- **Point-light shadows** — same march per light is N× cost; rim/AO carries
  interiors for now. Decide if a game needs it.
- **Bent shadow rays** — arrives with light.md Tier 2 (the ray is already a
  field march, so nothing here blocks it).
- **Hero-caster shadow map** — only if exact dynamic-mesh silhouettes are ever
  required; folds into the same visibility term.
- **Lua control** — the Lighting node's shadow fields aren't scripted yet
  (same gap as the PostProcess node; do both together).

Sources consulted while designing: iq's soft-shadow article (`min(k·d/t)` +
Sebastian Aaltonen's improved estimator), RTSDF (real-time SDF generation for
soft shadows), classic retro techniques (blob shadows / geometry-baked
shadows, polycount retro-3D FAQ, N64 homebrew writeups).
