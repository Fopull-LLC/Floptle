//! Mesh → GPU BVH for **exact** distance raymarching (sharp SDF matter).
//!
//! Unlike [`crate::mesh2sdf`] (which voxelizes and so rounds corners), this keeps
//! the triangles: it builds a bounding-volume hierarchy and flattens it, plus the
//! triangles' positions/UVs and a small color atlas, into GPU-ready buffers. The
//! raymarch shader traverses the BVH for the exact closest-triangle distance, so
//! edges stay as crisp as the source mesh — rounded only by a tiny shell thickness.
//! Still an SDF: it smin-blends with other matter and deforms.

use floptle_core::math::Vec3;

use crate::mesh2sdf::BakePart;

const MAX_LEAF: usize = 4;

/// A BVH node (32 bytes, std430-friendly). `hi == 0` ⇒ interior (left child is
/// `self_index + 1`, right child is `lo`); `hi >= 1` ⇒ leaf (`lo` = first triangle,
/// `hi` = count).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BvhNodeGpu {
    pub aabb_min: [f32; 3],
    pub lo: u32,
    pub aabb_max: [f32; 3],
    pub hi: u32,
}

/// A triangle's positions (48 bytes; w lanes are padding).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BvhTriGpu {
    pub a: [f32; 4],
    pub b: [f32; 4],
    pub c: [f32; 4],
}

/// A triangle's UVs + its material's atlas rect (48 bytes; std430 padding so `rect`
/// is 16-aligned).
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct BvhTriDataGpu {
    pub uv_a: [f32; 2],
    pub uv_b: [f32; 2],
    pub uv_c: [f32; 2],
    pub _pad: [f32; 2],
    pub rect: [f32; 4], // atlas (offset.xy, scale.xy)
}

const _: () = assert!(std::mem::size_of::<BvhNodeGpu>() == 32);
const _: () = assert!(std::mem::size_of::<BvhTriGpu>() == 48);
const _: () = assert!(std::mem::size_of::<BvhTriDataGpu>() == 48);

/// A mesh baked to a GPU BVH + triangle buffers + a color atlas.
pub struct BakedBvh {
    pub center: [f32; 3],
    pub half_extent: [f32; 3],
    pub thickness: f32,
    pub nodes: Vec<BvhNodeGpu>,
    pub tris: Vec<BvhTriGpu>,
    pub tri_data: Vec<BvhTriDataGpu>,
    pub atlas_pixels: Vec<u8>,
    pub atlas_w: u32,
    pub atlas_h: u32,
}

struct FTri {
    a: Vec3,
    b: Vec3,
    c: Vec3,
    uv: [[f32; 2]; 3],
    part: usize,
}

impl FTri {
    fn centroid(&self) -> Vec3 {
        (self.a + self.b + self.c) / 3.0
    }
    fn amin(&self) -> Vec3 {
        self.a.min(self.b).min(self.c)
    }
    fn amax(&self) -> Vec3 {
        self.a.max(self.b).max(self.c)
    }
}

/// Build a BVH + triangle/atlas buffers from `parts`. `thickness` is the shell
/// half-width (tiny → sharp), applied in the shader as `unsigned_dist - thickness`.
pub fn bake_bvh(parts: &[BakePart], thickness: f32) -> BakedBvh {
    // Flatten all parts' triangles.
    let mut ftris: Vec<FTri> = Vec::new();
    for (pi, part) in parts.iter().enumerate() {
        for t in part.indices.chunks_exact(3) {
            let (ia, ib, ic) = (t[0] as usize, t[1] as usize, t[2] as usize);
            ftris.push(FTri {
                a: Vec3::from(part.positions[ia]),
                b: Vec3::from(part.positions[ib]),
                c: Vec3::from(part.positions[ic]),
                uv: [part.uvs[ia], part.uvs[ib], part.uvs[ic]],
                part: pi,
            });
        }
    }

    let (atlas_pixels, atlas_w, atlas_h, part_rect) = build_atlas(parts);

    // World AABB (+ a margin for the shell + box-cull).
    let mut amin = Vec3::splat(f32::INFINITY);
    let mut amax = Vec3::splat(f32::NEG_INFINITY);
    for t in &ftris {
        amin = amin.min(t.amin());
        amax = amax.max(t.amax());
    }
    if ftris.is_empty() {
        amin = Vec3::ZERO;
        amax = Vec3::ZERO;
    }
    let center = (amin + amax) * 0.5;
    let half = (amax - amin) * 0.5 + Vec3::splat(thickness + 0.01);

    // Build the BVH; `order` is the leaf-contiguous triangle permutation.
    let mut nodes: Vec<BvhNodeGpu> = Vec::new();
    let mut order: Vec<usize> = Vec::new();
    if ftris.is_empty() {
        nodes.push(BvhNodeGpu { aabb_min: [0.0; 3], lo: 0, aabb_max: [0.0; 3], hi: 1 });
    } else {
        let mut idx: Vec<usize> = (0..ftris.len()).collect();
        build(&mut nodes, &ftris, &mut idx, &mut order);
    }

    let tris: Vec<BvhTriGpu> = order
        .iter()
        .map(|&i| {
            let t = &ftris[i];
            BvhTriGpu {
                a: [t.a.x, t.a.y, t.a.z, 0.0],
                b: [t.b.x, t.b.y, t.b.z, 0.0],
                c: [t.c.x, t.c.y, t.c.z, 0.0],
            }
        })
        .collect();
    let tri_data: Vec<BvhTriDataGpu> = order
        .iter()
        .map(|&i| {
            let t = &ftris[i];
            BvhTriDataGpu {
                uv_a: t.uv[0],
                uv_b: t.uv[1],
                uv_c: t.uv[2],
                _pad: [0.0, 0.0],
                rect: part_rect.get(t.part).copied().unwrap_or([0.0, 0.0, 1.0, 1.0]),
            }
        })
        .collect();

    BakedBvh {
        center: center.to_array(),
        half_extent: half.to_array(),
        thickness,
        nodes,
        tris: if tris.is_empty() { vec![BvhTriGpu { a: [0.0; 4], b: [0.0; 4], c: [0.0; 4] }] } else { tris },
        tri_data: if tri_data.is_empty() {
            vec![BvhTriDataGpu { uv_a: [0.0; 2], uv_b: [0.0; 2], uv_c: [0.0; 2], _pad: [0.0; 2], rect: [0.0, 0.0, 1.0, 1.0] }]
        } else {
            tri_data
        },
        atlas_pixels,
        atlas_w,
        atlas_h,
    }
}

