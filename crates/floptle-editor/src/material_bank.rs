//! The project's materials: the `.ron` files under `materials/`, which any
//! node, model part or map face can follow.
//!
//! A material that follows one names it in [`floptle_core::Material::source`]
//! and carries a copy of it. The copy is what draws — in the editor and in a
//! built game alike — and this module keeps it current: on load, and every
//! time the project material changes, every copy is rewritten from the file.
//! So a brick wall built from forty map meshes is restyled by one edit.

use crate::Editor;
use floptle_core::{Entity, Material, ObjectMaterials};

/// The folder the project's materials live in, relative to the project root.
pub(crate) const BANK_DIR: &str = "materials";

/// The link a material stores to follow the project material `name`.
pub(crate) fn bank_link(name: &str) -> String {
    format!("{BANK_DIR}/{name}.ron")
}

/// The project material a link names, when it names one.
pub(crate) fn bank_name(link: &str) -> Option<&str> {
    link.strip_prefix(BANK_DIR)?.strip_prefix('/')?.strip_suffix(".ron").filter(|n| !n.contains('/'))
}

/// `base`, or `base 2`, `base 3`… — the first not already in `taken`.
pub(crate) fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = base.trim();
    let base = if base.is_empty() { "Material" } else { base };
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base} {n}")).find(|n| !taken(n)).unwrap_or_else(|| base.to_string())
}

/// Which material the view edits.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MaterialTarget {
    /// A node's own Material.
    Node(Entity),
    /// One part of a node — a model's sub-object or a map mesh's face slot:
    /// the node's `ObjectMaterials` entry under this key.
    Part(Entity, String),
}

impl MaterialTarget {
    pub(crate) fn entity(&self) -> Entity {
        match self {
            MaterialTarget::Node(e) | MaterialTarget::Part(e, _) => *e,
        }
    }
}

/// The material a target names, if it is still there.
pub(crate) fn target_material<'w>(world: &'w mut floptle_core::World, t: &MaterialTarget) -> Option<&'w mut Material> {
    match t {
        MaterialTarget::Node(e) => world.get_mut::<Material>(*e),
        MaterialTarget::Part(e, key) => world.get_mut::<ObjectMaterials>(*e)?.0.get_mut(key),
    }
}

impl Editor {
    /// The project material `name`, as the material a follower wears.
    pub(crate) fn bank_material(&self, name: &str) -> Option<Material> {
        let (_, doc) = self.materials.iter().find(|(n, _)| n == name)?;
        let mut m = doc.to_material();
        m.source = Some(bank_link(name));
        Some(m)
    }

    /// Rewrite every material that follows a project material from the
    /// current file. One whose project material is gone keeps the last copy it
    /// had, so deleting a file never blanks a scene. Returns how many changed.
    pub(crate) fn sync_linked_materials(&mut self) -> usize {
        let mut changed = 0;
        let nodes: Vec<Entity> = self
            .world
            .query::<Material>()
            .filter(|(_, m)| m.source.is_some())
            .map(|(e, _)| e)
            .collect();
        for e in nodes {
            let fresh = self
                .world
                .get::<Material>(e)
                .and_then(|m| m.source.as_deref())
                .and_then(bank_name)
                .and_then(|n| self.bank_material(n));
            if let Some(fresh) = fresh
                && let Some(m) = self.world.get_mut::<Material>(e)
                && *m != fresh
            {
                *m = fresh;
                changed += 1;
            }
        }
        let parts: Vec<Entity> = self
            .world
            .query::<ObjectMaterials>()
            .filter(|(_, om)| om.0.values().any(|m| m.source.is_some()))
            .map(|(e, _)| e)
            .collect();
        for e in parts {
            let Some(om) = self.world.get::<ObjectMaterials>(e) else { continue };
            let fresh: Vec<(String, Material)> = om
                .0
                .iter()
                .filter_map(|(k, m)| {
                    let f = self.bank_material(bank_name(m.source.as_deref()?)?)?;
                    (f != *m).then(|| (k.clone(), f))
                })
                .collect();
            if let Some(om) = self.world.get_mut::<ObjectMaterials>(e) {
                for (k, f) in fresh {
                    om.0.insert(k, f);
                    changed += 1;
                }
            }
        }
        changed
    }

