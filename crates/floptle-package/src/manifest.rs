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

/// What a package **is**, as its author shelves it.
///
/// A closed list on purpose. Free-text categories are how a catalogue ends up
/// with `3d`, `3D`, `three-d` and `models` as four different shelves, none of
/// which can be filtered on. The long tail — "low-poly", "pixel", "sci-fi",
/// "PBR" — is what [`Manifest::keywords`] is for, and it is already searched.
///
/// Multi-valued, because a package can honestly be two things: an environment
/// kit and the tool that places it is one package, not two.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Category {
    /// Extends the editor itself — panels, tools, importers.
    EditorTool,
    /// Runtime Lua a game attaches to nodes.
    Scripts,
    /// Models, materials, environment kits.
    Art3D,
    /// Sprites, tilesets, textures, UI art.
    Art2D,
    /// Music, SFX, impulse responses.
    Audio,
    /// `.flsl` shaders and material presets.
    Shaders,
    /// Particle effects.
    Vfx,
    /// UI kits and themes.
    Ui,
    /// Typefaces.
    Fonts,
    /// Starter projects and example scenes.
    Template,
    /// Tables, configs, rule sets.
    Data,
}

impl Category {
    /// What a person browsing calls this shelf.
    pub fn label(self) -> &'static str {
        match self {
            Category::EditorTool => "Editor tools",
            Category::Scripts => "Scripts",
            Category::Art3D => "3D art",
            Category::Art2D => "2D art",
            Category::Audio => "Audio",
            Category::Shaders => "Shaders",
            Category::Vfx => "VFX",
            Category::Ui => "UI",
            Category::Fonts => "Fonts",
            Category::Template => "Templates",
            Category::Data => "Data",
        }
    }

    /// One line, for the author choosing between them.
    pub fn describe(self) -> &'static str {
        match self {
            Category::EditorTool => "extends the editor itself — panels, tools, importers",
            Category::Scripts => "runtime Lua a game attaches to nodes",
            Category::Art3D => "models, materials, environment kits",
            Category::Art2D => "sprites, tilesets, textures, UI art",
            Category::Audio => "music, sound effects, impulse responses",
            Category::Shaders => "shaders and material presets",
            Category::Vfx => "particle effects",
            Category::Ui => "UI kits and themes",
            Category::Fonts => "typefaces",
            Category::Template => "starter projects and example scenes",
            Category::Data => "tables, configs, rule sets",
        }
    }

    /// A small glyph, so a grid cell can say what it is without a word.
    pub fn glyph(self) -> &'static str {
        match self {
            Category::EditorTool => "⚒",
            Category::Scripts => "¶",
            Category::Art3D => "⬣",
            Category::Art2D => "🖼",
            Category::Audio => "♪",
            Category::Shaders => "◈",
            Category::Vfx => "✨",
            Category::Ui => "◫",
            Category::Fonts => "A",
            Category::Template => "⎙",
            Category::Data => "▤",
        }
    }

    pub const ALL: &'static [Category] = &[
        Category::EditorTool,
        Category::Scripts,
        Category::Art3D,
        Category::Art2D,
        Category::Audio,
        Category::Shaders,
        Category::Vfx,
        Category::Ui,
        Category::Fonts,
        Category::Template,
        Category::Data,
    ];
}

/// One picture or video showing what a package is.
///
/// **Exactly one** of `image` / `video` is set — a media entry that is both, or
/// neither, is a manifest mistake rather than something to render half of.
///
/// `image` and `poster` are **package-relative paths** in a `package.ron`, which
/// is what makes them work offline and while an author is still writing the
/// package. An absolute `http(s)` URL is accepted too — that is what the web
/// catalogue's `index.json` carries, resolved to the published revision, and it
/// means one type serves both files.
///
/// `video` is always an absolute URL. A repository should not carry video, and a
/// catalogue that clones one to show a thumbnail is a catalogue nobody browses.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Media {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub video: Option<String>,
    /// The still shown before a video plays.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,
    /// May be empty — a screenshot that speaks for itself needs no caption.
    #[serde(default)]
    pub caption: String,
}

