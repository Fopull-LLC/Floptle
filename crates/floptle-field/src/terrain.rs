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

/// The SHAPE of a brush's weight from its center to its rim — the thing that decides
/// whether a stroke reads as a soft airbrush or a hard stamp.
///
/// Every brush in the editor (terrain sculpt/paint AND vertex paint) runs through this,
/// because both used to hardcode `w = strength * (1 - d/radius)` — a fixed linear ramp.
/// That is *why* everything looked blurry: there was no profile to configure, only one
/// soft gradient. Two knobs, deliberately, in the shape artists already know:
///
/// * `hardness` — the fraction of the radius that gets FULL weight before any falloff
///   starts. `1.0` = a hard-edged stamp with no gradient at all (the N64/PS1 look);
///   `0.0` = falloff across the entire radius (an airbrush).
/// * `falloff` — the shape of the ramp over the remaining rim.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BrushProfile {
    pub hardness: f32,
    pub falloff: Falloff,
}

/// The ramp shape from the hard core out to the rim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Falloff {
    /// Smoothstep — the soft, blendy default.
    Smooth,
    /// Straight line. Predictable; what the old hardcoded brushes always did.
    Linear,
    /// Squared — clings to full strength then drops away late. Good for crisp-ish
    /// edges that still feather slightly.
    Sharp,
    /// Inverse of Sharp — drops immediately, trails off. A wide, faint halo.
    Soft,
}

impl Default for BrushProfile {
    fn default() -> Self {
        // Slightly hard by default: the old fully-linear ramp is what read as "blurry".
        Self { hardness: 0.35, falloff: Falloff::Smooth }
    }
}

impl BrushProfile {
    /// A hard-edged stamp — no gradient anywhere inside the radius.
    pub fn hard() -> Self {
        Self { hardness: 1.0, falloff: Falloff::Linear }
    }

    /// Weight at distance `d` from the brush center, for a brush of `radius`.
    /// Returns 0 outside the radius, 1 inside the hard core.
    pub fn weight(&self, d: f32, radius: f32) -> f32 {
        if radius <= 0.0 {
            return 0.0;
        }
        let t = d / radius;
        // Outside the radius FIRST. Clamping t to 1 before this test made `t <= hardness`
        // true at ANY distance when hardness == 1 — i.e. a hard brush painted the whole
        // mesh, ignoring its radius entirely. (Caught by
        // `every_profile_is_bounded_and_dies_at_the_rim`; keep that test.)
        if t >= 1.0 {
            return 0.0;
        }
        let h = self.hardness.clamp(0.0, 1.0);
        if t <= h {
            return 1.0; // inside the hard core (hardness 1 ⇒ the whole disc)
        }
        // u: 0 at the core edge → 1 at the rim. s: the remaining strength.
        let u = ((t - h) / (1.0 - h)).clamp(0.0, 1.0);
        let s = 1.0 - u;
        match self.falloff {
            Falloff::Linear => s,
            Falloff::Smooth => s * s * (3.0 - 2.0 * s),
            Falloff::Sharp => 1.0 - u * u,
            Falloff::Soft => s * s,
        }
    }
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

