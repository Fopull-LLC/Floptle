//! Where themes come from, how they are read, and the choice of one.
//!
//! Three places, in the order the picker lists them:
//!
//! 1. **Built in.** `themes/<id>/theme.ron` in this crate, compiled in. The
//!    same format as everyone else's (contract §7.5): if the format cannot
//!    say something a built-in needs, the format is what gets fixed.
//! 2. **Yours.** The `themes/` folder beside the editor's settings
//!    (`~/.config/floptle/themes` on Linux). Each theme is a folder holding a
//!    `theme.ron`, or a `.floptletheme` file, which is that folder zipped.
//!    The Hub and the editor read the same folder and the same choice.
//! 3. **From a package.** An installed package's `themes/<id>/theme.ron` or
//!    `themes/*.floptletheme`. Packages belong to a project, so choosing one
//!    copies it into your folder, and it stays yours whichever project is
//!    open, and in the Hub.
//!
//! The choice is saved **by id** (§7.4), in `theme.ron` beside the themes
//! folder. The editor used to save an index into a hard-coded list; that file
//! is read once, translated to the id it meant, and not read again.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::color::Rgba;
use crate::model::{self, Assets, Origin, Theme, ThemeError, ThemeFile};

/// The file a theme's settings live in, inside its folder or zip.
pub const THEME_FILE: &str = "theme.ron";
/// The extension of a shareable, zipped theme.
pub const EXTENSION: &str = "floptletheme";
/// The default theme's id.
pub const DEFAULT_ID: &str = "floptle-dark";

/// A file inside a theme may be at most this big, and a whole theme this big.
/// A theme is a few images and a shader, and a zip that unpacks to gigabytes
/// is not one.
const MAX_FILE: u64 = 32 * 1024 * 1024;
const MAX_TOTAL: u64 = 96 * 1024 * 1024;

macro_rules! builtins {
    ($($id:literal),* $(,)?) => {
        /// Every built-in theme: `(id, theme.ron)`. The picker's order.
        pub const BUILTINS: &[(&str, &str)] = &[
            $(($id, include_str!(concat!("../themes/", $id, "/theme.ron"))),)*
        ];
    };
}
builtins!(
    "floptle-dark",
    "floptle-light",
    "midnight",
    "slate",
    "carbon",
    "high-contrast",
    "high-contrast-light",
    "galaxy",
    "aurora",
    "retrowave",
    "terminal",
    "paper",
);

/// What the old index-based setting meant, by position. Only ever read to
/// migrate it.
const LEGACY_INDEX: &[&str] = &["floptle-dark", "midnight", "slate", "carbon", "floptle-light"];

/// Where a theme's files are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Builtin(&'static str),
    Dir(PathBuf),
    Zip(PathBuf),
}

impl Source {
    /// What to call it in an error.
    pub fn label(&self) -> String {
        match self {
            Source::Builtin(id) => format!("built-in theme {id}"),
            Source::Dir(p) => p.join(THEME_FILE).display().to_string(),
            Source::Zip(p) => p.display().to_string(),
        }
    }

    fn read_ron(&self) -> Result<String, ThemeError> {
        let err = |m: String| ThemeError { file: self.label(), message: m };
        match self {
            Source::Builtin(id) => BUILTINS
                .iter()
                .find(|b| b.0 == *id)
                .map(|b| b.1.to_string())
                .ok_or_else(|| err(format!("there is no built-in theme {id:?}"))),
            Source::Dir(d) => std::fs::read_to_string(d.join(THEME_FILE)).map_err(|e| err(e.to_string())),
            Source::Zip(p) => {
                let (mut zip, prefix) = open_zip(p).map_err(err)?;
                read_zip_entry(&mut zip, &format!("{prefix}{THEME_FILE}"), 1024 * 1024)
                    .and_then(|b| String::from_utf8(b).map_err(|_| "theme.ron is not UTF-8 text".into()))
                    .map_err(err)
            }
        }
    }

