//! **Upgrade steps that keep an older project's models looking as they did.**
//!
//! 0.117 changed three things about how models and materials draw, each for
//! the better in a new project and each a visible change in an old one:
//!
//! * **Model textures** follow each file's sampler. In 0.116 every model
//!   texture drew crisp; an old project is set to "always crisp".
//! * **Imported materials** honour factor-only metallic and roughness, the
//!   alpha mode, backface culling and the texture transform (from rc7). An
//!   older project is set to "Classic (0.116)", which draws by the old rules.
//! * **Blend mode** is a setting of its own instead of something opacity
//!   implied. A material from before it reads as the mode it meant: blended
//!   below full opacity, otherwise cutout, or opaque at a cutoff of 0.
//!
//! The reading side resolves a missing setting to the old answer
//! (`ProjectConfigDoc::model_texture_filter_resolved`,
//! `imported_materials_resolved`, `MaterialDoc::blend_resolved`). The project
//! settings are written down, because a new project's defaults differ and a
//! later release must not change them underneath a shipped game. A blend mode
//! is not: a missing one only ever means a file from before the setting.

use std::path::{Path, PathBuf};

/// Write an old project's model look into its config, where the file says
/// nothing: the texture filter and the material rules it drew with. Returns
/// whether it changed anything. A setting already there is never touched.
pub(crate) fn pin_model_look(cfg: &mut floptle_scene::ProjectConfigDoc, from: Option<&str>) -> bool {
    let mut changed = false;
    if cfg.model_texture_filter.is_none() {
        cfg.model_texture_filter = Some(old_filter(from));
        changed = true;
    }
    if cfg.imported_materials.is_none() {
        cfg.imported_materials = Some(old_rules(from));
        changed = true;
    }
    changed
}

/// What [`upgrade_project`] changed. The upgrade pass reports each written
/// file on its own line.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Upgraded {
    pub(crate) model_look: bool,
    /// Every file written, for the upgrade report.
    pub(crate) written: Vec<PathBuf>,
}

/// The upgrade step (`upgrade::STEPS`, "model-look-and-blend"): [`upgrade_project`]
/// over the project at `root`, returning the files it wrote.
pub(crate) fn keep_model_look(root: &Path, from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    upgrade_project(root, from).map(|u| u.written)
}

/// The model texture filter an old project drew with. A 0.117 pre-release
/// already followed each file's sampler; anything older drew every model
/// crisp.
fn old_filter(from: Option<&str>) -> floptle_scene::ModelTextureFilterDoc {
    let rc = from.is_some_and(|v| floptle_dist::version_key(v) >= floptle_dist::version_key("0.117.0-rc1"));
    if rc { floptle_scene::ModelTextureFilterDoc::AsFile } else { floptle_scene::ModelTextureFilterDoc::Crisp }
}

/// The imported-material rules an old project drew with. 0.117.0-rc7 already
/// drew imported materials as their files describe; anything older drew them
/// by the 0.116 rules.
fn old_rules(from: Option<&str>) -> floptle_scene::ImportedMaterialsDoc {
    let rc = from.is_some_and(|v| floptle_dist::version_key(v) >= floptle_dist::version_key("0.117.0-rc7"));
    if rc { floptle_scene::ImportedMaterialsDoc::AsFile } else { floptle_scene::ImportedMaterialsDoc::Classic }
}