    /// surface as the `.tfield` it came from.
    pub fn baked_distance_at(&self, p: [f32; 3]) -> f32 {
        let b = &self.baked;
        let [w, h, d] = b.dims;
        let vs = [
            2.0 * b.half_extent[0] / (w.max(2) - 1) as f32,
            2.0 * b.half_extent[1] / (h.max(2) - 1) as f32,
            2.0 * b.half_extent[2] / (d.max(2) - 1) as f32,
        ];
        let lo = [
            b.center[0] - b.half_extent[0],
            b.center[1] - b.half_extent[1],
            b.center[2] - b.half_extent[2],
        ];
        let g = [(p[0] - lo[0]) / vs[0], (p[1] - lo[1]) / vs[1], (p[2] - lo[2]) / vs[2]];
        let i = [g[0].floor() as i32, g[1].floor() as i32, g[2].floor() as i32];
        let f = [g[0] - i[0] as f32, g[1] - i[1] as f32, g[2] - i[2] as f32];
        let at = |dx: i32, dy: i32, dz: i32| {
            let x = (i[0] + dx).clamp(0, w as i32 - 1) as u32;
            let y = (i[1] + dy).clamp(0, h as i32 - 1) as u32;
            let z = (i[2] + dz).clamp(0, d as i32 - 1) as u32;
            b.distance[((z * h + y) * w + x) as usize]
        };
        let l = |a: f32, bb: f32, t: f32| a + (bb - a) * t;
        let x00 = l(at(0, 0, 0), at(1, 0, 0), f[0]);
        let x10 = l(at(0, 1, 0), at(1, 1, 0), f[0]);
        let x01 = l(at(0, 0, 1), at(1, 0, 1), f[0]);
        let x11 = l(at(0, 1, 1), at(1, 1, 1), f[0]);
        l(l(x00, x10, f[1]), l(x01, x11, f[1]), f[2])
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
        let [w, h, d] = self.baked.dims;
        // Continuous grid coords (voxel centers at integer+0.5). For a point outside the
        // box these clamp to the edge, so the trilinear value below is the nearest EDGE
        // voxel's distance.
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
        let interior = lerp(lerp(c00, c10, fy), lerp(c01, c11, fy), fz);
        if outside > 1e-4 {
            // Outside the box, continue the field as AIR: box distance PLUS the nearest
            // edge's air gap. Without this the box face dips to ~0 even in mid-air, and
            // that near-zero "shell" reappears when terrains are combined (each terrain's
            // box face becomes a faint membrane). A solid edge (interior ≤ 0) gives a
            // clean cliff instead. Mirrors the `grow()` air-fill exactly.
            return outside + interior.max(0.0);
        }
        interior
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
    /// translation, full `f64`), into ONE [`Terrain`] for rendering. Overlaps blend
    /// via [`smin`] (the same polynomial smooth-min the GPU uses), so terrains fuse
    /// seamlessly; painted color/slot blend toward the nearer surface.
    ///
    /// Returns `(anchor, field)`: the field's own coordinates are relative to the
    /// returned **`f64` anchor** (the union's snapped world minimum), never absolute
    /// world space — so the fold is exact no matter how far out the terrains sit
    /// (ADR-0015). Reconstruct world positions as `anchor + local`. All the interior
    /// math is `f64` already; only small residuals are ever narrowed to `f32`.
    ///
    /// `k` is the blend radius. Voxel size = the finest of the sources (per axis),
    /// clamped so no axis exceeds `MAX_DIM` cells (far-apart terrains share a coarser
    /// grid — the documented resolution-spread trade-off).
    pub fn combine(volumes: &[([f64; 3], &Terrain)], k: f32) -> ([f64; 3], Terrain) {
        const MAX_DIM: u32 = 256;
        if volumes.is_empty() {
            return ([0.0; 3], Terrain::flat([1, 1, 1], [0.0; 3], [1.0; 3], 0.0, [1.0; 3]));
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
            // Anchor-relative: the field's frame starts at `world_min`, so its center
            // is just half the span — a small, exact number at any world position.
            center[a] = (span * 0.5) as f32;
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
        (world_min, Terrain { baked: BakedSdf { dims, center, half_extent: half, distance, color } })
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

    /// The voxel index range (inclusive) covering the world AABB of a brush. Public so
    /// the editor can upload just this sub-box to the GPU after a paint dab.
    pub fn brush_range(&self, center: [f32; 3], radius: f32) -> [[u32; 3]; 2] {
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
    /// only cells the brush actually changes are written. Returns the inclusive voxel
    /// AABB of the cells that actually changed (`None` if nothing did), so the editor can
    /// upload just that sub-box to the GPU instead of the whole volume.
    pub fn sculpt(
        &mut self,
        brush: Brush,
        center: [f32; 3],
        radius: f32,
        strength: f32,
        profile: BrushProfile,
    ) -> Option<[[u32; 3]; 2]> {
        match brush {
            Brush::Smooth => return self.smooth(center, radius, strength),
            Brush::Flatten => return self.flatten(center, radius, strength, profile),
            Brush::Paint => return None,
            _ => {}
        }
        let s = strength.clamp(0.02, 1.0);
        let [w, h, d] = self.baked.dims;
        let mut lo = [u32::MAX; 3];
        let mut hi = [0u32; 3];
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
                        lo = [lo[0].min(ix), lo[1].min(iy), lo[2].min(iz)];
                        hi = [hi[0].max(ix), hi[1].max(iy), hi[2].max(iz)];
                    }
                }
            }
        }
        (lo[0] <= hi[0]).then_some([lo, hi])
    }

