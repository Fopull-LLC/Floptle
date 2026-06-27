# ADR-0006 — Blender pipeline: glTF 2.0

- **Status:** Accepted · 2026-06-27
- **Decider:** Ty Johnston (Fopull LLC)

## Context
The developer models in Blender and wants models in-game with geometry, UVs,
textures, materials, and animations intact, with minimal friction.

## Decision
Use **glTF 2.0** (`.glb`/`.gltf`) as the import format, via the `gltf` crate.
Blender exports glTF natively.

## Why
- **First-class Blender export** covering meshes, UVs, normals, PBR materials,
  skins, and **animations** — the whole list the developer needs.
- **Open standard**, well-specified, broadly tooled, easy to validate.
- The `gltf` Rust crate is mature and gives us direct buffer access for fast
  GPU upload.

## Alternatives considered
- **FBX** — ubiquitous but proprietary and historically messy to parse robustly.
- **USD** — powerful but heavy; overkill for our scope.
- **A custom Blender exporter/format** — full control, but ongoing maintenance
  against Blender's release cadence; not worth it versus glTF.

## Consequences
- Some Blender material features don't map 1:1 to glTF PBR; our material system
  (and shader IR) extends beyond glTF anyway, so imports seed materials we then
  enrich.
- A small optional Blender helper add-on may come later for one-click export
  presets, but is **not** required.