    /// Read the files a theme names. `contents` false only checks they exist
    /// (what listing needs), and records each as empty.
    fn read_assets(&self, paths: &[String], contents: bool) -> Result<Assets, ThemeError> {
        let err = |m: String| ThemeError { file: self.label(), message: m };
        let mut out = Assets::default();
        let mut total = 0u64;
        for p in paths {
            safe_rel(p).map_err(err)?;
            let bytes: Vec<u8> = match self {
                Source::Builtin(_) => {
                    return Err(err(format!("names the file {p:?}, and a built-in theme ships no files")));
                }
                Source::Dir(d) => {
                    let f = d.join(p);
                    let meta = std::fs::metadata(&f).map_err(|_| err(format!("names {p:?}, which is not in the theme's folder")))?;
                    if meta.len() > MAX_FILE {
                        return Err(err(format!("{p:?} is {} MB; a theme's files are at most 32 MB each", meta.len() / 1_000_000)));
                    }
                    total += meta.len();
                    if contents { std::fs::read(&f).map_err(|e| err(format!("{p}: {e}")))? } else { Vec::new() }
                }
                Source::Zip(z) => {
                    let (mut zip, prefix) = open_zip(z).map_err(err)?;
                    let name = format!("{prefix}{p}");
                    let size = zip
                        .by_name(&name)
                        .map(|f| f.size())
                        .map_err(|_| err(format!("names {p:?}, which is not in the .floptletheme")))?;
                    if size > MAX_FILE {
                        return Err(err(format!("{p:?} is {} MB; a theme's files are at most 32 MB each", size / 1_000_000)));
                    }
                    total += size;
                    if contents { read_zip_entry(&mut zip, &name, MAX_FILE).map_err(err)? } else { Vec::new() }
                }
            };
            if total > MAX_TOTAL {
                return Err(err("the theme's files add up to more than 96 MB".into()));
            }
            out.files.insert(p.clone(), Arc::from(bytes.into_boxed_slice()));
        }
        Ok(out)
    }
}

/// A path inside a theme: relative, and never climbing out of it.
fn safe_rel(p: &str) -> Result<(), String> {
    let path = Path::new(p);
    if p.is_empty()
        || path.is_absolute()
        || p.starts_with('/')
        || p.starts_with('\\')
        || path.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("{p:?} must be a path inside the theme, such as images/back.png"));
    }
    Ok(())
}

type Zip = zip::ZipArchive<std::io::BufReader<std::fs::File>>;

/// Open a `.floptletheme` and find its `theme.ron`: at the root, or inside
/// the one folder somebody zipped. Returns the prefix the files sit under.
fn open_zip(p: &Path) -> Result<(Zip, String), String> {
    let f = std::fs::File::open(p).map_err(|e| e.to_string())?;
    let zip = zip::ZipArchive::new(std::io::BufReader::new(f))
        .map_err(|e| format!("is not a .floptletheme (a zip with theme.ron in it): {e}"))?;
    let mut best: Option<String> = None;
    for name in zip.file_names() {
        if let Some(prefix) = name.strip_suffix(THEME_FILE)
            && (prefix.is_empty() || prefix.ends_with('/'))
            && prefix.matches('/').count() <= 1
            && best.as_ref().is_none_or(|b| prefix.len() < b.len())
        {
            best = Some(prefix.to_string());
        }
    }
    let prefix = best.ok_or_else(|| "has no theme.ron inside it".to_string())?;
    Ok((zip, prefix))
}

fn read_zip_entry(zip: &mut Zip, name: &str, cap: u64) -> Result<Vec<u8>, String> {
    let f = zip.by_name(name).map_err(|_| format!("{name} is missing"))?;
    let mut out = Vec::new();
    f.take(cap + 1).read_to_end(&mut out).map_err(|e| format!("{name}: {e}"))?;
    if out.len() as u64 > cap {
        return Err(format!("{name} is larger than a theme file may be"));
    }
    Ok(out)
}

/// Parse a built-in's file. Panics only on a built-in that does not parse,
/// which a test makes impossible to ship.
fn builtin_file(id: &str) -> Option<ThemeFile> {
    let (_, text) = BUILTINS.iter().find(|b| b.0 == id)?;
    model::parse_file(text, id).ok()
}

/// A built-in's file merged down its chain to Floptle Dark.
fn builtin_merged(id: &str, depth: u32) -> Result<ThemeFile, String> {
    let f = builtin_file(id).ok_or_else(|| format!("there is no built-in theme {id:?}"))?;
    if id == DEFAULT_ID {
        return Ok(f);
    }
    if depth > 4 {
        return Err("the built-in themes extend each other in a circle".into());
    }
    let parent = f.extends.clone().unwrap_or_else(|| DEFAULT_ID.into());
    Ok(model::merge(&f, &builtin_merged(&parent, depth + 1)?))
}

