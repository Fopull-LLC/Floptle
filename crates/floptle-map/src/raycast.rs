//! Object-local ray picking over the face set (fan triangulation, Möller–
//! Trumbore, no acceleration structure — map meshes are hundreds of faces,
//! and the editor casts one ray per frame).

use crate::{face_normal, MapMesh};
use glam::Vec3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FaceHit {
    pub face: u32,
    pub t: f32,
    /// Hit position in object-local space.
    pub pos: Vec3,
    /// Face normal (not interpolated — faces are flat).
    pub normal: Vec3,
}

/// Both-sided Möller–Trumbore; `t` in units of `rd`'s length.
fn ray_tri(ro: Vec3, rd: Vec3, a: Vec3, b: Vec3, c: Vec3) -> Option<f32> {
    let (e1, e2) = (b - a, c - a);
    let p = rd.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-7 {
        return None;
    }
    let inv = 1.0 / det;
    let s = ro - a;
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = rd.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = e2.dot(q) * inv;
    (t >= 0.0).then_some(t)
}

/// Nearest face hit along `ro + t*rd` with `0 <= t <= max_t`, or `None`.
/// Backfaces DO hit (a map builder often works from inside a room).
/// `rd` need not be normalized (`t` is in units of `rd`'s length).
pub fn raycast(mesh: &MapMesh, ro: Vec3, rd: Vec3, max_t: f32) -> Option<FaceHit> {
    let mut best: Option<(u32, f32)> = None;
    for (fi, f) in mesh.faces.iter().enumerate() {
        if f.verts.len() < 3 || f.verts.iter().any(|&v| v as usize >= mesh.verts.len()) {
            continue;
        }
        let a = mesh.verts[f.verts[0] as usize];
        for i in 1..f.verts.len() - 1 {
            let b = mesh.verts[f.verts[i] as usize];
            let c = mesh.verts[f.verts[i + 1] as usize];
            if let Some(t) = ray_tri(ro, rd, a, b, c)
                && t <= max_t
                && best.is_none_or(|(_, bt)| t < bt)
            {
                best = Some((fi as u32, t));
            }
        }
    }
    best.map(|(face, t)| FaceHit {
        face,
        t,
        pos: ro + rd * t,
        normal: face_normal(mesh, &mesh.faces[face as usize]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::box_mesh;
    use glam::Vec3;

    #[test]
    fn hits_the_facing_face_from_outside() {
        let m = box_mesh(Vec3::ONE);
        let hit = raycast(&m, Vec3::new(0.2, 0.3, 5.0), Vec3::NEG_Z, 100.0).unwrap();
        assert!((hit.t - 4.0).abs() < 1e-4);
        assert!(hit.normal.z > 0.99); // the +Z face
        assert!((hit.pos - Vec3::new(0.2, 0.3, 1.0)).length() < 1e-4);
    }

    #[test]
    fn hits_backfaces_from_inside() {
        let m = box_mesh(Vec3::ONE);
        let hit = raycast(&m, Vec3::ZERO, Vec3::X, 100.0).unwrap();
        assert!((hit.t - 1.0).abs() < 1e-4);
        assert!(hit.normal.x > 0.99); // the +X face, hit from behind
    }

    #[test]
    fn respects_max_t_and_misses() {
        let m = box_mesh(Vec3::ONE);
        assert!(raycast(&m, Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z, 3.0).is_none());
        assert!(raycast(&m, Vec3::new(5.0, 0.0, 5.0), Vec3::NEG_Z, 100.0).is_none());
        assert!(raycast(&m, Vec3::new(0.0, 0.0, 5.0), Vec3::Z, 100.0).is_none());
    }

    #[test]
    fn unnormalized_direction_scales_t() {
        let m = box_mesh(Vec3::ONE);
        let hit = raycast(&m, Vec3::new(0.0, 0.0, 5.0), Vec3::NEG_Z * 2.0, 100.0).unwrap();
        assert!((hit.t - 2.0).abs() < 1e-4);
        assert!((hit.pos.z - 1.0).abs() < 1e-4);
    }
}
