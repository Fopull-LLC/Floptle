# Floptle Vertex Painting — Design Proposal

**Status:** **SHIPPED 2026-07-15 — phases 1–4 complete and usable** (transport, glTF
`COLOR_0` import, the 🖌 brush, and the re-import guard). Phases 5–6 (shader-IR
`in.vertexColor` node, skinned/Lua/AO-bake reach, the "Unlit scene" toggle) remain. See
§8/§8.1 for what is verified and what isn't.
**Author:** synthesis of two deep research passes over the live workspace, 2026-07-15
**Scope:** per-vertex color authored in-editor — data model, GPU transport, brush tool,
persistence, shader-IR seam — and the classic no-realtime-lighting look it unlocks
**Grounded against:** the live workspace as of 2026-07-15 — `Vertex`/`MeshData`
(`floptle-render/src/mesh.rs`), the 16/16-full raster instance stream
(`floptle-render/src/raster.rs`), `raster.wgsl`'s `VsOut`, the terrain sculpt triad
(`terrain_edit.rs` + `terrain_ui.rs`), `history.rs`, and the shader IR's `Input` enum

---

## 1. Executive summary

Vertex painting is **the retro art pipeline's shortcut around lighting**. You paint color
straight onto a mesh's vertices; the GPU interpolates it across triangles for free; with
`unlit` on, that painted color *is* the final look. No lightmaps, no bake, no realtime
lights — the way N64 and PS1 shipped, and the way a small team ships a lot of surface
variety without a lot of work.

Floptle is unusually well set up for this. The shading path already carries a
`@location(2) color: vec4<f32>` varying through `VsOut` (`raster.wgsl:75`) and multiplies
it into albedo at exactly one line (`raster.wgsl:166`), and `MaterialParams` already has an
`unlit: bool` (`raster.rs:123`). Vertex paint slots into machinery that exists rather than
demanding a new render path.

The five decisions this document makes and defends:

1. **Paint is per-NODE, not per-mesh.** This is forced, not chosen. Every primitive of a
   given shape shares one `MeshId` (`render_frame.rs:756`: `self.mesh_ids.get(*shape as usize)`),
   so mesh-keyed paint would paint *every cube in the scene* identically. Paint keys off a
   stable `paint_id: u32` on the node — the exact precedent `Matter::Terrain { id: u32 }`
   already set (`matter.rs:312-316`) for exactly this reason.

2. **Color rides a storage buffer + a packed per-instance offset — not a vertex attribute.**
   The vertex attribute budget is **genuinely full at 16/16** and the code says so:
   *"location 15 (the last free slot under the 16-attribute floor)"* (`raster.rs:171`).
   One global `array<u32>` of RGBA8 colors, plus a per-node base offset packed into the
   `params.z` lane, gives per-node paint with **zero new attributes and batching preserved**.

3. **Painted color multiplies albedo; it does not replace it.** `albedo = texel * instance
   * vertex`. Multiply is what bakes lighting/AO into vertices (the retro trick); "replace"
   is just multiply against a white texture, so it needs no separate mode.

4. **The tool is a triad, copying terrain exactly**: a `Tool::Paint` variant for viewport
   input takeover, a `🖌 Paint` dock tab for brush settings, and a viewport ring overlay.
   Terrain proves this shape; the Inspector is wrong here because it rebuilds per selection
   and brush state must outlive selection changes.

5. **`in.vertexColor` is a new shader-IR input, NOT folded into `in.instanceColor`.**
   Folding paint into the existing `InstanceColor` (`ir.rs:119`) would silently change the
   meaning of every `.flsl` already written.

---

## 2. Why the vertex attribute budget forces the design

This is the single constraint that shapes everything, so it goes first.

Two vertex buffers feed the raster pipelines:

| Buffer | Locations | Contents |
|---|---|---|
| 0 — per-vertex (`mesh.rs:23-27`) | **0, 1, 2** | `pos` F32x3, `normal` F32x3, `uv` F32x2 |
| 1 — per-instance (`raster.rs:158-173`) | **3 … 15** | 13 × `Float32x4` — model matrix, normal matrix, color, emissive, specular, params, rim, tile |

Locations **0–15 are fully consumed**. `device.rs:68` requests `wgpu::Limits::default()`,
whose `max_vertex_attributes` is **16** — the WebGPU downlevel guarantee. So:

- **A `@location(16) vcolor` does not exist.** Raising the requested limit works on most
  desktop adapters but spends portability the codebase has deliberately been holding, and
  it would be spent on a cosmetic feature.
- **A second vertex buffer does not help.** Shader locations are global across buffers, so
  a dedicated color buffer still needs a 17th location.
