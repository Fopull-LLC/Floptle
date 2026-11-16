//! The material texture registry: an image path, as a material writes it, to
//! the texture on the GPU that draws it.
//!
//! A map that counts its own changes. Two per-frame passes ask "could what I
//! decided last frame be wrong now?" about it — the scene's texture warm-up
//! (`ensure_scene_textures`) and the shader-material walk
//! (`ensure_flsl_materials`) — and both used to answer by looking at all of
//! it every frame. With the counts each answers with one comparison.
//!
//! Reading goes straight through to the map. Writing goes through the four
//! methods here, so a write that does not count is not one that compiles.

use std::collections::HashMap;
use std::ops::Deref;

use floptle_render::TexId;

#[derive(Default)]
pub(crate) struct TextureRegistry {
    map: HashMap<String, TexId>,
    /// Moves whenever a path comes to name a different texture, or none.
    revision: u64,
    /// Moves whenever a path is dropped: reloaded from disk, given new import
    /// settings, freed by the script that made it, or the project closed. A
    /// material naming that path needs it registered again.
    evictions: u64,
}

impl Deref for TextureRegistry {
    type Target = HashMap<String, TexId>;

    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

impl TextureRegistry {
    /// Register `id` under `path`. Putting back the texture a path already
    /// names changes nothing, and is not counted as a change.
    pub(crate) fn insert(&mut self, path: String, id: TexId) -> Option<TexId> {
        let old = self.map.insert(path, id);
        if old != Some(id) {
            self.revision += 1;
        }
        old
    }

    /// Drop `path`, so whatever names it is registered again on next use.
    pub(crate) fn remove(&mut self, path: &str) -> Option<TexId> {
        let old = self.map.remove(path);
        if old.is_some() {
            self.dropped();
        }
        old
    }

    /// Keep only the paths `keep` accepts.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(&String, &mut TexId) -> bool) {
        let before = self.map.len();
        self.map.retain(|k, v| keep(k, v));
        if self.map.len() != before {
            self.dropped();
        }
    }

    /// Drop every path.
    pub(crate) fn clear(&mut self) {
        if !self.map.is_empty() {
            self.map.clear();
            self.dropped();
        }
    }

    /// A number that moves whenever any path's texture changed. Unchanged, a
    /// pass that resolved paths through the registry would resolve them the
    /// same way again.
    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }

    /// A number that moves whenever any path was dropped. Unchanged, every
    /// path that was registered still is.
    pub(crate) fn evictions(&self) -> u64 {
        self.evictions
    }

    fn dropped(&mut self) {
        self.revision += 1;
        self.evictions += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_write_that_changes_a_path_is_counted() {
        let mut r = TextureRegistry::default();
        r.insert("a.png".into(), TexId(1));
        assert_eq!((r.revision(), r.evictions()), (1, 0));
        r.insert("a.png".into(), TexId(1));
        assert_eq!(r.revision(), 1, "the same texture put back is not a change");
        r.insert("a.png".into(), TexId(2));
        assert_eq!((r.revision(), r.evictions()), (2, 0), "a reload under a new id is a change");
        assert!(r.remove("missing.png").is_none());
        assert_eq!(r.evictions(), 0, "removing what was never there changes nothing");
        r.remove("a.png");
        assert_eq!((r.revision(), r.evictions()), (3, 1));
        r.insert("b.png".into(), TexId(3));
        r.insert("c.png".into(), TexId(4));
        r.retain(|_, _| true);
        assert_eq!(r.evictions(), 1, "a retain that kept everything dropped nothing");
        r.retain(|k, _| k != "b.png");
        assert_eq!(r.evictions(), 2);
        r.clear();
        assert_eq!(r.evictions(), 3);
        r.clear();
        assert_eq!(r.evictions(), 3, "clearing an empty registry dropped nothing");
        assert!(r.is_empty());
    }
}