/// Read and resolve the theme at `src`. `contents` false skips reading the
/// files it names (checking only that they are there): what listing needs.
pub fn load(src: &Source, origin: Origin, contents: bool) -> Result<Theme, ThemeError> {
    let label = src.label();
    let text = src.read_ron()?;
    let file = model::parse_file(&text, &label)?;
    let err = |m: String| ThemeError { file: label.clone(), message: m };
    let builtin = matches!(src, Source::Builtin(_));
    if !builtin && BUILTINS.iter().any(|b| b.0 == file.id) {
        return Err(err(format!(
            "its id {:?} is a built-in theme's; give it an id of its own (and extends: Some({:?}) to start from that one)",
            file.id, file.id
        )));
    }
    let merged = if builtin && file.id == DEFAULT_ID {
        file
    } else {
        let parent = file.extends.clone().unwrap_or_else(|| DEFAULT_ID.into());
        if !BUILTINS.iter().any(|b| b.0 == parent) {
            return Err(err(format!(
                "extends {parent:?}, which is not a built-in theme; a theme can extend {}",
                BUILTINS.iter().map(|b| b.0).collect::<Vec<_>>().join(", ")
            )));
        }
        model::merge(&file, &builtin_merged(&parent, 0).map_err(err)?)
    };
    // Only this file's own paths are read from this source: a built-in parent
    // names none.
    let paths = model::referenced_paths(&merged);
    let assets = src.read_assets(&paths, contents)?;
    for face in [&merged.fonts.ui, &merged.fonts.mono, &merged.fonts.display].into_iter().flatten() {
        if contents
            && let Some(b) = assets.files.get(face.as_str())
            && !crate::fonts::is_font(b)
        {
            return Err(err(format!("{face:?} is not a TrueType or OpenType font")));
        }
    }
    model::resolve(&merged, &label, assets, origin)
}

/// One theme in the picker.
#[derive(Clone, Debug)]
pub struct Entry {
    pub id: String,
    pub name: String,
    pub author: Option<String>,
    pub description: Option<String>,
    pub dark: bool,
    /// ground, surface, text, accent, accent_hi: enough to draw a card.
    pub swatch: [Rgba; 5],
    pub radius: f32,
    pub animated: bool,
    pub effects: bool,
    pub warnings: Vec<String>,
    pub origin: Origin,
    pub source: Source,
    /// The theme's own picture for its card, if it ships one.
    pub preview: Option<Arc<[u8]>>,
}

/// A theme that would not load, kept so the picker can say why.
#[derive(Clone, Debug)]
pub struct Refused {
    pub source: Source,
    pub error: ThemeError,
}

/// Every theme on this machine.
#[derive(Clone, Debug, Default)]
pub struct Library {
    pub entries: Vec<Entry>,
    pub refused: Vec<Refused>,
}

impl Library {
    /// Built-ins, then `user_dir`, then each `(package id, package root)`.
    pub fn scan(user_dir: Option<&Path>, packages: &[(String, PathBuf)]) -> Library {
        let mut lib = Library::default();
        for (id, _) in BUILTINS {
            lib.add(Source::Builtin(id), Origin::Builtin);
        }
        if let Some(d) = user_dir {
            for src in sources_in(d) {
                let p = match &src {
                    Source::Dir(p) | Source::Zip(p) => p.clone(),
                    Source::Builtin(_) => continue,
                };
                lib.add(src, Origin::User(p));
            }
        }
        for (pkg, root) in packages {
            for src in sources_in(&root.join("themes")) {
                let p = match &src {
                    Source::Dir(p) | Source::Zip(p) => p.clone(),
                    Source::Builtin(_) => continue,
                };
                lib.add(src, Origin::Package { package: pkg.clone(), path: p });
            }
        }
        lib
    }

    fn add(&mut self, src: Source, origin: Origin) {
        match load(&src, origin.clone(), false) {
            Ok(t) => {
                if let Some(prev) = self.entries.iter().position(|e| e.id == t.id) {
                    // A package's copy of a theme you already have: yours wins,
                    // since that is the one you chose or edited.
                    if matches!(origin, Origin::Package { .. }) {
                        return;
                    }
                    self.entries.remove(prev);
                }
                let k = &t.tokens;
                let preview = t
                    .preview
                    .as_ref()
                    .and_then(|p| src.read_assets(std::slice::from_ref(p), true).ok())
                    .and_then(|a| a.files.into_values().next());
                self.entries.push(Entry {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    author: t.author.clone(),
                    description: t.description.clone(),
                    dark: t.dark,
                    swatch: [k.solid(k.ground), k.solid(k.surface), k.text, k.accent, k.accent_hi],
                    radius: t.shape.radius,
                    animated: t.is_animated(),
                    effects: t.has_effects(),
                    warnings: t.warnings.clone(),
                    origin,
                    source: src,
                    preview,
                });
            }
            Err(error) => self.refused.push(Refused { source: src, error }),
        }
    }

