//! Is the art drawn at the density the camera keeps pixel-perfect?
//!
//! A 2D camera with `artPixels` holds its zoom where each art pixel covers a
//! whole number of screen pixels. That only helps art drawn at that many
//! pixels per world unit: a sprite at a different density can never sit on
//! the same grid as the level, and its pixels come out uneven whatever the
//! camera does. The usual cause is a node scaled up to make a small sprite
//! bigger. Read off the scene's nodes, so `floptle check` and Play report the
//! same thing.

use std::collections::HashMap;
use std::path::Path;

use floptle_scene::{MatterDoc, NodeDoc};

/// One line per Sprite or Tilemap node whose art pixels per world unit (its
/// texture's pixels over its world size, parent scale included) differs from
/// the `artPixels` a 2D camera in `nodes` asks for. Empty when no camera asks.
pub(crate) fn art_mismatches(root: &Path, nodes: &[NodeDoc]) -> Vec<String> {
    let Some(art) = nodes
        .iter()
        .filter_map(|n| n.camera_2d.as_ref())
        .map(|c| c.art_pixels)
        .find(|a| *a > 0.0)
    else {
        return Vec::new();
    };
    let by_id: HashMap<u32, &NodeDoc> = nodes.iter().filter_map(|n| n.id.map(|id| (id, n))).collect();
    // The node's horizontal scale in the world: its own times every parent's.
    let world_scale = |n: &NodeDoc| {
        let mut s = n.transform.scale[0].abs();
        let mut at = n.parent_id;
        let mut guard = 0;
        while let Some(p) = at.and_then(|id| by_id.get(&id)) {
            s *= p.transform.scale[0].abs();
            at = p.parent_id;
            guard += 1;
            if guard > 256 {
                break;
            }
        }
        s.max(1e-6)
    };
    let mut widths: HashMap<String, Option<u32>> = HashMap::new();
    let mut width_of = |tex: &str| -> Option<u32> {
        *widths.entry(tex.to_string()).or_insert_with(|| {
            let abs = crate::project::resolve_asset_path(root, tex);
            floptle_assets::load_texture(&abs).map(|t| t.width)
        })
    };
    let mut out = Vec::new();
    for n in nodes {
        let who = if n.name.is_empty() { "an unnamed node" } else { n.name.as_str() };
        let density = match &n.matter {
            MatterDoc::Sprite { ppu, size, .. } => {
                let m = n.material.as_ref();
                if *ppu > 0.0 {
                    Some(*ppu / world_scale(n))
                } else {
                    let tex = m.and_then(|m| m.texture.as_deref()).filter(|t| !t.is_empty());
                    let cols = m.map(|m| m.sheet_cols.max(1)).unwrap_or(1);
                    tex.and_then(&mut width_of).map(|w| w as f32 / cols as f32 / (size * world_scale(n)))
                }
            }
            MatterDoc::Tilemap { tile, tileset, .. } if !tileset.is_empty() => {
                let set = floptle_vfs::read_to_string(root.join(tileset))
                    .ok()
                    .and_then(|t| floptle_tiles::TileSet::from_ron(&t).ok());
                set.and_then(|s| {
                    let w = width_of(&s.texture)?;
                    Some(w as f32 / s.sheet_cols.max(1) as f32 / (tile * world_scale(n)))
                })
            }
            _ => None,
        };
        if let Some(d) = density
            && (d - art).abs() > art * 0.01
        {
            out.push(format!(
                "{who} draws its art at {d:.1} pixels per unit, and the camera keeps {art} pixels per unit \
                 pixel-perfect, so its pixels cannot sit on the same grid as the rest; match its \
                 pixels per unit (and leave its scale at 1) to {art}"
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node from scene-file text, the way a scene on disk says it.
    fn node(text: &str) -> NodeDoc {
        ron::from_str(text).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    fn sprite(name: &str, ppu: f32, scale: f32) -> NodeDoc {
        node(&format!(
            "(name: \"{name}\", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: ({scale}, {scale}, 1.0)), matter: Sprite(ppu: {ppu}), scripts: [])"
        ))
    }

    fn camera(art: f32) -> NodeDoc {
        node(&format!(
            "(name: \"Camera\", transform: (translation: (0.0, 0.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), \
             scale: (1.0, 1.0, 1.0)), matter: Empty, scripts: [], camera_2d: Some((art_pixels: {art})))"
        ))
    }

    /// The Runmanguy case: 8-pixel tiles, and a player sprite of 32 pixels per
    /// unit under a node scaled 3x — 10.7 art pixels a unit against the tiles'
    /// 8. Only the player is named.
    #[test]
    fn a_scaled_sprite_is_named_and_matching_art_is_not() {
        let lines = art_mismatches(
            Path::new("/nonexistent"),
            &[camera(8.0), sprite("Player", 32.0, 3.0), sprite("Coin", 8.0, 1.0)],
        );
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Player draws its art at 10.7"), "{}", lines[0]);
    }

    #[test]
    fn nothing_is_checked_without_a_camera_asking() {
        assert!(art_mismatches(Path::new("/nonexistent"), &[sprite("Player", 32.0, 3.0)]).is_empty());
    }
}
