# Floptle Shader System — Design Proposal

**Status:** ACCEPTED — **phases 1–5 SHIPPED 2026-07-15** (core / fragment
materials / tiling / Sdf stage; commits b676295, 5ce0b7b, e617c8c, fb23e37 —
then phase 5, the ◈ Shaders graph editor + the built-in example set).
Next: phase 6 (CPU parity → Field Shape collision)
**Author:** synthesis of ADR-0007 (shader IR, Accepted 2026-06-27), the 2026-06 pre-specs
(`docs/subsystems/shaders.md`, `docs/subsystems/materials-and-textures.md`), and a deep
research pass over the live workspace
**Scope:** the shader IR — data model, `.flsl` text format, WGSL transpile, material
integration, tiling, editor workflow, hot reload — and exactly where it plugs into the
renderer as it exists today
**Grounded against:** the live workspace as of 2026-07-14 — the `include_str!` WGSL set, the
`field.wgsl` concat seam, the per-instance-packed `Material` component, the `map`/`map_d`
mirror, the retro/post chain, the IDE tab, and the mtime-cache house patterns cited inline

---

## 1. Executive summary

**ADR-0007's decision stands unchanged**: one custom **shader IR** is the single source of
truth; the editor presents it as a node graph; "Open in VSCode" gives the *same shader* as
readable text (`.flsl`); either view round-trips; the IR transpiles to **WGSL validated by
naga**. We start with a usable stdlib subset and grow it. Nothing in the engine contradicts
that decision — what this proposal replaces is the *integration story*, which was written
against a blank renderer that no longer exists.

The big decisions, made and defended below:

1. **Text first, graph second.** The `.flsl` parser/printer and the WGSL transpiler are the
   core; they're provable headlessly (round-trip tests, golden WGSL, render probes) and they
   deliver AI-assisted authoring on day one through the already-working Open-in-VSCode flow
   (ADR-0011, `prefs.rs`). The node-graph editor — the first box-and-wire UI in the codebase —
   comes **after** the text/transpile core is proven in real materials (§10 phases).

2. **IR shaders are additive; the fixed-function path is the default and stays bit-identical.**
   Today's `Material` component (`floptle-core/src/material.rs`) is a mature Blinn-Phong block
   packed into the raster instance stream, edited everywhere, saved as `.ron` presets, reused
   by terrain and blobs. It does not go away and does not change cost. A material *may*
   reference a shader (§6); when none does, the compiled pipelines and the frame are exactly
   today's. This is the same zero-cost-when-off contract ADR-0016 set for light tiers.

3. **The fragment stage lands on the concat seam.** The engine already composes WGSL by
   concatenation — `raster.wgsl + field.wgsl` and `raymarch.wgsl + field.wgsl`
   (`raster.rs:221-226`, `raymarch.rs:301-302`). A Fragment-stage shader transpiles to one
   WGSL function with a fixed signature, concatenated into the raster module in place of a
   default stub, producing **one pipeline per shader**; instances are bucketed per pipeline
   exactly like today's opaque/transparent/texture buckets. Because `field.wgsl` rides along,
   custom surfaces keep SDF sun-shadows, AO, and fog **as stdlib nodes**, not reimplementations.

4. **Params become a real material uniform block at group(3).** The fixed-function material
   keeps riding the instance stream, but IR-shader params can't (they're shader-shaped, not
   engine-shaped). The transpiler lays out each shader's exposed uniforms into a `#[repr(C)]`
   param block + declared texture slots, bound as a generated **group(3)** bind group. A param
   edit is a uniform write — never a recompile. Many materials share one compiled shader.

5. **The Raymarch stage injects into *both* halves of the field mirror.** The scene SDF lives
   twice: the color-carrying `map` in `raymarch.wgsl` and the distance-only `map_d` in
   `field.wgsl` (which normals, shadows, AO, and *mesh* fragments march). An SDF-stage shader
   therefore emits **two functions from one graph** — `flslMapD` (distance) and `flslMapCol`
   (distance + color) — spliced into both files, `smin`-fused with terrain and blobs like
   everything else. Rendering ships first; **physics parity is an explicit, phase-gated
   follow-up** via a CPU interpreter over the same IR (§7.3) — until then custom SDF surfaces
   are render-only and say so in the Inspector.

