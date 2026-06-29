//! Editable terrain — a signed-distance voxel field you sculpt and paint in the
//! editor, then raymarch (it reuses the volume path the mesh→SDF bake already
//! feeds: a [`BakedSdf`] of distance + color the renderer uploads as 3D textures).
//!
//! The field is a regular grid spanning `[center - half_extent, center + half]`,
//! `x`-fastest. Sculpting edits the distance grid with smooth CSG (union to raise,
//! subtraction to dig); painting edits the co-located color grid. Both are CPU-side
//! so they stay simple and undoable; the editor re-uploads after each stroke.

use crate::mesh2sdf::BakedSdf;

/// A sculptable terrain volume.
#[derive(Clone, Debug)]
pub struct Terrain {
    pub baked: BakedSdf,
}

/// What a sculpt brush does to the distance field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brush {
    /// Add material (raise / build up).
    Raise,
    /// Remove material (dig / carve).
    Lower,
    /// Pull the surface toward flat (a low-pass on height).
    Smooth,
    /// Pull the surface toward the height where the stroke landed (level it).
    Flatten,
    /// Paint the surface color (leaves the shape alone).
    Paint,
}

impl Terrain {
    /// A flat ground at world height `ground_y`, filling a box of `dims` voxels
    /// centered at `center` with `half_extent`, tinted `color` (sRGB 0..1).
    pub fn flat(
        dims: [u32; 3],
        center: [f32; 3],
        half_extent: [f32; 3],
        ground_y: f32,
        color: [f32; 3],
    ) -> Self {
        let n = (dims[0] * dims[1] * dims[2]) as usize;
        // Alpha is the painted texture-slot index (0 = untextured / flat tint).
        let rgba = [
            (color[0] * 255.0) as u8,
            (color[1] * 255.0) as u8,
            (color[2] * 255.0) as u8,
            0,
        ];
        let mut t = Terrain {
            baked: BakedSdf { dims, center, half_extent, distance: vec![0.0; n], color: vec![rgba; n] },
        };
        for iz in 0..dims[2] {
            for iy in 0..dims[1] {
                for ix in 0..dims[0] {
                    let p = t.voxel_world(ix, iy, iz);
                    // SDF to the horizontal ground plane: below = solid (negative).
                    let i = t.idx(ix, iy, iz);
                    t.baked.distance[i] = p[1] - ground_y;
                }
            }
        }
        t
    }

