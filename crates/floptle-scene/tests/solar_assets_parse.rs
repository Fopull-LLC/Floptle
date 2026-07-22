//! The bundled `solar/` project's scenes and prefabs must always PARSE — a
//! hand-authored or generated `.ron` that drifts from the doc structs would
//! otherwise only fail at runtime, in the editor, on Ty's screen.

use std::path::PathBuf;

fn solar_dir() -> Option<PathBuf> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../solar");
    d.exists().then_some(d)
}

#[test]
fn solar_project_config_parses() {
    // project.ron carries the mixer (tracks + effects) — a bad track/effect
    // spelling should fail HERE, not silently drop the audio bus at runtime.
    let Some(solar) = solar_dir() else { return };
    let cfg = floptle_scene::try_load_project(&solar.join("project.ron"));
    let cfg = cfg.expect("project.ron parse error").expect("project.ron missing");
    assert!(
        cfg.mixer.tracks.iter().any(|t| t.name == "SFX"),
        "the solar mixer defines an SFX bus"
    );
}

#[test]
fn solar_scenes_parse() {
    let Some(solar) = solar_dir() else { return };
    let mut checked = 0;
    for entry in std::fs::read_dir(solar.join("scenes")).expect("solar/scenes") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("ron") {
            continue;
        }
        let doc = floptle_scene::load(&path);
        assert!(doc.is_ok(), "{} failed to parse: {:?}", path.display(), doc.err());
        checked += 1;
    }
    assert!(checked >= 4, "expected the solar scenes, found {checked}");
}

#[test]
fn solar_prefabs_parse() {
    let Some(solar) = solar_dir() else { return };
    let mut checked = 0;
    for entry in std::fs::read_dir(solar.join("prefabs")).expect("solar/prefabs") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.ends_with(".prefab.ron") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let body = text.trim_start().strip_prefix("//floptle-nodes-v1").unwrap_or(&text);
        let docs = ron::from_str::<Vec<floptle_scene::NodeDoc>>(body.trim_start());
        assert!(docs.is_ok(), "{name} failed to parse: {:?}", docs.err());
        checked += 1;
    }
    assert!(checked >= 9, "expected the part prefabs, found {checked}");
}

#[test]
fn solar_vfx_parse() {
    // Every bundled effect (`solar/vfx/*.vfx.ron`) must deserialize — a
    // color-curve typo or renamed enum variant would otherwise only fail
    // when the effect first spawns in-game (an invisible explosion).
    let Some(solar) = solar_dir() else { return };
    let mut checked = 0;
    for entry in std::fs::read_dir(solar.join("vfx")).expect("solar/vfx") {
        let path = entry.unwrap().path();
        if !path.file_name().unwrap().to_string_lossy().ends_with(".vfx.ron") {
            continue;
        }
        let doc = floptle_scene::load_vfx_effect(&path);
        assert!(doc.is_ok(), "{} failed to parse: {:?}", path.display(), doc.err());
        checked += 1;
    }
    assert!(checked >= 3, "expected the solar effects (Explosion/Flame/Smoke), found {checked}");
}

#[test]
fn solar_anims_parse() {
    // Baked clips (`*.anim.ron`) + controllers (`*.actl.ron`) discovered anywhere
    // under solar/ must deserialize — a bad channel/track (e.g. the generated
    // character cycles) would otherwise fail only when the animator first binds.
    let Some(solar) = solar_dir() else { return };
    fn walk(dir: &std::path::Path, clips: &mut u32, ctls: &mut u32) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, clips, ctls);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if name.ends_with(".anim.ron") {
                let d = floptle_scene::load_anim_clip(&path);
                assert!(d.is_ok(), "{} failed to parse: {:?}", path.display(), d.err());
                *clips += 1;
            } else if name.ends_with(".actl.ron") {
                let d = floptle_scene::load_anim_controller(&path);
                assert!(d.is_ok(), "{} failed to parse: {:?}", path.display(), d.err());
                *ctls += 1;
            }
        }
    }
    let (mut clips, mut ctls) = (0, 0);
    walk(&solar, &mut clips, &mut ctls);
    assert!(clips >= 3, "expected the character clips (idle/run/jump), found {clips}");
    assert!(ctls >= 1, "expected the character controller, found {ctls}");
}
