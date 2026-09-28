//! Absolute file paths in a project's `.ron` files: finding them, and turning
//! the ones inside the project back into project-relative ones.
//!
//! A path like `/home/you/Game/models/tower.glb` loads on the machine that
//! wrote it and nowhere else. The editor, `run`, `shot` and `check` all resolve
//! it fine there, so the first anyone hears of it is a build on somebody
//! else's machine with every building missing.
//!
//! Scans text rather than parsed documents: a path can sit in any field of any
//! kind of file (a model, a texture, a sound, a material reference), and the
//! text is also what gets rewritten. A string counts as a path only when it
//! looks like one — absolute, with a directory part, and either on disk or
//! ending in a file extension — so a UI label reading `/quit` is left alone.

use std::path::{Path, PathBuf};

/// One absolute path found in a file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AbsRef {
    /// 1-based line.
    pub(crate) line: usize,
    /// The nearest `name: "…"` above it: the node (or effect, or track) it
    /// belongs to.
    pub(crate) owner: Option<String>,
    /// The field it is the value of, when it sits on a `key: "…"` line.
    pub(crate) field: Option<String>,
    pub(crate) value: String,
    /// Its project-relative form, when it points inside the project.
    pub(crate) relative: Option<String>,
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
    if let Some(c) = crate::assets::canonical(root) {
        out.push(c);
    }
    out.dedup();
    out
}

/// `value`'s path relative to the project, if it is inside it.
fn relative_to(value: &str, roots: &[PathBuf]) -> Option<String> {
    let p = Path::new(value);
    let canon = crate::assets::canonical(p);
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

/// Every absolute path in `text`, with where it is.
pub(crate) fn find(text: &str, root: &Path) -> Vec<AbsRef> {
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
pub(crate) fn relativize(text: &str, root: &Path) -> (String, usize) {
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