    /// Write the project material `name` and restyle everything following it.
    pub(crate) fn store_bank_material(&mut self, name: &str, doc: &floptle_scene::MaterialDoc) {
        let mut doc = doc.clone();
        // A project material follows nothing itself.
        doc.source = None;
        let _ = floptle_scene::save_material(name, &doc, &self.materials_dir());
        match self.materials.iter_mut().find(|(n, _)| n == name) {
            Some((_, d)) => *d = doc,
            None => {
                self.materials.push((name.to_string(), doc));
                self.materials.sort_by(|a, b| a.0.cmp(&b.0));
            }
        }
        self.sync_linked_materials();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_name_their_project_material() {
        assert_eq!(bank_link("Brick"), "materials/Brick.ron");
        assert_eq!(bank_name("materials/Brick.ron"), Some("Brick"));
        assert_eq!(bank_name("materials/sub/Brick.ron"), None);
        assert_eq!(bank_name("textures/Brick.ron"), None);
        assert_eq!(unique_name("Brick", |n| n == "Brick" || n == "Brick 2"), "Brick 3");
        assert_eq!(unique_name("  ", |_| false), "Material");
    }

    /// **One edit to a project material restyles everything that follows it**
    /// — node materials and model/map part materials alike — and leaves
    /// materials of their own alone. A follower whose file is gone keeps its
    /// last copy rather than going blank.
    #[test]
    fn editing_a_project_material_restyles_its_followers() {
        let dir = std::env::temp_dir().join(format!("floptle-bank-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = Editor { project_root: dir.clone(), ..Default::default() };
        let brick = floptle_scene::MaterialDoc { color: [0.8, 0.3, 0.2], ..Default::default() };
        ed.store_bank_material("Brick", &brick);
        assert!(dir.join("materials/Brick.ron").is_file());

        let wall = ed.world.spawn();
        ed.world.insert(wall, ed.bank_material("Brick").unwrap());
        let own = ed.world.spawn();
        let blue = Material { color: [0.0, 0.0, 1.0], ..Default::default() };
        ed.world.insert(own, blue.clone());
        let model = ed.world.spawn();
        let mut om = ObjectMaterials::default();
        om.0.insert("Roof".into(), ed.bank_material("Brick").unwrap());
        om.0.insert("Door".into(), blue.clone());
        ed.world.insert(model, om);
        let ghost = ed.world.spawn();
        let gone = Material { color: [0.5; 3], source: Some(bank_link("Gone")), ..Default::default() };
        ed.world.insert(ghost, gone.clone());

        let mossy = floptle_scene::MaterialDoc { color: [0.2, 0.6, 0.2], ..brick };
        ed.store_bank_material("Brick", &mossy);

        assert_eq!(ed.world.get::<Material>(wall).unwrap().color, [0.2, 0.6, 0.2]);
        assert_eq!(ed.world.get::<Material>(wall).unwrap().source.as_deref(), Some("materials/Brick.ron"));
        let om = ed.world.get::<ObjectMaterials>(model).unwrap();
        assert_eq!(om.0["Roof"].color, [0.2, 0.6, 0.2]);
        assert_eq!(om.0["Door"], blue, "a material of its own is left alone");
        assert_eq!(ed.world.get::<Material>(own), Some(&blue));
        assert_eq!(ed.world.get::<Material>(ghost), Some(&gone), "the last copy is kept");

        // What was written is what a reload reads back.
        let back = floptle_scene::load_materials(&dir.join("materials"));
        assert_eq!(back.iter().find(|(n, _)| n == "Brick").map(|(_, d)| d.color), Some([0.2, 0.6, 0.2]));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
