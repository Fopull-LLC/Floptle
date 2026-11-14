//! Upgrade step: a scene with a long shadow reach keeps its one shadow map.
//!
//! The sun shadow map draws a near cascade past a reach of
//! `Light::SHADOW_NEAR_FROM_DISTANCE` (160 m): a second map around the camera,
//! so a small caster near it keeps its shadow detail. That is the default for
//! a new project. A scene from before it drew one map, so this step writes
//! `shadow_near_distance: 0` into each scene whose lighting reaches past
//! 160 m and says nothing about the cascade, and that scene renders exactly
//! as it did. Setting `shadowNearDistance` (or the panel's "near detail")
//! turns the cascade on for it.
//!
//! What it does not touch: a scene at a reach of 160 m or less (the cascade
//! never draws there), one that already names `shadow_near_distance`, and
//! every file it has nothing to do in, which is left byte for byte.
//!
//! The one key goes in right after `lighting: (`, so the rest of the file,
//! its comments and its layout, stays as it was. The edit is checked by
//! reading the file back; a file it cannot edit that way is written whole
//! instead.
//!
//! **Safe to run any number of times:** a file that names
//! `shadow_near_distance` is never changed again.

use std::path::{Path, PathBuf};

use floptle_core::Light;

/// The upgrade step (`upgrade::STEPS`, "shadow-near-cascade-off"): every scene
/// under `root` whose lighting reaches past the cascade's distance and does
/// not mention it gets `shadow_near_distance: 0`; the rest is left byte for
/// byte.
pub(crate) fn keep_one_shadow_map(root: &Path, _from: Option<&str>) -> Result<Vec<PathBuf>, String> {
    let mut changed = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = floptle_vfs::read_dir(&dir) else { continue };
        for entry in rd {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.is_dir() {
                if !name.starts_with('.') && name != "target" {
                    stack.push(p);
                }
                continue;
            }
            // Only scenes carry a Lighting node's settings.
            if !name.ends_with(".ron")
                || name.ends_with(floptle_scene::VFX_EXT)
                || name.ends_with(".prefab.ron")
                || name == "project.ron"
            {
                continue;
            }
            let Ok(text) = floptle_vfs::read_to_string(&p) else { continue };
            if let Some(next) = upgrade_scene_text(&text) {
                floptle_scene::portable::write(&p, &next).map_err(|e| format!("{}: {e}", p.display()))?;
                changed.push(p);
            }
        }
    }
    Ok(changed)
}

/// A scene file's text with the cascade switched off, or `None` when it needs
/// nothing (or does not parse as a scene, which is some other step's
/// business).
pub(crate) fn upgrade_scene_text(text: &str) -> Option<String> {
    if text.contains("shadow_near_distance") {
        return None;
    }
    let doc = floptle_scene::from_ron(text).ok()?;
    if doc.lighting.shadow_distance.clamp(8.0, 400.0) <= Light::SHADOW_NEAR_FROM_DISTANCE {
        return None;
    }
    let mut want = doc;
    want.lighting.shadow_near_distance = 0.0;
    let want_ron = floptle_scene::to_ron(&want).ok()?;
    // The one key, in place; kept only if the file then reads as intended.
    if let Some(at) = lighting_open(text) {
        let next = format!("{}shadow_near_distance: 0.0, {}", &text[..at], &text[at..]);
        if floptle_scene::from_ron(&next).ok().and_then(|d| floptle_scene::to_ron(&d).ok()).as_deref() == Some(want_ron.as_str()) {
            return Some(next);
        }
    }
    Some(want_ron)
}

