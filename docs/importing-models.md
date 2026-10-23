# Importing models

Put a model in your project and use it. Floptle draws glTF (`.glb`), so any
other format is converted to one the moment it arrives: drop an `.fbx` into the
Assets panel, copy an `.obj` into `models/` from your file manager, or pick files
with **⬇ Import…**, and a few seconds later the model is ready to drag into the
scene. You do not press anything.

## What converts

| Format | Read by | What comes across |
|---|---|---|
| `.glb` | — | Used as it is. |
| `.gltf` (loose, with its `.bin` and pictures) | Floptle | Used as it is. **⇄ Convert to .glb now** packs it into one file, unchanged: nodes, skins, animation, every material map, texture transforms and sampler filters. |
| `.fbx` (binary and ASCII, every version) | [ufbx](https://github.com/ufbx/ufbx) | Meshes, every material map, skins with their bones, and every animation take as its own clip. |
| `.obj` with its `.mtl` | ufbx | Meshes and every `.mtl` map. |
| `.stl` (binary and ASCII) | Floptle | Geometry; normals are recomputed. |
| `.ply` | Floptle | Geometry and per-vertex colour. |
| `.blend` | **Blender**, if installed | What Blender's own glTF exporter writes: modifiers applied, materials, armatures, actions. |
| `.dae` (Collada) | **Blender** up to 4.x | As above. Blender 5 dropped Collada; export from it as `.fbx` instead. |
| `.usd`, `.usda`, `.usdc`, `.usdz` | **Blender** | As above. |

`.3ds`, `.max`, `.ma`/`.mb`, `.c4d` and other applications' own files do not
convert. Export them from their program as `.fbx`, `.obj` or `.glb`.

### Blender formats

`.blend`, `.dae` and `.usd` files are converted by running the Blender you
already have, in the background. Floptle finds it on your `PATH` or where its
installer puts it; set `FLOPTLE_BLENDER` to the program to point at one
somewhere else. Without Blender the Console says so and names the alternatives.
Keeping your `.blend` in the project works well: save it in Blender and the
model in your scene updates a moment later.

## Where the converted model goes

Beside its source, with the same name: `models/crate.fbx` becomes
`models/crate.glb`. The Assets panel shows the pair as one model, named
`crate.glb ⇄ from crate.fbx`. That `.glb` is what your scenes use and what a
build ships; the `.fbx` stays in the project as the thing you edit and is left
out of builds.

Dragging the source into the scene places its converted model. Deleting,
moving or renaming the model in the Assets panel does the same to its source,
so the two never drift apart.

**Your own `.glb` is never replaced.** Every converted model records which
source it came from. If a `crate.glb` you made yourself is already next to
`crate.fbx`, the conversion is refused and says why; rename one of them to
convert.

**Change the source and the model follows.** Saving the `.fbx` (or the `.obj`'s
`.mtl`) again converts it again. A folder that was only copied or cloned, with
new file times and the same contents, is not converted again.

## How materials are picked up

Every map is assigned to the slot it was plugged into in the modelling
program, and turned into what glTF's slots expect:

| In the source | In Floptle |
|---|---|
| Diffuse / base colour map and colour | Base colour |
| Opacity, transparency or `map_d` | The base colour's alpha. Mostly solid-or-empty is a cut-out; anything else blends. |
| Normal map | Normal |
| Bump map, `map_Bump`, `bump` | Normal. A bump slot holds a normal map in some files and a height map in others; Floptle looks at the picture, and turns a height map into a normal map. |
| Roughness, `map_Pr` | Roughness, packed with metallic into one picture the way glTF wants |
| Metallic, `map_Pm` | Metallic, packed with roughness |
| Glossiness, shininess, `map_Ns` | Roughness, inverted (glossy is smooth) |
| Specular map, when there is no roughness or gloss map | Roughness, from the specular map |
| Ambient occlusion | Occlusion, in the same packed picture when it fits |
| Emissive colour and map, `map_Ke` | Emissive |
| Texture tiling and offset on the base colour map | Built into the model's texture coordinates |

Displacement and reflection maps have no slot and are named in the report.

**The material draws as it was set.** A `.glb` or `.gltf` (converted or not)
draws its metallic and roughness values whether or not it has a picture for
them; a material set to *Opaque* ignores its picture's alpha, *Alpha Clip*
(MASK) cuts at its own threshold and *Blend* blends; a material exported with
backface culling on is not drawn from behind; and the base map's tiling,
offset and rotation (`KHR_texture_transform`) apply to all of the material's
maps, which share one set of texture coordinates.

**Pictures are found where packs put them.** The path in the file is tried as
written, relative to the model; then the file name in the model's folder, in
`textures/`, `Textures/`, `maps/`, `images/`, `materials/` or `sourceimages/`
beside it or one folder up, and in the `<name>.fbm` folder an FBX's embedded
pictures unpack to. Names are matched without regard to case, and a picture
recorded as `.tga` that was re-saved as `.png` is still found. Pictures
embedded in an FBX are used directly. PNG, JPEG, TGA, BMP, TIFF, WebP, GIF,
QOI and DDS all read.

## The import report

Each conversion writes one entry to the Console:

```
Imported crate.fbx as crate.glb: 2 meshes, 1,204 triangles, 2 materials, 4 textures embedded
  Crate: base colour ← crate_d.png, normal ← crate_n.png, roughness ← crate_gloss.png (glossiness, inverted)
  ⚠ Trim: the base colour map `trim_d.png` was not found beside the model or in a textures folder near it
```

Animation clips and skinned meshes are listed when there are any; a picture
that could not be found is named with the material and slot that wanted it.
Put it in place and use **⇄ Re-convert from crate.fbx** in the model's
right-click menu.

Cameras, lights and blend shapes do not come across, and the report says so.
FBX takes that animate only a camera or a light are left out the same way.

## Animated and rigged FBX

A skinned FBX converts with its skeleton: each bone becomes a node, the mesh
bends with its bones, and each animation take becomes a clip named after it,
ready for [animation.md](animation.md)'s controllers. A part parented to a
bone without skinning, such as a sword in a hand, rides that bone. FBX's
rotation orders, pivots and units are resolved during conversion, so a clip
plays at the size and speed it was authored.
