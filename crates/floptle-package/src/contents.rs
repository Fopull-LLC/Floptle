//! What a package **demonstrably has**, counted from the files it ships.
//!
//! [`Manifest::categories`](crate::Category) is what the author *says* the
//! package is. This is what it *contains*, and nobody types it: it is a walk of
//! the content folders, classified by extension.
//!
//! That difference is the point. A catalogue with reach in it gets packages
//! shelved for reach — a prefab pack tagged `Audio` because that is where the
//! traffic is. A filter built on this cannot be gamed, because the only way to
//! contain audio is to ship audio.
//!
//! ```text
//! Brutalist Kit          categories: [Art3D]        ← the claim
//!                        contains:   models 42, textures 88, prefabs 6
//!                                                   ← the fact
//! ```
//!
//! # The rules are the interface
//!
//! The web catalogue computes the same facets from the same repository, so
//! these rules are shared with the site (floptle-platform `0134`) and pinned by
//! [`tests`]. Two implementations that disagree about the same package produce
//! a browser whose filters mean one thing in the editor and another on the web,
//! which is worse than having no filter at all.
//!
//! Facets come out in [`Facet::ALL`] order — the table's order, not discovery
//! order — so two implementations produce byte-identical lists.
//!
//! # What is not counted
//!
//! Anything under a `media/` folder. A screenshot of a model pack is not a
//! texture the pack ships, and a package whose only "textures" are its own
//! marketing shots would filter into the wrong shelf every time.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::manifest::{DirKind, Manifest};

/// One kind of thing a package can hold.
///
/// Serialised as the lowercase name (`"models"`), because it is a web file's
/// vocabulary before it is a Rust enum's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Facet {
    Models,
    Textures,
    Audio,
    Shaders,
    Prefabs,
    Vfx,
    Scenes,
    Materials,
    Fonts,
    Scripts,
    Editor,
    Animations,
    Tilesets,
}

impl Facet {
    /// Every facet, in the order they are reported. **This order is part of the
    /// interface** — see the module docs.
    pub const ALL: &'static [Facet] = &[
        Facet::Models,
        Facet::Textures,
        Facet::Audio,
        Facet::Shaders,
        Facet::Prefabs,
        Facet::Vfx,
        Facet::Scenes,
        Facet::Materials,
        Facet::Fonts,
        Facet::Scripts,
        Facet::Editor,
        Facet::Animations,
        Facet::Tilesets,
    ];

    /// The name in `index.json` and in a filter's query string.
    pub fn key(self) -> &'static str {
        match self {
            Facet::Models => "models",
            Facet::Textures => "textures",
            Facet::Audio => "audio",
            Facet::Shaders => "shaders",
            Facet::Prefabs => "prefabs",
            Facet::Vfx => "vfx",
            Facet::Scenes => "scenes",
            Facet::Materials => "materials",
            Facet::Fonts => "fonts",
            Facet::Scripts => "scripts",
            Facet::Editor => "editor",
            Facet::Animations => "animations",
            Facet::Tilesets => "tilesets",
        }
    }

    /// What a person filtering calls it — "has models".
    pub fn label(self) -> &'static str {
        match self {
            Facet::Models => "3D models",
            Facet::Textures => "textures",
            Facet::Audio => "audio",
            Facet::Shaders => "shaders",
            Facet::Prefabs => "prefabs",
            Facet::Vfx => "effects",
            Facet::Scenes => "scenes",
            Facet::Materials => "materials",
            Facet::Fonts => "fonts",
            Facet::Scripts => "game scripts",
            Facet::Editor => "editor tools",
            Facet::Animations => "animations",
            Facet::Tilesets => "tilesets",
        }
    }

    pub fn from_key(k: &str) -> Option<Facet> {
        Facet::ALL.iter().copied().find(|f| f.key() == k)
    }
}

