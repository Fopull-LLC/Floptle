//! The desktop: `std::fs`, one call deep.

use std::io;
use std::path::Path;

use crate::DirEntry;

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    std::fs::write(path, contents)
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

pub(crate) fn modified_impl(path: &Path) -> Option<floptle_core::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// List a directory. Order is the platform's, as with `std::fs::read_dir`;
/// sort if it matters.
pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<Vec<DirEntry>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
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

/// Remove an EMPTY directory, as `std::fs::remove_dir` does.
pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    std::fs::remove_dir(path)
}
