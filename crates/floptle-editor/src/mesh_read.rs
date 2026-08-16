//! Reading a node's or an asset's triangles, for a package.
//!
//! `mesh.read` in an editor extension. A tool that exports a level, measures a
//! volume, uploads geometry for analysis, or bakes anything of its own needs the
//! actual triangles, and until this the only shape a package could see was a
//! bounding box.
//!
//! ## Where the geometry comes from
//!
//! The same places [`crate::nav_bake`]'s gather reads it, and deliberately by
//! the same calls: `gltf_import::geometry` for a model,
//! `floptle_map::triangulate` for a map mesh, `matter_catalog::primitive_mesh`
//! for a built-in shape. Two readers that disagreed about what a node's
//! geometry is would be the [`two-gathers-must-agree`] shape all over again —
//! a package would measure one thing and the navmesh another, and both would
//! look right on their own.
//!
//! ## Local space, not world
//!
//! Positions come back in the node's own space, which is what a mesh file
//! holds and what an exporter wants. `scene.info(id)` carries the transform for
//! anybody who needs to place them. Returning world space would bake the
//! current transform into data a tool might be about to save.
//!
//! ## Flat arrays
//!
//! `positions` is `{x, y, z, x, y, z, …}` rather than a table per vertex. A
//! table per vertex costs one of LuaJIT's ~8000 registry slots each and
//! `create_table` **panics** when they run out — the same reason `nav.areas` is
//! flat. A hundred-thousand-vertex model would take the editor down.

use floptle_core::{Entity, Matter, World};
use floptle_render::MeshData;

/// How many triangles one read may return.
///
/// A limit rather than no limit because this crosses into Lua: a million
/// triangles is twelve million numbers, and a package asking for a whole
/// terrain by accident should get a sentence rather than a stalled editor.
pub(crate) const MAX_TRIANGLES: usize = 400_000;

/// Triangles, ready to hand to Lua.
#[derive(Debug)]
pub(crate) struct Geometry {
    /// `{x, y, z}` per vertex, flattened.
    pub(crate) positions: Vec<f32>,
    /// `{x, y, z}` per vertex, flattened. Empty where the source has none.
    pub(crate) normals: Vec<f32>,
    /// `{u, v}` per vertex, flattened. Empty where the source has none.
    pub(crate) uvs: Vec<f32>,
    /// Triangle corners, **zero-based** — the convention every mesh format and
    /// every consumer of one uses. Lua's 1-based tables are the odd ones out
    /// here, and converting would make `positions[indices[i] * 3 + 1]` wrong in
    /// a way nobody would spot.
    pub(crate) indices: Vec<u32>,
    /// What was read, for a tool that wants to say so: `"model"`, `"map"`,
    /// `"primitive"`.
    pub(crate) source: &'static str,
}

impl Geometry {
    pub(crate) fn vertex_count(&self) -> usize {
        self.positions.len() / 3
    }
    pub(crate) fn triangle_count(&self) -> usize {
        self.indices.len() / 3
    }

    fn from_meshes<'a>(
        parts: impl Iterator<Item = &'a MeshData>,
        source: &'static str,
    ) -> Result<Self, String> {
        let mut g = Geometry {
            positions: Vec::new(),
            normals: Vec::new(),
            uvs: Vec::new(),
            indices: Vec::new(),
            source,
        };
        for part in parts {
            // Every part is appended into one buffer, so the indices of the
            // second part have to move past the first. A model with two
            // materials is two parts, and forgetting this draws the second one
            // on top of the first.
            let base = (g.positions.len() / 3) as u32;
            for v in &part.vertices {
                g.positions.extend_from_slice(&v.pos);
                g.normals.extend_from_slice(&v.normal);
                g.uvs.extend_from_slice(&v.uv);
            }
            g.indices.extend(part.indices.iter().map(|i| i + base));
            if g.triangle_count() > MAX_TRIANGLES {
                return Err(format!(
                    "that is more than {MAX_TRIANGLES} triangles — read it in pieces, or \
                     use scene.bounds if a box will do"
                ));
            }
        }
        Ok(g)
    }
}

/// What a package asked to read.
pub(crate) enum MeshSource {
    /// A node in the open scene, by the id `scene.*` uses.
    Node(u32),
    /// A model file, project-relative or `pkg://`.
    Asset(String),
}

/// Read a model file's triangles.
pub(crate) fn read_asset(project_root: &std::path::Path, rel: &str) -> Result<Geometry, String> {
    let path = crate::project::resolve_asset_path(project_root, rel);
    // The memoised geometry-only import from `floptle/0140`: no textures are
    // decoded, and a second read of the same file is a hash lookup.
    let model = floptle_assets::gltf_import::geometry(&path)
        .map_err(|e| format!("{rel}: {e}"))?;
    Geometry::from_meshes(model.parts.iter().map(|p| &p.mesh), "model")
}

