//! Persistence for vertex paint: `<project>/paint/<scene>.vpaint`.
//!
//! ONE container per scene, not one file per node — an all-painted scene would
//! otherwise mean hundreds of tiny files and hundreds of syscalls per load. The
//! format is deliberately "the GPU buffer, serialized": an index of
//! `paint_id → (offset, count, geom_hash)` followed by the bulk RGBA8, so loading is
//! one read and a run of block allocations.
//!
//! Binary, not RON, because per-vertex arrays in a `.ron` would be unreadable and
//! enormous — the same call terrain fields make.
//!
//! NOTE the scene-name keying inherits the bug class fixed on 2026-07-14 for terrain:
//! files keyed by `scene_name` get overwritten if the name changes underfoot. Paint
//! rides the existing mitigations — `save_scene` refuses during Play, and paint reloads
//! through `adopt_paint` after any scene load/undo-restore.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::vertex_paint::PaintBlocks;
use crate::Editor;

const MAGIC: &[u8; 4] = b"FLVP";
const VERSION: u16 = 1;

/// One node's stored paint.
#[derive(Clone)]
pub(crate) struct StoredPaint {
    pub(crate) id: u32,
    /// Per part: the colors, and the geometry hash they were painted against.
    pub(crate) parts: Vec<(Vec<[u8; 4]>, u64)>,
}

