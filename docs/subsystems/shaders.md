# Floptle — Shader IR (`floptle-shader`)

> One shader, one source of truth — editable as a node graph *and* as readable
> text (`.flsl`), transpiled to WGSL. See
> [`../decisions/0007-shader-ir.md`](../decisions/0007-shader-ir.md),
> the "Open in VSCode" workflow
> [`../decisions/0011-vscode-integration.md`](../decisions/0011-vscode-integration.md),
> [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §5, the renderer in
> [`./renderer.md`](./renderer.md), and materials in
> [`./materials-and-textures.md`](./materials-and-textures.md).

> **STATUS (2026-07-15): text core SHIPPED — phases 1–4 of
> [`../shader-system-proposal.md`](../shader-system-proposal.md)** (which
> supersedes this pre-spec's integration details). What's live:
> `floptle-shader` (IR arena + checker, round-trippable `.flsl` parse/print,
> WGSL transpile, naga validation with `.flsl` line mapping, stdlib v1);
> **Fragment stage** — `Material.shader` names a `.flsl`, one pipeline per
> shader with a generated group(3) param UBO + up to 8 texture slots, drawn
> beside the built-in look (which stays byte-identical when unused); **Sdf
> stage** — a **Field Shape** node's shader IS its geometry, spliced into
> `map_d`/`map` (renders, casts/receives shadows + AO, up to 4 per scene,
> visual-only until the CPU evaluator); the **tiling block** (UV
> count/offset/rotation + triplanar, per binding, both paths); Inspector rows
> generated from `uniform`/`texture` declarations; mtime hot reload with
> last-good-pipeline fallback; `.flsl` syntax highlighting, live squiggles and
> a stdlib Docs section in the Scripting tab; `◈ New Shader` in Assets.
> Divergences from this pre-spec: stdlib identifiers are **camelCase**, the
> stage is named `sdf` (not raymarch), and `Vertex`/light/post stages are
> reserved (proposal §9). Probes: `shader_probe`, `field_shape_probe`.
>
> **Phase 5 — the ◈ Shaders GRAPH EDITOR — is live too** (this doc's §2
> two-view diagram, realized): a pan/zoom node canvas (`floptle-shader::graph`
> projects the same IR; the editor's `shader_graph.rs` renders it). Every
> call/operator is a node, literal args edit inline on the port, named `let`s
> keep their names, and anything anonymous is promoted to a `let` the moment
> you drag it. Wires type-check on connect (a bad wire bounces with the
> checker's message); node positions ride the `//@layout` trailing annotation
> (lets by name, sources as `in.uv` / `u.speed` / `tex.slot`, the sink as
> `out`). Right-click = a searchable palette built from the stdlib registry +
> knobs (uniforms), texture slots, constants, combine/split and inputs. Edits
> re-print the `.flsl` to disk (graph-local undo), the Scripting-tab buffer
> follows, external text edits re-sync the graph by mtime, and hot reload
> shows every change live in the scene. Double-clicking a `.flsl` opens the
> graph; `</>` jumps to text. Navigation is the node-editor standard (wheel
> zooms about the pointer, middle-drag pans, left-drag box-selects) with full
> multi-select: ctrl/shift-click, group move, group delete, Ctrl+D duplicate
> (intra-selection wires follow the copies). Positions are STABLE — layout
> entries plus a session cache keyed by two reparse-stable identities mean a
> node only ever moves when dragged. Eighteen commented example shaders
> (`floptle-shader::examples`, incl. `water.flsl` with fieldDistance shoreline
> foam) seed into `shaders/examples/` per project (missing ones fill in on
> open; deleting the folder opts out) and are compile-tested against the REAL
> pass sources. Eight of them are `stage sky` skyboxes — dayBreeze /
> sunsetStreaks / stormNight (per-cycle randomized lightning) / starryNight
> (wheeling worley star field + milky way) / moonlitClouds / auroraVeil /
> retroSun (synthwave grid floor) / nebulaDream — all animated (scrolling
> decks, twinkle, sway, hue drift), all verified by
> `sky_examples_probe` (a contact sheet at three times that also asserts each
> one moves).
>
> **LIVE PER-NODE PREVIEWS** (§6's "live preview", realized Unity-style):
> every node draws a thumbnail of its own value, updating in real time as the
> graph is edited. One generated WGSL module per shader
> (`floptle-shader::preview`) renders every tile into a grid atlas in a single
> pass — fragment values on a lit soft dome (floats grayscale, vec2/vec3 as
> color, vec4 alpha-composited over a checker; engine hooks get neutral
> stand-ins and `fieldDistance` a ground plane so foam/contact looks read),
> sdf values as the classic 2D distance cross-section (iso bands + zero
> line). Literal numbers ride a uniform lane array instead of being baked in,
> so DRAGGING any inline value or knob repaints thumbnails without a pipeline
> rebuild (the pipeline only rebuilds when the generated WGSL changes).
> Texture slots bind the textures of the first scene material using the
> shader (checker fallback). 👁 in the header toggles all previews;
> right-click a node to hide just its own. Auto-layout spaces columns for the
> thumbnail strip, and the header's ⇅ **Arrange** re-lays-out the whole graph
> (one undoable commit) — the fix for graphs saved before previews existed.
> Editor side: `shader_preview.rs`;
> probe: `preview_tiles_probe`. Also fixed: adding a knob/texture slot from
> the palette now shows its node immediately (the view includes DECLARED
> uniforms/textures, not just referenced ones), and the criss-cross triangle
> glitch on `.flsl` materials — the Plane primitive's coplanar double face
> z-fighting itself — is gone (single-face plane; all fragment paths flip the
> shading normal toward the viewer via `facing_normal`; probe:
> `flsl_prepass_probe`).

This is Floptle's biggest lever for visuals nobody else can make (see
[VISION](../VISION.md) §4.2). We *own the representation*, so we can add
non-standard nodes — raymarch/SDF warps, feedback, impossible color transport —
that drive the otherworldly look. We start with a usable subset and grow the
stdlib.

## 1. The IR is the single source of truth

A shader is a graph of **nodes** connected by **edges** through **typed ports**.
Every authoring view is a projection of this one structure; nothing lives only in
the graph or only in the text.

```rust
enum PortType { Float, Vec2, Vec3, Vec4, Color, Sampler, Sdf }

struct Port { name: String, ty: PortType }

struct Node {
    id:     NodeId,
    op:     OpKind,                 // "noise.fbm", "sdf.mandelbox", "color.palette", ...
    params: BTreeMap<String, Const>,// inline constants (RON-serialized)
    inputs: Vec<Port>,
    outputs: Vec<Port>,
}

struct Edge { from: (NodeId, PortIdx), to: (NodeId, PortIdx) }

struct ShaderIr {
    stage:    Stage,                // Fragment | Vertex | Raymarch
    uniforms: Vec<Uniform>,         // time, params, exposed material knobs
    inputs:   Vec<VertexInput>,     // uv, world_pos, normal, ...
    nodes:    Vec<Node>,
    edges:    Vec<Edge>,
    output:   NodeId,               // the single Output node (sink)
}
```

The graph is a **DAG** with one **Output** node as the sink. Type-checking is
edge-level: a `Color` output may feed a `Vec4` input (widening), an `Sdf` port
only connects to SDF-aware inputs. The IR is RON-serialized like everything else
authored in Floptle.

## 2. Two synchronized views

```
   ┌─────────────┐   print (.flsl)   ┌──────────────┐
   │  NODE GRAPH │ ────────────────▶ │  .flsl  TEXT │
   │  (in editor)│ ◀──────────────── │  (in VSCode) │
   └──────┬──────┘   parse (.flsl)   └──────┬───────┘
          │                                 │
          └──────────────┬──────────────────┘
                   same  ShaderIr
                         │
                     transpile → WGSL → naga validate → renderer
```

Both views are lossless projections of `ShaderIr`. Press **Open in VSCode**
(ADR-0011: `code <projectRoot> --goto <file>.flsl`) and the graph is *printed* to
`.flsl`; edit and save and it's *parsed* back into the identical IR. Round-trip
is structural, not textual: we print from the IR, parse into the IR, and the
graph re-lays out — so AI/manual text edits and graph edits are interchangeable.

### A small `.flsl` example

A swirling, palette-cycled plasma over UV space:

```flsl
shader plasma {
  stage fragment
  uniform time: float
  in uv: vec2

  let warped = domain_warp(uv, scale: 3.0, time: time);     // space-melt
  let n      = fbm(warped, octaves: 5);                      // noise field
  let hue    = hue_shift(palette(n, "sunset"), time * 0.1);  // nostalgic cycle
  let final  = posterize(hue, steps: 6);                     // retro quantize

  output color = final;
}
```

`let` bindings are nodes; named arguments (`scale:`, `octaves:`) are params or
edges; `output` is the sink node. The printer emits this from the graph; the
parser rebuilds the graph from it. Both reduce to the same `ShaderIr`.

## 3. Transpile to WGSL

`ShaderIr` → **WGSL** in one pass: topo-sort the DAG, emit each node's WGSL
snippet binding its inputs to upstream SSA temporaries, declare uniforms/inputs,
write the Output node to the stage's return. The result is handed to **naga** for
validation (ADR-0002 ships it) before the renderer builds a pipeline. naga errors
map back to node ids / `.flsl` lines for in-editor diagnostics.

```
ShaderIr ─▶ topo sort ─▶ emit WGSL per node ─▶ naga validate ─▶ pipeline/material
                                                    │
                                              errors → node id / .flsl:line
```

Raymarch-stage shaders emit a `map(p, t)` distance function consumed by the
renderer's raymarch pass ([`./renderer.md`](./renderer.md) §3) rather than a
fragment color — same IR, different output contract.

## 4. Stdlib node categories

Start small (the ADR-0007 subset), grow over time. Categories, with examples:

- **Inputs / uniforms** — `time`, `uv`, `world_pos`, `normal`, `camera_pos`,
  `camera_dir`, `resolution`, plus material-exposed knobs.
- **Math / vector ops** — `add/mul/mix`, `dot/cross/normalize`, `length`,
  `clamp/smoothstep`, `sin/cos`, swizzles, splits/combines.
- **Noise** — `perlin`, `simplex`, `worley`, `fbm` (octaves/lacunarity/gain).
- **SDF** — primitives (`sphere`, `box`, `torus`) and fractals (`mandelbulb`,
  `mandelbox`, `menger`, `kleinian`); operators `union`/`subtract`/`intersect`,
  `smooth_min`, and **domain warps** (`twist`, `bend`, `repeat`, `domain_warp`).
  These let shaders **author raymarched looks**, feeding §5's raymarch hook.
- **Color** — `hue_shift`, `palette` (named/LUT), `posterize`, `gamma`,
  `contrast`, `to_hsv`/`from_hsv`.
- **Texture sampling** — `sample(tex, uv)` honoring the material's **tiling
  options** (repeat/clamp/flip/count) so "drag on and tile" needs no shader edit;
  see [`./materials-and-textures.md`](./materials-and-textures.md).

### 4.1 Hooks into the raymarch pass

A `Raymarch` output node packages an `Sdf` graph as the `map()` function the
renderer marches, plus optional shade/normal hooks. This is how a shader authored
in the editor becomes a fly-through fractal: the SDF subgraph *is* the world.

```flsl
shader inside_box {
  stage raymarch
  uniform time: float

  let f = smooth_min(mandelbox(world_pos, power: 8.0, time: time),
                     sphere(world_pos, r: 2.0), k: 0.3);
  output sdf = f;          // becomes map(p, t) for the raymarch pass
}
```

## 5. Materials reference a compiled shader

A **material** binds a compiled shader plus its **param block** (uniform values,
texture slots, tiling). Many materials can share one shader with different params;
changing a param is a uniform write, not a recompile. Full design and the RON
shape live in [`./materials-and-textures.md`](./materials-and-textures.md).

```
material "lava.ron" ─▶ shader "plasma.flsl" (compiled WGSL)
                    └▶ params { time: <driven>, palette: "sunset", tex: lava.png ×3 }
```

## 6. Editor UX

- **Graph editor** — drag nodes from a categorized palette, wire typed ports
  (type-checked, colored by `PortType`), edit inline constants. Same dark /
  high-contrast / retro theme as the rest of the editor (VISION §6).
- **Open in VSCode** — a button at the top of the graph prints the current IR to
  `.flsl` and opens it in VSCode at the project root (ADR-0011). Edit with AI or
  by hand; on save the graph re-syncs from the file.
- **Live preview** — a preview viewport (quad, mesh, or a small raymarch volume)
  recompiles on edit and shows the result immediately; naga errors surface inline
  on the offending node / `.flsl` line.

## 7. Out of scope (day one)

- Full **GLSL/HLSL feature parity** — we ship a usable subset and grow the stdlib.
- Geometry/tessellation/mesh shaders, arbitrary compute kernels authored in-graph
  (the renderer's compute passes are hand-written for now).
- Multi-pass shader *graphs* spanning render targets — that's the render graph's
  job ([`./renderer.md`](./renderer.md) §1), not a single shader's.

If a node doesn't help make something nobody's seen, it waits.
