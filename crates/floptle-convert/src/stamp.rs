//! **Which `.glb` files this converter wrote, and from what.**
//!
//! Conversion is automatic, so it has to know which models it may write over.
//! A `.glb` somebody exported by hand, or fixed up after a conversion and saved
//! back, must never be replaced because an `.fbx` with the same name sits next
//! to it. So every converted file carries a stamp in its own `asset.extras` —
//! the source's format, a hash of the source's bytes, and this converter's
//! version — and only a file with a stamp is ever written over.
//!
//! In the file rather than in a sidecar beside it: a stamp that travels with
//! the model survives a copy, a zip and a commit, and there is no second file
//! to lose or to leave behind.

use std::io::Read;
use std::path::Path;

/// Bumped when a conversion would come out differently, so models converted
/// by an older converter are converted again with what it has learned.
pub const CONVERTER: u32 = 3;

/// The key under `asset.extras`.
const KEY: &str = "floptleConverted";

/// What a converted model says about where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The source's extension, lower case: `fbx`, `obj`, `blend`.
    pub from: String,
    pub converter: u32,
    /// [`hash_source`] of the source when it was converted.
    pub source_hash: String,
}

/// `glb` with `stamp` written into its JSON. A file that is not a `.glb` is
/// returned as it is.
pub fn stamp_glb(glb: &[u8], stamp: &Stamp) -> Vec<u8> {
    let Some((json, bin)) = split(glb) else { return glb.to_vec() };
    let Ok(mut root) = serde_json::from_slice::<serde_json::Value>(json) else { return glb.to_vec() };
    if !root["asset"].is_object() {
        root["asset"] = serde_json::json!({ "version": "2.0" });
    }
    if !root["asset"]["extras"].is_object() {
        root["asset"]["extras"] = serde_json::json!({});
    }
    root["asset"]["extras"][KEY] = serde_json::json!({
        "from": stamp.from,
        "converter": stamp.converter,
        "sourceHash": stamp.source_hash,
    });
    join(&root, bin)
}

/// The generator the converter signed `.glb` files with before they carried
/// a stamp. Its files beside their source are ours from before there was a
/// way to say so, and are converted again (see [`crate::Output::Unstamped`]).
pub const OLD_GENERATOR: &str = "Floptle glb_write";

/// The stamp in a `.glb` on disk, reading only its header and JSON chunk.
/// `None` for a file this converter did not write.
pub fn read_stamp(path: &Path) -> Option<Stamp> {
    let root = read_json(path)?;
    let s = &root["asset"]["extras"][KEY];
    Some(Stamp {
        from: s["from"].as_str()?.to_string(),
        converter: s["converter"].as_u64()? as u32,
        source_hash: s["sourceHash"].as_str()?.to_string(),
    })
}

/// The `asset.generator` a `.glb` on disk names.
pub fn read_generator(path: &Path) -> Option<String> {
    read_json(path)?["asset"]["generator"].as_str().map(str::to_string)
}

/// A `.glb`'s JSON chunk, read without its binary one.
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 20];
    f.read_exact(&mut head).ok()?;
    if &head[0..4] != b"glTF" || u32::from_le_bytes(head[16..20].try_into().ok()?) != 0x4E4F534A {
        return None;
    }
    let len = u32::from_le_bytes(head[12..16].try_into().ok()?) as usize;
    // A JSON chunk this large is not one of ours; refusing it keeps a stray
    // multi-gigabyte file from being read to answer a yes-or-no question.
    if len > 64 << 20 {
        return None;
    }
    let mut json = vec![0u8; len];
    f.read_exact(&mut json).ok()?;
    serde_json::from_slice(&json).ok()
}

/// A hash of what a conversion reads: the source file, and for an OBJ its
/// `.mtl` beside it, which holds the materials. FNV-1a, 64-bit, written out
/// here so the value is the same on every machine and every Rust release —
/// it is compared against a value stored in a file.
pub fn hash_source(src: &Path) -> std::io::Result<String> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut feed = |path: &Path| -> std::io::Result<()> {
        let mut f = std::fs::File::open(path)?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                return Ok(());
            }
            for &b in &buf[..n] {
                h ^= b as u64;
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    };
    feed(src)?;
    if src.extension().is_some_and(|e| e.eq_ignore_ascii_case("obj")) {
        let mtl = src.with_extension("mtl");
        if mtl.is_file() {
            feed(&mtl)?;
        }
    }
    Ok(format!("{h:016x}"))
}

/// The JSON and BIN chunks of a `.glb`.
pub(crate) fn split(glb: &[u8]) -> Option<(&[u8], &[u8])> {
    if glb.len() < 20 || &glb[0..4] != b"glTF" {
        return None;
    }
    let mut off = 12;
    let (mut json, mut bin): (Option<&[u8]>, &[u8]) = (None, &[]);
    while off + 8 <= glb.len() {
        let len = u32::from_le_bytes(glb[off..off + 4].try_into().ok()?) as usize;
        let kind = u32::from_le_bytes(glb[off + 4..off + 8].try_into().ok()?);
        let body = glb.get(off + 8..off + 8 + len)?;
        match kind {
            0x4E4F534A => json = Some(body),
            0x004E4942 => bin = body,
            _ => {}
        }
        off += 8 + len;
    }
    Some((json?, bin))
}

/// A `.glb` from a JSON document and a BIN chunk.
pub(crate) fn join(root: &serde_json::Value, bin: &[u8]) -> Vec<u8> {
    let mut json = serde_json::to_vec(root).unwrap_or_default();
    while !json.len().is_multiple_of(4) {
        json.push(b' ');
    }
    let mut bin = bin.to_vec();
    while !bin.len().is_multiple_of(4) {
        bin.push(0);
    }
    let total = 12 + 8 + json.len() + if bin.is_empty() { 0 } else { 8 + bin.len() };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&(total as u32).to_le_bytes());
    out.extend_from_slice(&(json.len() as u32).to_le_bytes());
    out.extend_from_slice(&0x4E4F534Au32.to_le_bytes());
    out.extend_from_slice(&json);
    if !bin.is_empty() {
        out.extend_from_slice(&(bin.len() as u32).to_le_bytes());
        out.extend_from_slice(&0x004E4942u32.to_le_bytes());
        out.extend_from_slice(&bin);
    }
    out
}
