//! Map-building suite (docs/map-tools-proposal.md): the editor-side store for
//! editable map meshes and their GPU/registry sync.
//!
//! A `Matter::MapMesh { id }` node carries only its stable id; the geometry
//! lives here in [`MapStore`] (ids survive undo's world respawn, exactly like
//! terrain/vpaint) and persists to `<project>/maps/<scene>.map.ron`. Rendering
//! rides the normal mesh path: each map mesh registers a `MeshAsset` under the
//! synthetic key `@map/<id>` whose parts are DYNAMIC meshes (the terrain
//! slots — edits are cheap `replace_dynamic` uploads, slots recycle), one part
//! per material slot with faces, so per-face materials resolve through the
//! existing `ObjectMaterials`/`part_look` machinery keyed by slot name.

use crate::{Editor, MeshAsset, PartMeta};
use floptle_map::MapMesh;
use floptle_render::{MeshData, MeshId, Vertex};
use floptle_scene::MatterDoc;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// The synthetic `mesh_registry` key a map mesh renders under.
pub(crate) fn map_key(id: u32) -> String {
    format!("@map/{id}")
}

/// The map id behind a `@map/<id>` key — i.e. "is this mesh one the editor
/// itself edits?", which the paint machinery has to know because such geometry
/// changes under it.
pub(crate) fn map_key_id(key: &str) -> Option<u32> {
    key.strip_prefix("@map/")?.parse().ok()
}

/// The dev-grid texture every new blockout shape starts with, as the
/// project-relative ref stored on the node (see [`Editor::map_default_material`]).
/// A blockout you can read the scale of beats an untextured grey slab: the map
/// UVs are 1 unit = 1 tile, so this grid measures the level for you.
pub(crate) const MAP_DEFAULT_TEXTURE: &str = "textures/Tile4.png";

/// Where that texture ships in the engine's own asset folder — the source we
/// seed a project from when it hasn't got one yet.
const MAP_DEFAULT_TEXTURE_SHIPPED: &str = "assets/textures/Tile4.png";

/// Editor-side authority for map-mesh geometry.
#[derive(Default)]
pub(crate) struct MapStore {
    pub(crate) meshes: HashMap<u32, MapMesh>,
    /// Live dynamic GPU parts per id (parallel to the registry entry's parts,
    /// tagged with the slot index each part draws). This is the authority for
    /// FREEING: `mesh_registry` is cleared wholesale on scene switches, but
    /// dynamic slots must be returned to the raster free-lists explicitly.
    pub(crate) parts: HashMap<u32, Vec<(MeshId, u16)>>,
    /// Ids whose GPU parts need a rebuild (geometry or slots changed).
    pub(crate) dirty: BTreeSet<u32>,
    /// Ids rebuilt since paint last looked — the brush's CPU geometry is stale
    /// and any paint on them has to be re-attached. Drained by `sync_map_paint`.
    pub(crate) paint_stale: BTreeSet<u32>,
    /// What every render vertex/triangle of a PAINTED map mesh was at its last
    /// rebuild, so the paint can follow the surfaces that survived an edit.
    /// Only kept for nodes that actually carry paint — see `map_paint`.
    pub(crate) paint_ident: HashMap<u32, crate::map_paint::MapPaintIdent>,
    /// Set when the sidecar for this scene exists but could NOT be read/parsed.
    /// While it is set the store is NOT the authority: unknown ids keep their
    /// nodes empty instead of being healed into boxes, and `save_maps` refuses
    /// to write (a save would otherwise replace a whole level with 1x1 cubes —
    /// the failure mode that ate Ty's vertex paint in July). Cleared by a
    /// successful adopt.
    pub(crate) load_failed: bool,
}

/// The blockout shapes offered by the Add menu / Map tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MapShape {
    Box,
    Plane,
    Wedge,
    Cylinder,
    Sphere,
    Stairs,
    Arch,
}

/// The Map tool's knobs: shape resolution (shared by the draw tool and the
/// tab's spawn buttons, so what you tweak is what you get) and the default
/// distance each discrete op moves by.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MapOpts {
    pub(crate) sides: u32,
    pub(crate) rings: u32,
    pub(crate) steps: u32,
    pub(crate) arch_segments: u32,
    /// Arch opening WIDTH as a fraction of the shape's half-width, and its
    /// HEIGHT (jamb + arc) as a fraction of the shape's full height.
    pub(crate) arch_width: f32,
    pub(crate) arch_height: f32,
    /// How far E / the Extrude button pushes (grid size wins when snap is on).
    pub(crate) extrude: f32,
    pub(crate) inset: f32,
    /// Weld radius: verts closer than this to each other merge.
    pub(crate) weld: f32,
}

impl Default for MapOpts {
    fn default() -> Self {
        Self {
            sides: 16,
            rings: 8,
            steps: 8,
            arch_segments: 8,
            arch_width: 0.6,
            arch_height: 0.75,
            extrude: 1.0,
            inset: 0.25,
            weld: 0.05,
        }
    }
}

impl MapShape {
    pub(crate) const ALL: [MapShape; 7] = [
        MapShape::Box,
        MapShape::Plane,
        MapShape::Wedge,
        MapShape::Cylinder,
        MapShape::Sphere,
        MapShape::Stairs,
        MapShape::Arch,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            MapShape::Box => "Map Box",
            MapShape::Plane => "Map Plane",
            MapShape::Wedge => "Map Wedge",
            MapShape::Cylinder => "Map Cylinder",
            MapShape::Sphere => "Map Sphere",
            MapShape::Stairs => "Map Stairs",
            MapShape::Arch => "Map Arch",
        }
    }

    /// The command that arms this shape for drawing — the UI asks the keybind
    /// table for its chord, so every hint stays true after a rebind.
    pub(crate) fn cmd(self) -> crate::map_keys::MapCmd {
        use crate::map_keys::MapCmd as C;
        match self {
            MapShape::Box => C::DrawBox,
            MapShape::Plane => C::DrawPlane,
            MapShape::Wedge => C::DrawWedge,
            MapShape::Cylinder => C::DrawCylinder,
            MapShape::Sphere => C::DrawSphere,
            MapShape::Stairs => C::DrawStairs,
            MapShape::Arch => C::DrawArch,
        }
    }

    /// True when the shape is flat — the draw tool commits it on the base drag
    /// and never asks for a height.
    pub(crate) fn is_flat(self) -> bool {
        self == MapShape::Plane
    }

    pub(crate) fn kind(self) -> floptle_map::ShapeKind {
        use floptle_map::ShapeKind as K;
        match self {
            MapShape::Box => K::Box,
            MapShape::Plane => K::Plane,
            MapShape::Wedge => K::Wedge,
            MapShape::Cylinder => K::Cylinder,
            MapShape::Sphere => K::Sphere,
            MapShape::Stairs => K::Stairs,
            MapShape::Arch => K::Arch,
        }
    }

    pub(crate) fn of_kind(kind: floptle_map::ShapeKind) -> Self {
        use floptle_map::ShapeKind as K;
        match kind {
            K::Box => MapShape::Box,
            K::Plane => MapShape::Plane,
            K::Wedge => MapShape::Wedge,
            K::Cylinder => MapShape::Cylinder,
            K::Sphere => MapShape::Sphere,
            K::Stairs => MapShape::Stairs,
            K::Arch => MapShape::Arch,
        }
    }

    /// The resolution knob this shape has, as a label — `[` / `]` adjust it.
    pub(crate) fn detail(self, opts: MapOpts) -> Option<String> {
        match self {
            MapShape::Stairs => Some(format!("{} steps  [ ]", opts.steps)),
            MapShape::Cylinder => Some(format!("{} sides  [ ]", opts.sides)),
            MapShape::Sphere => Some(format!("{} x {} segments  [ ]", opts.sides, opts.rings)),
            MapShape::Arch => Some(format!("{} arch segments  [ ]", opts.arch_segments)),
            MapShape::Box | MapShape::Plane | MapShape::Wedge => None,
        }
    }

    /// True for the shapes with a low end and a high end — the ones whose
    /// direction the editor must show and let you flip.
    pub(crate) fn rises(self) -> bool {
        self.kind().rises()
    }

    /// Blockout-scale defaults (bigger than the 0.7-half primitives — these
    /// are level pieces, not props).
    pub(crate) fn mesh(self, opts: MapOpts) -> MapMesh {
        self.sized(floptle_core::math::Vec3::ONE, opts)
    }

    /// The shape built to exact HALF-extents — the draw tool's output, and what
    /// the Map tab's spawn buttons use. The mesh comes back TAGGED with the
    /// spec, so its parameters stay editable until the geometry is touched.
    pub(crate) fn sized(self, half: floptle_core::math::Vec3, opts: MapOpts) -> MapMesh {
        self.spec(half, opts).build()
    }

    pub(crate) fn spec(
        self,
        half: floptle_core::math::Vec3,
        opts: MapOpts,
    ) -> floptle_map::ShapeSpec {
        floptle_map::ShapeSpec {
            kind: self.kind(),
            half,
            sides: opts.sides,
            rings: opts.rings,
            steps: opts.steps,
            arch_segments: opts.arch_segments,
            arch_width: opts.arch_width,
            arch_height: opts.arch_height,
        }
    }
}

/// One material slot's triangulation as uploadable geometry.
pub(crate) fn slot_mesh_data(sm: &floptle_map::SlotMesh) -> MeshData {
    MeshData {
        vertices: (0..sm.positions.len())
            .map(|i| Vertex { pos: sm.positions[i], normal: sm.normals[i], uv: sm.uvs[i] })
            .collect(),
        indices: sm.indices.clone(),
        colors: None,
    }
}

impl Editor {
    pub(crate) fn maps_file_path(&self) -> PathBuf {
        self.project_root.join("maps").join(format!("{}.map.ron", self.scene_name))
    }

    /// A fresh stable map-mesh id: one past the max over the store AND live
    /// components (orphans included — never re-issue a key still on disk).
    pub(crate) fn next_map_id(&self) -> u32 {
        let live = self
            .world
            .query::<floptle_core::Matter>()
            .filter_map(|(_, m)| match m {
                floptle_core::Matter::MapMesh { id } => Some(*id),
                _ => None,
            })
            .max();
        self.maps.meshes.keys().copied().max().max(live).map_or(0, |m| m + 1)
    }

    /// Spawn a new map-mesh node in front of the camera with `shape`'s geometry.
    pub(crate) fn add_map_shape(&mut self, shape: MapShape) {
        let mesh = shape.mesh(self.map_opts);
        self.spawn_map_node(shape.label(), mesh, None);
    }

    /// Spawn a map-mesh node carrying `mesh`, optionally at an explicit world
    /// transform (the draw tool's commit); `None` = in front of the camera.
    /// Returns the new node.
    pub(crate) fn spawn_map_node(
        &mut self,
        name: &str,
        mesh: MapMesh,
        at: Option<floptle_core::Transform>,
    ) -> Option<floptle_core::Entity> {
        let id = self.next_map_id();
        self.maps.meshes.insert(id, mesh);
        self.maps.dirty.insert(id);
        self.add_node_at(name, MatterDoc::MapMesh { id, geo: None }, at);
        let e = self.primary();
        // Blockout geometry is WORLD geometry: a wall you can walk through is
        // never what you meant. `Collidable` bakes the exact triangulation into
        // the static trimesh on Play; untick it in the Inspector for decoration.
        if let Some(e) = e {
            self.world.insert(e, floptle_core::Collidable);
            if let Some(mat) = self.map_default_material() {
                self.world.insert(e, mat);
            }
        }
        e
    }

    /// The material a new blockout shape starts with: the dev grid texture, at
    /// the mesh's own 1-unit-per-tile UVs. `None` when this project has no copy
    /// of the texture and we couldn't seed one — a shape with no Material draws
    /// flat grey, which is far better than one carrying a dangling reference.
    ///
    /// It is a node-level `Material`, so it covers every slot AND stays out of
    /// the way: a per-slot override (Map tab → "New material for selected
    /// faces") still wins for its own faces.
    fn map_default_material(&mut self) -> Option<floptle_core::Material> {
        self.ensure_map_default_texture()?;
        Some(floptle_core::Material {
            texture: Some(MAP_DEFAULT_TEXTURE.to_string()),
            ..Default::default()
        })
    }

