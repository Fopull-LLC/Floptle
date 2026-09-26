//! # floptle-assets
//!
//! Gets your Blender work into the game with geometry, UVs, materials, skins,
//! and animations intact (glTF 2.0), and handles textures — including the
//! "just drag it on and tell it how to tile" workflow without writing a shader.
//! See `docs/subsystems/materials-and-textures.md` + `asset-pipeline.md`.
//!
//! Modules:
//! - `gltf_import` : meshes/UVs/materials/skins/animations from Blender. **Live**
//!   for geometry (Phase 2 slice 2a); materials/skins/animations are later slices.
//! - `texture`     : decode + GPU upload; tiling/repeat/flip/clamp options. *(TODO)*
//! - `material`    : the material asset (shader ref + params + textures). *(TODO)*
//! - `db`          : asset database (stable ids, hot-reload, dependency graph). *(TODO)*

pub mod auto_rig;
pub mod glb_write;
pub mod gltf_import;
pub mod gltf_mirror;
pub mod gltf_rig;
pub mod texture;

pub use auto_rig::{add_flow_rig, RigReport};
pub use glb_write::{write_glb, WriteMesh, WriteNode, WriteSkin};
pub use gltf_import::{import, ImportError, ImportedModel};
pub use gltf_mirror::{mirror_apply, MirrorReport};
pub use gltf_rig::{import_rigged, probe_animations, RiggedModel, RiggedPart};
pub use texture::{
    decode_png, decode_untrusted, encode_png, load_texture, load_texture_sized,
    load_texture_sized_filtered, save_texture_png, UNTRUSTED_MAX_SIDE,
};

/// A model file as the engine draws it: with its node tree when it has one
/// worth keeping, baked flat per material when it does not.
#[derive(Debug)]
pub enum Model {
    Rigged(RiggedModel),
    Static(ImportedModel),
}

/// Import a model, reading and decoding the file **once**.
///
/// [`import_rigged`] followed by [`import`] reads the file and decodes every
/// image in it, finds a lone static mesh, and hands over to the static import,
/// which reads and decodes it all again: twice the cost for the most common
/// model there is. This reads once and branches. A rig that fails to build
/// still falls back to the static bake, as it always has.
pub fn import_model(path: &std::path::Path) -> Result<Model, ImportError> {
    let (doc, buffers, images) = gltf_import::read_gltf(path, true)?;
    if gltf_rig::wants_rig(&doc) {
        match gltf_rig::build_rigged(path, &doc, &buffers, &images) {
            Ok(Some(m)) => return Ok(Model::Rigged(m)),
            Ok(None) => {}
            Err(e) => floptle_say::say_err!("  rig import {} failed ({e}); trying static", path.display()),
        }
    }
    gltf_import::build_static(path, &doc, &buffers, &images).map(Model::Static)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// Reading once must not change what comes out: the same parts, the same
    /// branch, as the two importers it replaces.
    #[test]
    fn one_read_imports_what_two_did() {
        let prop = Path::new("../../assets/SaesRapier.glb");
        assert!(crate::import_rigged(prop).unwrap().is_none(), "the fixture is a lone static mesh");
        let old = crate::import(prop).unwrap();
        let crate::Model::Static(new) = crate::import_model(prop).unwrap() else {
            panic!("a lone static mesh took the rigged branch")
        };
        assert_eq!(new.parts.len(), old.parts.len());
        assert_eq!(new.textures.len(), old.textures.len());
        assert_eq!(new.parts[0].mesh.vertices.len(), old.parts[0].mesh.vertices.len());
        assert_eq!(new.size, old.size);

        let rigged = Path::new("../../assets/models/Sae.glb");
        let old = crate::import_rigged(rigged).unwrap().expect("the fixture keeps its tree");
        let crate::Model::Rigged(new) = crate::import_model(rigged).unwrap() else {
            panic!("a rigged model took the static branch")
        };
        assert_eq!(new.parts.len(), old.parts.len());
        assert_eq!(new.clips.len(), old.clips.len());
    }
}
