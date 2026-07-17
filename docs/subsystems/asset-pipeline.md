# Floptle — Assets & the Blender Pipeline (`floptle-assets`)

> Model in Blender, export glTF, drop it in — meshes, materials, skins, and
> animations land intact, get stable ids, and hot-reload on save. See
> [`../decisions/0006-asset-pipeline-gltf.md`](../decisions/0006-asset-pipeline-gltf.md),
> materials in [`./materials-and-textures.md`](./materials-and-textures.md),
> animation in [`./animation.md`](./animation.md), the editor's asset browser in
> [`./editor.md`](./editor.md), temp assets in
> [`../decisions/0010-temporary-assets.md`](../decisions/0010-temporary-assets.md),
> and [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §6.

`floptle-assets` is the bridge from *files on disk* to *engine resources on the
GPU and in the ECS*. It imports glTF, decodes textures, assigns stable ids, tracks
dependencies, and watches files so a save in Blender/VSCode/an image editor shows
up live in the running editor.

## 1. glTF 2.0 import (the Blender path)

Blender exports **glTF 2.0** (`.glb` preferred — single binary file) via its
native exporter ([ADR-0006](../decisions/0006-asset-pipeline-gltf.md)); we read it
with the `gltf` crate, which gives direct buffer access for fast upload.

**What we import:**

- **Meshes** — positions, indices, **UVs** (set 0), **normals** (computed if absent),
  and **vertex colors** (`COLOR_0` → `MeshData.colors`, the vertex-paint store — see
  [`../vertex-paint-proposal.md`](../vertex-paint-proposal.md)). Split per-primitive
  (per-material). *Tangents and UV sets beyond 0 are not imported yet.*
- **PBR materials** — base color, metallic/roughness, normal, emissive,
  occlusion. These **seed** Floptle materials ([`./materials-and-textures.md`](./materials-and-textures.md))
  which we then enrich; glTF PBR maps onto a built-in `lit_textured`-style shader.
- **Skins** — joints + inverse-bind matrices, for skeletal animation.
- **Animations** — TRS keyframe channels → clips consumed by `floptle-anim`
  ([`./animation.md`](./animation.md)).

```
Blender ──export .glb──▶ gltf crate ──▶ ImportedModel
                                          ├─ meshes[]      → GPU vertex/index buffers
                                          ├─ materials[]   → seed materials/*.ron
                                          ├─ textures[]    → decode + upload + mips
                                          ├─ skins[]       → skeleton (floptle-anim)
                                          └─ animations[]  → clips  (floptle-anim)
```

### Blender export workflow

Recommended export preset (a future one-click Blender add-on is noted in ADR-0006,
**not** required): glTF Binary `.glb`, +Y-up, apply modifiers, include selected
objects, export normals/tangents/UVs, include skinning + animations, sample
animations at scene FPS. Material name = the seed material name in Floptle.

### Material mapping

A glTF material becomes a Floptle `Material` ([`./materials-and-textures.md`](./materials-and-textures.md) §2):
PBR factors → shader params, PBR textures → texture bindings with default
`Repeat` tiling, double-sided flag → `cull: None`. From there it's a normal,
editable Floptle material — you can swap the shader for a surreal one.

## 2. The asset database

The heart of the pipeline: every asset gets a **stable GUID** that survives
renames and moves, so references never break.

```rust
struct AssetId(u128);                  // stable GUID, minted on first import

struct AssetEntry {
    id:       AssetId,
    path:     PathBuf,                 // project-relative source file
    kind:     AssetKind,              // Texture | Mesh | Material | Shader | Script | Scene | Vfx | Audio
    import:   ImportSettings,         // per-asset, RON sidecar (§2.1)
    deps:     Vec<AssetId>,           // what this asset references
    hash:     u64,                    // content hash, for change detection
    state:    LoadState,              // Unloaded | Loading | Ready | Failed
}

struct AssetDb {
    entries:  HashMap<AssetId, AssetEntry>,
    by_path:  HashMap<PathBuf, AssetId>,
    rdeps:    HashMap<AssetId, Vec<AssetId>>,  // reverse deps — who uses me
    watcher:  FileWatcher,            // notify-based
}
```

GUIDs live in a sidecar (`<file>.meta.ron`) committed alongside the asset, so the
same file imported on another machine keeps its id. References in scenes/materials
store the **`AssetId`**, not the path.

### 2.1 Per-asset import settings

A `.meta.ron` sidecar per asset records how to import it. Editing it (or its
panel) triggers a reimport.

```ron
// stone_albedo.png.meta.ron
ImportSettings(
    id: "b9f2...e1",                 // stable GUID
    kind: Texture,
    texture: (
        srgb: true,                  // color vs data (normal/roughness = false)
        generate_mips: true,
        format: Auto,                // Auto | Rgba8 | Bc7 | ...
        max_size: 2048,
    ),
)
```

### 2.2 Dependency graph

`deps`/`rdeps` form a DAG: a **scene** uses **prefabs** + **materials**; a
**material** uses a **shader** + **textures**; a **mesh** may seed materials.

```
scene.ron ─▶ prefab.ron ─▶ material.ron ─┬─▶ shader.flsl
                                          └─▶ texture.png
```

Reverse deps drive **targeted hot-reload**: changing `texture.png` only re-uploads
that texture and refreshes the materials that bind it — nothing else rebuilds.

### 2.3 Hot-reload on file change

A `notify`-based watcher diffs content hashes; on change we reimport the asset and
**propagate up the reverse-dependency edges**:

```
file saved ─▶ watcher ─▶ hash changed? ─▶ reimport asset
                                              │
                                  notify rdeps (materials, scenes…)
                                              │
                          texture: re-upload + mips     · material: rebuild bind group
                          .flsl:   re-transpile (naga)   · mesh: re-upload buffers
                          .lua:    re-bind script (floptle-script)
```

Editing in Blender (re-export `.glb`), VSCode (`.flsl`/`.lua`), or an image editor
all show up live in the running editor with no manual reimport.

## 3. Texture decode & GPU upload

Images decode via the `image` crate (PNG/JPG/etc.), then:

1. Convert to the import format (sRGB vs linear per `srgb`).
2. Upload to a wgpu texture; pooled staging buffers ([ADR-0008](../decisions/0008-object-pooling.md)).
3. **Generate mipmaps** (when `generate_mips`) — a small compute/blit downsample
   chain — so tiled/triplanar textures ([`./materials-and-textures.md`](./materials-and-textures.md))
   stay crisp at distance and don't shimmer.

KTX2/basis-compressed textures are a later optimization; PNG/JPG cover launch.

## 4. Project layout & export

A Floptle **project** is a directory of RON + assets ([ARCHITECTURE](../ARCHITECTURE.md) §8):

```
MyGame/
├─ project.ron              # settings, entry scene, asset-root config
├─ scenes/*.ron             # node trees
├─ prefabs/*.ron            # reusable node subtrees
├─ materials/*.ron          # shader + params + texture bindings
├─ vfx/*.ron                # particle effects
├─ shaders/*.flsl           # textual shader IR (also editable as graphs)
├─ scripts/*.lua            # Lua behavior
└─ assets/
   ├─ models/*.glb          # Blender exports
   └─ textures/*.png        # + *.meta.ron sidecars
```

**Export** packs this project with **`floptle-runtime`** into a standalone build
([ROADMAP](../ROADMAP.md) Phase 9): collect the dependency closure from the entry
scene, drop editor-only data, optionally pack assets into a single archive next to
the runtime binary, and emit per-OS bundles (Linux/Windows/macOS).

```
project dir ─▶ walk deps from entry scene ─▶ strip editor data
            ─▶ pack assets + RON ─▶ link with floptle-runtime ─▶ platform bundle
```

## 5. Temporary OoT test textures

Per [ADR-0010](../decisions/0010-temporary-assets.md): Ocarina of Time textures
are **local-only placeholders** under `assets/textures/_oot_temp/`, which is
**git-ignored**. They are never committed (keeps history clean for the future OSS
release) and are **replaced with original Fopull art before any release**
(a Phase-10 gate, [ROADMAP](../ROADMAP.md)). The engine must never hard-depend on
those files — built-in defaults ([`./materials-and-textures.md`](./materials-and-textures.md) §4)
cover real content. A "drop test textures here" note lives in `assets/textures/README.md`.

## 6. Editor UX — the Asset Browser

A panel ([`./editor.md`](./editor.md) §2) listing project assets by folder/kind:

```
┌─ Asset Browser ──────────────────────────────┐
│ [models] [textures] [materials] [shaders] ▾   │
│ ┌────┐ ┌────┐ ┌────┐ ┌────┐                   │
│ │mesh│ │ tex│ │ tex│ │ mat│   ⟵ thumbnails     │
│ └────┘ └────┘ └────┘ └────┘                   │
│ drag → Scene View / Inspector / Material slot │
│ right-click: Reimport · Open in VSCode · Show │
└──────────────────────────────────────────────┘
```

- **Import-on-drop** — drag a `.glb`/`.png` from the OS file manager into the
  browser (or project folder) and it's imported with default settings + a sidecar.
- **Drag-to-use** — drag a texture onto an object in the Scene View (auto-tiling,
  [`./materials-and-textures.md`](./materials-and-textures.md) §6) or a material
  slot; drag a model into the scene to instantiate it.
- **Reimport** — right-click to re-run import (after changing settings); the
  sidecar panel edits `ImportSettings`.
- **Open in VSCode** for `.flsl`/`.lua` ([ADR-0011](../decisions/0011-vscode-integration.md)).

## 7. Out of scope

We are lightweight — **not a content-management platform.**

- **FBX / USD import** — glTF only ([ADR-0006](../decisions/0006-asset-pipeline-gltf.md));
  FBX is proprietary/messy, USD is overkill for our scope.
- **A networked asset server** — assets are local files in a project dir; no
  remote/team asset service.
- **Full asset-bundle / streaming system** at launch — export packs the whole
  dependency closure; on-demand streaming and partial bundles are *future*
  ([ROADMAP](../ROADMAP.md) "Later"), added if a game needs them.

If a pipeline feature serves big-studio asset logistics over a solo dev's
drop-it-in-and-go loop, it waits.