    /// Level the brushed region toward the height the stroke landed on (`center.y`).
    fn flatten(
        &mut self,
        center: [f32; 3],
        radius: f32,
        strength: f32,
        profile: BrushProfile,
    ) -> Option<[[u32; 3]; 2]> {
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
                    let wgt = s * profile.weight(dxz, radius);
                    self.baked.distance[i] = cur + (target - cur) * wgt;
                }
            }
        }
        Some([lo, hi])
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
    fn smooth(&mut self, center: [f32; 3], radius: f32, strength: f32) -> Option<[[u32; 3]; 2]> {
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
        Some([lo, hi])
    }

    /// Paint the surface color within a spherical brush (only near the surface, so
    /// you tint what you'd actually see).
    pub fn paint(
        &mut self,
        center: [f32; 3],
        radius: f32,
        strength: f32,
        color: [f32; 3],
        profile: BrushProfile,
    ) {
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
                    let w = strength * profile.weight(dc, radius);
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

    /// Sphere tracing (and everything that marches the field — shading normals, SDF
    /// AO, sun shadows) assumes the field is 1-Lipschitz: |∇d| ≤ 1, i.e. `d` never
    /// grows faster than distance itself can. Break that and the march STEPS PAST the
    /// surface; the symptom is blotchy AO and speckle, not an obvious crash.
    ///
    /// `grow` used to fill new cells with `box_distance(..) + edge_air`. Summing two
    /// distance-like terms sums their gradients — two aligned unit gradients give 2.0.
    /// Measured on Ty's real 289×271×307 field: 14.4% of near-surface voxels violated
    /// the bound, p95 |∇d| = 2.25. That ~2.0 IS the sum. `max` unions the same two
    /// bounds while staying 1-Lipschitz.
    #[test]
    fn a_hard_brush_has_no_gradient_and_a_soft_one_is_all_gradient() {
        let hard = BrushProfile::hard();
        // Full weight everywhere inside the radius — this is the flat, stamped retro
        // edge that the old hardcoded linear ramp made impossible.
        for d in [0.0, 0.5, 0.9, 0.999] {
            assert_eq!(hard.weight(d, 1.0), 1.0, "hard brush must be flat at d={d}");
        }
        assert_eq!(hard.weight(1.0, 1.0), 0.0, "…and stop dead at the rim");

        let soft = BrushProfile { hardness: 0.0, falloff: Falloff::Smooth };
        assert_eq!(soft.weight(0.0, 1.0), 1.0);
        assert!(soft.weight(0.5, 1.0) < 0.9, "a soft brush must actually fall off");
        assert_eq!(soft.weight(1.0, 1.0), 0.0);
    }

    #[test]
    fn hardness_sets_the_size_of_the_full_strength_core() {
        let p = BrushProfile { hardness: 0.5, falloff: Falloff::Linear };
        assert_eq!(p.weight(0.0, 1.0), 1.0);
        assert_eq!(p.weight(0.5, 1.0), 1.0, "the core edge is still full strength");
        // Half way across the remaining rim → half weight (linear).
        assert!((p.weight(0.75, 1.0) - 0.5).abs() < 1e-5, "got {}", p.weight(0.75, 1.0));
        assert_eq!(p.weight(1.0, 1.0), 0.0);
    }

    #[test]
    fn every_profile_is_bounded_and_dies_at_the_rim() {
        for falloff in [Falloff::Smooth, Falloff::Linear, Falloff::Sharp, Falloff::Soft] {
            for h in [0.0, 0.25, 0.5, 0.75, 1.0] {
                let p = BrushProfile { hardness: h, falloff };
                for i in 0..=20 {
                    let w = p.weight(i as f32 / 20.0, 1.0);
                    assert!((0.0..=1.0).contains(&w), "{falloff:?}/{h}: weight {w} out of range");
                }
                assert_eq!(p.weight(1.5, 1.0), 0.0, "{falloff:?}/{h}: outside the radius must be 0");
            }
        }
        // A degenerate radius must not divide by zero.
        assert_eq!(BrushProfile::default().weight(1.0, 0.0), 0.0);
    }

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
        t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 2.0, 1.0, BrushProfile::default());
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
    fn sculpt_returns_changed_aabb() {
        let mut t = Terrain::flat([32, 24, 32], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.4, 0.7, 0.3]);
        // Raise a bump at the center — the returned box must cover the brush center voxel.
        let bb = t.sculpt(Brush::Raise, [0.0, 0.0, 0.0], 3.0, 1.0, BrushProfile::default()).expect("raise changed cells");
        let [lo, hi] = bb;
        for a in 0..3 {
            assert!(lo[a] <= hi[a], "axis {a}: lo {} hi {}", lo[a], hi[a]);
            assert!(hi[a] < t.baked.dims[a], "in-bounds");
        }
        // The center voxel (≈ mid index) is inside the changed box.
        let mid = [t.baked.dims[0] / 2, t.baked.dims[1] / 2, t.baked.dims[2] / 2];
        for a in 0..3 {
            assert!(lo[a] <= mid[a] && mid[a] <= hi[a], "center axis {a} outside box");
        }
        // Painting reports no geometry change (returns None from sculpt's Paint arm).
        assert!(t.sculpt(Brush::Paint, [0.0; 3], 3.0, 1.0, BrushProfile::default()).is_none());
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
        let (anchor, c) = Terrain::combine(&[([0.0, 0.0, 0.0], &a), ([10.0, 0.0, 0.0], &b)], 1.0);
        // The field is anchor-relative; world = anchor + local.
        let s = |x: f32, y: f32, z: f32| {
            c.sample([x - anchor[0] as f32, y - anchor[1] as f32, z - anchor[2] as f32])
        };
        // Combined world box spans roughly x in [-8, 18].
        let cx = anchor[0] as f32 + c.baked.center[0];
        assert!(cx > 3.0 && cx < 7.0, "world center x {cx}");
        assert!(c.baked.half_extent[0] >= 12.0, "half {:?}", c.baked.half_extent);
        // Ground is flat at y=0 across the whole span (above positive, below negative).
        for &x in &[-6.0f32, 0.0, 5.0, 10.0, 16.0] {
            assert!(s(x, 2.0, 0.0) > 0.0, "above ground at x={x}");
            assert!(s(x, -2.0, 0.0) < 0.0, "below ground at x={x}");
        }
        // A downward ray in the overlap hits near y=0 (one fused surface, no double).
        let hit = c
            .raycast(
                [5.0 - anchor[0] as f32, 6.0 - anchor[1] as f32, 0.0 - anchor[2] as f32],
                [0.0, -1.0, 0.0],
            )
            .expect("hits ground in seam");
        let hit_y = anchor[1] as f32 + hit[1];
        assert!(hit_y.abs() < 0.4, "seam hit y={hit_y}");
    }

    #[test]
    fn combine_is_exact_far_from_world_origin() {
        // ADR-0015: the fold must not lose precision when the terrains sit millions of
        // units out — the largeness lives in the f64 anchor, the field stays small.
        let a = Terrain::flat([32, 24, 32], [0.0; 3], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        let far = 1.0e7f64;
        let (anchor, c) = Terrain::combine(&[([far, 0.0, far], &a)], 1.0);
        // The anchor carries the world offset; the field's own numbers are small.
        assert!(anchor[0] > far - 100.0 && anchor[0] < far + 100.0, "anchor {anchor:?}");
        assert!(c.baked.center[0].abs() < 100.0, "center is local: {:?}", c.baked.center);
        // Ground surface exactly at world y = 0 (sampled anchor-relative).
        let lx = (far - anchor[0]) as f32; // volume center in field coords
        assert!(c.sample([lx, 2.0 - anchor[1] as f32, lx]) > 0.0, "air above");
        assert!(c.sample([lx, -2.0 - anchor[1] as f32, lx]) < 0.0, "solid below");
    }

    #[test]
    fn raise_makes_a_bump() {
        let mut t = Terrain::flat([48, 32, 48], [0.0, 0.0, 0.0], [8.0, 6.0, 8.0], 0.0, [0.4, 0.7, 0.3]);
        // Repeatedly raise at the origin -> the surface should rise above 0.
        for _ in 0..40 {
            t.sculpt(Brush::Raise, [0.0, 0.5, 0.0], 2.0, 1.0, BrushProfile::default());
        }
        let hit = t.raycast([0.0, 5.0, 0.0], [0.0, -1.0, 0.0]).expect("hits raised ground");
        assert!(hit[1] > 0.3, "expected a bump, hit y={}", hit[1]);
    }
}
