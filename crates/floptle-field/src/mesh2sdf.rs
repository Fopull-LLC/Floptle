//! Mesh → signed distance field bake (one-time, CPU) — turns imported triangle
//! meshes into **textured SDF matter** (the unified-field thesis, ADR-0013).
//!
//! Two bake modes, because authored meshes come in two flavors:
//! - [`BakeMode::Solid`] — a watertight prop (a Duck): the field is the filled
//!   solid, sign from the generalized solid-angle **winding number** (robust to
//!   minor non-manifoldness).
//! - [`BakeMode::Shell`] — a **level/shell** (a map): the field is the surface
//!   thickened by `thickness` (`|d| → d − thickness`). It needs **no inside/outside
//!   test at all**, so an intentionally open mesh (e.g. an unmodeled bottom face)
//!   converts cleanly and the creator's surfaces are preserved as matter.
//!
//! For each voxel it also bakes the **nearest-surface albedo** (the owning part's
//! texture sampled at the closest triangle's UV), so the matter carries its texture
//! and blends across `smin` seams. Parallelized over z-slabs; an AABB prune keeps
//! the closest-triangle search cheap.

use std::f32::consts::PI;

use floptle_core::math::Vec3;
use rayon::prelude::*;

/// How the mesh becomes a field.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BakeMode {
    /// Filled solid; sign via winding number (needs a (mostly) closed mesh).
    Solid,
    /// Surface thickened to a slab of half-width `thickness`; no sign needed, so
    /// open / non-watertight meshes convert correctly.
    Shell { thickness: f32 },
}

/// A baked SDF volume: a dense grid of signed distance + RGBA8 albedo spanning the
/// local box `[center - half_extent, center + half_extent]`.
#[derive(Clone, Debug)]
pub struct BakedSdf {
    pub dims: [u32; 3],
    pub center: [f32; 3],
    pub half_extent: [f32; 3],
    /// Signed distance per voxel (negative inside), `dims.x*y*z`, x-fastest.
    pub distance: Vec<f32>,
    /// Nearest-surface albedo per voxel, RGBA8, same indexing as `distance`.
    pub color: Vec<[u8; 4]>,
}

/// A base-color texture for the bake: tightly-packed RGBA8.
pub struct TexRef<'a> {
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
}

/// One material's geometry + its texture, baked together into a model.
pub struct BakePart<'a> {
    pub positions: &'a [[f32; 3]],
    pub indices: &'a [u32],
    pub uvs: &'a [[f32; 2]],
    pub texture: Option<TexRef<'a>>,
    pub tint: [f32; 3],
}

/// Bake a single mesh as a filled solid (convenience wrapper over [`bake_model`]).
pub fn bake(
    positions: &[[f32; 3]],
    indices: &[u32],
    uvs: &[[f32; 2]],
    texture: Option<TexRef>,
    tint: [f32; 3],
    res: u32,
    padding_voxels: f32,
) -> BakedSdf {
    bake_model(
        &[BakePart { positions, indices, uvs, texture, tint }],
        res,
        padding_voxels,
        BakeMode::Solid,
    )
}

struct Tri {
    a: Vec3,
    b: Vec3,
    c: Vec3,
    amin: Vec3,
    amax: Vec3,
    part: usize,
    ia: usize,
    ib: usize,
    ic: usize,
}

