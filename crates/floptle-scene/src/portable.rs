//! Absolute file paths in a project's text files: finding them, turning the
//! ones inside the project back into project-relative ones, and writing every
//! file a developer authors through that pass.
//!
//! A path like `/home/you/Game/models/tower.glb` loads on the machine that
//! wrote it and nowhere else. The editor, `run`, `shot` and `check` all resolve
//! it fine there, so the first anyone hears of it is a build on somebody
//! else's machine with every building missing.
//!
//! **Every authored file is written through [`write`].** It finds the project
//! the file belongs to (the nearest folder above it holding `project.ron`) and
//! rewrites any absolute path inside that project to its relative form before
//! the bytes reach the disk. A tool that copies an absolute path out of memory
//! into a document (a clip's source model, a tileset's sheet, a material's
//! texture) then still writes a portable file, and no new kind of document has
//! to remember to ask. `floptle check` reports what slipped past anyway, and
//! `check --fix` repairs it.
//!
//! Scans text rather than parsed documents: a path can sit in any field of any
//! kind of file (a model, a texture, a sound, a material reference), and the
//! text is also what gets rewritten. A string counts as a path only when it
//! looks like one — absolute, with a directory part, and either on disk or
//! ending in a file extension — so a UI label reading `/quit` is left alone.

use std::path::{Path, PathBuf};

/// One absolute path found in a file.
#[derive(Clone, Debug, PartialEq)]
pub struct AbsRef {
    /// 1-based line.
    pub line: usize,
    /// The nearest `name: "…"` above it: the node (or effect, or track) it
    /// belongs to.
    pub owner: Option<String>,
    /// The field it is the value of, when it sits on a `key: "…"` line.
    pub field: Option<String>,
    pub value: String,
    /// Its project-relative form, when it points inside the project.
    pub relative: Option<String>,
}

/// Does `s` look like an absolute file path?
fn looks_absolute(s: &str) -> bool {
    let b = s.as_bytes();
    let unix = b.first() == Some(&b'/') && !s.starts_with("//");
    let drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/');
    let unc = s.starts_with("\\\\");
    if !(unix || drive || unc) {
        return false;
    }
    let body = s.trim_start_matches(['/', '\\']);
    let has_dir = body.contains('/') || body.contains('\\');
    let file = body.rsplit(['/', '\\']).next().unwrap_or("");
    let has_ext = file.rsplit_once('.').is_some_and(|(stem, ext)| {
        !stem.is_empty() && !ext.is_empty() && ext.len() <= 8 && ext.chars().all(|c| c.is_ascii_alphanumeric())
    });
    has_dir && (has_ext || floptle_vfs::exists(s))
}

/// The ways `root` can be spelled at the front of a path: as given, made
/// absolute, and with symlinks resolved.
fn root_spellings(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![root.to_path_buf()];
    if let Ok(a) = std::path::absolute(root) {
        out.push(a);
    }
    if let Some(c) = canonical(root) {
        out.push(c);
    }
    out.dedup();
    out
}

/// `value`'s path relative to the project, if it is inside it.
fn relative_to(value: &str, roots: &[PathBuf]) -> Option<String> {
    let p = Path::new(value);
    let canon = canonical(p);
    for r in roots {
        for cand in std::iter::once(p).chain(canon.as_deref()) {
            if let Ok(rel) = cand.strip_prefix(r) {
                let rel = rel.to_string_lossy().replace('\\', "/");
                if !rel.is_empty() {
                    return Some(rel);
                }
            }
        }
    }
    None
}

/// Every string literal in `text`, as `(byte range of its contents, line)`.
/// Escapes are skipped over, not decoded: a path with an escaped quote in it
/// is not a path anybody has.
fn string_literals(text: &str) -> Vec<(std::ops::Range<usize>, usize)> {
    let mut out = Vec::new();
    let b = text.as_bytes();
    let (mut i, mut line) = (0, 1);
    while i < b.len() {
        match b[i] {
            b'\n' => line += 1,
            b'"' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        j += 1;
                    } else if b[j] == b'\n' {
                        line += 1;
                    }
                    j += 1;
                }
                out.push((start..j.min(b.len()), line));
                i = j;
            }
            // A comment can hold a quote that opens nothing.
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The field a literal starting at byte `start` is the value of: the
/// identifier before `key:` just ahead of its opening quote, on any layout.
fn key_before(text: &str, start: usize) -> Option<&str> {
    let head = text[..start.saturating_sub(1)].trim_end();
    let head = head.strip_suffix(':')?.trim_end();
    let from = head
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map_or(0, |i| i + 1);
    let key = &head[from..];
    (!key.is_empty()).then_some(key)
}

/// `p` with symlinks resolved, where the platform has them to resolve.
fn canonical(p: &Path) -> Option<PathBuf> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        p.canonicalize().ok()
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = p;
        None
    }
}

/// The project a file at `path` belongs to: the nearest folder above it that
/// holds a `project.ron`. `None` for a file in no project.
pub fn project_root_of(path: &Path) -> Option<PathBuf> {
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    abs.ancestors().skip(1).find(|d| floptle_vfs::exists(d.join("project.ron"))).map(Path::to_path_buf)
}

/// `path` relative to `root` when it is inside it, with `/` separators;
/// anything else (already relative, or outside the project) as it was.
pub fn rel_path(path: &str, root: &Path) -> String {
    let slashed = path.replace('\\', "/");
    if let Ok(p) = Path::new(&slashed).strip_prefix(root) {
        return p.to_string_lossy().replace('\\', "/");
    }
    if (looks_absolute(&slashed) || Path::new(&slashed).is_absolute())
        && let Some(rel) = relative_to(&slashed, &root_spellings(root))
    {
        return rel;
    }
    slashed
}

