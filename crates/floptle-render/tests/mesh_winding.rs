//! Every built-in shape must be wound to agree with its own normals.
//!
//! Nothing culls back faces in this renderer, so a wrong winding does not make
//! geometry disappear — it makes `facing_normal` (raster.wgsl) decide the
//! surface is being seen from behind and flip the shading normal. The shape then
//! lights **inside out**: dark where the light is, a bright rim on the
//! silhouette. It reads as a strange material rather than as a bug, which is why
//! it survived until the `pbr_probe` measured a normal map tilting the wrong way
//! and the scan behind this test found `cube` correct and every other shape
//! wrong — `uv_sphere` and `capsule` fully inverted, `pyramid`, `cone` and
//! `cylinder` inverted in part, so one shape lit from both sides at once.
//!
//! `orient_faces` fixes it from the data at build time. This is the test that
//! keeps it fixed, and that a NEW shape arrives correct.

use floptle_render::mesh::*;
use glam::Vec3;

/// Triangles whose winding agrees with their vertex normals, and those that
/// don't. Degenerate triangles (a UV sphere's pole rows) have no winding and are
/// counted as neither.
fn tally(m: &MeshData) -> (usize, usize) {
    let mut agree = 0;
    let mut against = 0;
    for t in m.indices.as_chunks::<3>().0 {
        let p = |i: u32| Vec3::from(m.vertices[i as usize].pos);
        let n: Vec3 = t.iter().map(|&i| Vec3::from(m.vertices[i as usize].normal)).sum();
        let cross = (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0]));
        if cross.length_squared() < 1e-12 {
            continue;
        }
        if cross.dot(n) > 0.0 {
            agree += 1;
        } else {
            against += 1;
        }
    }
    (agree, against)
}

#[test]
fn every_built_in_shape_is_wound_to_match_its_normals() {
    let shapes: Vec<(&str, MeshData)> = vec![
        ("cube", cube(1.0)),
        ("uv_sphere", uv_sphere(1.0, 12, 18)),
        ("capsule", capsule(1.0, 1.0, 8, 16)),
        ("plane", plane(1.0)),
        ("pyramid", pyramid(1.0, 2.0)),
        ("cone", cone(1.0, 2.0, 16)),
        ("cylinder", cylinder(1.0, 1.0, 16)),
        ("tilemap", tilemap(4, 3, 1.0, 2, 2, [0.5, 0.5], &[0, 1, 2, 3, 0, 1, 2, 3, 0, 1, 2, 3])),
    ];
    for (name, m) in shapes {
        let (agree, against) = tally(&m);
        assert!(agree > 0, "{name} produced no non-degenerate triangles to check");
        assert_eq!(
            against, 0,
            "{name} has {against} triangle(s) wound AGAINST their own normals (of \
             {} that have a winding). Those faces light inside out — dark toward the \
             light, bright on the silhouette. Build the shape through `oriented`.",
            agree + against
        );
    }
}
