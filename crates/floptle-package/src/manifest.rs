//! `package.ron` — what a package says about itself.
//!
//! One file at the root of a package folder. Everything but `id`, `name` and
//! `version` has a default, so the smallest legal manifest is three lines and a
//! folder of assets is a package.
//!
//! ```ron
//! (
//!     id: "com.example.grasstools",
//!     name: "Grass Tools",
//!     version: "1.2.0",
//! )
//! ```
//!
//! **The `id` is the identity, not the folder name.** It is what a dependency
//! names, what `pkg://` addresses resolve through, and what the installed list
//! keys on — so a package can be renamed, moved, or vendored under any folder
//! and still be the same package.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::version::{Version, VersionReq};

/// Who wrote it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// One package this package needs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Dependency {
    pub id: String,
    /// A range — see [`VersionReq`]. Bare `"1.2.0"` means "compatible with".
    pub version: VersionReq,
}

/// An optional extra shipped alongside the package — example scenes, a demo
/// project, art nobody needs unless they asked. Samples are NOT loaded; they are
/// copied into the project on request, so a package can carry a hundred
/// megabytes of demo without costing every project that installs it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub name: String,
    /// Package-relative folder.
    pub path: String,
    #[serde(default)]
    pub description: String,
}

/// What a package is allowed to reach for. Declared in the manifest and shown
/// before it is installed — the point is that somebody deciding whether to trust
/// a package can see the answer without reading its Lua.
///
/// This is disclosure with teeth: [`crate::Loaded::grants`] is what the editor
/// checks before handing an extension the matching API, and an undeclared
/// capability is absent from the extension's Lua environment rather than
/// failing at the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Permission {
    /// `http.*` from an editor extension — talking to a server.
    Network,
    /// Reading and writing files anywhere in the project (`assets.write`,
    /// `assets.read` outside the package's own folder).
    Files,
    /// Opening a URL in the user's browser, and the loopback listener that a
    /// browser sign-in needs to hear the answer on.
    Browser,
}

impl Permission {
    /// One line, in the words of the person deciding whether to install it.
    pub fn describe(self) -> &'static str {
        match self {
            Permission::Network => "talk to servers over the network",
            Permission::Files => "read and write files in your project",
            Permission::Browser => "open pages in your browser and listen for the reply",
        }
    }

    pub const ALL: &'static [Permission] =
        &[Permission::Network, Permission::Files, Permission::Browser];
}

/// A parsed `package.ron`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    /// Reverse-DNS identity, e.g. `com.example.grasstools`. Unique across a
    /// project.
    pub id: String,
    /// What a person calls it.
    pub name: String,
    pub version: Version,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Which engine versions this works with, e.g. `">=0.55.0"`. Absent = any,
    /// which is a claim worth making deliberately.
    #[serde(default)]
    pub engine: Option<VersionReq>,
    #[serde(default)]
    pub dependencies: Vec<Dependency>,
    /// Folders of Lua run **in the editor** when the package loads.
    #[serde(default = "default_editor_dirs")]
    pub editor: Vec<String>,
    /// Folders of Lua the **game** can attach to nodes.
    #[serde(default = "default_script_dirs")]
    pub scripts: Vec<String>,
    /// Folders of assets — meshes, textures, prefabs, scenes, effects, shaders.
    #[serde(default = "default_asset_dirs")]
    pub assets: Vec<String>,
    #[serde(default)]
    pub samples: Vec<Sample>,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

fn default_editor_dirs() -> Vec<String> {
    vec!["editor".into()]
}
fn default_script_dirs() -> Vec<String> {
    vec!["scripts".into()]
}
fn default_asset_dirs() -> Vec<String> {
    vec!["assets".into()]
}

/// The manifest file's name, at the root of every package folder.
pub const MANIFEST_FILE: &str = "package.ron";

/// RON reading rules shared by `package.ron` and `packages.ron`: bare values
/// are accepted where an `Option` is expected.
pub(crate) fn ron_options() -> ron::Options {
    ron::Options::default().with_default_extension(ron::extensions::Extensions::IMPLICIT_SOME)
}