impl Media {
    /// The path or URL to draw for this entry: the image, or a video's poster.
    /// `None` for a video with no poster, which is a play button and a caption.
    pub fn still(&self) -> Option<&str> {
        self.image.as_deref().or(self.poster.as_deref())
    }

    pub fn is_video(&self) -> bool {
        self.video.is_some()
    }
}

/// Is this an absolute `http(s)` address rather than a package-relative path?
pub fn is_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

/// A typeface a package ships and names, so its own panels can be drawn in it.
///
/// `name` is how the package's Lua asks for it and is scoped to that package —
/// two packages may both ship a face called `"Heading"` without either seeing
/// the other's. `path` is package-relative and points at a `.ttf` or `.otf`
/// inside the package folder.
///
/// Shipping a face is not a capability: reading a file inside your own folder is
/// what `require` already does, so this needs no permission.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontFace {
    pub name: String,
    pub path: String,
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
    /// Which shelves this belongs on. Empty is legal and means uncategorised —
    /// the catalogue has somewhere to put those rather than hiding them.
    #[serde(default)]
    pub categories: Vec<Category>,
    /// The square image that IS this package in a grid — in the editor's browser
    /// and on the site both. Package-relative, or an absolute URL. It has to
    /// survive being drawn at 128px.
    #[serde(default)]
    pub thumbnail: Option<String>,
    /// A wide image for the top of the package's own page. Optional; a package
    /// without one uses its thumbnail.
    #[serde(default)]
    pub banner: Option<String>,
    /// Screenshots and videos, in the order the author wants them seen.
    #[serde(default)]
    pub media: Vec<Media>,
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
    /// Typefaces this package ships and names, for drawing its own panels.
    #[serde(default)]
    pub fonts: Vec<FontFace>,
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
            categories: Vec::new(),
            thumbnail: None,
            banner: None,
            media: Vec::new(),
            engine: None,
            dependencies: Vec::new(),
            editor: default_editor_dirs(),
            scripts: default_script_dirs(),
            assets: default_asset_dirs(),
            samples: Vec::new(),
            fonts: Vec::new(),
            permissions: Vec::new(),
        }
    }

    /// Read `<dir>/package.ron`.
    pub fn load(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = floptle_vfs::read_to_string(&path).map_err(|e| ManifestError {
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
        floptle_vfs::create_dir_all(dir)?;
        floptle_vfs::write(dir.join(MANIFEST_FILE), format!("{text}\n"))
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
        let mut cats = self.categories.clone();
        cats.sort();
        cats.dedup();
        if cats.len() != self.categories.len() {
            problems.push("`categories` names the same shelf twice".into());
        }
        // Media paths are checked the same way content folders are: an image
        // path that could climb out of the package would make "show me this
        // package" mean "show me a file on your disk".
        for (label, p) in [("thumbnail", &self.thumbnail), ("banner", &self.banner)] {
            if let Some(p) = p
                && let Err(e) = validate_media_ref(p)
            {
                problems.push(format!("`{label}`: {e}"));
            }
        }
        for (i, m) in self.media.iter().enumerate() {
            match (&m.image, &m.video) {
                (Some(_), Some(_)) => problems.push(format!(
                    "media entry {} is both an `image` and a `video` — one entry shows one thing",
                    i + 1
                )),
                (None, None) => problems.push(format!(
                    "media entry {} has neither an `image` nor a `video`",
                    i + 1
                )),
                _ => {}
            }
            if let Some(v) = &m.video
                && !is_url(v)
            {
                problems.push(format!(
                    "media entry {}: `video` must be a full https:// address — a repository \
                     should not carry video",
                    i + 1
                ));
            }
            for p in [m.image.as_deref(), m.poster.as_deref()].into_iter().flatten() {
                if let Err(e) = validate_media_ref(p) {
                    problems.push(format!("media entry {}: {e}", i + 1));
                }
            }
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
            .filter(|p| floptle_vfs::is_dir(p))
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

/// A media reference is either an absolute `http(s)` URL or a package-relative
/// path that stays inside the package — the same rule as a content folder, for
/// the same reason.
fn validate_media_ref(p: &str) -> Result<(), String> {
    if is_url(p) {
        return Ok(());
    }
    if p.trim().is_empty() {
        return Err("an image path is empty".into());
    }
    validate_rel(p)
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
        floptle_vfs::create_dir_all(&dir).unwrap();
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

    // ---- what an art package needs to say about itself (0134) --------------

    #[test]
    fn an_art_package_declares_its_shelves_and_its_pictures() {
        let m = Manifest::parse(
            r#"(
                id: "com.fopull.brutalistkit",
                name: "Brutalist Kit",
                version: "1.0.0",
                categories: [Art3D, EditorTool],
                thumbnail: "media/icon.png",
                banner: "media/wide.png",
                media: [
                    (image: "media/overview.png", caption: "Every piece in the kit"),
                    (image: "media/detail.png"),
                    (video: "https://youtu.be/xxxx", poster: "media/poster.png",
                     caption: "60 seconds"),
                ],
            )"#,
        )
        .unwrap();
        assert_eq!(m.categories, vec![Category::Art3D, Category::EditorTool]);
        assert_eq!(m.thumbnail.as_deref(), Some("media/icon.png"));
        assert_eq!(m.media.len(), 3);
        assert!(!m.media[0].is_video());
        assert_eq!(m.media[1].caption, "", "a screenshot may speak for itself");
        assert!(m.media[2].is_video());
        assert_eq!(m.media[2].still(), Some("media/poster.png"));

        // The smallest manifest still has none of it.
        let bare = Manifest::new("com.e.g", "G", Version::new(1, 0, 0));
        assert!(bare.categories.is_empty());
        assert!(bare.thumbnail.is_none());
        assert!(bare.media.is_empty());
    }

    /// A media path that could climb out of the package would make "show me
    /// this package" mean "show me a file on your disk" — the same rule, and the
    /// same reason, as a content folder.
    #[test]
    fn a_media_path_may_not_climb_out_of_the_package() {
        let err = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 thumbnail: "../../.ssh/id_rsa" )"#,
        )
        .unwrap_err();
        assert!(err.contains(".."), "{err}");
        assert!(Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 media: [(image: "/etc/passwd")] )"#
        )
        .is_err());
        // An absolute URL is fine — that is what the web catalogue carries.
        assert!(Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 thumbnail: "https://example.com/icon.png" )"#
        )
        .is_ok());
    }

    #[test]
    fn a_media_entry_shows_exactly_one_thing() {
        let both = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 media: [(image: "a.png", video: "https://y/z")] )"#,
        )
        .unwrap_err();
        assert!(both.contains("both"), "{both}");

        let neither = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 media: [(caption: "nothing here")] )"#,
        )
        .unwrap_err();
        assert!(neither.contains("neither"), "{neither}");

        // A repository should not carry video, so a relative one is a mistake
        // worth naming rather than a file nobody can play.
        let local = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 media: [(video: "media/tour.mp4")] )"#,
        )
        .unwrap_err();
        assert!(local.contains("https://"), "{local}");
    }

    #[test]
    fn a_shelf_named_twice_is_a_mistake() {
        let err = Manifest::parse(
            r#"( id: "com.e.g", name: "G", version: "1.0.0",
                 categories: [Audio, Audio] )"#,
        )
        .unwrap_err();
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn the_new_fields_survive_save_and_load() {
        let dir = std::env::temp_dir().join(format!("flpkg-media-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut m = Manifest::new("com.example.kit", "Kit", Version::new(1, 0, 0));
        m.categories = vec![Category::Art2D, Category::Ui];
        m.thumbnail = Some("media/icon.png".into());
        m.media = vec![Media {
            image: Some("media/a.png".into()),
            caption: "A".into(),
            ..Default::default()
        }];
        m.save(&dir).unwrap();
        assert_eq!(Manifest::load(&dir).unwrap(), m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_shelf_names_and_describes_itself() {
        for c in Category::ALL {
            assert!(!c.label().is_empty(), "{c:?}");
            assert!(!c.describe().is_empty(), "{c:?}");
            assert!(!c.glyph().is_empty(), "{c:?}");
        }
        // The JSON spelling is the enum's own name, so package.ron and
        // index.json read alike — the site was told PascalCase.
        assert_eq!(serde_json::to_string(&Category::Art3D).unwrap(), "\"Art3D\"");
        assert_eq!(serde_json::to_string(&Category::EditorTool).unwrap(), "\"EditorTool\"");
    }
}