/// DFS BVH build over `idx` (a subset of `ftris`); appends nodes, fills `order`
/// with the leaf-contiguous triangle indices. Returns this node's index.
fn build(nodes: &mut Vec<BvhNodeGpu>, ftris: &[FTri], idx: &mut [usize], order: &mut Vec<usize>) -> u32 {
    let node_i = nodes.len() as u32;
    nodes.push(BvhNodeGpu { aabb_min: [0.0; 3], lo: 0, aabb_max: [0.0; 3], hi: 0 });

    let mut amin = Vec3::splat(f32::INFINITY);
    let mut amax = Vec3::splat(f32::NEG_INFINITY);
    let mut cmin = Vec3::splat(f32::INFINITY);
    let mut cmax = Vec3::splat(f32::NEG_INFINITY);
    for &i in idx.iter() {
        amin = amin.min(ftris[i].amin());
        amax = amax.max(ftris[i].amax());
        let c = ftris[i].centroid();
        cmin = cmin.min(c);
        cmax = cmax.max(c);
    }

    if idx.len() <= MAX_LEAF {
        let first = order.len() as u32;
        order.extend_from_slice(idx);
        nodes[node_i as usize] =
            BvhNodeGpu { aabb_min: amin.to_array(), lo: first, aabb_max: amax.to_array(), hi: idx.len() as u32 };
        return node_i;
    }

    let ext = cmax - cmin;
    let axis = if ext.x >= ext.y && ext.x >= ext.z {
        0
    } else if ext.y >= ext.z {
        1
    } else {
        2
    };
    idx.sort_by(|&a, &b| {
        ftris[a].centroid()[axis].partial_cmp(&ftris[b].centroid()[axis]).unwrap_or(std::cmp::Ordering::Equal)
    });
    let mid = idx.len() / 2;
    let (l, r) = idx.split_at_mut(mid);
    let _left = build(nodes, ftris, l, order); // == node_i + 1
    let right = build(nodes, ftris, r, order);
    nodes[node_i as usize] =
        BvhNodeGpu { aabb_min: amin.to_array(), lo: right, aabb_max: amax.to_array(), hi: 0 };
    node_i
}

