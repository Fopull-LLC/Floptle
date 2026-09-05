//! The browser: the mounted bundle, with saves persisted in `localStorage`.
//!
//! Everything a page reads was in the bundle the export packed; everything it
//! writes goes to an overlay in memory and, byte for byte, to `localStorage`
//! under `floptle:<game>:<path>` so it is there after a reload. `localStorage`
//! holds strings, so a file is stored as the Latin-1 string of its bytes — one
//! code unit per byte, exact both ways, and no encoder to depend on.
//!
//! This is a downgrade from a folder on disk, and it is named as one: storage
//! is per origin (a game on a shared host shares the origin with every other
//! game there, which is why the key carries the game's name), a browser may
//! evict it under pressure, and it is typically capped at a few megabytes —
//! plenty for save slots, not a place for anything large.

use std::io;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use crate::DirEntry;
use crate::bundle::Bundle;
use crate::mem::MemFs;

struct Web {
    fs: MemFs,
    /// `floptle:<game>:` — the storage key prefix.
    prefix: String,
}

static WEB: Mutex<Option<Web>> = Mutex::new(None);

fn lock() -> MutexGuard<'static, Option<Web>> {
    // Single-threaded; a poisoned lock would mean a panic mid-write, and the
    // data is still the best answer there is.
    WEB.lock().unwrap_or_else(|e| e.into_inner())
}

fn unmounted() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no game bundle is mounted yet")
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn to_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

fn from_latin1(s: &str) -> Vec<u8> {
    s.chars().map(|c| c as u32 as u8).collect()
}

/// Make `data` the filesystem. Nothing persists until [`open_saves`] names
/// the game — the manifest that names it is inside this very bundle.
pub fn mount(data: Vec<u8>) -> Result<usize, String> {
    let bundle = Bundle::parse(data)?;
    let n = bundle.len();
    *lock() = Some(Web { fs: MemFs::new(bundle), prefix: String::new() });
    Ok(n)
}

/// Name the storage namespace saves persist under — the exported title — so
/// two games on one origin do not read each other's slots. Restores every
/// save already in storage for that name, and turns persistence on.
pub fn open_saves(game: &str) -> Result<(), String> {
    let mut g = lock();
    let w = g.as_mut().ok_or("no game bundle is mounted yet")?;
    w.prefix = format!("floptle:{}:", game.trim());
    if let Some(st) = storage()
        && let Ok(len) = st.length()
    {
        for i in 0..len {
            let Ok(Some(k)) = st.key(i) else { continue };
            let Some(path) = k.strip_prefix(&w.prefix) else { continue };
            if let Ok(Some(v)) = st.get_item(&k) {
                w.fs.restore(path.to_string(), from_latin1(&v));
            }
        }
    }
    Ok(())
}

/// A file's size in bytes, without copying it out of the bundle.
pub fn size<P: AsRef<Path>>(path: P) -> Option<u64> {
    lock().as_ref()?.fs.size(path.as_ref())
}

/// Copy a file: a read and a write through the overlay.
pub fn copy<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<u64> {
    let bytes = read(from)?;
    let n = bytes.len() as u64;
    write(to, bytes)?;
    Ok(n)
}

pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
    lock().as_ref().ok_or_else(unmounted)?.fs.read(path.as_ref())
}

pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    let mut g = lock();
    let w = g.as_mut().ok_or_else(unmounted)?;
    let bytes = contents.as_ref();
    let k = w.fs.write(path.as_ref(), bytes);
    if !w.prefix.is_empty()
        && let Some(st) = storage()
        && let Err(e) = st.set_item(&format!("{}{k}", w.prefix), &to_latin1(bytes))
    {
        // The overlay has it for this session; what failed is surviving a
        // reload, and the caller (a save) should hear that.
        return Err(io::Error::other(format!(
            "the browser refused to keep {} ({:?}) — storage may be full or disabled",
            path.as_ref().display(),
            e
        )));
    }
    Ok(())
}

pub fn exists<P: AsRef<Path>>(path: P) -> bool {
    lock().as_ref().is_some_and(|w| w.fs.exists(path.as_ref()))
}

pub fn is_file<P: AsRef<Path>>(path: P) -> bool {
    lock().as_ref().is_some_and(|w| w.fs.is_file(path.as_ref()))
}

pub fn is_dir<P: AsRef<Path>>(path: P) -> bool {
    lock().as_ref().is_some_and(|w| w.fs.is_dir(path.as_ref()))
}

pub(crate) fn modified_impl(_path: &Path) -> Option<floptle_core::time::SystemTime> {
    None
}

pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<Vec<DirEntry>> {
    lock().as_ref().ok_or_else(unmounted)?.fs.read_dir(path.as_ref())
}

/// Directories are implicit in a bundle; there is nothing to create.
pub fn create_dir_all<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    if lock().is_some() { Ok(()) } else { Err(unmounted()) }
}

/// Directories are implicit in a bundle; an empty one is already gone.
pub fn remove_dir<P: AsRef<Path>>(_path: P) -> io::Result<()> {
    if lock().is_some() { Ok(()) } else { Err(unmounted()) }
}

pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    let mut g = lock();
    let w = g.as_mut().ok_or_else(unmounted)?;
    let k = w.fs.remove_file(path.as_ref())?;
    if !w.prefix.is_empty()
        && let Some(st) = storage()
    {
        let _ = st.remove_item(&format!("{}{k}", w.prefix));
    }
    Ok(())
}