- **Reclaiming a lane is possible but invasive.** The three `normal_mat` columns
  (locs 7–9) only ever read `.xyz` (`raster.wgsl:91`), and `color`/`emissive`/`specular`
  could pack to `Unorm8x4`. Any of these frees a slot — at the cost of touching **all five
  pipelines** that pair `Vertex::LAYOUT` with `INSTANCE_LAYOUT` (`raster.rs:482, 526, 558,
  598, 703`) plus every `VsIn` in `raster.wgsl`.

**The chosen route sidesteps all three.** Per-vertex color goes in a **storage buffer**
indexed by `@builtin(vertex_index)`, and the per-node base offset into that buffer is
**packed into an existing instance lane**. Nothing is added; nothing is reclaimed.

This is not a workaround — it is the pattern the engine will need again. **GPU vertex
skinning** (`docs/subsystems/animation.md`, still pending) wants `joints` + `weights` =
two more per-vertex streams that cannot fit either. Establishing "extra per-vertex streams
live in storage buffers, indexed by `vertex_index`" solves vertex color *and* unblocks
skinning with the same seam.

### 2.1 The packed offset — confined to the vertex shader

Reserving a lane for the offset is impossible, so it shares one. `params` is
`[shininess, rim_strength, unlit, ambient]` (`raster.rs:100`) — and **`unlit` is a `0/1`
flag occupying a full `f32`**. That is the lane.

> **⚠ The naive packing is a bug — this cost us once already, on paper.** `params.z` is read
> exactly once, at `raster.wgsl:179`, as `if (in.params.z > 0.5)`. That is a **threshold
> test, not a bit test**. Packing an offset into the high bits makes `params.z` nonzero for
> every painted node, so **every painted node would silently render `unlit`**. Any
> implementation that packs this lane and leaves `:179` alone ships that bug.

The fix is also the design's best simplification: **decode in the vertex shader and re-emit
a clean flag, so the fragment shader never learns the lane is packed.**

```
// instance attribute (VS-side only, never interpolated)
params.z  =  f32( unlit_bit | ((paint_base + 1) << 1) )   // high bits 0 = unpainted
```

```wgsl
// vs — the packing lives and dies here
let pz    = u32(in.params.z);          // instance attr: read exactly, no interpolation
let unlit = (pz & 1u) != 0u;
let pbase = pz >> 1u;                  // 0 = this node has no paint

out.vcolor   = select(vec4(1.0), unpack4x8unorm(vpaint[pbase + vid]), pbase != 0u);
out.params   = vec4(in.params.x, in.params.y, select(0.0, 1.0, unlit), in.params.w);
//                                            ^ clean 0/1 again — fs:179 is UNCHANGED
```

This confinement is not just tidiness; it dodges a second, subtler failure. `VsOut.params`
is a **perspective-interpolated varying**. Interpolating a large integer-as-float
(≈16.7M) across a triangle is not guaranteed to round-trip exactly — `u32()` of
`16777213.9998` truncates to the wrong offset and reads the wrong node's colors. Because
the offset is consumed in the VS **from the instance attribute directly**, it is never
interpolated, and the only thing that *does* interpolate (`out.vcolor`) is a genuine
per-vertex value that *should* interpolate. The alternative — tagging the varying
`@interpolate(flat)` — also works, but changes an existing struct's semantics to fix a
problem this design simply never creates.

**The packing is idiomatic here, not a hack.** The neighbouring `rim.w` lane already does
exactly this — `raster.rs:102-104` packs `mode (0|1|2) + round(rotation_degrees * 10) * 4`
into one float and documents it as "an exact small int". `f32` represents integers exactly
to 2²⁴, so `pz ≤ 16,777,215` ⇒ **`paint_base ≤ 8,388,606` painted vertices per scene**
before the encoding saturates — far past any retro-style scene, but assert + Console-warn
at allocation rather than corrupting silently (§7).

The payoff is the whole reason this design exists: painted nodes **keep their place in the
instanced batch**. Same `MeshId`, same bucket, same draw call — only `params.z` differs.
See §4.1 for what that means for an all-painted scene.

**Implementation nit for phase 1:** keep a dummy element at `vpaint[0]` so the
`pbase == 0` path can never read out of bounds regardless of how `select` evaluates its
arms (WGSL does not guarantee lazy evaluation).

### 2.2 The buffer

```wgsl
// group(0) — bound once per pass, alongside RasterGlobals
@group(0) @binding(1) var<storage, read> vpaint: array<u32>;   // RGBA8, packed
```