impl Manifest {
    /// The smallest manifest that validates — what "New Package…" writes.
    pub fn new(id: impl Into<String>, name: impl Into<String>, version: Version) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            version,
            description: String::new(),
            author: None,
            license: None,
            homepage: None,
            keywords: Vec::new(),
            engine: None,
            dependencies: Vec::new(),
            editor: default_editor_dirs(),
            scripts: default_script_dirs(),
            assets: default_asset_dirs(),
            samples: Vec::new(),
            permissions: Vec::new(),
        }
    }

    /// Read `<dir>/package.ron`.
    pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path).map_err(|e| ManifestError {
            path: path.clone(),
            message: if e.kind() == std::io::ErrorKind::NotFound {
                format!("no {MANIFEST_FILE} here — a package folder is the one holding it")
            } else {
                e.to_string()
            },
        })?;
        Manifest::parse(&text).map_err(|message| ManifestError { path, message })
    }

    /// Parse and validate manifest text.
    ///
    /// Optional fields may be written bare — `engine: ">=0.55.0"` rather than
    /// `engine: Some(">=0.55.0")`. This is a file people write by hand, and
    /// `Some(…)` is a Rust word showing through into an authoring format.
    /// `Some(…)` still parses, so a manifest the editor wrote reads back.
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let m: Manifest = ron_options().from_str(text).map_err(|e| e.to_string())?;
        m.validate()?;
        Ok(m)
    }

    /// Write `<dir>/package.ron`, pretty-printed so a human can edit it after.
    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        let cfg = ron::ser::PrettyConfig::new().struct_names(false);
        let text = ron::ser::to_string_pretty(self, cfg)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join(MANIFEST_FILE), format!("{text}\n"))
    }

    /// Everything wrong with this manifest, in one message — a package author
    /// fixing five typos should learn about five typos, not one per reload.
    pub fn validate(&self) -> Result<(), String> {
        let mut problems: Vec<String> = Vec::new();
        if let Err(e) = validate_id(&self.id) {
            problems.push(e);
        }
        if self.name.trim().is_empty() {
            problems.push("`name` is empty — it is what a person sees in the package list".into());
        }
        let mut seen = BTreeSet::new();
        for d in &self.dependencies {
            if let Err(e) = validate_id(&d.id) {
                problems.push(format!("dependency: {e}"));
            }
            if d.id == self.id {
                problems.push(format!("`{}` depends on itself", self.id));
            }
            if !seen.insert(d.id.clone()) {
                problems.push(format!("`{}` is named twice in `dependencies`", d.id));
            }
        }
        for dir in self.editor.iter().chain(&self.scripts).chain(&self.assets) {
            if let Err(e) = validate_rel(dir) {
                problems.push(e);
            }
        }
        for s in &self.samples {
            if s.name.trim().is_empty() {
                problems.push("a sample has no `name`".into());
            }
            if let Err(e) = validate_rel(&s.path) {
                problems.push(format!("sample `{}`: {e}", s.name));
            }
        }
        let mut perms = self.permissions.clone();
        perms.sort();
        perms.dedup();
        if perms.len() != self.permissions.len() {
            problems.push("`permissions` names the same capability twice".into());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }

    /// Does this package declare `p`?
    pub fn grants(&self, p: Permission) -> bool {
        self.permissions.contains(&p)
    }

    /// The content folders that actually exist under `root`, in the manifest's
    /// order. A package listing a folder it does not ship is not an error — the
    /// defaults name three folders and most packages have one.
    pub fn dirs_that_exist(&self, root: &Path, kind: DirKind) -> Vec<PathBuf> {
        let list = match kind {
            DirKind::Editor => &self.editor,
            DirKind::Scripts => &self.scripts,
            DirKind::Assets => &self.assets,
        };
        list.iter()
            .map(|d| root.join(d))
            .filter(|p| p.is_dir())
            .collect()
    }
}

/// Which of a manifest's three content lists to look at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirKind {
    Editor,
    Scripts,
    Assets,
}

/// A manifest that would not load, and where it lives.
#[derive(Clone, Debug)]
pub struct ManifestError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

impl std::error::Error for ManifestError {}

/// A package id is reverse-DNS: lowercase letters, digits, `-` and `_`, in dot-
/// separated parts, at least two of them.
///
/// Two parts minimum is the whole point — a bare `grass` is a name anybody might
/// pick, and two packages with one id is the failure this rule exists to
/// prevent.
pub fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("`id` is empty — it should look like `com.you.thing`".into());
    }
    let parts: Vec<&str> = id.split('.').collect();
    if parts.len() < 2 {
        return Err(format!(
            "`{id}` is not a package id — it needs at least two dot-separated parts, \
             like `com.you.{id}`, so two authors can both ship a `{id}`"
        ));
    }
    for p in &parts {
        if p.is_empty() {
            return Err(format!("`{id}` is not a package id — it has an empty part"));
        }
        if !p.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
            return Err(format!(
                "`{id}` is not a package id — each part must start with a lowercase letter"
            ));
        }
        if let Some(bad) = p.chars().find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '_'))
        {
            return Err(format!(
                "`{id}` is not a package id — `{bad}` is not allowed \
                 (lowercase letters, digits, `-` and `_`)"
            ));
        }
    }
    Ok(())
}

