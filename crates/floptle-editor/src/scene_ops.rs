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
            ComponentClip::Particles(p) => {
                self.world.insert(e, p);
            }
            ComponentClip::Audio(a) => {
                self.world.insert(e, a);
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
                        refs: i.refs.clone(),
                        strs: i.strs.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let material = self.world.get::<Material>(e).map(MaterialDoc::from_material);
        let rigidbody =
            self.world.get::<floptle_core::RigidBody>(e).map(floptle_scene::RigidBodyDoc::from_rigidbody);
        let celestial = self
            .world
            .get::<floptle_core::CelestialBody>(e)
            .map(floptle_scene::CelestialBodyDoc::from_body);
        let mesh_collider = self.world.get::<floptle_core::MeshCollider>(e).is_some();
        // Carry the paint KEY, not a copy of the colors: the pasted node points at the
        // same block and forks it only if painted (copy-on-write, proposal §9.0). So
        // duplicating a painted prop is free, and painting the copy doesn't touch the
        // original.
        let paint = self.world.get::<floptle_core::VertexPaint>(e).map(|p| p.id);
        let tex_paint = self.world.get::<floptle_core::TexturePaint>(e).map(|p| p.id);
        let collidable = self.world.get::<floptle_core::Collidable>(e).is_some();
        let trigger = self.world.get::<floptle_core::Trigger>(e).is_some();
        let visible = self.world.get::<floptle_core::Visible>(e).map(|v| v.0).unwrap_or(true);
        let cast_shadow =
            self.world.get::<floptle_core::CastShadow>(e).map(|c| c.0).unwrap_or(true);
        let anim_controller =
            self.world.get::<floptle_core::AnimController>(e).map(|c| c.asset.clone());
        let particles = self
            .world
            .get::<floptle_core::ParticleSystem>(e)
            .map(floptle_scene::ParticleSystemDoc::from_component);
        let net = self
            .world
            .get::<floptle_core::Replicated>(e)
            .map(floptle_scene::ReplicatedDoc::from_component);
        let ui_layer = self.world.get::<floptle_ui::UiLayer>(e).copied();
        let ui = self.world.get::<floptle_ui::ElementSpec>(e).cloned();
        let audio = self.world.get::<floptle_audio::AudioSource>(e).cloned();
        let layer = self.world.get::<floptle_core::Layer>(e).map(|l| l.0.clone());
        let tags = self.world.get::<floptle_core::Tags>(e).map(|t| t.0.clone()).unwrap_or_default();
        Some(NodeDoc {
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            rigidbody,
            celestial,
            mesh_collider,
            paint,
            tex_paint,
            collidable,
            trigger,
            visible,
            cast_shadow,
            anim_controller,
            particles,
            parent: None,
            attachment: None, // captured/restored by save-load (to_doc/from_doc), not the clipboard
            net,
            ui_layer,
            ui,
            audio,
            layer,
            tags,
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
                    refs: s.refs.clone(),
                    strs: s.strs.clone(),
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
        if node.trigger {
            self.world.insert(e, floptle_core::Trigger);
        }
        if !node.visible {
            self.world.insert(e, floptle_core::Visible(false));
        }
        if !node.cast_shadow {
            self.world.insert(e, floptle_core::CastShadow(false));
        }
        if let Some(ctl) = &node.anim_controller {
            self.world.insert(e, floptle_core::AnimController { asset: ctl.clone() });
        }
        if let Some(p) = &node.particles {
            self.world.insert(e, p.to_component());
        }
        if let Some(n) = &node.net {
            self.world.insert(e, n.to_component());
        }
        if let Some(l) = &node.ui_layer {
            self.world.insert(e, *l);
        }
        if let Some(u) = &node.ui {
            self.world.insert(e, u.clone());
        }
        if let Some(a) = &node.audio {
            self.world.insert(e, a.clone());
        }
        if let Some(l) = &node.layer {
            self.world.insert(e, floptle_core::Layer(l.clone()));
        }
        if !node.tags.is_empty() {
            self.world.insert(e, floptle_core::Tags(node.tags.clone()));
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
            celestial: None,
            mesh_collider: false,
            paint: None,
            tex_paint: None,
            collidable: false,
            trigger: false,
            visible: true,
            cast_shadow: true,
            anim_controller: None,
            particles: None,
            parent: None,
            attachment: None,
            net: None,
            ui_layer: None,
            ui: None,
            audio: None,
            layer: None,
            tags: Vec::new(),
        };
        let e = self.spawn_node(&node);
        self.select_single(e);
    }

    /// Drop of an asset from the browser: spawn a model or a prefab instance at
    /// the cursor, or attach a script to the selection.
    pub(crate) fn drop_asset(&mut self, path: &str) {
        if crate::assets::is_prefab(path) {
            let at = self.cursor_world();
            self.instantiate_prefab(path, Some(at), None);
        } else if is_model(path) {
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
                celestial: None,
                mesh_collider: false,
                paint: None,
                tex_paint: None,
                collidable: false,
                trigger: false,
                visible: true,
                cast_shadow: true,
                anim_controller: None,
                particles: None,
                parent: None,
                attachment: None,
                net: None,
                ui_layer: None,
                ui: None,
                audio: None,
                layer: None,
                tags: Vec::new(),
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
        // Deleting a node deletes its WHOLE subtree — children don't silently
        // become orphaned roots. (PostProcess stays even if it's a descendant.)
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let mut doomed = Vec::new();
        let mut queue: std::collections::VecDeque<Entity> = targets.into();
        while let Some(e) = queue.pop_front() {
            if matches!(self.world.get::<Matter>(e), Some(Matter::PostProcess { .. })) {
                continue;
            }
            doomed.push(e);
            queue.extend(kids.get(&e).map(|v| v.as_slice()).unwrap_or(&[]));
        }
        for e in doomed {
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

    /// Serialize `roots` — each with its WHOLE subtree — into the flat node-list
    /// format shared by the clipboard and prefab files: `parent` is an index into
    /// the returned list (`None` = a root). Children keep their local transforms
    /// (and bone attachments); roots bake their WORLD transform, since whatever
    /// they were parented to isn't coming along. Selecting both a parent and its
    /// child captures the child once (inside the parent's subtree).
    pub(crate) fn subtree_docs(&self, roots: &[Entity]) -> Vec<NodeDoc> {
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let roots: Vec<Entity> = roots
            .iter()
            .copied()
            .filter(|&r| !roots.iter().any(|&o| o != r && self.is_descendant(r, o)))
            .collect();
        let mut docs: Vec<NodeDoc> = Vec::new();
        let mut queue: std::collections::VecDeque<(Entity, Option<usize>)> =
            roots.iter().map(|&r| (r, None)).collect();
        while let Some((e, pidx)) = queue.pop_front() {
            let Some(mut doc) = self.node_of(e) else { continue };
            doc.parent = pidx;
            if pidx.is_none() {
                doc.transform =
                    TransformDoc::from(&floptle_core::world_transform(&self.world, e));
            } else if let Some(a) = self.world.get::<floptle_core::BoneAttach>(e) {
                // A bone-attached child rides along; its live Transform is a
                // derived pose value, so serialize a stable identity (exactly
                // like scene save — resolve_attachments re-derives it).
                doc.attachment = Some(floptle_scene::AttachmentDoc {
                    bone: a.bone.clone(),
                    offset: TransformDoc::from(&a.offset),
                });
                doc.transform = TransformDoc::default();
            }
            let idx = docs.len();
            docs.push(doc);
            for &k in kids.get(&e).map(|v| v.as_slice()).unwrap_or(&[]) {
                queue.push_back((k, Some(idx)));
            }
        }
        docs
    }

    /// Spawn a flat node list (the clipboard/prefab format), wiring `Parent` and
    /// bone attachments from the internal indices. Returns every spawned entity in
    /// doc order — roots are the entries whose `doc.parent` is `None`.
    pub(crate) fn spawn_docs(&mut self, docs: &[NodeDoc]) -> Vec<Entity> {
        let ents: Vec<Entity> = docs.iter().map(|d| self.spawn_node(d)).collect();
        for (i, d) in docs.iter().enumerate() {
            if let Some(p) = d.parent
                && p != i
                && let Some(&pe) = ents.get(p)
            {
                self.world.insert(ents[i], floptle_core::Parent(pe));
                if let Some(a) = &d.attachment {
                    self.world.insert(
                        ents[i],
                        floptle_core::BoneAttach {
                            target: pe,
                            bone: a.bone.clone(),
                            offset: a.offset.to_transform(),
                        },
                    );
                }
            }
        }
        ents
    }

    /// The tag line marking clipboard text as Floptle nodes (RON follows).
    const NODE_CLIP_TAG: &'static str = "//floptle-nodes-v1";

    /// Lazily connect the OS clipboard (arboard under the hood; falls back to
    /// an in-app buffer if the OS clipboard is unreachable).
    fn ensure_os_clipboard(&mut self) {
        if self.os_clipboard.is_none() {
            use winit::raw_window_handle::HasDisplayHandle;
            let handle =
                self.window.as_ref().and_then(|w| w.display_handle().ok()).map(|h| h.as_raw());
            self.os_clipboard = Some(egui_winit::clipboard::Clipboard::new(handle));
        }
    }

    pub(crate) fn copy_selected(&mut self) {
        let nodes = self.subtree_docs(&self.selected_matter_duplicable());
        if !nodes.is_empty() {
            // Mirror onto the OS clipboard as tagged RON: paste then works in
            // ANOTHER scene, another editor window, even another project —
            // and you can read/share the copied nodes as plain text.
            if let Ok(ron) = ron::ser::to_string_pretty(&nodes, ron::ser::PrettyConfig::default())
            {
                self.ensure_os_clipboard();
                if let Some(c) = self.os_clipboard.as_mut() {
                    c.set_text(format!("{}\n{ron}", Self::NODE_CLIP_TAG));
                }
            }
            self.clipboard = nodes;
        }
    }

    /// Spawn the given nodes (roots offset slightly, subtrees intact) and select
    /// the new roots — used by paste/dup.
    pub(crate) fn spawn_offset(&mut self, mut nodes: Vec<NodeDoc>) {
        if nodes.is_empty() {
            return;
        }
        self.record();
        self.selection.clear();
        for node in nodes.iter_mut().filter(|n| n.parent.is_none()) {
            node.transform.translation[0] += 0.5;
            node.transform.translation[2] += 0.5;
        }
        let ents = self.spawn_docs(&nodes);
        self.selection.extend(
            ents.iter().zip(&nodes).filter(|(_, n)| n.parent.is_none()).map(|(&e, _)| e),
        );
    }

    pub(crate) fn paste(&mut self) {
        // Prefer the OS clipboard when it holds tagged Floptle nodes — that's
        // what makes copy → switch scene/instance/project → paste just work.
        // Anything else on the OS clipboard (plain text) is ignored and the
        // in-app clipboard is used.
        self.ensure_os_clipboard();
        let external = self
            .os_clipboard
            .as_mut()
            .and_then(|c| c.get())
            .and_then(|t| {
                t.strip_prefix(Self::NODE_CLIP_TAG)
                    .map(|rest| ron::from_str::<Vec<NodeDoc>>(rest.trim_start()))
            })
            .and_then(|r| r.ok());
        let nodes = external.unwrap_or_else(|| self.clipboard.clone());
        self.spawn_offset(nodes);
    }

    pub(crate) fn duplicate_selected(&mut self) {
        let nodes = self.subtree_docs(&self.selected_matter_duplicable());
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
        // Moving a node in the hierarchy detaches it from any bone (else BoneAttach's
        // target would diverge from the new Parent and resolve the wrong mesh).
        self.world.remove::<floptle_core::BoneAttach>(child);
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

#[cfg(test)]
mod subtree_tests {
    use floptle_core::math::DVec3;
    use super::*;

    fn node(ed: &mut Editor, name: &str, at: DVec3, parent: Option<Entity>) -> Entity {
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::from_translation(at));
        ed.world.insert(e, Name(name.into()));
        ed.world.insert(e, Matter::Empty);
        if let Some(p) = parent {
            ed.world.insert(e, floptle_core::Parent(p));
        }
        e
    }

    /// The clipboard/duplicate/prefab capture format: a parent → child →
    /// grandchild chain round-trips through `subtree_docs` → `spawn_docs` with
    /// hierarchy, local transforms, and per-node components intact; selecting
    /// a parent AND its child captures the child once; and deleting the parent
    /// removes the WHOLE subtree (no orphaned roots).
    #[test]
    fn subtrees_round_trip_and_delete_removes_children() {
        let mut ed = Editor::default();
        let parent = node(&mut ed, "Rig", DVec3::new(5.0, 0.0, 0.0), None);
        let child = node(&mut ed, "Arm", DVec3::new(1.0, 0.0, 0.0), Some(parent));
        let grand = node(&mut ed, "Hand", DVec3::new(0.5, 0.0, 0.0), Some(child));
        ed.world.insert(child, floptle_core::CastShadow(false));
        ed.world.insert(grand, floptle_core::Tags(vec!["grip".into()]));

        let docs = ed.subtree_docs(&[parent]);
        assert_eq!(docs.len(), 3, "the whole chain is captured");
        assert_eq!(docs[0].parent, None);
        assert_eq!(docs[1].parent, Some(0));
        assert_eq!(docs[2].parent, Some(1));
        assert_eq!(docs[1].transform.translation, [1.0, 0.0, 0.0], "children stay local");
        assert!(!docs[1].cast_shadow);
        assert_eq!(docs[2].tags, vec!["grip".to_string()]);
        // A redundant child in the root set must not duplicate its subtree.
        assert_eq!(ed.subtree_docs(&[parent, child]).len(), 3);

        let ents = ed.spawn_docs(&docs);
        assert_eq!(ents.len(), 3);
        assert_eq!(
            ed.world.get::<floptle_core::Parent>(ents[1]).map(|p| p.0),
            Some(ents[0]),
            "hierarchy re-wires to the NEW entities"
        );
        assert_eq!(ed.world.get::<floptle_core::Parent>(ents[2]).map(|p| p.0), Some(ents[1]));
        assert_eq!(
            ed.world.get::<floptle_core::CastShadow>(ents[1]).map(|c| c.0),
            Some(false),
            "the shadow opt-out survives the round trip"
        );

        // Deleting the original parent takes its children with it…
        ed.selection = vec![parent];
        ed.delete_selected();
        for e in [parent, child, grand] {
            assert!(!ed.world.is_alive(e), "subtree fully deleted");
        }
        // …while the spawned copies are untouched.
        for e in &ents {
            assert!(ed.world.is_alive(*e));
        }
    }
}