    pub fn find(&self, id: &str) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// Load `id` completely (its images, shaders and fonts read).
    pub fn load(&self, id: &str) -> Result<Theme, ThemeError> {
        let e = self.find(id).ok_or_else(|| ThemeError {
            file: id.to_string(),
            message: "no theme with this id is installed".into(),
        })?;
        load(&e.source, e.origin.clone(), true)
    }
}

/// The theme sources directly inside `dir`: each `<x>/theme.ron` folder and
/// each `*.floptletheme`, sorted by name so the list does not shuffle.
pub fn sources_in(dir: &Path) -> Vec<Source> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<Source> = rd
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            if p.is_dir() && p.join(THEME_FILE).is_file() {
                Some(Source::Dir(p))
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(EXTENSION)) && p.is_file() {
                Some(Source::Zip(p))
            } else {
                None
            }
        })
        .collect();
    out.sort_by_key(|s| s.label());
    out
}

/// Copy a theme (a folder, or a `.floptletheme`) into `user_dir`, after
/// checking it loads. Replaces a theme of yours with the same id. Returns
/// its id.
pub fn install(src: &Source, user_dir: &Path) -> Result<String, ThemeError> {
    let t = load(src, Origin::Builtin, true)?;
    std::fs::create_dir_all(user_dir).map_err(|e| ThemeError { file: user_dir.display().to_string(), message: e.to_string() })?;
    // One id, one place: a folder and a zip of the same theme would be two
    // entries fighting over one id.
    let dir_dest = user_dir.join(&t.id);
    let zip_dest = user_dir.join(format!("{}.{EXTENSION}", t.id));
    let io = |e: std::io::Error| ThemeError { file: src.label(), message: e.to_string() };
    match src {
        Source::Builtin(_) => {}
        Source::Zip(p) => {
            if p != &zip_dest {
                let _ = std::fs::remove_dir_all(&dir_dest);
                std::fs::copy(p, &zip_dest).map_err(io)?;
            }
        }
        Source::Dir(p) => {
            if p != &dir_dest {
                let _ = std::fs::remove_file(&zip_dest);
                let _ = std::fs::remove_dir_all(&dir_dest);
                copy_dir(p, &dir_dest).map_err(io)?;
            }
        }
    }
    Ok(t.id)
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)?.flatten() {
        let (s, d) = (e.path(), to.join(e.file_name()));
        if s.is_dir() { copy_dir(&s, &d)? } else { std::fs::copy(&s, &d).map(|_| ())? }
    }
    Ok(())
}

/// Serialise a theme file the way a person would write it.
pub fn to_ron(file: &ThemeFile) -> String {
    let cfg = ron::ser::PrettyConfig::new()
        .depth_limit(4)
        .struct_names(false)
        .separate_tuple_members(false)
        .indentor("    ".to_string());
    let body = ron::ser::to_string_pretty(file, cfg).unwrap_or_default();
    format!("// A Floptle theme. Every key is optional; what is left out comes from the theme it extends.\n{body}\n")
}

/// Write `file` and the files it names to a `.floptletheme` at `dest`.
pub fn write_zip(file: &ThemeFile, assets: &Assets, dest: &Path) -> std::io::Result<()> {
    use std::io::Write as _;
    let f = std::fs::File::create(dest)?;
    let mut z = zip::ZipWriter::new(f);
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    z.start_file(THEME_FILE, opts)?;
    z.write_all(to_ron(file).as_bytes())?;
    for (path, bytes) in &assets.files {
        if safe_rel(path).is_ok() && !bytes.is_empty() {
            z.start_file(path.as_str(), opts)?;
            z.write_all(bytes)?;
        }
    }
    z.finish()?;
    Ok(())
}

/// Write `file` and its files as a theme folder `user_dir/<id>/`.
pub fn write_dir(file: &ThemeFile, assets: &Assets, user_dir: &Path) -> std::io::Result<PathBuf> {
    let dir = user_dir.join(&file.id);
    std::fs::create_dir_all(&dir)?;
    // A zip of the same id would shadow this folder on the next scan.
    let _ = std::fs::remove_file(user_dir.join(format!("{}.{EXTENSION}", file.id)));
    for (path, bytes) in &assets.files {
        if safe_rel(path).is_ok() && !bytes.is_empty() {
            let p = dir.join(path);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(p, bytes)?;
        }
    }
    std::fs::write(dir.join(THEME_FILE), to_ron(file))?;
    Ok(dir)
}

