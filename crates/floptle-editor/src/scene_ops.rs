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
#[cfg(feature = "editor-ui")]
use crate::inspector::{ComponentClip};
use crate::matter_catalog::{matter_doc_name};
use crate::{Editor, snap_dvec3};

impl Editor {
    /// Paste the component clipboard onto `e` (the held clip decides the kind). Adds
    /// the component if missing, else overwrites its values; scripts add-or-update by
    /// name. Pasting a "type" (Matter) never morphs a Terrain node (its field is
    /// out-of-ECS).
    #[cfg(feature = "editor-ui")]
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
        let object_materials = self
            .world
            .get::<floptle_core::ObjectMaterials>(e)
            .map(|om| {
                om.0.iter().map(|(k, m)| (k.clone(), MaterialDoc::from_material(m))).collect()
            })
            .unwrap_or_default();
        let rigidbody =
            self.world.get::<floptle_core::RigidBody>(e).map(floptle_scene::RigidBodyDoc::from_rigidbody);
        let celestial = self
            .world
            .get::<floptle_core::CelestialBody>(e)
            .map(floptle_scene::CelestialBodyDoc::from_body);
        let disabled = self.world.get::<floptle_core::Disabled>(e).is_some();
        let mesh_collider = self.world.get::<floptle_core::MeshCollider>(e).is_some();
        // Carry the paint KEY, not a copy of the colors: the pasted node points at the
        // same block and forks it only if painted (copy-on-write, proposal §9.0). So
        // duplicating a painted prop is free, and painting the copy doesn't touch the
        // original.
        let paint = self.world.get::<floptle_core::VertexPaint>(e).map(|p| p.id);
        let tex_paint = self.world.get::<floptle_core::TexturePaint>(e).map(|p| p.id);
        // The tint travels with the node — duplicating a ghosted preview or a
        // team-coloured prop copies what makes it look that way.
        let tint = self
            .world
            .get::<floptle_core::Tint>(e)
            .filter(|t| !t.is_identity())
            .map(|t| [t.color[0], t.color[1], t.color[2], t.alpha]);
        let collidable = self.world.get::<floptle_core::Collidable>(e).is_some();
        let trigger = self.world.get::<floptle_core::Trigger>(e).is_some();
        let nav_exclude = self.world.get::<floptle_core::NavMeshExclude>(e).is_some();
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
        let terrain_gen = self.world.get::<floptle_core::TerrainGen>(e).map(|g| g.0.clone());
        // Copy/duplicate carries the sorting layer: a duplicated sprite that
        // silently returned to Default would draw behind the thing it was copied
        // from, which reads as the copy having failed.
        let sorting = self
            .world
            .get::<floptle_core::Sorting>(e)
            .map(|s| (s.layer.clone(), s.order));
        // …and its sort MODE with it. A duplicated Y-sorted character that came
        // back on plain `order` would draw at a fixed depth while its original
        // kept following the player around, which reads as the copy being
        // broken rather than as a setting having been dropped.
        let sort_mode = self
            .world
            .get::<floptle_core::Sorting>(e)
            .and_then(|s| s.mode.as_str())
            .map(str::to_string);
        // …and its parallax factor, for the same reason again: a duplicated
        // background layer that came back moving with the world would sit on top
        // of the one it was copied from and drift away from it.
        let parallax = self
            .world
            .get::<floptle_core::Parallax>(e)
            .filter(|p| !p.is_identity())
            .map(|p| (p.factor[0], p.factor[1]));
        // …and so does its 2D lighting, for the same reason: a duplicated torch
        // that forgot which layers it lit would light the whole scene.
        let lit = self.world.get::<floptle_core::Lighting2D>(e);
        let lit_2d = lit.map(|l| l.mode.name().to_string());
        let light_layers = lit.map(|l| l.layers.clone()).unwrap_or_default();
        // …including its shaping. A duplicated torch that came back with the
        // default falloff would be a different torch (`floptle/0126`).
        let light_inner = lit.map(|l| l.inner);
        let light_falloff = lit.map(|l| l.falloff);
        let light_shadows = lit.map(|l| l.shadows);
        let shadow_2d =
            self.world.get::<floptle_core::Shadow2D>(e).map(|s| s.0.name().to_string());
        Some(NodeDoc {
            id: None,
            parent_id: None,
            // …and how it follows, so a duplicated 2D camera keeps its target,
            // its dead zone and its limits rather than becoming a static one.
            camera_2d: self
                .world
                .get::<floptle_core::camera2d::Camera2D>(e)
                .map(floptle_scene::Camera2DDoc::from),
            terrain_gen,
            name,
            transform,
            matter: MatterDoc::from(matter),
            scripts,
            material,
            object_materials,
            tint,
            rigidbody,
            celestial,
            disabled,
            mesh_collider,
            paint,
            tex_paint,
            collidable,
            trigger,
            nav_exclude,
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
            sorting,
            sort_mode,
            parallax,
            lit_2d,
            light_layers,
            shadow_2d,
            light_inner,
            light_falloff,
            light_shadows,
        })
    }

    pub(crate) fn spawn_node(&mut self, node: &NodeDoc) -> Entity {
        let e = self.world.spawn();
        self.insert_doc(e, node);
        e
    }

    /// Put a node document onto an entity.
    ///
    /// Split out of [`Self::spawn_node`] so that writing a document to a node
    /// that already exists — which is what `scene.set` from a package does —
    /// goes through the SAME code that loading a scene does, rather than a
    /// second copy of it that agrees today ([[two-gathers-must-agree]]).
    ///
    /// **Adds only.** A field the document leaves out is left alone, not
    /// removed, which is right for a fresh entity and wrong for a reused one —
    /// so the reuse path clears first. See `Editor::clear_doc_components`.
    pub(crate) fn insert_doc(&mut self, e: Entity, node: &NodeDoc) {
        self.world.insert(e, node.transform.to_transform());
        self.world.insert(e, Name(node.name.clone()));
        self.world.insert(e, node.matter.to_matter());
        // Inline map geometry (prefab instance / paste / duplicate): give this
        // node its OWN id in this scene's store. Without it the doc's id would
        // key into whatever that id happens to mean here — an empty node in a
        // fresh scene, or somebody else's wall in a busy one.
        if let MatterDoc::MapMesh { geo: Some(geo), .. } = &node.matter {
            let id = self.next_map_id();
            self.maps.meshes.insert(id, geo.clone());
            self.maps.dirty.insert(id);
            self.world.insert(e, floptle_core::Matter::MapMesh { id });
        }
        // The same rule for the OTHER id-bearing nodes: an id is identity, not
        // data to copy. A Nav Link's id is how a script names it and how a bake
        // matches routes back; a Nav Mesh's id keys its bake file. A copy
        // arriving with a taken id (duplicate, paste, prefab instance) — or
        // with the hand-written default 0 — mints its own. (`cmd.add` already
        // mints fresh ids for fresh nodes; this closes the copy paths.)
        self.rekey_matter_id(e);
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
        if !node.object_materials.is_empty() {
            self.world.insert(
                e,
                floptle_core::ObjectMaterials(
                    node.object_materials
                        .iter()
                        .map(|(k, m)| (k.clone(), m.to_material()))
                        .collect(),
                ),
            );
        }
        if let Some(t) = node.tint {
            self.world
                .insert(e, floptle_core::Tint { color: [t[0], t[1], t[2]], alpha: t[3] });
        }
        if let Some(rb) = &node.rigidbody {
            self.world.insert(e, rb.to_rigidbody());
        }
        if node.mesh_collider {
            self.world.insert(e, floptle_core::MeshCollider);
        }
        if node.disabled {
            self.world.insert(e, floptle_core::Disabled);
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
        // **The 2D half of a node document.** These were collected into the doc
        // and never written back, so duplicate, paste and prefab-instance all
        // dropped them — a copied background lost its parallax and sat on top of
        // the one it was copied from, and a copied 2D camera stopped following.
        // `clear_doc_components` above already removes them, so a package doing
        // a read-modify-write on any node was destroying them outright.
        if let Some(c) = node.camera_2d.as_ref() {
            self.world.insert(e, floptle_core::camera2d::Camera2D::from(c));
        }
        if let Some((x, y)) = node.parallax {
            let p = floptle_core::Parallax { factor: [x, y] };
            if !p.is_identity() {
                self.world.insert(e, p);
            }
        }
        let mode = node.sort_mode.as_deref().map(floptle_core::SortMode::parse).unwrap_or_default();
        if node.sorting.is_some() || mode != floptle_core::SortMode::default() {
            let (layer, order) = node.sorting.clone().unwrap_or_default();
            self.world.insert(e, floptle_core::Sorting { layer, order, mode });
        }
        if let Some(c) = node.shadow_2d.as_deref().and_then(floptle_core::Cast2D::parse) {
            self.world.insert(e, floptle_core::Shadow2D(c));
        }
        if node.lit_2d.is_some()
            || !node.light_layers.is_empty()
            || node.light_inner.is_some()
            || node.light_falloff.is_some()
            || node.light_shadows.is_some()
        {
            let d = floptle_core::Lighting2D::default();
            self.world.insert(
                e,
                floptle_core::Lighting2D {
                    mode: node
                        .lit_2d
                        .as_deref()
                        .and_then(floptle_core::Lit2D::parse)
                        .unwrap_or(d.mode),
                    layers: node.light_layers.clone(),
                    inner: node.light_inner.unwrap_or(d.inner),
                    falloff: node.light_falloff.unwrap_or(d.falloff),
                    shadows: node.light_shadows.unwrap_or(d.shadows),
                },
            );
        }
    }

    /// Take off everything a node document can put on, so that writing a
    /// document to a live node LEAVES it saying what the document says.
    ///
    /// The exhaustive `let NodeDoc { .. }` below is the point of this function:
    /// a field added to `NodeDoc` stops this compiling, and whoever adds it has
    /// to decide how a package clears it. A silent no-op here would mean a tool
    /// that removes a rigidbody appears to work and does not, which is the
    /// failure this whole API exists to stop being possible.
    pub(crate) fn clear_doc_components(&mut self, e: Entity, doc: &NodeDoc) {
        // Named, not `..`, so this is a compile error when NodeDoc grows.
        #[allow(unused_variables)]
        let NodeDoc {
            name,
            transform,
            matter,
            scripts,
            material,
            object_materials,
            tint,
            rigidbody,
            celestial,
            mesh_collider,
            disabled,
            paint,
            tex_paint,
            terrain_gen,
            collidable,
            trigger,
            nav_exclude,
            visible,
            cast_shadow,
            anim_controller,
            particles,
            id,
            parent_id,
            parent,
            attachment,
            net,
            ui_layer,
            ui,
            audio,
            layer,
            tags,
            sorting,
            sort_mode,
            parallax,
            lit_2d,
            light_layers,
            shadow_2d,
            light_inner,
            light_falloff,
            light_shadows,
            camera_2d,
        } = doc;

        // `name`, `transform` and `matter` are always written by `insert_doc`,
        // so they need no clearing — a node always has all three.
        self.world.remove::<Scripts>(e);
        self.world.remove::<Material>(e);
        self.world.remove::<floptle_core::ObjectMaterials>(e);
        self.world.remove::<floptle_core::Tint>(e);
        self.world.remove::<floptle_core::RigidBody>(e);
        self.world.remove::<floptle_core::CelestialBody>(e);
        self.world.remove::<floptle_core::MeshCollider>(e);
        self.world.remove::<floptle_core::Disabled>(e);
        self.world.remove::<floptle_core::Collidable>(e);
        self.world.remove::<floptle_core::Trigger>(e);
        self.world.remove::<floptle_core::NavMeshExclude>(e);
        self.world.remove::<floptle_core::Visible>(e);
        self.world.remove::<floptle_core::CastShadow>(e);
        self.world.remove::<floptle_core::AnimController>(e);
        self.world.remove::<floptle_core::ParticleSystem>(e);
        self.world.remove::<floptle_core::Layer>(e);
        self.world.remove::<floptle_core::Tags>(e);
        self.world.remove::<floptle_core::camera2d::Camera2D>(e);
        self.world.remove::<floptle_core::Sorting>(e);
        self.world.remove::<floptle_core::Parallax>(e);
        self.world.remove::<floptle_core::Lighting2D>(e);
        self.world.remove::<floptle_core::Shadow2D>(e);
        // `paint`, `tex_paint` and `terrain_gen` are KEYS into per-scene stores
        // and not components; `id`, `parent_id`, `parent` and `attachment` are
        // the scene file's linkage, owned by save/load. None of them is a
        // package's to clear, and `insert_doc` does not write them either.
    }

    /// Give `e` a fresh id if its Matter carries one that is 0 or already taken
    /// by another node — see the call in [`Self::spawn_node`].
    fn rekey_matter_id(&mut self, e: Entity) {
        enum Kind {
            Link,
            Mesh,
        }
        let (kind, id) = match self.world.get::<Matter>(e) {
            Some(Matter::NavLink { id, .. }) => (Kind::Link, *id),
            Some(Matter::NavMesh { id, .. }) => (Kind::Mesh, *id),
            _ => return,
        };
        let same_kind = |m: &Matter| -> Option<u32> {
            match (m, &kind) {
                (Matter::NavLink { id, .. }, Kind::Link) => Some(*id),
                (Matter::NavMesh { id, .. }, Kind::Mesh) => Some(*id),
                _ => None,
            }
        };
        let taken = id == 0
            || self
                .world
                .query::<Matter>()
                .any(|(o, m)| o != e && same_kind(m) == Some(id));
        if !taken {
            return;
        }
        let next = self
            .world
            .query::<Matter>()
            .filter(|(o, _)| *o != e)
            .filter_map(|(_, m)| same_kind(m))
            .max()
            .map_or(1, |n| n + 1);
        match self.world.get_mut::<Matter>(e) {
            Some(Matter::NavLink { id, .. }) => *id = next,
            Some(Matter::NavMesh { id, .. }) => *id = next,
            _ => {}
        }
    }

    /// Spawn a new node ~5 units in front of the camera, and select it.
    pub(crate) fn add_node(&mut self, name: &str, matter: MatterDoc) {
        self.add_node_at(name, matter, None);
    }

    /// `add_node`, but at an explicit world transform when the caller has one
    /// (the Map tool draws a shape where the cursor put it, not 5 units in
    /// front of the camera).
    pub(crate) fn add_node_at(
        &mut self,
        name: &str,
        matter: MatterDoc,
        at: Option<floptle_core::Transform>,
    ) {
        self.record();
        let cam = self.camera.render_camera();
        let mut pos = cam.world_position + (cam.rotation * Vec3::NEG_Z * 5.0).as_dvec3();
        if self.grid.snap {
            pos = snap_dvec3(pos, self.grid.size as f64);
        }
        let transform = match at {
            Some(t) => TransformDoc {
                translation: [t.translation.x, t.translation.y, t.translation.z],
                rotation: t.rotation.to_array(),
                scale: t.scale.to_array(),
            },
            None => TransformDoc { translation: [pos.x, pos.y, pos.z], ..Default::default() },
        };
        let node = NodeDoc {
            id: None,
            parent_id: None,
            camera_2d: None,
            terrain_gen: None,
            name: name.into(),
            transform,
            matter,
            scripts: Vec::new(),
            material: None,
            object_materials: Default::default(),
            tint: None,
            rigidbody: None,
            celestial: None,
            disabled: false,
            mesh_collider: false,
            paint: None,
            tex_paint: None,
            collidable: false,
            trigger: false,
            nav_exclude: false,
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
            sorting: None,
            sort_mode: None,
            parallax: None,
            lit_2d: None,
            light_layers: Vec::new(),
            shadow_2d: None,
            light_inner: None,
            light_falloff: None,
            light_shadows: None,
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
        } else if crate::assets::is_map_sidecar(path) {
            let at = self.cursor_world();
            self.import_map_file(path, Some(at));
        // **A texture becomes a Sprite.** Dropping a model makes a Mesh node, so
        // dropping a sprite doing nothing at all — no node, no toast, no Console
        // line — is the least expected outcome available, and in a 2D project it
        // is the very first thing anybody tries.
        } else if crate::assets::is_texture(path) {
            self.record();
            let pos = self.cursor_world();
            let rel = crate::assets::asset_rel_path(path, &self.project_root);
            let name = std::path::Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Sprite".into());
            self.add_node_at(
                &name,
                MatterDoc::Sprite {
                    ppu: 32.0,
                    size: 1.0,
                    cell: 0,
                    flip_x: false,
                    flip_y: false,
                    pivot: [0.5, 0.5],
                },
                Some(floptle_core::Transform {
                    translation: pos,
                    ..Default::default()
                }),
            );
            // …wearing the texture that was dropped, which is the whole point.
            if let Some(&e) = self.selection.first() {
                let mut m = floptle_core::Material::default();
                let (c, r) = crate::assets::tex_setting(
                    &self.texture_settings,
                    &self.project_root,
                    &rel,
                )
                .sheet();
                m.texture = Some(rel);
                m.sheet_cols = c;
                m.sheet_rows = r;
                self.world.insert(e, m);
            }
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
                id: None,
                parent_id: None,
                camera_2d: None,
                terrain_gen: None,
                name,
                transform: TransformDoc {
                    translation: [pos.x, pos.y, pos.z],
                    ..Default::default()
                },
                matter: MatterDoc::Mesh { asset_path: path.to_string() },
                scripts: Vec::new(),
                material: None,
                object_materials: Default::default(),
                tint: None,
                rigidbody: None,
                celestial: None,
                disabled: false,
                mesh_collider: false,
                paint: None,
                tex_paint: None,
                collidable: false,
                trigger: false,
            nav_exclude: false,
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
                sorting: None,
                sort_mode: None,
                parallax: None,
                lit_2d: None,
                light_layers: Vec::new(),
                shadow_2d: None,
                light_inner: None,
                light_falloff: None,
                light_shadows: None,
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
            // A map mesh's geometry lives in the per-scene sidecar, which a
            // prefab or a clipboard payload can't reach — carry it inline so
            // pasting into another scene/project rebuilds the real shape
            // instead of the placeholder box.
            if let MatterDoc::MapMesh { id, geo } = &mut doc.matter {
                *geo = self.maps.meshes.get(id).cloned();
            }
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
    /// Put `text` on the system clipboard.
    ///
    /// **One call, two backings, because a build must still copy.** The editor
    /// goes through egui-winit's clipboard, which already handles the Wayland
    /// and X11 cases the whole application depends on. A build has no
    /// egui-winit, and the alternative was to do nothing — which for Cut is not
    /// "the feature is missing", it is deletion with nowhere to paste from. So
    /// the player keeps its own `arboard` clipboard (the same crate egui-winit
    /// wraps) alive for the session: on X11 the contents belong to a live
    /// owner, so it cannot be opened and dropped per copy.
    pub(crate) fn os_clipboard_set(&mut self, text: String) {
        #[cfg(feature = "editor-ui")]
        {
            self.ensure_os_clipboard();
            if let Some(c) = self.os_clipboard.as_mut() {
                c.set_text(text);
            }
        }
        #[cfg(all(not(feature = "editor-ui"), not(target_arch = "wasm32")))]
        {
            if self.player_clipboard.is_none() {
                match arboard::Clipboard::new() {
                    Ok(c) => self.player_clipboard = Some(c),
                    Err(e) => {
                        log::warn!("no system clipboard available: {e}");
                        return;
                    }
                }
            }
            if let Some(c) = self.player_clipboard.as_mut()
                && let Err(e) = c.set_text(text)
            {
                log::warn!("could not put the selection on the clipboard: {e}");
            }
        }
        // A browser's clipboard is an async, permissioned API reached through
        // the page, not a library call. Not wired yet — and said, rather than
        // dropped, because Cut has already removed the text by the time we get
        // here.
        #[cfg(all(not(feature = "editor-ui"), target_arch = "wasm32"))]
        {
            let _ = text;
            log::warn!(
                "the system clipboard is not wired up in a browser build yet, so this copy \
                 went nowhere"
            );
        }
    }

    #[cfg(feature = "editor-ui")]
    pub(crate) fn ensure_os_clipboard(&mut self) {
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
            #[cfg(feature = "editor-ui")]
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

    /// Nodes sitting on the OS clipboard, when it holds tagged Floptle RON.
    ///
    /// This is what makes copy → switch scene/instance/project → paste work.
    /// A build has no cross-application node clipboard (nothing in a game can
    /// copy a node), so there it is simply nothing.
    fn os_clipboard_nodes(&mut self) -> Option<Vec<NodeDoc>> {
        #[cfg(feature = "editor-ui")]
        {
            self.ensure_os_clipboard();
            self.os_clipboard
                .as_mut()
                .and_then(|c| c.get())
                .and_then(|t| {
                    t.strip_prefix(Self::NODE_CLIP_TAG)
                        .map(|rest| ron::from_str::<Vec<NodeDoc>>(rest.trim_start()))
                })
                .and_then(|r| r.ok())
        }
        #[cfg(not(feature = "editor-ui"))]
        None
    }

    pub(crate) fn paste(&mut self) {
        // Prefer the OS clipboard when it holds tagged Floptle nodes. Anything
        // else on it (plain text) is ignored and the in-app clipboard is used.
        let nodes = self.os_clipboard_nodes().unwrap_or_else(|| self.clipboard.clone());
        self.spawn_offset(nodes);
    }

    pub(crate) fn duplicate_selected(&mut self) {
        let nodes = self.subtree_docs(&self.selected_matter_duplicable());
        self.spawn_offset(nodes);
    }

    // ---- scene-graph (parenting) -------------------------------------------
    /// True if `e` is `ancestor` or one of its descendants (cycle guard).
    /// Everything under `roots`, deduped, excluding the roots themselves.
    ///
    /// Breadth-first off a single parent→children index, so this costs one pass
    /// over the scene rather than one per root — a level root with a few
    /// thousand nodes under it is the case that matters, and it is also the case
    /// where anybody actually asks the question.
    pub(crate) fn descendants_of(&self, roots: &[Entity]) -> Vec<Entity> {
        let mut kids: std::collections::HashMap<Entity, Vec<Entity>> =
            std::collections::HashMap::new();
        for (e, p) in self.world.query::<floptle_core::Parent>() {
            kids.entry(p.0).or_default().push(e);
        }
        let mut seen: std::collections::HashSet<Entity> = roots.iter().copied().collect();
        let mut out = Vec::new();
        let mut queue: std::collections::VecDeque<Entity> = roots.iter().copied().collect();
        while let Some(e) = queue.pop_front() {
            for &c in kids.get(&e).map(Vec::as_slice).unwrap_or(&[]) {
                // `seen` starts as the roots, so a child that is ALSO selected
                // is never reported as its own parent's extra — otherwise
                // selecting a parent and its child would offer to change the
                // child twice and count it as an unselected extra.
                if seen.insert(c) {
                    out.push(c);
                    queue.push_back(c);
                }
            }
        }
        out
    }

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

    /// Re-parent every node in `children` under `parent` (or make them roots if
    /// `None`) as ONE undo step, preserving each node's world placement. Filters
    /// out the target itself, cycles (can't parent under your own descendant),
    /// and any node whose ancestor is also moving (the ancestor's move carries it).
    pub(crate) fn reparent_many(&mut self, children: &[Entity], parent: Option<Entity>) {
        let moved: Vec<Entity> = children
            .iter()
            .copied()
            .filter(|&c| {
                parent != Some(c)
                    && !parent.is_some_and(|p| self.is_descendant(p, c))
                    && !children.iter().any(|&a| a != c && self.is_descendant(c, a))
            })
            .collect();
        if moved.is_empty() {
            return;
        }
        self.record();
        for child in moved {
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

// These exercise the AUTHORING half — the dock, the Inspector, the
// command line — so they compile only where that half does. Without the
// gate the player configuration cannot be linted or tested at all, which
// is how it went unlinted through a whole release.
#[cfg(feature = "editor-ui")]
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

    /// The whole route the user actually takes: the Inspector's pick becomes a
    /// command, a node with children raises the prompt instead of applying, and
    /// the answer applies the set the answer chose.
    ///
    /// Driven through `apply_frame_commands`, not by calling `apply_layer`
    /// directly — the wiring between the picker and the edit is where this
    /// feature can be broken while every piece of it still works.
    #[test]
    fn a_layer_pick_asks_about_children_and_then_applies_what_was_answered() {
        use floptle_core::{Layer, Matter, Parent, Shape, Transform};
        let mut ed = crate::Editor::default();
        let node = |ed: &mut crate::Editor| {
            let e = ed.world.spawn();
            ed.world.insert(e, Transform::IDENTITY);
            ed.world.insert(e, Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] });
            e
        };
        let root = node(&mut ed);
        let child = node(&mut ed);
        ed.world.insert(child, Parent(root));
        let lone = node(&mut ed);

        let pick = |ed: &mut crate::Editor, targets: Vec<floptle_core::Entity>| {
            let cmd = crate::EditorCmd {
                set_layer: Some(crate::SetLayer { targets, layer: "Enemy".into() }),
                ..Default::default()
            };
            ed.apply_frame_commands(cmd, false);
        };
        let layer = |ed: &crate::Editor, e| {
            ed.world.get::<Layer>(e).map(|l| l.0.clone())
        };

        // A node with no children applies straight away — nothing to ask.
        pick(&mut ed, vec![lone]);
        assert!(ed.layer_children_confirm.is_none(), "nothing to ask about");
        assert_eq!(layer(&ed, lone).as_deref(), Some("Enemy"));

        // A node WITH children asks, and changes nothing until it is answered.
        pick(&mut ed, vec![root]);
        let prompt = ed.layer_children_confirm.clone().expect("it has a child, so it asks");
        assert_eq!(prompt.children, vec![child]);
        assert_eq!(layer(&ed, root), None, "asking is not doing");
        assert_eq!(layer(&ed, child), None);

        // "Just this node".
        let cmd = crate::EditorCmd {
            do_set_layer: Some(crate::SetLayer {
                targets: prompt.targets.clone(),
                layer: prompt.layer.clone(),
            }),
            ..Default::default()
        };
        ed.apply_frame_commands(cmd, false);
        assert_eq!(layer(&ed, root).as_deref(), Some("Enemy"));
        assert_eq!(layer(&ed, child), None, "the child was explicitly left alone");

        // "Include the children".
        let mut all = prompt.targets.clone();
        all.extend_from_slice(&prompt.children);
        let cmd = crate::EditorCmd {
            do_set_layer: Some(crate::SetLayer { targets: all, layer: "Prop".into() }),
            ..Default::default()
        };
        ed.apply_frame_commands(cmd, false);
        assert_eq!(layer(&ed, root).as_deref(), Some("Prop"));
        assert_eq!(layer(&ed, child).as_deref(), Some("Prop"), "and now it comes along");
    }

    /// Setting a collision layer has to reach every SELECTED node.
    ///
    /// The Inspector says "an edit here applies to all of them" in the panel
    /// itself; the layer picker addressed one entity, so twenty crates and one
    /// pick moved exactly one crate and the banner said otherwise.
    #[test]
    fn a_layer_change_reaches_the_whole_selection() {
        use floptle_core::Layer;
        let mut ed = crate::Editor::default();
        let nodes: Vec<_> = (0..5).map(|_| ed.world.spawn()).collect();

        ed.apply_layer(&nodes, "Enemy");
        for &e in &nodes {
            assert_eq!(
                ed.world.get::<Layer>(e).map(|l| l.0.as_str()),
                Some("Enemy"),
                "every selected node moves, not just the last one picked"
            );
        }

        // …and back to Default REMOVES the component, rather than storing a
        // layer name that means "no layer".
        ed.apply_layer(&nodes, floptle_core::layers::DEFAULT_LAYER);
        for &e in &nodes {
            assert!(ed.world.get::<Layer>(e).is_none(), "Default is the absence of it");
        }
    }

    /// The whole set is ONE undo step, not one per node.
    ///
    /// Counted across the world rather than by entity handle: undo restores the
    /// scene by respawning it, so the handles afterwards are not the handles
    /// before, and asserting on them would be asserting about the snapshot
    /// mechanism instead of about the edit.
    #[test]
    fn re_layering_a_selection_undoes_in_one_step() {
        use floptle_core::{Layer, Transform};
        let mut ed = crate::Editor::default();
        let nodes: Vec<_> = (0..4)
            .map(|_| {
                let e = ed.world.spawn();
                ed.world.insert(e, Transform::IDENTITY);
                ed.world.insert(
                    e,
                    floptle_core::Matter::Primitive {
                        shape: floptle_core::Shape::Cube,
                        color: [1.0; 3],
                    },
                );
                ed.world.insert(e, Layer("Prop".into()));
                e
            })
            .collect();
        let on = |ed: &crate::Editor, name: &str| {
            ed.world.query::<Layer>().filter(|(_, l)| l.0 == name).count()
        };
        assert_eq!(on(&ed, "Prop"), 4);

        ed.apply_layer(&nodes, "Enemy");
        assert_eq!(on(&ed, "Enemy"), 4, "all four moved");
        assert_eq!(on(&ed, "Prop"), 0);

        ed.undo();
        assert_eq!(on(&ed, "Prop"), 4, "one Ctrl+Z puts the whole change back");
        assert_eq!(on(&ed, "Enemy"), 0, "and leaves none of it behind");
    }

    /// The children the prompt offers are the real subtree — every level of it,
    /// not just the direct children, and never a node that is already selected.
    #[test]
    fn the_children_offered_are_the_whole_subtree_and_never_the_selection_itself() {
        use floptle_core::Parent;
        let mut ed = crate::Editor::default();
        let root = ed.world.spawn();
        let child = ed.world.spawn();
        let grandchild = ed.world.spawn();
        let stranger = ed.world.spawn();
        ed.world.insert(child, Parent(root));
        ed.world.insert(grandchild, Parent(child));

        let kids = ed.descendants_of(&[root]);
        assert_eq!(kids.len(), 2, "child AND grandchild: {kids:?}");
        assert!(kids.contains(&child) && kids.contains(&grandchild));
        assert!(!kids.contains(&root), "a root is not its own child");
        assert!(!kids.contains(&stranger));

        // Selecting a parent AND its child must not report the child as an
        // extra the user has not already chosen — otherwise the prompt offers
        // to change nodes that are in the selection anyway, and the count lies.
        let kids = ed.descendants_of(&[root, child]);
        assert_eq!(kids, vec![grandchild], "only the genuinely unselected: {kids:?}");

        // A childless node asks nothing at all.
        assert!(ed.descendants_of(&[stranger]).is_empty());
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