/// Which facet a file belongs to, given the folder it was found under.
///
/// `rel` is the path **relative to the package root**, lowercased by the caller
/// or not — this lowercases it itself, because a `.PNG` is a texture.
///
/// `kind` is which manifest list the containing folder came from, which is the
/// only way to tell a game script from an editor script: both are `.lua`, and
/// what separates them is where the manifest said to look.
fn classify(rel: &str, kind: DirKind) -> Option<Facet> {
    let p = rel.to_ascii_lowercase().replace('\\', "/");

    // A package's own marketing art is not content it ships.
    if p == "media" || p.starts_with("media/") || p.contains("/media/") {
        return None;
    }

    // Compound extensions first — `.prefab.ron` must not be read as a scene
    // `.ron` that happens to live in the wrong folder.
    for (suffix, facet) in [
        (".prefab.ron", Facet::Prefabs),
        (".vfx.ron", Facet::Vfx),
        (".tileset.ron", Facet::Tilesets),
        (".anim.ron", Facet::Animations),
        (".actl.ron", Facet::Animations),
    ] {
        if p.ends_with(suffix) {
            return Some(facet);
        }
    }

    for (ext, facet) in [
        (".glb", Facet::Models),
        (".gltf", Facet::Models),
        (".png", Facet::Textures),
        (".jpg", Facet::Textures),
        (".jpeg", Facet::Textures),
        (".webp", Facet::Textures),
        (".bmp", Facet::Textures),
        (".tga", Facet::Textures),
        (".tif", Facet::Textures),
        (".tiff", Facet::Textures),
        (".gif", Facet::Textures),
        (".qoi", Facet::Textures),
        (".wav", Facet::Audio),
        (".ogg", Facet::Audio),
        (".mp3", Facet::Audio),
        (".flac", Facet::Audio),
        (".flsl", Facet::Shaders),
        (".ttf", Facet::Fonts),
        (".otf", Facet::Fonts),
    ] {
        if p.ends_with(ext) {
            return Some(facet);
        }
    }

    // A bare `.ron` is a scene or a material depending on the folder it is in —
    // the same rule the project's own asset browser uses.
    if p.ends_with(".ron") {
        if p.contains("scenes/") {
            return Some(Facet::Scenes);
        }
        if p.contains("materials/") {
            return Some(Facet::Materials);
        }
        return None;
    }

    // Lua is a game script or an editor tool by which list its folder came from.
    if p.ends_with(".lua") {
        return Some(match kind {
            DirKind::Editor => Facet::Editor,
            _ => Facet::Scripts,
        });
    }

    None
}

/// What a package holds, and how much of each.
///
/// A facet with no files is **absent**, not present with a count of zero: "has
/// audio: 0" is a row in a filter that means nothing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Contents {
    counts: BTreeMap<Facet, u32>,
}

impl Contents {
    /// Walk `root`'s content folders and count what is there.
    ///
    /// A folder the manifest names but does not ship contributes nothing —
    /// the defaults name three folders and most packages have one.
    pub fn scan(manifest: &Manifest, root: &Path) -> Contents {
        let mut counts: BTreeMap<Facet, u32> = BTreeMap::new();
        for kind in [DirKind::Editor, DirKind::Scripts, DirKind::Assets] {
            for dir in manifest.dirs_that_exist(root, kind) {
                walk(&dir, &dir, kind, &mut counts, &mut 0);
            }
        }
        Contents { counts }
    }

