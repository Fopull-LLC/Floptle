//! The bundle: one file holding a project folder, for a page to fetch.
//!
//! ```text
//! "FLPK"  u8 version  u32 count
//! count × ( u32 path_len, path bytes, u64 offset, u64 len )
//! blob …
//! ```
//!
//! Paths are forward-slash, relative to the bundle root, with no `.` or `..`.
//! Offsets are into the blob that follows the index, so an entry is a slice —
//! the whole bundle stays as one allocation the page handed over and nothing
//! is copied until a read asks for it. Little-endian throughout.
//!
//! There is a version byte, and it is checked: a compact format is not
//! self-describing, and a bundle written by a newer engine must be refused by
//! name rather than misread.

use std::collections::BTreeMap;

const MAGIC: &[u8; 4] = b"FLPK";
const VERSION: u8 = 1;

/// Pack `(path, bytes)` pairs into a bundle. Paths are normalised
/// ([`normalize`]) and sorted, so the same files pack to the same bytes.
pub fn pack<'a>(entries: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
    let sorted: BTreeMap<String, &[u8]> =
        entries.into_iter().map(|(p, b)| (normalize(p), b)).filter(|(p, _)| !p.is_empty()).collect();
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
    let mut offset = 0u64;
    for (path, bytes) in &sorted {
        out.extend_from_slice(&(path.len() as u32).to_le_bytes());
        out.extend_from_slice(path.as_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        offset += bytes.len() as u64;
    }
    for bytes in sorted.values() {
        out.extend_from_slice(bytes);
    }
    out
}

/// A parsed bundle: an index over one buffer.
#[derive(Debug)]
pub struct Bundle {
    data: Vec<u8>,
    /// path → (start, end) into `data`.
    index: BTreeMap<String, (usize, usize)>,
}

impl Bundle {
    /// An empty bundle — what is mounted before any fetch completes.
    pub fn empty() -> Self {
        Self { data: Vec::new(), index: BTreeMap::new() }
    }

    pub fn parse(data: Vec<u8>) -> Result<Self, String> {
        let mut at = 0usize;
        let take = |at: &mut usize, n: usize| -> Result<&[u8], String> {
            let s = data.get(*at..*at + n).ok_or_else(|| format!("bundle truncated at byte {at}"))?;
            *at += n;
            Ok(s)
        };
        if take(&mut at, 4)? != MAGIC {
            return Err("not a Floptle bundle (bad magic)".into());
        }
        let version = take(&mut at, 1)?[0];
        if version != VERSION {
            return Err(format!(
                "bundle version {version} — this engine reads version {VERSION}; the export and the \
                 player come from different engine releases"
            ));
        }
        let count = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap()) as usize;
        let mut raw = Vec::with_capacity(count);
        for _ in 0..count {
            let len = u32::from_le_bytes(take(&mut at, 4)?.try_into().unwrap()) as usize;
            let path = std::str::from_utf8(take(&mut at, len)?)
                .map_err(|e| format!("bundle path is not UTF-8: {e}"))?
                .to_string();
            let offset = u64::from_le_bytes(take(&mut at, 8)?.try_into().unwrap()) as usize;
            let size = u64::from_le_bytes(take(&mut at, 8)?.try_into().unwrap()) as usize;
            raw.push((path, offset, size));
        }
        let blob = at;
        let mut index = BTreeMap::new();
        for (path, offset, size) in raw {
            let start = blob + offset;
            let end = start + size;
            if end > data.len() {
                return Err(format!("bundle entry {path} runs past the end ({end} > {})", data.len()));
            }
            index.insert(path, (start, end));
        }
        Ok(Self { data, index })
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The bytes of `path` (already normalised), if the bundle holds it.
    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.index.get(path).map(|&(s, e)| &self.data[s..e])
    }

    pub fn contains(&self, path: &str) -> bool {
        self.index.contains_key(path)
    }

    /// Whether any entry lives under `dir` (a normalised path; `""` is the root).
    pub fn is_dir(&self, dir: &str) -> bool {
        if dir.is_empty() {
            return true;
        }
        let prefix = format!("{dir}/");
        self.index.range(prefix.clone()..).next().is_some_and(|(k, _)| k.starts_with(&prefix))
    }

    /// Every path in the bundle, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }
}

/// Fold a path to the bundle's form: forward slashes, no leading `/`, no `.`,
/// `..` resolved against what precedes it (and dropped at the root — a bundle
/// has nothing above it). A Windows drive prefix is dropped the same way.
pub fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p if p.len() == 2 && p.ends_with(':') && parts.is_empty() => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundle_round_trips_its_files() {
        let packed = pack([("scenes/first.ron", &b"(nodes: [])"[..]), ("project.ron", b"()"), ("a/b/c.bin", &[1, 2, 3])]);
        let b = Bundle::parse(packed).unwrap();
        assert_eq!(b.len(), 3);
        assert_eq!(b.get("scenes/first.ron"), Some(&b"(nodes: [])"[..]));
        assert_eq!(b.get("a/b/c.bin"), Some(&[1u8, 2, 3][..]));
        assert_eq!(b.get("missing"), None);
        assert!(b.is_dir("a") && b.is_dir("a/b") && b.is_dir(""));
        assert!(!b.is_dir("a/b/c.bin") && !b.is_dir("scenes/first"));
        assert_eq!(b.paths().collect::<Vec<_>>(), vec!["a/b/c.bin", "project.ron", "scenes/first.ron"]);
    }

    #[test]
    fn an_empty_file_and_an_empty_bundle_are_fine() {
        let b = Bundle::parse(pack([("empty", &[][..])])).unwrap();
        assert_eq!(b.get("empty"), Some(&[][..]));
        let e = Bundle::parse(pack([])).unwrap();
        assert!(e.is_empty());
    }

    #[test]
    fn the_wrong_version_is_refused_by_name() {
        let mut packed = pack([("x", &b"y"[..])]);
        packed[4] = 9;
        let err = Bundle::parse(packed).unwrap_err();
        assert!(err.contains("version 9"), "{err}");
        assert!(Bundle::parse(b"nope".to_vec()).unwrap_err().contains("bad magic"));
        let mut short = pack([("x", &b"y"[..])]);
        short.truncate(short.len() - 1);
        assert!(Bundle::parse(short).unwrap_err().contains("past the end"));
    }

    #[test]
    fn paths_normalise_to_one_form() {
        assert_eq!(normalize("./scenes//first.ron"), "scenes/first.ron");
        assert_eq!(normalize("/assets/../assets/a.png"), "assets/a.png");
        assert_eq!(normalize("../../escape"), "escape");
        assert_eq!(normalize("C:\\game\\assets\\x.ron"), "game/assets/x.ron");
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("/"), "");
    }

    #[test]
    fn packing_is_deterministic_regardless_of_input_order() {
        let a = pack([("b", &b"2"[..]), ("a", b"1")]);
        let b = pack([("a", &b"1"[..]), ("b", b"2")]);
        assert_eq!(a, b);
    }
}