Group 0 is right: the buffer is **global**, not per-mesh (group 1 is the per-mesh texture
bind group) and not per-material (group 3 is `.flsl` params). One growable
`STORAGE | COPY_DST` buffer, packed with every painted node's color block back to back,
re-uploaded on edit. At 4 bytes/vertex this is small — a 50k-vertex painted scene is 200 KB.

```wgsl
// vs — one line
out.vcolor = select(vec4(1.0), unpack4x8unorm(vpaint[pbase + vid]), pbase != 0u);
```

`unpack4x8unorm` is core WGSL. `@builtin(vertex_index)` under an indexed draw yields the
index-buffer value with `base_vertex = 0`, which is what the raster path issues — correct
by construction, but worth an assertion if a base-vertex draw ever appears.

---

## 3. Data model

### 3.1 The node key

**Shipped as an additive COMPONENT, not a `Matter` field** — a correction to this
document's original sketch, made during implementation:

```rust
// floptle-core/src/matter.rs
pub struct VertexPaint { pub id: u32 }        // additive component
// floptle-scene: NodeDoc { paint: Option<u32>, .. }  — one serde field, defaulted
```

Putting `paint_id` inside `Matter::Mesh`/`Matter::Primitive` would have touched **94 match
and construction sites across 16 files**, and it was conceptually wrong anyway: paint is
orthogonal to what a node *is*. The engine already has the right idiom — Material,
RigidBody, Collidable and Scripts are all additive components beside the
mutually-exclusive Matter type. `VertexPaint` joins them, touching **zero** existing match
sites. `NodeDoc.paint` is `#[serde(default, skip_serializing_if = "Option::is_none")]`, so
scene RON is untouched for anyone not using the feature. **It must be a stable `u32`, never an
`Entity`**: `history.rs:52`'s `restore()` respawns the whole `World`, invalidating entity
handles. This is precisely why `Matter::Terrain` carries `id: u32`, and the same reasoning
applies unchanged.

### 3.2 On disk

Per-vertex arrays must not live in the scene `.ron` — that is why terrain fields don't
(`project.rs:717-783` writes `<project>/terrain/<scene>.<id>.tfield` beside the scene).
Vertex paint follows the same shape — but as **one container per scene**, not one file per
node: `<project>/paint/<scene>.vpaint`. **See §9.3 for the resolved format** (an index of
`paint_id → vertex_offset/count/geom_hash`, followed by the bulk RGBA8 that uploads to
`vpaint` verbatim). One file, one read, one `write_buffer`.

**`geom_hash` is the re-import guard**, and it earns its place. Import splits meshes
per-material into parts with independently re-indexed vertex arrays
(`gltf_import.rs:155-165`) and then recenters them (`recenter_and_measure`). Part ordering
falls out of material iteration order — so **a re-export from Blender can silently permute
parts or change vertex counts and scramble someone's paint work**. On load, mismatched
`vertex_count`/`geom_hash` must **not** silently apply garbage: warn in the Console, keep
the orphaned block, and offer *"re-project paint by nearest position"* (§6, phase 4).

> **This side-steps a known bug class.** The terrain sidecars key by *scene name*, which
> caused the 2026-07-14 cross-scene overwrite bug. `<scene>.<paint_id>` inherits that
> scene-keying — so the fix that landed for terrain (snapshot on Play; no saves during
> Play; restore the scene name before adopt) must be mirrored here, or the paint tool
> re-opens the same hole. **See §9 decision 3** — asset-keyed storage avoids it entirely
> but cannot express per-node paint.

---

## 4. Rendering

The injection point is a single line. `raster.wgsl:166` currently reads:

```wgsl
let albedo = texel.rgb * in.color.rgb;          // in.color = the INSTANCE tint
```

becomes

```wgsl
let albedo = texel.rgb * in.color.rgb * in.vcolor.rgb;
```

with alpha likewise at `:169`. Everything downstream — SDF sun shadows, `sdf_ao`, point
lights, Blinn-Phong, rim, fog — is untouched. **The `unlit` early-out at `:176` is what
makes this the retro path**: painted color, straight out, no lighting math.

`VsOut` gains `@location(11) vcolor: vec4<f32>` — interstage locations are nowhere near
their limit (11 of ~16 used), unlike vertex attributes.

Two entry points share `VsOut` and both need care:

- **`fs_depth` (`:230-237`)**, the conservative alpha-test depth prepass, reads
  `in.color.a`. If vertex alpha can drive transparency it **must** be mirrored here or the
  prepass will reject fragments the color pass keeps.
- **`fs_mask` (`:221`)**, the selection outline, should *ignore* vcolor — a painted object's
  silhouette must not change.