    /// Make sure `<project>/textures/Tile4.png` exists, copying the engine's
    /// shipped copy in when it doesn't. Asset refs are project-relative, so a
    /// default texture has to actually live in the project it is referenced
    /// from; this is the one place that is true of.
    fn ensure_map_default_texture(&mut self) -> Option<()> {
        // No project (or a headless test Editor) — never touch the filesystem.
        if self.project_root.as_os_str().is_empty() || !self.project_root.is_dir() {
            return None;
        }
        if self.resolve_asset_path(MAP_DEFAULT_TEXTURE).is_file() {
            return Some(());
        }
        // Seed from the engine checkout this editor was built from, or from a
        // packaged bundle's assets beside the executable.
        let shipped = crate::export::repo_root()
            .map(|r| r.join(MAP_DEFAULT_TEXTURE_SHIPPED))
            .filter(|p| p.is_file())
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|e| Some(e.parent()?.join(MAP_DEFAULT_TEXTURE_SHIPPED)))
                    .filter(|p| p.is_file())
            })?;
        let dest = self.project_root.join(MAP_DEFAULT_TEXTURE);
        std::fs::create_dir_all(dest.parent()?).ok()?;
        std::fs::copy(&shipped, &dest).ok()?;
        self.asset_tree = crate::assets::build_assets(&self.project_root);
        self.map_note(
            floptle_script::LogLevel::Debug,
            format!("added the blockout grid texture to this project ({MAP_DEFAULT_TEXTURE})"),
        );
        Some(())
    }

    /// Per-frame sync (called before the render gather): self-heal duplicated
    /// ids (duplicate/paste copies the component verbatim — the LATER node
    /// gets a fresh id + its own copy of the geometry, so map meshes edit
    /// independently), materialize store entries for unknown ids (cross-scene
    /// paste), and rebuild dirty geometry into the registry's dynamic parts.
    pub(crate) fn sync_map_meshes(&mut self) {
        // Self-heal pass over the live components (cheap: map nodes are few).
        let live: Vec<(floptle_core::Entity, u32)> = self
            .world
            .query::<floptle_core::Matter>()
            .filter_map(|(e, m)| match m {
                floptle_core::Matter::MapMesh { id } => Some((e, *id)),
                _ => None,
            })
            .collect();
        let mut seen = BTreeSet::new();
        for (e, id) in live {
            if seen.insert(id) {
                if !self.maps.meshes.contains_key(&id) && !self.maps.load_failed {
                    // An id with no geometry and a healthy sidecar means the node
                    // arrived from somewhere that couldn't carry its mesh (an old
                    // hand-written .ron). Give it a box so it is visible and
                    // editable rather than an invisible nothing.
                    //
                    // When the sidecar FAILED to load we do the opposite: leave it
                    // empty and let `save_maps` refuse to write, so a transient IO
                    // error can't turn a level into cubes and then persist them.
                    let seed = MapShape::Box.mesh(self.map_opts);
                    self.maps.meshes.insert(id, seed);
                    self.maps.dirty.insert(id);
                }
                continue;
            }
            let fresh = self.next_map_id();
            let Some(geo) = self.maps.meshes.get(&id).cloned() else { continue };
            self.maps.meshes.insert(fresh, geo);
            self.maps.dirty.insert(fresh);
            if let Some(m) = self.world.get_mut::<floptle_core::Matter>(e) {
                *m = floptle_core::Matter::MapMesh { id: fresh };
            }
            seen.insert(fresh);
        }
        if self.maps.dirty.is_empty() {
            return;
        }
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_mut()) else {
            return; // keep dirty — rebuild once the GPU exists
        };
        let ids: Vec<u32> = std::mem::take(&mut self.maps.dirty).into_iter().collect();
        // Whatever changed, the paint's view of this geometry is now stale.
        self.maps.paint_stale.extend(ids.iter().copied());
        for id in ids {
            let Some(mesh) = self.maps.meshes.get(&id) else { continue };
            let slots = floptle_map::triangulate(mesh);
            let mut old = self.maps.parts.remove(&id).unwrap_or_default();
            let mut parts: Vec<(MeshId, u16)> = Vec::with_capacity(slots.len());
            let mut part_meta = Vec::with_capacity(slots.len());
            for sm in &slots {
                let data = slot_mesh_data(sm);
                // Reuse a retiring slot when the geometry still fits, else
                // re-register at the new size (the terrain upload pattern).
                let mid = match old.pop() {
                    Some((mid, _)) if raster.replace_dynamic(gpu, mid, &data) => mid,
                    Some((mid, _)) => {
                        raster.free_dynamic(mid);
                        let fresh = raster.register_dynamic(
                            gpu,
                            data.vertices.len() as u32,
                            data.indices.len() as u32,
                            false,
                        );
                        raster.replace_dynamic(gpu, fresh, &data);
                        fresh
                    }
                    None => {
                        let fresh = raster.register_dynamic(
                            gpu,
                            data.vertices.len() as u32,
                            data.indices.len() as u32,
                            false,
                        );
                        raster.replace_dynamic(gpu, fresh, &data);
                        fresh
                    }
                };
                parts.push((mid, sm.slot));
                let name = mesh
                    .slots
                    .get(sm.slot as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("Slot {}", sm.slot));
                part_meta.push(PartMeta { material: name, base_color: [0.75, 0.75, 0.75], textured: false });
            }
            for (mid, _) in old {
                raster.free_dynamic(mid);
            }
            let size = mesh
                .bounds()
                .map(|(lo, hi)| (hi - lo).length())
                .unwrap_or(1.0)
                .max(0.1);
            self.mesh_registry.insert(
                map_key(id),
                MeshAsset {
                    parts: parts.iter().map(|&(m, _)| m).collect(),
                    part_meta,
                    tex_filter: None,
                    size,
                    rig: None,
                },
            );
            self.maps.parts.insert(id, parts);
        }
    }

    /// Write the whole store beside the scene. Deliberately never drops an
    /// entry: geometry whose node was deleted stays in the file (an undo can
    /// resurrect the node after a save; a few kilobytes of RON is nothing).
    pub(crate) fn save_maps(&mut self) {
        if self.maps.load_failed {
            // The store is not the authority for this scene — writing it would
            // replace geometry we never managed to read.
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "⬢ map geometry NOT saved — {} could not be read this session (fix or remove it, then reload the scene)",
                    self.maps_file_path().display()
                ),
                None,
            );
            return;
        }
        if self.maps.meshes.is_empty() {
            let _ = std::fs::remove_file(self.maps_file_path());
            return;
        }
        let ordered: BTreeMap<u32, &MapMesh> =
            self.maps.meshes.iter().map(|(&k, v)| (k, v)).collect();
        let pretty = ron::ser::PrettyConfig::new().depth_limit(3);
        match ron::ser::to_string_pretty(&ordered, pretty) {
            Ok(text) => {
                let dir = self.project_root.join("maps");
                let _ = std::fs::create_dir_all(&dir);
                if let Err(e) = std::fs::write(self.maps_file_path(), text) {
                    self.console.push(
                        floptle_script::LogLevel::Error,
                        format!("💾 save map meshes failed: {e}"),
                        None,
                    );
                }
            }
            Err(e) => self.console.push(
                floptle_script::LogLevel::Error,
                format!("💾 encode map meshes failed: {e}"),
                None,
            ),
        }
    }

    /// Reload the store for the current scene (any scene load — the same slot
    /// `adopt_terrain`/`adopt_paint` occupy). Frees the previous scene's
    /// dynamic parts first; every loaded mesh is marked dirty and re-uploads
    /// on the next `sync_map_meshes`.
    pub(crate) fn adopt_maps(&mut self) {
        if let Some(raster) = self.raster.as_mut() {
            for (_, parts) in self.maps.parts.drain() {
                for (mid, _) in parts {
                    raster.free_dynamic(mid);
                }
            }
        }
        self.maps.parts.clear();
        self.maps.meshes.clear();
        self.maps.dirty.clear();
        self.maps.paint_stale.clear();
        self.maps.paint_ident.clear();
        self.maps.load_failed = false;
        let path = self.maps_file_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            // No file is the normal case for a scene with no map meshes. A file
            // that exists but won't open is NOT — poison the store so the next
            // save can't overwrite it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) => {
                self.maps.load_failed = true;
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("⬢ map sidecar {} could not be read: {e} — map nodes will stay empty and saving map geometry is disabled until this is fixed", path.display()),
                    None,
                );
                return;
            }
        };
        match ron::from_str::<BTreeMap<u32, MapMesh>>(&text) {
            Ok(loaded) => {
                for (id, mesh) in loaded {
                    self.maps.dirty.insert(id);
                    self.maps.meshes.insert(id, mesh);
                }
            }
            Err(e) => {
                self.maps.load_failed = true;
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("⬢ map sidecar {} failed to parse: {e} — map nodes will stay empty and saving map geometry is disabled until this is fixed", path.display()),
                    None,
                );
            }
        }
    }

    /// Drop stored geometry no live node references (copy/paste and duplicate
    /// each mint a fresh id + a full copy, and deleted nodes deliberately leave
    /// theirs behind so an undo can resurrect them — over a long session that
    /// piles up). Returns how many entries went. Undo history that references
    /// them still restores VALUES, so this only ever costs disk, never edits.
    pub(crate) fn prune_map_orphans(&mut self) -> usize {
        let mut live: BTreeSet<u32> = self
            .world
            .query::<floptle_core::Matter>()
            .filter_map(|(_, m)| match m {
                floptle_core::Matter::MapMesh { id } => Some(*id),
                _ => None,
            })
            .collect();
        // Ids any undo/redo step could bring BACK count as live: a scene
        // snapshot carries the node but not its geometry, so pruning one out
        // from under the history would undo a deletion into a placeholder box.
        for snap in self.history.undo.iter().chain(self.history.redo.iter()) {
            match snap {
                crate::Snapshot::Scene(doc) => live.extend(doc.nodes.iter().filter_map(|n| {
                    match n.matter {
                        MatterDoc::MapMesh { id, .. } => Some(id),
                        _ => None,
                    }
                })),
                crate::Snapshot::MapMesh(id, _) => {
                    live.insert(*id);
                }
                _ => {}
            }
        }
        let drop: Vec<u32> =
            self.maps.meshes.keys().copied().filter(|id| !live.contains(id)).collect();
        if let Some(raster) = self.raster.as_mut() {
            for id in &drop {
                for (mid, _) in self.maps.parts.remove(id).unwrap_or_default() {
                    raster.free_dynamic(mid);
                }
                self.mesh_registry.remove(&map_key(*id));
            }
        }
        for id in &drop {
            self.maps.meshes.remove(id);
        }
        drop.len()
    }

    /// Undo/redo value swap through the store (the terrain/paint pattern —
    /// the ECS is untouched, so Entity churn can't orphan geometry).
    pub(crate) fn swap_map_mesh(&mut self, id: u32, mesh: &MapMesh) -> MapMesh {
        let old = self.maps.meshes.insert(id, mesh.clone()).unwrap_or_default();
        self.maps.dirty.insert(id);
        // The sub-object selection SURVIVES an undo (so extrude / undo / retry
        // works), but the restored mesh may have fewer verts/faces than the
        // selection remembers — drop whatever no longer exists.
        if let Some(sel) = self.map_sel.as_mut().filter(|s| s.id == id) {
            sel.prune(mesh);
        }
        old
    }
}

// ===== Sub-object editing (Tool::MapEdit) ====================================

/// How far the cursor has to travel between press and release for the gesture
/// to count as a DRAG rather than a click. Shared by the box-select rectangle
/// (which only draws past it) and the release handler (which only applies a box
/// past it), so what you see and what happens can't disagree.
pub(crate) const MAP_DRAG_PX: f32 = 4.0;

/// Which sub-object kind the Map tool selects.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MapSubMode {
    Vertex,
    Edge,
    #[default]
    Face,
}

impl MapSubMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            MapSubMode::Vertex => "vertex",
            MapSubMode::Edge => "edge",
            MapSubMode::Face => "face",
        }
    }

    pub(crate) fn next(self) -> Self {
        match self {
            MapSubMode::Vertex => MapSubMode::Edge,
            MapSubMode::Edge => MapSubMode::Face,
            MapSubMode::Face => MapSubMode::Vertex,
        }
    }

    /// The three modes in switch order, with the glyph the viewport strip and
    /// the Map tab both label them with — one vocabulary, so the chip you click
    /// in the viewport is the chip you see in the panel.
    pub(crate) const ALL: [MapSubMode; 3] =
        [MapSubMode::Vertex, MapSubMode::Edge, MapSubMode::Face];

    pub(crate) fn glyph(self) -> &'static str {
        match self {
            MapSubMode::Vertex => "◆",
            MapSubMode::Edge => "╱",
            MapSubMode::Face => "◼",
        }
    }

    /// The keybind that jumps straight to this mode.
    pub(crate) fn cmd(self) -> crate::map_keys::MapCmd {
        match self {
            MapSubMode::Vertex => crate::map_keys::MapCmd::ModeVertex,
            MapSubMode::Edge => crate::map_keys::MapCmd::ModeEdge,
            MapSubMode::Face => crate::map_keys::MapCmd::ModeFace,
        }
    }

    /// Plural noun for counts and button labels ("select every face").
    pub(crate) fn plural(self) -> &'static str {
        match self {
            MapSubMode::Vertex => "vertices",
            MapSubMode::Edge => "edges",
            MapSubMode::Face => "faces",
        }
    }
}

/// What a click or a box drag does to the existing sub-object selection.
///
/// Shift ADDS and Ctrl SUBTRACTS, which is the convention every modeling tool
/// shares — and the reason both used to mean "toggle" was that there was only
/// one code path for them. Toggling is fine for one click and useless for a
/// box: dragging a box over a region you have half-selected would flip the
/// overlap back off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectMode {
    Replace,
    Add,
    Subtract,
}

impl SelectMode {
    /// From the live modifier state.
    pub(crate) fn of(shift: bool, ctrl: bool) -> Self {
        match (shift, ctrl) {
            (_, true) => SelectMode::Subtract,
            (true, false) => SelectMode::Add,
            _ => SelectMode::Replace,
        }
    }

    pub(crate) fn keeps_existing(self) -> bool {
        self != SelectMode::Replace
    }
}

/// Which frame the sub-object gizmo's handles point along.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MapOrient {
    /// World axes.
    Global,
    /// The node's own axes.
    Local,
    /// The SELECTION's frame: the average face normal (face mode) or the edge
    /// direction (edge mode), falling back to Local when there is no direction
    /// to speak of. The default — pushing a diagonal wall straight out of
    /// itself is the whole point of a modeling gizmo.
    #[default]
    Normal,
}

impl MapOrient {
    pub(crate) fn label(self) -> &'static str {
        match self {
            MapOrient::Global => "global",
            MapOrient::Local => "local",
            MapOrient::Normal => "normal",
        }
    }
}

/// What the sub-object gizmo does. The global tool stays on ⬢ Map (switching
/// to the Rotate/Scale TOOLS would drop the sub-object selection), so the map
/// tool carries its own transform mode — G/R/S, as in every modeling package.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum MapXform {
    #[default]
    Move,
    Rotate,
    Scale,
}

impl MapXform {
    pub(crate) fn label(self) -> &'static str {
        match self {
            MapXform::Move => "move",
            MapXform::Rotate => "rotate",
            MapXform::Scale => "scale",
        }
    }

    /// The gizmo the viewport should draw/hit-test for this mode.
    pub(crate) fn tool(self) -> crate::gizmo::Tool {
        match self {
            MapXform::Move => crate::gizmo::Tool::MapEdit,
            MapXform::Rotate => crate::gizmo::Tool::Rotate,
            MapXform::Scale => crate::gizmo::Tool::Scale,
        }
    }
}

/// Which half of a draw gesture is in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DrawPhase {
    /// Dragging out the footprint on the build plane.
    Base,
    /// Base is fixed; the cursor now sets the height along the plane normal.
    Height,
}

/// An in-progress "draw a shape" gesture: a build plane (picked from whatever
/// surface the press landed on), a rectangle on it, and a height off it.
pub(crate) struct MapDraw {
    pub(crate) shape: MapShape,
    pub(crate) phase: DrawPhase,
    /// Build-plane frame in world space. `u`/`v` span the plane, `normal` is
    /// the height axis, and `origin` is where the press landed.
    pub(crate) origin: floptle_core::math::DVec3,
    pub(crate) u: floptle_core::math::Vec3,
    pub(crate) v: floptle_core::math::Vec3,
    pub(crate) normal: floptle_core::math::Vec3,
    /// Footprint corners in plane coordinates, relative to `origin`.
    pub(crate) a: floptle_core::math::Vec2,
    pub(crate) b: floptle_core::math::Vec2,
    /// Signed height along `normal`.
    pub(crate) height: f32,
    /// Quarter turns about the build-plane normal, applied on top of the one
    /// the drag direction implies: `,` / `.` step it by one, Z by two ("climb
    /// the other way"). The footprint you dragged is the footprint you get —
    /// an odd turn swaps the shape's X/Z extents so it still fills the
    /// rectangle rather than poking out of it.
    pub(crate) turns: i32,
}

impl MapDraw {
    /// World position of a plane coordinate.
    pub(crate) fn point(&self, p: floptle_core::math::Vec2) -> floptle_core::math::DVec3 {
        self.origin + (self.u * p.x + self.v * p.y).as_dvec3()
    }

    /// Total quarter turns about the normal: the drag direction contributes a
    /// half turn (so stairs climb the way you dragged), `,` / `.` / Z the rest.
    fn quarter_turns(&self) -> i32 {
        let from_drag = if self.b.y >= self.a.y { 2 } else { 0 };
        (from_drag + self.turns).rem_euclid(4)
    }

    /// Half-extents of the shape being drawn, in its own local frame
    /// (X across `u`, Y along `normal`, Z across `v`) — swapped on an odd
    /// quarter turn so the shape keeps filling the drawn footprint.
    pub(crate) fn half(&self) -> floptle_core::math::Vec3 {
        let d = (self.b - self.a).abs() * 0.5;
        let (x, z) = if self.quarter_turns() % 2 == 0 { (d.x, d.y) } else { (d.y, d.x) };
        floptle_core::math::Vec3::new(x, self.height.abs() * 0.5, z)
    }

    /// The node transform the finished shape gets: centered on the footprint,
    /// lifted half its height off the plane, rotated into the plane's frame.
    pub(crate) fn transform(&self) -> floptle_core::Transform {
        use floptle_core::math::{Mat3, Quat, Vec3};
        let mid = (self.a + self.b) * 0.5;
        let center = self.point(mid) + (self.normal * (self.height * 0.5)).as_dvec3();
        // Asymmetric shapes (stairs, ramps) are tall at local -Z, so point -Z
        // along the drag: a staircase climbs the way you dragged it. The turn
        // is a ROTATION of both in-plane axes about the normal — mirroring one
        // axis would invert the winding and turn the shape inside out.
        let q = Quat::from_axis_angle(
            self.normal,
            self.quarter_turns() as f32 * std::f32::consts::FRAC_PI_2,
        );
        let basis = Mat3::from_cols(q * self.u, self.normal, q * self.v);
        floptle_core::Transform {
            translation: center,
            rotation: Quat::from_mat3(&basis).normalize(),
            scale: Vec3::ONE,
        }
    }

    /// True once the footprint is big enough to be a real shape.
    pub(crate) fn has_base(&self) -> bool {
        let d = (self.b - self.a).abs();
        d.x > 1e-3 && d.y > 1e-3
    }

    /// The dimension readout drawn beside the cursor, including whichever
    /// resolution knob this shape has (so `[` / `]` show their effect as a
    /// number, not just a silhouette).
    pub(crate) fn readout(&self, opts: MapOpts) -> String {
        let h = self.half() * 2.0;
        let size = match self.phase {
            DrawPhase::Base => format!("{:.2} x {:.2}", h.x, h.z),
            DrawPhase::Height => format!("{:.2} x {:.2} x {:.2}", h.x, h.y, h.z),
        };
        match self.shape.detail(opts) {
            Some(d) => format!("{size}  ·  {d}"),
            None => size,
        }
    }

