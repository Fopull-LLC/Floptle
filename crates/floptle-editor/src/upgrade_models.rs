//! **Upgrade steps that keep an older project's models looking as they did.**
//!
//! 0.117 changed three things about how models and materials draw, each for
//! the better in a new project and each a visible change in an old one:
//!
//! * **Model textures** follow each file's sampler. In 0.116 every model
//!   texture drew crisp; an old project is set to "always crisp".
//! * **Imported materials** honour factor-only metallic and roughness, the
//!   alpha mode, backface culling and the texture transform. An old project is
//!   set to "Classic (0.116)", which draws by the old rules.
//! * **Blend mode** is a setting of its own instead of something opacity
//!   implied. A material from before it is given the mode it meant: blended
//!   below full opacity, otherwise cutout, or opaque at a cutoff of 0.
//!
//! The reading side already resolves a missing setting to the old answer
//! (`ProjectConfigDoc::model_texture_filter_resolved`,
//! `imported_materials_resolved`, `MaterialDoc::blend_resolved`), so a project
//! opened without these steps still looks as it did. The steps write the
//! answer down, so no later release can change it underneath a shipped game.

use std::path::{Path, PathBuf};

/// Write an old project's model look into its config: crisp textures and the
/// 0.116 material rules, where the file says nothing. Returns whether it
/// changed anything. A setting already there is never touched.
pub(crate) fn pin_model_look(cfg: &mut floptle_scene::ProjectConfigDoc, from: Option<&str>) -> bool {
    let mut changed = false;
    if cfg.model_texture_filter.is_none() {
        cfg.model_texture_filter = Some(old_filter(from));
        changed = true;
    }
    if cfg.imported_materials.is_none() {
        cfg.imported_materials = Some(floptle_scene::ImportedMaterialsDoc::Classic);
        changed = true;
    }
    changed
}

/// Give a material without a blend mode the one it meant. Returns whether it
/// changed.
pub(crate) fn pin_blend(m: &mut floptle_scene::MaterialDoc) -> bool {
    if m.blend.is_some() {
        return false;
    }
    m.blend = Some(floptle_scene::BlendDoc::from_mode(m.blend_resolved()));
    true
}

/// [`pin_blend`] over every material a scene holds: each node's own and each
/// object override. Returns how many it wrote.
pub(crate) fn pin_scene_blends(doc: &mut floptle_scene::SceneDoc) -> usize {
    pin_node_blends(&mut doc.nodes)
}

/// [`pin_blend`] over every material some nodes hold.
fn pin_node_blends(nodes: &mut [floptle_scene::NodeDoc]) -> usize {
    let mut n = 0;
    for node in nodes {
        if let Some(m) = node.material.as_mut() {
            n += pin_blend(m) as usize;
        }
        for m in node.object_materials.values_mut() {
            n += pin_blend(m) as usize;
        }
    }
    n
}

/// What [`upgrade_project`] changed. The upgrade pass reports each written
/// file on its own line.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Upgraded {
    pub(crate) model_look: bool,
    /// `(file, materials given a blend mode)` for every file rewritten.
    pub(crate) blends: Vec<(String, usize)>,
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

/// Leading `//` comment lines of a file, kept through a rewrite: another
/// step marks the files it has done that way.
fn leading_comments(text: &str) -> String {
    text.lines().take_while(|l| l.trim_start().starts_with("//")).map(|l| format!("{l}\n")).collect()
}

