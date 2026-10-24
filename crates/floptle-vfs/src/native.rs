//! The desktop: `std::fs`, one call deep.

use std::io;
use std::path::{Path, PathBuf};

use crate::DirEntry;

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    std::fs::write(path, contents)
}

/// Replace a file so that it is either wholly the old one or wholly the new
/// one, whenever the process stops. The bytes go to a hidden sibling first,
/// reach the disk, and only then take the name, so a kill or a full disk
/// mid-write leaves the previous file exactly as it was. `write` truncates
/// first, and a save cut off there is a save with nothing in it.
///
/// The sibling is `.<name>.tmp` beside the file: the same folder keeps the
/// rename on one filesystem, and a left-over one from a killed process is
/// simply written over by the next save.
pub fn write_atomic<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    use std::io::Write as _;
    let path = path.as_ref();
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "a file needs a name"))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(contents.as_ref())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub fn exists<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().exists()
}

pub fn is_file<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().is_file()
}

pub fn is_dir<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().is_dir()
}

/// Is the path itself a symbolic link? Asked of the link, not of what it
/// points at — the one question `is_dir`/`is_file` cannot answer.
pub fn is_symlink<P: AsRef<Path>>(path: P) -> bool {
    std::fs::symlink_metadata(path).map(|m| m.file_type().is_symlink()).unwrap_or(false)
}

pub(crate) fn modified_impl(path: &Path) -> Option<floptle_core::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// List a directory. Order is the platform's, as with `std::fs::read_dir`;
/// sort if it matters.
///
/// A link to a folder lists as a folder, so a project whose `ui/` or
/// `animation_controllers/` is a link keeps its styles and controllers. A link
/// back to the folder being listed, or to one of its parents, lists as a file:
/// every walk in the engine would otherwise descend it forever.
pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<Vec<DirEntry>> {
    let path = path.as_ref();
    let mut here: Option<Option<PathBuf>> = None;
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type().ok();
        let is_dir = match kind {
            Some(t) if t.is_symlink() => match std::fs::canonicalize(entry.path()) {
                Ok(target) if target.is_dir() => {
                    let here = here.get_or_insert_with(|| std::fs::canonicalize(path).ok());
                    here.as_ref().is_none_or(|h| !h.starts_with(&target))
                }
                _ => false,
            },
            Some(t) => t.is_dir(),
            None => false,
        };
        out.push(DirEntry::new(entry.path(), is_dir));
    }
    Ok(out)
}

pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::create_dir_all(path)
}

pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_file(path)
}

/// A file's size in bytes, without reading it. `None` if it is not a file.
pub fn size<P: AsRef<Path>>(path: P) -> Option<u64> {
    let m = std::fs::metadata(path).ok()?;
    m.is_file().then_some(m.len())
}

/// Copy a file, as `std::fs::copy` does. Returns the bytes copied.
pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    std::fs::copy(from, to)
}

/// Remove an empty directory, as `std::fs::remove_dir` does.
pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_dir(path)
}

/// The working directory a relative path is relative to.
pub(crate) fn cwd() -> Option<PathBuf> {
    std::env::current_dir().ok()
}