> **Gotcha, from the shader-system notes:** `TEST_PRELUDE` (`floptle-shader/src/transpile.rs`)
> mirrors `raster.wgsl`'s `VsOut` for `.flsl` validation. Adding a varying **requires
> updating it in lockstep** or every shader test starts failing.

### 4.1 The all-painted scene — the case this is built for

The target workload is **a whole scene of vertex-painted, unlit meshes and no realtime
lighting**. That is not the degenerate case for this design; it is the case it wins hardest
on. Concretely:

**Draw calls: identical to an unpainted scene.** This is the crux. Instances bucket by
`(mesh, tex, flsl)`; `params.z` is per-instance data *inside* the instance stream, not a
binding. 500 painted cubes are 500 instances of one `MeshId` in **one draw call** — exactly
as if none of them were painted. Paint costs zero draw calls, forever. (This is precisely
what the rejected interleaved-`Vertex` design could not do: per-node paint would have forced
a unique vertex buffer per painted node, i.e. **one draw call each**. In an all-painted
scene that converts a handful of batched draws into hundreds of unbatched ones — the 8×
memory figure was the *smaller* half of that mistake.)

**Runtime memory: 4 bytes per painted vertex. That's it.** A heavy retro scene at 1M total
vertices = **4 MB** of paint data. The interleaved design would have duplicated
position+normal+uv per painted instance (~36 B/vert ⇒ 36 MB+) *and* lost batching. For
reference, one 1024² RGBA texture is 4 MB — the entire scene's paint costs one texture.

**Fragment cost: strongly negative — an all-unlit painted scene is much *faster* than a lit
one.** The `unlit` branch at `raster.wgsl:179` early-returns **before** SDF sun shadows
(`sun_shadow`), `sdf_ao`, the 16-light `point_diffuse` loop, Blinn-Phong specular, and the
rim/fresnel term. With the whole scene unlit the branch is uniform — no divergence — so
every fragment skips all of it. **You are not paying for lighting you deleted.** Vertex
paint is not a cost you accept for the retro look; it is how the retro look gets cheap.

**Vertex cost: one `array<u32>` fetch + `unpack4x8unorm` per vertex.** Coherent (adjacent
vertices read adjacent `u32`s), and vertex shading is not this renderer's bottleneck —
`docs/subsystems/renderer.md`'s perf work targeted marches and fragment cost.

**Editor-only cost, paid by nobody who plays the game:** the CPU `MeshData` + spatial hash
the brush needs (§6.5). The **runtime never loads it** — it needs only the 4 B/vertex block.

> **The compounding win — recommended, small, separate.** If a scene is *entirely* unlit,
> the SDF shadow pass and SSAO are computing results **no fragment will ever read**. A
> scene-level **"Unlit scene"** toggle on the Lighting node that skips the shadow + AO
> passes wholesale turns "I deleted lighting" into an actual frame-time refund rather than
> just an unread one. This is independent of vertex paint (it needs no paint data) and
> should be **its own small change** — but it is the other half of making a
> deliberately-unlit game the *fast* option, so it is tracked here at phase 6.

---

## 5. The shader-IR seam

```rust
// floptle-shader/src/ir.rs — new Input variant
/// The node's painted per-vertex color (`vec4`), interpolated. Fragment only.
VertexColor,
```

→ `"vertexColor"` in `Input::name()` (camelCase, per house style), mapping to `in.vcolor`
in `preview.rs:248-253` beside the existing `Input::InstanceColor => "in.color"`.

**Deliberately kept separate from `InstanceColor`.** Folding paint into `in.color` would
retroactively change what every shipped `.flsl` means — including the seeded examples that
already reference `in.instanceColor` (`examples.rs:41, 68, 108, 127, 149, 189`). Two
inputs, two meanings, no silent breakage.

This also gives the graph editor vertex paint **for free**: `in.vertexColor` becomes a
draggable node, so a `.flsl` can use painted color as a *mask* (blend two textures by
painted red, drive dissolve by painted alpha, tint emissive by painted blue) rather than
only as albedo. That is where vertex paint stops being a lighting shortcut and starts being
a general per-vertex data channel — the surreal-look payoff Floptle is actually for.

---

## 6. The editor tool

Terrain sculpting is the template, near line for line.

### 6.1 The triad

1. **`Tool::Paint`** — `gizmo.rs:30` enum + `from_digit` (`:46`, next free digit) + `label`
   (`:58`). **Must be added to the three `Select | Sculpt` early-outs** at `gizmo.rs:203`
   (`build_gizmo`), `:367` (`hit_test`), `:492` (`paint_gizmo`) — otherwise the move gizmo
   draws over the brush and steals clicks.