    /// Serialize the field to a compact binary blob (saved alongside the scene).
    /// Layout: magic, dims[3]u32, center[3]f32, half[3]f32, distance(f32…), color(u8…).
    pub fn to_bytes(&self) -> Vec<u8> {
        let b = &self.baked;
        let n = b.distance.len();
        let mut out = Vec::with_capacity(40 + n * 8);
        out.extend_from_slice(b"FTRN");
        for v in b.dims {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in b.center.iter().chain(&b.half_extent) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for &d in &b.distance {
            out.extend_from_slice(&d.to_le_bytes());
        }
        for c in &b.color {
            out.extend_from_slice(c);
        }
        out
    }

    /// Parse a field written by [`to_bytes`](Self::to_bytes). `None` if malformed.
    pub fn from_bytes(data: &[u8]) -> Option<Terrain> {
        if data.len() < 40 || &data[0..4] != b"FTRN" {
            return None;
        }
        let mut o = 4;
        let u32_at = |o: &mut usize| {
            let v = u32::from_le_bytes(data[*o..*o + 4].try_into().ok()?);
            *o += 4;
            Some(v)
        };
        let dims = [u32_at(&mut o)?, u32_at(&mut o)?, u32_at(&mut o)?];
        let f32_at = |o: &mut usize| {
            let v = f32::from_le_bytes(data[*o..*o + 4].try_into().ok()?);
            *o += 4;
            Some(v)
        };
        let center = [f32_at(&mut o)?, f32_at(&mut o)?, f32_at(&mut o)?];
        let half_extent = [f32_at(&mut o)?, f32_at(&mut o)?, f32_at(&mut o)?];
        let n = (dims[0] * dims[1] * dims[2]) as usize;
        if data.len() < o + n * 8 {
            return None;
        }
        let mut distance = Vec::with_capacity(n);
        for _ in 0..n {
            distance.push(f32::from_le_bytes(data[o..o + 4].try_into().ok()?));
            o += 4;
        }
        let mut color = Vec::with_capacity(n);
        for _ in 0..n {
            color.push([data[o], data[o + 1], data[o + 2], data[o + 3]]);
            o += 4;
        }
        Some(Terrain { baked: BakedSdf { dims, center, half_extent, distance, color } })
    }

    #[inline]
    fn idx(&self, x: u32, y: u32, z: u32) -> usize {
        let [w, h, _] = self.baked.dims;
        ((z * h + y) * w + x) as usize
    }

    /// World position of a voxel center.
    fn voxel_world(&self, x: u32, y: u32, z: u32) -> [f32; 3] {
        let [w, h, d] = self.baked.dims;
        let c = self.baked.center;
        let hf = self.baked.half_extent;
        let f = |i: u32, n: u32, ci: f32, hi: f32| ci - hi + (i as f32 + 0.5) / n as f32 * 2.0 * hi;
        [f(x, w, c[0], hf[0]), f(y, h, c[1], hf[1]), f(z, d, c[2], hf[2])]
    }

    /// Trilinearly-sampled signed distance at a world point. Outside the box it
    /// returns the (positive) distance to the box, so a ray can march toward it.
    pub fn sample(&self, p: [f32; 3]) -> f32 {
        let c = self.baked.center;
        let hf = self.baked.half_extent;
        let rel = [p[0] - c[0], p[1] - c[1], p[2] - c[2]];
        let q = [rel[0].abs() - hf[0], rel[1].abs() - hf[1], rel[2].abs() - hf[2]];
        let outside =
            (q[0].max(0.0).powi(2) + q[1].max(0.0).powi(2) + q[2].max(0.0).powi(2)).sqrt();
        if outside > 1e-4 {
            return outside;
        }
        let [w, h, d] = self.baked.dims;
        // Continuous grid coords (voxel centers at integer+0.5).
        let g = |r: f32, hi: f32, n: u32| ((r / (2.0 * hi) + 0.5) * n as f32 - 0.5).clamp(0.0, n as f32 - 1.0);
        let gx = g(rel[0], hf[0], w);
        let gy = g(rel[1], hf[1], h);
        let gz = g(rel[2], hf[2], d);
        let (x0, y0, z0) = (gx.floor() as u32, gy.floor() as u32, gz.floor() as u32);
        let (x1, y1, z1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1), (z0 + 1).min(d - 1));
        let (fx, fy, fz) = (gx - x0 as f32, gy - y0 as f32, gz - z0 as f32);
        let s = |x, y, z| self.baked.distance[self.idx(x, y, z)];
        let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
        let c00 = lerp(s(x0, y0, z0), s(x1, y0, z0), fx);
        let c10 = lerp(s(x0, y1, z0), s(x1, y1, z0), fx);
        let c01 = lerp(s(x0, y0, z1), s(x1, y0, z1), fx);
        let c11 = lerp(s(x0, y1, z1), s(x1, y1, z1), fx);
        lerp(lerp(c00, c10, fy), lerp(c01, c11, fy), fz)
    }

    /// March a ray (world space) and return the first surface hit reached *from
    /// outside* — so when the camera starts inside/below solid ground we march out
    /// first instead of immediately reporting the origin (the "sculpts at the
    /// camera" bug). `None` if the ray never meets the surface from outside.
    pub fn raycast(&self, ro: [f32; 3], rd: [f32; 3]) -> Option<[f32; 3]> {
        let mut t = 0.0f32;
        let mut been_outside = false;
        for _ in 0..400 {
            let p = [ro[0] + rd[0] * t, ro[1] + rd[1] * t, ro[2] + rd[2] * t];
            let dd = self.sample(p);
            if dd > 0.1 {
                been_outside = true;
            }
            if been_outside && dd < 0.02 {
                return Some(p);
            }
            // `abs` so we still advance while inside solid (where dd is negative).
            t += dd.abs().max(0.05);
            if t > 500.0 {
                break;
            }
        }
        None
    }

    /// The voxel index range (inclusive) covering the world AABB of a brush.
    fn brush_range(&self, center: [f32; 3], radius: f32) -> [[u32; 3]; 2] {
        let [w, h, d] = self.baked.dims;
        let c = self.baked.center;
        let hf = self.baked.half_extent;
        let to_grid = |v: f32, ci: f32, hi: f32, n: u32| {
            (((v - ci) / (2.0 * hi) + 0.5) * n as f32).clamp(0.0, n as f32 - 1.0)
        };
        let lo = |i: usize, n: u32| to_grid(center[i] - radius, c[i], hf[i], n).floor() as u32;
        let hi_ = |i: usize, n: u32| to_grid(center[i] + radius, c[i], hf[i], n).ceil() as u32;
        [
            [lo(0, w), lo(1, h), lo(2, d)],
            [hi_(0, w).min(w - 1), hi_(1, h).min(h - 1), hi_(2, d).min(d - 1)],
        ]
    }

