# Floptle — Assets & the Blender Pipeline (`floptle-assets`)

> Model in Blender, export glTF, drop it in — meshes, materials, skins, and
> animations land intact, get stable ids, and hot-reload on save. See
> ADR-0006,
> materials in [`./materials-and-textures.md`](./materials-and-textures.md),
> animation in [`./animation.md`](./animation.md), the editor's asset browser in
> [`./editor.md`](./editor.md), temp assets in
> ADR-0010,
> and [`../ARCHITECTURE.md`](../ARCHITECTURE.md) §6.

`floptle-assets` is the bridge from *files on disk* to *engine resources on the
GPU and in the ECS*. It imports glTF, decodes textures, assigns stable ids, tracks
dependencies, and watches files so a save in Blender/VSCode/an image editor shows
up live in the running editor.

## 1. glTF 2.0 import (the Blender path)

Blender exports **glTF 2.0** (`.glb` preferred — single binary file) via its
native exporter (ADR-0006); we read it
with the `gltf` crate, which gives direct buffer access for fast upload.

**What we import:**

- **Meshes** — positions, indices, **UVs** (set 0), **normals** (computed if absent),
  and **vertex colors** (`COLOR_0` → `MeshData.colors`, the vertex-paint store — see
  [`./materials-and-textures.md`](./materials-and-textures.md)). Split per-primitive
  (per-material). *Tangents and UV sets beyond 0 are not imported yet.*
- **PBR materials** — base color, metallic/roughness, normal, emissive,
  occlusion, textures and factors (UV set 0). A part draws them as the same
  maps set by hand on a material would ([`./materials-and-textures.md`](./materials-and-textures.md),
  "The maps").
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
2. Upload to a wgpu texture; pooled staging buffers (ADR-0008).
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
(see [export-builds.md](../export-builds.md)): collect the dependency closure from the entry
scene, drop editor-only data, optionally pack assets into a single archive next to
the runtime binary, and emit per-OS bundles (Linux/Windows/macOS).

```
project dir ─▶ walk deps from entry scene ─▶ strip editor data
            ─▶ pack assets + RON ─▶ link with floptle-runtime ─▶ platform bundle
```

## 5. Placeholder test textures

Third-party textures used while building test content are **local-only
placeholders**, under a git-ignored folder. They are never committed — this
repository is public, so nothing whose licence does not permit it goes into its
history — and nothing that ships is built on them: placeholders are replaced with
original Fopull art before a release, and that is a gate on the release rather
than a habit.

The rule that makes this safe is that the engine never hard-depends on any of
them. Built-in defaults ([`./materials-and-textures.md`](./materials-and-textures.md) §4)
cover real content, so a checkout with no placeholders in it renders correctly.

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
- **Open in VSCode** for `.flsl`/`.lua` (ADR-0011).

## 7. Converting a model the engine cannot open

Asset packs are FBX. Scans are PLY. Anything from CAD is STL. Telling somebody
their model is the wrong kind of file is true and useless, so the editor
converts it **on arrival**: a supported model anywhere in the project is
converted in the background, and again whenever its source changes. The how-to
is [`../importing-models.md`](../importing-models.md); this is the design.

The result is written beside the source and is **one self-contained `.glb`** —
geometry, materials and embedded textures — with the source left untouched.
Beside rather than in a cache, because the `.glb` is what a scene names and a
build ships: next to its source it is an ordinary asset that a clone, an export
and the web build all already handle. The Assets panel folds the pair into one
row, and delete, move and rename carry both.

**Only its own output is written over.** Every converted `.glb` carries a stamp
in `asset.extras.floptleConverted`: the source's format, a hash of its bytes and
the converter's version. A `.glb` without one is somebody's own and is never
replaced. A source newer than its output is hashed before it is called changed,
so a fresh clone does not convert the project again; an output from an older
converter is converted again.

| From | Notes |
|---|---|
| `.fbx` | Binary and ASCII, every version. Read with [ufbx](https://github.com/ufbx/ufbx). Skins and animation takes come across (below). |
| `.obj` | With its `.mtl`, through the same reader, so the two cannot disagree. |
| `.stl` | Binary and ASCII. Normals are recomputed — most STL files have wrong or zeroed ones. |
| `.ply` | Per-vertex colour kept, which is a scan's whole value. A point cloud with no faces is refused with an explanation rather than converted to an empty model. |
| `.gltf` | Not a format change — a **verbatim repack**: the JSON is kept and only its buffer and image references move into the one file. Not done automatically, since the engine opens a loose `.gltf` as it is. |
| `.blend`, `.dae`, `.usd*` | Through the installed Blender, run in the background with its own glTF exporter. No Rust reader keeps per-face materials, the up axis and transformed normals for these; Blender's importers do. |

**It normalises, it does not just re-encode.** Every one of these formats
disagrees with glTF about something, and a converter that only re-encodes hands
back a model that loads and is wrong:

- **Axes** — FBX is Z-up from 3ds Max, Y-up from Maya; glTF is always Y-up.
- **Units** — FBX records its own, and most exporters write centimetres. glTF is
  metres, and a hundredfold error reads as a broken importer.
- **Winding** — correcting handedness, or any mirrored node, inverts triangle
  winding. A model with every face backwards is invisible from outside and solid
  from within. (The glTF importer also turns a mirrored node's triangles round,
  as the glTF spec asks.)
- **N-gons** — FBX and OBJ have quads; glTF has triangles.
- **Materials** — every map goes to the glTF slot it means: separate roughness
  and metallic pictures are packed into one (with occlusion in red when it
  fits), glossiness and shininess maps are inverted into roughness, a height map
  in a bump slot becomes a normal map, and an opacity map becomes the base
  colour's alpha with a cut-out or blend mode chosen from what is in it.
- **Pictures** — found as written, then by name in the texture folders packs
  use, case blind and with the other picture extensions tried. A picture that
  is not found anywhere is reported with its material and slot.

**Rigs and clips.** A model with a skin or an animation that moves a mesh or a
bone keeps its node tree. Skinned meshes are written in their bind pose with
each bone's inverse bind matrix; a rigid part keeps its node; each animation
take is resampled by ufbx into translation, rotation and scale keys, so FBX's
rotation orders, pivots and inherit modes arrive resolved. A model with neither
is baked flat into world-space meshes, which sidesteps all of that.

**What is dropped is named, never silent.** Cameras, lights, blend shapes,
animation that moves only cameras or lights, and maps with no glTF slot
(displacement, reflection) do not come across; the Console report says which.

## 8. Out of scope

We are lightweight — **not a content-management platform.**

- **FBX / USD as a runtime format** — the engine loads glTF and only glTF
  (ADR-0006). That has not changed
  and should not: one well-specified load path is worth more than four
  half-supported ones. **Converting at authoring time is a different question,
  and is now answered** — see §7, including USD through Blender.
- **A networked asset server** — assets are local files in a project dir; no
  remote/team asset service.
- **Full asset-bundle / streaming system** at launch — export packs the whole
  dependency closure; on-demand streaming and partial bundles are *future*
  (later work), added if a game needs them.

If a pipeline feature serves big-studio asset logistics over a solo dev's
drop-it-in-and-go loop, it waits.
