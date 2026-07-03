//! Mesh → signed distance field bake (one-time, CPU) — the bridge that turns an
//! imported triangle mesh into **textured SDF matter** (the unified-field thesis,
//! ADR-0013). For every voxel of a tight local grid it computes the signed distance
//! to the mesh (closest-point-on-triangle for magnitude, generalized solid-angle
//! winding number for the inside/outside sign — robust to the non-watertight
//! meshes a Blender export can contain) and the **nearest-surface albedo** (the
//! source texture sampled at the closest triangle's interpolated UV), so once
//! raymarched the matter carries its texture and blends with other matter.
//!
//! This is the brute-force baseline (fine for small meshes); a triangle BVH +
//! disk cache are the scaling step for large assets.

use std::f32::consts::PI;

use floptle_core::math::Vec3;

/// A baked SDF volume for one mesh: a dense grid of signed distance + RGBA8 albedo,
/// spanning the local box `[center - half_extent, center + half_extent]`.
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

/// A base-color texture for the bake: tightly-packed RGBA8, `width*height*4` bytes.
pub struct TexRef<'a> {
    pub pixels: &'a [u8],
    pub width: u32,
    pub height: u32,
}

/// Bake `positions`/`indices` (+ per-vertex `uvs`) into a `res³` SDF volume,
/// sampling `texture` × `tint` at the nearest surface for color. `padding_voxels`
/// keeps the surface off the grid border so trilinear reads + sphere-tracing stay
/// valid near the edges.
pub fn bake(
    positions: &[[f32; 3]],
    indices: &[u32],
    uvs: &[[f32; 2]],
    texture: Option<TexRef>,
    tint: [f32; 3],
    res: u32,
    padding_voxels: f32,
) -> BakedSdf {
    let res = res.max(2);

    // Local box: mesh AABB centered, padded so the isosurface never touches the edge.
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for p in positions {
        let v = Vec3::from(*p);
        min = min.min(v);
        max = max.max(v);
    }
    let center = (min + max) * 0.5;
    let extent = (max - min) * 0.5;
    let voxel = (extent * 2.0).max_element().max(1e-4) / res as f32;
    let half = extent + Vec3::splat(voxel * padding_voxels.max(1.0));

    // Precompute triangles (positions + the vertex indices, for UV interpolation).
    let tris: Vec<Tri> = indices
        .chunks_exact(3)
        .map(|t| Tri {
            a: Vec3::from(positions[t[0] as usize]),
            b: Vec3::from(positions[t[1] as usize]),
            c: Vec3::from(positions[t[2] as usize]),
            ia: t[0] as usize,
            ib: t[1] as usize,
            ic: t[2] as usize,
        })
        .collect();

    let n = (res * res * res) as usize;
    let mut distance = vec![0.0f32; n];
    let mut color = vec![[255u8; 4]; n];

    let step = (half * 2.0) / res as f32;
    let origin = center - half + step * 0.5;

    for k in 0..res {
        for j in 0..res {
            for i in 0..res {
                let p = origin + Vec3::new(i as f32 * step.x, j as f32 * step.y, k as f32 * step.z);

                // nearest surface point (magnitude + which triangle/barycentric)
                let mut best = f32::INFINITY;
                let mut best_tri = 0usize;
                let mut best_bary = (1.0f32, 0.0f32, 0.0f32);
                for (ti, tri) in tris.iter().enumerate() {
                    let (cp, u, v, w) = closest_point_on_triangle(p, tri.a, tri.b, tri.c);
                    let d = (p - cp).length();
                    if d < best {
                        best = d;
                        best_tri = ti;
                        best_bary = (u, v, w);
                    }
                }

                // sign via generalized winding number (≈ ±1 inside, ≈ 0 outside)
                let mut wsum = 0.0f32;
                for tri in &tris {
                    wsum += solid_angle(p, tri.a, tri.b, tri.c);
                }
                let inside = (wsum / (4.0 * PI)).abs() > 0.5;

                let idx = (k * res * res + j * res + i) as usize;
                distance[idx] = if inside { -best } else { best };

                // nearest-surface albedo
                let tri = &tris[best_tri];
                let (bu, bv, bw) = best_bary;
                let uv = [
                    uvs[tri.ia][0] * bu + uvs[tri.ib][0] * bv + uvs[tri.ic][0] * bw,
                    uvs[tri.ia][1] * bu + uvs[tri.ib][1] * bv + uvs[tri.ic][1] * bw,
                ];
                color[idx] = sample_albedo(&texture, uv, tint);
            }
        }
    }

    BakedSdf {
        dims: [res, res, res],
        center: center.to_array(),
        half_extent: half.to_array(),
        distance,
        color,
    }
}