    /// Sculpt the distance field with a spherical brush. `strength` (0..1) is how
    /// far the field is pulled toward the target this stroke (so dragging builds up
    /// gradually). Raise = smooth union with a sphere, Lower = subtraction.
    ///
    /// The whole grid is scanned (not just the brush AABB): a CSG sphere changes the
    /// distance field well beyond the sphere itself (`min(ground, sphere)` differs
    /// from `ground` across a whole region above a raised bump), and leaving those
    /// cells stale makes the raymarcher overshoot. The per-cell work is cheap and
    /// only cells the brush actually changes are written.
    pub fn sculpt(&mut self, brush: Brush, center: [f32; 3], radius: f32, strength: f32) {
        match brush {
            Brush::Smooth => return self.smooth(center, radius, strength),
            Brush::Flatten => return self.flatten(center, radius, strength),
            Brush::Paint => return,
            _ => {}
        }
        let s = strength.clamp(0.02, 1.0);
        let [w, h, d] = self.baked.dims;
        for iz in 0..d {
            for iy in 0..h {
                for ix in 0..w {
                    let p = self.voxel_world(ix, iy, iz);
                    let dc = ((p[0] - center[0]).powi(2)
                        + (p[1] - center[1]).powi(2)
                        + (p[2] - center[2]).powi(2))
                    .sqrt();
                    let sphere = dc - radius; // sphere SDF: negative inside the brush
                    let i = self.idx(ix, iy, iz);
                    let cur = self.baked.distance[i];
                    let target = match brush {
                        Brush::Raise => cur.min(sphere), // union: add solid
                        Brush::Lower => cur.max(-sphere), // subtraction: carve
                        _ => cur,
                    };
                    if (target - cur).abs() > 1e-6 {
                        self.baked.distance[i] = cur + (target - cur) * s;
                    }
                }
            }
        }
    }

