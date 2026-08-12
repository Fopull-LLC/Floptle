//! **Packages** — modular expansions of the engine that anybody can write,
//! share and install.
//!
//! A package is a folder with a [`package.ron`](manifest) in it. What it holds
//! is up to it: Lua that runs *in the editor* (an editor extension), Lua the
//! *game* can attach to nodes, assets, prefabs, scenes, shaders, effects — or
//! nothing but a folder of textures. All four are the same kind of thing to a
//! project, and all four install the same way.
//!
//! ```text
//! my-package/
//!   package.ron      what it is, what it needs, what it may reach for
//!   editor/          Lua run in the editor  — ed.*, gui.*, handles.*
//!   scripts/         Lua the game can attach to nodes
//!   assets/          meshes, textures, prefabs, scenes, effects, shaders
//!   samples/         optional extras, copied in on request
//! ```
//!
//! ## The four moving parts
//!
//! - [`manifest`] — `package.ron`: identity, version, dependencies, the
//!   folders it ships, and the capabilities it declares.
//! - [`registry`] — `packages.ron` at the project root: what this project has
//!   installed and **where each one came from**, so a clone is reproducible.
//! - [`install`] — putting one in and taking it out: from a folder, from a Git
//!   URL, linked in place while you write it, or scaffolded new.
//! - [`resolve`] — the installed list plus every manifest, checked and put in
//!   dependency order. Nothing here can stop a project opening: a package that
//!   will not load is *skipped, loudly*.
//!
//! [`index`] is the fifth piece and the only one that is about the outside
//! world: the catalogue the package browser reads.
//!
//! ## Two rules worth stating once
//!
//! **Identity is the `id`, never the folder.** `pkg://com.example.grass/x.png`
//! finds that file whether the package was copied into the project, linked to a
//! working copy on another disk, or renamed on the way in.
//!
//! **A package can only be given what it asked for.** [`Permission`] is
//! declared in the manifest, shown before install, and checked when the editor
//! builds an extension's Lua environment — an undeclared capability is *absent*,
//! not merely refused at the call.

pub mod index;
pub mod install;
pub mod manifest;
pub mod registry;
pub mod resolve;
pub mod version;

pub use index::{Index, Listing, Release};
pub use install::InstallError;
pub use manifest::{
    Author, Dependency, DirKind, Manifest, ManifestError, Permission, Sample, MANIFEST_FILE,
};
pub use registry::{Entry, Registry, Source, PACKAGES_DIR, REGISTRY_FILE};
pub use resolve::{resolve, resolve_pkg_url, LoadReport, Loaded, Problem, Severity, PKG_SCHEME};
pub use version::{Version, VersionReq};

#[cfg(test)]
mod integration {
    //! One test that walks the whole path a real package takes: written on
    //! disk, installed into a project, resolved, addressed, disabled, removed.

    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flpkg-e2e-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_package_goes_in_loads_and_comes_out_again() {
        let base = temp("full");
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();

        // ---- written on disk, by hand ------------------------------------
        let src = base.join("grass");
        std::fs::create_dir_all(src.join("editor")).unwrap();
        std::fs::create_dir_all(src.join("assets/textures")).unwrap();
        std::fs::write(
            src.join("package.ron"),
            r#"(
                id: "com.example.grass",
                name: "Grass Tools",
                version: "1.2.0",
                description: "Paint grass",
                engine: ">=0.50.0",
                permissions: [Network],
                samples: [ (name: "Demo", path: "samples/demo", description: "A field") ],
            )"#,
        )
        .unwrap();
        std::fs::write(src.join("editor/main.lua"), "ed.log('hi')\n").unwrap();
        std::fs::write(src.join("assets/textures/blade.png"), b"\x89PNG").unwrap();
        std::fs::create_dir_all(src.join("samples/demo")).unwrap();
        std::fs::write(src.join("samples/demo/field.ron"), "()").unwrap();

        // ---- installed ---------------------------------------------------
        let entry = install::install_from_dir(&proj, &src, false).unwrap();
        assert_eq!(entry.version, Version::new(1, 2, 0));

        // ---- resolved ----------------------------------------------------
        let engine = Version::new(0, 55, 0);
        let report = resolve(&proj, &engine);
        assert_eq!(report.loaded.len(), 1, "{:?}", report.problems);
        let pkg = &report.loaded[0];
        assert!(pkg.grants(Permission::Network));
        assert!(!pkg.grants(Permission::Files));
        assert_eq!(pkg.editor_scripts().len(), 1);

        // ---- addressed ---------------------------------------------------
        let tex = resolve_pkg_url(&report.loaded, "pkg://com.example.grass/assets/textures/blade.png")
            .unwrap();
        assert!(tex.exists(), "{}", tex.display());

        // ---- a sample, copied in on request ------------------------------
        let sample = &pkg.manifest.samples[0];
        let dest = install::import_sample(
            &proj,
            &pkg.root,
            &pkg.manifest.name,
            &sample.name,
            &sample.path,
        )
        .unwrap();
        assert!(dest.join("field.ron").exists());

        // ---- switched off ------------------------------------------------
        install::set_enabled(&proj, "com.example.grass", false).unwrap();
        let off = resolve(&proj, &engine);
        assert!(off.loaded.is_empty());
        assert_eq!(off.disabled.len(), 1);
        install::set_enabled(&proj, "com.example.grass", true).unwrap();

        // ---- and out -----------------------------------------------------
        install::remove(&proj, "com.example.grass").unwrap();
        assert!(resolve(&proj, &engine).loaded.is_empty());
        assert!(!proj.join("packages/com.example.grass").exists());
        // The sample stays: it was copied into the project and is the
        // project's now.
        assert!(dest.join("field.ron").exists());
        // The folder it was installed FROM is untouched.
        assert!(src.join("package.ron").exists());

        let _ = std::fs::remove_dir_all(&base);
    }
}