    /// The world-space rise direction (local -Z), for the direction arrow.
    pub(crate) fn rise_dir(&self) -> floptle_core::math::Vec3 {
        self.transform().rotation * floptle_core::math::Vec3::NEG_Z
    }
}

/// The active sub-object selection on one map-mesh node.
#[derive(Clone, Debug)]
pub(crate) struct MapSel {
    pub(crate) entity: floptle_core::Entity,
    pub(crate) id: u32,
    pub(crate) verts: BTreeSet<u32>,
    /// Canonical (a < b) vertex pairs.
    pub(crate) edges: BTreeSet<(u32, u32)>,
    pub(crate) faces: BTreeSet<u32>,
}

impl MapSel {
    pub(crate) fn new(entity: floptle_core::Entity, id: u32) -> Self {
        Self { entity, id, verts: BTreeSet::new(), edges: BTreeSet::new(), faces: BTreeSet::new() }
    }

    pub(crate) fn clear(&mut self) {
        self.verts.clear();
        self.edges.clear();
        self.faces.clear();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.verts.is_empty() && self.edges.is_empty() && self.faces.is_empty()
    }

    /// Re-express this selection in `to`'s currency, the way every modeling
    /// package does when you switch sub-object mode: a face selection becomes
    /// its verts/edges, a vert selection becomes the faces it fully encloses.
    /// Losing the selection on every Tab press made iterating impossible.
    pub(crate) fn convert(&mut self, mesh: &MapMesh, to: MapSubMode) {
        if self.is_empty() {
            return;
        }
        let verts = self.drag_verts(mesh);
        match to {
            MapSubMode::Vertex => {
                self.verts = verts;
                self.edges.clear();
                self.faces.clear();
            }
            MapSubMode::Edge => {
                self.edges = mesh
                    .edges()
                    .into_iter()
                    .filter(|(a, b)| verts.contains(a) && verts.contains(b))
                    .collect();
                self.verts.clear();
                self.faces.clear();
            }
            MapSubMode::Face => {
                self.faces = (0..mesh.faces.len() as u32)
                    .filter(|&f| {
                        mesh.faces[f as usize].verts.iter().all(|v| verts.contains(v))
                    })
                    .collect();
                self.verts.clear();
                self.edges.clear();
            }
        }
    }

    /// Drop anything the mesh no longer has (an op reindexed under us).
    pub(crate) fn prune(&mut self, mesh: &MapMesh) {
        let (nv, nf) = (mesh.verts.len() as u32, mesh.faces.len() as u32);
        self.verts.retain(|&v| v < nv);
        self.faces.retain(|&f| f < nf);
        self.edges.retain(|&(a, b)| a < nv && b < nv);
    }

    /// Every vertex the current selection drags (faces/edges expand to verts).
    pub(crate) fn drag_verts(&self, mesh: &MapMesh) -> BTreeSet<u32> {
        let mut out = self.verts.clone();
        for &(a, b) in &self.edges {
            out.insert(a);
            out.insert(b);
        }
        for &f in &self.faces {
            if let Some(face) = mesh.faces.get(f as usize) {
                out.extend(face.verts.iter().copied());
            }
        }
        out
    }
}

/// What the cursor is over, per the active sub-mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MapHover {
    Vert(u32),
    Edge(u32, u32),
    Face(u32),
}

/// A knife cut waiting for its second click: the face it started on and the
/// border point it starts from.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MapKnife {
    pub(crate) face: u32,
    pub(crate) at: floptle_map::CutPoint,
}

/// A live sub-object gizmo drag: the dragged verts' pre-drag local positions.
pub(crate) struct MapDrag {
    pub(crate) verts: Vec<(u32, floptle_core::math::Vec3)>,
}

/// Screen-space overlay for the Scene tab (physical px; scene_tab divides by ppp).
#[derive(Default)]
pub(crate) struct MapViz {
    /// (a, b, selected) projected edges of the target mesh.
    pub(crate) edges: Vec<(floptle_core::math::Vec2, floptle_core::math::Vec2, bool)>,
    /// (pos, selected) projected verts — only drawn in Vertex mode.
    pub(crate) verts: Vec<(floptle_core::math::Vec2, bool)>,
    /// Projected outlines of SELECTED faces.
    pub(crate) sel_faces: Vec<Vec<floptle_core::math::Vec2>>,
    /// Outline of the hovered element (vert ring / edge / face).
    pub(crate) hover: Vec<(floptle_core::math::Vec2, floptle_core::math::Vec2)>,
    /// Box-select rectangle in progress (anchor, current).
    pub(crate) rect: Option<(floptle_core::math::Vec2, floptle_core::math::Vec2)>,
    /// Show vert dots (Vertex mode only — face/edge modes stay uncluttered).
    pub(crate) show_verts: bool,
    /// The shape being drawn right now: its wireframe…
    pub(crate) preview: Vec<(floptle_core::math::Vec2, floptle_core::math::Vec2)>,
    /// …its footprint on the build plane (drawn heavier — it's the thing you
    /// are actually sizing)…
    pub(crate) base_ring: Vec<floptle_core::math::Vec2>,
    /// …the height axis while it is being dragged…
    pub(crate) height_axis: Option<(floptle_core::math::Vec2, floptle_core::math::Vec2)>,
    /// …and the live dimension readout, anchored near the cursor.
    pub(crate) label: Option<(floptle_core::math::Vec2, String)>,
    /// Which way a stair/ramp climbs: `(low end, high end)` on the base, drawn
    /// as an arrow. Shown while drawing AND for a selected rising shape, so
    /// "which way is up" is never a guess.
    pub(crate) arrow: Option<(floptle_core::math::Vec2, floptle_core::math::Vec2)>,
    /// Knife: where the pending cut starts (`None` before the first click),
    /// where it would end right now, and whether that end is an existing CORNER
    /// (drawn as a ring — a corner cut adds no vertex, and knowing which you're
    /// about to get is the whole difference between a clean cut and a sliver).
    pub(crate) knife_from: Option<floptle_core::math::Vec2>,
    pub(crate) knife_to: Option<(floptle_core::math::Vec2, bool)>,
}

/// One Map-tab operation, routed through `EditorCmd` (the tab holds disjoint
/// borrows and can't call `&mut Editor` methods).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MapOp {
    Extrude,
    Inset,
    DeleteFaces,
    FlipFaces,
    FlipAll,
    WeldSelected,
    Subdivide,
    Bridge,
    SnapToGrid,
    /// Re-generate a still-parametric shape with new parameters (stair steps,
    /// cylinder sides). Refused once the geometry has been edited.
    Reshape(floptle_map::ShapeSpec),
    /// Rescale the whole mesh to these local extents, about its bounds center.
    Resize(floptle_core::math::Vec3),
    /// Move the node's origin to the mesh's bounds center (the node moves to
    /// compensate, so nothing shifts on screen).
    CenterPivot,
    /// Move the node's origin onto the current sub-object selection.
    PivotToSelection,
    AssignSlot(u16),
    AddSlot(String),
    /// Add a slot AND give it a fresh material override in one step — the
    /// "make these faces a different material" button.
    MaterialFromSelection(String),
    SelectAll,
    SelectNone,
    /// Everything of the current mode that ISN'T selected, and nothing that is.
    SelectInvert,
    Grow,
    SelectConnected,
    SelectCoplanar,
    SelectSlot(u16),
    /// Extend an edge selection along its quad loops.
    SelectLoop,
}

impl Editor {
    /// The map-mesh node sub-object editing targets: the PRIMARY selected node,
    /// when it is a `Matter::MapMesh`.
    pub(crate) fn map_target(&self) -> Option<(floptle_core::Entity, u32)> {
        let e = self.primary()?;
        match self.world.get::<floptle_core::Matter>(e) {
            Some(floptle_core::Matter::MapMesh { id }) => Some((e, *id)),
            _ => None,
        }
    }

    /// Switch the vertex/edge/face sub-mode, CONVERTING the current selection
    /// instead of dropping it (pick a face, press Tab, and you are holding its
    /// four verts — losing the selection on every mode switch made iterating
    /// on a shape impossible).
    pub(crate) fn set_map_mode(&mut self, mode: MapSubMode) {
        if self.map_mode == mode {
            return;
        }
        self.map_mode = mode;
        let mesh = self.map_sel.as_ref().and_then(|s| self.maps.meshes.get(&s.id)).cloned();
        if let (Some(sel), Some(mesh)) = (self.map_sel.as_mut(), mesh) {
            sel.convert(&mesh, mode);
        }
    }

    /// The gizmo tool the viewport should build/hit-test/paint. The map tool
    /// carries its own move/rotate/scale mode so that switching it can't drop
    /// the sub-object selection the way switching the global TOOL does.
    pub(crate) fn gizmo_tool(&self) -> crate::gizmo::Tool {
        if self.tool == crate::gizmo::Tool::MapEdit { self.map_xform.tool() } else { self.tool }
    }

    /// Keep `map_sel` pointing at the current target (clearing it when the
    /// node selection moved elsewhere); returns the live target.
    pub(crate) fn map_sync_sel(&mut self) -> Option<(floptle_core::Entity, u32)> {
        let target = self.map_target();
        match (&self.map_sel, target) {
            (Some(sel), Some((e, id))) if sel.entity == e && sel.id == id => {}
            (_, Some((e, id))) => self.map_sel = Some(MapSel::new(e, id)),
            (_, None) => self.map_sel = None,
        }
        target
    }

    /// Per-vertex "the camera can see this" mask, or `None` when select-hidden
    /// is on (everything counts). Without it you constantly grab the vertex on
    /// the FAR side of a wall, which is the classic beginner trap in every
    /// modeling tool that skips this test.
    /// The camera's position in `e`'s local frame, or `None` when hidden
    /// sub-objects are selectable anyway (no test needed).
    fn map_eye_local(&self, e: floptle_core::Entity) -> Option<floptle_core::math::Vec3> {
        if self.map_select_hidden {
            return None;
        }
        let t = floptle_core::world_transform(&self.world, e);
        let cam = self.camera.render_camera();
        let m_inv = t.render_matrix(cam.world_position).inverse();
        m_inv.is_finite().then(|| {
            // The render matrix is camera-relative, so the eye is the preimage
            // of the origin.
            (m_inv * floptle_core::math::Vec4::new(0.0, 0.0, 0.0, 1.0)).truncate()
        })
    }

    /// Screen-space hit test for the current sub-mode. `cursor` in physical px.
    /// Vert/edge picking is by projected distance (constant grab feel at any
    /// depth, the gizmo convention); face picking is an exact kernel raycast.
    pub(crate) fn map_pick(&self, cursor: floptle_core::math::Vec2) -> Option<MapHover> {
        use floptle_core::math::{Vec2, Vec4};
        let (e, id) = self.map_target()?;
        let mesh = self.maps.meshes.get(&id)?;
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let t = floptle_core::world_transform(&self.world, e);
        let world_of = |p: floptle_core::math::Vec3| {
            t.translation + (t.rotation * (t.scale * p)).as_dvec3()
        };
        // Occlusion is one raycast per CANDIDATE, and candidates are ranked by
        // screen distance first — testing every vertex up front was O(verts x
        // faces) on a mesh that can hold thousands of both, every frame.
        let eye = self.map_eye_local(e);
        let visible = |v: u32| {
            eye.is_none_or(|eye| {
                let p = mesh.verts[v as usize];
                // Stop just short of the vertex so its own faces don't occlude it.
                floptle_map::raycast(mesh, eye, p - eye, 0.999).is_none()
            })
        };
        match self.map_mode {
            MapSubMode::Vertex => {
                let mut cands: Vec<(u32, f32)> = Vec::new();
                for (i, &p) in mesh.verts.iter().enumerate() {
                    if let Some(s) = crate::viz::project(world_of(p), cam.world_position, vp, w, h) {
                        let d = (s - cursor).length();
                        if d <= crate::gizmo::HANDLE_PX + 2.0 {
                            cands.push((i as u32, d));
                        }
                    }
                }
                cands.sort_by(|a, b| a.1.total_cmp(&b.1));
                cands.into_iter().find(|&(v, _)| visible(v)).map(|(v, _)| MapHover::Vert(v))
            }
            MapSubMode::Edge => {
                let mut cands: Vec<((u32, u32), f32)> = Vec::new();
                for (a, b) in mesh.edges() {
                    let (Some(sa), Some(sb)) = (
                        crate::viz::project(world_of(mesh.verts[a as usize]), cam.world_position, vp, w, h),
                        crate::viz::project(world_of(mesh.verts[b as usize]), cam.world_position, vp, w, h),
                    ) else {
                        continue;
                    };
                    let d = crate::gizmo::seg_dist(cursor, sa, sb);
                    if d <= crate::gizmo::HANDLE_PX {
                        cands.push(((a, b), d));
                    }
                }
                cands.sort_by(|x, y| x.1.total_cmp(&y.1));
                cands
                    .into_iter()
                    .find(|&((a, b), _)| visible(a) || visible(b))
                    .map(|((a, b), _)| MapHover::Edge(a, b))
            }
            MapSubMode::Face => {
                // The pick() ray recipe (camera-relative, ADR-0015), pushed into
                // the node's local frame; unnormalized rd keeps t comparable.
                let inv = vp.inverse();
                let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
                let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
                let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
                let ro = near.truncate() / near.w;
                let rd = (far.truncate() / far.w - ro).normalize();
                let m_inv = t.render_matrix(cam.world_position).inverse();
                if !m_inv.is_finite() {
                    return None;
                }
                let ro_l = (m_inv * ro.extend(1.0)).truncate();
                let rd_l = (m_inv * rd.extend(0.0)).truncate();
                floptle_map::raycast(mesh, ro_l, rd_l, f32::MAX).map(|hit| MapHover::Face(hit.face))
            }
        }
    }

    /// Apply a click at `cursor` to the sub-object selection. Returns true when
    /// the click landed on a sub-object (so the caller doesn't fall through to
    /// node picking).
    pub(crate) fn map_click(&mut self, cursor: floptle_core::math::Vec2, how: SelectMode) -> bool {
        if self.map_sync_sel().is_none() {
            return false;
        }
        let hit = self.map_pick(cursor);
        let Some(sel) = self.map_sel.as_mut() else { return false };
        let Some(hit) = hit else {
            // Clicked bare space: a plain click clears the sub-selection but
            // stays in map mode (box-select may start here instead).
            if !how.keeps_existing() {
                sel.clear();
            }
            return false;
        };
        if !how.keeps_existing() {
            sel.clear();
        }
        // Shift ADDS, Ctrl SUBTRACTS — but a Shift-click on something already
        // in the selection still toggles it off, because that is the only way
        // to drop ONE item without a box, and every tool does it.
        match hit {
            MapHover::Vert(v) => match how {
                SelectMode::Subtract => {
                    sel.verts.remove(&v);
                }
                SelectMode::Add if sel.verts.contains(&v) => {
                    sel.verts.remove(&v);
                }
                _ => {
                    sel.verts.insert(v);
                }
            },
            MapHover::Edge(a, b) => {
                let k = (a, b);
                match how {
                    SelectMode::Subtract => {
                        sel.edges.remove(&k);
                    }
                    SelectMode::Add if sel.edges.contains(&k) => {
                        sel.edges.remove(&k);
                    }
                    _ => {
                        sel.edges.insert(k);
                    }
                }
            }
            MapHover::Face(f) => match how {
                SelectMode::Subtract => {
                    sel.faces.remove(&f);
                }
                SelectMode::Add if sel.faces.contains(&f) => {
                    sel.faces.remove(&f);
                }
                _ => {
                    sel.faces.insert(f);
                }
            },
        }
        true
    }