6. **The tiling block finally ships, for both paths.** The pre-spec's promise — repeat count,
   offset, rotation, mirror-on-alternate, triplanar, *without writing a shader* — was never
   built (today's `TexSampling` covers filter + wrap only). It lands as a `Tiling` value on
   the material's texture binding, honored by the fixed-function raster shader (one extra
   instance vec4 pair) *and* by the IR stdlib's `sample()` node, so both worlds tile the same
   way (§8).

7. **`.flsl` text is the on-disk format.** One diffable, AI-editable file per shader is the
   asset; the IR is its in-memory form; graph node *positions* are cosmetic metadata in a
   trailing annotation block the parser ignores semantically (§4.3). No sidecar files, no
   RON-vs-text dual canon.

Crate layout: the IR, parser/printer, transpiler, and stdlib fill the existing
**`floptle-shader`** stub exactly along its planned module lines (`ir` / `text` /
`transpile` / `stdlib`, plus `graph` later). `naga = "29"` is un-commented in the workspace
`Cargo.toml` (it's already pinned there, waiting). Runtime pipeline plumbing lives in
`floptle-render`; the material DTO grows in `floptle-scene`; editor surfaces in
`floptle-editor`. No workspace rewiring.

---

## 2. Relation to the 2026-06 pre-spec

`docs/subsystems/shaders.md` and `docs/subsystems/materials-and-textures.md` remain the
record of the original design. This proposal keeps their heart — the IR, the two views, the
`.flsl` shape, the stdlib categories, the tiling model — and supersedes their integration
assumptions, which predate: the `Material` component and its per-instance packing, the
`field.wgsl` shared-module seam, the `map`/`map_d` mirror, retro mode, the post chain, the
depth prepass, floating-origin camera-relative space, and the editor's egui_dock reality.
Rewrite both subsystem docs to match once Phase 2 lands (the particle system set this
precedent).

Two pre-spec details are dropped deliberately:

- **"Materials name a shader from day one."** Materials name a shader *optionally*; the
  fixed-function block is the permanent default (decision 2 above).
- **snake_case stdlib names.** `.flsl` is user-facing surface like Lua, so stdlib
  identifiers are **camelCase** (`domainWarp`, `hueShift`, `smoothMin`, `worldPos`), matching
  the house rule. Rust internals stay snake_case.

---

## 3. The IR (`floptle-shader::ir`)

Unchanged in spirit from the pre-spec; stated here as the implementable contract.

```rust
enum PortType { Float, Vec2, Vec3, Vec4, Color, Texture, Sdf }

struct Node {
    id:      NodeId,
    op:      OpKind,                  // stdlib op ("noise.fbm", "color.palette", …)
    params:  BTreeMap<String, Const>, // inline constants
    inputs:  Vec<PortRef>,            // edge or constant per declared input
}

enum Stage { Fragment, Sdf }          // Vertex + Light + Post are future stages (§9)

struct ShaderIr {
    name:     String,
    stage:    Stage,
    uniforms: Vec<Uniform>,           // exposed knobs → the material param block (§6.2)
    textures: Vec<TexSlot>,           // named slots → material texture bindings (§6.2)
    nodes:    Vec<Node>,              // a DAG; edges are the PortRefs
    outputs:  BTreeMap<String, NodeId>, // stage-defined sinks (§5, §7)
}
```

- The graph is a **DAG**; validation = cycle check + edge-level type check (widening rules:
  `Float → VecN` splats, `Color ↔ Vec4` free; `Sdf` connects only to SDF-aware inputs).
- `Stage` ships with two variants, not three: the pre-spec's `Raymarch` is renamed **`Sdf`**
  (it describes what the shader *is*, not which pass consumes it — meshes march it too), and
  `Vertex` is deferred (§9). Adding a stage is additive.
- **Everything user-visible is camelCase**: op names in `.flsl`, uniform names, input names.
- The IR is *not* serialized as RON. Its durable form is `.flsl` text (§4); RON stays the
  format for scenes/materials/effects, text stays the format for *languages* (`.lua`, `.flsl`).

### 3.1 Stdlib (`floptle-shader::stdlib`)

Each op = a signature (typed inputs/outputs + params) and a WGSL snippet template. The
starting set, per ADR-0007's "usable subset":

- **Inputs** — `uv`, `worldPos` (**camera-relative** — floating origin, ADR-0015; document
  loudly), `objectPos` (**surface-locked** — object-local scaled to world units, the coord
  triplanar rides; use this for procedural detail that must STICK to the hull instead of
  swimming as the camera moves), `normal`, `viewDir`, `time`, `instanceColor` (the node's
  fixed material tint, so IR shaders compose with per-node tinting), `screenUv`.
- **Math** — arithmetic, `dot/cross/normalize/length`, `mix/clamp/smoothstep/step`,
  `sin/cos/pow/abs/floor/fract`, swizzle/split/combine.
- **Noise** — `valueNoise`, `simplex`, `worley`, `fbm(octaves, lacunarity, gain)`.
- **Color** — `hueShift`, `palette` (built-in ramps + LUT texture), `posterize`, `gamma`,
  `contrast`, `toHsv`/`fromHsv`.
- **Texture** — `sample(slot, uv)` honoring the binding's tiling block (§8); `sampleTriplanar`.
- **SDF** — `sphere/box/torus/plane`, `opUnion/opSubtract/opIntersect`, `smoothMin`,
  `twist/bend/repeat/domainWarp`; fractal estimators (`mandelbulb`, `menger`, `mandelbox`)
  arrive with ADR-0020, not before.
- **Engine lighting hooks** (the seam dividend, §5.2) — `litSurface(albedo, …)`,
  `sunShadow(p, n)`, `sdfAo(p, n)`, `applyFog(color, dist)`: thin wrappers over the functions
  `field.wgsl` already defines, available because the field module is concatenated into every
  material pipeline. Custom shaders participate in the engine's look instead of forking it.

Growth rule stays the ADR's: if a node doesn't help make something nobody's seen, it waits.

---

## 4. The `.flsl` text format (`floptle-shader::text`)

The pre-spec's shape survives intact — `let` bindings are nodes, named arguments are params
or edges, `output` is the sink — with camelCase stdlib:

```flsl
shader plasma {
  stage fragment
  uniform speed: float = 0.1        // exposed → material param block
  uniform tint: color = #E6E6F2
  texture ramp                      // exposed → material texture slot

  let warped = domainWarp(uv, scale: 3.0, time: time)
  let n      = fbm(warped, octaves: 5)
  let hue    = hueShift(palette(n, ramp), time * speed)

  output color = posterize(hue, steps: 6) * tint
}
```

### 4.1 Round-trip contract

`parse(print(ir)) == ir`, structurally. The printer is deterministic (topo order, stable
naming) so text diffs stay small; the parser produces exact source spans per node so naga
errors and type errors land on `.flsl` lines (§5.3) *and* on graph nodes (§10.2). Hand-written
text that parses is valid even if the printer would format it differently — formatting
normalizes on the next print, like `cargo fmt`.

### 4.2 Uniform/texture declarations are the material's schema

`uniform` and `texture` lines define what a material binding this shader exposes in the
Inspector: name, type, default, and (optional) `range(min, max)` annotation for slider
bounds. Re-declaring reshapes dependent materials' param blocks on recompile; removed params
are dropped with a Console warning, new ones take defaults.

### 4.3 Graph layout metadata

Node positions (and only positions) persist as a trailing annotation block:

```flsl
//@layout { warped: (120, 80), n: (320, 80), hue: (520, 96) }
```

The parser reads it into cosmetic metadata; semantic equality ignores it; hand-edits may drop
it freely (the graph auto-lays-out anything unplaced). One file, no sidecars, text edits and
graph edits stay interchangeable.

---

## 5. Transpile to WGSL (`floptle-shader::transpile`)

Topo-sort the DAG → emit each node's snippet as an SSA temporary → wrap in the stage's
function signature → prepend the generated param-block struct → **validate the assembled
module with naga 29** (the same naga wgpu 29 embeds, so validation-passed means
pipeline-creation-passes) → hand WGSL to `floptle-render`.

### 5.1 Fragment stage contract — the concat seam

`raster.wgsl` gains one seam: its lighting tail is factored so the material color/lighting
computation runs through a function with a fixed signature,

```wgsl
fn flsl_surface(in: SurfaceIn) -> vec4<f32>   // SurfaceIn: uv, world_pos, normal, view,
                                              // instance color/params, time
```

whose **default body is today's Blinn-Phong, verbatim** — the no-IR module concatenates
`raster.wgsl + field.wgsl + default_surface.wgsl` and must produce today's pixels
bit-for-bit (probe-asserted, §11). A Fragment shader replaces only that third chunk:

```
module(shader S) = raster.wgsl + field.wgsl + transpile(S)
```

One `wgpu::RenderPipeline` per compiled shader (opaque + transparent variants), created once
and cached by shader-source hash. The frame gather buckets instances by pipeline — the exact
mechanism `raster.rs` already uses for opaque/transparent/texture buckets — so draw order
stays: opaque buckets (built-in first, then per-shader), raymarch, transparent buckets.
Groups 0–2 (frame globals, base texture, shared field) are unchanged; custom-shader draws
additionally bind group(3) (§6.2). Retro mode and the post chain need **zero work** — custom
surfaces draw into the same targets at the same point in the frame, so they get retro-res,
SSAO, bloom, and outlines like every mesh.

### 5.2 What a Fragment shader can reach

Everything the concatenated module already sees: `Globals` (sun, points, ambient), the field
bindings at group(2) (hence `sunShadow`/`sdfAo`/`applyFog` as stdlib nodes), the instance
stream (tint, params), plus its own group(3). This is the payoff of landing on the house
seam instead of a parallel path: custom shaders are *inside* the engine's lighting world.

### 5.3 Errors

Type/cycle errors are caught in the IR with node ids + `.flsl` spans. naga validation errors
are mapped back through the emitter's span table to the offending node/line. Both surface in
the Console and inline in the IDE tab (Lua's red-squiggle infrastructure, extended). A shader
that fails keeps its **last good pipeline** running — hot reload never black-screens a scene.

---

## 6. Materials: one component, optional shader

### 6.1 The data model change

`floptle_core::Material` / `floptle_scene::MaterialDoc` grow, backwards-compatibly:

```rust
pub struct Material {
    // …every existing field, untouched…
    pub shader: Option<String>,                  // project-relative .flsl; None = built-in
    pub shader_params: BTreeMap<String, ParamVal>, // overrides for the shader's uniforms
    pub shader_textures: BTreeMap<String, TexBinding>, // slot name → texture + tiling (§8)
}
```

- `shader: None` (the default, and every existing scene/preset) = today's fixed-function
  path, byte-identical instance packing, zero new cost.
- `shader: Some(path)` = the compiled pipeline + group(3) path. The fixed-function fields
  stay meaningful where they compose (`instanceColor` input; alpha picks the opaque vs
  transparent pipeline variant).
- Existing `.ron` material presets, the Inspector section, copy/paste, and terrain/blob
  material reuse all keep working untouched. The Inspector's Material section gains a
  **Shader** row (dropdown of project + built-in `.flsl`, default "Built-in"); choosing one
  regenerates the params/texture rows below it from the shader's declarations (§4.2).

### 6.2 The param block — group(3)

The transpiler lays out each shader's `uniform`s into a std140-compatible block and each
`texture` slot into a texture+sampler pair; together they form a generated
`wgpu::BindGroupLayout` at **group(3)**:

```
binding 0            = params UBO   (the shader's uniforms, material-overridden)
binding 1, 2, …      = texture + sampler per declared slot (registry textures, §8 samplers)
```

Per *material* (not per shader): one small uniform buffer + one bind group, rebuilt only
when a binding changes; param edits are queued buffer writes. Many materials share one
shader's pipeline with different groups — the pre-spec's "param change is a uniform write,
not a recompile" holds by construction. Texture slots resolve through the existing
`texture_registry` path→`TexId` machinery; a missing texture falls back to the 1×1 white
like everywhere else.

**Cap for v1:** 8 texture slots per shader (comfortably under every backend's per-stage
sampler floor alongside groups 0–2). Lift later if a real shader hits it.

---

## 7. The Sdf stage — into the field mirror

### 7.1 The seam

Both field files gain a custom hook, default-stubbed to "nothing here":

```wgsl
// field.wgsl                                   // raymarch.wgsl
fn custom_d(p: vec3<f32>, t: f32) -> f32 {      fn custom_col(p: vec3<f32>, t: f32) -> Hit {
    return 1e9;                                     return Hit(1e9, vec3(0.0));
}                                               }
```

`map_d` becomes `smin(smin(analytic_d, volumes_d, k), custom_d(p, t), k)` and `map` does the
same on the color path — with the stub, `smin(x, 1e9, k) == x` and compiled output is
today's (probe-asserted). An Sdf-stage shader transpiles to **both functions from the one
graph**: the distance DAG emits `custom_d`, and the same DAG plus its color/material outputs
emits `custom_col`. Splice = the same concatenation trick: the stub chunk is replaced by
generated code and the raster + raymarch modules rebuild (pipelines re-created once, cached).

Because `custom_d` lives in `field.wgsl`, an IR-authored SDF automatically: casts and
receives sun shadows, occludes with AO, gets correct normals, fogs, and — the part no other
seam placement would give — **shadows the raster meshes standing next to it**.

### 7.2 How an Sdf shader enters a scene

A material whose shader is Sdf-stage can't sit on a mesh; it needs field presence. New
matter type in the modular Inspector: **Field Shape** — a node whose `Matter` is
`FieldShape { material }`, contributing its shader's SDF evaluated in the node's local frame
(position/rotation/scale fold into the generated code's input transform), with an authored
bounding radius for march bounding and shadow relevance (the same relevance machinery
proxies/volumes use). **v1 cap: 4 Field Shape nodes per scene**, each its own splice slot;
they smin-fuse with terrain and blobs. This is also exactly the slot ADR-0020's
`Fractal(Custom)` estimator plugs into later — the fractal primitive becomes "a built-in
Field Shape whose estimator ships with the engine."

The eventual **ADR-0016 hooks** (`light` rules, `bend` fields) are further outputs on this
same stage — declared now as future sinks (§3's `outputs` map), not built.

### 7.3 The CPU-eval question (physics parity) — decided, phase-gated

ADR-0012's promise is *one field, drawn and collided*. A GPU-only custom SDF breaks that:
the renderer shows a surface physics can't feel. Decision:

- **Phase 4 ships render-only.** `FieldShape` nodes don't collide; the Inspector labels the
  component "visual only (no collision yet)" so nobody is surprised. This matches how the
  engine already treats some visual-only constructs and keeps the render seam honest.
- **Phase 6 adds `floptle-shader::eval`** — a small interpreter that walks the same SDF DAG
  on the CPU in `f64`, giving `floptle-physics` a `dist(p)` closure per Field Shape (fused
  via the existing smin machinery). The IR is deliberately interpretable: pure ops, no
  texture fetches on the `Sdf`-typed path (`sample()` is fragment-only; SDF nodes are math).
  Same graph, two backends — the particle system's CPU-reference/GPU-committed split, again.
- The interpreter is also what makes `-∇f` gravity and ADR-0020's walkability probes work on
  custom fields later. It is a committed phase, not an escape hatch.

---

## 8. The tiling block (folded in, both paths)

The pre-spec's `Tiling` model ships as designed, attached to texture bindings:

```rust
enum Tiling { Uv(UvTransform), Triplanar(TriplanarProj) }

struct UvTransform {
    count:    [f32; 2],   // repeats across the UV span (4×4 …)
    offset:   [f32; 2],
    rotation: f32,        // degrees around UV center
    mirror:   bool,       // flip alternate repeats — the seam-hiding trick
}
struct TriplanarProj { scale: f32, blend: f32 }   // world tile size, edge sharpness
```

- **Fixed-function path:** `Material` (no shader) gains an optional `tiling` on its one
  texture. Data rides the instance stream — two added vec4s on `InstanceRaw`
  (count+offset, rotation+mode+blend+mirror) — and `raster.wgsl`'s sampling applies the UV
  transform / triplanar branch. Defaults encode "identity," keeping untouched scenes
  bit-identical. This alone pays for itself: it's the pre-spec's "drag on and tile, no
  shader" promise, usable before any IR exists — so it's **Phase 3, independent of and
  before the Sdf stage**.
- **IR path:** each `TexBinding` in `shader_textures` carries its own `Tiling`; the
  binding's transform params live in the group(3) params UBO and the stdlib `sample()` /
  `sampleTriplanar()` nodes apply them. Same struct, same Inspector widget, same defaults.
- **Inspector:** tiling controls sit under the texture row (mode radio, count/offset/
  rotation, mirror toggle), matching the pre-spec's mockup. Wrap/filter stay where they are
  (per-texture `TexSampling` in `.floptle/textures.ron`) — tiling is per-*binding*, sampling
  is per-*texture*, and the UI keeps them visually adjacent but distinct.

---

## 9. Deferred stages (contracts reserved, nothing built)

- **Vertex** — displacement in the material's vertex stage (renderer.md §4). Waits for a
  driving use case; the `Stage` enum and the raster seam make it additive.
- **Light** — ADR-0016 Tier 1/2 (`light L(…)` rules, `bend` fields) become additional
  Sdf-stage outputs; the WGSL proof harness comes first per that ADR.
- **Post** — "post effects authored in the IR" (renderer.md §5) waits until the post chain
  itself needs to be user-extensible; the built-in chain is not rewritten onto the IR.
- **Particles on IR materials** — `VfxRenderDoc` keeps texture-by-path; when a track wants a
  material reference, the billboard pass gains the same group(3) mechanics. Not before.

---

## 10. Editor & workflow

### 10.1 Text-era editor work (ships with Phase 2)

- **Asset browser:** `is_shader` = `*.flsl`; icon `◈` (violet), "New Shader…" in the folder
  menu seeding a commented starter shader. Compound-extension handling is already safe
  (the rename fix covers first-dot suffixes).
- **Open in VSCode:** already works (ADR-0011); `.flsl` just needs to route through the
  same open action scripts use.
- **IDE tab:** `.flsl` syntax highlighting (keyword/type/stdlib tables — the Lua
  highlighter's structure, new word lists), live parse + naga check on edit with red
  squiggles, stdlib autocomplete from the op registry, and a Docs page generated from stdlib
  signatures — the same treatment the Lua API gets.
- **Hot reload:** the mtime-cache house pattern (texture registry, `prefab_cache`): shader
  files are re-checked by mtime, recompile on change, pipelines swap on success, last-good
  stays on failure with Console + inline errors. No file watcher, no new dependency.
- **Inspector:** the Shader row + generated param/texture/tiling rows (§6.1, §8).

### 10.2 The graph editor (its own phase, after the core is proven)

The first box-and-wire canvas in the codebase — new UI territory (nearest precedents: the
particle curve editor's draggable keys/tangents, the mixer's EQ node graph, the timeline).
Scope for its first cut:

- Pan/zoom canvas; nodes from a categorized palette (the stdlib registry *is* the palette);
  typed ports colored by `PortType`; type-checked wiring; inline constant editing;
  box-select/drag/delete; layout persisted via §4.3.
- **Open in VSCode** button on the canvas (print → open; file save re-syncs the graph) —
  the ADR-0007 demo loop: build in graph, tweak a line as text, switch back, same shader.
- **Live preview** = the scene itself (materials hot-swap on edit), plus a small preview
  viewport (sphere/quad, headless-render machinery from the probe examples) for editing a
  shader no open scene uses.
- Errors pin to nodes (§5.3's span table in reverse).

Everything the graph needs from the core (spans, layout metadata, deterministic print,
stdlib introspection) is built in Phases 1–2 — the graph phase is UI work, not plumbing.

---

## 11. The optimization contract

- **Off = free, asserted.** With no IR materials in a scene, compiled WGSL is byte-identical
  to today's concatenation and the frame does zero extra work. A probe example
  (`shader_probe`, the `sky_probe`/`blob_probe` pattern) renders the golden scene both ways
  and diffs PNGs; the default-stub modules are also string-compared in a unit test.
- **Compile off the hot path.** Shader compiles happen on load/edit, never mid-frame draw;
  pipelines cache by source hash; a scene's shaders compile once at open.
- **Param edits are uniform writes.** Bind-group rebuilds only on texture rebinds.
- **Sdf shaders honor the march budget.** Generated `custom_d` code participates in the
  bounded-march machinery (bounding radius → relevance masks, same as volumes/proxies);
  `MAX_STEPS`/`EPS·t`/early-out are the pass's, not the shader's, to override.
- **Perf gate:** `perf_probe` numbers with 0 and 4 Field Shapes recorded before/after each
  phase, same as the render-perf pass.

---

## 12. What ships when — phased roadmap

1. **Phase 1 — the core, headless.** `floptle-shader`: IR + validation, `.flsl`
   parser/printer (round-trip tested), WGSL transpiler + naga validation, stdlib v1
   (inputs/math/noise/color/texture ops), golden-WGSL tests. No renderer changes. Demo: a
   CLI/unit test turns `plasma.flsl` into validated WGSL.
2. **Phase 2 — fragment materials end-to-end.** The `flsl_surface` seam + default stub
   (bit-identical probe), pipeline-per-shader buckets, group(3) param blocks,
   `Material.shader` + Inspector rows, asset classification, IDE highlighting + errors,
   mtime hot reload. Demo: the ADR-0007 demo — author a trippy material, open as text,
   tweak, watch it live-update on a mesh in-scene.
3. **Phase 3 — the tiling block.** `Tiling` on both paths (fixed-function instance vec4s +
   IR `sample()`), Inspector tiling controls, triplanar. Independent of Phase 4; delivers
   the oldest unkept promise in the docs.
4. **Phase 4 — the Sdf stage, render-only.** The `custom_d`/`custom_col` stubs + splice,
   `FieldShape` matter type (4-slot cap, bounding radius, "visual only" label), SDF stdlib
   ops. Demo: an authored `smoothMin(box, sphere)` shape sitting in a scene, shadowing the
   meshes around it.
5. **Phase 5 — the graph editor.** The canvas, palette, typed wires, layout persistence,
   preview, error pins (§10.2). Demo: graph ⇄ VSCode round trip on the same shader.
6. **Phase 6 — CPU parity + first downstream.** `floptle-shader::eval` interpreter →
   Field Shape collision through `floptle-physics`; walkability probes; from here ADR-0020's
   fractal primitive and ADR-0016's light hooks have their substrate.

Each phase is independently shippable and committable; nothing in an earlier phase is
throwaway for a later one.

---

## 13. Decisions log + open questions

**Proposed-and-decided in this document** (flag to reopen any):

- Text (`.flsl`) is the on-disk canon; graph layout is an in-file annotation (§4.3).
- Fixed-function `Material` is permanent; `shader` is an optional reference on it, not a new
  component (§6.1).
- Stage names: `Fragment` + `Sdf` now; `Vertex`/`Light`/`Post` reserved (§3, §9).
- camelCase for all `.flsl`-facing identifiers (§2).
- Sdf stage ships render-only; CPU interpreter is committed Phase 6, not optional (§7.3).
- Caps for v1: 8 texture slots per shader, 4 Field Shapes per scene (§6.2, §7.2).

**Open for Ty:**

1. **Phase 4 vs Phase 5 order** — Sdf stage before graph editor (as sequenced: more engine
   leverage first) or graph first (more daily-driver UX first)? The core doesn't care.
2. **Built-in shader set for Phase 2** — which worked examples ship? Proposed minimum:
   `palette_cycle.flsl`, `space_melt.flsl` (the surreal showcases), plus a commented
   `lit_textured.flsl` that reproduces the fixed-function look via `litSurface` as the
   learn-by-reading example.
3. **`.flsl` keyword surface** — happy with `shader/stage/uniform/texture/let/output`
   (§4's example) before the parser is written? Cheap to change now, annoying after.