/// Read an authored file, with every absolute path inside its project made
/// project-relative: [`write`]'s other half. A file an older editor saved
/// with absolute paths then comes into memory in the same spelling a save
/// writes, so nothing comparing one with the other finds two names for one
/// model. A file in no project is read as it is.
pub fn read(path: &Path) -> std::io::Result<String> {
    let text = floptle_vfs::read_to_string(path)?;
    // Nothing that looks absolute: the common case skips the root search.
    if !text.contains(":/") && !text.contains(":\\") && !text.contains("\"/") && !text.contains("\"\\\\") {
        return Ok(text);
    }
    Ok(match project_root_of(path) {
        Some(root) => relativize(&text, &root).0,
        None => text,
    })
}

/// Write an authored file: `text` with every absolute path inside its project
/// made project-relative (see the module docs). A file in no project is
/// written as given.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    match project_root_of(path) {
        Some(root) => floptle_vfs::write(path, relativize(text, &root).0),
        None => floptle_vfs::write(path, text),
    }
}

/// Every absolute path in `text`, with where it is.
pub fn find(text: &str, root: &Path) -> Vec<AbsRef> {
    let roots = root_spellings(root);
    let mut owner: Option<String> = None;
    let mut out = Vec::new();
    for (range, line) in string_literals(text) {
        let value = &text[range.clone()];
        // Track the nearest `name: "…"` as we go: literals come in file order.
        let key = key_before(text, range.start);
        if key == Some("name") {
            owner = Some(value.to_string());
            continue;
        }
        if !looks_absolute(value) {
            continue;
        }
        out.push(AbsRef {
            line,
            owner: owner.clone(),
            field: key.map(str::to_string),
            value: value.to_string(),
            relative: relative_to(value, &roots),
        });
    }
    out
}

/// `text` with every absolute path inside the project replaced by its
/// project-relative form, and how many were. Paths outside the project are
/// left as they are: there is nothing correct to replace them with.
pub fn relativize(text: &str, root: &Path) -> (String, usize) {
    let roots = root_spellings(root);
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut n = 0;
    for (range, _) in string_literals(text) {
        let value = &text[range.clone()];
        if !looks_absolute(value) {
            continue;
        }
        let Some(rel) = relative_to(value, &roots) else { continue };
        out.push_str(&text[last..range.start]);
        out.push_str(&rel);
        last = range.end;
        n += 1;
    }
    out.push_str(&text[last..]);
    (out, n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("floptle_abs_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(dir.join("models"));
        std::fs::write(dir.join("models/tower.glb"), b"glb").unwrap();
        dir
    }

    const SCENE: &str = r#"(
    name: "first",
    nodes: [
        (
            name: "Building 7",
            matter: Mesh(
                asset_path: "ROOT/models/tower.glb",
            ),
            id: Some(812),
        ),
        (
            name: "Radio",
            label: "/quit",
            sound: "/opt/shared/sfx/hum.ogg",
            texture: "textures/ok.png",
        ),
    ],
)"#;

    /// Finds the in-project path and the outside one, says which node and
    /// field each is, and leaves a label that only starts with a slash alone.
    #[test]
    fn finds_absolute_paths_with_their_node_and_field() {
        let root = project("find");
        let text = SCENE.replace("ROOT", &root.to_string_lossy());
        let got = find(&text, &root);
        assert_eq!(got.len(), 2, "{got:#?}");
        assert_eq!(got[0].owner.as_deref(), Some("Building 7"));
        assert_eq!(got[0].field.as_deref(), Some("asset_path"));
        assert_eq!(got[0].line, 7);
        assert_eq!(got[0].relative.as_deref(), Some("models/tower.glb"));
        assert_eq!(got[1].owner.as_deref(), Some("Radio"));
        assert_eq!(got[1].relative, None, "outside the project has no relative form");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **An authored file is written portable, and only in a project.** A
    /// document holding the project's own absolute path is written with the
    /// relative one; the same text written outside any project is untouched.
    #[test]
    fn write_relativizes_inside_a_project_and_nowhere_else() {
        let root = project("write");
        std::fs::write(root.join("project.ron"), "()").unwrap();
        std::fs::create_dir_all(root.join("materials")).unwrap();
        let text = SCENE.replace("ROOT", &root.to_string_lossy());
        let inside = root.join("materials/stone.ron");
        write(&inside, &text).unwrap();
        let got = std::fs::read_to_string(&inside).unwrap();
        assert!(got.contains(r#"asset_path: "models/tower.glb""#), "{got}");
        assert_eq!(read(&inside).unwrap(), got, "reading it back changes nothing more");
        // An older file with the absolute path reads back relative.
        std::fs::write(&inside, &text).unwrap();
        assert!(read(&inside).unwrap().contains(r#"asset_path: "models/tower.glb""#));

        let outside = std::env::temp_dir().join(format!("floptle_abs_outside_{}.ron", std::process::id()));
        write(&outside, &text).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), text, "a file in no project was rewritten");
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Relativizing rewrites the inside path, and only it, and is idempotent.
    #[test]
    fn relativize_rewrites_only_what_is_inside_the_project() {
        let root = project("fix");
        let text = SCENE.replace("ROOT", &root.to_string_lossy());
        let (fixed, n) = relativize(&text, &root);
        assert_eq!(n, 1);
        assert!(fixed.contains(r#"asset_path: "models/tower.glb""#), "{fixed}");
        assert!(fixed.contains("/opt/shared/sfx/hum.ogg"), "an outside path must be left alone");
        assert!(fixed.contains(r#"label: "/quit""#));
        assert_eq!(relativize(&fixed, &root), (fixed.clone(), 0), "a second pass changes nothing");
        let _ = std::fs::remove_dir_all(&root);
    }
}
