//! The editor's font stack, in one place.
//!
//! Two callers need the *same* stack and used to build it separately: the real
//! window in `main.rs` and `icons::test_context` for headless tests. That is the
//! shape of a drift bug — a test that draws in a font the editor does not have
//! answers a question nobody asked. Both call [`definitions`] now.
//!
//! Packages can add to it. A package names a typeface in its `package.ron`:
//!
//! ```ron
//! fonts: [ (name: "Heading", path: "fonts/Kimberley-Black.ttf") ]
//! ```
//!
//! and its Lua draws a run of widgets in it with `gui.font("Heading", fn)`. The
//! name is **scoped to the package**, so two packages may both ship a
//! `"Heading"`: the family egui sees is `<package id>:<name>`, which no package
//! can spell from Lua.
//!
//! Faces are read once per package load and merged into one `set_fonts` call.
//! egui rebuilds its glyph atlas on `set_fonts`, so doing it per frame — or once
//! per package — would be paying an alphabet's worth of rasterisation for a
//! heading. A project whose packages ship no fonts calls `set_fonts` exactly as
//! often as it did before this existed: once.

/// One typeface a package shipped, read off disk and ready to register.
#[derive(Clone, Debug)]
pub(crate) struct PackageFont {
    /// The family egui knows it by: `<package id>:<name>`. Built by
    /// [`family_key`] so the two ends cannot disagree.
    pub(crate) family: String,
    pub(crate) bytes: Vec<u8>,
}

/// The family name egui stores a package's face under.
///
/// Scoped by package id, because `"Heading"` is a name two packages will both
/// pick and neither should win.
pub(crate) fn family_key(pkg_id: &str, name: &str) -> String {
    format!("{pkg_id}:{name}")
}

/// Every character the editor's proportional stack can actually draw.
///
/// **Read from the fonts' character maps, not from `epaint::Fonts::has_glyph`.**
/// That call resolves a character to a face and compares it against the face
/// holding the replacement glyph, so a character whose real home happens to be
/// that face reports missing — upstream marks the case with a TODO. Acting on
/// it has already cost this repo one round of swapping away icons that were
/// fine (see `icons.rs`). The charmap is the fact the renderer acts on.
///
/// The **proportional chain only**, deliberately. A package face is checked
/// against the stack it falls back to rather than against itself, so a package
/// shipping a rare glyph in its own face is told `false` for it. That is the
/// conservative direction: the answer is used to pick between an icon and a
/// word, and a word where an icon would have worked is a smaller failure than a
/// box where a word would have worked.
pub(crate) fn drawable(defs: &egui::FontDefinitions) -> std::collections::HashSet<char> {
    let mut out = std::collections::HashSet::new();
    let names = defs
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for name in &names {
        let Some(data) = defs.font_data.get(name) else { continue };
        let Ok(font) = skrifa::FontRef::from_index(&data.font, data.index) else { continue };
        for (cp, _) in skrifa::MetadataProvider::charmap(&font).mappings() {
            if let Some(c) = char::from_u32(cp) {
                out.insert(c);
            }
        }
    }
    out
}

/// The editor's own stack, plus any package faces.
///
/// egui's proportional fallback (Ubuntu + the two emoji fonts) is missing many
/// of the arrow/geometry glyphs the editor uses as icons (→ ● ◌ ⊘ ⊕ …); Hack
/// covers them and already ships with egui, so it is appended or those labels
/// render as tofu squares.
pub(crate) fn definitions(packages: &[PackageFont]) -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();
    if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
        fam.push("Hack".into());
    }
    for f in packages {
        fonts.font_data.insert(
            f.family.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(f.bytes.clone())),
        );
        // Its own family, containing itself and then the editor's proportional
        // stack. The fallback matters: a display face ships an alphabet and not
        // much else, so a heading with an arrow or an emoji in it still draws.
        let mut chain = vec![f.family.clone()];
        chain.extend(
            fonts
                .families
                .get(&egui::FontFamily::Proportional)
                .cloned()
                .unwrap_or_default(),
        );
        fonts
            .families
            .insert(egui::FontFamily::Name(f.family.clone().into()), chain);
    }
    fonts
}

/// Read the faces a package declared, reporting each failure once.
///
/// A face that will not load is a warning and not an error: the panel draws in
/// the editor's type, which is worse-looking and entirely usable. Refusing to
/// load the package over a missing `.ttf` would be the wrong trade.
pub(crate) fn read_package_fonts(
    pkg_id: &str,
    root: &std::path::Path,
    declared: &[floptle_package::FontFace],
) -> (Vec<PackageFont>, Vec<String>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for face in declared {
        if face.name.is_empty() {
            problems.push("a font in package.ron has no name".to_owned());
            continue;
        }
        // Package-relative, and it has to stay that way: a font is read with no
        // permission declared, so the path must not be able to leave the folder.
        let rel = std::path::Path::new(&face.path);
        if rel.is_absolute() || rel.components().any(|c| c == std::path::Component::ParentDir) {
            problems.push(format!(
                "font {:?}: {:?} is outside the package folder",
                face.name, face.path
            ));
            continue;
        }
        match std::fs::read(root.join(rel)) {
            Ok(bytes) if is_font(&bytes) => out.push(PackageFont {
                family: family_key(pkg_id, &face.name),
                bytes,
            }),
            Ok(_) => problems.push(format!(
                "font {:?}: {:?} is not a TrueType or OpenType file",
                face.name, face.path
            )),
            Err(e) => problems.push(format!("font {:?}: {}: {e}", face.name, face.path)),
        }
    }
    (out, problems)
}