    /// Build from an already-counted map — how the catalogue's `contains` list
    /// is read back, where counts are not carried.
    pub fn from_facets(facets: impl IntoIterator<Item = Facet>) -> Contents {
        Contents { counts: facets.into_iter().map(|f| (f, 1)).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    pub fn has(&self, f: Facet) -> bool {
        self.counts.contains_key(&f)
    }

    pub fn count(&self, f: Facet) -> u32 {
        self.counts.get(&f).copied().unwrap_or(0)
    }

    /// Present facets, in [`Facet::ALL`] order, with their counts.
    pub fn facets(&self) -> Vec<(Facet, u32)> {
        Facet::ALL
            .iter()
            .filter_map(|f| self.counts.get(f).map(|n| (*f, *n)))
            .collect()
    }

    /// Just the facet names, in order — what `index.json` carries.
    pub fn keys(&self) -> Vec<&'static str> {
        self.facets().into_iter().map(|(f, _)| f.key()).collect()
    }

    /// One line for a package row: `42 models · 88 textures · 6 prefabs`.
    pub fn summary(&self) -> String {
        self.facets()
            .into_iter()
            .map(|(f, n)| format!("{n} {}", f.label()))
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// A ceiling on how much of a package tree is walked.
///
/// A package that vendors a `node_modules` or a `.git` — and people do — must
/// not turn "draw this row" into a filesystem crawl. The count stops being
/// interesting long before this; what matters past it is only *whether* a facet
/// is present, and by then it is.
const MAX_FILES: u32 = 20_000;

fn walk(dir: &Path, base: &Path, kind: DirKind, out: &mut BTreeMap<Facet, u32>, seen: &mut u32) {
    let Ok(entries) = floptle_vfs::read_dir(dir) else { return };
    for e in entries {
        if *seen >= MAX_FILES {
            return;
        }
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        // Version control and editor droppings are not package contents.
        if name.starts_with('.') {
            continue;
        }
        if e.is_dir() {
            walk(&path, base, kind, out, seen);
        } else {
            *seen += 1;
            let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy().to_string();
            if let Some(f) = classify(&rel, kind) {
                *out.entry(f).or_insert(0) += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Version;

    fn temp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flpkg-contents-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        floptle_vfs::create_dir_all(p.parent().unwrap()).unwrap();
        floptle_vfs::write(p, b"x").unwrap();
    }

    /// **This test is the interface with the website** (floptle-platform 0134).
    /// If it changes, the site's derivation changes with it or the two halves
    /// disagree about the same package.
    #[test]
    fn every_facet_is_derived_from_the_extension_the_site_was_told_about() {
        for (rel, kind, want) in [
            ("a.glb", DirKind::Assets, Some(Facet::Models)),
            ("a.gltf", DirKind::Assets, Some(Facet::Models)),
            ("t/a.PNG", DirKind::Assets, Some(Facet::Textures)),
            ("t/a.qoi", DirKind::Assets, Some(Facet::Textures)),
            ("s/a.ogg", DirKind::Assets, Some(Facet::Audio)),
            ("s/a.flac", DirKind::Assets, Some(Facet::Audio)),
            ("a.flsl", DirKind::Assets, Some(Facet::Shaders)),
            ("a.prefab.ron", DirKind::Assets, Some(Facet::Prefabs)),
            ("a.vfx.ron", DirKind::Assets, Some(Facet::Vfx)),
            ("a.tileset.ron", DirKind::Assets, Some(Facet::Tilesets)),
            ("a.anim.ron", DirKind::Assets, Some(Facet::Animations)),
            ("a.actl.ron", DirKind::Assets, Some(Facet::Animations)),
            ("scenes/a.ron", DirKind::Assets, Some(Facet::Scenes)),
            ("materials/a.ron", DirKind::Assets, Some(Facet::Materials)),
            ("f/a.ttf", DirKind::Assets, Some(Facet::Fonts)),
            ("f/a.otf", DirKind::Assets, Some(Facet::Fonts)),
            ("a.lua", DirKind::Scripts, Some(Facet::Scripts)),
            ("a.lua", DirKind::Editor, Some(Facet::Editor)),
            // Nothing we classify.
            ("README.md", DirKind::Assets, None),
            ("a.ron", DirKind::Assets, None),
            ("a.txt", DirKind::Assets, None),
        ] {
            assert_eq!(classify(rel, kind), want, "{rel} ({kind:?})");
        }
    }

    /// A compound extension must win over the bare one it ends with, or every
    /// prefab in a `scenes/` folder counts as a scene.
    #[test]
    fn a_compound_extension_beats_the_bare_one_inside_it() {
        assert_eq!(classify("scenes/town.prefab.ron", DirKind::Assets), Some(Facet::Prefabs));
        assert_eq!(classify("materials/fire.vfx.ron", DirKind::Assets), Some(Facet::Vfx));
    }

    /// A package's own screenshots are not textures it ships — otherwise every
    /// package with marketing art filters as an art package.
    #[test]
    fn marketing_art_is_not_content() {
        assert_eq!(classify("media/icon.png", DirKind::Assets), None);
        assert_eq!(classify("media/shots/wide.png", DirKind::Assets), None);
        assert_eq!(classify("kit/media/poster.png", DirKind::Assets), None);
        // …but a folder that merely starts with the same letters is content.
        assert_eq!(classify("mediaeval/wall.png", DirKind::Assets), Some(Facet::Textures));
    }

    #[test]
    fn a_scan_counts_what_is_there_and_omits_what_is_not() {
        let root = temp("scan");
        touch(&root, "assets/models/wall.glb");
        touch(&root, "assets/models/floor.glb");
        touch(&root, "assets/textures/wall.png");
        touch(&root, "assets/README.md");
        touch(&root, "media/icon.png");
        touch(&root, "scripts/door.lua");
        touch(&root, "editor/tool.lua");

        let m = Manifest::new("com.e.kit", "Kit", Version::new(1, 0, 0));
        let c = Contents::scan(&m, &root);

        assert_eq!(c.count(Facet::Models), 2);
        assert_eq!(c.count(Facet::Textures), 1, "the media/ icon must not be counted");
        assert_eq!(c.count(Facet::Scripts), 1);
        assert_eq!(c.count(Facet::Editor), 1);
        // Absent, not zero.
        assert!(!c.has(Facet::Audio));
        assert_eq!(c.count(Facet::Audio), 0);

        // Reported in ALL order regardless of what the filesystem handed back.
        assert_eq!(c.keys(), vec!["models", "textures", "scripts", "editor"]);
        assert!(c.summary().starts_with("2 3D models · 1 textures"), "{}", c.summary());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same `.lua` is a game script or an editor tool by which manifest list
    /// its folder came from — there is nothing else to tell them apart.
    #[test]
    fn lua_is_sorted_by_the_folder_the_manifest_named() {
        let root = temp("lua");
        touch(&root, "tools/a.lua");
        touch(&root, "game/b.lua");
        let mut m = Manifest::new("com.e.k", "K", Version::new(1, 0, 0));
        m.editor = vec!["tools".into()];
        m.scripts = vec!["game".into()];
        m.assets = vec![];
        let c = Contents::scan(&m, &root);
        assert_eq!(c.count(Facet::Editor), 1);
        assert_eq!(c.count(Facet::Scripts), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_package_that_ships_nothing_contains_nothing() {
        let root = temp("empty");
        floptle_vfs::create_dir_all(&root).unwrap();
        let m = Manifest::new("com.e.k", "K", Version::new(1, 0, 0));
        let c = Contents::scan(&m, &root);
        assert!(c.is_empty());
        assert_eq!(c.summary(), "");
        assert!(c.keys().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn facet_keys_round_trip_and_are_unique() {
        let mut keys: Vec<&str> = Facet::ALL.iter().map(|f| f.key()).collect();
        let n = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "two facets share a key");
        for f in Facet::ALL {
            assert_eq!(Facet::from_key(f.key()), Some(*f));
        }
        assert_eq!(Facet::from_key("nonsense"), None);
    }
}

#[cfg(test)]
mod kit_tests {
    use super::*;
    use crate::Manifest;

    /// The sample kit that ships in this repo, checked as the thing it claims to
    /// be. It is the catalogue's first multi-category listing and the fixture
    /// the site derives its own facets against, so "what does this package
    /// actually contain" is worth being an assertion rather than a belief.
    #[test]
    fn the_fofighter_kit_contains_what_its_manifest_claims() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../packages/fofighter-kit");
        if !root.exists() {
            return; // Packaged crate without the repo around it.
        }
        let m = Manifest::load(&root).expect("the kit's manifest parses");
        m.validate().expect("…and is valid");

        let held = Contents::scan(&m, &root);
        let facets: Vec<Facet> = held.facets().into_iter().map(|(f, _)| f).collect();
        assert_eq!(
            facets,
            vec![Facet::Models, Facet::Audio, Facet::Shaders, Facet::Fonts],
            "the kit should hold exactly these, in `Facet::ALL` order — which is \
             part of the interface, so a listing and a site agree on it"
        );
        assert_eq!(held.count(Facet::Models), 5);
        assert_eq!(held.count(Facet::Audio), 4);
        assert_eq!(held.count(Facet::Fonts), 1);
        assert_eq!(held.count(Facet::Shaders), 1);

        // Nine gallery images, a thumbnail and a banner — and not one of them
        // counted as content. A package whose marketing art filtered it as an
        // art package would be the exact bug `media/` is excluded to avoid,
        // and this kit is the first listing with enough of it to notice.
        assert_eq!(held.count(Facet::Textures), 0, "media/ is not content");

        // It ships no code at all, which is the claim the README makes to
        // anybody nervous about installing it.
        assert_eq!(held.count(Facet::Scripts), 0);
        assert_eq!(held.count(Facet::Editor), 0);
    }
}