/// Read a node's triangles, in its own space.
///
/// `maps` is the editor's map-geometry store, which a map node keys into.
pub(crate) fn read_node(
    world: &World,
    e: Entity,
    project_root: &std::path::Path,
    maps: &std::collections::HashMap<u32, floptle_map::MapMesh>,
) -> Result<Geometry, String> {
    match world.get::<Matter>(e) {
        Some(Matter::Mesh { asset_path }) => read_asset(project_root, &asset_path.clone()),
        Some(Matter::Primitive { shape, .. }) => {
            let mesh = crate::matter_catalog::primitive_mesh(*shape);
            Geometry::from_meshes(std::iter::once(&mesh), "primitive")
        }
        Some(Matter::MapMesh { id }) => {
            let Some(mesh) = maps.get(id) else {
                return Err("that map node has no geometry in this scene".into());
            };
            let mut g = Geometry {
                positions: Vec::new(),
                normals: Vec::new(),
                uvs: Vec::new(),
                indices: Vec::new(),
                source: "map",
            };
            for sm in floptle_map::triangulate(mesh) {
                let base = (g.positions.len() / 3) as u32;
                for p in &sm.positions {
                    g.positions.extend_from_slice(p);
                }
                g.indices.extend(sm.indices.iter().map(|i| i + base));
                if g.triangle_count() > MAX_TRIANGLES {
                    return Err(format!("that map is more than {MAX_TRIANGLES} triangles"));
                }
            }
            Ok(g)
        }
        // Terrain is a FIELD, not a mesh: it is meshed on demand, per chunk, at
        // whatever detail the camera wants, and "the terrain's triangles" is
        // not a question with one answer. Said plainly rather than returning
        // some arbitrary level of detail a tool would treat as the truth.
        Some(Matter::Terrain { .. }) => Err(
            "terrain has no fixed triangles — it is meshed per chunk at the detail it is \
             viewed at. Sample it with scene.raycast instead"
                .into(),
        ),
        Some(other) => Err(format!(
            "a {} node has no geometry to read",
            crate::ext::scene_mirror::kind_name(other)
        )),
        None => Err("that node is gone".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world_with_primitive() -> (World, Entity) {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Box".into()));
        w.insert(
            e,
            Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0, 1.0, 1.0] },
        );
        (w, e)
    }

    #[test]
    fn a_primitive_reads_as_triangles() {
        let (w, e) = world_with_primitive();
        let g = read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap();
        assert_eq!(g.source, "primitive");
        assert_eq!(g.triangle_count(), 12, "a cube is twelve triangles");
        assert_eq!(g.positions.len() % 3, 0);
        assert_eq!(g.normals.len(), g.positions.len(), "one normal per position");
        assert_eq!(g.uvs.len() / 2, g.vertex_count(), "one uv per vertex");
    }

    /// Indices are zero-based and address the flat position array. Off by one
    /// here and every consumer silently reads the wrong corner.
    #[test]
    fn every_index_addresses_a_real_vertex() {
        let (w, e) = world_with_primitive();
        let g = read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap();
        let n = g.vertex_count() as u32;
        assert!(g.indices.iter().all(|&i| i < n), "an index past the end");
        assert!(g.indices.contains(&0), "zero-based, so 0 must appear");
    }

    /// The geometry a package reads has to be the geometry the navmesh bakes,
    /// or a tool measures one shape and the level paths on another. Same call,
    /// checked rather than assumed.
    #[test]
    fn a_package_reads_the_same_primitive_geometry_the_navmesh_bakes() {
        let (w, e) = world_with_primitive();
        let g = read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap();
        let baked = crate::matter_catalog::primitive_mesh(floptle_core::Shape::Cube);
        assert_eq!(g.vertex_count(), baked.vertices.len());
        assert_eq!(g.indices.len(), baked.indices.len());
        for (i, v) in baked.vertices.iter().enumerate() {
            assert_eq!(&g.positions[i * 3..i * 3 + 3], &v.pos, "vertex {i} moved");
        }
    }

    #[test]
    fn a_node_with_no_geometry_says_so_rather_than_answering_nothing() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Light".into()));
        w.insert(e, Matter::PointLight { color: [1.0; 3], intensity: 1.0, range: 10.0,
                                        shadows: Default::default(), shape: Default::default(),
                                        spot_angle: floptle_core::OMNI_ANGLE, spot_softness: 0.25 });
        let err = read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap_err();
        assert!(err.contains("pointLight"), "{err}");
    }

    #[test]
    fn terrain_says_why_it_cannot_be_read_as_a_mesh() {
        let mut w = World::new();
        let e = w.spawn();
        w.insert(e, floptle_core::Name("Ground".into()));
        w.insert(e, Matter::Terrain { id: 0 });
        let err = read_node(&w, e, std::path::Path::new("."), &Default::default()).unwrap_err();
        assert!(err.contains("per chunk"), "{err}");
        assert!(err.contains("raycast"), "it has to say what to do instead: {err}");
    }

    /// Two parts appended into one buffer: the second part's indices have to
    /// move past the first, or it draws on top of it.
    #[test]
    fn a_model_of_two_parts_offsets_the_second_parts_indices() {
        let a = floptle_render::cube(0.5);
        let b = floptle_render::cube(0.5);
        let n = a.vertices.len() as u32;
        let g = Geometry::from_meshes([&a, &b].into_iter(), "model").unwrap();
        assert_eq!(g.vertex_count(), (n * 2) as usize);
        assert!(
            g.indices[a.indices.len()..].iter().all(|&i| i >= n),
            "the second part's indices still point at the first part's vertices"
        );
        assert!(g.indices.iter().all(|&i| i < n * 2));
    }
}
