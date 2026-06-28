# Architecture Decision Records

Each ADR captures one significant decision: the context, what we chose, *why*,
what we rejected, and the consequences. They're short on purpose. When a decision
changes, we don't delete the ADR — we add a new one that supersedes it, so the
reasoning history stays intact.

| # | Decision | Status |
|---|---|---|
| [0001](0001-language-rust.md) | Core language: **Rust** | Accepted |
| [0002](0002-render-backend-wgpu.md) | GPU portability: **wgpu** (not raw Vulkan) | Accepted |
| [0003](0003-scripting-lua.md) | Game scripting: **Lua (LuaJIT/mlua)** | Accepted |
| [0004](0004-editor-egui.md) | Editor UI: **egui + egui_dock** | Accepted |
| [0005](0005-scene-model-ecs-node-hybrid.md) | Data model: **ECS core + Node facade** | Accepted |
| [0006](0006-asset-pipeline-gltf.md) | Blender pipeline: **glTF 2.0** | Accepted |
| [0007](0007-shader-ir.md) | Shaders: **custom IR, graph ⇄ text → WGSL** | Accepted |
| [0008](0008-object-pooling.md) | **Engine-native automatic object pooling** | Accepted |
| [0009](0009-license.md) | License & openness | Proposed |
| [0010](0010-temporary-assets.md) | Temporary OoT test assets handling | Accepted |
| [0011](0011-vscode-integration.md) | "Open in VSCode" workflow | Accepted |
| [0012](0012-physics-sdf-first.md) | Physics: **custom SDF-first** | Accepted |
| [0013](0013-deformable-matter.md) | **Unified deformable-matter substrate** (field + tiers) | Accepted |
| [0014](0014-gravity-fields.md) | **Mass/density-driven gravity as a field** | Accepted |
| [0015](0015-large-world-space.md) | **Large-world / floating-origin space** (default-on) | Accepted |
| [0016](0016-programmable-light.md) | **Programmable light transport** (light as a field) | Accepted |
| [0017](0017-time-as-a-field.md) | **Time as a rate field** (local clocks) | Accepted |
| [0018](0018-lawset-realm.md) | **The Lawset / Realm meta-spine** (a world's laws) | Accepted |
| [0019](0019-field-interaction-graph.md) | **Field-interaction graph** (composition by design) | Accepted |
| [0020](0020-fractal-shape-primitive.md) | **Fractal as a first-class shape primitive** (walkable/delvable, out of the box) | Accepted |

> Date format: ISO `YYYY-MM-DD`. Decider: Ty Johnston (Fopull LLC).
