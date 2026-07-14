//! Project-defined **collision/query layers** + the resolution point everything
//! shares. A project names up to [`MAX_LAYERS`] layers (Project Settings);
//! scenes, scripts and the Inspector reference them **by name** — so reordering
//! the project's layer list can never silently re-layer a scene (the classic
//! index-based footgun). Names resolve to bit indices here, once per Play:
//! physics filters body-vs-collider contacts through the collision matrix, and
//! raycasts filter with the same bits (`raycast(..., { layers = {"Ground"} })`).
//!
//! The matrix is stored in `project.ron` as **exceptions** — the pairs that
//! DON'T collide, by name — so the default is "everything collides" and the
//! file stays tiny and readable.

use crate::ecs::{Entity, World};
use crate::matter::Layer;

/// Hard cap on project layers — each layer is one bit of a `u32` mask.
pub const MAX_LAYERS: usize = 32;

/// The implicit layer every node starts on (bit 0). It always exists and
/// can't be removed; a node with no [`Layer`] component is on it.
pub const DEFAULT_LAYER: &str = "Default";

/// The project's resolved layer table: names (index = bit) plus the collision
/// matrix. Built once per Play from `project.ron` and handed to physics and
/// the script host — the single place layer names become bits.
#[derive(Clone, Debug, PartialEq)]
pub struct Layers {
    /// Layer names; the Vec index is the bit index. `names[0]` is always
    /// [`DEFAULT_LAYER`].
    pub names: Vec<String>,
    /// Bit `j` of `matrix[i]` = layers `i` and `j` collide (kept symmetric).
    pub matrix: [u32; MAX_LAYERS],
}

impl Default for Layers {
    fn default() -> Self {
        Self::resolve(vec![DEFAULT_LAYER.to_string()], &[])
    }
}

impl Layers {
    /// Build from the project's layer list + its "these pairs don't collide"
    /// exceptions (both by name). Guarantees [`DEFAULT_LAYER`] sits at index 0,
    /// drops blank/duplicate names, and caps at [`MAX_LAYERS`]. Exception pairs
    /// naming unknown layers are ignored (a removed layer's rule just expires).
    pub fn resolve(mut names: Vec<String>, no_collide: &[(String, String)]) -> Self {
        names.retain(|n| !n.trim().is_empty());
        if let Some(p) = names.iter().position(|n| n == DEFAULT_LAYER) {
            names.remove(p);
        }
        names.insert(0, DEFAULT_LAYER.to_string());
        let mut seen = std::collections::HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        names.truncate(MAX_LAYERS);
        let mut me = Self { names, matrix: [!0u32; MAX_LAYERS] };
        for (a, b) in no_collide {
            if let (Some(i), Some(j)) = (me.index_of(a), me.index_of(b)) {
                me.matrix[i as usize] &= !(1u32 << j);
                me.matrix[j as usize] &= !(1u32 << i);
            }
        }
        me
    }

    /// The bit index of `name`, or `None` if the project doesn't define it.
    pub fn index_of(&self, name: &str) -> Option<u8> {
        self.names.iter().position(|n| n == name).map(|i| i as u8)
    }

    /// The bit index a node resolves to: its [`Layer`] component's name, else
    /// Default (0). An unknown name also falls back to Default — the editor
    /// warns about those at Play start; physics never guesses.
    pub fn index_for(&self, world: &World, e: Entity) -> u8 {
        world.get::<Layer>(e).and_then(|l| self.index_of(&l.0)).unwrap_or(0)
    }

    /// Whether layers `a` and `b` collide per the matrix (out-of-range = yes,
    /// matching the all-collide default).
    pub fn collides(&self, a: u8, b: u8) -> bool {
        match self.matrix.get(a as usize) {
            Some(row) => (row >> b) & 1 == 1,
            None => true,
        }
    }

    /// A query bitmask covering the named layers (unknown names contribute
    /// nothing — callers that want a hard error validate with [`Self::index_of`]).
    pub fn mask_of<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> u32 {
        names.into_iter().filter_map(|n| self.index_of(n)).fold(0u32, |m, i| m | (1u32 << i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layer_is_pinned_first_and_deduped() {
        let l = Layers::resolve(
            vec!["Enemies".into(), "Default".into(), "".into(), "Enemies".into()],
            &[],
        );
        assert_eq!(l.names, vec!["Default".to_string(), "Enemies".to_string()]);
        assert_eq!(l.index_of("Default"), Some(0));
        assert_eq!(l.index_of("Enemies"), Some(1));
        assert_eq!(l.index_of("Nope"), None);
    }

    #[test]
    fn no_collide_exceptions_clear_both_directions() {
        let l = Layers::resolve(
            vec!["Default".into(), "Ghosts".into(), "Walls".into()],
            &[("Ghosts".into(), "Walls".into())],
        );
        assert!(!l.collides(1, 2));
        assert!(!l.collides(2, 1));
        assert!(l.collides(0, 1), "unlisted pairs still collide");
        assert!(l.collides(1, 1), "a layer collides with itself unless excepted");
    }

    #[test]
    fn masks_cover_known_names_only() {
        let l = Layers::resolve(vec!["Default".into(), "Ground".into()], &[]);
        assert_eq!(l.mask_of(["Ground"]), 0b10);
        assert_eq!(l.mask_of(["Ground", "Default"]), 0b11);
        assert_eq!(l.mask_of(["Typo"]), 0);
    }

    #[test]
    fn nodes_resolve_by_component_name_with_default_fallback() {
        let l = Layers::resolve(vec!["Default".into(), "Enemies".into()], &[]);
        let mut w = World::default();
        let plain = w.spawn();
        let tagged = w.spawn();
        w.insert(tagged, Layer("Enemies".into()));
        let stale = w.spawn();
        w.insert(stale, Layer("RemovedLayer".into()));
        assert_eq!(l.index_for(&w, plain), 0);
        assert_eq!(l.index_for(&w, tagged), 1);
        assert_eq!(l.index_for(&w, stale), 0, "unknown names fall back to Default");
    }
}
