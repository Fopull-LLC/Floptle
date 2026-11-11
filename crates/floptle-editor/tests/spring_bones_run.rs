//! **Spring bones swing a rigged model's chain, and a disabled chain does
//! not** (card 0693). Rigged Sae's hair (a root and four bones below it) as
//! a chain, the world's gravity turned sideways and strong: after a second
//! the hair's tip is well off where the clip holds it, toward the pull.
//! With the chain disabled it is exactly where the clip holds it. Read the
//! way a game reads it, `anim:boneWorld` under `floptle run`.

#![cfg(feature = "editor-ui")]

use std::process::Command;

/// Where Sae's hair tip is after 60 frames, with `springs` (a node field, or
/// empty for none) on her node.
fn hand_after_a_second(tag: &str, springs: &str) -> [f64; 3] {
    hair_after_a_second(tag, springs, "")
}

/// [`hand_after_a_second`], the script running `each` (Lua, with `a` the
/// animator) every frame before it looks.
fn hair_after_a_second(tag: &str, springs: &str, each: &str) -> [f64; 3] {
    let d = std::env::temp_dir().join(format!("flspring-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("scenes")).unwrap();
    std::fs::create_dir_all(d.join("scripts")).unwrap();
    let sae = concat!(env!("CARGO_MANIFEST_DIR"), "/../floptle-assets/tests/fixtures/Sae.mirrored.rigged.glb");
    std::fs::copy(sae, d.join("Sae.glb")).unwrap();
    std::fs::write(d.join("project.ron"), "(title: Some(\"b\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
    std::fs::write(
        d.join("scripts/ask.lua"),
        format!("local frames = 0\n\
         function update(node, dt)\n\
         \x20 local a = find('Sae'):animator()\n\
         \x20 {each}\n\
         \x20 frames = frames + 1\n\
         \x20 if frames == 60 then\n\
         \x20   local p = find('Sae'):animator():boneWorld('Hair_4')\n\
         \x20   print(string.format('HAND %.5f %.5f %.5f', p.x, p.y, p.z))\n\
         \x20 end\n\
         end\n"),
    )
    .unwrap();
    std::fs::write(
        d.join("scenes/first.ron"),
        format!(
            "(name: \"s\", nodes: [\
             (name: \"Sae\", matter: Mesh(asset_path: \"Sae.glb\"){springs}),\
             (name: \"Driver\", scripts: [(kind: \"ask\")])])"
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_floptle"))
        .args(["run", &d.to_string_lossy(), "--frames", "62"])
        .output()
        .expect("run floptle");
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&d);
    let line = text.lines().find(|l| l.contains("HAND ")).unwrap_or_else(|| panic!("{tag}: no hand position:\n{text}"));
    let v: Vec<f64> = line.split("HAND ").nth(1).unwrap().split_whitespace().map(|x| x.parse().unwrap()).collect();
    [v[0], v[1], v[2]]
}

#[test]
fn a_spring_chain_swings_the_hair_and_a_disabled_one_does_not() {
    let chain = |enabled: bool| {
        format!(
            ", spring_bones: Some((chains: [(name: \"Hair\", root: \"Hair_root\", stiffness: 0.0, drag: 0.3, max_angle: 170.0, enabled: {enabled})], gravity: (40.0, 0.0, 0.0)))"
        )
    };
    let still = hand_after_a_second("none", "");
    let swung = hand_after_a_second("on", &chain(true));
    let off = hand_after_a_second("off", &chain(false));
    let dist = |a: [f64; 3], b: [f64; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(dist(still, swung) > 0.05, "sideways gravity moved the hair tip only {:.4} (clip {still:?}, sprung {swung:?})", dist(still, swung));
    assert!(swung[0] > still[0], "the hair did not swing toward +X, where gravity pulls: clip {still:?}, sprung {swung:?}");
    assert!(dist(still, off) < 1e-4, "a disabled chain moved the hair {:.5}", dist(still, off));
}

/// **A script switches a chain off, and sets the gravity it feels.** Off
/// from the first frame (`anim:setSpringEnabled`), the hair stays where the
/// clip holds it; with the gravity turned the other way
/// (`anim:setSpringGravity`), it swings the other way.
#[test]
fn a_script_switches_a_chain_off_and_turns_its_gravity() {
    let chain = ", spring_bones: Some((chains: [(name: \"Hair\", root: \"Hair_root\", stiffness: 0.0, drag: 0.3, max_angle: 170.0)], gravity: (40.0, 0.0, 0.0)))";
    let still = hand_after_a_second("s-none", "");
    let off = hair_after_a_second("s-off", chain, "a:setSpringEnabled('Hair', false)");
    let back = hair_after_a_second("s-back", chain, "a:setSpringGravity(-40, 0, 0)");
    let dist = |a: [f64; 3], b: [f64; 3]| ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();
    assert!(dist(still, off) < 1e-4, "a chain a script switched off moved the hair {:.5}", dist(still, off));
    assert!(back[0] < still[0] - 0.01, "gravity turned to -X did not swing the hair that way: clip {still:?}, sprung {back:?}");
}

/// **The swung hair is what is drawn.** `floptle shot` of rigged Sae a
/// second in, her hair under sideways gravity, against the same shot with
/// the chain disabled: the pictures differ where the hair is. Set
/// `FLOPTLE_SPRING_PNG` to a folder to keep the two pictures.
#[test]
fn a_shot_draws_the_swung_hair() {
    let shoot = |tag: &str, enabled: bool| -> Option<image::RgbaImage> {
        let d = std::env::temp_dir().join(format!("flspringshot-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("scenes")).unwrap();
        let sae = concat!(env!("CARGO_MANIFEST_DIR"), "/../floptle-assets/tests/fixtures/Sae.mirrored.rigged.glb");
        std::fs::copy(sae, d.join("Sae.glb")).unwrap();
        std::fs::write(d.join("project.ron"), "(title: Some(\"b\"), entry_scene: Some(\"scenes/first.ron\"))").unwrap();
        std::fs::write(
            d.join("scenes/first.ron"),
            format!(
                "(name: \"s\", lighting: (direction: (0.3, 0.6, 0.7), ambient: (0.3, 0.3, 0.3)), nodes: [\
                 (name: \"Camera\", transform: (translation: (0.0, -1.0, 9.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)), matter: Camera(fov_y: 0.9, active: true)),\
                 (name: \"Sae\", matter: Mesh(asset_path: \"Sae.glb\"), spring_bones: Some((chains: [(name: \"Hair\", root: \"Hair_root\", stiffness: 0.0, drag: 0.3, max_angle: 170.0, enabled: {enabled})], gravity: (40.0, 0.0, 0.0))))])"
            ),
        )
        .unwrap();
        let png = d.join("shot.png");
        let r = std::process::Command::new(env!("CARGO_BIN_EXE_floptle"))
            .args(["shot", &d.to_string_lossy(), "--scene", "first", "--size", "320x240", "--after", "1.0", "--out", &png.to_string_lossy()])
            .output()
            .expect("run shot");
        if !r.status.success() {
            let why = String::from_utf8_lossy(&r.stderr);
            assert!(why.contains("no GPU") || why.contains("could not build the renderer"), "shot failed:\n{why}");
            return None;
        }
        let img = image::open(&png).expect("a PNG was written").to_rgba8();
        if let Ok(dir) = std::env::var("FLOPTLE_SPRING_PNG") {
            let _ = std::fs::copy(&png, std::path::Path::new(&dir).join(format!("{tag}.png")));
        }
        let _ = std::fs::remove_dir_all(&d);
        Some(img)
    };
    let Some(swung) = shoot("swung", true) else { return };
    let Some(still) = shoot("still", false) else { return };
    let lit = still.pixels().filter(|p| p[0] > 20 || p[1] > 20 || p[2] > 20).count();
    assert!(lit > 2000, "the model is not in the picture: {lit} lit pixels");
    let differ = swung.pixels().zip(still.pixels()).filter(|(a, b)| a != b).count();
    assert!(differ > 200, "the swung hair drew the same picture as the still one ({differ} pixels differ)");
}
