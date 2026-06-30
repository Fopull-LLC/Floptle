//! Editable terrain — a signed-distance voxel field you sculpt and paint in the
//! editor, then raymarch (it reuses the volume path the mesh→SDF bake already
//! feeds: a [`BakedSdf`] of distance + color the renderer uploads as 3D textures).
//!
//! The field is a regular grid spanning `[center - half_extent, center + half]`,
//! `x`-fastest. Sculpting edits the distance grid with smooth CSG (union to raise,
//! subtraction to dig); painting edits the co-located color grid. Both are CPU-side
//! so they stay simple and undoable; the editor re-uploads after each stroke.

use crate::mesh2sdf::BakedSdf;
use crate::smin;

/// Signed distance from point `p` to an axis-aligned box (`center` ± `half`): positive
/// outside, negative inside. The standard analytic box SDF.
fn box_distance(p: [f32; 3], center: [f32; 3], half: [f32; 3]) -> f32 {
    let q = [
        (p[0] - center[0]).abs() - half[0],
        (p[1] - center[1]).abs() - half[1],
        (p[2] - center[2]).abs() - half[2],
    ];
    let ox = q[0].max(0.0);
    let oy = q[1].max(0.0);
    let oz = q[2].max(0.0);
    let outside = (ox * ox + oy * oy + oz * oz).sqrt();
    let inside = q[0].max(q[1]).max(q[2]).min(0.0);
    outside + inside
}

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

    /// Grow the grid (keeping voxel size constant) so a brush at local `point`
    /// with `margin` stays comfortably inside the field — sculpting near an edge
    /// extends the *bounds* outward so there's room to keep sculpting. The new area
    /// is filled with AIR (not a copy of the edge voxel), so the existing terrain is
    /// NOT dragged outward into flat land — it stays a finite shape and you can sculpt
    /// isolated features (spires, islands, cliffs). Use [`fill_bounds`](Self::fill_bounds)
    /// to deliberately lay flat ground across the bounds. All three axes can grow.
    /// Returns true if the grid actually changed — the caller must re-upload to the GPU.
    pub fn ensure_contains(&mut self, point: [f32; 3], margin: f32) -> bool {
        // A generous per-axis cell cap so an endless drag can't exhaust memory.
        const MAX_DIM: u32 = 384;
        let dims = self.baked.dims;
        let c = self.baked.center;
        let hf = self.baked.half_extent;
        let mut add_lo = [0u32; 3];
        let mut add_hi = [0u32; 3];
        for i in 0..3 {
            let vs = 2.0 * hf[i] / dims[i] as f32; // voxel size on this axis
            let lo = c[i] - hf[i];
            let hi = c[i] + hf[i];
            // Grow in chunks so we don't reallocate on every dab near an edge.
            let chunk = (margin * 2.0).max(vs * 8.0);
            if point[i] - margin < lo {
                add_lo[i] = ((lo - (point[i] - margin) + chunk) / vs).ceil() as u32;
            }
            if point[i] + margin > hi {
                add_hi[i] = (((point[i] + margin) - hi + chunk) / vs).ceil() as u32;
            }
        }
        // Clamp each axis to the cell cap (shave the high side first).
        for i in 0..3 {
            let room = MAX_DIM.saturating_sub(dims[i]);
            let want = add_lo[i] + add_hi[i];
            if want > room {
                let mut excess = want - room;
                let cut = excess.min(add_hi[i]);
                add_hi[i] -= cut;
                excess -= cut;
                add_lo[i] -= excess.min(add_lo[i]);
            }
        }
        if add_lo.iter().chain(&add_hi).all(|&a| a == 0) {
            return false;
        }
        self.grow(add_lo, add_hi);
        true
    }

    /// Reallocate the grid with `add_lo`/`add_hi` extra cells per axis. Old cells copy
    /// across unchanged; new border cells are filled with AIR (the signed distance to
    /// the OLD bounds box, which is positive outside it) so the terrain ends at a clean
    /// edge rather than the ground being smeared outward. New cells inherit the nearest
    /// edge COLOR so a cliff face is tinted like the terrain. Voxel size is held
    /// constant, so `center`/`half_extent` are recomputed from the new cell counts.
    fn grow(&mut self, add_lo: [u32; 3], add_hi: [u32; 3]) {
        let old = self.baked.clone();
        let [ow, oh, od] = old.dims;
        let new_dims = [ow + add_lo[0] + add_hi[0], oh + add_lo[1] + add_hi[1], od + add_lo[2] + add_hi[2]];
        let vs = [
            2.0 * old.half_extent[0] / ow as f32,
            2.0 * old.half_extent[1] / oh as f32,
            2.0 * old.half_extent[2] / od as f32,
        ];
        let new_half = [
            new_dims[0] as f32 * vs[0] * 0.5,
            new_dims[1] as f32 * vs[1] * 0.5,
            new_dims[2] as f32 * vs[2] * 0.5,
        ];
        // new_lo = old_lo - add_lo*vs ; new_center = new_lo + new_half
        let new_lo = [
            (old.center[0] - old.half_extent[0]) - add_lo[0] as f32 * vs[0],
            (old.center[1] - old.half_extent[1]) - add_lo[1] as f32 * vs[1],
            (old.center[2] - old.half_extent[2]) - add_lo[2] as f32 * vs[2],
        ];
        let new_center =
            [new_lo[0] + new_half[0], new_lo[1] + new_half[1], new_lo[2] + new_half[2]];
        let n = (new_dims[0] * new_dims[1] * new_dims[2]) as usize;
        let mut distance = vec![0.0f32; n];
        let mut color = vec![[0u8; 4]; n];
        let clamp = |v: i64, hi: u32| v.clamp(0, hi as i64 - 1) as u32;
        for nz in 0..new_dims[2] {
            let iz = nz as i64 - add_lo[2] as i64;
            for ny in 0..new_dims[1] {
                let iy = ny as i64 - add_lo[1] as i64;
                for nx in 0..new_dims[0] {
                    let ix = nx as i64 - add_lo[0] as i64;
                    let ni = ((nz * new_dims[1] + ny) * new_dims[0] + nx) as usize;
                    // Color always edge-clamps so cliff faces keep the terrain's tint.
                    let oi = ((clamp(iz, od) * oh + clamp(iy, oh)) * ow + clamp(ix, ow)) as usize;
                    color[ni] = old.color[oi];
                    let in_old = ix >= 0
                        && ix < ow as i64
                        && iy >= 0
                        && iy < oh as i64
                        && iz >= 0
                        && iz < od as i64;
                    distance[ni] = if in_old {
                        old.distance[oi]
                    } else {
                        // Continue the field outward so the terrain ends cleanly. The
                        // box distance gives a vertical cliff where the old edge was
                        // SOLID; adding the old edge's air gap (`.max(0.0)`) keeps AIR
                        // as air — without it, the field dips to ~0 at the old boundary
                        // even up in the sky, and that thin near-zero "shell" is what
                        // the raymarch speckled and the sculpt brush kept colliding with.
                        let wp = [
                            new_lo[0] + (nx as f32 + 0.5) * vs[0],
                            new_lo[1] + (ny as f32 + 0.5) * vs[1],
                            new_lo[2] + (nz as f32 + 0.5) * vs[2],
                        ];
                        box_distance(wp, old.center, old.half_extent) + old.distance[oi].max(0.0)
                    };
                }
            }
        }
        self.baked = BakedSdf { dims: new_dims, center: new_center, half_extent: new_half, distance, color };
    }

    /// Lay a flat slab of solid ground across the bounds, unioned with what's already
    /// there: every voxel inside the box `[walls ± inset] × [floor_y, top_y]` is filled
    /// solid (taller existing features survive; pits fill up to `top_y`). This is the
    /// deliberate counterpart to edge-sculpt no longer auto-expanding the ground — pour
    /// in flat land where you want it, sized by `inset` (margin from the X/Z walls) and
    /// bounded vertically by `floor_y`..`top_y`. Filled cells take `color`.
    pub fn fill_bounds(&mut self, top_y: f32, floor_y: f32, inset: f32, color: [f32; 3]) {
        let b = &self.baked;
        let (lo, hi) = ([b.center[0] - b.half_extent[0], b.center[2] - b.half_extent[2]],
                        [b.center[0] + b.half_extent[0], b.center[2] + b.half_extent[2]]);
        let cx = 0.5 * (lo[0] + hi[0]);
        let cz = 0.5 * (lo[1] + hi[1]);
        let hx = (0.5 * (hi[0] - lo[0]) - inset).max(0.0);
        let hz = (0.5 * (hi[1] - lo[1]) - inset).max(0.0);
        let top = top_y.max(floor_y);
        let cy = 0.5 * (floor_y + top);
        let hy = 0.5 * (top - floor_y).max(0.0);
        let rgb = [(color[0] * 255.0) as u8, (color[1] * 255.0) as u8, (color[2] * 255.0) as u8];
        let dims = self.baked.dims;
        for iz in 0..dims[2] {
            for iy in 0..dims[1] {
                for ix in 0..dims[0] {
                    let p = self.voxel_world(ix, iy, iz);
                    let bd = box_distance([p[0], p[1], p[2]], [cx, cy, cz], [hx, hy, hz]);
                    let i = self.idx(ix, iy, iz);
                    if bd < self.baked.distance[i] {
                        self.baked.distance[i] = bd;
                    }
                    if bd <= 0.0 {
                        let c = &mut self.baked.color[i];
                        c[0] = rgb[0];
                        c[1] = rgb[1];
                        c[2] = rgb[2];
                    }
                }
            }
        }
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

    /// Fill the WHOLE terrain's surface color with `color` (the RGB tint), leaving
    /// the shape + texture slots — "fill terrain with this color".
    pub fn fill_color(&mut self, color: [f32; 3]) {
        let rgb = [
            (color[0] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[1] * 255.0).round().clamp(0.0, 255.0) as u8,
            (color[2] * 255.0).round().clamp(0.0, 255.0) as u8,
        ];
        for c in &mut self.baked.color {
            c[0] = rgb[0];
            c[1] = rgb[1];
            c[2] = rgb[2];
        }
    }

    /// Fill the WHOLE terrain with a texture palette `slot` (1-based; 0 = untextured)
    /// — "fill terrain with this texture". Leaves the shape + RGB tint.
    pub fn fill_texture(&mut self, slot: u8) {
        for c in &mut self.baked.color {
            c[3] = slot;
        }
    }

    /// Nearest-voxel color (RGBA8) at a local point; clamps to the grid outside the
    /// box. The alpha (painted texture slot) is taken from the nearest voxel, never
    /// interpolated, so slots stay crisp when combining terrains.
    pub fn sample_color(&self, p: [f32; 3]) -> [u8; 4] {
        let [w, h, d] = self.baked.dims;
        let c = self.baked.center;
        let hf = self.baked.half_extent;
        let g = |r: f32, ci: f32, hi: f32, n: u32| {
            (((r - ci) / (2.0 * hi) + 0.5) * n as f32 - 0.5).clamp(0.0, n as f32 - 1.0)
        };
        let x = g(p[0], c[0], hf[0], w).round() as u32;
        let y = g(p[1], c[1], hf[1], h).round() as u32;
        let z = g(p[2], c[2], hf[2], d).round() as u32;
        self.baked.color[self.idx(x.min(w - 1), y.min(h - 1), z.min(d - 1))]
    }

    /// Fold several terrain fields, each placed at a WORLD `origin` (its node's world
    /// translation), into ONE world-space [`Terrain`] for rendering. Overlaps blend
    /// via [`smin`] (the same polynomial smooth-min the GPU uses), so terrains fuse
    /// seamlessly; painted color/slot blend toward the nearer surface. The combined
    /// field's own local space IS world space (its `baked.center` is the world union
    /// center), so the editor renders it with no extra transform.
    ///
    /// `k` is the blend radius. Voxel size = the finest of the sources (per axis),
    /// clamped so no axis exceeds `MAX_DIM` cells (far-apart terrains share a coarser
    /// grid — the documented resolution-spread trade-off).
    pub fn combine(volumes: &[([f64; 3], &Terrain)], k: f32) -> Terrain {
        const MAX_DIM: u32 = 256;
        if volumes.is_empty() {
            return Terrain::flat([1, 1, 1], [0.0; 3], [1.0; 3], 0.0, [1.0; 3]);
        }
        // Union world bounds + finest per-axis voxel size.
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        let mut vs = [f64::INFINITY; 3];
        for (origin, t) in volumes {
            for a in 0..3 {
                let c = t.baked.center[a] as f64 + origin[a];
                let h = t.baked.half_extent[a] as f64;
                lo[a] = lo[a].min(c - h);
                hi[a] = hi[a].max(c + h);
                vs[a] = vs[a].min(2.0 * h / t.baked.dims[a].max(1) as f64);
            }
        }
        // Snap the world origin to the lattice (so a sub-voxel move of one terrain
        // doesn't reshuffle every cell), then size the grid, clamping cells per axis.
        let mut dims = [0u32; 3];
        let mut center = [0f32; 3];
        let mut half = [0f32; 3];
        let mut world_min = [0f64; 3];
        let mut cell = [0f64; 3];
        for a in 0..3 {
            let mut v = vs[a].max(1e-3);
            world_min[a] = (lo[a] / v).floor() * v;
            let extent = (hi[a] - world_min[a]).max(v);
            let mut n = (extent / v).ceil() as u32 + 1;
            n = n.clamp(2, MAX_DIM);
            v = extent / (n as f64 - 1.0); // keep the box spanning the full extent
            let span = v * n as f64;
            dims[a] = n;
            cell[a] = v;
            half[a] = (span * 0.5) as f32;
            center[a] = (world_min[a] + span * 0.5) as f32;
        }
        let [w, h, d] = dims;
        let n = (w * h * d) as usize;
        let mut distance = vec![1.0e4f32; n];
        let mut color = vec![[255u8, 255, 255, 0]; n];
        let kk = k.max(1e-4);
        for iz in 0..d {
            let wz = world_min[2] + (iz as f64 + 0.5) * cell[2];
            for iy in 0..h {
                let wy = world_min[1] + (iy as f64 + 0.5) * cell[1];
                for ix in 0..w {
                    let wx = world_min[0] + (ix as f64 + 0.5) * cell[0];
                    let mut dval = 1.0e4f32;
                    let mut col = [255u8, 255, 255, 0];
                    for (origin, t) in volumes {
                        let lp = [
                            (wx - origin[0]) as f32,
                            (wy - origin[1]) as f32,
                            (wz - origin[2]) as f32,
                        ];
                        // AABB early-out: a volume whose box is farther than `dval+k`
                        // can't lower the smin result, so skip its (8-tap) sample.
                        let rel = [
                            lp[0] - t.baked.center[0],
                            lp[1] - t.baked.center[1],
                            lp[2] - t.baked.center[2],
                        ];
                        let qx = rel[0].abs() - t.baked.half_extent[0];
                        let qy = rel[1].abs() - t.baked.half_extent[1];
                        let qz = rel[2].abs() - t.baked.half_extent[2];
                        if qx.max(qy).max(qz) > dval + kk {
                            continue;
                        }
                        let sd = t.sample(lp);
                        let scol = t.sample_color(lp);
                        // smin (distance) + matched color crossfade by the same weight.
                        let hh = (0.5 + 0.5 * (sd - dval) / kk).clamp(0.0, 1.0);
                        dval = smin(dval, sd, k);
                        for c in 0..3 {
                            col[c] = (scol[c] as f32 * (1.0 - hh) + col[c] as f32 * hh)
                                .round()
                                .clamp(0.0, 255.0) as u8;
                        }
                        // Slot (alpha) from the NEARER surface, never blended.
                        col[3] = if hh >= 0.5 { col[3] } else { scol[3] };
                    }
                    let oi = ((iz * h + iy) * w + ix) as usize;
                    distance[oi] = dval;
                    color[oi] = col;
                }
            }
        }
        Terrain { baked: BakedSdf { dims, center, half_extent: half, distance, color } }
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
    fn ensure_contains_grows_bounds_not_surface() {
        let mut t = Terrain::flat([32, 24, 32], [0.0, 0.0, 0.0], [16.0, 6.0, 16.0], 0.0, [0.4, 0.7, 0.3]);
        // Voxel size before growth — must stay constant after.
        let vs0 = 2.0 * t.baked.half_extent[0] / t.baked.dims[0] as f32;
        let before = t.baked.dims;
        // Brush well past the +X edge (box reaches +16) forces horizontal growth.
        let grew = t.ensure_contains([40.0, 0.0, 0.0], 2.0);
        assert!(grew, "should have expanded");
        assert!(t.baked.dims[0] > before[0], "x grew");
        assert_eq!(t.baked.dims[1], before[1], "y unchanged (brush at surface)");
        let vs1 = 2.0 * t.baked.half_extent[0] / t.baked.dims[0] as f32;
        assert!((vs0 - vs1).abs() < 1e-4, "voxel size held constant");
        assert!(t.baked.center[0] + t.baked.half_extent[0] >= 40.0, "box reaches the brush");
        // The ORIGINAL terrain (inside the old box) is preserved...
        assert!(t.sample([10.0, 2.0, 0.0]) > 0.0, "above ground inside old box");
        assert!(t.sample([10.0, -2.0, 0.0]) < 0.0, "below ground inside old box");
        // ...but the NEW area is AIR, not extended flat land (the whole point: bounds
        // grew, the surface did not follow). Below where ground used to be is now empty.
        assert!(t.sample([34.0, -2.0, 0.0]) > 0.0, "new area is air below, not solid");
        assert!(t.sample([34.0, 2.0, 0.0]) > 0.0, "new area is air above too");
        // Idempotent: a brush back inside the (now larger) box doesn't grow it.
        let dims = t.baked.dims;
        assert!(!t.ensure_contains([0.0, 0.0, 0.0], 2.0));
        assert_eq!(t.baked.dims, dims);
    }

    #[test]
    fn fill_bounds_lays_flat_ground() {
        // Start from an all-air field (ground far below the box), then fill up to y=0.
        let mut t = Terrain::flat([24, 24, 24], [0.0; 3], [8.0, 8.0, 8.0], -100.0, [0.4, 0.7, 0.3]);
        assert!(t.sample([0.0, -2.0, 0.0]) > 0.0, "starts as air");
        t.fill_bounds(0.0, -8.0, 1.0, [0.5, 0.5, 0.5]);
        // Inside the inset footprint: solid below y=0, air above.
        assert!(t.sample([0.0, -2.0, 0.0]) < 0.0, "filled solid below top");
        assert!(t.sample([0.0, 2.0, 0.0]) > 0.0, "air above the fill height");
        // The inset keeps a margin from the walls: near the +X wall (x≈7.5) stays air.
        assert!(t.sample([7.6, -2.0, 0.0]) > 0.0, "inset margin near the wall is air");
    }

    #[test]
    fn fill_sets_every_voxel() {
        let mut t = Terrain::flat([8, 8, 8], [0.0; 3], [4.0, 4.0, 4.0], 0.0, [0.4, 0.7, 0.3]);
        t.fill_color([1.0, 0.0, 0.0]);
        assert!(t.baked.color.iter().all(|c| c[0] == 255 && c[1] == 0 && c[2] == 0));
        t.fill_texture(3);
        assert!(t.baked.color.iter().all(|c| c[3] == 3));
        // fill_color leaves the slot (alpha) alone.
        t.fill_color([0.0, 0.0, 1.0]);
        assert!(t.baked.color.iter().all(|c| c[3] == 3 && c[2] == 255));
    }

    #[test]
    fn combine_two_overlapping_terrains_blends() {
        // Two flat terrains, offset so their boxes overlap. The combined field must
        // span both, have ground near y=0 across the seam, and be solid below.
        let a = Terrain::flat([32, 24, 32], [0.0; 3], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        let b = Terrain::flat([32, 24, 32], [0.0; 3], [8.0, 6.0, 8.0], 0.0, [0.3, 0.4, 0.7]);
        // place b shifted +10 in x (boxes overlap from x=2..8 of a / -8..-2 of b world)
        let c = Terrain::combine(&[([0.0, 0.0, 0.0], &a), ([10.0, 0.0, 0.0], &b)], 1.0);
        // Combined world box spans roughly x in [-8, 18].
        assert!(c.baked.center[0] > 3.0 && c.baked.center[0] < 7.0, "center {:?}", c.baked.center);
        assert!(c.baked.half_extent[0] >= 12.0, "half {:?}", c.baked.half_extent);
        // Ground is flat at y=0 across the whole span (above positive, below negative).
        for &x in &[-6.0f32, 0.0, 5.0, 10.0, 16.0] {
            assert!(c.sample([x, 2.0, 0.0]) > 0.0, "above ground at x={x}");
            assert!(c.sample([x, -2.0, 0.0]) < 0.0, "below ground at x={x}");
        }
        // A downward ray in the overlap hits near y=0 (one fused surface, no double).
        let hit = c.raycast([5.0, 6.0, 0.0], [0.0, -1.0, 0.0]).expect("hits ground in seam");
        assert!(hit[1].abs() < 0.4, "seam hit y={}", hit[1]);
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
