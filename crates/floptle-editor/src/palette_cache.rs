//! Terrain palette pictures, read once.
//!
//! A palette slot's picture is decoded from its file and resized to the
//! palette's layer size: for a 2048² photograph that is most of a second,
//! and a palette is up to 32 of them plus their maps. The result is kept
//! under `.floptle/cache/palette/`, keyed by the file, its size and time, the
//! layer size and the filter, so the next load reads it back in
//! milliseconds. Beside each picture is its mean colour, a few bytes the
//! frame reads to show the ground in its own colours before any picture is
//! in.
//!
//! A file with no modified time (a browser has none) is not cached: there
//! would be no way to tell a changed picture from the one cached.

use std::path::{Path, PathBuf};

/// The cache entry for `file` at layer size `size`, read nearest or smooth,
/// if the file can be told apart from a changed one.
fn entry(root: &Path, file: &Path, size: u32, nearest: bool) -> Option<PathBuf> {
    let modified = floptle_vfs::modified(file)?;
    let len = floptle_vfs::size(file)?;
    // FNV-1a: stable across builds, where the standard hasher is not.
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    let text = format!("{}|{len}|{modified:?}|{size}|{nearest}", file.display());
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    Some(root.join(".floptle/cache/palette").join(format!("{h:016x}")))
}

const MAGIC: &[u8; 4] = b"FPL1";

/// Every file decoded rather than read from the cache, for the guard that a
/// second load decodes nothing.
#[cfg(test)]
pub(crate) static DECODED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// `file`'s picture at `size`² (nearest or smooth) and its mean linear
/// colour: from the cache, or decoded and resized and then cached.
pub(crate) fn load(root: &Path, file: &Path, size: u32, nearest: bool) -> Option<(floptle_render::TextureData, [f32; 3])> {
    let at = entry(root, file, size, nearest);
    if let Some(at) = &at
        && let Ok(bytes) = floptle_vfs::read(at.with_extension("pal"))
        && bytes.len() == 4 + (size * size * 4) as usize
        && &bytes[..4] == MAGIC
    {
        let tex = floptle_render::TextureData { pixels: bytes[4..].to_vec(), width: size, height: size };
        let mean = read_mean(at).unwrap_or_else(|| crate::terrain_edit::layer_mean(&tex));
        return Some((tex, mean));
    }
    #[cfg(test)]
    if let Ok(mut d) = DECODED.lock() {
        d.push(file.to_path_buf());
    }
    let tex = floptle_assets::load_texture_sized_filtered(file, size, size, nearest)?;
    let mean = crate::terrain_edit::layer_mean(&tex);
    if let Some(at) = at {
        let mut bytes = Vec::with_capacity(4 + tex.pixels.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&tex.pixels);
        let mut m = Vec::with_capacity(12);
        for c in mean {
            m.extend_from_slice(&c.to_le_bytes());
        }
        // A cache that cannot be written is only a slower next load, and
        // one cut short is the wrong length and read as missing.
        if let Some(dir) = at.parent() {
            let _ = floptle_vfs::create_dir_all(dir);
        }
        let _ = floptle_vfs::write(at.with_extension("pal"), bytes);
        let _ = floptle_vfs::write(at.with_extension("mean"), m);
    }
    Some((tex, mean))
}

/// The mean colour cached for `file`, without its picture: what the frame
/// fills a slot with until the picture lands.
pub(crate) fn mean(root: &Path, file: &Path, size: u32, nearest: bool) -> Option<[f32; 3]> {
    read_mean(&entry(root, file, size, nearest)?)
}

fn read_mean(at: &Path) -> Option<[f32; 3]> {
    let b = floptle_vfs::read(at.with_extension("mean")).ok()?;
    if b.len() != 12 {
        return None;
    }
    let f = |i: usize| f32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Some([f(0), f(4), f(8)])
}