/// Does this look like a font file?
///
/// Checked here rather than left to egui, which **panics** on data it cannot
/// parse. A package that ships a mistyped path should cost a Console line, not
/// the editor.
fn is_font(bytes: &[u8]) -> bool {
    matches!(
        bytes.get(..4),
        Some(b"\x00\x01\x00\x00") | Some(b"true") | Some(b"ttcf") | Some(b"OTTO")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory with a per-test name. Cleared first: a run killed
    /// part-way through leaves its files behind, and a leftover fixture that
    /// poisons the next run reads as a code change.
    fn fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("floptle_fonts_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn a_real_font() -> Vec<u8> {
        // Whatever egui ships — the point is that it is a font, not which one.
        let defs = egui::FontDefinitions::default();
        let name = defs
            .families
            .get(&egui::FontFamily::Proportional)
            .and_then(|f| f.first())
            .expect("egui's default stack has a proportional font")
            .clone();
        defs.font_data[&name].font.to_vec()
    }

    #[test]
    fn a_project_with_no_package_fonts_gets_exactly_the_editors_stack() {
        let plain = definitions(&[]);
        let base = egui::FontDefinitions::default();
        assert_eq!(
            plain.font_data.len(),
            base.font_data.len(),
            "no package shipped a font, so nothing should have been added"
        );
        assert!(
            plain.families[&egui::FontFamily::Proportional].contains(&"Hack".to_owned()),
            "Hack is what draws the editor's icon glyphs"
        );
    }

    #[test]
    fn two_packages_may_both_ship_a_face_called_heading() {
        let faces = vec![
            PackageFont {
                family: family_key("com.example.lumen", "Heading"),
                bytes: a_real_font(),
            },
            PackageFont {
                family: family_key("com.other.tool", "Heading"),
                bytes: a_real_font(),
            },
        ];
        let defs = definitions(&faces);
        assert!(defs
            .families
            .contains_key(&egui::FontFamily::Name("com.example.lumen:Heading".into())));
        assert!(defs
            .families
            .contains_key(&egui::FontFamily::Name("com.other.tool:Heading".into())));
    }

    #[test]
    fn a_package_face_falls_back_to_the_editors_stack_for_glyphs_it_lacks() {
        let defs = definitions(&[PackageFont {
            family: family_key("com.a.b", "Display"),
            bytes: a_real_font(),
        }]);
        let chain = &defs.families[&egui::FontFamily::Name("com.a.b:Display".into())];
        assert_eq!(chain.first().map(String::as_str), Some("com.a.b:Display"));
        assert!(
            chain.len() > 1,
            "a display face with no emoji must not draw a heading as tofu"
        );
    }

    #[test]
    fn a_font_path_that_leaves_the_package_folder_is_refused() {
        let dir = fixture("escape");
        let (fonts, problems) = read_package_fonts(
            "com.a.b",
            &dir,
            &[floptle_package::FontFace {
                name: "Sneaky".into(),
                path: "../../../etc/passwd".into(),
            }],
        );
        assert!(fonts.is_empty());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("outside the package folder"), "{problems:?}");
    }

    #[test]
    fn a_file_that_is_not_a_font_is_a_console_line_and_not_a_panic() {
        let dir = fixture("not_a_font");
        std::fs::write(dir.join("nope.ttf"), b"this is not a font").unwrap();
        let (fonts, problems) = read_package_fonts(
            "com.a.b",
            &dir,
            &[floptle_package::FontFace {
                name: "Heading".into(),
                path: "nope.ttf".into(),
            }],
        );
        assert!(fonts.is_empty());
        assert!(problems[0].contains("not a TrueType"), "{problems:?}");
    }

    /// The check that the whole feature turns on, and the one a "does it
    /// register" test cannot make: text laid out in a package's family must
    /// come out a **different shape** from the same text in the editor's.
    ///
    /// Registering a family that silently falls back to the default looks
    /// exactly like registering one that works — right up until somebody looks
    /// at a panel and asks why the heading did not change.
    #[test]
    fn text_in_a_package_face_lays_out_differently_from_the_editors_type() {
        // A face that is unmistakably not the editor's: egui's monospace.
        let base = egui::FontDefinitions::default();
        let mono = base.families[&egui::FontFamily::Monospace][0].clone();
        let bytes = base.font_data[&mono].font.to_vec();

        let ctx = egui::Context::default();
        ctx.set_fonts(definitions(&[PackageFont {
            family: family_key("com.a.b", "Display"),
            bytes,
        }]));
        // Fonts do not exist until a frame has run.
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            },
            |_| {},
        );

        let measure = |family: egui::FontFamily| {
            ctx.fonts_mut(|f| {
                f.layout_no_wrap(
                    "Lumen iiii WWWW".to_owned(),
                    egui::FontId::new(20.0, family),
                    egui::Color32::WHITE,
                )
                .size()
                .x
            })
        };
        let editors = measure(egui::FontFamily::Proportional);
        let packages = measure(egui::FontFamily::Name("com.a.b:Display".into()));
        assert!(
            (editors - packages).abs() > 1.0,
            "the package face laid out the same width as the editor's ({editors} vs \
             {packages}) — it is registered but not being drawn with"
        );
    }

    #[test]
    fn a_font_that_loads_is_registered_under_its_scoped_name() {
        let dir = fixture("good_font");
        std::fs::write(dir.join("good.ttf"), a_real_font()).unwrap();
        let (fonts, problems) = read_package_fonts(
            "com.a.b",
            &dir,
            &[floptle_package::FontFace {
                name: "Heading".into(),
                path: "good.ttf".into(),
            }],
        );
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(fonts.len(), 1);
        assert_eq!(fonts[0].family, "com.a.b:Heading");
    }
}