/// A cheap order-sensitive hash of a part's vertex positions — the re-import guard.
///
/// Import splits per-material into parts with independently re-indexed vertex arrays,
/// and part order falls out of material iteration order. So a re-export from Blender can
/// silently permute parts or change counts. Paint keyed by vertex INDEX would then land
/// on the wrong vertices — visibly scrambled, with no error. This hash lets the loader
/// notice and refuse instead.
pub(crate) fn geom_hash(verts: &[floptle_render::Vertex]) -> u64 {
    // FNV-1a over quantized positions. Quantized because a re-export can perturb the
    // last float bit without meaningfully moving a vertex, and we don't want to cry
    // wolf over that.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for v in verts {
        for c in v.pos {
            let q = (c * 1024.0).round() as i64;
            for b in q.to_le_bytes() {
                h ^= b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    h ^ (verts.len() as u64)
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

pub(crate) fn encode(blocks: &[StoredPaint]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    put_u32(&mut out, blocks.len() as u32);
    for b in blocks {
        put_u32(&mut out, b.id);
        put_u32(&mut out, b.parts.len() as u32);
        for (colors, hash) in &b.parts {
            put_u32(&mut out, colors.len() as u32);
            out.extend_from_slice(&hash.to_le_bytes());
        }
    }
    // Bulk last, in index order — so a loader can stream it straight into the store.
    for b in blocks {
        for (colors, _) in &b.parts {
            for c in colors {
                out.extend_from_slice(c);
            }
        }
    }
    out
}

pub(crate) fn decode(bytes: &[u8]) -> Option<Vec<StoredPaint>> {
    if bytes.len() < 10 || &bytes[0..4] != MAGIC {
        return None;
    }
    let ver = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
    if ver != VERSION {
        return None; // a future format — refuse rather than misread
    }
    let mut p = 6;
    let n = rd_u32(bytes, &mut p)?;
    // Header first: counts + hashes, so the bulk can be sliced in one pass.
    let mut shape: Vec<(u32, Vec<(u32, u64)>)> = Vec::new();
    for _ in 0..n {
        let id = rd_u32(bytes, &mut p)?;
        let parts = rd_u32(bytes, &mut p)?;
        let mut ps = Vec::new();
        for _ in 0..parts {
            let count = rd_u32(bytes, &mut p)?;
            let hash = rd_u64(bytes, &mut p)?;
            ps.push((count, hash));
        }
        shape.push((id, ps));
    }
    let mut out = Vec::new();
    for (id, ps) in shape {
        let mut parts = Vec::new();
        for (count, hash) in ps {
            let mut colors = Vec::with_capacity(count as usize);
            for _ in 0..count {
                let c = bytes.get(p..p + 4)?;
                colors.push([c[0], c[1], c[2], c[3]]);
                p += 4;
            }
            parts.push((colors, hash));
        }
        out.push(StoredPaint { id, parts });
    }
    Some(out)
}

impl Editor {
    pub(crate) fn paint_file_path(&self) -> PathBuf {
        self.project_root.join("paint").join(format!("{}.vpaint", self.scene_name))
    }

    /// Write every painted node's colors beside the scene. Called from `save_scene`.
    ///
    /// Entries the last adopt could NOT apply (`paint_orphans` — mesh unloadable or
    /// the re-import guard refused) are carried through UNCHANGED, as long as a node
    /// still references their id and the user hasn't repainted it. Before this, one
    /// save from a session with broken asset resolution silently destroyed every
    /// unloaded node's paint (Ty's `assets` project lost ~90% of both paint files,
    /// 2026-07-20).
    pub(crate) fn save_paint(&mut self) {
        let referenced: std::collections::HashSet<u32> =
            self.world.query::<floptle_core::VertexPaint>().map(|(_, vp)| vp.id).collect();
        let keep: Vec<StoredPaint> = self
            .paint_orphans
            .iter()
            .filter(|sp| referenced.contains(&sp.id) && !self.paint_data.contains_key(&sp.id))
            .cloned()
            .collect();
        let ids: Vec<(u32, PaintBlocks)> =
            self.paint_data.iter().map(|(&k, v)| (k, v.clone())).collect();
        if ids.is_empty() && keep.is_empty() {
            // Nothing painted: drop a stale file so a cleared scene doesn't resurrect
            // paint on next load.
            let _ = std::fs::remove_file(self.paint_file_path());
            return;
        }
        // Which mesh key each paint id belongs to, so we can hash the right geometry.
        let keys: HashMap<u32, String> = self
            .world
            .query::<floptle_core::VertexPaint>()
            .filter_map(|(e, vp)| self.paint_key(e).map(|(k, _)| (vp.id, k)))
            .collect();

        let mut stored = Vec::new();
        for (id, blocks) in ids {
            let Some(raster) = self.raster.as_ref() else { return };
            let key = keys.get(&id);
            let mut parts = Vec::new();
            for (i, &(base, count)) in blocks.parts.iter().enumerate() {
                let colors = raster.paint_block(base, count);
                let hash = key
                    .and_then(|k| self.paint_meshes.get(k))
                    .and_then(|ps| ps.get(i))
                    .map_or(0, |pp| geom_hash(&pp.verts));
                parts.push((colors, hash));
            }
            stored.push(StoredPaint { id, parts });
        }
        stored.extend(keep);
        let dir = self.project_root.join("paint");
        let _ = std::fs::create_dir_all(&dir);
        if let Err(e) = std::fs::write(self.paint_file_path(), encode(&stored)) {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("💾 save vertex paint failed: {e}"),
                None,
            );
        }
    }

    /// Reload paint for the current scene into the `vpaint` store, re-pointing every
    /// painted node at its block. Called after any scene load / undo-restore, the same
    /// way `adopt_terrain` is — the World is rebuilt from scratch, so the bases are gone.
    pub(crate) fn adopt_paint(&mut self) {
        if self.gpu.is_none() || self.raster.is_none() {
            // Same boot-order trap as adopt_tex_paint: block allocation needs the GPU, and
            // a silent no-op here loses every saved vertex color on startup.
            log::error!("adopt_paint called before gpu/raster exist — saved vertex paint NOT loaded");
            return;
        }
        self.paint_data.clear();
        self.paint_orphans.clear();
        self.vpaint_epoch += 1; // blocks realloc — texture-paint mirrors must resync
        let Ok(bytes) = std::fs::read(self.paint_file_path()) else { return };
        let Some(stored) = decode(&bytes) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                "🖌 vertex paint file is unreadable (wrong magic/version) — ignoring".into(),
                None,
            );
            return;
        };
        // Make sure each painted node's geometry is cached, so counts/hashes can be
        // checked and the brush is ready to go.
        let painted: Vec<(floptle_core::Entity, u32)> = self
            .world
            .query::<floptle_core::VertexPaint>()
            .map(|(e, vp)| (e, vp.id))
            .collect();
        let mut key_of: HashMap<u32, String> = HashMap::new();
        for (e, id) in &painted {
            if let Some(k) = self.ensure_paint_mesh_pub(*e) {
                key_of.insert(*id, k);
            }
        }

        let referenced: std::collections::HashSet<u32> =
            painted.iter().map(|&(_, id)| id).collect();
        for sp in stored {
            let Some(key) = key_of.get(&sp.id).cloned() else {
                if referenced.contains(&sp.id) {
                    // A node still wants this paint but its mesh couldn't be loaded
                    // (missing file, broken ref) — PRESERVE the stored entry so the
                    // next save carries it forward instead of destroying it.
                    self.paint_orphans.push(sp);
                }
                continue; // no node references this id any more — drop it
            };
            let mut blocks = PaintBlocks::default();
            let mut ok = true;
            for (i, (colors, hash)) in sp.parts.iter().enumerate() {
                let live = self.paint_meshes.vertex_count(&key, i);
                let live_hash = self
                    .paint_meshes
                    .get(&key)
                    .and_then(|ps| ps.get(i))
                    .map_or(0, |pp| geom_hash(&pp.verts));
                // The re-import guard. Applying a stale block would put paint on the
                // WRONG vertices — visibly scrambled, silently. Refuse and say so.
                if live != colors.len() as u32 || (*hash != 0 && live_hash != 0 && *hash != live_hash) {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!(
                            "🖌 '{key}' part {i} changed since it was painted \
                             ({} verts stored, {live} now) — that node's paint was NOT applied \
                             (re-paint it, or restore the old model)",
                            colors.len()
                        ),
                        None,
                    );
                    ok = false;
                    break;
                }
                let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
                    return;
                };
                let base = raster.paint_alloc_from(gpu, colors);
                if base == 0 {
                    ok = false;
                    break;
                }
                blocks.parts.push((base, live));
            }
            if ok {
                self.paint_data.insert(sp.id, blocks);
            } else {
                // The guard refused (mesh changed / alloc failed): keep the stored
                // entry on file — "restore the old model" (the advice in the warning
                // above) only works if a save can't wipe the paint meanwhile.
                self.paint_orphans.push(sp);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_round_trips() {
        let blocks = vec![
            StoredPaint {
                id: 3,
                parts: vec![
                    (vec![[1, 2, 3, 4], [5, 6, 7, 8]], 0xdead_beef),
                    (vec![[9; 4]], 42),
                ],
            },
            StoredPaint { id: 7, parts: vec![(vec![[10, 20, 30, 40]], 1)] },
        ];
        let out = decode(&encode(&blocks)).expect("round trip");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, 3);
        assert_eq!(out[0].parts[0].0, vec![[1, 2, 3, 4], [5, 6, 7, 8]]);
        assert_eq!(out[0].parts[0].1, 0xdead_beef);
        assert_eq!(out[0].parts[1].0, vec![[9; 4]]);
        assert_eq!(out[1].id, 7);
        assert_eq!(out[1].parts[0].0, vec![[10, 20, 30, 40]]);
    }

    #[test]
    fn garbage_is_refused_not_misread() {
        assert!(decode(b"").is_none());
        assert!(decode(b"NOPE\x01\x00\x00\x00\x00\x00").is_none());
        // Right magic, wrong version — must refuse rather than misinterpret.
        let mut v = encode(&[]);
        v[4] = 99;
        assert!(decode(&v).is_none());
    }

    #[test]
    fn geom_hash_notices_moved_vertices_but_tolerates_float_noise() {
        let mk = |x: f32| {
            vec![floptle_render::Vertex { pos: [x, 0.0, 0.0], normal: [0.0, 1.0, 0.0], uv: [0.0; 2] }]
        };
        assert_eq!(geom_hash(&mk(1.0)), geom_hash(&mk(1.0)));
        // A real move changes the hash → the guard fires.
        assert_ne!(geom_hash(&mk(1.0)), geom_hash(&mk(1.5)));
        // Sub-quantum float noise does NOT → the guard doesn't cry wolf on a re-export.
        assert_eq!(geom_hash(&mk(1.0)), geom_hash(&mk(1.0 + 1e-7)));
        // Vertex COUNT is part of the hash.
        let mut two = mk(1.0);
        two.push(two[0]);
        assert_ne!(geom_hash(&mk(1.0)), geom_hash(&two));
    }
}