2. **`EditorTab::Paint`** — `dock.rs:7-26` variant, `title()` `"🖌 Paint"` (`:29-45`),
   `focus_paint_tab()` (`:119-125`), placed beside Inspector/Terrain in `default_dock()`
   (`:71-97`), dispatched at `main.rs:586`. `set_tool()` (`selection.rs:19-27`) auto-focuses
   it, mirroring the existing `if tool == Tool::Sculpt { self.focus_terrain(); }`.
3. **Viewport overlay** — a brush ring in the surface tangent plane, projected via
   `viz::project()` and painted in `scene_tab.rs`, exactly as `TerrainViz` is at `:265-290`.

Input takeover follows `main.rs:2242-2290`: when `over_scene && tool == Tool::Paint`, the
tool consumes the whole click — picking and the gizmo never run.

### 6.2 The stroke

`vertex_paint_frame_update()`, called from `render_frame.rs:45` beside
`terrain_frame_update()`, modelled on `terrain_edit.rs:162-301`:

1. **Gate** — `tool == Tool::Paint && cursor_over_scene() && !playing`.
2. **Ray build** — reuse `terrain_edit.rs:167-177` verbatim (camera-relative, ADR-0015).
3. **Hit** — ray vs. mesh triangles. **This does not exist yet** (§7).
4. **Telegraph** — 40-point ring, tangent plane from the hit normal.
5. **Dab spacing** — copy `terrain_edit.rs:228-245`: re-dab when the cursor moves
   `≥ radius * 0.34` **or** 0.10 s elapses. This is what makes a stroke feel like a brush
   instead of a stutter, and it is already tuned.
6. **Apply** — for every vertex within `radius` of the hit, `falloff = smoothstep` by
   distance; `color = lerp(color, brush.color, strength * falloff)`. Write to the CPU block,
   mark the node's range dirty.
7. **Upload** — partial `write_buffer` into the node's slice of the global paint buffer.
   The dirty-range union in `terrain_edit.rs:284-299` is the pattern.

### 6.3 Brush UI

`VertexBrush`, mirroring `TerrainBrush` (`terrain_ui.rs:9-47`):

```rust
pub(crate) struct VertexBrush {
    mode: PaintMode,     // Paint | Smooth | Fill | Sample(eyedropper)
    radius: f32,
    strength: f32,
    color: [f32; 3],
    alpha: f32,
    channels: [bool; 4], // R/G/B/A masking — for shader-mask authoring (§5)
    falloff: Falloff,    // Smooth | Linear | Constant(hard retro edge)
    backfaces: bool,     // paint through to vertices facing away
}
```

`Constant` falloff matters more than it looks: hard-edged per-vertex color with no gradient
is *the* N64 look, and a smooth-only brush cannot make it.

### 6.4 Undo

`Snapshot::VertexPaint(paint_id, Vec<u8>)` + `swap_vertex_paint_bytes()`, following
`swap_terrain_bytes` (`history.rs:73`) — undo/redo is a byte swap that never touches the
ECS. Stroke banking copies sculpt exactly: lazily snapshot on the **first dab** (you don't
know which node was hit until the ray lands), bank on LMB-up if `stroke_dabbed`
(`main.rs:2295-2300`).

> **The Play gotcha (`history.rs:17-29`), as shipped:** `push_history` — and therefore `record()`,
> `begin_edit()`, `undo()`, `redo()` — is a **hard no-op while `self.playing`**. Sculpt
> merely tolerates this. Paint should **actively gate itself off during Play** (grey the
> toolbar button): paint edits asset-adjacent data that Stop does not revert, so a
> Play-time stroke would survive Stop while being unrecorded and un-undoable. That is a
> data-loss trap, not a nuisance.

### 6.5 The CPU geometry problem

**The editor does not retain CPU mesh data.** `MeshAsset` (`main.rs:1830-1834`) holds only
`parts: Vec<MeshId>`, `size`, `rig`. Every consumer that needs vertices — `play.rs:107`,
`terrain_edit.rs:121`, `net.rs:1511`, `viz.rs:112` — **re-imports the `.glb` from disk**.

A brush cannot re-import per dab. So the paint tool must retain, per painted mesh:

- the CPU `MeshData` (positions + indices), and
- a **ray-triangle acceleration structure**, which also does not exist —
  `TriMeshCollider` (`physics/src/shapes.rs:196`) is a uniform spatial hash for *unsigned
  closest-point* queries, not a ray intersector.

So phase 1 must add a Möller–Trumbore ray-triangle test plus an accel structure. The
cheapest honest option is to **reuse `TriMeshCollider`'s uniform-spatial-hash approach**
(`shapes.rs:202-262`) rather than introduce a BVH — same idea, ray-walked instead of
sphere-queried, and it keeps one spatial structure concept in the codebase.

