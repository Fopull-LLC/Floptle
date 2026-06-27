//! # floptle-assets
//!
//! Gets your Blender work into the game with geometry, UVs, materials, skins,
//! and animations intact (glTF 2.0), and handles textures — including the
//! "just drag it on and tell it how to tile" workflow without writing a shader.
//! See `docs/subsystems/materials-and-textures.md` + `asset-pipeline.md`.
//!
//! Planned modules:
//! - `gltf_import` : meshes/UVs/materials/skins/animations from Blender.
//! - `texture`     : decode + GPU upload; tiling/repeat/flip/clamp options.
//! - `material`    : the material asset (shader ref + params + textures).
//! - `db`          : asset database (stable ids, hot-reload, dependency graph).