/// Run both steps over the project at `root`: its `project.ron`, every scene
/// and prefab, and every project material under `materials/`. A file is
/// rewritten only when something in it changed, and left byte for byte
/// otherwise.
pub(crate) fn upgrade_project(root: &Path, from: Option<&str>) -> Result<Upgraded, String> {
    let mut out = Upgraded::default();
    let project_ron = root.join("project.ron");
    if let Ok(Some(mut cfg)) = floptle_scene::try_load_project(&project_ron)
        && pin_model_look(&mut cfg, from)
    {
        floptle_scene::save_project(&cfg, &project_ron).map_err(|e| e.to_string())?;
        out.model_look = true;
        out.written.push(project_ron.clone());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
        for e in rd {
            let p = e.path();
            let file = e.file_name().to_string_lossy().to_string();
            if file.starts_with('.') {
                continue;
            }
            if e.is_dir() {
                if file != "target" {
                    stack.push(p);
                }
                continue;
            }
            let name = p.to_string_lossy().replace('\\', "/");
            if !file.ends_with(".ron") || file == "project.ron" || file.ends_with(floptle_scene::VFX_EXT) {
                continue;
            }
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            let Ok(text) = floptle_vfs::read_to_string(&p) else { continue };
            let head = leading_comments(&text);
            let (next, n) = if crate::assets::is_material(&name) {
                let Ok(mut m) = ron::from_str::<floptle_scene::MaterialDoc>(&text) else { continue };
                if !pin_blend(&mut m) {
                    continue;
                }
                (ron::ser::to_string_pretty(&m, ron::ser::PrettyConfig::default()).map_err(|e| e.to_string())?, 1)
            } else if file.ends_with(".prefab.ron") {
                const TAG: &str = "//floptle-nodes-v1";
                let body = text.trim_start();
                let body = body.strip_prefix(TAG).unwrap_or(body);
                let Ok(mut nodes) = ron::from_str::<Vec<floptle_scene::NodeDoc>>(body) else { continue };
                let n = pin_node_blends(&mut nodes);
                if n == 0 {
                    continue;
                }
                // The tag is a comment line, kept with the others in `head`.
                (ron::ser::to_string_pretty(&nodes, ron::ser::PrettyConfig::default()).map_err(|e| e.to_string())?, n)
            } else {
                let Ok(mut doc) = floptle_scene::from_ron(&text) else { continue };
                let n = pin_scene_blends(&mut doc);
                if n == 0 {
                    continue;
                }
                (floptle_scene::to_ron(&doc).map_err(|e| e.to_string())?, n)
            };
            floptle_scene::portable::write(&p, &format!("{head}{next}")).map_err(|e| format!("{}: {e}", p.display()))?;
            out.blends.push((rel, n));
            out.written.push(p);
        }
    }
    out.blends.sort();
    out.written.sort();
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

    /// **An old material gets the blend mode it drew with**, written down:
    /// below full opacity it blended, a cutoff of 0 was solid, anything else
    /// cut out at its cutoff.
    #[test]
    fn an_old_scene_s_materials_are_given_the_blend_mode_they_meant() {
        use floptle_core::BlendMode as B;
        let mat = |s: &str| ron::from_str::<floptle_scene::MaterialDoc>(s).unwrap();
        for (text, want) in [
            ("(alpha: 0.4)", B::Blend),
            ("(alpha_cutoff: 0.0)", B::Opaque),
            ("()", B::Cutout),
            ("(alpha_cutoff: 0.3)", B::Cutout),
        ] {
            let mut m = mat(text);
            assert_eq!(m.to_material().blend, want, "{text} reads as it drew");
            assert!(pin_blend(&mut m));
            assert_eq!(m.blend.map(|b| b.to_mode()), Some(want), "{text} is written down");
            let again = ron::to_string(&m).unwrap();
            assert!(again.contains("blend:"), "{again}");
        }
        // A written blend mode is the material's, whatever its opacity says.
        let mut chosen = mat("(alpha: 0.4, blend: Some(Opaque))");
        assert!(!pin_blend(&mut chosen));
        assert_eq!(chosen.to_material().blend, B::Opaque);
    }

    /// The whole project pass rewrites only what changed and says so.
    #[test]
    fn the_project_pass_pins_the_config_and_every_scene() {
        let root = std::env::temp_dir().join(format!("floptle-upgrade-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        std::fs::create_dir_all(root.join("materials")).unwrap();
        std::fs::write(root.join("project.ron"), "()").unwrap();
        std::fs::write(
            root.join("scenes/one.ron"),
            "(name: \"one\", nodes: [(name: \"Glass\", material: Some((alpha: 0.5))), (name: \"Plain\")])",
        )
        .unwrap();
        std::fs::write(root.join("materials/Leaf.ron"), "(alpha_cutoff: 0.4)").unwrap();
        let done = upgrade_project(&root, None).unwrap();
        assert!(done.model_look);
        assert_eq!(done.blends, [("materials/Leaf.ron".to_string(), 1), ("scenes/one.ron".to_string(), 1)]);
        let scene = floptle_scene::load(&root.join("scenes/one.ron")).unwrap();
        assert_eq!(scene.nodes[0].material.as_ref().unwrap().blend, Some(floptle_scene::BlendDoc::Blend));
        let again = upgrade_project(&root, None).unwrap();
        assert_eq!(again, Upgraded::default(), "a second pass does nothing");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The whole upgrade pass, twice, over an old project**: the first
    /// run pins the look and the blend modes and keeps every file's leading
    /// comments (another step's mark among them, so it does not run again);
    /// the second changes nothing at all.
    #[test]
    fn the_pass_is_idempotent_and_keeps_other_steps_marks() {
        let root = std::env::temp_dir().join(format!("floptle-upgrade-models-pass-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        std::fs::create_dir_all(root.join("prefabs")).unwrap();
        std::fs::write(root.join("project.ron"), "()").unwrap();
        std::fs::write(
            root.join("scenes/one.ron"),
            "// a mark another step left\n(name: \"one\", nodes: [(name: \"Glass\", material: Some((alpha: 0.5)))])",
        )
        .unwrap();
        std::fs::write(
            root.join("prefabs/pane.prefab.ron"),
            "//floptle-nodes-v1\n[(name: \"Pane\", material: Some((alpha: 0.4)))]",
        )
        .unwrap();
        crate::upgrade::run(&root, None);
        let scene = std::fs::read_to_string(root.join("scenes/one.ron")).unwrap();
        assert!(scene.starts_with("// a mark another step left\n"), "{scene}");
        assert!(scene.contains("blend: Some(Blend)"), "{scene}");
        let prefab = std::fs::read_to_string(root.join("prefabs/pane.prefab.ron")).unwrap();
        assert_eq!(prefab.matches("//floptle-nodes-v1").count(), 1, "{prefab}");
        assert!(prefab.contains("blend: Some(Blend)"), "{prefab}");
        let cfg = floptle_scene::load_project(&root.join("project.ron"));
        assert_eq!(cfg.model_texture_filter, Some(floptle_scene::ModelTextureFilterDoc::Crisp));
        let files = ["project.ron", "scenes/one.ron", "prefabs/pane.prefab.ron"];
        let before: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(root.join(f)).unwrap()).collect();
        let again = crate::upgrade::run(&root, None);
        assert!(again.iter().all(|d| d.result.as_ref().is_ok_and(Vec::is_empty)), "{:?}", crate::upgrade::report(&root, &again));
        let after: Vec<Vec<u8>> = files.iter().map(|f| std::fs::read(root.join(f)).unwrap()).collect();
        assert_eq!(before, after, "the second pass changed a file");
        // A 0.117 pre-release already drew models as their files say.
        let mut rc: floptle_scene::ProjectConfigDoc = ron::from_str("()").unwrap();
        pin_model_look(&mut rc, Some("0.117.0-rc6"));
        assert_eq!(rc.model_texture_filter, Some(floptle_scene::ModelTextureFilterDoc::AsFile));
        let _ = std::fs::remove_dir_all(&root);
    }
}
