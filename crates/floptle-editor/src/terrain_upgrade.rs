//! Upgrade step: a project's planets keep the generator they were made by.
//!
//! A planet's save holds only the edits made to it, laid over the ground its
//! recipe generates when it loads. So a change to how the generator makes
//! ground would move the ground under every saved dig. Each change is keyed on
//! a generator version instead (`floptle_field::procgen`), a recipe that names
//! no version is made by the version its project names, and a project that
//! names none is made by the oldest.
//!
//! This step writes that version into `project.ron` for a project from before
//! the setting: the version of the engine it was last run on, read from its
//! stamp. Its saves, its on-node recipes and its scripts' `generatePlanet`
//! calls then make the worlds they made before the upgrade. A new project is
//! made with the current version and needs nothing.

use std::path::{Path, PathBuf};

/// The generator version the engine a project was stamped with made its
/// planets by. Each line is the release a change to the generator shipped in.
/// A project from before stamps, or before any of them, gets the oldest.
pub(crate) fn generator_for_stamp(from: Option<&str>) -> u32 {
    use floptle_field::procgen::{DEEP_ROCK_NEEDS_CAVES, LATITUDE_BANDS, RIVERS_FOLLOW_DRAINAGE};
    const SHIPPED: &[(&str, u32)] = &[
        ("0.117.0-rc3", RIVERS_FOLLOW_DRAINAGE),
        ("0.117.0-rc4", DEEP_ROCK_NEEDS_CAVES),
        ("0.117.0-rc7", LATITUDE_BANDS),
    ];
    SHIPPED.iter().filter(|(since, _)| !crate::upgrade::older(from, since)).map(|&(_, v)| v).max().unwrap_or(0)
}

/// Write the generator version a project's planets were made by into its
/// `project.ron`, when the file has none. A file that already names one, a
/// project with no `project.ron`, and one whose file does not parse are left
/// alone.
pub(crate) fn pin_terrain_generator(root: &Path, from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let path = root.join("project.ron");
    let mut cfg = match floptle_scene::try_load_project(&path) {
        Ok(Some(cfg)) => cfg,
        Ok(None) | Err(_) => return Ok(Vec::new()),
    };
    if cfg.terrain_generator.is_some() {
        return Ok(Vec::new());
    }
    cfg.terrain_generator = Some(generator_for_stamp(from));
    floptle_scene::save_project(&cfg, &path).map_err(|e| e.to_string())?;
    Ok(vec![path])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A project gets the generator its last engine made planets by.** A
    /// stamp before the first change, or none, is the oldest; each release
    /// that changed the generator, and every one after it until the next, is
    /// that change's version.
    #[test]
    fn a_stamp_names_the_generator_its_engine_had() {
        let v = |s: Option<&str>| generator_for_stamp(s);
        assert_eq!(v(None), 0);
        assert_eq!(v(Some("0.116.1")), 0);
        assert_eq!(v(Some("0.117.0-rc2")), 0);
        assert_eq!(v(Some("0.117.0-rc3")), 1);
        assert_eq!(v(Some("0.117.0-rc4")), 2);
        assert_eq!(v(Some("0.117.0-rc6")), 2);
        assert_eq!(v(Some("0.117.0-rc7")), 3);
        assert_eq!(v(Some("0.117.0")), floptle_field::procgen::GENERATOR_VERSION);
    }

    /// **The step writes the version once, and leaves a file that has one.**
    /// Run on a project from 0.117.0-rc4 it pins 2; run again, or on a
    /// project that already names a version, or one with no `project.ron`, it
    /// changes nothing.
    #[test]
    fn the_step_pins_the_generator_once() {
        let dir = std::env::temp_dir().join(format!("floptle_terrain_upgrade_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(pin_terrain_generator(&dir, Some("0.117.0-rc4")).unwrap().is_empty(), "a project.ron was made up");
        assert!(!dir.join("project.ron").exists());

        let path = dir.join("project.ron");
        std::fs::write(&path, "(retro: false, engine_version: Some(\"0.117.0-rc4\"))").unwrap();
        assert_eq!(pin_terrain_generator(&dir, Some("0.117.0-rc4")).unwrap(), vec![path.clone()]);
        let cfg = floptle_scene::try_load_project(&path).unwrap().unwrap();
        assert_eq!(cfg.terrain_generator, Some(2));
        let once = std::fs::read_to_string(&path).unwrap();
        assert!(pin_terrain_generator(&dir, Some("0.117.0-rc4")).unwrap().is_empty(), "the second run wrote again");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), once);

        // A file that names one keeps it, whatever the stamp says.
        std::fs::write(&path, "(terrain_generator: Some(1))").unwrap();
        assert!(pin_terrain_generator(&dir, None).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "(terrain_generator: Some(1))");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A file without the setting reads as the oldest generator; a new
    /// project names the current one.**
    #[test]
    fn an_old_project_file_reads_as_the_oldest_generator() {
        let old: floptle_scene::ProjectConfigDoc = ron::from_str("(retro: false)").unwrap();
        assert_eq!(old.terrain_generator, None);
        assert_eq!(old.terrain_generator_resolved(), 0);
        let new = floptle_scene::ProjectConfigDoc::for_new_project();
        assert_eq!(new.terrain_generator_resolved(), floptle_field::procgen::GENERATOR_VERSION);
    }
}
