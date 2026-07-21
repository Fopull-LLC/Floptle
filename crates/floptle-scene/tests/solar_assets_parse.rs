//! The bundled `solar/` project's scenes and prefabs must always PARSE — a
//! hand-authored or generated `.ron` that drifts from the doc structs would
//! otherwise only fail at runtime, in the editor, on Ty's screen.

use std::path::PathBuf;

fn solar_dir() -> Option<PathBuf> {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../solar");
    d.exists().then_some(d)
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