    /// Box-select release: everything of the current mode whose projection
    /// falls inside the rect joins (or leaves) the selection.
    pub(crate) fn map_box_apply(
        &mut self,
        a: floptle_core::math::Vec2,
        b: floptle_core::math::Vec2,
        how: SelectMode,
    ) {
        let Some((e, id)) = self.map_sync_sel() else { return };
        let Some(mesh) = self.maps.meshes.get(&id) else { return };
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let t = floptle_core::world_transform(&self.world, e);
        let (lo, hi) = (a.min(b), a.max(b));
        let inside = |p: floptle_core::math::Vec3| {
            let wp = t.translation + (t.rotation * (t.scale * p)).as_dvec3();
            crate::viz::project(wp, cam.world_position, vp, w, h)
                .is_some_and(|s| s.x >= lo.x && s.x <= hi.x && s.y >= lo.y && s.y <= hi.y)
        };
        let mode = self.map_mode;
        // Cheap screen test first; the occlusion raycast only runs for verts
        // that actually fall inside the rectangle.
        let eye = self.map_eye_local(e);
        let hit = |v: u32| {
            let p = mesh.verts[v as usize];
            inside(p) && eye.is_none_or(|eye| floptle_map::raycast(mesh, eye, p - eye, 0.999).is_none())
        };
        let mut verts = Vec::new();
        let mut edges = Vec::new();
        let mut faces = Vec::new();
        match mode {
            MapSubMode::Vertex => {
                verts = (0..mesh.verts.len() as u32).filter(|&v| hit(v)).collect();
            }
            MapSubMode::Edge => {
                edges =
                    mesh.edges().into_iter().filter(|&(x, y)| hit(x) && hit(y)).collect();
            }
            MapSubMode::Face => {
                faces = (0..mesh.faces.len() as u32)
                    .filter(|&f| mesh.faces[f as usize].verts.iter().all(|&v| hit(v)))
                    .collect();
            }
        }
        let Some(sel) = self.map_sel.as_mut() else { return };
        match how {
            SelectMode::Replace => {
                sel.clear();
                sel.verts.extend(verts);
                sel.edges.extend(edges);
                sel.faces.extend(faces);
            }
            SelectMode::Add => {
                sel.verts.extend(verts);
                sel.edges.extend(edges);
                sel.faces.extend(faces);
            }
            SelectMode::Subtract => {
                for v in verts {
                    sel.verts.remove(&v);
                }
                for e in edges {
                    sel.edges.remove(&e);
                }
                for f in faces {
                    sel.faces.remove(&f);
                }
            }
        }
    }

    // ---- knife ---------------------------------------------------------------

