# Floptle Terrain 2.0 — Meshed SDF Terrain

**Status:** proposal / implementation plan — **directional sign-off given by Ty 2026-07-16**
("large runtime-modifiable maps, triangulate at runtime, LOD by distance, lighting/AO like
meshes"); §9 lists the four knobs still open
**Author:** Fable synthesis, 2026-07-16 — grounded in a session spent *inside* this code:
the voxel-stretch root-cause (`terrain_dims`), the `grow()` Lipschitz fix, the measured
|∇d| audit of Ty's real 192 MB field, and the vertex-paint storage-buffer transport
**Scope:** replace terrain's *primary-ray rendering* (sphere-traced dense voxel SDF) with
**runtime-extracted chunk meshes** over a **sparse CPU field**, while keeping the SDF as
the authoring/physics/shadow substrate — sculpting, field-first sun shadows, and the
raymarched look for blobs/Field Shapes all stay

---

## 1. Executive summary

Terrain stops being *drawn* as math and starts being *drawn* as mesh — while staying
*authored, collided, and shadowed* as math.

The SDF voxel field remains the *single authority*: brushes write it, physics samples it,
sun shadows and contact AO march it. What changes is the camera's view of it: instead of
sphere-tracing the field per pixel through an f16 atlas, the engine **triangulates the
field into 32³-voxel chunk meshes on the CPU (surface nets), with per-vertex normals taken
from the field gradient**, and draws them through the existing instanced raster path —
the same path every mesh in the engine already uses. Chunks remesh when a brush dirties
them (that is the "modified at runtime"), and remesh at coarser strides with distance
(that is the LOD).

One sentence of architecture: **primary rays go raster; secondary rays stay SDF.**

Why this wins, in the order Ty asked:

1. **Large maps.** A dense grid is O(n³); Ty's current 433×406×460-unit terrain is
   **24.4M voxels / 192 MB**, and the 384-cell axis cap is the only thing stopping worse.
   A sparse chunked field stores only the narrow band around the surface — the same map
   is ~3–8 MB, and "growing" terrain means allocating a chunk, not reallocating the world
   (the entire `grow()`/`ensure_contains` bug class — including the Lipschitz corruption
   fixed yesterday — ceases to exist).
2. **The up-close glitches die by construction, not by tuning.** Every artifact Ty has
   photographed this week is a primary-ray artifact (§2.2 maps each one to its mechanism).
   A rasterized triangle with an interpolated gradient normal has none of them.
3. **Lighting/AO "like meshes" because it IS a mesh.** Terrain chunks are raster
   instances: they get Blinn-Phong, SSAO, the depth prepass, `.flsl` materials, and they
   *receive* field sun-shadows through the same `group(2)` binding every mesh already
   uses. Zero new lighting code.
4. **Runtime modification is the same loop the editor uses.** Brush (or Lua) writes
   voxels → dirty chunks → remesh → upload. No bake step, no split between edit-time and
   play-time terrain.

What is deliberately *kept*: the signature raymarched look for blobs and Field Shapes
(`.flsl` SDF matter), field-first sun shadows (ADR-0016/shadow system), SDF-first physics
(ADR-0012), the sculpt brushes and their UX, and the `BrushProfile` work from yesterday.

---

## 2. Why the current terrain cannot get there

### 2.1 The scaling wall (measured, not estimated)

| fact | value | source |
|---|---|---|
| Ty's active terrain | 289×271×307 voxels = 24.4M | `first.2.tfield`, read this session |
| CPU memory | **192 MB** (f32 distance + RGBA8 color, dense) | file size |
| undo cost | **192 MB per stroke** — `Snapshot::Terrain` snapshots the whole field | `terrain_edit.rs:279` |
| hard cap | 384 cells/axis (`MAX_DIM`) — at cubic 1.5-unit voxels that is **~576 units**, full stop | `ensure_contains` |
| GPU | R16Float 3D atlas, 16 slots shared by all volumes | `raymarch.rs:1193` |

Dense grids scale O(n³) but the *surface* — the only part anyone sees or edits — scales
O(n²). A 4×4 km map at 1.5-unit voxels is ~7M *surface-band* voxels but would be
**19 billion** dense. The representation is the wall; no parameter tuning moves it.

### 2.2 The up-close glitches, each with its mechanism

Everything Ty photographed is intrinsic to *sphere-tracing a trilinearly-filtered voxel
texture as primary visibility*:

1. **Shading faceting/wobble up close.** Normals are central differences of the
   trilinearly-interpolated field. Trilinear interpolation is C⁰ — its gradient is
   *discontinuous at every cell face* — so the normal visibly kinks on the voxel lattice
   no matter how cubic the cells are. This is the residual "weird up close" after the
   voxel-stretch fix. → Meshing computes one gradient **per vertex** on the CPU (f32,
   wider stencil, smoothed), and the GPU interpolates it across the triangle: smooth by
   construction, identical in kind to every imported mesh.
2. **f16 atlas quantization.** The render field is R16Float; the CPU field is f32. →
   Primary visibility stops reading the atlas at all.
3. **March/epsilon artifacts at grazing angles + bound-edge effects** (the `real_surface`
   shell-ring gotcha, box-edge speckle). → No primary march, no march artifacts. Bounds
   become chunk borders, which are interior mesh seams, not shading inputs.
4. **"AO acting weird."** SDF-AO cone-marches the same bumpy approximate field the
   normals come from, per pixel. → Terrain-as-mesh gets **SSAO from the depth buffer
   exactly like every mesh** (this is specifically what "AO like meshes" means), plus
   optional SDF contact AO from the coarse field where it is visually forgiving.
5. **Per-pixel cost.** Shadows/AO/primary march all scale with screen coverage ×
   march length. → Primary cost becomes vertex/raster cost, which the instanced path
   already handles at scale; secondary rays march a *coarse* field (§3.6).

### 2.3 What the SDF is still the best tool for

Sculpting (CSG-ish brushes on a field are *the* reason terrain feels good), volumetric
shapes (caves/overhangs — a heightmap is rejected for this reason, §8), soft sun shadows
without shadow maps, contact AO, and SDF-first physics. All stay on the field. The field
was never the problem; *pointing the camera directly at it* was.

---

## 3. Architecture

```
                   CPU (authority)                          GPU
        ┌───────────────────────────────┐
brush / │  SPARSE CHUNK FIELD           │   remesh    ┌──────────────────────┐
Lua ───▶│  HashMap<[i32;3], Chunk>      │────────────▶│ chunk meshes (arena) │──▶ raster.wgsl
        │  32³ voxels, narrow band      │  (surface   │ verts+idx+normals    │    (lit, SSAO,
        │  f32 dist + color             │   nets +    │ color: storage buf   │    prepass, flsl,
        │                               │   ∇ normals)│ per-chunk instances  │    RECEIVES field
        │        │ downsample           │             └──────────────────────┘    shadows @ group2)
        │        ▼                      │
        │  SHADOW CLIPMAP (coarse)      │   partial   ┌──────────────────────┐
        │  camera-centered volume       │────────────▶│ R16F atlas slot(s)   │──▶ field.wgsl
        │                               │   upload    │ flag = shadow/AO-only│    sun_shadow, sdf_ao
        └───────────────────────────────┘             └──────────────────────┘    (blobs still
                 │                                                                 raymarch here)
                 ▼
          floptle-physics (SDF queries, unchanged in kind)
```

### 3.1 Sparse chunk field (`floptle-field/src/chunks.rs`)

```rust
pub struct ChunkField {
    chunks: HashMap<[i32; 3], Chunk>,   // chunk coord → data; absent = uniform
    voxel: f32,                          // ONE cubic voxel edge, world units
    // uniform-region shortcuts: chunks not in the map are all-air; a solid
    // sentinel covers deep interior so hollow mountains cost nothing
}
pub struct Chunk {
    dist: Box<[f32; 32*32*32]>,          // f32 while resident (phase 5 quantizes)
    color: Box<[[u8; 4]; 32*32*32]>,
    dirty: bool,
}
```

- **32³ chunks** (decision defended: 16³ doubles chunk count and per-chunk overhead; 64³
  makes remesh granularity and LOD rings too coarse; 32³ ≈ 48 units at 1.5-unit voxels —
  a good brush-dirty granularity and a sub-millisecond remesh unit).
- Narrow band: writes clamp the stored field to ±(4 × voxel); chunks that end up uniform
  are dropped back to sentinels. The Lipschitz property is maintained *by the brushes*
  (already true) and **asserted by the existing `grow_keeps_the_field_1_lipschitz`-style
  test, ported to chunk writes** — that invariant caught real corruption once already;
  it becomes a permanent regression gate on every write path.
- `ensure_contains`, `grow`, `MAX_DIM`, and whole-field `to_bytes` **are deleted**.
  Unbounded terrain is now literally free: touching a new region allocates a chunk.
- Sampling API: `d(p)`, `grad(p)` (trilinear over chunk borders — readers never know
  chunks exist), `raycast(ro, rd)` (chunk-DDA + sphere trace inside chunks — replaces the
  dense raycast the brush uses today), `write_brush(...)` returning dirty chunk coords.

### 3.2 Meshing: naive surface nets (`floptle-field/src/mesher.rs`)

**Surface nets over marching cubes / dual contouring**, decided:

- One vertex per surface cell (positioned at the mean of edge crossings) → smooth,
  low-poly output that fits both the sculpted-organic content and the retro budget; MC
  emits 2–5× the triangles for the same field and its tables are a maintenance tax.
- Dual contouring's QEF machinery buys *sharp features* — smin-blended sculpted terrain
  has none. Rejected as complexity without payoff (revisit only if hard-edged CSG
  stamps become a feature).
- **Normals = CPU field gradient at each vertex** (central differences on the f32 field,
  1-voxel stencil), *not* face normals — this single choice is what makes terrain shading
  match imported-mesh shading, and it directly retires glitch §2.2-1.
- Each chunk meshes its 31³ interior cells and samples **1 voxel of apron** from
  neighbor chunks via the border-transparent sampling API — vertices and normals agree
  exactly at chunk seams (trap T3, §7).
- Acceptance (unit-tested, no GPU): mesh an analytic sphere field → every vertex within
  0.3 voxel of the true surface; every interior edge shared by exactly 2 triangles
  (watertight); vertex normals within 5° of analytic; remesh of one 32³ chunk of rolling
  terrain **< 1 ms** in release (measured, asserted — the paint-brush freeze taught us
  perf tests must use realistic data and real budgets).

### 3.3 LOD by distance

- LOD ℓ meshes a chunk sampling the field at stride 2^ℓ (the field API already resamples
  arbitrarily; no extra storage). Ring radii in *chunks* from the camera, e.g. lod0 ≤ 4,
  lod1 ≤ 10, lod2 ≤ 24, lod3 beyond; **hysteresis of ±1 chunk** so ring crossings don't
  thrash remeshing.
- Cracks between neighboring LODs: **skirts** (a one-voxel downward apron around each
  chunk mesh), not transvoxel stitching — skirts are ~30 lines, transvoxel is ~1500, and
  under this engine's aesthetic (and fog/retro) skirt seams are invisible. Upgrade path
  noted, not built.
- Remesh scheduling: a priority queue (dirty-by-brush first, then LOD-ring changes,
  nearest first), drained N chunks/frame on a worker thread (`std::thread` + channel —
  same shape as the audio decode worker); results land as buffer uploads on the main
  thread. The chunk being actively brushed remeshes *synchronously* so sculpting feels
  instant (it is < 1 ms by the §3.2 budget).

### 3.4 GPU residency: a chunk-mesh arena in `Raster`

`Raster::register` appends forever and `update_mesh_vertices` requires an identical
vertex count — neither fits meshes that churn. Add a **fixed-capacity arena**:

- `register_dynamic(capacity_verts, capacity_indices) -> DynMeshId` and
  `replace_dynamic(id, &MeshData)` (writes within capacity; grows by realloc + bind-group
  rebuild only on overflow, logged). Per-chunk capacity ~8k verts / 24k indices covers
  surface nets on 31³ cells with generous headroom; steady-state sculpting allocates
  nothing.
- Freed chunk slots go to a free-list (terrain is the first *dynamic* mesh citizen; the
  arena is deliberately general — CPU-skinned meshes could migrate later).
- Draw path: chunk instances enter the same instance buckets as every mesh (one
  `InstanceRaw` per chunk, camera-relative model matrix from the terrain node transform —
  floating-origin correct because chunk-local vertex coordinates stay small; ADR-0015).

### 3.5 Color and materials

- Terrain color stays per-voxel in the chunk field; the mesher emits **per-vertex color**
  sampled at each vertex. Transport: the **vertex-paint storage-buffer pattern shipped
  this week** — but through its own arena-managed buffer (`binding 9` beside `vpaint`),
  NOT the user-paint store: `vpaint` is bump-allocated and never freed, and remeshing
  churns blocks (leak trap T5). Slots in the terrain color buffer are paired 1:1 with
  arena slots, so alloc/free is the same free-list. (`raster.wgsl` reads it exactly like
  `vpaint` — a second base offset packed the same way; the fragment multiply already
  exists.)
- Phase-1 look: painted vertex color × optional single triplanar texture via the existing
  `Material` tiling — already more capable than today's flat-color + one-slot look.
- **Texture splatting** (the 16-slot palette blended per-vertex) is its own phase: a
  terrain `.flsl` (or small raster variant) reading per-vertex slot indices/weights from
  the same storage buffer and blending 2–4 palette layers triplanar. The palette,
  per-slot nearest-filter mask, and `textures.ron` plumbing from this week carry over
  unchanged.

### 3.6 Shadows and AO: the field keeps its crown

This is the part that makes the hybrid elegant rather than a rewrite:

- Terrain volumes flip from render (`vol_center.w = 1`) to **shadow-only (`w = 2`) — a
  mechanism that already exists** (built for baked level-mesh occluders). The raymarch
  pass stops drawing terrain; `sun_shadow` keeps marching it; every raster mesh —
  *including the new terrain chunks* — keeps receiving those shadows through `group(2)`.
  Phase 2 is small because of this.
- One shader nuance (trap T2): `map_d` (the AO field) deliberately *skips* shadow-only
  volumes, so props would lose SDF contact-darkening against terrain. Add flag **`w = 3`
  = shadow **and** AO, not render** — a two-line change in `field.wgsl`'s volume filters;
  terrain uses it.
- **Large maps:** the atlas cannot hold a 4 km field at render resolution — and no longer
  needs to. Phase 5 replaces per-terrain volumes with a **shadow clipmap**: one
  camera-centered volume (~192³ at 3–6-unit voxels), downsampled from the sparse field,
  re-centered + partially re-uploaded when the camera moves a threshold (the partial
  3D-upload path from `terrain_region_dirty` already exists). Soft sun shadows are
  visually forgiving of coarse fields; primary visibility — the unforgiving part — no
  longer touches it.
- SSAO (default-on, screen-space, depth-based) covers terrain automatically once terrain
  writes the depth buffer like any mesh. This is the single biggest "AO acts weird" fix.
- **Blob↔terrain melt (§9-D):** blobs currently smin-fuse with terrain volumes in the
  primary march. With terrain meshed, a blob resting on terrain z-intersects like a mesh
  instead of melting. *Option kept open:* blobs still fuse against the coarse
  shadow-clipmap field in the raymarch pass — approximate melt, nearly free. Ty decides
  whether the melt look matters (§9).

### 3.7 Editing, undo, persistence

- Brush flow becomes: `write_brush` → dirty chunk set → sync-remesh the hit chunk,
  queue the rest → partial shadow-clipmap upload. `terrain_frame_update`'s ray comes from
  `ChunkField::raycast`. `BrushProfile`, spacing, telegraph, dab logic: untouched.
- **Undo: per-stroke chunk snapshots** — first dab lazily copies only the chunks the
  stroke touches (`Snapshot::TerrainChunks(id, Vec<([i32;3], ChunkData)>)`), byte-swap on
  undo exactly like `swap_terrain_bytes` today. A typical stroke touches 1–8 chunks ≈
  0.3–2.5 MB, versus **192 MB today**. This alone justifies the storage change.
- Persistence: `terrain/<scene>.<id>.chunks/` region files (8³ chunks per file,
  version-headered) or a single packfile — dealer's choice at implementation, but
  **keyed by scene + id exactly as today and carrying the same Play-gating** (the
  2026-07-14 cross-scene overwrite fix must be mirrored, not rediscovered).
  **Migration:** on load, a `.tfield` dense grid imports into chunks (narrow-band clamp,
  uniform-chunk drop) — one-time, lossless where it matters, old scenes just work.
- Runtime/Lua (phase 6): `terrain.sculpt(pos, radius, strength, mode)`,
  `terrain.paint(pos, radius, color)`, `terrain.height(x, z)`, `terrain.query(pos)` —
  camelCase, same write path as the editor brush, honoring the remesh queue. This is
  "modified at runtime" delivered to gameplay, not just the editor.

### 3.8 What stays raymarched

Blobs and Field Shapes (`.flsl` SDF matter) keep the full sphere-traced pipeline — that
*is* the engine's signature and it operates at object scale where none of §2's terrain
pathologies bite. The raymarch pass composes with raster via depth exactly as today.

---

## 4. Honest ledger

**Killed outright:** trilinear-normal faceting; f16 primary visibility; grazing-angle
march speckle; bound-edge shell rings; AO weirdness on terrain (SSAO takes over);
`grow()`/`ensure_contains` and its bug class; the 384-cell/576-unit map ceiling; 192 MB
undo steps; O(n³) memory.

**Changed, visibly:** blob↔terrain melt becomes optional/approximate (§3.6, §9-D);
terrain silhouettes become polygonal at lod boundaries instead of pixel-exact math
(mitigated by LOD density; in practice imported meshes already set the engine's
silhouette standard); a brushed edit shows ~0–2 frames of remesh latency on *neighboring*
chunks (the brushed chunk itself is synchronous).

**New costs:** the mesher + arena + LOD scheduler (~1.5–2.5k lines, unit-testable);
remesh CPU (budgeted, threaded, measured); a second storage buffer binding in raster.

---

## 5. Budgets (targets, asserted where testable)

| thing | target |
|---|---|
| chunk remesh (32³, release) | < 1 ms; brush chunk synchronous |
| sculpt visual latency | brushed chunk same-frame; neighbors ≤ 2 frames |
| active chunk meshes @ 600 u view | ~300–600 instances, 0.3–1 M tris total (LOD'd) |
| Ty's current map, CPU | 192 MB → **3–8 MB** sparse f32 (→ ~2 MB quantized, phase 5) |
| 4×4 km map, CPU resident | ~84 MB quantized narrow band + region streaming (phase 5) |
| undo per stroke | ≤ ~2.5 MB (touched chunks only) |
| shadow clipmap upload on recenter | partial, ≤ 8 MB worst case, amortized |

---

## 6. Phases (each lands green: clippy zero, tests, probe PNGs)

**P1 — Sparse field + mesher (pure `floptle-field`, no GPU).**
`chunks.rs` (ChunkField, sampling, raycast, brush writes, narrow band) + `mesher.rs`
(surface nets + gradient normals + skirts). Port brush ops from `terrain.rs`. Tests:
§3.2 acceptance + Lipschitz-on-write + border-sampling seam test + remesh perf on
realistic terrain. *Exit: mesh a sculpted field to OBJ-in-memory and assert properties.*

**P2 — Render swap, static.** Arena (`register_dynamic`/`replace_dynamic` + terrain
color buffer @ binding 9) in `Raster`; editor builds chunk meshes for terrain nodes at
load; terrain volumes flip to `w = 3` (add the flag); raymarch stops drawing terrain.
Probes: re-render `terrain_probe`/`closeup`/`grazing` scenes through the new path — the
grazing/closeup shots that motivated this doc become the before/after evidence. *Exit:
parity screenshots + editor runs clean on `first.ron` via `.tfield` migration.*

**P3 — Live editing + undo + persistence.** Dirty-chunk remesh loop, sync brush chunk;
`Snapshot::TerrainChunks`; region-file save/load + migration; delete dense paths
(`grow`, `ensure_contains`, whole-field snapshots). *Exit: sculpt feels identical in
editor; undo memory measured; scene round-trips.*

**P4 — LOD + async.** Stride rings, hysteresis, skirts at LOD seams, worker thread +
budget queue. *Exit: 600-unit view over large terrain at frame budget; no crack pixels
in probe sweep; ring-crossing thrash test.*

**P5 — Large-map storage + shadow clipmap.** i8-quantized narrow band (dist in
1/16-voxel units) + palette-indexed color (~3 B/voxel); region streaming
(load-on-approach, evict-clean); camera-centered shadow clipmap replacing per-terrain
shadow volumes. *Exit: a 4 km test map sculptable end-to-end within the memory table.*

**P6 — Runtime & polish.** Lua terrain API; splat-texture terrain shader (palette blend);
optional blob-melt-vs-clipmap; docs (`docs/subsystems/terrain.md` rewrite) + ADR.

P2 is the moment Ty's complaint dies; P4 is "large maps feel real"; P5 is "large maps
are real."

---

## 7. Traps for the implementer (each cost someone a day once already)

- **T1 — 16/16 vertex attributes are FULL.** Terrain color/splat data must ride storage
  buffers indexed by `vertex_index` (the vpaint pattern), never a new attribute. If
  `VsOut` changes, `TEST_PRELUDE` changes in lockstep.
- **T2 — `map_d` skips shadow-only volumes.** Use the new `w = 3` for terrain or props
  lose contact AO. Check `real_surface` too (its strict terrain-box test was a past
  gotcha — it can simply stop special-casing terrain once terrain leaves the march).
- **T3 — chunk aprons.** Vertices on a chunk's +faces need neighbor samples; normals need
  a further ring. Sample through `ChunkField` (border-transparent), never chunk-local
  arrays, or seams shade visibly.
- **T4 — remesh threading.** Field writes happen on the main thread mid-frame; the worker
  snapshots the chunks it needs (copy-on-queue) rather than locking the field. Uploads
  only on the main thread (wgpu queue discipline as used everywhere).
- **T5 — do not put terrain colors in the user `vpaint` store** — it never frees;
  remeshing would leak it. Arena-paired buffer with a free-list.
- **T6 — undo/Play gates.** `record()`/`push_history` no-op during Play; scene-name-keyed
  files need the 2026-07-14 mitigations mirrored.
- **T7 — perf tests on real shapes.** The paint-freeze lesson: synthetic quads pass while
  real meshes hang. The mesher/LOD perf tests must run on sculpted fields (port the
  probe terrain), asserted in ms.
- **T8 — keep clippy at zero and verify with probe PNGs** (blob_probe discipline), not
  by reasoning about shaders.

---

## 8. Alternatives rejected

- **Heightmap terrain** — no caves/overhangs/volumes; collides with the engine's whole
  volumetric-surreal identity and ADR-0012/0013. Rejected outright.
- **Fix the raymarch instead** (f32 atlas, tricubic filtering, smoothed gradient
  normals): tricubic is 8× texture cost per sample in the hottest loop; memory still
  O(n³); the map ceiling and AO complaints remain. Some pieces (better normals) are
  salvage for *blobs* someday, but this path cannot reach "large maps."
- **GPU compute meshing** — attractive later (the particle system already commits to a
  compute phase), but the engine has no compute infra today and CPU surface nets at
  < 1 ms/chunk doesn't need it. Deferred, not rejected; the chunk/arena architecture is
  compute-ready (same buffers, different producer).
- **Marching cubes + transvoxel** — 2–5× triangle count, big tables, 1.5k lines of seam
  logic; surface nets + skirts delivers the same visual class for this content at a
  fraction of the complexity.

---

## 9. Open decisions for Ty (defaults chosen; overridable)

- **A. Default voxel density** — default **1.5 units** (matches current cubic fix;
  4 km maps fit budget). 1.0-unit density doubles fidelity at ~2.25× band memory. The
  detail slider becomes *units-per-voxel directly* (an honest density knob at last).
- **B. Splat textures now or later** — plan says **later (P6)**: vertex color + one
  triplanar material at P2 already beats today's look; splatting is additive.
- **C. Legacy raymarched terrain** — plan says **hard-cut at P3** once parity probes
  pass (keep `.tfield` import forever). A dual render path kept alive "just in case" is
  how engines rot.
- **D. Blob↔terrain melt** — accept crisp intersection, or fund the cheap
  approximate melt against the shadow clipmap in P6. Plan defaults to *accept crisp*,
  flag the option.

---

## 10. Why this is the right shape for Floptle specifically

The engine's identity is *fields you can touch* — sculptable matter, field shadows,
SDF physics. This plan doesn't retreat from that; it puts the field where fields are
strong (authoring, volume, secondary light) and puts triangles where sampling theory says
surfaces belong (primary visibility). Terrain inherits every mesh feature already built —
SSAO, prepass, `.flsl` materials, vertex-channel storage buffers, the whole shading model
— and every future mesh feature for free. And the sculpting loop, the part Ty actually
*uses*, doesn't change at all: it just stops lying to the camera about what it made.