/// Bake all `parts` (each its own geometry + texture) into one `res³` SDF volume.
pub fn bake_model(parts: &[BakePart], res: u32, padding_voxels: f32, mode: BakeMode) -> BakedSdf {
    let res = res.max(2);

    // Flatten every part's triangles, remembering the owning part (for color).
    let mut tris: Vec<Tri> = Vec::new();
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for (pi, part) in parts.iter().enumerate() {
        for t in part.indices.chunks_exact(3) {
            let a = Vec3::from(part.positions[t[0] as usize]);
            let b = Vec3::from(part.positions[t[1] as usize]);
            let c = Vec3::from(part.positions[t[2] as usize]);
            min = min.min(a).min(b).min(c);
            max = max.max(a).max(b).max(c);
            tris.push(Tri {
                a,
                b,
                c,
                amin: a.min(b).min(c),
                amax: a.max(b).max(c),
                part: pi,
                ia: t[0] as usize,
                ib: t[1] as usize,
                ic: t[2] as usize,
            });
        }
    }
    if tris.is_empty() {
        return BakedSdf {
            dims: [1, 1, 1],
            center: [0.0; 3],
            half_extent: [1.0; 3],
            distance: vec![1.0e9],
            color: vec![[255; 4]],
        };
    }

    let center = (min + max) * 0.5;
    let extent = (max - min) * 0.5;
    let voxel = (extent * 2.0).max_element().max(1e-4) / res as f32;
    // Shell matter extends `thickness` past the surface; reserve box room for it.
    let margin = match mode {
        BakeMode::Shell { thickness } => thickness,
        BakeMode::Solid => 0.0,
    };
    let half = extent + Vec3::splat(voxel * padding_voxels.max(1.0) + margin);

    let step = (half * 2.0) / res as f32;
    let origin = center - half + step * 0.5;
    let plane = (res * res) as usize;

    // One z-slab per rayon task.
    let slabs: Vec<(Vec<f32>, Vec<[u8; 4]>)> = (0..res)
        .into_par_iter()
        .map(|k| {
            let mut sd = vec![0.0f32; plane];
            let mut sc = vec![[255u8; 4]; plane];
            for j in 0..res {
                for i in 0..res {
                    let p = origin
                        + Vec3::new(i as f32 * step.x, j as f32 * step.y, k as f32 * step.z);

                    let mut best = f32::INFINITY;
                    let mut best_tri = 0usize;
                    let mut best_bary = (1.0f32, 0.0f32, 0.0f32);
                    for (ti, tri) in tris.iter().enumerate() {
                        // skip triangles whose AABB is already farther than `best`
                        let q = (tri.amin - p).max(p - tri.amax).max(Vec3::ZERO);
                        if q.length() >= best {
                            continue;
                        }
                        let (cp, u, v, w) = closest_point_on_triangle(p, tri.a, tri.b, tri.c);
                        let d = (p - cp).length();
                        if d < best {
                            best = d;
                            best_tri = ti;
                            best_bary = (u, v, w);
                        }
                    }

                    let signed = match mode {
                        BakeMode::Shell { thickness } => best - thickness,
                        BakeMode::Solid => {
                            let mut wsum = 0.0f32;
                            for tri in &tris {
                                wsum += solid_angle(p, tri.a, tri.b, tri.c);
                            }
                            if (wsum / (4.0 * PI)).abs() > 0.5 {
                                -best
                            } else {
                                best
                            }
                        }
                    };

                    let idx = (j * res + i) as usize;
                    sd[idx] = signed;

                    let tri = &tris[best_tri];
                    let part = &parts[tri.part];
                    let (bu, bv, bw) = best_bary;
                    let uv = [
                        part.uvs[tri.ia][0] * bu + part.uvs[tri.ib][0] * bv + part.uvs[tri.ic][0] * bw,
                        part.uvs[tri.ia][1] * bu + part.uvs[tri.ib][1] * bv + part.uvs[tri.ic][1] * bw,
                    ];
                    sc[idx] = sample_albedo(&part.texture, uv, part.tint);
                }
            }
            (sd, sc)
        })
        .collect();

    let n = plane * res as usize;
    let mut distance = vec![0.0f32; n];
    let mut color = vec![[255u8; 4]; n];
    for (k, (sd, sc)) in slabs.into_iter().enumerate() {
        let off = k * plane;
        distance[off..off + plane].copy_from_slice(&sd);
        color[off..off + plane].copy_from_slice(&sc);
    }

    BakedSdf {
        dims: [res, res, res],
        center: center.to_array(),
        half_extent: half.to_array(),
        distance,
        color,
    }
}

/// Closest point on triangle `abc` to `p`, plus barycentric `(u,v,w)`. Ericson,
/// *Real-Time Collision Detection*.
fn closest_point_on_triangle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> (Vec3, f32, f32, f32) {
    let ab = b - a;
    let ac = c - a;
    let ap = p - a;
    let d1 = ab.dot(ap);
    let d2 = ac.dot(ap);
    if d1 <= 0.0 && d2 <= 0.0 {
        return (a, 1.0, 0.0, 0.0);
    }
    let bp = p - b;
    let d3 = ab.dot(bp);
    let d4 = ac.dot(bp);
    if d3 >= 0.0 && d4 <= d3 {
        return (b, 0.0, 1.0, 0.0);
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let v = d1 / (d1 - d3);
        return (a + ab * v, 1.0 - v, v, 0.0);
    }
    let cp = p - c;
    let d5 = ab.dot(cp);
    let d6 = ac.dot(cp);
    if d6 >= 0.0 && d5 <= d6 {
        return (c, 0.0, 0.0, 1.0);
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let w = d2 / (d2 - d6);
        return (a + ac * w, 1.0 - w, 0.0, w);
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let w = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (b + (c - b) * w, 0.0, 1.0 - w, w);
    }
    let denom = 1.0 / (va + vb + vc);
    let v = vb * denom;
    let w = vc * denom;
    (a + ab * v + ac * w, 1.0 - v - w, v, w)
}