/// The byte just after the `(` that opens the scene's `lighting` value: the
/// `lighting` key one level inside the scene's own parentheses. Strings and
/// comments are skipped, so a name or a note that says "lighting" is not
/// taken for it.
fn lighting_open(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    let (mut i, mut depth) = (0usize, 0i32);
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 1;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b'l' if depth == 1 && text[i..].starts_with("lighting") && (i == 0 || !is_ident(b[i - 1])) => {
                let mut j = i + "lighting".len();
                if b.get(j).is_some_and(|&c| is_ident(c)) {
                    i += 1;
                    continue;
                }
                while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                    j += 1;
                }
                if b.get(j) != Some(&b':') {
                    i += 1;
                    continue;
                }
                j += 1;
                while b.get(j).is_some_and(u8::is_ascii_whitespace) {
                    j += 1;
                }
                return (b.get(j) == Some(&b'(')).then_some(j + 1);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An old scene at `reach`, with a comment and a node that both say
    /// "lighting", so the edit has to find the real key.
    fn old_scene(reach: f32) -> String {
        format!(
            "// the lighting of the valley\n(\n    name: \"lighting test\",\n    lighting: (direction: (0.4, 0.9, 0.45), shadows: true, shadow_map: true, shadow_distance: {reach}),\n    nodes: [\n        (name: \"lighting\", transform: (translation: (0.0, 0.0, 0.0)), matter: Empty),\n    ],\n)\n"
        )
    }

    /// **A long-reach scene from before the cascade is pinned to one map, in
    /// place.** The key goes in right after `lighting: (`, so the comment, the
    /// node called "lighting" and every other byte stay, and the scene reads
    /// back with the cascade off; a second pass changes nothing.
    #[test]
    fn a_long_reach_scene_keeps_one_map_and_the_rest_of_its_bytes() {
        let old = old_scene(364.0);
        let next = upgrade_scene_text(&old).expect("a 364 m scene needs the pin");
        assert_eq!(
            next,
            old.replace("lighting: (direction", "lighting: (shadow_near_distance: 0.0, direction"),
            "the edit moved more than the one key"
        );
        let light = floptle_scene::from_ron(&next).unwrap().lighting.to_light();
        assert_eq!(light.shadow_near_distance, 0.0);
        assert_eq!(light.shadow_distance, 364.0);
        assert_eq!(upgrade_scene_text(&next), None, "a second pass changed it again");
    }

    /// **A scene at or under the cascade's reach, or one that already chose,
    /// is left alone**, and so is a file that is not a scene.
    #[test]
    fn a_scene_the_cascade_never_reaches_is_left_alone() {
        for reach in [40.0, 150.0, 160.0] {
            assert_eq!(upgrade_scene_text(&old_scene(reach)), None, "a {reach} m scene was changed");
        }
        assert_eq!(upgrade_scene_text("(name: \"bare\", nodes: [])"), None, "the default lighting reaches 150 m");
        let chose = old_scene(364.0).replace("shadow_distance: 364", "shadow_near_distance: 24.0, shadow_distance: 364");
        assert_eq!(upgrade_scene_text(&chose), None, "a scene that set its cascade was changed");
        assert_eq!(upgrade_scene_text("not ron at all"), None);
    }

    /// **The whole step over a project:** the 364 m scene is rewritten and
    /// named, the 150 m scene and the project file stay byte for byte.
    #[test]
    fn the_step_rewrites_only_the_long_reach_scenes() {
        let root = std::env::temp_dir().join(format!("floptle-shadow-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes")).unwrap();
        let (far, near) = (old_scene(364.0), old_scene(150.0));
        std::fs::write(root.join("scenes/far.ron"), &far).unwrap();
        std::fs::write(root.join("scenes/near.ron"), &near).unwrap();
        std::fs::write(root.join("project.ron"), "(engine_version: Some(\"0.118.2\"))").unwrap();
        let wrote = keep_one_shadow_map(&root, Some("0.118.2")).unwrap();
        assert_eq!(wrote, vec![root.join("scenes/far.ron")]);
        assert_eq!(std::fs::read_to_string(root.join("scenes/near.ron")).unwrap(), near, "the 150 m scene was touched");
        assert_eq!(std::fs::read_to_string(root.join("project.ron")).unwrap(), "(engine_version: Some(\"0.118.2\"))");
        assert!(keep_one_shadow_map(&root, Some("0.118.2")).unwrap().is_empty(), "a second pass wrote again");
        let _ = std::fs::remove_dir_all(&root);
    }
}