This retained cache is a real cost (positions + indices + hash per painted mesh) but it
**pays down existing debt**: the re-import-from-disk pattern above is already a latent
performance smell, and a shared CPU mesh cache is the thing that fixes it.

---

## 7. Risks & honest costs

| Risk | Severity | Mitigation |
|---|---|---|
| **No ray-triangle intersector exists** | **High** — phase 1 blocker | Möller–Trumbore + reuse `TriMeshCollider`'s spatial-hash pattern (§6.5) |
| **No retained CPU mesh** | **High** — phase 1 blocker | Paint-side `MeshData` cache; doubles as the fix for the re-import smell |
| **Naive `params.z` packing silently unlits every painted node** (`:179` is a `> 0.5` threshold, not a bit test) | **High** — the sharpest trap | Implement the **§2.1 VS-confined form**: decode in `vs`, re-emit clean `0/1`, leave `:179` alone. Add a probe asserting a painted **lit** node stays lit. |
| **Re-import scrambles paint** | Medium | `geom_hash` + `vertex_count` guard; warn, never silently apply; offer re-projection |
| **Scene-keyed sidecar re-opens the 2026-07-14 overwrite bug** | Medium | Mirror the terrain fix (Play snapshot, no saves during Play, name restore) — or decide asset-keyed (§9.3) |
| **`params.z` packing saturates past ~8.3M painted vertices** | Low | Assert + Console warn on allocation; the ceiling is far past any real scene |
| **`TEST_PRELUDE` drifts from `VsOut`** | Low | Update in the same commit; the shader tests fail loudly if not |
| **Vertex alpha vs. the depth prepass** | Low | Mirror into `fs_depth`, or forbid vertex alpha driving transparency in phase 1 |
| **Storage buffer in the vertex stage** | Low | `Limits::default()` allows it; only `downlevel_webgl2_defaults()` would not, and the engine does not target it |

**What this proposal explicitly does not do:** per-texel painting.
`docs/subsystems/materials-and-textures.md:243` puts *"per-texel painting / texture baking
in-editor"* out of scope — *"that's Blender's job"* — and **that line stays**. Vertex paint
is a per-vertex attribute, not a texel operation; it is cheap, it is the retro idiom, and it
does not drag in UV layout, texture resolution, or a bake pipeline. The boundary is
deliberate and this document reinforces it rather than eroding it.

`docs/subsystems/asset-pipeline.md:26` currently **claims Floptle imports vertex colors**.
It does not — `read_colors` has zero hits workspace-wide. Phase 1 makes that line true.

---

## 8. Roadmap

