//! Scene graph operations: spawn/delete/duplicate, the node clipboard,
//! re-parenting, component paste, and asset drops into the scene.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::Name;
use floptle_core::ScriptInst;
use floptle_core::Scripts;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use floptle_scene::MaterialDoc;
use floptle_scene::MatterDoc;
use floptle_scene::NodeDoc;
use floptle_scene::ScriptDoc;
use floptle_scene::TransformDoc;
use crate::assets::{is_model, is_script};
use crate::inspector::{ComponentClip};
use crate::matter_catalog::{matter_doc_name};
use crate::{Editor, snap_dvec3};

impl Editor {
    /// Paste the component clipboard onto `e` (the held clip decides the kind). Adds
    /// the component if missing, else overwrites its values; scripts add-or-update by
    /// name. Pasting a "type" (Matter) never morphs a Terrain node (its field is
    /// out-of-ECS).
    pub(crate) fn paste_onto(&mut self, e: Entity) {
        let Some(clip) = self.component_clip.clone() else { return };
        if !self.world.is_alive(e) {
            return;
        }
        self.record();
        let mut physics = false;
        match clip {
            ComponentClip::Transform(t) => {
                if let Some(cur) = self.world.get_mut::<Transform>(e) {
                    *cur = t;
                }
            }
            ComponentClip::Matter(m) => {
                // Terrain keeps its type (out-of-ECS field). The PostProcess node only
                // accepts PostProcess values (that's how settings copy between scenes),
                // and no other node may be turned into one by paste.
                let target_is_post =
                    matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. }));
                let clip_is_post = matches!(m, Matter::PostProcess { .. });
                if !matches!(self.world.get::<Matter>(e), Some(Matter::Terrain { .. }))
                    && target_is_post == clip_is_post
                {
                    self.world.insert(e, m);
                    physics = true;
                }
            }
            ComponentClip::Material(m) => {
                self.world.insert(e, *m);
            }
            ComponentClip::RigidBody(rb) => {
                self.world.insert(e, rb);
                physics = true;
            }
            ComponentClip::Script(si) => {
                let scripts = match self.world.get_mut::<Scripts>(e) {
                    Some(s) => s,
                    None => {
                        self.world.insert(e, Scripts::default());
                        self.world.get_mut::<Scripts>(e).unwrap()
                    }
                };
                if let Some(existing) = scripts.0.iter_mut().find(|i| i.kind == si.kind) {
                    existing.params = si.params;
                    existing.enabled = si.enabled;
                } else {
                    scripts.0.push(si);
                }
            }
        }
        if physics {
            self.rebuild_sim();
        }
    }

    // ---- node create / delete / clipboard -----------------------------------
    pub(crate) fn node_of(&self, e: Entity) -> Option<NodeDoc> {
        let matter = self.world.get::<Matter>(e)?;
        let transform =
            self.world.get::<Transform>(e).map(TransformDoc::from).unwrap_or_default();
        let name = self.world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_else(|| "node".into());
        let scripts = self
            .world
            .get::<Scripts>(e)
            .map(|s| {
                s.0.iter()
                    .map(|i| ScriptDoc {
                        kind: i.kind.clone(),
                        enabled: i.enabled,
                        params: i.params.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let material = self.world.get::<Material>(e).map(MaterialDoc::from_material);
        let rigidbody =
            self.world.get::<floptle_core::RigidBody>(e).map(floptle_scene::RigidBodyDoc::from_rigidbody);
        let mesh_collider = self.world.get::<floptle_core::MeshCollider>(e).is_some();
        let collidable = self.world.get::<floptle_core::Collidable>(e).is_some();
        let visible = self.world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
        let cast_shadow =
            self.world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
        let anim_controller =
            self.world.get::<floptle_core::AnimController>(e).map(|c| c.asset.clone());
        Some(NodeDoc {
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            rigidbody,
            mesh_collider,
            collidable,
            visible,
            cast_shadow,
            anim_controller,
            parent: None,
        })
    }

    pub(crate) fn spawn_node(&mut self, node: &NodeDoc) -> Entity {
        let e = self.world.spawn();
        self.world.insert(e, node.transform.to_transform());
        self.world.insert(e, Name(node.name.clone()));
        self.world.insert(e, node.matter.to_matter());
        if !node.scripts.is_empty() {
            let insts = node
                .scripts
                .iter()
                .map(|s| ScriptInst {
                    kind: s.kind.clone(),
                    enabled: s.enabled,
                    params: s.params.clone(),
                })
                .collect();
            self.world.insert(e, Scripts(insts));
        }
        if let Some(m) = &node.material {
            self.world.insert(e, m.to_material());
        }
        if let Some(rb) = &node.rigidbody {
            self.world.insert(e, rb.to_rigidbody());
        }
        if node.mesh_collider {
            self.world.insert(e, floptle_core::MeshCollider);
        }
        if node.collidable {
            self.world.insert(e, floptle_core::Collidable);
        }
        if !node.visible {
            self.world.insert(e, floptle_core::Visible(false));
        }
        if let Some(ctl) = &node.anim_controller {
            self.world.insert(e, floptle_core::AnimController { asset: ctl.clone() });
        }
        e
    }

    /// Spawn a new node ~5 units in front of the camera, and select it.
    pub(crate) fn add_node(&mut self, name: &str, matter: MatterDoc) {
        self.record();
        let cam = self.camera.render_camera();
        let mut pos = cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3();
        if self.grid.snap {
            pos = snap_dvec3(pos, self.grid.size as f64);
        }
        let node = NodeDoc {
            name: name.into(),
            transform: TransformDoc { translation: [pos.x, pos.y, pos.z], ..Default::default() },
            matter,
            scripts: Vec::new(),
            material: None,
            rigidbody: None,
            mesh_collider: false,
            collidable: false,
            visible: true,
            cast_shadow: true,
            anim_controller: None,
            parent: None,
        };
        let e = self.spawn_node(&node);
        self.select_single(e);
    }

    /// Drop of an asset from the browser: spawn a model, or attach a script to the
    /// selection (a model dropped on the viewport, a script anywhere).
    pub(crate) fn drop_asset(&mut self, path: &str) {
        if is_model(path) {
            if !self.import_model(path) {
                return;
            }
            self.record();
            let pos = self.cursor_world();
            let name = std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "mesh".into());
            let node = NodeDoc {
                name,
                transform: TransformDoc {
                    translation: [pos.x, pos.y, pos.z],
                    ..Default::default()
                },
                matter: MatterDoc::Mesh { asset_path: path.to_string() },
                scripts: Vec::new(),
                material: None,
                rigidbody: None,
                mesh_collider: false,
                collidable: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                parent: None,
            };
            let e = self.spawn_node(&node);
            self.select_single(e);
        } else if is_script(path) {
            self.attach_script_file(path, self.primary());
        }
    }

    pub(crate) fn delete_selected(&mut self) {
        let mut targets = self.selected_matter();
        // The PostProcess node is mandatory — every scene has exactly one. Disable
        // the chain with its `enabled` switch instead of deleting the node.
        let n = targets.len();
        targets.retain(|&e| !matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })));
        if targets.len() != n {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "Post Processing is a mandatory scene node and can't be deleted — untick 'enabled' on it to turn post-processing off".into(),
                None,
            );
        }
        if targets.is_empty() {
            return;
        }
        self.record();
        for e in targets {
            if self.terrains.remove(&e).is_some() {
                if self.active_terrain == Some(e) {
                    self.active_terrain = None;
                }
                self.terrain_gpu_dirty = true;
            }
            self.world.despawn(e);
        }
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
    }

    /// Selected entities minus the PostProcess node — a scene has exactly one, so
    /// copy/duplicate never clone it (copy its VALUES via the Type header instead).
    pub(crate) fn selected_matter_duplicable(&self) -> Vec<Entity> {
        let mut v = self.selected_matter();
        v.retain(|&e| !matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })));
        v
    }

    pub(crate) fn copy_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter_duplicable().iter().filter_map(|&e| self.node_of(e)).collect();
        if !nodes.is_empty() {
            self.clipboard = nodes;
        }
    }

    /// Spawn the given nodes (offset slightly) and select them — used by paste/dup.
    pub(crate) fn spawn_offset(&mut self, nodes: Vec<NodeDoc>) {
        if nodes.is_empty() {
            return;
        }
        self.record();
        self.selection.clear();
        for mut node in nodes {
            node.transform.translation[0] += 0.5;
            node.transform.translation[2] += 0.5;
            let e = self.spawn_node(&node);
            self.selection.push(e);
        }
    }

    pub(crate) fn paste(&mut self) {
        let nodes = self.clipboard.clone();
        self.spawn_offset(nodes);
    }

    pub(crate) fn duplicate_selected(&mut self) {
        let nodes: Vec<NodeDoc> =
            self.selected_matter_duplicable().iter().filter_map(|&e| self.node_of(e)).collect();
        self.spawn_offset(nodes);
    }

    // ---- scene-graph (parenting) -------------------------------------------
    /// True if `e` is `ancestor` or one of its descendants (cycle guard).
    pub(crate) fn is_descendant(&self, e: Entity, ancestor: Entity) -> bool {
        let mut cur = e;
        for _ in 0..64 {
            if cur == ancestor {
                return true;
            }
            match self.world.get::<floptle_core::Parent>(cur).copied() {
                Some(floptle_core::Parent(p)) => cur = p,
                None => return false,
            }
        }
        false
    }

    /// Re-parent `child` under `parent` (or make it a root if `None`), preserving
    /// its world placement. Rejects cycles (can't parent under your own descendant).
    pub(crate) fn reparent(&mut self, child: Entity, parent: Option<Entity>) {
        if let Some(p) = parent
            && self.is_descendant(p, child) {
                return;
            }
        self.record();
        let world = floptle_core::world_transform(&self.world, child);
        match parent {
            Some(p) => self.world.insert(child, floptle_core::Parent(p)),
            None => {
                self.world.remove::<floptle_core::Parent>(child);
            }
        }
        self.set_world_transform(child, world); // keep the same world placement
    }

    /// Spawn a new node as a child of `parent`, sitting at the parent's origin.
    pub(crate) fn add_parented(&mut self, matter: MatterDoc, parent: Entity) {
        self.record();
        let name = matter_doc_name(&matter);
        let e = self.world.spawn();
        self.world.insert(e, Transform::IDENTITY);
        self.world.insert(e, Name(name.into()));
        self.world.insert(e, matter.to_matter());
        self.world.insert(e, floptle_core::Parent(parent));
        self.select_single(e);
    }
}