/// Run the step over the project at `root`: write the old model look into
/// its `project.ron` where it says nothing.
///
/// Blend modes are not written into scenes, prefabs or materials. Every save
/// since the setting writes the mode, so a material without one is always from
/// before it, and `MaterialDoc::blend_resolved` gives it the mode it drew with.
/// Writing it down changed nothing about how a file draws, and rewrote every
/// scene in a project, dropping the comments in each.
pub(crate) fn upgrade_project(root: &Path, from: Option<&str>) -> Result<Upgraded, String> {
    let mut out = Upgraded::default();
    let project_ron = root.join("project.ron");
    if let Ok(Some(mut cfg)) = floptle_scene::try_load_project(&project_ron)
        && pin_model_look(&mut cfg, from)
    {
        floptle_scene::save_project(&cfg, &project_ron).map_err(|e| e.to_string())?;
        out.model_look = true;
        out.written.push(project_ron);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **An old project keeps its models' look; a new one draws as files
    /// say.** A `project.ron` with neither setting is pinned to crisp and
    /// Classic; one that chose is left alone.
    #[test]
    fn an_old_project_is_pinned_to_the_0116_model_look() {
        let mut old: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        assert!(pin_model_look(&mut old, None));
        assert_eq!(old.model_texture_filter, Some(floptle_scene::ModelTextureFilterDoc::Crisp));
        assert_eq!(old.imported_materials, Some(floptle_scene::ImportedMaterialsDoc::Classic));
        assert!(!pin_model_look(&mut old, None), "a second pass changes nothing");
        let mut new = floptle_scene::ProjectConfigDoc::for_new_project();
        assert!(!pin_model_look(&mut new, None), "a new project already says as each file says");
        assert_eq!(new.model_texture_filter_resolved(), floptle_scene::ModelTextureFilterDoc::AsFile);
        assert_eq!(new.imported_materials_resolved(), floptle_scene::ImportedMaterialsDoc::AsFile);
    }

    /// **An old material reads as the blend mode it drew with**: below full
    /// opacity it blended, a cutoff of 0 was solid, anything else cut out at
    /// its cutoff. A written mode is the material's, whatever its opacity says.
    #[test]
    fn an_old_material_reads_as_the_blend_mode_it_meant() {
        use floptle_core::BlendMode as B;
        let mat = |s: &str| ron::from_str::<floptle_scene::MaterialDoc>(s).unwrap();
        for (text, want) in [
            ("(alpha: 0.4)", B::Blend),
            ("(alpha_cutoff: 0.0)", B::Opaque),
            ("()", B::Cutout),
            ("(alpha_cutoff: 0.3)", B::Cutout),
            ("(alpha: 0.4, blend: Some(Opaque))", B::Opaque),
        ] {
            assert_eq!(mat(text).to_material().blend, want, "{text} reads as it drew");
        }
    }

    /// **The whole upgrade pass over an old project writes its config and none
    /// of its scenes, prefabs or materials.** A scene with a translucent material and comments through
    /// it stays byte for byte, and still draws its glass blended; a second
    /// pass changes nothing at all.
    #[test]
    fn the_pass_leaves_scenes_prefabs_and_materials_byte_for_byte() {
        let root = std::env::temp_dir().join(format!("floptle-upgrade-models-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["scenes", "prefabs", "materials"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("project.ron"), "()").unwrap();
        let files = [
            (
                "scenes/one.ron",
                "// the hall\n(name: \"one\", nodes: [\n    // the window\n    (name: \"Glass\", material: Some((alpha: 0.5))),\n])",
            ),
            ("prefabs/pane.prefab.ron", "//floptle-nodes-v1\n[(name: \"Pane\", material: Some((alpha: 0.4)))]"),
            ("materials/Leaf.ron", "// cut out\n(alpha_cutoff: 0.4)"),
        ];
        for (f, text) in files {
            std::fs::write(root.join(f), text).unwrap();
        }
        let done = crate::upgrade::run(&root, None);
        let written: Vec<_> = done.iter().filter_map(|d| d.result.as_ref().ok()).flatten().collect();
        let touched: Vec<_> = files.iter().filter(|(f, _)| written.contains(&&root.join(f))).collect();
        assert!(touched.is_empty(), "the pass wrote {touched:?}: {:?}", crate::upgrade::report(&root, &done));
        for (f, text) in files {
            assert_eq!(std::fs::read_to_string(root.join(f)).unwrap(), text, "{f} was rewritten");
        }
        let scene = floptle_scene::load(&root.join("scenes/one.ron")).unwrap();
        assert_eq!(scene.nodes[0].material.as_ref().unwrap().blend_resolved(), floptle_core::BlendMode::Blend);
        let cfg = floptle_scene::load_project(&root.join("project.ron"));
        assert_eq!(cfg.model_texture_filter, Some(floptle_scene::ModelTextureFilterDoc::Crisp));
        assert_eq!(cfg.imported_materials, Some(floptle_scene::ImportedMaterialsDoc::Classic));
        let before = std::fs::read(root.join("project.ron")).unwrap();
        let again = crate::upgrade::run(&root, None);
        assert!(again.iter().all(|d| d.result.as_ref().is_ok_and(Vec::is_empty)), "{:?}", crate::upgrade::report(&root, &again));
        assert_eq!(std::fs::read(root.join("project.ron")).unwrap(), before, "the second pass changed the config");
        // A 0.117 pre-release already filtered textures as their files say,
        // and from rc7 drew imported materials as their files describe.
        let mut rc: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        pin_model_look(&mut rc, Some("0.117.0-rc6"));
        assert_eq!(rc.model_texture_filter, Some(floptle_scene::ModelTextureFilterDoc::AsFile));
        assert_eq!(rc.imported_materials, Some(floptle_scene::ImportedMaterialsDoc::Classic));
        let mut rc7: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        pin_model_look(&mut rc7, Some("0.117.0-rc7"));
        assert_eq!(rc7.imported_materials, Some(floptle_scene::ImportedMaterialsDoc::AsFile));
        let _ = std::fs::remove_dir_all(&root);
    }
}