/// Bake `positions`/`indices` into a shadow-OCCLUDER volume: an **unsigned,
/// conservative** distance-to-surface field, fast enough to run at scene load for
/// whole level meshes (where [`bake`]'s exact signed brute force would take
/// minutes). Surfaces are voxelized with exact point-to-triangle distances, the
/// rest of the grid is filled by a two-pass 26-neighbour chamfer transform, and
/// the result is scaled slightly under (chamfer over-estimates Euclidean by up to
/// ~8%, and sphere tracing must never overshoot) then fattened by half a voxel so
/// thin walls read as a watertight sheet to a crossing shadow ray. The interior
/// of the fattened sheet is negative; everything else is a safe lower bound.
///
/// Color is left white — occluder volumes are never drawn, only marched by
/// `light_vis` (the sun-shadow ray). Voxels are cubic; `res` sets the voxel count
/// along the mesh's LONGEST axis (other axes scale down proportionally).
pub fn bake_occluder(positions: &[[f32; 3]], indices: &[u32], res: u32) -> BakedSdf {
    let res = res.max(4);
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for p in positions {
        let v = Vec3::from(*p);
        min = min.min(v);
        max = max.max(v);
    }
    if !min.x.is_finite() {
        // no vertices — a 1³ "nothing" volume (all far)
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
    const PAD: u32 = 2; // voxels of air on every face, so edge reads stay valid
    let dims = [
        ((extent.x * 2.0 / voxel).ceil() as u32 + 2 * PAD).max(4),
        ((extent.y * 2.0 / voxel).ceil() as u32 + 2 * PAD).max(4),
        ((extent.z * 2.0 / voxel).ceil() as u32 + 2 * PAD).max(4),
    ];
    let half = Vec3::new(
        dims[0] as f32 * voxel * 0.5,
        dims[1] as f32 * voxel * 0.5,
        dims[2] as f32 * voxel * 0.5,
    );
    let origin = center - half + Vec3::splat(voxel * 0.5); // voxel-center grid

    let n = (dims[0] * dims[1] * dims[2]) as usize;
    let mut dist = vec![f32::INFINITY; n];
    let idx = |i: u32, j: u32, k: u32| ((k * dims[1] + j) * dims[0] + i) as usize;

    // Seed: exact point-to-triangle distance, but only for voxels near each
    // triangle (its AABB ± 1 voxel) — O(surface area), not O(volume × tris).
    for t in indices.chunks_exact(3) {
        let a = Vec3::from(positions[t[0] as usize]);
        let b = Vec3::from(positions[t[1] as usize]);
        let c = Vec3::from(positions[t[2] as usize]);
        let tmin = ((a.min(b).min(c) - origin) / voxel).floor() - Vec3::ONE;
        let tmax = ((a.max(b).max(c) - origin) / voxel).ceil() + Vec3::ONE;
        let (i0, j0, k0) = (
            (tmin.x.max(0.0)) as u32,
            (tmin.y.max(0.0)) as u32,
            (tmin.z.max(0.0)) as u32,
        );
        let (i1, j1, k1) = (
            (tmax.x as u32).min(dims[0] - 1),
            (tmax.y as u32).min(dims[1] - 1),
            (tmax.z as u32).min(dims[2] - 1),
        );
        for k in k0..=k1 {
            for j in j0..=j1 {
                for i in i0..=i1 {
                    let p = origin + Vec3::new(i as f32, j as f32, k as f32) * voxel;
                    let (cp, _, _, _) = closest_point_on_triangle(p, a, b, c);
                    let d = (p - cp).length();
                    let cell = &mut dist[idx(i, j, k)];
                    if d < *cell {
                        *cell = d;
                    }
                }
            }
        }
    }

    // Chamfer distance transform: forward + backward sweeps over the 26-neighbour
    // half-masks propagate the seeded surface distances through the whole grid.
    let w = [voxel, voxel * std::f32::consts::SQRT_2, voxel * 3.0f32.sqrt()];
    let mut mask: Vec<([i32; 3], f32)> = Vec::with_capacity(13);
    for dk in -1i32..=1 {
        for dj in -1i32..=1 {
            for di in -1i32..=1 {
                // strictly "earlier" neighbours in scan order form the forward mask
                if dk < 0 || (dk == 0 && (dj < 0 || (dj == 0 && di < 0))) {
                    let m = (di.abs() + dj.abs() + dk.abs()) as usize;
                    mask.push(([di, dj, dk], w[m - 1]));
                }
            }
        }
    }
    let (dx, dy, dz) = (dims[0] as i32, dims[1] as i32, dims[2] as i32);
    let relax = |dist: &mut [f32], i: i32, j: i32, k: i32, flip: bool| {
        let at = ((k * dy + j) * dx + i) as usize;
        let mut best = dist[at];
        for &([di, dj, dk], wgt) in &mask {
            let (ni, nj, nk) = if flip { (i - di, j - dj, k - dk) } else { (i + di, j + dj, k + dk) };
            if ni < 0 || nj < 0 || nk < 0 || ni >= dx || nj >= dy || nk >= dz {
                continue;
            }
            let v = dist[((nk * dy + nj) * dx + ni) as usize] + wgt;
            if v < best {
                best = v;
            }
        }
        dist[at] = best;
    };
    for k in 0..dz {
        for j in 0..dy {
            for i in 0..dx {
                relax(&mut dist, i, j, k, false);
            }
        }
    }
    for k in (0..dz).rev() {
        for j in (0..dy).rev() {
            for i in (0..dx).rev() {
                relax(&mut dist, i, j, k, true);
            }
        }
    }

    // Conservative scale (sphere tracing must never overshoot the chamfer's slight
    // over-estimate) + half-voxel fatten (the sheet gets a real negative interior,
    // so rays crossing between voxel centers still register a hard hit).
    for d in dist.iter_mut() {
        *d = *d * 0.92 - 0.5 * voxel;
    }

    BakedSdf {
        dims,
        center: center.to_array(),
        half_extent: half.to_array(),
        distance: dist,
        color: vec![[255; 4]; n],
    }
}

struct Tri {
    a: Vec3,
    b: Vec3,
    c: Vec3,
    ia: usize,
    ib: usize,
    ic: usize,
}

/// Closest point on triangle `abc` to `p`, plus its barycentric coords `(u,v,w)`
/// (so `closest = u*a + v*b + w*c`). Ericson, *Real-Time Collision Detection*.
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

/// Signed solid angle subtended by triangle `abc` at `p` (van Oosterom–Strackee).
/// Summed over a mesh and divided by 4π it is the generalized winding number.
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
            // nearest + REPEAT, matching the raster path's retro sampler
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

    /// An axis-aligned cube from -1..1 (8 verts, 12 triangles, CCW outward).
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
            [0, 3, 2], // -Z
            [4, 5, 6],
            [4, 6, 7], // +Z
            [0, 1, 5],
            [0, 5, 4], // -Y
            [3, 7, 6],
            [3, 6, 2], // +Y
            [0, 4, 7],
            [0, 7, 3], // -X
            [1, 2, 6],
            [1, 6, 5], // +X
        ];
        let idx: Vec<u32> = f.into_iter().flatten().collect();
        let uv = vec![[0.0, 0.0]; v.len()];
        (v, idx, uv)
    }

    #[test]
    fn cube_signed_distance_is_correct() {
        let (pos, idx, uv) = unit_cube();
        let baked = bake(&pos, &idx, &uv, None, [1.0, 1.0, 1.0], 32, 2.0);
        let res = baked.dims[0];
        let half = Vec3::from(baked.half_extent);
        let center = Vec3::from(baked.center);
        let step = (half * 2.0) / res as f32;
        let origin = center - half + step * 0.5;

        let sample = |p: Vec3| {
            // nearest voxel
            let g = ((p - origin) / step).round();
            let (i, j, k) = (
                (g.x as i32).clamp(0, res as i32 - 1) as u32,
                (g.y as i32).clamp(0, res as i32 - 1) as u32,
                (g.z as i32).clamp(0, res as i32 - 1) as u32,
            );
            baked.distance[(k * res * res + j * res + i) as usize]
        };

        let voxel = step.max_element();
        // center is inside, ~1 unit from the nearest face → ≈ -1
        let dc = sample(Vec3::ZERO);
        assert!(dc < 0.0, "center should be inside (negative), got {dc}");
        assert!((dc + 1.0).abs() < 2.0 * voxel, "center distance ≈ -1, got {dc}");
        // a point well outside (+x) is positive
        let doo = sample(Vec3::new(1.6, 0.0, 0.0));
        assert!(doo > 0.0, "outside point should be positive, got {doo}");
    }

    #[test]
    fn occluder_bake_blocks_rays_through_the_shell() {
        let (pos, idx, _) = unit_cube();
        let baked = bake_occluder(&pos, &idx, 32);
        // dims follow the (cubic) AABB + padding
        assert!(baked.dims.iter().all(|&d| (32..=40).contains(&d)), "dims {:?}", baked.dims);
        let [dx, dy, dz] = baked.dims;
        let half = Vec3::from(baked.half_extent);
        let center = Vec3::from(baked.center);
        let voxel = half.x * 2.0 / dx as f32;
        let sample = |p: Vec3| {
            let g = ((p - center + half) / voxel - Vec3::splat(0.5)).round();
            let (i, j, k) = (
                (g.x as i32).clamp(0, dx as i32 - 1) as u32,
                (g.y as i32).clamp(0, dy as i32 - 1) as u32,
                (g.z as i32).clamp(0, dz as i32 - 1) as u32,
            );
            baked.distance[((k * dy + j) * dx + i) as usize]
        };
        // ON the cube's +X face: inside the fattened sheet → negative (a shadow ray
        // crossing here must register a hard hit)
        let on_face = sample(Vec3::new(1.0, 0.0, 0.0));
        assert!(on_face <= 0.0, "surface sheet should be negative, got {on_face}");
        // a grid corner (well off the surface) is a safe positive lower bound
        let far = sample(center + half - Vec3::splat(voxel));
        assert!(far > 0.0, "far corner should be positive, got {far}");
        // ...and never OVER-estimates the true distance (sphere tracing safety):
        // true distance from that corner to the cube is ~|corner| - cube reach
        let corner = center + half - Vec3::splat(voxel);
        let true_d = (corner - Vec3::new(1.0, 1.0, 1.0)).length().min(corner.length() - 1.0);
        assert!(far <= true_d + voxel, "occluder distance must stay conservative ({far} vs {true_d})");
    }

    #[test]
    fn bake_carries_texture_color() {
        let (pos, idx, uv) = unit_cube();
        // a 2×2 all-red texture: every nearest-surface sample should come back red
        let red = vec![255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255];
        let baked = bake(
            &pos,
            &idx,
            &uv,
            Some(TexRef { pixels: &red, width: 2, height: 2 }),
            [1.0, 1.0, 1.0],
            16,
            2.0,
        );
        let reds = baked.color.iter().filter(|c| c[0] > 200 && c[1] < 50 && c[2] < 50).count();
        assert!(reds > 0, "bake must carry texture color (found {reds} red voxels)");
    }
}