/// Signed solid angle of triangle `abc` at `p` (van Oosterom–Strackee).
fn solid_angle(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> f32 {
    let va = a - p;
    let vb = b - p;
    let vc = c - p;
    let la = va.length();
    let lb = vb.length();
    let lc = vc.length();
    if la < 1e-12 || lb < 1e-12 || lc < 1e-12 {
        return 0.0;
    }
    let num = va.dot(vb.cross(vc));
    let den = la * lb * lc + va.dot(vb) * lc + va.dot(vc) * lb + vb.dot(vc) * la;
    2.0 * num.atan2(den)
}

fn sample_albedo(texture: &Option<TexRef>, uv: [f32; 2], tint: [f32; 3]) -> [u8; 4] {
    let base = match texture {
        Some(t) if t.width > 0 && t.height > 0 => {
            let x = ((uv[0].rem_euclid(1.0)) * t.width as f32) as u32 % t.width;
            let y = ((uv[1].rem_euclid(1.0)) * t.height as f32) as u32 % t.height;
            let i = ((y * t.width + x) * 4) as usize;
            [
                t.pixels[i] as f32 / 255.0,
                t.pixels[i + 1] as f32 / 255.0,
                t.pixels[i + 2] as f32 / 255.0,
            ]
        }
        _ => [1.0, 1.0, 1.0],
    };
    [
        ((base[0] * tint[0]).clamp(0.0, 1.0) * 255.0) as u8,
        ((base[1] * tint[1]).clamp(0.0, 1.0) * 255.0) as u8,
        ((base[2] * tint[2]).clamp(0.0, 1.0) * 255.0) as u8,
        255,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_cube() -> (Vec<[f32; 3]>, Vec<u32>, Vec<[f32; 2]>) {
        let v = vec![
            [-1.0, -1.0, -1.0],
            [1.0, -1.0, -1.0],
            [1.0, 1.0, -1.0],
            [-1.0, 1.0, -1.0],
            [-1.0, -1.0, 1.0],
            [1.0, -1.0, 1.0],
            [1.0, 1.0, 1.0],
            [-1.0, 1.0, 1.0],
        ];
        let f = vec![
            [0u32, 2, 1],
            [0, 3, 2],
            [4, 5, 6],
            [4, 6, 7],
            [0, 1, 5],
            [0, 5, 4],
            [3, 7, 6],
            [3, 6, 2],
            [0, 4, 7],
            [0, 7, 3],
            [1, 2, 6],
            [1, 6, 5],
        ];
        let idx: Vec<u32> = f.into_iter().flatten().collect();
        let uv = vec![[0.0, 0.0]; v.len()];
        (v, idx, uv)
    }

    #[test]
    fn solid_cube_signed_distance() {
        let (pos, idx, uv) = unit_cube();
        let baked = bake(&pos, &idx, &uv, None, [1.0, 1.0, 1.0], 32, 2.0);
        let res = baked.dims[0];
        let half = Vec3::from(baked.half_extent);
        let center = Vec3::from(baked.center);
        let step = (half * 2.0) / res as f32;
        let origin = center - half + step * 0.5;
        let sample = |p: Vec3| {
            let g = ((p - origin) / step).round();
            let (i, j, k) = (
                (g.x as i32).clamp(0, res as i32 - 1) as u32,
                (g.y as i32).clamp(0, res as i32 - 1) as u32,
                (g.z as i32).clamp(0, res as i32 - 1) as u32,
            );
            baked.distance[(k * res * res + j * res + i) as usize]
        };
        let voxel = step.max_element();
        let dc = sample(Vec3::ZERO);
        assert!(dc < 0.0 && (dc + 1.0).abs() < 2.0 * voxel, "center ≈ -1, got {dc}");
        assert!(sample(Vec3::new(1.6, 0.0, 0.0)) > 0.0, "outside should be positive");
    }

    #[test]
    fn shell_mode_needs_no_closed_mesh() {
        // An OPEN box (drop the -Y face) still bakes: shell distance is just the
        // thickened surface, no winding/sign — the elegant open-mesh case.
        let (pos, mut idx, uv) = unit_cube();
        idx.drain(12..18); // remove the two -Y triangles → a hole in the bottom
        let baked = bake_model(
            &[BakePart { positions: &pos, indices: &idx, uvs: &uv, texture: None, tint: [1.0; 3] }],
            32,
            2.0,
            BakeMode::Shell { thickness: 0.25 },
        );
        let res = baked.dims[0];
        let half = Vec3::from(baked.half_extent);
        let center = Vec3::from(baked.center);
        let step = (half * 2.0) / res as f32;
        let origin = center - half + step * 0.5;
        let sample = |p: Vec3| {
            let g = ((p - origin) / step).round();
            let (i, j, k) = (
                (g.x as i32).clamp(0, res as i32 - 1) as u32,
                (g.y as i32).clamp(0, res as i32 - 1) as u32,
                (g.z as i32).clamp(0, res as i32 - 1) as u32,
            );
            baked.distance[(k * res * res + j * res + i) as usize]
        };
        // on the +X face → inside the thickened shell (negative)
        assert!(sample(Vec3::new(1.0, 0.0, 0.0)) < 0.0, "shell surface should be matter");
        // deep in the hollow center → outside the shell (positive)
        assert!(sample(Vec3::ZERO) > 0.0, "hollow interior is not matter in shell mode");
    }
}
