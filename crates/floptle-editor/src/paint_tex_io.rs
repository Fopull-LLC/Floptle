//! Persistence for texture paint: `<project>/paint/<scene>.tpaint`.
//!
//! ONE container per scene (like `.vpaint`), but the payload is PNG-encoded rather than raw
//! — a paint atlas is up to 2048² RGBA (16 MB) and mostly flat, so PNG shrinks it by an
//! order of magnitude. The header carries, per node/part, the atlas `edge` and a geometry
//! hash; on load the atlas is rebuilt (its layout is DETERMINISTIC, so an unchanged mesh
//! rebuilds an identical atlas) and the saved pixels are dropped onto it. A mismatch — the
//! mesh changed since it was painted — is refused with a warning rather than scrambled.
//!
//! Inherits the same scene-name keying caveat as vertex paint (`paint_io`): files keyed by
//! `scene_name` get overwritten if the name changes underfoot. It rides the same
//! mitigations — `save_scene` refuses during Play, and paint reloads through
//! `adopt_tex_paint` after any scene load.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::paint_io::geom_hash;
use crate::Editor;

const MAGIC: &[u8; 4] = b"FLTP";
/// v2: the paint image became a transparent OVERLAY (alpha = coverage). v1 files were a
/// baked-canvas (fully opaque, base texture resampled in) — loading one as an overlay would
/// blanket the node with the resampled base, the exact bug the overlay fixed. Refused.
const VERSION: u16 = 2;

/// One node's stored texture paint.
pub(crate) struct StoredTexPaint {
    pub(crate) id: u32,
    pub(crate) parts: Vec<StoredTexPart>,
}

/// One part: the atlas edge it was laid out for, the geometry hash it was painted against,
/// and the PNG-encoded image.
pub(crate) struct StoredTexPart {
    pub(crate) edge: u32,
    pub(crate) hash: u64,
    pub(crate) png: Vec<u8>,
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn rd_u32(b: &[u8], p: &mut usize) -> Option<u32> {
    let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
    *p += 4;
    Some(v)
}
fn rd_u64(b: &[u8], p: &mut usize) -> Option<u64> {
    let v = u64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
    *p += 8;
    Some(v)
}

pub(crate) fn encode(nodes: &[StoredTexPaint]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_u32(&mut out, nodes.len() as u32);
    for n in nodes {
        put_u32(&mut out, n.id);
        put_u32(&mut out, n.parts.len() as u32);
        for part in &n.parts {
            put_u32(&mut out, part.edge);
            out.extend_from_slice(&part.hash.to_le_bytes());
            put_u32(&mut out, part.png.len() as u32);
        }
    }
    // Bulk PNG blobs last, in index order.
    for n in nodes {
        for part in &n.parts {
            out.extend_from_slice(&part.png);
        }
    }
    out
}

pub(crate) fn decode(bytes: &[u8]) -> Option<Vec<StoredTexPaint>> {
    if bytes.len() < 10 || &bytes[0..4] != MAGIC {
        return None;
    }
    let ver = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
    if ver != VERSION {
        return None; // a future format — refuse rather than misread
    }
    let mut p = 6;
    let n = rd_u32(bytes, &mut p)?;
    // (id, [(edge, hash, png_len)]) — the header, sliced before the bulk PNG blobs.
    type NodeShape = (u32, Vec<(u32, u64, u32)>);
    let mut shape: Vec<NodeShape> = Vec::new();
    for _ in 0..n {
        let id = rd_u32(bytes, &mut p)?;
        let parts = rd_u32(bytes, &mut p)?;
        let mut ps = Vec::new();
        for _ in 0..parts {
            let edge = rd_u32(bytes, &mut p)?;
            let hash = rd_u64(bytes, &mut p)?;
            let len = rd_u32(bytes, &mut p)?;
            ps.push((edge, hash, len));
        }
        shape.push((id, ps));
    }
    let mut out = Vec::new();
    for (id, ps) in shape {
        let mut parts = Vec::new();
        for (edge, hash, len) in ps {
            let png = bytes.get(p..p + len as usize)?.to_vec();
            p += len as usize;
            parts.push(StoredTexPart { edge, hash, png });
        }
        out.push(StoredTexPaint { id, parts });
    }
    Some(out)
}

impl Editor {
    pub(crate) fn tex_paint_file_path(&self) -> PathBuf {
        self.project_root.join("paint").join(format!("{}.tpaint", self.scene_name))
    }

    /// Write every texture-painted node's images beside the scene. Called from `save_scene`.
    pub(crate) fn save_tex_paint(&mut self) {
        if self.paint_tex.is_empty() {
            // Nothing painted: drop a stale file so a cleared scene doesn't resurrect paint.
            let _ = std::fs::remove_file(self.tex_paint_file_path());
            return;
        }
        // Which mesh key each paint id belongs to, so we can hash the right geometry.
        let keys: HashMap<u32, String> = self
            .world
            .query::<floptle_core::TexturePaint>()
            .filter_map(|(e, tp)| self.paint_key(e).map(|(k, _)| (tp.id, k)))
            .collect();

        let ids: Vec<u32> = self.paint_tex.keys().copied().collect();
        let mut nodes = Vec::new();
        for id in ids {
            let key = keys.get(&id).cloned();
            let Some(pt) = self.paint_tex.get(&id) else { continue };
            let mut parts = Vec::new();
            for (i, pp) in pt.parts.iter().enumerate() {
                let Some(png) = floptle_assets::encode_png(&floptle_render::TextureData {
                    pixels: pp.pixels.clone(),
                    width: pp.edge,
                    height: pp.edge,
                }) else {
                    continue;
                };
                let hash = key
                    .as_deref()
                    .and_then(|k| self.paint_meshes.get(k))
                    .and_then(|ps| ps.get(i))
                    .map_or(0, |mp| geom_hash(&mp.verts));
                parts.push(StoredTexPart { edge: pp.edge, hash, png });
            }
            if !parts.is_empty() {
                nodes.push(StoredTexPaint { id, parts });
            }
        }
        let dir = self.project_root.join("paint");
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(self.tex_paint_file_path(), encode(&nodes)) {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("💾 save texture paint failed: {e}"),
                None,
            );
        }
    }