    /// Level the brushed region toward the height the stroke landed on (`center.y`).
    fn flatten(&mut self, center: [f32; 3], radius: f32, strength: f32) {
        let [lo, hi] = self.brush_range(center, radius);
        let s = strength.clamp(0.02, 1.0);
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = self.voxel_world(ix, iy, iz);
                    let dxz = ((p[0] - center[0]).powi(2) + (p[2] - center[2]).powi(2)).sqrt();
                    if dxz > radius {
                        continue;
                    }
                    let i = self.idx(ix, iy, iz);
                    let cur = self.baked.distance[i];
                    let target = p[1] - center[1]; // plane SDF at the hit height
                    let wgt = s * (1.0 - dxz / radius).clamp(0.0, 1.0);
                    self.baked.distance[i] = cur + (target - cur) * wgt;
                }
            }
        }
    }

    /// The surface normal at a world point (the normalized SDF gradient). Used to
    /// orient the brush telegraph. Falls back to +Y for a degenerate gradient.
    pub fn normal(&self, p: [f32; 3]) -> [f32; 3] {
        let e = 0.2; // ~a voxel; large enough to average out f16 grid noise
        let dx = self.sample([p[0] + e, p[1], p[2]]) - self.sample([p[0] - e, p[1], p[2]]);
        let dy = self.sample([p[0], p[1] + e, p[2]]) - self.sample([p[0], p[1] - e, p[2]]);
        let dz = self.sample([p[0], p[1], p[2] + e]) - self.sample([p[0], p[1], p[2] - e]);
        let len = (dx * dx + dy * dy + dz * dz).sqrt();
        if len < 1e-6 {
            [0.0, 1.0, 0.0]
        } else {
            [dx / len, dy / len, dz / len]
        }
    }

    /// Pull the surface toward its local average (a small box blur of distance).
    fn smooth(&mut self, center: [f32; 3], radius: f32, strength: f32) {
        let [lo, hi] = self.brush_range(center, radius);
        let [w, h, d] = self.baked.dims;
        let src = self.baked.distance.clone();
        let at = |x: u32, y: u32, z: u32| src[((z * h + y) * w + x) as usize];
        for iz in lo[2].max(1)..=hi[2].min(d - 2) {
            for iy in lo[1].max(1)..=hi[1].min(h - 2) {
                for ix in lo[0].max(1)..=hi[0].min(w - 2) {
                    let p = self.voxel_world(ix, iy, iz);
                    let dc = ((p[0] - center[0]).powi(2)
                        + (p[1] - center[1]).powi(2)
                        + (p[2] - center[2]).powi(2))
                    .sqrt();
                    if dc > radius {
                        continue;
                    }
                    let mut sum = 0.0;
                    for dz in -1i32..=1 {
                        for dy in -1i32..=1 {
                            for dx in -1i32..=1 {
                                sum += at(
                                    (ix as i32 + dx) as u32,
                                    (iy as i32 + dy) as u32,
                                    (iz as i32 + dz) as u32,
                                );
                            }
                        }
                    }
                    let avg = sum / 27.0;
                    let i = self.idx(ix, iy, iz);
                    let wgt = strength * (1.0 - dc / radius).clamp(0.0, 1.0);
                    self.baked.distance[i] += (avg - self.baked.distance[i]) * wgt;
                }
            }
        }
    }

    /// Paint the surface color within a spherical brush (only near the surface, so
    /// you tint what you'd actually see).
    pub fn paint(&mut self, center: [f32; 3], radius: f32, strength: f32, color: [f32; 3]) {
        let [lo, hi] = self.brush_range(center, radius);
        let rgb = [color[0] * 255.0, color[1] * 255.0, color[2] * 255.0];
        let strength = strength.clamp(0.0, 1.0);
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = self.voxel_world(ix, iy, iz);
                    let dc = ((p[0] - center[0]).powi(2)
                        + (p[1] - center[1]).powi(2)
                        + (p[2] - center[2]).powi(2))
                    .sqrt();
                    if dc > radius {
                        continue;
                    }
                    let i = self.idx(ix, iy, iz);
                    // Only paint near the surface (|sdf| small).
                    if self.baked.distance[i].abs() > radius {
                        continue;
                    }
                    let w = strength * (1.0 - dc / radius).clamp(0.0, 1.0);
                    let c = &mut self.baked.color[i];
                    for k in 0..3 {
                        c[k] = (c[k] as f32 + (rgb[k] - c[k] as f32) * w) as u8;
                    }
                }
            }
        }
    }

    /// Paint a terrain texture slot (1-based; 0 clears to untextured) onto the
    /// surface within a brush. Stored in the color alpha; the renderer triplanar-maps
    /// the matching palette layer. `slot` is the palette index + 1.
    pub fn paint_texture(&mut self, center: [f32; 3], radius: f32, slot: u8) {
        let [lo, hi] = self.brush_range(center, radius);
        for iz in lo[2]..=hi[2] {
            for iy in lo[1]..=hi[1] {
                for ix in lo[0]..=hi[0] {
                    let p = self.voxel_world(ix, iy, iz);
                    let dc = ((p[0] - center[0]).powi(2)
                        + (p[1] - center[1]).powi(2)
                        + (p[2] - center[2]).powi(2))
                    .sqrt();
                    if dc > radius {
                        continue;
                    }
                    let i = self.idx(ix, iy, iz);
                    // Only where there's a surface nearby (so we don't paint deep
                    // interior cells you'll never see).
                    if self.baked.distance[i].abs() <= radius {
                        self.baked.color[i][3] = slot;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_ground_surface_at_zero() {
        let t = Terrain::flat([32, 24, 32], [0.0, 0.0, 0.0], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        // sample above ground is positive, below is negative
        assert!(t.sample([0.0, 2.0, 0.0]) > 0.0);
        assert!(t.sample([0.0, -2.0, 0.0]) < 0.0);
        // a downward ray from above hits near y=0
        let hit = t.raycast([0.0, 5.0, 0.0], [0.0, -1.0, 0.0]).expect("ray hits ground");
        assert!(hit[1].abs() < 0.3, "hit y={}", hit[1]);
    }

    #[test]
    fn bytes_round_trip() {
        let mut t = Terrain::flat([20, 16, 20], [1.0, 0.0, -2.0], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 2.0, 1.0);
        t.paint_texture([0.0, 0.5, 0.0], 2.0, 2);
        let bytes = t.to_bytes();
        let back = Terrain::from_bytes(&bytes).expect("parses");
        assert_eq!(t.baked.dims, back.baked.dims);
        assert_eq!(t.baked.center, back.baked.center);
        assert_eq!(t.baked.distance, back.baked.distance);
        assert_eq!(t.baked.color, back.baked.color);
        assert!(Terrain::from_bytes(b"nope").is_none());
    }

    #[test]
    fn raise_makes_a_bump() {
        let mut t = Terrain::flat([48, 32, 48], [0.0, 0.0, 0.0], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        // Repeatedly raise at the origin -> the surface should rise above 0.
        for _ in 0..40 {
            t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 2.0, 1.0);
        }
        let hit = t.raycast([0.0, 5.0, 0.0], [0.0, -1.0, 0.0]).expect("hits raised ground");
        assert!(hit[1] > 0.3, "expected a bump, hit y={}", hit[1]);
    }
}