| Phase | Deliverable |
|---|---|
| **1 — Transport** ✅ **DONE** | `vpaint` storage buffer at `@group(0) @binding(1)` + bump allocator (`Raster::alloc_paint`/`mesh_paint_base`); `MeshData.colors`; `MaterialParams.paint_base`; VS-confined `params.z` packing; `VsOut.vcolor`; the albedo multiply + `fs_depth` alpha; `TEST_PRELUDE` sync. Verified by `examples/vertex_paint_probe.rs`. |
| **2 — Import** ✅ **DONE (code)** | `read_colors(0)` in `gltf_import.rs` + `gltf_rig.rs`, normalized via `into_rgba_u8`, with white back-fill so parts mixing painted/unpainted primitives stay parallel to `vertices`. Closes the `asset-pipeline.md:26` claim. **Not yet exercised against a real painted `.glb`** — no such asset exists in-repo (see §8.1). |
| **3 — The brush** ✅ **DONE** | `paint_mesh.rs` (retained CPU geometry + Möller–Trumbore + uniform-grid accel, 4 unit tests); `VertexPaint` **component** (NOT a `Matter` field — see below); `Tool::Paint` (key 7) + 🖌 Paint tab + magenta ring overlay; `VertexBrush`; dab spacing reused from sculpt; `paint_io.rs` `.vpaint` container; `Snapshot::VertexPaint` undo; **hard** Play gate. |
| **4 — Robustness** ✅ **DONE** | `geom_hash` re-import guard (quantized FNV-1a — fires on real moves, tolerates float noise); Paint/Smooth/Sample modes; channel masking; Fill/Clear; copy-on-write forking. *Nearest-position re-projection deferred: the guard currently refuses + warns rather than re-projecting.* |
| **5 — Shader IR** | `Input::VertexColor` → `in.vertexColor` graph node; a seeded example shader that blends two textures by painted mask. |
| **6 — Reach** | Vertex paint on skinned meshes (rides the same buffer — paint is bound to `vertex_index`, so CPU skinning's `update_mesh_vertices` re-upload cannot stomp it); Lua read/write; per-vertex AO bake ("bake the SDF AO the engine already computes *into* the vertices, then go unlit"); the **"Unlit scene" Lighting-node toggle** (§4.1) that skips the shadow + AO passes wholesale — independent of paint, but the other half of making a deliberately-unlit game the fast option. |

Phase 2 is deliberately ordered before the brush: it makes an existing doc claim true, and
it lets Blender-authored paint ship while phase 3's CPU-mesh and raycast work lands.

### 8.1 What is verified — and what isn't

**Verified by `cargo run -p floptle-render --example vertex_paint_probe`** (assertions, not
eyeballing):

1. **Paint reaches pixels** — material tint white, painted red ⇒ `[255, 0, 0]`, so the color
   can only have come through `vpaint`.
2. **The trap is dodged** — painted+lit `[228, 0, 0]` is **not byte-identical** to
   painted+unlit `[255, 0, 0]`. This is the assertion that matters: the bug it guards is
   invisible in a screenshot (the paint looks perfect; only the lighting quietly vanishes),
   so it is asserted rather than looked at.
3. **No regression** — unpainted geometry still renders `[255, 255, 255]`; `paint_base 0`
   resolves to the white identity, not black or garbage.
4. **Per-node paint on a SHARED mesh** — two instances of ONE `MeshId`, two blocks, in one
   instanced batch, render `left [255,0,0]` / `right [0,0,255]`. This is the claim the whole
   design exists for, and the thing mesh-keyed paint could not do.

**Verified by unit tests** (`cargo test -p floptle-editor paint`, 7 tests): ray-triangle hit
/ miss / **nearest-of-two-surfaces**, radius queries, `.vpaint` container round-trip,
garbage refusal (bad magic *and* wrong version), and `geom_hash` firing on a real vertex
move while tolerating sub-quantum float noise.

**Verified end to end:** glTF `COLOR_0` import against a real painted `.glb` (1984 verts,
482 distinct colors, stream exactly parallel to `vertices`). None of the repo's test models
ship `COLOR_0`, so one was synthesized by injecting a position-gradient `COLOR_0` into
`domer.glb`. The editor also **launches and runs on a real GPU** with zero wgpu validation
errors — which is what proves the new `@group(0) @binding(1)` storage buffer and the
rewritten `vs` are valid on real hardware, not just in a headless probe.

**Honestly NOT verified:** nobody has dragged the brush across a mesh in a live window. The
interactive path (cursor ray → dab → upload) is compile-checked, its raycaster and container
are unit-tested, and its transport is pixel-verified — but the *gesture* is unexercised.
Feel/ergonomics (radius defaults, dab spacing on a small prop, whether the magenta ring
reads well) are unproven and are the first thing to shake out. Also unverified: a full
save → reload → paint-still-there cycle through the real editor.

Workspace `cargo test` green; `cargo clippy --workspace --all-targets` at **zero**.

## 9. Decisions — RESOLVED (signed off 2026-07-15)

Ty delegated the four calls. They are now spec. One mechanism — **shared blocks with
copy-on-write** — resolves three of them at once, so it is stated first.

### 9.0 The unifying mechanism: paint blocks are shared until painted

`paint_base` is *just an offset into `vpaint`*. Nothing stops **N nodes pointing at the same
offset**. That single observation collapses decisions 1, 4, and most of 3:

- **Sharing is free.** Many nodes → one block → one `pbase`. Zero extra memory, zero extra
  draw calls, and they still batch (they differ only in `params.z`… which, when shared, they
  don't even differ in).
- **Forking is cheap and lazy.** The first dab on a node whose block is shared allocates a
  new block, `memcpy`s the old one, and rewrites that node's `paint_base`. **Copy-on-write.**

So paint is **per-node in identity, shared in storage**. You get asset-level "paint once,
every barrel inherits" *and* per-node override, from one ~20-line mechanism, with no second
storage key and no second concept.

### 9.1 Per-node paint — **CONFIRMED**, with COW removing the sting

Per-node is forced by `render_frame.rs:756` (all primitives of a shape share one `MeshId`),
so it was never really optional. §9.0 removes the cost that made it worth asking about:
paint *can* follow an asset (a shared default block), and a node only pays for its own block
once you actually paint it. **Duplicating a painted node shares its block** (free) until the
copy is painted — which is exactly the right default for laying out a scene of painted
props. Inspector gets **Copy Paint / Paste Paint** rows (phase 4), which are just `pbase`
assignments.

### 9.2 The `params.z` packing — **CONFIRMED**, now VS-confined

Kept, for the batching in §4.1 — but **only in the corrected form of §2.1**: decode in the
vertex shader, re-emit a clean `0/1`, leave `raster.wgsl:179` untouched. The naive version
of this packing silently unlit-ed every painted node and risked interpolation corruption of
the offset; the VS-confined version has neither failure mode and touches the fragment shader
not at all. **Phase 1 must implement the §2.1 form, not the obvious one.**

### 9.3 Sidecar — **ONE container file per scene**, not one per node

`<project>/paint/<scene>.vpaint` — a **single file holding every painted node's block**,
indexed by `paint_id`. The earlier `<scene>.<paint_id>.vpaint` sketch was wrong for the
target workload: an all-painted scene would mean **hundreds of tiny files**, hundreds of
`open`/`read` syscalls per scene load, and a directory nobody can read.

The container is deliberately **the GPU buffer, serialized**:

```
magic "FLVP" · version u16 · block_count u32
  index:  [ paint_id u32 · vertex_offset u32 · vertex_count u32 · geom_hash u64 ] × N
  data:   RGBA8 × total_vertices          // ← uploaded to `vpaint` verbatim
```

Load = read file → one `write_buffer` → assign each node's `pbase` from the index. No
per-node I/O, no repacking, no parse of the bulk. Save = dump. Shared blocks fall out for
free: two `paint_id`s with the same `vertex_offset`.

Scene-keying inherits the 2026-07-14 cross-scene overwrite bug class, so it **must** mirror
the terrain fix that landed for it: snapshot on Play, no saves during Play, restore the
scene name before adopt. Tracked as a phase-3 acceptance criterion, not a hope.

Allocation is **bump + free-list**, compacted on save/load (the container is written in
`vertex_offset` order, so load is inherently compact — fragmentation cannot accumulate
across sessions).

### 9.4 Prefabs — **share the prefab's block; fork on paint**

A painted prefab carries its block in a `<prefab>.vpaint` sidecar beside the `.prefab.ron`.
Every spawn — editor drag, `spawn(name, pos, fn)` from Lua — **points at the prefab's block**
(same `pbase`, refcounted). Spawn 200 painted barrels: **one block, one draw call, 4 B/vertex
total, not ×200.** Painting one forks it (§9.0). This is the case per-node keying handled
worst in the original draft and now handles best.

Refcount lives with the block; freeing a block is deferred to save-time compaction, so
runtime spawn/destroy never touches the allocator.

---

## 10. Using it

1. **Press `7`** (or pick 🖌 paint in the viewport toolbar). The 🖌 Paint tab focuses itself.
2. **Hover a mesh or primitive.** A magenta ring telegraphs where the dab lands.
3. **LMB-drag to paint.** Dabs are movement-spaced, so it strokes like a brush.
4. **Turn the material's `unlit` on** for the classic no-lighting look — and the fast path
   (§4.1: the unlit branch skips shadows, AO, point lights, specular and rim entirely).
5. **Ctrl+Z** undoes a whole stroke, not a dab.

Brush knobs worth knowing:

- **Hard falloff** — no gradient. This is *the* N64 look; a smooth-only brush cannot make it.
- **Channel mask (R/G/B/A)** — paint into one channel without disturbing the others. This is
  what makes paint usable as **shader data** rather than just color (phase 5 exposes it to
  `.flsl` as `in.vertexColor`).
- **Sample** — eyedropper. **Fill / Clear** act on the whole selected node.
- **White erases**: white is the identity for the albedo multiply, so painting white is
  visually identical to unpainted.

Paint is **per-node**: two cubes can be painted differently even though every cube in the
scene shares one `MeshId`. Duplicating a painted node **shares** its block for free and
**forks on the first dab** (§9.0), so copies never bleed into their original. Paint lives in
`<project>/paint/<scene>.vpaint`, ships with exports (the export copies the whole project
tree), and is **disabled during Play** — deliberately (§6.4).

## 11. Why this is worth building

Every prop, wall, and rock gets baked-in color variety with zero lighting cost, zero
lightmap bake, and zero shader authoring — and it composes with the machinery Floptle
already has: `unlit` for the flat classic look, `sdf_ao` bakeable *into* the vertices
(phase 6), the retro-res post chain quantizing it, and `in.vertexColor` promoting paint from
"cheap lighting" to a per-vertex mask channel any `.flsl` in the graph editor can read.

That last part is the argument. Most engines treat vertex color as a legacy compatibility
feature. Wired into Floptle's shader IR, it becomes a **per-vertex data channel you author
with a brush** — which is exactly the "flexible art design without a lot of work" this
engine exists to deliver.