    /// Reload texture paint for the current scene, rebuilding each painted node's atlas and
    /// dropping the saved pixels onto it. Called after any scene load, next to `adopt_paint`.
    /// MUST run with gpu/raster live — atlases and textures are GPU allocations.
    pub(crate) fn adopt_tex_paint(&mut self) {
        if self.gpu.is_none() || self.raster.is_none() {
            // A silent no-op here LOSES saved paint (the boot-order bug of 2026-07-16:
            // adopt ran before `self.gpu = Some(..)`). Shout so it can't hide again.
            log::error!("adopt_tex_paint called before gpu/raster exist — saved texture paint NOT loaded");
            return;
        }
        self.paint_tex.clear();
        let Ok(bytes) = std::fs::read(self.tex_paint_file_path()) else { return };
        let Some(stored) = decode(&bytes) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                "🖌 texture paint file is unreadable (wrong magic/version) — ignoring".into(),
                None,
            );
            return;
        };
        let by_id: HashMap<u32, StoredTexPaint> = stored.into_iter().map(|s| (s.id, s)).collect();

        // Painted nodes in the loaded scene, with their mesh geometry cached so the atlas can
        // be rebuilt and the geometry hash checked.
        let painted: Vec<(floptle_core::Entity, u32)> = self
            .world
            .query::<floptle_core::TexturePaint>()
            .map(|(e, tp)| (e, tp.id))
            .collect();
        for (e, id) in painted {
            let Some(sp) = by_id.get(&id) else { continue }; // no saved image for this id
            let Some(key) = self.ensure_paint_mesh_pub(e) else { continue };
            // Rebuild the (deterministic) atlas — seeds from the base texture.
            if self.ensure_paint_tex(e, &key).is_none() {
                continue;
            }
            let mut ok = true;
            for (i, part) in sp.parts.iter().enumerate() {
                let live_hash = self
                    .paint_meshes
                    .get(&key)
                    .and_then(|ps| ps.get(i))
                    .map_or(0, |mp| geom_hash(&mp.verts));
                if part.hash != 0 && live_hash != 0 && part.hash != live_hash {
                    ok = false;
                    break;
                }
                let Some(img) = floptle_assets::decode_png(&part.png) else {
                    ok = false;
                    break;
                };
                if img.width != part.edge
                    || !self.overwrite_tex_paint_part(id, i, part.edge, img.pixels)
                {
                    ok = false;
                    break;
                }
            }
            if !ok {
                // The mesh changed since it was painted (or the layout no longer matches):
                // drop the paint rather than scramble it, and say so.
                self.clear_texture_paint(e);
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "🖌 '{key}' changed since it was texture-painted — that node's paint \
                         was NOT restored (re-paint it, or restore the old model)"
                    ),
                    None,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_round_trips() {
        let nodes = vec![
            StoredTexPaint {
                id: 5,
                parts: vec![
                    StoredTexPart { edge: 256, hash: 0xdead_beef, png: vec![1, 2, 3, 4, 5] },
                    StoredTexPart { edge: 512, hash: 7, png: vec![9, 9] },
                ],
            },
            StoredTexPaint {
                id: 11,
                parts: vec![StoredTexPart { edge: 128, hash: 0, png: vec![42] }],
            },
        ];
        let out = decode(&encode(&nodes)).expect("round trip");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, 5);
        assert_eq!(out[0].parts[0].edge, 256);
        assert_eq!(out[0].parts[0].hash, 0xdead_beef);
        assert_eq!(out[0].parts[0].png, vec![1, 2, 3, 4, 5]);
        assert_eq!(out[0].parts[1].png, vec![9, 9]);
        assert_eq!(out[1].id, 11);
        assert_eq!(out[1].parts[0].png, vec![42]);
    }

    #[test]
    fn garbage_is_refused_not_misread() {
        assert!(decode(b"").is_none());
        assert!(decode(b"NOPE\x01\x00\x00\x00\x00\x00").is_none());
        let mut v = encode(&[]);
        v[4] = 99; // right magic, wrong version
        assert!(decode(&v).is_none());
    }

    /// A real PNG round-trip through the container — the bytes the loader will decode.
    #[test]
    fn png_payload_round_trips() {
        let tex = floptle_render::TextureData {
            pixels: (0..256 * 4).map(|i| (i % 256) as u8).collect(),
            width: 16,
            height: 16,
        };
        let png = floptle_assets::encode_png(&tex).expect("encode");
        let nodes = vec![StoredTexPaint {
            id: 1,
            parts: vec![StoredTexPart { edge: 16, hash: 1, png }],
        }];
        let out = decode(&encode(&nodes)).expect("round trip");
        let back = floptle_assets::decode_png(&out[0].parts[0].png).expect("decode");
        assert_eq!(back.width, 16);
        assert_eq!(back.pixels, tex.pixels);
    }
}
