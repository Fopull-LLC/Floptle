//! The format is proven by the built-ins (contract §7.5): every one loads,
//! resolves completely, and compiles its shaders.

use crate::model::{self, Origin};
use crate::source::{self, BUILTINS, Library, Source};

#[test]
fn every_builtin_theme_loads() {
    for (id, _) in BUILTINS {
        let t = source::load(&Source::Builtin(id), Origin::Builtin, true)
            .unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(&t.id, id, "a built-in's folder and id disagree");
        // Built-ins are the showcase: none of them may be hard to read.
        assert!(t.warnings.is_empty(), "{id}: {:?}", t.warnings);
    }
}

#[test]
fn every_builtin_shader_compiles() {
    for (name, src) in crate::backdrop::BUILTIN_SHADERS {
        crate::backdrop::validate(src).unwrap_or_else(|e| panic!("builtin:{name}:\n{e}"));
    }
}

/// Floptle Dark carries the brand's exact values (contract §2).
#[test]
fn floptle_dark_is_the_brand() {
    let t = crate::default_theme();
    let k = &t.tokens;
    let hex = |c: crate::Rgba| c.to_hex();
    assert_eq!(hex(k.ground), "#141517");
    assert_eq!(hex(k.surface), "#1b1b1b");
    assert_eq!(hex(k.surface_2), "#202020");
    assert_eq!(hex(k.well), "#101112");
    assert_eq!(hex(k.hairline), "#2c2e31");
    assert_eq!(hex(k.text), "#e8e8ea");
    assert_eq!(hex(k.dim), "#9a9ba0");
    assert_eq!(hex(k.faint), "#6b6d72");
    assert_eq!(hex(k.accent), "#00c8a0");
    assert_eq!(hex(k.accent_hi), "#7fe4cd");
    assert_eq!(hex(k.on_accent), "#07120f");
    assert_eq!(k.accent_wash, crate::Rgba([0, 200, 160, 26]));
    assert_eq!((t.shape.radius, t.shape.radius_small), (6.0, 4.0));
    assert!(t.shape.shadow.is_none(), "hairlines, never shadows");
    assert_eq!((t.fonts.ui.as_str(), t.fonts.mono.as_str(), t.fonts.display.as_str()),
        ("IBM Plex Sans", "IBM Plex Mono", "Bricolage Grotesque"));
}

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("floptle_theme_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A theme that sets only its accent gets every other value from Floptle
/// Dark, never from egui's stock look (§7.1).
#[test]
fn a_missing_key_falls_back_to_floptle_dark() {
    let d = scratch("fallback");
    std::fs::create_dir_all(d.join("mine")).unwrap();
    std::fs::write(
        d.join("mine/theme.ron"),
        r##"(id: "mine", name: "Mine", colors: (accent: "#ff0080", on_accent: "#000000"))"##,
    )
    .unwrap();
    let t = source::load(&Source::Dir(d.join("mine")), Origin::Builtin, true).unwrap();
    let dark = crate::default_theme();
    assert_eq!(t.tokens.accent.to_hex(), "#ff0080");
    assert_eq!(t.tokens.ground, dark.tokens.ground);
    assert_eq!(t.fonts, dark.fonts);
    // The wash follows the new accent, because Floptle Dark writes it as a
    // reference to the accent rather than as a number.
    assert_eq!(t.tokens.accent_wash, crate::Rgba([255, 0, 128, 26]));
}

/// A theme that fails is refused whole, with a sentence naming the key, and
/// never half-applies (§7.6).
#[test]
fn a_bad_theme_is_refused_by_name() {
    let cases: &[(&str, &str)] = &[
        (r##"(id: "x", name: "X", colors: (acent: "#fff"))"##, "acent"),
        (r##"(id: "x", name: "X", colors: (ground: "teal"))"##, "colors.ground"),
        (r##"(id: "x", name: "X", colors: (accent: "#ff0000"))"##, "on_accent"),
        (r##"(id: "x", name: "X", colors: (dim: "$nothing"))"##, "$nothing"),
        (r##"(id: "x", name: "X", extends: "nope")"##, "nope"),
        (r##"(id: "x", name: "X", format: 9)"##, "newer Floptle"),
        (r##"(id: "floptle-dark", name: "Mine")"##, "built-in"),
        (r##"(id: "x", name: "X", surfaces: {"ground": (layers: [Image(path: "../../etc/passwd")])})"##, "inside the theme"),
        (r##"(id: "x", name: "X", surfaces: {"ground": (layers: [Shader(shader: "builtin:nope")])})"##, "builtin:galaxy"),
    ];
    for (i, (text, needle)) in cases.iter().enumerate() {
        let d = scratch(&format!("bad{i}"));
        std::fs::write(d.join("theme.ron"), text).unwrap();
        let Err(e) = source::load(&Source::Dir(d.clone()), Origin::Builtin, true) else {
            panic!("case {i} loaded: {text}");
        };
        assert!(e.to_string().contains(needle), "case {i}: {e}");
        assert!(e.to_string().contains("theme.ron"), "the error names the file: {e}");
    }
}

/// Text that is hard to read still loads, and somebody is told (§7.7).
#[test]
fn low_contrast_is_warned_about_and_still_loads() {
    let d = scratch("contrast");
    std::fs::write(d.join("theme.ron"), r##"(id: "dim", name: "Dim", colors: (text: "#2a2b2e"))"##).unwrap();
    let t = source::load(&Source::Dir(d), Origin::Builtin, true).unwrap();
    assert!(t.warnings.iter().any(|w| w.contains("text on ground")), "{:?}", t.warnings);
}

/// Folder → zip → folder: what one person exports is what another imports,
/// images and all.
#[test]
fn a_floptletheme_round_trips_with_its_files() {
    let d = scratch("zip");
    let src = d.join("src");
    std::fs::create_dir_all(src.join("images")).unwrap();
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([10, 20, 30, 255]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(src.join("images/a.png"), &png).unwrap();
    std::fs::write(
        src.join("theme.ron"),
        r##"(id: "zipped", name: "Zipped", surfaces: {"panel": (fill: "#00000080", layers: [Image(path: "images/a.png", fit: Tile)])})"##,
    )
    .unwrap();
    let t = source::load(&Source::Dir(src), Origin::Builtin, true).unwrap();
    let z = d.join("zipped.floptletheme");
    source::write_zip(&t.file, &t.assets, &z).unwrap();
    let user = d.join("user");
    let id = source::install(&Source::Zip(z), &user).unwrap();
    assert_eq!(id, "zipped");
    let lib = Library::scan(Some(&user), &[]);
    let back = lib.load("zipped").unwrap();
    assert_eq!(&*back.assets.files["images/a.png"], &png[..]);
    assert!(back.has_effects());
}

/// The choice is saved by id; the old index file is read once and means the
/// theme it used to mean (§7.4).
#[test]
fn the_old_index_migrates_to_an_id_once() {
    let d = scratch("prefs");
    std::fs::write(d.join("engine_theme"), "3").unwrap();
    assert_eq!(source::load_prefs(&d).theme, "carbon");
    assert!(source::prefs_path(&d).exists(), "the migration is written down");
    // …and once written, the old file no longer decides anything.
    std::fs::write(d.join("engine_theme"), "1").unwrap();
    assert_eq!(source::load_prefs(&d).theme, "carbon");
}

/// A theme inside a package is found at `themes/<id>/theme.ron`.
#[test]
fn a_package_theme_is_listed() {
    let d = scratch("pkg");
    let root = d.join("com.example.themes");
    std::fs::create_dir_all(root.join("themes/sunset")).unwrap();
    std::fs::write(root.join("themes/sunset/theme.ron"), r##"(id: "sunset", name: "Sunset")"##).unwrap();
    let lib = Library::scan(None, &[("com.example.themes".into(), root)]);
    let e = lib.find("sunset").expect("listed");
    assert!(matches!(&e.origin, model::Origin::Package { package, .. } if package == "com.example.themes"));
}

/// The region chain: a tab with nothing of its own shows the panel's
/// layers, which show the ground's.
#[test]
fn a_tab_inherits_the_grounds_backdrop() {
    let lib = Library::scan(None, &[]);
    let g = lib.load("galaxy").unwrap();
    let (_, layers) = g.surface("tab.inspector");
    assert!(matches!(layers.first(), Some(model::Layer::Shader { .. })));
    let (_, code) = g.surface("code_editor");
    assert!(code.is_empty(), "code sits on a solid well, never on stars");
    let dark = lib.load("floptle-dark").unwrap();
    assert!(dark.surface("tab.inspector").1.is_empty());
}

/// A program applies its theme before its first frame (the editor does,
/// every frame, before `run_ui`), and egui has no fonts at all until that
/// frame. Asking for them then panics: this crashed the editor on start.
/// And on the first frame after `set_fonts`, the display family is not bound
/// yet, so naming it panics too.
#[test]
fn applying_a_theme_before_the_first_frame_does_not_panic() {
    let ctx = egui::Context::default();
    crate::apply(&ctx, crate::default_theme(), &crate::Prefs::default());
    ctx.set_fonts(crate::fonts::definitions(&crate::default_theme()));
    let input = || egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(800.0, 600.0))),
        ..Default::default()
    };
    for _ in 0..3 {
        crate::apply(&ctx, crate::default_theme(), &crate::Prefs::default());
        let _ = ctx.run_ui(input(), |ui| {
            ui.heading("Title");
            ui.label(crate::look::title(ui, "Title"));
        });
    }
}