/// A content folder is package-relative and may not climb out of the package.
/// A manifest that could name `../../..` would make "install this package" mean
/// "let this package read my disk".
fn validate_rel(p: &str) -> Result<(), String> {
    if p.trim().is_empty() {
        return Err("a folder entry is empty".into());
    }
    let path = Path::new(p);
    if path.is_absolute() {
        return Err(format!("`{p}` is an absolute path — package folders are relative to the package"));
    }
    for c in path.components() {
        if matches!(c, std::path::Component::ParentDir) {
            return Err(format!("`{p}` climbs out of the package with `..`"));
        }
        if matches!(c, std::path::Component::Prefix(_) | std::path::Component::RootDir) {
            return Err(format!("`{p}` is not a package-relative folder"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_smallest_manifest_is_three_fields() {
        let m = Manifest::parse(
            r#"(
                id: "com.example.grass",
                name: "Grass",
                version: "1.0.0",
            )"#,
        )
        .unwrap();
        assert_eq!(m.id, "com.example.grass");
        // The content folders default rather than having to be spelled out.
        assert_eq!(m.editor, vec!["editor".to_string()]);
        assert_eq!(m.scripts, vec!["scripts".to_string()]);
        assert_eq!(m.assets, vec!["assets".to_string()]);
        assert!(m.permissions.is_empty());
    }

    /// A hand-written manifest should not have to say `Some(…)`, and one the
    /// editor wrote (which does) must still read back.
    #[test]
    fn optional_fields_read_bare_or_wrapped() {
        let bare = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0", engine: ">=0.55.0",
                 homepage: "https://example.com" )"#,
        )
        .unwrap();
        let wrapped = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0", engine: Some(">=0.55.0"),
                 homepage: Some("https://example.com") )"#,
        )
        .unwrap();
        assert_eq!(bare, wrapped);
        assert_eq!(bare.engine.unwrap().as_str(), ">=0.55.0");
    }

    #[test]
    fn round_trips_through_save_and_load() {
        let dir = std::env::temp_dir().join(format!("flpkg-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = Manifest::new("com.example.grass", "Grass", Version::new(1, 2, 3));
        m.description = "Grass".into();
        m.permissions = vec![Permission::Network];
        m.dependencies = vec![Dependency { id: "com.example.core".into(), version: "^1.0".parse().unwrap() }];
        m.engine = Some(">=0.55.0".parse().unwrap());
        m.save(&dir).unwrap();
        let back = Manifest::load(&dir).unwrap();
        assert_eq!(m, back);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_manifest_says_what_a_package_folder_is() {
        let dir = std::env::temp_dir().join("flpkg-nothing-here");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = Manifest::load(&dir).unwrap_err();
        assert!(err.message.contains("package.ron"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_id_needs_two_parts() {
        assert!(validate_id("grass").is_err());
        assert!(validate_id("com.example.grass").is_ok());
        assert!(validate_id("com.example").is_ok());
    }

    #[test]
    fn an_id_rejects_capitals_and_spaces() {
        assert!(validate_id("com.Example.grass").is_err());
        assert!(validate_id("com.example.my grass").is_err());
        assert!(validate_id("com.example.my-grass_2").is_ok());
        assert!(validate_id("com.example.").is_err());
        assert!(validate_id("com.2example.grass").is_err());
    }

    /// The rule that keeps "install this package" from meaning "read my disk".
    #[test]
    fn a_content_folder_may_not_climb_out() {
        let bad = Manifest::parse(
            r#"(
                id: "com.example.grass", name: "Grass", version: "1.0.0",
                assets: ["../../../etc"],
            )"#,
        );
        let err = bad.unwrap_err();
        assert!(err.contains(".."), "{err}");
        assert!(Manifest::parse(
            r#"( id: "com.example.grass", name: "Grass", version: "1.0.0", assets: ["/etc"] )"#
        )
        .is_err());
    }

    #[test]
    fn validation_reports_every_problem_at_once() {
        let err = Manifest::parse(
            r#"(
                id: "Grass",
                name: "",
                version: "1.0.0",
                assets: ["../x"],
            )"#,
        )
        .unwrap_err();
        assert!(err.contains("id"), "{err}");
        assert!(err.contains("name"), "{err}");
        assert!(err.contains(".."), "{err}");
        assert_eq!(err.lines().count(), 3, "{err}");
    }

    #[test]
    fn a_package_may_not_depend_on_itself() {
        let err = Manifest::parse(
            r#"(
                id: "com.example.grass", name: "Grass", version: "1.0.0",
                dependencies: [ (id: "com.example.grass", version: "*") ],
            )"#,
        )
        .unwrap_err();
        assert!(err.contains("itself"), "{err}");
    }

    #[test]
    fn permissions_default_to_none_and_are_checked_by_name() {
        let m = Manifest::parse(
            r#"( id: "com.example.g", name: "G", version: "1.0.0",
                 permissions: [Network, Browser] )"#,
        )
        .unwrap();
        assert!(m.grants(Permission::Network));
        assert!(m.grants(Permission::Browser));
        assert!(!m.grants(Permission::Files));
    }

    #[test]
    fn every_permission_describes_itself() {
        for p in Permission::ALL {
            assert!(!p.describe().is_empty(), "{p:?}");
        }
    }
}
