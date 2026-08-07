//! **Tilemap kernel** — everything about tiles that is not drawing them.
//!
//! A `Matter::Tilemap` node holds the grid; [`floptle_render::mesh::tilemap`]
//! turns it into one mesh; the packed square (cell index + orientation) is
//! defined in `floptle_core::tile`. This crate is the layer between: what each
//! tile *means*, how a group of tiles picks itself, what a tool does to a grid,
//! and how solid tiles become colliders.
//!
//! It has no editor and no renderer in it, which is the point — the ▦ Tiles tab
//! is a view onto these functions, so "does the bucket fill leak through a
//! diagonal" and "does a rotated stamp land where its preview showed" are tests
//! in the gate rather than things somebody notices in a level six weeks later.
//!
//! ## The four pieces
//!
//! * [`tileset`] — [`TileSet`]: per-tile collision, tags, autotile group and
//!   animation frames, saved to `<project>/tilesets/<name>.tileset.ron`. Authored
//!   once per spritesheet, shared by every tilemap cut from it.
//! * [`autotile`] — neighbour masks, the corner rule that turns 256
//!   neighbourhoods into 47, and [`Autotiler`], the resolver a paint stroke asks.
//! * [`grid`] — [`TileGrid`] and the tools: brush, rectangle, line, bucket,
//!   stamp, move, re-orient, resize, retile.
//! * [`collide`] — the greedy rectangle merge that makes a 100×100 solid floor
//!   ONE box instead of ten thousand.
//!
//! ## What is deliberately not here
//!
//! **Layers.** A tilemap layer is a `Matter::Tilemap` NODE — it already has a
//! transform, a material, a visibility flag, a name and a place in the hierarchy.
//! A second layer concept inside the component would duplicate all five and then
//! disagree with them: a layer you could hide in the Tiles tab but not in the
//! Hierarchy, or that had a Z order the transform contradicted. The ▦ Tiles tab
//! lists the scene's tilemap nodes as its layer list, and everything that moves,
//! hides or reorders a layer is the ordinary node operation.
//!
//! **Infinite grids / chunking.** A tilemap is `cols × rows`, and a very large
//! world is several tilemap nodes. Chunking belongs here eventually; it is not
//! needed to build a level, and a chunk boundary is a seam to get wrong.

pub mod autotile;
pub mod collide;
pub mod grid;
pub mod tileset;

pub use autotile::{canonical, preset_len, preset_masks, Autotiler};
pub use collide::{collision_shapes, solid_count, TileBox, TileColliders, TilePoly};
pub use grid::{tile_mask, tile_masks, Stamp, TileGrid};
pub use tileset::{
    AutotileGroup, AutotileKind, AutotileRule, TileCollision, TileInfo, TilePage, TileSet, TileShape,
    TileSide,
};

/// The folder a project keeps its tilesets in, relative to the project root.
pub const TILESET_DIR: &str = "tilesets";

/// The extension a tileset file carries. Two dots, matching `.prefab.ron` — a
/// tileset is a `.ron` (so an editor opens it as one, and a diff reads as one)
/// that the engine also knows the shape of.
pub const TILESET_EXT: &str = ".tileset.ron";

/// The project-relative path a tileset with this name is stored at.
pub fn tileset_path(name: &str) -> String {
    format!("{TILESET_DIR}/{name}{TILESET_EXT}")
}

/// The name behind a tileset path, if it is one.
pub fn tileset_name(path: &str) -> Option<&str> {
    let file = path.rsplit(['/', '\\']).next()?;
    file.strip_suffix(TILESET_EXT).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tileset_path_round_trips_through_its_name() {
        assert_eq!(tileset_path("bricks"), "tilesets/bricks.tileset.ron");
        assert_eq!(tileset_name("tilesets/bricks.tileset.ron"), Some("bricks"));
        assert_eq!(tileset_name(&tileset_path("cave walls")), Some("cave walls"));
        // A windows-style path a project file might carry.
        assert_eq!(tileset_name(r"tilesets\bricks.tileset.ron"), Some("bricks"));
    }

    #[test]
    fn something_that_is_not_a_tileset_is_not_named_one() {
        assert_eq!(tileset_name("scenes/first.ron"), None);
        assert_eq!(tileset_name("bricks.ron"), None);
        assert_eq!(tileset_name(".tileset.ron"), None, "an empty name is not a name");
        assert_eq!(tileset_name(""), None);
    }
}
