# Floptle — Shadows (plan; nothing implemented yet)

**Status: PROPOSED · 2026-07-02 — research + design, decisions open.**
The engine currently has **no cast shadows** (only the PostProcess node's
ambient occlusion, which grounds objects but has no direction). This doc lays
out how the industry does shadows, what fits Floptle's hybrid
raymarched-SDF + raster renderer, and a recommended path — flexible enough to
span *pretty modern soft shadows* ↔ *hard retro PS1 shadows* per game.

> Reads-with: [`./renderer.md`](./renderer.md) §3 (the march that will carry
> the shadow ray), [`./light.md`](./light.md) (Tier 0 promises "SDF soft
> shadows"; Tier 2 later *bends* the same shadow ray),
> [`./post-processing.md`](./post-processing.md) (AO — the sibling effect).

## 1. The landscape (what the industry does)

**Shadow mapping** (the raster default: render depth from the light, compare):
- *Hard 1-tap* — pixelated edges; the PS2/early-PC look.
- *PCF* — N depth taps, blurred edge; the standard "soft-ish" shadow.
- *PCSS* — penumbra widens with caster distance; pretty, several× PCF cost.
- *CSM* — cascades re-fit the map to the camera for big worlds; 2–4 maps every
  frame, and cascade seams/shimmer are a permanent maintenance tax. Every
  general engine (Unity/Unreal/Godot) ships CSM+PCF because they must serve
  arbitrary triangle soups. We don't have to.

**SDF shadows** (march the distance field toward the light, iq's
`min(k·d/t)` penumbra — the technique this engine's own renderer.md already
promises): one extra march per shaded pixel, **no shadow map, no cascades, no
resolution, no shimmer**, world-scale for free (the field is camera-relative),
and *analytically soft* — `k` sweeps hard→soft continuously. Limitation:
only things **in the field** cast.

**Retro looks** are mostly *degradations you must be able to opt into*:
N64/PS1 games used **blob shadows** (a dark disc under the character),
vertex-baked darkening, or a character-only stencil silhouette. A modern retro
game gets the vibe with: hard edges, **quantized penumbra** (2–3 bands),
**ordered-dither** in the penumbra, chunky resolution, and shadows that are
a flat tinted multiply rather than physically-varying darkness.

## 2. What fits Floptle — the field is the shadow map

The renderer already *is* a raymarcher over one fused field (terrain volumes +
blobs, `map()` in `raymarch.wgsl`). SDF shadows reuse it verbatim:

```wgsl
fn light_vis(p: vec3<f32>, l: vec3<f32>) -> f32 {   // 1 = lit, 0 = shadowed
    var t = t0;                       // skip off the surface
    var vis = 1.0;
    for (var i = 0; i < SHADOW_STEPS; i++) {
        let d = map(p + l * t).d;
        if (d < 0.001) { return 0.0; }               // hard hit
        vis = min(vis, k * d / t);                    // penumbra estimate
        t += clamp(d, t_min, t_max);
        if (t > max_dist) { break; }
    }
    return smoothstep(0.0, 1.0, vis);
}
```

- `k` **is the softness dial**: k≈2 dreamy-soft … k≈64 razor-hard. One float
  spans the whole style range — no PCF kernels, no PCSS estimator.
- Terrain shadows terrain, hills shadow valleys, blobs shadow terrain and each
  other — **all cross-shadowing falls out of the one `map()`**.
- Large-world safe by construction (everything is already camera-relative;
  no cascade fitting, ADR-0015 untouched).
- At retro internal resolutions (240–480 rows) the extra march is cheap; at
  full res it can drop to **half-res + blur-upsample** exactly like SSAO does.

**The mesh problem, solved with what we already have.** Raster meshes aren't
in the field, so they need two bridges:

1. **Meshes RECEIVE field shadows:** bind the same volume atlas + globals to
   the raster pass and call `light_vis(world_pos, l)` in `raster.wgsl`'s
   fragment — a character standing under a terrain arch is correctly darkened.
   (Medium plumbing: share the raymarch bind group with the raster pipeline.)
2. **Meshes CAST via proxy occluders:** inject cheap analytic SDFs — capsules /
   boxes / spheres — into the shadow march only. And we already author them:
   **a node's physics collider (RigidBody/Collidable shape) doubles as its
   shadow proxy.** A capsule character casts a soft capsule shadow — which is
   *exactly* the grounded-but-soft look retro-styled games want (a blob shadow
   is literally a low-`k` capsule shadow). Static meshes can alternatively
   bake real SDF volumes through the existing `mesh2sdf` path when their true
   silhouette matters.

This is the philosophy call: **stay single-march, field-first** (light.md §11
"no second geometry model") rather than bolting on a shadow-map pipeline that
exists only to serve triangle silhouettes we mostly don't have. It also means
Tier 2 *bent shadow rays* (shadows detached from casters — light.md §4) work
on day one of Tier 2, because the shadow ray is already a field march — a
shadow-map pipeline would have to be thrown away to get there.

**What we give up** (honesty): pixel-exact silhouettes of complex *dynamic*
meshes (a windmill's blades shadowing as blades, not as boxes). If a game
someday needs that, a single non-cascaded shadow map for "hero casters" can be
added *then*, rendered into the same `light_vis` term. Not in this plan's
scope.

## 3. Developer-facing knobs (the customization ask)

Shadow settings live on the **Lighting node** (they belong to the light, not
to post-processing), per scene like everything else:

| Knob | Range / meaning | Style it unlocks |
|---|---|---|
| `enabled` | on/off | — |
| `softness` | maps to `k` (hard 64 … soft 2) | PS1-hard ↔ modern-soft |
| `strength` | 0..1 — how dark full shadow gets | airy ↔ pitch black |
| `tint` | RGB — shadows multiply toward `tint×(1−strength)`, not black | purple dusk shadows, sepia, horror green |
| `quantize` | 0 (smooth) / 2–4 bands | posterized toon/retro penumbra |
| `dither` | off / Bayer pattern in the penumbra | the PS1 dither look, pairs with retro mode |
| `distance` | max shadow march distance + fade | perf fence + open-world haze |
| `proxies` | auto (from colliders) / off per-node | who casts |

`softness=hard + quantize=2 + dither=on + retro 240p` ≈ authentic PS1;
`softness=soft + strength=0.6 + warm tint` ≈ modern cozy. Both are the same
shader, different uniforms — the flexibility is free because the penumbra is
analytic.

## 4. Phased plan (thin slices, each PNG-probed)

1. **Sun shadows on SDF matter** — `light_vis` in `raymarch.wgsl`'s terrain +
   blob shading (directional light only); knobs `enabled/softness/strength/
   tint` on the Lighting node + Inspector. Probe: hills casting into a valley,
   off/on. *(Smallest visible win; no new passes.)*
2. **Meshes receive** — share the field bind group with `raster.wgsl`, call
   `light_vis` per fragment (gate: skip when shadows off → zero cost).
   Probe: mesh under a terrain overhang.
3. **Proxy casters** — a `shadow_proxies` uniform array (capsule/box/sphere,
   auto-harvested from colliders like the physics build already does); folded
   into the shadow march only. Probe: character capsule grounding on terrain.
4. **Retro pass** — `quantize` + `dither` + probe matrix of the style presets
   (document as recipes: "PS1", "N64 blob", "modern soft", "toon").
5. **Point-light shadows (deferred)** — same march per point light is N×cost;
   decide later if any game needs it (rim/AO usually carries interiors).
6. **Bent shadow rays** — arrives with light.md Tier 2, not this plan.

## 5. Open decisions (for Ty)

- **A. Field-first (recommended above) vs shadow maps vs both?** Shadow maps
  buy exact dynamic-mesh silhouettes at the cost of a second pipeline,
  cascades, and losing the bent-ray future. Recommendation: field-first.
- **B. Settings home:** Lighting node (recommended — it's the sun's property)
  vs the PostProcess node (keeps all "look" dials in one place).
- **C. Proxy default:** every collidable node casts by default (grounded by
  default, occasional surprises) vs opt-in per node (explicit, more setup).
  Recommendation: default-on with a per-node "casts shadows" toggle.
- **D. Half-res shadow term** from day one, or only if the heatmap says so?
  Recommendation: full-res first (retro rows make it cheap), measure, then
  optimize — renderer.md §6's posture.

Sources consulted: iq's soft-shadow article (the `min(k·d/t)` penumbra +
Sebastian Aaltonen's improved estimator), RTSDF (real-time SDF generation for
soft shadows), classic retro techniques (blob shadows / geometry-baked
shadows on N64-era hardware, per the polycount retro-3D FAQ and N64 homebrew
writeups).