/// Pack each part's texture (× tint) into a uniform-cell color atlas; returns the
/// pixels + per-part `(offset.xy, scale.xy)` rect.
fn build_atlas(parts: &[BakePart]) -> (Vec<u8>, u32, u32, Vec<[f32; 4]>) {
    let n = parts.len().max(1) as u32;
    let mut cell = 1u32;
    for p in parts {
        if let Some(t) = &p.texture {
            cell = cell.max(t.width).max(t.height);
        }
    }
    cell = cell.clamp(1, 128);
    let cols = (n as f32).sqrt().ceil().max(1.0) as u32;
    let rows = n.div_ceil(cols);
    let aw = cols * cell;
    let ah = rows * cell;
    let mut px = vec![0u8; (aw * ah * 4) as usize];
    let mut rects = Vec::with_capacity(parts.len());

    for (i, part) in parts.iter().enumerate() {
        let col = i as u32 % cols;
        let row = i as u32 / cols;
        let ox = col * cell;
        let oy = row * cell;
        let (tw, th) = match &part.texture {
            Some(t) if t.width > 0 && t.height > 0 => {
                let (cw, ch) = (t.width.min(cell), t.height.min(cell));
                for y in 0..ch {
                    for x in 0..cw {
                        let si = ((y * t.width + x) * 4) as usize;
                        let di = (((oy + y) * aw + (ox + x)) * 4) as usize;
                        px[di] = (t.pixels[si] as f32 * part.tint[0]).clamp(0.0, 255.0) as u8;
                        px[di + 1] = (t.pixels[si + 1] as f32 * part.tint[1]).clamp(0.0, 255.0) as u8;
                        px[di + 2] = (t.pixels[si + 2] as f32 * part.tint[2]).clamp(0.0, 255.0) as u8;
                        px[di + 3] = 255;
                    }
                }
                (cw, ch)
            }
            _ => {
                // solid tint in one texel
                let di = ((oy * aw + ox) * 4) as usize;
                px[di] = (part.tint[0] * 255.0) as u8;
                px[di + 1] = (part.tint[1] * 255.0) as u8;
                px[di + 2] = (part.tint[2] * 255.0) as u8;
                px[di + 3] = 255;
                (1, 1)
            }
        };
        rects.push([
            ox as f32 / aw as f32,
            oy as f32 / ah as f32,
            tw as f32 / aw as f32,
            th as f32 / ah as f32,
        ]);
    }
    (px, aw, ah, rects)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh2sdf::closest_point_on_triangle;

    fn tetra() -> (Vec<[f32; 3]>, Vec<u32>, Vec<[f32; 2]>) {
        // a small irregular mesh of several triangles to exercise the BVH
        let v = vec![
            [0.0, 0.0, 0.0],
            [2.0, 0.0, 0.3],
            [1.0, 2.0, -0.4],
            [1.0, 0.5, 2.0],
            [-1.0, 1.0, 1.0],
            [0.5, -1.5, 0.7],
        ];
        let idx = vec![0, 1, 2, 0, 2, 3, 0, 3, 1, 1, 3, 2, 0, 4, 3, 1, 5, 3];
        let uv = vec![[0.0, 0.0]; v.len()];
        (v, idx, uv)
    }

    /// The BVH query must return the exact same distance as brute force.
    fn brute(ftris: &[FTri], p: Vec3) -> f32 {
        ftris.iter().fold(f32::INFINITY, |b, t| {
            let (cp, ..) = closest_point_on_triangle(p, t.a, t.b, t.c);
            b.min((p - cp).length())
        })
    }

    fn query(nodes: &[BvhNodeGpu], otris: &[FTri], p: Vec3) -> f32 {
        let mut stack = vec![0u32];
        let mut best = f32::INFINITY;
        while let Some(ni) = stack.pop() {
            let n = &nodes[ni as usize];
            let q = (Vec3::from(n.aabb_min) - p).max(p - Vec3::from(n.aabb_max)).max(Vec3::ZERO);
            if q.length_squared() >= best * best {
                continue;
            }
            if n.hi == 0 {
                stack.push(ni + 1);
                stack.push(n.lo);
            } else {
                for k in 0..n.hi {
                    let t = &otris[(n.lo + k) as usize];
                    let (cp, ..) = closest_point_on_triangle(p, t.a, t.b, t.c);
                    best = best.min((p - cp).length());
                }
            }
        }
        best
    }

    #[test]
    fn bvh_query_matches_brute_force() {
        let (pos, idx, uv) = tetra();
        let parts = [BakePart { positions: &pos, indices: &idx, uvs: &uv, texture: None, tint: [1.0; 3] }];
        // rebuild the flattened tris + order exactly as bake_bvh does
        let mut ftris: Vec<FTri> = Vec::new();
        for t in idx.chunks_exact(3) {
            ftris.push(FTri {
                a: Vec3::from(pos[t[0] as usize]),
                b: Vec3::from(pos[t[1] as usize]),
                c: Vec3::from(pos[t[2] as usize]),
                uv: [[0.0; 2]; 3],
                part: 0,
            });
        }
        let mut nodes = Vec::new();
        let mut order = Vec::new();
        let mut indices: Vec<usize> = (0..ftris.len()).collect();
        build(&mut nodes, &ftris, &mut indices, &mut order);
        let otris: Vec<FTri> = order
            .iter()
            .map(|&i| FTri { a: ftris[i].a, b: ftris[i].b, c: ftris[i].c, uv: [[0.0; 2]; 3], part: 0 })
            .collect();

        for &p in &[
            Vec3::new(0.5, 0.5, 0.5),
            Vec3::new(3.0, 0.0, 0.0),
            Vec3::new(-2.0, -2.0, -2.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 5.0, 0.0),
        ] {
            let q = query(&nodes, &otris, p);
            let b = brute(&ftris, p);
            assert!((q - b).abs() < 1e-4, "BVH {q} != brute {b} at {p:?}");
        }

        // sanity: bake_bvh runs and produces a non-degenerate tree
        let baked = bake_bvh(&parts, 0.05);
        assert!(baked.nodes.len() >= 1 && baked.tris.len() == ftris.len());
    }
}