// ---------------------------------------------------------------------------
// The choice, and the knobs that make a theme cheap.
// ---------------------------------------------------------------------------

/// How much of a theme's imagery to draw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effects {
    /// Images, and shaders moving.
    #[default]
    Full,
    /// Images, and each shader drawn once and held. No per-frame cost.
    Still,
    /// Colours only: no images, no shaders. The theme's tokens still apply.
    Off,
}

/// The user's theme settings, shared by the Hub and the editor.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    /// The chosen theme's id.
    pub theme: String,
    pub effects: Effects,
    /// The resolution a shader backdrop is drawn at, against the window's.
    /// Backdrops are soft; half resolution is a quarter of the cost and looks
    /// the same.
    pub backdrop_scale: f32,
    /// How often a moving backdrop is redrawn, per second.
    pub backdrop_fps: f32,
    /// Hold backdrops still while the game is playing in the editor, so a
    /// theme never competes with the game for the GPU.
    pub pause_while_playing: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            theme: DEFAULT_ID.into(),
            effects: Effects::Full,
            backdrop_scale: 0.5,
            backdrop_fps: 30.0,
            pause_while_playing: true,
        }
    }
}

/// The editor's settings folder, where the theme choice and your themes live.
///
/// **The editor's, not the Hub's.** The Hub keeps its own settings under
/// `directories`' path, which differs from this one on Windows and macOS; the
/// theme is the one setting both programs share, so both read it here.
pub fn config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("floptle"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support/floptle"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|c| c.join("floptle"))
    }
}

/// `<config>/theme.ron`.
pub fn prefs_path(config_dir: &Path) -> PathBuf {
    config_dir.join("theme.ron")
}

/// `<config>/themes`.
pub fn user_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("themes")
}

/// The saved choice. When there is none yet, the editor's old index file
/// (`engine_theme`) is translated to the id it meant and saved, once.
pub fn load_prefs(config_dir: &Path) -> Prefs {
    if let Ok(text) = std::fs::read_to_string(prefs_path(config_dir))
        && let Ok(p) = ron::from_str::<Prefs>(&text)
    {
        return sanitize(p);
    }
    let mut p = Prefs::default();
    if let Ok(old) = std::fs::read_to_string(config_dir.join("engine_theme"))
        && let Ok(i) = old.trim().parse::<usize>()
        && let Some(id) = LEGACY_INDEX.get(i)
    {
        p.theme = (*id).into();
        save_prefs(config_dir, &p);
    }
    p
}

fn sanitize(mut p: Prefs) -> Prefs {
    p.backdrop_scale = if p.backdrop_scale.is_finite() { p.backdrop_scale.clamp(0.25, 1.0) } else { 0.5 };
    p.backdrop_fps = if p.backdrop_fps.is_finite() { p.backdrop_fps.clamp(5.0, 60.0) } else { 30.0 };
    if p.theme.is_empty() {
        p.theme = DEFAULT_ID.into();
    }
    p
}

pub fn save_prefs(config_dir: &Path, p: &Prefs) {
    let _ = std::fs::create_dir_all(config_dir);
    let text = ron::ser::to_string_pretty(p, ron::ser::PrettyConfig::new()).unwrap_or_default();
    // Written whole and renamed, so the other program polling this file never
    // reads half of it.
    let path = prefs_path(config_dir);
    let tmp = path.with_extension("ron.tmp");
    if std::fs::write(&tmp, text).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// When a file last changed, for noticing an edit made elsewhere: the other
/// program's choice, or the theme file somebody is writing in a text editor.
pub fn modified(p: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// When the theme at `src` last changed (the newest of its folder's files).
pub fn source_modified(src: &Source) -> Option<std::time::SystemTime> {
    match src {
        Source::Builtin(_) => None,
        Source::Zip(p) => modified(p),
        Source::Dir(d) => {
            let mut newest = modified(&d.join(THEME_FILE));
            fn walk(d: &Path, newest: &mut Option<std::time::SystemTime>, depth: u32) {
                if depth > 3 {
                    return;
                }
                for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        walk(&p, newest, depth + 1);
                    } else if let Some(m) = modified(&p) {
                        *newest = Some(newest.map_or(m, |n| n.max(m)));
                    }
                }
            }
            walk(d, &mut newest, 0);
            newest
        }
    }
}