    /// The cursor ray in `e`'s object space (origin, direction), camera-relative
    /// like every other pick in the editor (ADR-0015).
    fn map_local_ray(
        &self,
        e: floptle_core::Entity,
        cursor: floptle_core::math::Vec2,
    ) -> Option<(floptle_core::math::Vec3, floptle_core::math::Vec3)> {
        use floptle_core::math::{Vec2, Vec4};
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let inv = vp.inverse();
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro).normalize();
        let t = floptle_core::world_transform(&self.world, e);
        let m_inv = t.render_matrix(cam.world_position).inverse();
        m_inv
            .is_finite()
            .then(|| ((m_inv * ro.extend(1.0)).truncate(), (m_inv * rd.extend(0.0)).truncate()))
    }

    /// Where a knife click would land: the face under the cursor and the point
    /// on its border the cut would run from/to.
    ///
    /// The CORNER snap is done in screen space (within a gizmo handle of a
    /// projected corner), like every other grab in this editor, so aiming at a
    /// corner feels the same here as it does in vertex mode. The edge point is
    /// then solved exactly in object space from the ray hit — projecting the
    /// edge and interpolating in 2D would drift at grazing angles, which is
    /// precisely where you cut a wall.
    pub(crate) fn map_knife_pick(
        &self,
        cursor: floptle_core::math::Vec2,
    ) -> Option<(u32, floptle_map::CutPoint)> {
        let (e, id) = self.map_target()?;
        let mesh = self.maps.meshes.get(&id)?;
        let (ro, rd) = self.map_local_ray(e, cursor)?;
        let hit = floptle_map::raycast(mesh, ro, rd, f32::MAX)?;
        let face = mesh.faces.get(hit.face as usize)?;
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let t = floptle_core::world_transform(&self.world, e);
        let mut best: Option<(f32, u32)> = None;
        for &v in &face.verts {
            let Some(p) = mesh.verts.get(v as usize) else { continue };
            let wp = t.translation + (t.rotation * (t.scale * *p)).as_dvec3();
            if let Some(s) = crate::viz::project(wp, cam.world_position, vp, w, h) {
                let d = (s - cursor).length();
                if d <= crate::gizmo::HANDLE_PX && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, v));
                }
            }
        }
        if let Some((_, v)) = best {
            return Some((hit.face, floptle_map::CutPoint::Vert(v)));
        }
        // `0.0` corner-snap radius: corners were handled above, in the units
        // that actually match how the tool feels.
        floptle_map::nearest_cut_point(mesh, hit.face, hit.pos, 0.0).map(|c| (hit.face, c))
    }

    /// One knife click. The first sets the cut's anchor; the second cuts, and
    /// leaves the anchor on the corner it just made so a groove can be walked
    /// across a face in one gesture (Esc ends the chain).
    pub(crate) fn map_knife_click(&mut self, cursor: floptle_core::math::Vec2) {
        if self.playing {
            return;
        }
        let Some((face, at)) = self.map_knife_pick(cursor) else {
            self.map_note(
                floptle_script::LogLevel::Debug,
                "the knife cuts a face — aim at one (a cut runs from one edge or corner to another)",
            );
            return;
        };
        let Some(pending) = self.map_knife else {
            self.map_knife = Some(MapKnife { face, at });
            return;
        };
        let Some((_, id)) = self.map_target() else { return };
        let Some(pre) = self.maps.meshes.get(&id).cloned() else { return };
        // The two ends must belong to ONE face. After a cut the anchor sits on a
        // corner shared by both halves, so accept the face the second click
        // landed on whenever it owns the anchor — that is what makes chaining
        // work without asking which half you are on.
        let anchor_here = |mesh: &MapMesh, f: u32| -> bool {
            let Some(ring) = mesh.faces.get(f as usize) else { return false };
            match pending.at {
                floptle_map::CutPoint::Vert(v) => ring.verts.contains(&v),
                floptle_map::CutPoint::Edge { a, b, .. } => {
                    ring.verts.contains(&a) && ring.verts.contains(&b)
                }
            }
        };
        if face != pending.face && !anchor_here(&pre, face) {
            // Not one face: rather than refuse and strand the user holding a
            // stale anchor, restart the cut from where they just clicked.
            self.map_knife = Some(MapKnife { face, at });
            self.map_note(
                floptle_script::LogLevel::Debug,
                "both ends of a cut have to be on the same face — started a new cut here",
            );
            return;
        }
        let on = face;
        let Some(mesh) = self.maps.meshes.get_mut(&id) else { return };
        match floptle_map::knife(mesh, on, pending.at, at) {
            Ok(cut) => {
                let ends_at = cut.v1;
                // Chain: keep cutting from the corner this cut just made.
                let next_face = [cut.a, cut.b].into_iter().find(|&f| {
                    mesh.faces.get(f as usize).is_some_and(|r| r.verts.contains(&ends_at))
                });
                if let Some(sel) = self.map_sel.as_mut() {
                    sel.prune(mesh);
                }
                self.maps.dirty.insert(id);
                self.push_history(crate::Snapshot::MapMesh(id, pre));
                self.map_knife =
                    next_face.map(|f| MapKnife { face: f, at: floptle_map::CutPoint::Vert(ends_at) });
            }
            // A refusal keeps the anchor: the aim was wrong, not the intent.
            Err(why) => self.map_note(floptle_script::LogLevel::Warn, format!("knife: {why}")),
        }
    }

    /// Esc while the knife is armed: drop the pending cut first, then the tool.
    /// Returns true when it consumed the key.
    pub(crate) fn map_knife_cancel(&mut self) -> bool {
        if self.map_knife.take().is_some() {
            return true;
        }
        if self.map_knife_on {
            self.map_knife_on = false;
            return true;
        }
        false
    }

    /// Arm/disarm the knife. Arming disarms shape drawing (they both own the
    /// click) and drops any half-drawn shape.
    pub(crate) fn set_map_knife(&mut self, on: bool) {
        self.map_knife_on = on;
        self.map_knife = None;
        if on {
            self.map_arm = None;
            self.map_draw = None;
        }
    }

    /// Per-frame driver (before the render gather, like the sculpt/paint
    /// drivers): keeps `map_sel` honest and rebuilds the Scene-tab overlay.
    pub(crate) fn map_edit_frame_update(&mut self) {
        self.map_viz = None;
        if self.tool != crate::gizmo::Tool::MapEdit || self.playing {
            self.map_gizmo = None;
            self.map_draw = None;
            return;
        }
        // A draw gesture owns the viewport: no gizmo, no selection overlay,
        // just the shape taking form.
        if self.map_draw.is_some() {
            self.map_gizmo = None;
            if let Some(cursor) = self.cursor {
                self.map_draw_update(cursor);
            }
            self.map_viz = Some(self.map_draw_viz());
            return;
        }
        // The knife owns the click while it is armed, so the gizmo steps aside —
        // a handle under the cursor would eat the cut.
        self.map_gizmo = if self.map_knife_on { None } else { self.map_gizmo_xf() };
        let Some((e, id)) = self.map_sync_sel() else { return };
        let Some(mesh) = self.maps.meshes.get(&id) else { return };
        let Some(gpu) = self.gpu.as_ref() else { return };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let t = floptle_core::world_transform(&self.world, e);
        let project = |p: floptle_core::math::Vec3| {
            let wp = t.translation + (t.rotation * (t.scale * p)).as_dvec3();
            crate::viz::project(wp, cam.world_position, vp, w, h)
        };
        let sel = self.map_sel.as_ref();
        let mut viz = MapViz { show_verts: self.map_mode == MapSubMode::Vertex, ..Default::default() };
        for (a, b) in mesh.edges() {
            if let (Some(sa), Some(sb)) = (project(mesh.verts[a as usize]), project(mesh.verts[b as usize])) {
                let on = sel.is_some_and(|s| s.edges.contains(&(a, b)));
                viz.edges.push((sa, sb, on));
            }
        }
        for (i, &p) in mesh.verts.iter().enumerate() {
            if let Some(s) = project(p) {
                let on = sel.is_some_and(|s2| s2.verts.contains(&(i as u32)));
                viz.verts.push((s, on));
            }
        }
        if let Some(s) = sel {
            for &f in &s.faces {
                if let Some(face) = mesh.faces.get(f as usize) {
                    let ring: Vec<_> =
                        face.verts.iter().filter_map(|&v| project(mesh.verts[v as usize])).collect();
                    if ring.len() >= 3 {
                        viz.sel_faces.push(ring);
                    }
                }
            }
        }
        // Hover telegraph (only when the cursor is free — not mid-drag, and not
        // while the knife is showing its own snap point instead).
        if self.grabbed.is_none() && !self.map_knife_on && self.cursor_over_scene()
            && let Some(cursor) = self.cursor {
                match self.map_pick(cursor) {
                    Some(MapHover::Vert(v)) => {
                        if let Some(s) = project(mesh.verts[v as usize]) {
                            let r = crate::gizmo::HANDLE_PX;
                            viz.hover.push((s - floptle_core::math::Vec2::new(r, 0.0), s + floptle_core::math::Vec2::new(r, 0.0)));
                            viz.hover.push((s - floptle_core::math::Vec2::new(0.0, r), s + floptle_core::math::Vec2::new(0.0, r)));
                        }
                    }
                    Some(MapHover::Edge(a, b)) => {
                        if let (Some(sa), Some(sb)) =
                            (project(mesh.verts[a as usize]), project(mesh.verts[b as usize]))
                        {
                            viz.hover.push((sa, sb));
                        }
                    }
                    Some(MapHover::Face(f)) => {
                        if let Some(face) = mesh.faces.get(f as usize) {
                            let ring: Vec<_> = face
                                .verts
                                .iter()
                                .filter_map(|&v| project(mesh.verts[v as usize]))
                                .collect();
                            for i in 0..ring.len() {
                                viz.hover.push((ring[i], ring[(i + 1) % ring.len()]));
                            }
                        }
                    }
                    None => {}
                }
        }
        // A selected staircase/ramp shows its climb direction too — you should
        // never have to orbit the camera to work out which way it goes.
        if mesh.spec.is_some_and(|s| s.kind.rises())
            && let Some((lo_b, hi_b)) = mesh.bounds()
        {
            let mid = (lo_b + hi_b) * 0.5;
            let reach = (hi_b.z - lo_b.z) * 0.4;
            let at = |z: f32| {
                let p = floptle_core::math::Vec3::new(mid.x, lo_b.y, mid.z + z);
                t.translation + (t.rotation * (t.scale * p)).as_dvec3()
            };
            if let (Some(a), Some(b)) = (
                crate::viz::project(at(reach), cam.world_position, vp, w, h),
                crate::viz::project(at(-reach), cam.world_position, vp, w, h),
            ) {
                viz.arrow = Some((a, b)); // local -Z is the high end
            }
        }
        // The rectangle only appears once the press has become a DRAG — every
        // click now records an anchor (that is what lets a box start on the
        // mesh itself), and drawing a zero-size box on every click would flash.
        if let (Some(anchor), Some(cur)) = (self.map_box, self.cursor)
            && (cur - anchor).length() > MAP_DRAG_PX
        {
            viz.rect = Some((anchor, cur));
        }
        // Knife: the anchor, and the point the next click would cut to. Drawn
        // live so the cut is aimed BEFORE it is made, not discovered after.
        if self.map_knife_on {
            viz.knife_from =
                self.map_knife.and_then(|k| k.at.position(mesh)).and_then(&project);
            if let Some(cursor) = self.cursor.filter(|_| self.cursor_over_scene())
                && let Some((_, at)) = self.map_knife_pick(cursor)
                && let Some(p) = at.position(mesh).and_then(&project)
            {
                viz.knife_to = Some((p, matches!(at, floptle_map::CutPoint::Vert(_))));
            }
        }
        self.map_viz = Some(viz);
    }

    /// The in-progress shape's overlay: its full wireframe, the footprint ring,
    /// the height axis, and the size readout. Everything is rebuilt from the
    /// gesture each frame, so the preview IS the geometry that will be built.
    fn map_draw_viz(&self) -> MapViz {
        use floptle_core::math::{Vec2, Vec3};
        let mut viz = MapViz::default();
        let Some(draw) = self.map_draw.as_ref() else { return viz };
        let Some(gpu) = self.gpu.as_ref() else { return viz };
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let vp = cam.view_proj(w / h);
        let project = |p: floptle_core::math::DVec3| crate::viz::project(p, cam.world_position, vp, w, h);

        // Footprint, always — this is what the base drag is sizing.
        let corners = [
            draw.a,
            Vec2::new(draw.b.x, draw.a.y),
            draw.b,
            Vec2::new(draw.a.x, draw.b.y),
        ];
        viz.base_ring = corners.iter().filter_map(|&c| project(draw.point(c))).collect();

        if draw.has_base() {
            // The real candidate mesh, placed exactly where it will spawn.
            let mesh = draw.shape.sized(draw.half(), self.map_opts);
            let t = draw.transform();
            let to_world = |p: Vec3| t.translation + (t.rotation * p).as_dvec3();
            for (a, b) in mesh.edges() {
                if let (Some(sa), Some(sb)) = (
                    project(to_world(mesh.verts[a as usize])),
                    project(to_world(mesh.verts[b as usize])),
                ) {
                    viz.preview.push((sa, sb));
                }
            }
        }
        // Which way it climbs — an arrow along the base, from the low end to
        // the high end. Drawn from the first drag frame on, so the direction is
        // visible while you are still sizing the footprint.
        if draw.shape.rises() && draw.has_base() {
            let dir = draw.rise_dir();
            let half = draw.half();
            let base = draw.point((draw.a + draw.b) * 0.5);
            let reach = (half.z * 0.8).max(0.05);
            if let (Some(lo), Some(hi)) = (
                project(base - (dir * reach).as_dvec3()),
                project(base + (dir * reach).as_dvec3()),
            ) {
                viz.arrow = Some((lo, hi));
            }
        }
        if draw.phase == DrawPhase::Height {
            let base = draw.point((draw.a + draw.b) * 0.5);
            if let (Some(s0), Some(s1)) =
                (project(base), project(base + (draw.normal * draw.height).as_dvec3()))
            {
                viz.height_axis = Some((s0, s1));
            }
        }
        if let Some(cursor) = self.cursor {
            viz.label = Some((cursor, draw.readout(self.map_opts)));
        }
        viz
    }

    /// The world transform the sub-object gizmo sits on: the selection's
    /// centroid, oriented per [`MapOrient`].
    ///
    /// The orientation is the difference between a modeling tool and a toy: on
    /// `Normal` a face's handles point straight out of that face, so a diagonal
    /// wall pushes out in ONE drag instead of two axis drags that only
    /// approximate it.
    pub(crate) fn map_gizmo_xf(&self) -> Option<floptle_core::Transform> {
        use floptle_core::math::{Quat, Vec3};
        let sel = self.map_sel.as_ref()?;
        let mesh = self.maps.meshes.get(&sel.id)?;
        let verts = sel.drag_verts(mesh);
        if verts.is_empty() {
            return None;
        }
        let centroid = verts.iter().filter_map(|&v| mesh.verts.get(v as usize)).copied().sum::<Vec3>()
            / verts.len() as f32;
        let t = floptle_core::world_transform(&self.world, sel.entity);
        // Object-local direction -> world, correcting for the node's (possibly
        // non-uniform, possibly mirrored) scale: normals transform by the
        // inverse transpose, which for a diagonal scale is a divide.
        let dir_to_world = |d: Vec3| {
            let s = Vec3::new(
                if t.scale.x.abs() > 1e-6 { 1.0 / t.scale.x } else { 0.0 },
                if t.scale.y.abs() > 1e-6 { 1.0 / t.scale.y } else { 0.0 },
                if t.scale.z.abs() > 1e-6 { 1.0 / t.scale.z } else { 0.0 },
            );
            (t.rotation * (d * s)).try_normalize()
        };
        let rotation = match self.map_orient {
            MapOrient::Global => Quat::IDENTITY,
            MapOrient::Local => t.rotation,
            MapOrient::Normal => self
                .map_sel_frame(mesh)
                .and_then(|(up, along)| {
                    let up = dir_to_world(up)?;
                    // Build a right-handed basis with +Y on the selection's
                    // normal, keeping +X on a stable in-plane reference so the
                    // handles don't spin as the camera moves.
                    let hint = along
                        .and_then(dir_to_world)
                        .filter(|h| h.cross(up).length_squared() > 1e-6)
                        .unwrap_or(if up.dot(Vec3::Y).abs() > 0.99 { Vec3::X } else { Vec3::Y });
                    let x = hint.cross(up).try_normalize()?;
                    Some(Quat::from_mat3(&floptle_core::math::Mat3::from_cols(x, up, x.cross(up))))
                })
                .unwrap_or(t.rotation),
        };
        Some(floptle_core::Transform {
            translation: t.translation + (t.rotation * (t.scale * centroid)).as_dvec3(),
            rotation: rotation.normalize(),
            scale: Vec3::ONE,
        })
    }

    /// The selection's own frame in OBJECT-local space: `(up, along)` — the
    /// direction the gizmo's +Y should take, plus an optional in-plane
    /// reference for +X. `None` when the selection has no direction (loose
    /// vertices).
    fn map_sel_frame(
        &self,
        mesh: &MapMesh,
    ) -> Option<(floptle_core::math::Vec3, Option<floptle_core::math::Vec3>)> {
        use floptle_core::math::Vec3;
        let sel = self.map_sel.as_ref()?;
        if !sel.faces.is_empty() {
            // Area-weighted average normal — the same direction `extrude` uses,
            // so drag-out and E agree.
            let mut n = Vec3::ZERO;
            let mut along = None;
            for &f in &sel.faces {
                let Some(face) = mesh.faces.get(f as usize) else { continue };
                n += floptle_map::face_normal(mesh, face);
                if along.is_none() && face.verts.len() >= 2 {
                    let (a, b) = (face.verts[0], face.verts[1]);
                    along = Some(mesh.verts[b as usize] - mesh.verts[a as usize]);
                }
            }
            return n.try_normalize().map(|n| (n, along));
        }
        if !sel.edges.is_empty() {
            // Edge mode points +Y along the edge: dragging an edge along
            // itself, or scaling the selection out from it, is the useful move.
            let mut d = Vec3::ZERO;
            for &(a, b) in &sel.edges {
                let (Some(&pa), Some(&pb)) = (mesh.verts.get(a as usize), mesh.verts.get(b as usize))
                else {
                    continue;
                };
                let e = pb - pa;
                // Average without cancelling opposite-facing edges out.
                d += if e.dot(d) < 0.0 { -e } else { e };
            }
            return d.try_normalize().map(|d| (d, None));
        }
        None
    }

    /// Start a sub-object gizmo drag: snapshot the mesh for undo and the
    /// dragged verts' local positions for the absolute-from-start apply.
    pub(crate) fn map_begin_drag(&mut self) -> bool {
        let Some(sel) = self.map_sel.as_ref() else { return false };
        let Some(mesh) = self.maps.meshes.get(&sel.id) else { return false };
        let verts = sel.drag_verts(mesh);
        if verts.is_empty() {
            return false;
        }
        self.map_stroke = Some((sel.id, mesh.clone()));
        self.map_drag = Some(MapDrag {
            verts: verts
                .into_iter()
                .filter_map(|v| mesh.verts.get(v as usize).map(|&p| (v, p)))
                .collect(),
        });
        true
    }

    /// The `set_world_transform` intercept: the gizmo drag is applied to the
    /// snapshot verts instead of the node's Transform.
    ///
    /// The gizmo's own start/new transforms describe a world-space motion
    /// (translate for Move, rotate about the centroid for Rotate, scale about
    /// it for Scale), so ONE piece of math covers all three modes: send each
    /// vert to world, through the gizmo's delta, and back into node-local.
    /// Absolute-from-start, so nothing drifts over a long drag.
    pub(crate) fn map_apply_drag(&mut self, start: floptle_core::Transform, new: floptle_core::Transform) {
        let Some(sel) = self.map_sel.as_ref() else { return };
        let (id, entity) = (sel.id, sel.entity);
        let t = floptle_core::world_transform(&self.world, entity);
        let inv_rot = t.rotation.inverse();
        let inv_scale = floptle_core::math::Vec3::new(
            if t.scale.x.abs() > 1e-6 { 1.0 / t.scale.x } else { 0.0 },
            if t.scale.y.abs() > 1e-6 { 1.0 / t.scale.y } else { 0.0 },
            if t.scale.z.abs() > 1e-6 { 1.0 / t.scale.z } else { 0.0 },
        );
        let start_inv_rot = start.rotation.inverse();
        let Some(drag) = self.map_drag.as_ref() else { return };
        let Some(mesh) = self.maps.meshes.get_mut(&id) else { return };
        for &(v, p0) in &drag.verts {
            let Some(p) = mesh.verts.get_mut(v as usize) else { continue };
            // Local -> world (f64 anchor differences only, so precision holds
            // out at floating-origin distances).
            let world = t.translation + (t.rotation * (t.scale * p0)).as_dvec3();
            let rel = (world - start.translation).as_vec3();
            let moved = new.rotation * (new.scale * (start_inv_rot * rel));
            let world2 = new.translation + moved.as_dvec3();
            // World -> local.
            let q = inv_rot * (world2 - t.translation).as_vec3();
            *p = q * inv_scale;
        }
        self.maps.dirty.insert(id);
    }

    // ---- the draw tool ------------------------------------------------------

    /// The cursor's world ray: `(origin, direction)`, direction unnormalized in
    /// world units so `t` reads as a distance.
    pub(crate) fn map_cursor_ray(
        &self,
        cursor: floptle_core::math::Vec2,
    ) -> Option<(floptle_core::math::DVec3, floptle_core::math::Vec3)> {
        use floptle_core::math::{Vec2, Vec4};
        let gpu = self.gpu.as_ref()?;
        let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
        let cam = self.camera.render_camera();
        let inv = cam.view_proj(w / h).inverse();
        if !inv.is_finite() {
            return None;
        }
        let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
        let near = inv * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
        let far = inv * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
        let ro = near.truncate() / near.w;
        let rd = (far.truncate() / far.w - ro).try_normalize()?;
        Some((cam.world_position + ro.as_dvec3(), rd))
    }

    /// Where a new shape should sit: the map surface under the cursor (so you
    /// can build straight onto the wall you just made), else the ground plane.
    /// Returns the hit point and the surface normal.
    fn map_build_surface(
        &self,
        cursor: floptle_core::math::Vec2,
    ) -> Option<(floptle_core::math::DVec3, floptle_core::math::Vec3)> {
        use floptle_core::math::Vec3;
        let (ro, rd) = self.map_cursor_ray(cursor)?;
        let mut best: Option<(f64, Vec3)> = None;
        for (e, m) in self.world.query::<floptle_core::Matter>() {
            let floptle_core::Matter::MapMesh { id } = m else { continue };
            let Some(mesh) = self.maps.meshes.get(id) else { continue };
            let t = floptle_core::world_transform(&self.world, e);
            // Ray into the node's local frame (translation handled in f64).
            let inv_rot = t.rotation.inverse();
            let inv_scale = Vec3::new(
                if t.scale.x.abs() > 1e-6 { 1.0 / t.scale.x } else { 0.0 },
                if t.scale.y.abs() > 1e-6 { 1.0 / t.scale.y } else { 0.0 },
                if t.scale.z.abs() > 1e-6 { 1.0 / t.scale.z } else { 0.0 },
            );
            let ro_l = (inv_rot * (ro - t.translation).as_vec3()) * inv_scale;
            let rd_l = (inv_rot * rd) * inv_scale;
            let Some(hit) = floptle_map::raycast(mesh, ro_l, rd_l, f32::MAX) else { continue };
            let world = t.translation + (t.rotation * (t.scale * hit.pos)).as_dvec3();
            let dist = (world - ro).length();
            if best.is_none_or(|(bd, _)| dist < bd) {
                // Normals transform by the inverse transpose (a divide, for a
                // diagonal scale) so a squashed node still builds square.
                let n = (t.rotation * (hit.normal * inv_scale)).try_normalize().unwrap_or(Vec3::Y);
                best = Some((dist, n));
            }
        }
        if let Some((dist, n)) = best {
            return Some((ro + (rd * dist as f32).as_dvec3(), n));
        }
        // Ground plane (y = 0) — the blockout default.
        if rd.y.abs() < 1e-5 {
            return None;
        }
        let t = -ro.y / rd.y as f64;
        (t > 0.0).then(|| (ro + (rd.as_dvec3() * t), Vec3::Y))
    }

    /// Snap a world point to the grid when snapping is on.
    fn map_snap_world(&self, p: floptle_core::math::DVec3) -> floptle_core::math::DVec3 {
        if self.grid.snap { crate::snap_dvec3(p, self.grid.size as f64) } else { p }
    }

    fn map_snap_plane(&self, p: floptle_core::math::Vec2) -> floptle_core::math::Vec2 {
        if !self.grid.snap {
            return p;
        }
        let s = self.grid.size.max(0.01);
        floptle_core::math::Vec2::new((p.x / s).round() * s, (p.y / s).round() * s)
    }

    /// Press with a shape armed: fix the build plane and start the footprint.
    pub(crate) fn map_draw_begin(&mut self, cursor: floptle_core::math::Vec2) -> bool {
        use floptle_core::math::{Vec2, Vec3};
        let Some(shape) = self.map_arm else { return false };
        let Some((hit, normal)) = self.map_build_surface(cursor) else {
            self.map_note(
                floptle_script::LogLevel::Warn,
                "aim at the ground plane or an existing map surface to start drawing",
            );
            return false;
        };
        // In-plane basis: keep +X as world-ish as possible so a wall drawn on a
        // wall still comes out upright.
        let u = if normal.dot(Vec3::Y).abs() > 0.99 {
            Vec3::X
        } else {
            Vec3::Y.cross(normal).normalize()
        };
        let v = u.cross(normal);
        // Snap IN THE PLANE only: rounding the normal component too would lift
        // the origin off the surface you aimed at (a wall at x = 2.5 would
        // start building 0.5 units inside or outside itself).
        let snapped = self.map_snap_world(hit);
        let origin = snapped - (normal * (snapped - hit).as_vec3().dot(normal)).as_dvec3();
        self.map_draw = Some(MapDraw {
            shape,
            phase: DrawPhase::Base,
            origin,
            u,
            v,
            normal,
            a: Vec2::ZERO,
            b: Vec2::ZERO,
            height: 0.0,
            turns: self.map_turns,
        });
        true
    }

    /// Per-frame update of the live draw gesture from the cursor.
    pub(crate) fn map_draw_update(&mut self, cursor: floptle_core::math::Vec2) {
        let Some(draw) = self.map_draw.as_ref() else { return };
        match draw.phase {
            DrawPhase::Base => {
                // Intersect the ray with the FIXED build plane (re-picking
                // geometry mid-drag would make the footprint jump).
                let Some((ro, rd)) = self.map_cursor_ray(cursor) else { return };
                let denom = rd.dot(draw.normal);
                if denom.abs() < 1e-6 {
                    return;
                }
                let t = (draw.origin - ro).as_vec3().dot(draw.normal) / denom;
                if t <= 0.0 {
                    return;
                }
                let p = ro + (rd * t).as_dvec3();
                let rel = (p - draw.origin).as_vec3();
                let b = self.map_snap_plane(floptle_core::math::Vec2::new(
                    rel.dot(draw.u),
                    rel.dot(draw.v),
                ));
                if let Some(d) = self.map_draw.as_mut() {
                    d.b = b;
                }
            }
            DrawPhase::Height => {
                // Height reads off the screen projection of the normal axis at
                // the footprint's center — the same math the move gizmo uses,
                // so it feels identical to dragging a handle.
                let Some(gpu) = self.gpu.as_ref() else { return };
                let (w, h) = (gpu.config.width as f32, gpu.config.height.max(1) as f32);
                let cam = self.camera.render_camera();
                let vp = cam.view_proj(w / h);
                let base = draw.point((draw.a + draw.b) * 0.5);
                let (Some(s0), Some(s1)) = (
                    crate::viz::project(base, cam.world_position, vp, w, h),
                    crate::viz::project(
                        base + draw.normal.as_dvec3(),
                        cam.world_position,
                        vp,
                        w,
                        h,
                    ),
                ) else {
                    return;
                };
                let dir = s1 - s0;
                let len2 = dir.length_squared();
                if len2 < 1e-6 {
                    return;
                }
                let mut units = (cursor - s0).dot(dir) / len2;
                if self.grid.snap {
                    let s = self.grid.size.max(0.01);
                    units = (units / s).round() * s;
                }
                if let Some(d) = self.map_draw.as_mut() {
                    d.height = units;
                }
            }
        }
    }

    /// LMB release during a draw: finish the footprint (flat shapes commit
    /// here, solids move on to the height phase).
    pub(crate) fn map_draw_release(&mut self) {
        let Some(draw) = self.map_draw.as_ref() else { return };
        if !draw.has_base() {
            // A click rather than a drag: nothing to build.
            self.map_draw = None;
            return;
        }
        if draw.shape.is_flat() {
            self.map_draw_commit();
        } else if let Some(d) = self.map_draw.as_mut() {
            d.phase = DrawPhase::Height;
        }
    }

    /// Build the drawn shape as a real node and keep the shape armed so the
    /// next drag draws another one.
    pub(crate) fn map_draw_commit(&mut self) {
        let Some(draw) = self.map_draw.take() else { return };
        if !draw.has_base() {
            return;
        }
        let mesh = draw.shape.sized(draw.half(), self.map_opts);
        let label = draw.shape.label();
        let readout = draw.readout(self.map_opts);
        self.spawn_map_node(label, mesh, Some(draw.transform()));
        if let Some(sel) = self.map_sel.as_mut() {
            sel.clear();
        }
        self.map_note(floptle_script::LogLevel::Debug, format!("{label} — {readout}"));
    }

    /// Escape / tool change: throw away the in-progress gesture.
    pub(crate) fn map_draw_cancel(&mut self) -> bool {
        self.map_draw.take().is_some()
    }

    /// Console feedback for the Map tool. Every op that declines to do
    /// something says WHY — a modeling tool that silently no-ops reads as
    /// broken.
    pub(crate) fn map_note(&mut self, level: floptle_script::LogLevel, msg: impl Into<String>) {
        self.console.push(level, format!("⬢ {}", msg.into()), None);
    }

    /// A discrete Map-tab / keyboard operation on the current selection.
    /// Each one is one undo step (whole-mesh snapshot before the op).
    pub(crate) fn apply_map_op(&mut self, op: MapOp) {
        if self.playing {
            self.map_note(
                floptle_script::LogLevel::Warn,
                "map editing is disabled during Play — press Stop first (in-Play edits would not be undoable and the collider would not rebuild)",
            );
            return;
        }
        let Some((entity, id)) = self.map_sync_sel() else {
            self.map_note(
                floptle_script::LogLevel::Warn,
                "select a map-mesh node first (click one in the viewport or the Hierarchy)",
            );
            return;
        };
        let Some(pre) = self.maps.meshes.get(&id).cloned() else { return };
        let extrude_dist =
            if self.grid.snap { self.grid.size.max(0.01) } else { self.map_opts.extrude };
        let (inset_amount, weld_eps, grid) =
            (self.map_opts.inset, self.map_opts.weld, self.grid.size.max(0.01));
        let mode = self.map_mode;
        let Some(sel) = self.map_sel.as_mut() else { return };
        let faces: Vec<u32> = sel.faces.iter().copied().collect();
        let Some(mesh) = self.maps.meshes.get_mut(&id) else { return };
        // `None` = the op ran; `Some(msg)` = it declined and here's why.
        let mut declined: Option<String> = None;
        let mut changed = true;
        // A pivot op moves the node to compensate for the geometry shift.
        let mut pivot_shift: Option<floptle_core::math::Vec3> = None;
        // Set when a reshape changed the face count and per-face material
        // assignments could not be carried over.
        let mut slots_reset = false;
        macro_rules! need_faces {
            () => {
                if faces.is_empty() {
                    declined = Some("select one or more FACES first (Tab switches sub-mode)".into());
                    changed = false;
                    Vec::new()
                } else {
                    faces.clone()
                }
            };
        }
        match op {
            MapOp::Extrude => {
                let f = need_faces!();
                if !f.is_empty() {
                    let moved = floptle_map::extrude_faces(mesh, &f, extrude_dist);
                    sel.clear();
                    sel.faces.extend(moved);
                }
            }
            MapOp::Inset => {
                let f = need_faces!();
                if !f.is_empty() {
                    let inner = floptle_map::inset_faces(mesh, &f, inset_amount);
                    if inner.is_empty() {
                        declined = Some("inset distance must be greater than zero".into());
                        changed = false;
                    } else {
                        sel.clear();
                        sel.faces.extend(inner);
                    }
                }
            }
            MapOp::DeleteFaces => {
                let f = need_faces!();
                if !f.is_empty() {
                    if f.len() >= mesh.faces.len() {
                        // Deleting every face would leave an invisible node
                        // with no way back except undo — delete the NODE.
                        declined = Some(
                            "that is every face — delete the node itself (Del in the Hierarchy) instead".into(),
                        );
                        changed = false;
                    } else {
                        floptle_map::delete_faces(mesh, &f);
                        sel.clear();
                    }
                }
            }
            MapOp::FlipFaces => {
                let f = need_faces!();
                if !f.is_empty() {
                    floptle_map::flip_faces(mesh, &f);
                }
            }
            MapOp::FlipAll => {
                let all: Vec<u32> = (0..mesh.faces.len() as u32).collect();
                if all.is_empty() {
                    declined = Some("this mesh has no faces".into());
                    changed = false;
                } else {
                    floptle_map::flip_faces(mesh, &all);
                }
            }
            MapOp::WeldSelected => {
                let verts: Vec<u32> = match mode {
                    MapSubMode::Vertex => sel.verts.iter().copied().collect(),
                    _ => sel.drag_verts(mesh).into_iter().collect(),
                };
                if verts.len() < 2 {
                    declined = Some("select at least two vertices to weld".into());
                    changed = false;
                } else {
                    let n = floptle_map::weld(mesh, &verts, weld_eps);
                    if n == 0 {
                        declined = Some(format!(
                            "nothing merged — no two selected verts are within {weld_eps:.3} of each other (raise the weld radius)"
                        ));
                        changed = false;
                    } else {
                        sel.clear();
                    }
                }
            }
            MapOp::Subdivide => {
                let f = need_faces!();
                if !f.is_empty() {
                    let created = floptle_map::subdivide_faces(mesh, &f);
                    sel.clear();
                    sel.faces.extend(created);
                }
            }
            MapOp::Bridge => {
                if faces.len() != 2 {
                    declined = Some("bridge needs exactly TWO faces selected".into());
                    changed = false;
                } else {
                    let walls = floptle_map::bridge_faces(mesh, faces[0], faces[1]);
                    if walls.is_empty() {
                        declined =
                            Some("those two faces have different corner counts — bridge needs a match".into());
                        changed = false;
                    } else {
                        sel.clear();
                        sel.faces.extend(walls);
                    }
                }
            }
            MapOp::SnapToGrid => {
                let verts: Vec<u32> = if sel.is_empty() {
                    (0..mesh.verts.len() as u32).collect()
                } else {
                    sel.drag_verts(mesh).into_iter().collect()
                };
                floptle_map::snap_verts(mesh, &verts, grid);
            }
            MapOp::Reshape(spec) => {
                if mesh.spec.is_none() {
                    declined = Some(
                        "this mesh has been edited, so it is no longer a plain shape — \
                         its parameters can't be changed any more (undo back past the \
                         edit, or draw a fresh one)"
                            .into(),
                    );
                    changed = false;
                } else {
                    let mut next = spec.build();
                    // Keep the material work: slot NAMES always, and the
                    // per-face assignment when the face count still lines up
                    // (a pure resize, or any knob that doesn't change topology).
                    next.slots = mesh.slots.clone();
                    if next.faces.len() == mesh.faces.len() {
                        for (n, o) in next.faces.iter_mut().zip(&mesh.faces) {
                            n.slot = o.slot;
                        }
                    } else {
                        slots_reset = next.faces.len() != mesh.faces.len();
                    }
                    *mesh = next;
                    sel.clear();
                }
            }
            MapOp::Resize(size) => {
                if mesh.bounds().is_none() {
                    declined = Some("this mesh has no geometry to resize".into());
                    changed = false;
                } else {
                    floptle_map::resize(mesh, size);
                }
            }
            MapOp::CenterPivot => {
                pivot_shift = Some(floptle_map::recenter(mesh));
            }
            MapOp::PivotToSelection => {
                let verts = sel.drag_verts(mesh);
                if verts.is_empty() {
                    declined = Some("select the sub-objects the origin should sit on".into());
                    changed = false;
                } else {
                    let c = verts
                        .iter()
                        .filter_map(|&v| mesh.verts.get(v as usize))
                        .copied()
                        .sum::<floptle_core::math::Vec3>()
                        / verts.len() as f32;
                    pivot_shift = Some(floptle_map::recenter_on(mesh, c));
                }
            }
            MapOp::AssignSlot(slot) => {
                let f = need_faces!();
                if !f.is_empty() {
                    floptle_map::set_face_slot(mesh, &f, slot);
                }
            }
            MapOp::AddSlot(ref name) | MapOp::MaterialFromSelection(ref name) => {
                let name = name.trim().to_string();
                if name.is_empty() {
                    declined = Some("give the material slot a name".into());
                    changed = false;
                } else if mesh.slots.contains(&name) {
                    declined = Some(format!("this mesh already has a slot called \"{name}\""));
                    changed = false;
                } else {
                    // The slot list is part of the mesh, so adding one IS an
                    // undoable change — banking it keeps undo in step with the
                    // per-slot material override that may key off the name.
                    mesh.slots.push(name.clone());
                    if !faces.is_empty() {
                        let slot = mesh.slots.len() as u16 - 1;
                        floptle_map::set_face_slot(mesh, &faces, slot);
                    }
                }
            }
            MapOp::SelectAll => {
                match mode {
                    MapSubMode::Vertex => sel.verts.extend(0..mesh.verts.len() as u32),
                    MapSubMode::Edge => sel.edges.extend(mesh.edges()),
                    MapSubMode::Face => sel.faces.extend(0..mesh.faces.len() as u32),
                }
                return; // selection-only: no geometry change, no undo snapshot
            }
            MapOp::SelectNone => {
                sel.clear();
                return;
            }
            MapOp::SelectInvert => {
                match mode {
                    MapSubMode::Vertex => {
                        let had = std::mem::take(&mut sel.verts);
                        sel.verts =
                            (0..mesh.verts.len() as u32).filter(|v| !had.contains(v)).collect();
                    }
                    MapSubMode::Edge => {
                        let had = std::mem::take(&mut sel.edges);
                        sel.edges = mesh.edges().into_iter().filter(|e| !had.contains(e)).collect();
                    }
                    MapSubMode::Face => {
                        let had = std::mem::take(&mut sel.faces);
                        sel.faces =
                            (0..mesh.faces.len() as u32).filter(|f| !had.contains(f)).collect();
                    }
                }
                return;
            }
            MapOp::Grow => {
                match mode {
                    MapSubMode::Face => {
                        let grown = floptle_map::grow_faces(mesh, &faces);
                        sel.faces.extend(grown);
                    }
                    _ => {
                        // Grow a vert/edge selection through the faces it
                        // touches, then convert back to the active mode.
                        let verts = sel.drag_verts(mesh);
                        let touching: Vec<u32> = (0..mesh.faces.len() as u32)
                            .filter(|&f| {
                                mesh.faces[f as usize].verts.iter().any(|v| verts.contains(v))
                            })
                            .collect();
                        sel.faces.extend(touching);
                        sel.convert(mesh, mode);
                    }
                }
                return;
            }
            MapOp::SelectConnected => {
                let seed: Vec<u32> = if faces.is_empty() {
                    let verts = sel.drag_verts(mesh);
                    (0..mesh.faces.len() as u32)
                        .filter(|&f| mesh.faces[f as usize].verts.iter().any(|v| verts.contains(v)))
                        .collect()
                } else {
                    faces.clone()
                };
                if seed.is_empty() {
                    declined = Some("select something to grow from first".into());
                } else {
                    sel.faces.extend(floptle_map::connected_faces(mesh, &seed));
                    sel.convert(mesh, mode);
                }
                changed = false;
            }
            MapOp::SelectCoplanar => {
                if faces.is_empty() {
                    declined = Some("select a face to spread across its flat region".into());
                } else {
                    sel.faces.extend(floptle_map::coplanar_faces(mesh, &faces, 1.0));
                    sel.convert(mesh, mode);
                }
                changed = false;
            }
            MapOp::SelectSlot(slot) => {
                sel.clear();
                sel.faces.extend(floptle_map::faces_with_slot(mesh, slot));
                sel.convert(mesh, mode);
                changed = false;
            }
            MapOp::SelectLoop => {
                if sel.edges.is_empty() {
                    declined = Some("select an EDGE first — loops run through quad junctions".into());
                } else {
                    let seeds: Vec<(u32, u32)> = sel.edges.iter().copied().collect();
                    for e in seeds {
                        sel.edges.extend(floptle_map::edge_loop(mesh, e));
                    }
                }
                changed = false;
            }
        }
        if changed && declined.is_none() {
            sel.prune(mesh);
            self.maps.dirty.insert(id);
            self.push_history(crate::Snapshot::MapMesh(id, pre));
            // A pivot move is one gesture across two stores: the geometry
            // snapshot above, and the node transform below (its own step would
            // let undo separate them and shift the node off its mesh).
            if let Some(shift) = pivot_shift
                && let Some(t) = self.world.get::<floptle_core::Transform>(entity).copied()
            {
                let world_shift = t.rotation * (t.scale * shift);
                if let Some(tm) = self.world.get_mut::<floptle_core::Transform>(entity) {
                    tm.translation += world_shift.as_dvec3();
                }
            }
            if slots_reset {
                self.map_note(
                    floptle_script::LogLevel::Debug,
                    "the new shape has a different face count, so per-face material \
                     assignments reset to the first slot (the slot names are kept)",
                );
            }
            if let MapOp::MaterialFromSelection(name) = &op {
                // Give the new slot a material of its own straight away —
                // otherwise "these faces are different" takes three more clicks
                // and looks like nothing happened.
                let mut om = self
                    .world
                    .get::<floptle_core::ObjectMaterials>(entity)
                    .cloned()
                    .unwrap_or_default();
                om.0.entry(name.clone()).or_insert_with(floptle_core::Material::default);
                self.world.insert(entity, om);
                self.scene_dirty = true;
                self.map_note(
                    floptle_script::LogLevel::Debug,
                    format!("{} face(s) now draw with the new \"{name}\" material", faces.len()),
                );
            }
        } else if let Some(msg) = declined {
            self.map_note(floptle_script::LogLevel::Warn, msg);
        }
    }

    /// Turn the selected map node by `quarters` * 90 degrees about its own up
    /// axis: `,` / `.` step it, Z half-turns it (for a staircase, "climb the
    /// other way"). A rotation rather than a re-generate, so it works even
    /// after the mesh has been edited and the node stays exactly where it is.
    pub(crate) fn map_turn(&mut self, quarters: i32) {
        if self.playing {
            self.map_note(floptle_script::LogLevel::Warn, "map editing is disabled during Play");
            return;
        }
        let Some((e, _)) = self.map_target() else {
            self.map_note(floptle_script::LogLevel::Warn, "select a map-mesh node first");
            return;
        };
        self.record();
        if let Some(t) = self.world.get_mut::<floptle_core::Transform>(e) {
            let q = floptle_core::math::Quat::from_rotation_y(
                quarters as f32 * std::f32::consts::FRAC_PI_2,
            );
            t.rotation = (t.rotation * q).normalize();
        }
    }

    /// The next key pressed while the Map tab is listening for a rebind.
    /// Escape cancels; anything the editor already owns (or another map
    /// command holds) is refused with the reason, so a conflicting binding
    /// can't be created in the first place.
    pub(crate) fn capture_map_rebind(&mut self, code: winit::keyboard::KeyCode) {
        use winit::keyboard::KeyCode as K;
        let Some(cmd) = self.map_rebind else { return };
        // Modifier keys on their own are the user reaching for a chord, not
        // the chord itself — keep listening.
        if matches!(
            code,
            K::ShiftLeft
                | K::ShiftRight
                | K::ControlLeft
                | K::ControlRight
                | K::AltLeft
                | K::AltRight
                | K::SuperLeft
                | K::SuperRight
        ) {
            return;
        }
        if code == K::Escape {
            self.map_rebind = None;
            self.map_rebind_err = None;
            return;
        }
        if self.ctrl {
            self.map_rebind_err =
                Some("Ctrl chords belong to the application (undo, save, copy…)".into());
            return;
        }
        let chord = crate::map_keys::Chord { key: code, shift: self.shift };
        match self.map_keys.set(cmd, chord) {
            Ok(()) => {
                crate::map_keys::save_map_keys(&self.map_keys);
                self.map_rebind = None;
                self.map_rebind_err = None;
                self.map_note(
                    floptle_script::LogLevel::Debug,
                    format!("{} is now {}", cmd.label(), chord.label()),
                );
            }
            Err(why) => self.map_rebind_err = Some(why),
        }
    }

    /// Run one bound Map command. Returns false when the command declined to
    /// consume the key, so the editor's own handler still gets it (delete-faces
    /// with nothing selected falls through to "delete node", which is the one
    /// key the map deliberately shares).
    pub(crate) fn run_map_command(&mut self, cmd: crate::map_keys::MapCmd) -> bool {
        use crate::map_keys::MapCmd as C;
        let shape_of = |c: C| match c {
            C::DrawBox => Some(MapShape::Box),
            C::DrawPlane => Some(MapShape::Plane),
            C::DrawWedge => Some(MapShape::Wedge),
            C::DrawCylinder => Some(MapShape::Cylinder),
            C::DrawSphere => Some(MapShape::Sphere),
            C::DrawStairs => Some(MapShape::Stairs),
            C::DrawArch => Some(MapShape::Arch),
            _ => None,
        };
        if let Some(shape) = shape_of(cmd) {
            self.map_draw = None;
            self.set_map_knife(false); // drawing and cutting both own the click
            // Pressing the armed shape's key again disarms it (a toggle, as in
            // every DCC).
            self.map_arm = if self.map_arm == Some(shape) { None } else { Some(shape) };
            return true;
        }
        match cmd {
            C::ResolutionDown => self.map_bump_resolution(-1),
            C::ResolutionUp => self.map_bump_resolution(1),
            C::TurnLeft => self.map_turn_input(-1),
            C::TurnRight => self.map_turn_input(1),
            C::TurnAround => self.map_turn_input(2),
            C::ModeCycle => self.set_map_mode(self.map_mode.next()),
            C::ModeVertex => self.set_map_mode(MapSubMode::Vertex),
            C::ModeEdge => self.set_map_mode(MapSubMode::Edge),
            C::ModeFace => self.set_map_mode(MapSubMode::Face),
            C::SelectAll => self.apply_map_op(MapOp::SelectAll),
            C::SelectNone => self.apply_map_op(MapOp::SelectNone),
            C::SelectInvert => self.apply_map_op(MapOp::SelectInvert),
            C::SelectGrow => self.apply_map_op(MapOp::Grow),
            C::SelectConnected => self.apply_map_op(MapOp::SelectConnected),
            C::SelectCoplanar => self.apply_map_op(MapOp::SelectCoplanar),
            C::SelectLoop => self.apply_map_op(MapOp::SelectLoop),
            C::ToggleSelectHidden => {
                self.map_select_hidden = !self.map_select_hidden;
                self.map_note(
                    floptle_script::LogLevel::Debug,
                    if self.map_select_hidden {
                        "selection now reaches sub-objects behind the surface"
                    } else {
                        "selection is limited to what you can see"
                    },
                );
            }
            C::GizmoCycle => {
                self.map_xform = match self.map_xform {
                    MapXform::Move => MapXform::Rotate,
                    MapXform::Rotate => MapXform::Scale,
                    MapXform::Scale => MapXform::Move,
                }
            }
            C::GizmoMove => self.map_xform = MapXform::Move,
            C::GizmoRotate => self.map_xform = MapXform::Rotate,
            C::GizmoScale => self.map_xform = MapXform::Scale,
            C::OrientCycle => {
                self.map_orient = match self.map_orient {
                    MapOrient::Normal => MapOrient::Local,
                    MapOrient::Local => MapOrient::Global,
                    MapOrient::Global => MapOrient::Normal,
                }
            }
            C::Extrude => self.apply_map_op(MapOp::Extrude),
            C::Inset => self.apply_map_op(MapOp::Inset),
            C::Subdivide => self.apply_map_op(MapOp::Subdivide),
            C::Bridge => self.apply_map_op(MapOp::Bridge),
            C::DeleteFaces => {
                // The shared key: with no face selection this is not ours.
                if self.map_sel.as_ref().is_none_or(|s| s.faces.is_empty()) {
                    return false;
                }
                self.apply_map_op(MapOp::DeleteFaces);
            }
            C::SplitOff => self.map_detach_selection(),
            C::Flip => self.apply_map_op(MapOp::FlipFaces),
            C::FlipAll => self.apply_map_op(MapOp::FlipAll),
            C::Weld => self.apply_map_op(MapOp::WeldSelected),
            C::SnapToGrid => self.apply_map_op(MapOp::SnapToGrid),
            C::Knife => {
                let on = !self.map_knife_on;
                self.set_map_knife(on);
            }
            C::CenterPivot => self.apply_map_op(MapOp::CenterPivot),
            C::PivotToSelection => self.apply_map_op(MapOp::PivotToSelection),
            C::NewMaterialFromSelection => {
                // Same as the Map tab's button, with its auto-generated name —
                // the tab's text field is for naming it deliberately.
                let n = self
                    .map_target()
                    .and_then(|(_, id)| self.maps.meshes.get(&id))
                    .map_or(1, |m| m.slots.len() + 1);
                self.apply_map_op(MapOp::MaterialFromSelection(format!("Material {n}")));
            }
            C::DrawBox
            | C::DrawPlane
            | C::DrawWedge
            | C::DrawCylinder
            | C::DrawSphere
            | C::DrawStairs
            | C::DrawArch => unreachable!("handled above"),
        }
        true
    }

    /// One `,` / `.` / Z press: turn whatever the tool is pointed at. While a
    /// shape is armed or being drawn that's the PREVIEW (and it sticks, so the
    /// next shape keeps the facing); otherwise it's the selected node.
    pub(crate) fn map_turn_input(&mut self, quarters: i32) {
        if self.map_draw.is_some() || self.map_arm.is_some() {
            self.map_turns = (self.map_turns + quarters).rem_euclid(4);
            if let Some(d) = self.map_draw.as_mut() {
                d.turns = (d.turns + quarters).rem_euclid(4);
            }
        } else {
            self.map_turn(quarters);
        }
    }

    /// `[` / `]`: step the active shape's resolution knob. While drawing (or
    /// with a shape armed) it retunes the preview; with a still-parametric map
    /// node selected it re-generates that node — so adding a stair step is one
    /// keypress on the thing you are looking at.
    pub(crate) fn map_bump_resolution(&mut self, delta: i32) {
        let bump = |v: &mut u32, lo: u32, hi: u32| {
            *v = (*v as i32 + delta).clamp(lo as i32, hi as i32) as u32;
        };
        let active = self.map_draw.as_ref().map(|d| d.shape).or(self.map_arm);
        if let Some(shape) = active {
            let o = &mut self.map_opts;
            match shape {
                MapShape::Stairs => bump(&mut o.steps, 1, 64),
                MapShape::Cylinder => bump(&mut o.sides, 3, 128),
                MapShape::Sphere => bump(&mut o.sides, 3, 128),
                MapShape::Arch => bump(&mut o.arch_segments, 2, 32),
                MapShape::Box | MapShape::Plane | MapShape::Wedge => {
                    self.map_note(
                        floptle_script::LogLevel::Debug,
                        format!("{} has no resolution to adjust", shape.label()),
                    );
                    return;
                }
            }
            let detail = shape.detail(self.map_opts);
            if let Some(d) = detail {
                self.map_note(floptle_script::LogLevel::Debug, d.replace("  [ ]", ""));
            }
            return;
        }
        // Nothing armed: retune the SELECTED shape in place.
        let Some((_, id)) = self.map_target() else { return };
        let Some(mut spec) = self.maps.meshes.get(&id).and_then(|m| m.spec) else {
            self.map_note(
                floptle_script::LogLevel::Warn,
                "this mesh has been edited, so its shape parameters are fixed",
            );
            return;
        };
        match spec.kind {
            floptle_map::ShapeKind::Stairs => bump(&mut spec.steps, 1, 64),
            floptle_map::ShapeKind::Cylinder | floptle_map::ShapeKind::Sphere => {
                bump(&mut spec.sides, 3, 128)
            }
            floptle_map::ShapeKind::Arch => bump(&mut spec.arch_segments, 2, 32),
            _ => {
                self.map_note(
                    floptle_script::LogLevel::Debug,
                    "this shape has no resolution to adjust",
                );
                return;
            }
        }
        self.apply_map_op(MapOp::Reshape(spec));
    }

    /// Split the selected faces into their own map node (same transform, so
    /// nothing moves), selecting the new node.
    pub(crate) fn map_detach_selection(&mut self) {
        if self.playing {
            self.map_note(floptle_script::LogLevel::Warn, "map editing is disabled during Play");
            return;
        }
        let Some((entity, id)) = self.map_sync_sel() else { return };
        let faces: Vec<u32> =
            self.map_sel.as_ref().map(|s| s.faces.iter().copied().collect()).unwrap_or_default();
        if faces.is_empty() {
            self.map_note(floptle_script::LogLevel::Warn, "select the faces to split off first");
            return;
        }
        let Some(pre) = self.maps.meshes.get(&id).cloned() else { return };
        let Some(mesh) = self.maps.meshes.get_mut(&id) else { return };
        let Some(part) = floptle_map::detach_faces(mesh, &faces) else {
            self.map_note(
                floptle_script::LogLevel::Warn,
                "that is the whole mesh — there would be nothing left behind",
            );
            return;
        };
        self.maps.dirty.insert(id);
        self.push_history(crate::Snapshot::MapMesh(id, pre));
        let name = self
            .world
            .get::<floptle_core::Name>(entity)
            .map(|n| format!("{} part", n.0))
            .unwrap_or_else(|| "Map part".into());
        let at = floptle_core::world_transform(&self.world, entity);
        let count = part.faces.len();
        self.spawn_map_node(&name, part, Some(at));
        if let Some(sel) = self.map_sel.as_mut() {
            sel.clear();
        }
        self.map_note(floptle_script::LogLevel::Debug, format!("split {count} face(s) into \"{name}\""));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::{Vec2, Vec3};

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join(format!("floptle-map-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("maps")).unwrap();
        d
    }

    /// A sidecar we could not read must NEVER be overwritten: the store is
    /// empty in that state, so a save would replace the whole level with
    /// nothing (and `sync_map_meshes` would have healed every node into a box).
    #[test]
    fn an_unreadable_sidecar_poisons_saving_instead_of_eating_the_level() {
        let dir = tmp_dir("poison");
        let mut ed =
            Editor { project_root: dir.clone(), scene_name: "level".into(), ..Default::default() };
        let path = ed.maps_file_path();
        std::fs::write(&path, "this is not RON").unwrap();

        ed.adopt_maps();
        assert!(ed.maps.load_failed, "a parse failure must poison the store");
        assert!(ed.maps.meshes.is_empty());

        // Whatever ends up in the store afterwards, the file stays untouched.
        ed.maps.meshes.insert(1, MapShape::Box.mesh(MapOpts::default()));
        ed.save_maps();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "this is not RON");

        // A healthy sidecar clears the poison and saves normally.
        std::fs::remove_file(&path).unwrap();
        ed.adopt_maps();
        assert!(!ed.maps.load_failed);
        ed.maps.meshes.insert(1, MapShape::Box.mesh(MapOpts::default()));
        ed.save_maps();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A missing sidecar is the normal "no map meshes yet" case and must not
    /// poison anything.
    #[test]
    fn a_missing_sidecar_is_not_a_failure() {
        let dir = tmp_dir("missing");
        let mut ed =
            Editor { project_root: dir.clone(), scene_name: "fresh".into(), ..Default::default() };
        ed.adopt_maps();
        assert!(!ed.maps.load_failed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Geometry carried inline by a prefab / clipboard doc must land in THIS
    /// scene's store under a fresh id — two pastes of the same doc are two
    /// independent meshes, and neither can hijack an existing node's id.
    #[test]
    fn inline_geometry_spawns_under_a_fresh_id_every_time() {
        let mut ed = Editor::default();
        // An id that is already taken here, to prove the doc's id is ignored.
        ed.maps.meshes.insert(0, MapShape::Box.mesh(MapOpts::default()));
        let geo = MapShape::Wedge.mesh(MapOpts::default());
        let doc = floptle_scene::NodeDoc {
            name: "Wall".into(),
            matter: MatterDoc::MapMesh { id: 0, geo: Some(geo.clone()) },
            ..blank_node()
        };
        let a = ed.spawn_node(&doc);
        let b = ed.spawn_node(&doc);
        let id_of = |e| match ed.world.get::<floptle_core::Matter>(e) {
            Some(floptle_core::Matter::MapMesh { id }) => *id,
            _ => panic!("not a map mesh"),
        };
        let (ia, ib) = (id_of(a), id_of(b));
        assert_ne!(ia, ib);
        assert_ne!(ia, 0);
        assert_eq!(ed.maps.meshes[&ia], geo);
        assert_eq!(ed.maps.meshes[&ib], geo);
        // The pre-existing entry is untouched.
        assert_eq!(ed.maps.meshes[&0].faces.len(), 6);
    }

    fn blank_node() -> floptle_scene::NodeDoc {
        floptle_scene::NodeDoc {
            id: None,
            parent_id: None,
            terrain_gen: None,
            name: String::new(),
            transform: Default::default(),
            matter: MatterDoc::Empty,
            scripts: Vec::new(),
            material: None,
            object_materials: Default::default(),
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
        }
    }

    /// The draw gesture's output: a footprint dragged on the ground plus a
    /// height becomes a correctly sized, correctly placed node.
    #[test]
    fn a_drawn_shape_is_sized_and_placed_from_the_gesture() {
        let draw = MapDraw {
            shape: MapShape::Box,
            phase: DrawPhase::Height,
            origin: floptle_core::math::DVec3::new(10.0, 0.0, -4.0),
            u: Vec3::X,
            v: Vec3::Z,
            normal: Vec3::Y,
            a: Vec2::ZERO,
            b: Vec2::new(4.0, 6.0),
            height: 3.0,
            turns: 0,
        };
        assert!(draw.has_base());
        assert_eq!(draw.half(), Vec3::new(2.0, 1.5, 3.0));
        let t = draw.transform();
        // Centered on the footprint, lifted half the height off the plane.
        assert!((t.translation - floptle_core::math::DVec3::new(12.0, 1.5, -1.0)).length() < 1e-9);
        // The mesh really measures what the readout said.
        let mesh = draw.shape.sized(draw.half(), MapOpts::default());
        let (lo, hi) = mesh.bounds().unwrap();
        assert!((hi - lo - Vec3::new(4.0, 3.0, 6.0)).length() < 1e-5);
        assert_eq!(draw.readout(MapOpts::default()), "4.00 x 3.00 x 6.00");
        // A drag that never left the press point builds nothing.
        let click = MapDraw { b: Vec2::ZERO, ..draw };
        assert!(!click.has_base());
    }

    /// A drawn shape is solid by default: blockout geometry you can walk
    /// through is never what anyone meant.
    #[test]
    fn drawn_shapes_are_collidable_and_parametric() {
        let mut ed = Editor::default();
        let mesh = MapShape::Stairs.mesh(MapOpts::default());
        let e = ed.spawn_map_node("Stairs", mesh, None).unwrap();
        assert!(
            ed.world.get::<floptle_core::Collidable>(e).is_some(),
            "map shapes must spawn solid"
        );
        let id = match ed.world.get::<floptle_core::Matter>(e) {
            Some(floptle_core::Matter::MapMesh { id }) => *id,
            _ => panic!("not a map mesh"),
        };
        assert_eq!(ed.maps.meshes[&id].spec.unwrap().kind, floptle_map::ShapeKind::Stairs);
    }

    /// New shapes come textured with the dev grid — and the reference is only
    /// ever written when the texture actually exists in the project (a dangling
    /// ref would render as untextured white and look like a bug).
    #[test]
    fn a_new_shape_gets_the_blockout_texture_or_none_at_all() {
        let dir = tmp_dir("tex");
        let mut ed =
            Editor { project_root: dir.clone(), scene_name: "s".into(), ..Default::default() };
        let mesh = MapShape::Box.mesh(MapOpts::default());
        let e = ed.spawn_map_node("Box", mesh, None).unwrap();
        let tex = ed.world.get::<floptle_core::Material>(e).and_then(|m| m.texture.clone());
        assert_eq!(
            tex.is_some(),
            dir.join(MAP_DEFAULT_TEXTURE).is_file(),
            "the reference and the file must arrive together"
        );
        if crate::export::repo_root().is_some() {
            // On a dev checkout the project gets seeded from the engine's copy.
            assert_eq!(tex.as_deref(), Some(MAP_DEFAULT_TEXTURE));
            assert!(dir.join(MAP_DEFAULT_TEXTURE).is_file());
        }
        // A headless Editor with no project touches nothing.
        let mut bare = Editor::default();
        let e2 = bare.spawn_map_node("Box", MapShape::Box.mesh(MapOpts::default()), None).unwrap();
        assert!(bare.world.get::<floptle_core::Material>(e2).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-generating a shape with new parameters keeps the material work:
    /// slot names always, per-face assignment when the topology still lines up.
    #[test]
    fn reshaping_keeps_the_material_slots() {
        let mut ed = Editor::default();
        let mut mesh = MapShape::Stairs.mesh(MapOpts::default());
        mesh.slots.push("Tread".into());
        floptle_map::set_face_slot(&mut mesh, &[3], 1);
        let e = ed.spawn_map_node("Stairs", mesh, None).unwrap();
        ed.selection = vec![e];
        let id = match ed.world.get::<floptle_core::Matter>(e) {
            Some(floptle_core::Matter::MapMesh { id }) => *id,
            _ => panic!(),
        };

        // Same topology (a pure resize): the assignment survives exactly.
        let mut spec = ed.maps.meshes[&id].spec.unwrap();
        spec.half *= 2.0;
        ed.apply_map_op(MapOp::Reshape(spec));
        assert_eq!(ed.maps.meshes[&id].slots, vec!["Default", "Tread"]);
        assert_eq!(ed.maps.meshes[&id].faces[3].slot, 1);

        // More steps changes the face count: names survive, assignment resets.
        spec.steps += 2;
        ed.apply_map_op(MapOp::Reshape(spec));
        let m = &ed.maps.meshes[&id];
        assert_eq!(m.slots, vec!["Default", "Tread"]);
        assert_eq!(m.faces.len(), 2 + (spec.steps as usize) * 4);
        assert_eq!(m.spec.unwrap().steps, spec.steps);

        // Undo walks back through both reshapes.
        ed.undo();
        assert_eq!(ed.maps.meshes[&id].faces.len(), 2 + 8 * 4);

        // Once the geometry is edited, reshaping is refused rather than
        // silently throwing the edit away.
        ed.apply_map_op(MapOp::SelectAll);
        ed.apply_map_op(MapOp::Subdivide);
        let faces = ed.maps.meshes[&id].faces.len();
        assert!(ed.maps.meshes[&id].spec.is_none());
        ed.apply_map_op(MapOp::Reshape(spec));
        assert_eq!(ed.maps.meshes[&id].faces.len(), faces, "reshape must decline");
    }

    /// Turning a placed node: `,` / `.` are quarter turns about its own up
    /// axis and Z is a half turn — the node never moves, and four quarter
    /// turns land exactly back where they started.
    #[test]
    fn turning_a_node_spins_it_in_place() {
        let mut ed = Editor::default();
        let mesh = MapShape::Stairs.mesh(MapOpts::default());
        let e = ed.spawn_map_node("Stairs", mesh, None).unwrap();
        ed.selection = vec![e];
        let start = floptle_core::world_transform(&ed.world, e);
        let rise = |t: &floptle_core::Transform| t.rotation * Vec3::NEG_Z;
        let up = |t: &floptle_core::Transform| t.rotation * Vec3::Y;

        // A half turn reverses the climb, in place.
        ed.map_turn(2);
        let flipped = floptle_core::world_transform(&ed.world, e);
        assert!(rise(&start).dot(rise(&flipped)) < -0.999, "the climb must reverse");
        assert!((start.translation - flipped.translation).length() < 1e-9);
        assert!(up(&flipped).dot(up(&start)) > 0.999, "up is unchanged");

        // Quarter turns are perpendicular, and `.` is the opposite of `,`.
        ed.map_turn(2); // back to the start
        ed.map_turn(1);
        let right = floptle_core::world_transform(&ed.world, e);
        assert!(rise(&right).dot(rise(&start)).abs() < 1e-3, "a quarter turn is 90 degrees");
        ed.map_turn(-2);
        let left = floptle_core::world_transform(&ed.world, e);
        assert!(rise(&left).dot(rise(&right)) < -0.999, ", and . must oppose each other");

        // Four quarters is identity.
        ed.map_turn(1);
        ed.map_turn(4);
        let back = floptle_core::world_transform(&ed.world, e);
        assert!(rise(&back).dot(rise(&start)) > 0.999);
        assert!((back.translation - start.translation).length() < 1e-9);
    }

    /// A quarter turn while drawing spins the shape INSIDE the footprint you
    /// dragged: it re-fits (X/Z extents swap) instead of poking out of it.
    #[test]
    fn a_quarter_turn_refits_the_drawn_footprint() {
        let draw = MapDraw {
            shape: MapShape::Box,
            phase: DrawPhase::Height,
            origin: floptle_core::math::DVec3::ZERO,
            u: Vec3::X,
            v: Vec3::Z,
            normal: Vec3::Y,
            a: Vec2::ZERO,
            b: Vec2::new(2.0, 8.0),
            height: 2.0,
            turns: 0,
        };
        assert_eq!(draw.half(), Vec3::new(1.0, 1.0, 4.0));
        let turned = MapDraw { turns: 1, ..draw };
        assert_eq!(turned.half(), Vec3::new(4.0, 1.0, 1.0), "odd turns swap X and Z");
        assert_eq!(MapDraw { turns: 2, ..draw }.half(), draw.half());
        // The world footprint is unchanged: local X now runs along the drag's
        // long axis, and the mesh still measures 2 x 8 on the ground.
        let t = turned.transform();
        let mesh = turned.shape.sized(turned.half(), MapOpts::default());
        let (lo, hi) = mesh.bounds().unwrap();
        let corners = [
            t.rotation * Vec3::new(lo.x, 0.0, lo.z),
            t.rotation * Vec3::new(hi.x, 0.0, hi.z),
        ];
        let span = (corners[1] - corners[0]).abs();
        assert!((span.x - 2.0).abs() < 1e-4 && (span.z - 8.0).abs() < 1e-4, "{span:?}");
        // Still right-handed, still standing on the plane normal.
        let (x, y, z) = (t.rotation * Vec3::X, t.rotation * Vec3::Y, t.rotation * Vec3::Z);
        assert!((x.cross(y) - z).length() < 1e-5);
        assert!((y - Vec3::Y).length() < 1e-5);
    }

    /// Z reverses the draw direction on top of whatever the drag implied.
    #[test]
    fn the_flip_key_reverses_the_drawn_direction() {
        let draw = MapDraw {
            shape: MapShape::Stairs,
            phase: DrawPhase::Height,
            origin: floptle_core::math::DVec3::ZERO,
            u: Vec3::X,
            v: Vec3::Z,
            normal: Vec3::Y,
            a: Vec2::ZERO,
            b: Vec2::new(2.0, 4.0),
            height: 2.0,
            turns: 0,
        };
        let flipped = MapDraw { turns: 2, ..draw };
        assert!(draw.rise_dir().dot(flipped.rise_dir()) < -0.999);
        // Still right-handed after the flip (a mirror would invert the winding).
        let t = flipped.transform();
        let (x, y, z) = (t.rotation * Vec3::X, t.rotation * Vec3::Y, t.rotation * Vec3::Z);
        assert!((x.cross(y) - z).length() < 1e-5);
        assert!((y - Vec3::Y).length() < 1e-5);
    }

    /// Stairs and ramps must climb the way you dragged them, and the basis has
    /// to stay right-handed doing it (a mirrored basis inverts the winding and
    /// renders the shape inside out).
    #[test]
    fn asymmetric_shapes_rise_along_the_drag() {
        let base = MapDraw {
            shape: MapShape::Stairs,
            phase: DrawPhase::Height,
            origin: floptle_core::math::DVec3::ZERO,
            u: Vec3::X,
            v: Vec3::Z,
            normal: Vec3::Y,
            a: Vec2::ZERO,
            b: Vec2::new(2.0, 4.0),
            height: 2.0,
            turns: 0,
        };
        for b in [Vec2::new(2.0, 4.0), Vec2::new(2.0, -4.0)] {
            let draw = MapDraw { b, ..base };
            let t = draw.transform();
            let drag = draw.v * (b.y - draw.a.y).signum();
            // Local -Z (the tall end of stairs/wedge) points along the drag.
            assert!(
                (t.rotation * Vec3::NEG_Z).dot(drag) > 0.999,
                "the rise must follow the drag direction"
            );
            let (x, y, z) = (t.rotation * Vec3::X, t.rotation * Vec3::Y, t.rotation * Vec3::Z);
            assert!((x.cross(y) - z).length() < 1e-5, "basis went left-handed");
            assert!((y - draw.normal).length() < 1e-5);
        }
    }

    /// Drawing onto a wall keeps the shape square to that wall (the basis has
    /// to stay right-handed or the geometry comes out mirrored).
    #[test]
    fn a_build_plane_on_a_wall_stays_right_handed() {
        let normal = Vec3::X;
        let u = Vec3::Y.cross(normal).normalize();
        let draw = MapDraw {
            shape: MapShape::Box,
            phase: DrawPhase::Base,
            origin: floptle_core::math::DVec3::ZERO,
            u,
            v: u.cross(normal),
            normal,
            a: Vec2::ZERO,
            b: Vec2::new(2.0, 2.0),
            height: 1.0,
            turns: 0,
        };
        let t = draw.transform();
        assert!((t.rotation * Vec3::Y).dot(normal) > 0.999, "local +Y must be the plane normal");
        assert!(t.rotation.is_normalized());
        // Right-handed: X cross Y == Z.
        let (x, y, z) = (t.rotation * Vec3::X, t.rotation * Vec3::Y, t.rotation * Vec3::Z);
        assert!((x.cross(y) - z).length() < 1e-5);
    }

    /// "Select every face" has to mean the mode you are IN, and inverting has
    /// to be the exact complement of it — including the empty and full cases,
    /// which are the two people actually reach for.
    #[test]
    fn select_all_and_invert_answer_in_the_current_mode() {
        let mut ed = Editor::default();
        let e = ed.spawn_map_node("Box", MapShape::Box.mesh(MapOpts::default()), None).unwrap();
        ed.selection = vec![e];
        let id = match ed.world.get::<floptle_core::Matter>(e) {
            Some(floptle_core::Matter::MapMesh { id }) => *id,
            _ => panic!("not a map node"),
        };
        let (nf, nv, ne) = {
            let m = &ed.maps.meshes[&id];
            (m.faces.len(), m.verts.len(), m.edges().len())
        };

        // Invert from nothing = everything, in whichever mode is live.
        ed.apply_map_op(MapOp::SelectInvert);
        assert_eq!(ed.map_sel.as_ref().unwrap().faces.len(), nf);
        // …and again = nothing.
        ed.apply_map_op(MapOp::SelectInvert);
        assert!(ed.map_sel.as_ref().unwrap().is_empty());

        for (mode, want) in
            [(MapSubMode::Vertex, nv), (MapSubMode::Edge, ne), (MapSubMode::Face, nf)]
        {
            ed.set_map_mode(mode);
            ed.apply_map_op(MapOp::SelectNone);
            ed.apply_map_op(MapOp::SelectAll);
            let s = ed.map_sel.as_ref().unwrap();
            let got = match mode {
                MapSubMode::Vertex => s.verts.len(),
                MapSubMode::Edge => s.edges.len(),
                MapSubMode::Face => s.faces.len(),
            };
            assert_eq!(got, want, "select-all in {mode:?}");
            // The complement of everything is nothing.
            ed.apply_map_op(MapOp::SelectInvert);
            assert!(ed.map_sel.as_ref().unwrap().is_empty(), "invert-all in {mode:?}");
        }

        // A partial selection inverts to exactly the rest.
        ed.set_map_mode(MapSubMode::Face);
        ed.apply_map_op(MapOp::SelectNone);
        ed.map_sel.as_mut().unwrap().faces.insert(2);
        ed.apply_map_op(MapOp::SelectInvert);
        let s = ed.map_sel.as_ref().unwrap();
        assert_eq!(s.faces.len(), nf - 1);
        assert!(!s.faces.contains(&2));
    }

    /// Shift adds, Ctrl removes — and a plain click replaces. The modifiers are
    /// read in ONE place, so what a click does and what a box does can't drift
    /// apart.
    #[test]
    fn shift_adds_and_ctrl_removes() {
        assert_eq!(SelectMode::of(false, false), SelectMode::Replace);
        assert_eq!(SelectMode::of(true, false), SelectMode::Add);
        assert_eq!(SelectMode::of(false, true), SelectMode::Subtract);
        // Ctrl wins when both are held: "remove these" is the more specific ask.
        assert_eq!(SelectMode::of(true, true), SelectMode::Subtract);
        assert!(!SelectMode::Replace.keeps_existing());
        assert!(SelectMode::Add.keeps_existing());
        assert!(SelectMode::Subtract.keeps_existing());
    }

    /// Every sub-mode is reachable by its own key and says so — the direct
    /// binds existed before this and nothing in the UI mentioned them.
    #[test]
    fn every_sub_mode_has_its_own_key() {
        let keys = crate::map_keys::MapKeys::default();
        let mut seen = Vec::new();
        for mode in MapSubMode::ALL {
            let c = keys.chord(mode.cmd()).expect("bound");
            assert!(!seen.contains(&c), "{mode:?} shares a chord");
            seen.push(c);
            assert!(!mode.glyph().is_empty());
            assert!(!mode.plural().is_empty());
        }
    }

    /// Tab must CONVERT the selection, not drop it.
    #[test]
    fn sub_mode_switches_convert_the_selection() {
        let mesh = MapShape::Box.mesh(MapOpts::default());
        let mut sel = MapSel::new(floptle_core::World::new().spawn(), 0);
        sel.faces.insert(0);
        sel.convert(&mesh, MapSubMode::Vertex);
        assert_eq!(sel.verts.len(), 4);
        assert!(sel.faces.is_empty());
        sel.convert(&mesh, MapSubMode::Edge);
        assert_eq!(sel.edges.len(), 4);
        sel.convert(&mesh, MapSubMode::Face);
        assert_eq!(sel.faces.iter().copied().collect::<Vec<_>>(), vec![0]);
        // An empty selection stays empty (and doesn't select the whole mesh).
        let mut empty = MapSel::new(floptle_core::World::new().spawn(), 0);
        empty.convert(&mesh, MapSubMode::Face);
        assert!(empty.is_empty());
    }

    /// Undoing an op that removed geometry must not leave the selection
    /// pointing at faces that no longer exist.
    #[test]
    fn a_restored_mesh_prunes_a_stale_selection() {
        let mut mesh = MapShape::Box.mesh(MapOpts::default());
        floptle_map::subdivide_faces(&mut mesh, &[0, 1, 2, 3, 4, 5]);
        let mut sel = MapSel::new(floptle_core::World::new().spawn(), 0);
        sel.faces.insert(20);
        sel.verts.insert(25);
        sel.prune(&mesh);
        assert_eq!(sel.faces.len(), 1);
        sel.prune(&MapShape::Box.mesh(MapOpts::default()));
        assert!(sel.is_empty(), "indices past the restored mesh must go");
    }

    /// The sidecar is RON of `BTreeMap<u32, MapMesh>` — guard the round trip
    /// (glam's serde + ron's non-string map keys both have to keep working).
    #[test]
    fn map_sidecar_round_trips() {
        let mut m = MapShape::Stairs.mesh(MapOpts::default());
        m.slots.push("Wall".into());
        floptle_map::set_face_slot(&mut m, &[0, 1], 1);
        let mut store: BTreeMap<u32, MapMesh> = BTreeMap::new();
        store.insert(3, m);
        store.insert(7, MapShape::Arch.mesh(MapOpts::default()));
        let text =
            ron::ser::to_string_pretty(&store, ron::ser::PrettyConfig::new().depth_limit(3))
                .unwrap();
        let back: BTreeMap<u32, MapMesh> = ron::from_str(&text).unwrap();
        assert_eq!(store, back);
        for mesh in back.values() {
            mesh.validate().unwrap();
        }
    }
}
