# Architecture Decision Records

Each ADR captures one significant decision: the context, what we chose, *why*,
what we rejected, and the consequences. They're short on purpose. When a decision
changes, we don't delete the ADR — we add a new one that supersedes it, so the
reasoning history stays intact.

| # | Decision | Status |
|---|---|---|
| [0001](0001-language-rust.md) | Core language: **Rust** | Accepted |
| [0002](0002-render-backend-wgpu.md) | GPU portability: **wgpu** (not raw Vulkan) | Accepted · awaiting final sign-off |
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

> Date format: ISO `YYYY-MM-DD`. Decider: Ty Johnston (Fopull LLC).
