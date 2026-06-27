<!-- markdownlint-disable MD033 MD041 -->
<div align="center">

# Floptle

**A lightweight, hyperoptimized game engine for surreal, otherworldly visuals.**

*Make games that look like they're from another dimension — without anyone else's restrictions.*

— a [Fopull LLC](https://github.com/Fopull-LLC) project · private / pre-release —

</div>

---

> ⚠️ **Status: planning + scaffold.** No engine code yet — this repo currently
> holds the architecture, decisions, and design docs that define what Floptle
> *will* be, plus a compiling Rust workspace skeleton. Start with
> [`docs/VISION.md`](docs/VISION.md), then [`docs/ROADMAP.md`](docs/ROADMAP.md).

## What Floptle is

Floptle is a game engine built around one belief: the most interesting games
don't come from chasing photoreal graphics — they come from showing people
something they've **never seen**. Floptle's renderer is designed to bend the
conventional rules of light and geometry: real-time fractals you can fly *into*,
meshes whose vertices shift and breathe, and a shader system meant to produce
images that feel surreal, dreamlike, and a little nostalgic underneath.

It's also built to be a joy to *make* things in. The particle editor works like
a video editor's timeline. The shader system is a node graph **and** editable
text at the same time. UI is drag-drop-and-script. Object pooling is automatic.
Blender models drop straight in. Nothing is buried under a thousand properties.

## Pillars

1. **Otherworldly, not realistic.** Surreal/dreamlike visuals via SDF raymarching, custom shaders, and reality-bending post.
2. **Lightweight & fast.** Rust, no GC, data-oriented core, optimized by default.
3. **Cross-platform.** Linux, Windows, macOS from one codebase.
4. **Made for makers.** Opinionated, fast workflows for VFX, shaders, UI, and scenes — power without clutter.
5. **Yours.** The renderer, shader language, and tools are ours top to bottom.

## The stack (decided)

| Layer | Choice | ADR |
|---|---|---|
| Language | **Rust** (edition 2024) | [0001](docs/decisions/0001-language-rust.md) |
| Window/GPU portability | **winit + wgpu** | [0002](docs/decisions/0002-render-backend-wgpu.md) |
| Scripting | **Lua (LuaJIT via mlua)**, hot-reload | [0003](docs/decisions/0003-scripting-lua.md) |
| Editor UI | **egui + egui_dock** (dark/retro) | [0004](docs/decisions/0004-editor-egui.md) |
| Scene model | **ECS core + Node/Component facade** | [0005](docs/decisions/0005-scene-model-ecs-node-hybrid.md) |
| Blender pipeline | **glTF 2.0** | [0006](docs/decisions/0006-asset-pipeline-gltf.md) |
| Shaders | **Custom IR: graph ⇄ text → WGSL** | [0007](docs/decisions/0007-shader-ir.md) |
| Pooling | **Engine-native automatic pools** | [0008](docs/decisions/0008-object-pooling.md) |
| Serialization | **RON** | (see ARCHITECTURE) |
| Physics/collision | **Custom SDF-first** (collide morphing fractals) | [0012](docs/decisions/0012-physics-sdf-first.md) |
| Deformable matter | **Unified field substrate + opt-in tiers** | [0013](docs/decisions/0013-deformable-matter.md) |
| Gravity & space | **Mass/density gravity fields** (walk on fractals) | [0014](docs/decisions/0014-gravity-fields.md) |
| World scale | **Large-world / floating-origin** (default-on) | [0015](docs/decisions/0015-large-world-space.md) |
| Light | **Programmable light transport** (light as a field) | [0016](docs/decisions/0016-programmable-light.md) |
| Time | **Time as a rate field** (local clocks) | [0017](docs/decisions/0017-time-as-a-field.md) |
| World rules | **Lawset/Realm spine + field-interaction** | [0018](docs/decisions/0018-lawset-realm.md) · [0019](docs/decisions/0019-field-interaction-graph.md) |

## Repository layout

```
Floptle/
├─ Cargo.toml              # workspace
├─ crates/
│  ├─ floptle-core/        # math, ECS, node facade, events, time, pools
│  ├─ floptle-field/       # implicit geometry: SDFs + CSG/blend ops (shared)
│  ├─ floptle-rules/       # Lawset/Realm meta-spine + field-interaction graph
│  ├─ floptle-render/      # wgpu render graph + the signature look
│  ├─ floptle-shader/      # shader IR: graph ⇄ text ⇄ WGSL
│  ├─ floptle-vfx/         # timeline particle system
│  ├─ floptle-anim/        # skeletal + state-machine animation
│  ├─ floptle-physics/     # SDF-first collision + character/vehicle movement
│  ├─ floptle-matter/      # deformable matter: morph, soft-body, stick, fracture
│  ├─ floptle-input/       # action mapping (kbd/mouse/gamepad)
│  ├─ floptle-ui/          # in-game UI + dialogue
│  ├─ floptle-assets/      # glTF import, textures, materials, asset DB
│  ├─ floptle-script/      # Lua host + hot reload
│  ├─ floptle-net/         # networking (deferred stub)
│  ├─ floptle-editor/      # the authoring tool (bin: `floptle`)
│  ├─ floptle-runtime/     # game player / export host (bin)
│  └─ floptle-proof/       # Beat 1: standalone raymarch "proof slice" (bin)
├─ docs/
│  ├─ VISION.md            # the north star
│  ├─ ARCHITECTURE.md      # how it all fits together
│  ├─ ROADMAP.md           # phased build plan
│  ├─ decisions/           # ADRs (why each choice was made)
│  └─ subsystems/          # per-system deep-dive designs
└─ assets/                 # default textures/materials/shaders
```

## Building

```bash
# Requires the stable Rust toolchain (rust-toolchain.toml pins it).
cargo build           # compiles the (currently stub) workspace
cargo run -p floptle-editor   # prints the planning banner for now
```

## License & status

Pre-release and **private**. Intended to become free & open-source (with a
donations option) once it's ready and all temporary test assets are replaced.
See [`docs/decisions/0009-license.md`](docs/decisions/0009-license.md) and
[`docs/decisions/0010-temporary-assets.md`](docs/decisions/0010-temporary-assets.md).
