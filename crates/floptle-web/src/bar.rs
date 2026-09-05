//! The probe's rig: a tapered, vertex-painted bar with four joints up its
//! length. The same shape `floptle-render`'s `skin_probe` example uses, plus a
//! paint gradient so one mesh exercises both storage-buffer paths the browser
//! has to prove — GPU skinning and vertex colour.

use floptle_render::{MeshData, Vertex};
use glam::{Mat4, Vec3};

/// Rings up the bar; each ring is weighted between two joints so a bend blends
/// smoothly rather than hinging.
pub const RINGS: usize = 24;
pub const SIDES: usize = 12;
pub const JOINTS: usize = 4;
/// The bar's height, in world units.
pub const HEIGHT: f32 = 3.0;

/// The rig as the renderer takes it: mesh (with vertex colours), and one joint
/// quad + weight quad per vertex.
pub struct Rig {
    pub mesh: MeshData,
    pub joints: Vec<[u16; 4]>,
    pub weights: Vec<[f32; 4]>,
}

/// A tapered bar along +Y, ring by ring, painted from warm at the base to cool
/// at the tip — a gradient the eye reads at any resolution.
pub fn rig() -> Rig {
    let mut vertices = Vec::new();
    let mut colors = Vec::new();
    let mut indices = Vec::new();
    let mut joints = Vec::new();
    let mut weights = Vec::new();
    for r in 0..=RINGS {
        let t = r as f32 / RINGS as f32;
        let y = t * HEIGHT;
        let radius = 0.35 * (1.0 - 0.5 * t);
        let jf = t * (JOINTS - 1) as f32;
        let j0 = (jf.floor() as usize).min(JOINTS - 1);
        let j1 = (j0 + 1).min(JOINTS - 1);
        let f = jf - j0 as f32;
        for s in 0..SIDES {
            let a = s as f32 / SIDES as f32 * std::f32::consts::TAU;
            let (sn, cs) = a.sin_cos();
            vertices.push(Vertex {
                pos: [cs * radius, y, sn * radius],
                normal: [cs, 0.0, sn],
                uv: [s as f32 / SIDES as f32, t],
            });
            colors.push(paint(t));
            joints.push([j0 as u16, j1 as u16, 0, 0]);
            weights.push([1.0 - f, f, 0.0, 0.0]);
        }
    }
    for r in 0..RINGS {
        for s in 0..SIDES {
            let a = (r * SIDES + s) as u32;
            let b = (r * SIDES + (s + 1) % SIDES) as u32;
            let c = a + SIDES as u32;
            let d = b + SIDES as u32;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    Rig { mesh: MeshData { vertices, indices, colors: Some(colors) }, joints, weights }
}

/// The paint at height fraction `t`: orange at the base, sky blue at the tip.
pub fn paint(t: f32) -> [u8; 4] {
    let lerp = |a: f32, b: f32| (a + (b - a) * t) * 255.0;
    [lerp(0.95, 0.35) as u8, lerp(0.55, 0.75) as u8, lerp(0.20, 0.95) as u8, 255]
}

/// The pose: every joint above the first rotates `bend` radians about Z,
/// accumulated up the chain, so the bar curls. Returns `(fallback, palette)`,
/// the palette being `nodeWorld · inverseBind` per slot — what the vertex
/// shader consumes.
pub fn pose(bend: f32) -> (Mat4, Vec<Mat4>) {
    let step = HEIGHT / (JOINTS - 1) as f32;
    let bind: Vec<Mat4> =
        (0..JOINTS).map(|j| Mat4::from_translation(Vec3::new(0.0, j as f32 * step, 0.0))).collect();
    let mut world = Vec::with_capacity(JOINTS);
    let mut acc = Mat4::IDENTITY;
    for j in 0..JOINTS {
        if j > 0 {
            acc *= Mat4::from_translation(Vec3::new(0.0, step, 0.0)) * Mat4::from_rotation_z(bend);
        }
        world.push(acc);
    }
    let palette = world.iter().zip(&bind).map(|(w, b)| *w * b.inverse()).collect();
    (Mat4::IDENTITY, palette)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_vertex_has_a_joint_quad_a_weight_quad_and_a_colour() {
        let r = rig();
        let n = r.mesh.vertices.len();
        assert_eq!(n, (RINGS + 1) * SIDES);
        assert_eq!(r.joints.len(), n);
        assert_eq!(r.weights.len(), n);
        assert_eq!(r.mesh.colors.as_ref().map(Vec::len), Some(n));
        assert_eq!(r.mesh.indices.len(), RINGS * SIDES * 6);
    }

    #[test]
    fn weights_sum_to_one_and_name_real_joints() {
        for (w, j) in rig().weights.iter().zip(&rig().joints) {
            let sum: f32 = w.iter().sum();
            assert!((sum - 1.0).abs() < 1e-5, "weights {w:?} sum to {sum}");
            assert!(j.iter().all(|&j| (j as usize) < JOINTS), "joint quad {j:?} names a slot past the rig");
        }
    }

    #[test]
    fn no_bend_is_the_bind_pose() {
        let (_, palette) = pose(0.0);
        assert_eq!(palette.len(), JOINTS);
        for m in palette {
            assert!(m.abs_diff_eq(Mat4::IDENTITY, 1e-5), "an unbent joint must be the identity, got {m:?}");
        }
    }

    #[test]
    fn a_bend_moves_the_tip_and_leaves_the_root() {
        let (_, palette) = pose(0.5);
        let tip = palette[JOINTS - 1].transform_point3(Vec3::new(0.0, HEIGHT, 0.0));
        assert!(tip.x.abs() > 0.5, "the tip did not move sideways: {tip:?}");
        assert!(palette[0].abs_diff_eq(Mat4::IDENTITY, 1e-5), "the root joint moved");
    }

    #[test]
    fn the_paint_runs_warm_to_cool() {
        let base = paint(0.0);
        let tip = paint(1.0);
        assert!(base[0] > base[2], "the base should be warm: {base:?}");
        assert!(tip[2] > tip[0], "the tip should be cool: {tip:?}");
    }
}
