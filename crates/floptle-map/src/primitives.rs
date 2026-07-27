//! Blockout primitive generators. All centered at the origin, all one slot
//! ("Default", slot 0), all CCW-from-outside winding, all watertight except
//! `plane` (open sheet) and `arch`/`stairs` (hidden internal seams allowed).

use crate::{Face, MapMesh};
use glam::{Vec2, Vec3};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Which generator built a mesh. Part of [`ShapeSpec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ShapeKind {
    Box,
    Plane,
    Wedge,
    Cylinder,
    Sphere,
    Stairs,
    Arch,
}

impl ShapeKind {
    /// True for shapes with a low end and a high end, where "which way is up"
    /// is a design decision the editor must show and let you flip.
    pub fn rises(self) -> bool {
        matches!(self, ShapeKind::Stairs | ShapeKind::Wedge)
    }
}

/// How a mesh was generated — carried on the [`MapMesh`] for as long as the
/// mesh still IS that shape, so the editor can re-generate it with different
/// parameters (more stair steps, more cylinder sides) instead of making you
/// delete and redraw.
///
/// Any op that moves a vertex or changes the face set clears it (see
/// `ops::touched`): once you have pulled a face, the mesh is no longer the
/// primitive and silently regenerating would throw your edit away.
///
/// `#[serde(default)]` at the container: a spec stored by an older build is
/// missing whatever fields have been added since, and losing the WHOLE mesh to
/// a parse error over one absent knob would be a terrible trade.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShapeSpec {
    pub kind: ShapeKind,
    /// Half-extents in the mesh's own local frame.
    pub half: Vec3,
    pub sides: u32,
    pub rings: u32,
    pub steps: u32,
    pub arch_segments: u32,
    /// Arch opening WIDTH, as a fraction of the shape's half-width.
    pub arch_width: f32,
    /// Arch opening HEIGHT (jamb + arc), as a fraction of the shape's height.
    pub arch_height: f32,
}

impl Default for ShapeSpec {
    fn default() -> Self {
        Self::new(ShapeKind::Box, Vec3::ONE)
    }
}

impl ShapeSpec {
    pub fn new(kind: ShapeKind, half: Vec3) -> Self {
        Self {
            kind,
            half,
            sides: 16,
            rings: 8,
            steps: 8,
            arch_segments: 8,
            arch_width: 0.6,
            arch_height: 0.75,
        }
    }

    /// Generate the mesh this spec describes, tagged with the spec itself.
    /// Round shapes are built at unit radius and stretched, so a non-square
    /// footprint gives an honest ellipse/ellipsoid rather than a scaled node
    /// (node scale would skew the box-projected UVs).
    pub fn build(&self) -> MapMesh {
        let h = Vec3::new(self.half.x.max(0.001), self.half.y.max(0.001), self.half.z.max(0.001));
        let stretch = |mut m: MapMesh, r: Vec3| {
            for v in &mut m.verts {
                *v *= r;
            }
            m
        };
        let mut mesh = match self.kind {
            ShapeKind::Box => box_mesh(h),
            ShapeKind::Plane => plane(Vec2::new(h.x, h.z)),
            ShapeKind::Wedge => wedge(h),
            ShapeKind::Cylinder => {
                stretch(cylinder(1.0, h.y, self.sides), Vec3::new(h.x, 1.0, h.z))
            }
            ShapeKind::Sphere => stretch(sphere(1.0, self.sides, self.rings), h),
            ShapeKind::Stairs => stairs(h * 2.0, self.steps),
            ShapeKind::Arch => arch(
                h,
                Vec2::new(
                    h.x * self.arch_width.clamp(0.05, 0.98),
                    h.y * 2.0 * self.arch_height.clamp(0.05, 0.98),
                ),
                self.arch_segments,
            ),
        };
        mesh.spec = Some(*self);
        mesh
    }
}

/// Incremental builder that welds identical positions (exact f32 bits), so
/// primitives assembled face-by-face come out connected for vertex editing.
struct Builder {
    mesh: MapMesh,
    ids: HashMap<[u32; 3], u32>,
}

impl Builder {
    fn new() -> Self {
        Self { mesh: MapMesh::new(), ids: HashMap::new() }
    }

    fn vid(&mut self, p: Vec3) -> u32 {
        // `-0.0` and `0.0` are different bit patterns but the same point — a
        // trig-generated ring produces both (cos of an obtuse angle times a
        // zero radius), and treating them as separate verts silently tears the
        // mesh open at the poles. Normalize before keying AND before storing.
        let p = p + Vec3::ZERO;
        let key = [p.x.to_bits(), p.y.to_bits(), p.z.to_bits()];
        *self.ids.entry(key).or_insert_with(|| {
            self.mesh.verts.push(p);
            self.mesh.verts.len() as u32 - 1
        })
    }

    fn face(&mut self, pts: &[Vec3]) {
        let verts = pts.iter().map(|&p| self.vid(p)).collect();
        self.mesh.faces.push(Face { verts, slot: 0 });
    }
}

/// Axis-aligned box with half-extents `half` — 6 quad faces.
pub fn box_mesh(half: Vec3) -> MapMesh {
    let (x, y, z) = (half.x, half.y, half.z);
    let p = |sx: f32, sy: f32, sz: f32| Vec3::new(sx * x, sy * y, sz * z);
    let mut b = Builder::new();
    // +Z, -Z, +X, -X, +Y, -Y (each CCW viewed from outside).
    b.face(&[p(-1., -1., 1.), p(1., -1., 1.), p(1., 1., 1.), p(-1., 1., 1.)]);
    b.face(&[p(1., -1., -1.), p(-1., -1., -1.), p(-1., 1., -1.), p(1., 1., -1.)]);
    b.face(&[p(1., -1., 1.), p(1., -1., -1.), p(1., 1., -1.), p(1., 1., 1.)]);
    b.face(&[p(-1., -1., -1.), p(-1., -1., 1.), p(-1., 1., 1.), p(-1., 1., -1.)]);
    b.face(&[p(-1., 1., -1.), p(-1., 1., 1.), p(1., 1., 1.), p(1., 1., -1.)]);
    b.face(&[p(-1., -1., -1.), p(1., -1., -1.), p(1., -1., 1.), p(-1., -1., 1.)]);
    b.mesh
}

/// Single quad facing +Y, half-extents `half` in XZ.
pub fn plane(half: Vec2) -> MapMesh {
    let mut b = Builder::new();
    b.face(&[
        Vec3::new(-half.x, 0.0, -half.y),
        Vec3::new(-half.x, 0.0, half.y),
        Vec3::new(half.x, 0.0, half.y),
        Vec3::new(half.x, 0.0, -half.y),
    ]);
    b.mesh
}

/// Ramp / triangular prism: box footprint `half`, sloping from full height at
/// -Z down to zero at +Z. 2 triangle sides + bottom quad + back quad + slope
/// quad (5 faces).
pub fn wedge(half: Vec3) -> MapMesh {
    let (x, y, z) = (half.x, half.y, half.z);
    let mut b = Builder::new();
    // Bottom (-Y), back (-Z), slope (+Y+Z), sides (+X / -X triangles).
    b.face(&[
        Vec3::new(-x, -y, -z),
        Vec3::new(x, -y, -z),
        Vec3::new(x, -y, z),
        Vec3::new(-x, -y, z),
    ]);
    b.face(&[
        Vec3::new(x, -y, -z),
        Vec3::new(-x, -y, -z),
        Vec3::new(-x, y, -z),
        Vec3::new(x, y, -z),
    ]);
    b.face(&[
        Vec3::new(-x, -y, z),
        Vec3::new(x, -y, z),
        Vec3::new(x, y, -z),
        Vec3::new(-x, y, -z),
    ]);
    b.face(&[Vec3::new(x, -y, -z), Vec3::new(x, y, -z), Vec3::new(x, -y, z)]);
    b.face(&[Vec3::new(-x, -y, -z), Vec3::new(-x, -y, z), Vec3::new(-x, y, -z)]);
    b.mesh
}

/// UV sphere centered at the origin: `rings` latitude bands (>= 2), `segments`
/// longitude divisions (>= 3). Pole bands come out as triangles (the degenerate
/// corner is deduped), everything else as quads.
pub fn sphere(radius: f32, segments: u32, rings: u32) -> MapMesh {
    let n = segments.clamp(3, 128) as usize;
    let m = rings.clamp(2, 128) as usize;
    let at = |ri: usize, si: usize| {
        // The poles are placed EXACTLY (sin(PI) is 8.7e-8, not 0 — trusting it
        // would leave a hair-thin ring instead of a single shared vertex).
        if ri == 0 {
            return Vec3::new(0.0, radius, 0.0);
        }
        if ri == m {
            return Vec3::new(0.0, -radius, 0.0);
        }
        let phi = ri as f32 / m as f32 * std::f32::consts::PI; // 0 = +Y pole
        let theta = si as f32 / n as f32 * std::f32::consts::TAU;
        Vec3::new(
            radius * phi.sin() * theta.cos(),
            radius * phi.cos(),
            radius * phi.sin() * theta.sin(),
        )
    };
    let mut b = Builder::new();
    for ri in 0..m {
        for si in 0..n {
            let sj = (si + 1) % n;
            // (ri,si) -> (ri,sj) -> (ri+1,sj) -> (ri+1,si) winds CCW from
            // outside (same hand as `cylinder`'s walls).
            let quad = [at(ri, si), at(ri, sj), at(ri + 1, sj), at(ri + 1, si)];
            // The pole bands have a doubled corner — drop it rather than emit a
            // degenerate quad.
            let mut pts: Vec<Vec3> = Vec::with_capacity(4);
            for &p in &quad {
                if pts.last() != Some(&p) {
                    pts.push(p);
                }
            }
            if pts.len() > 1 && pts.first() == pts.last() {
                pts.pop();
            }
            if pts.len() >= 3 {
                b.face(&pts);
            }
        }
    }
    b.mesh
}

/// Cylinder along Y: two n-gon caps + `sides` wall quads. `sides >= 3`
/// (clamped).
pub fn cylinder(radius: f32, half_height: f32, sides: u32) -> MapMesh {
    let n = sides.clamp(3, 128) as usize;
    let ring = |y: f32, i: usize| {
        let a = i as f32 / n as f32 * std::f32::consts::TAU;
        Vec3::new(radius * a.cos(), y, radius * a.sin())
    };
    let mut b = Builder::new();
    // Increasing angle winds -Y in this frame: bottom cap direct, top reversed.
    let bottom: Vec<Vec3> = (0..n).map(|i| ring(-half_height, i)).collect();
    let top: Vec<Vec3> = (0..n).rev().map(|i| ring(half_height, i)).collect();
    b.face(&bottom);
    b.face(&top);
    for i in 0..n {
        let j = (i + 1) % n;
        b.face(&[ring(-half_height, i), ring(half_height, i), ring(half_height, j), ring(-half_height, j)]);
    }
    b.mesh
}

/// Staircase filling a `size.x` wide, `size.y` tall, `size.z` deep box
/// (size = full extents, centered), `steps >= 1` equal steps rising toward
/// -Z. Solid: per-step tread + riser, a stacked band per step down each side,
/// plus the back wall and floor.
///
/// The side bands and the back wall carry COLLINEAR corners at every step
/// level. That looks redundant until you drag a face: without them the side of
/// the staircase would have vertices sitting in the middle of the back wall's
/// edge (a T-junction), and pulling the back wall would tear it off the sides
/// instead of stretching them. Every vertex on a face's edge IS a corner of
/// that face — see `assert_no_t_junctions`.
pub fn stairs(size: Vec3, steps: u32) -> MapMesh {
    let n = steps.clamp(1, 256) as usize;
    let (hx, hy, hz) = (size.x * 0.5, size.y * 0.5, size.z * 0.5);
    let (dy, dz) = (size.y / n as f32, size.z / n as f32);
    // Step k occupies z in [zb, zf] and rises to `yhi`; both series are exact
    // at the ends so the shell closes on the bounding box.
    let zf = |k: usize| if k == 0 { hz } else { hz - k as f32 * dz };
    let yhi = |k: usize| if k + 1 == n { hy } else { -hy + (k + 1) as f32 * dy };
    let ylo = |k: usize| if k == 0 { -hy } else { -hy + k as f32 * dy };
    let mut b = Builder::new();
    // Floor over the full footprint (the side bands reach z = -hz at the
    // bottom, so this needs no extra corners).
    b.face(&[
        Vec3::new(-hx, -hy, -hz),
        Vec3::new(hx, -hy, -hz),
        Vec3::new(hx, -hy, hz),
        Vec3::new(-hx, -hy, hz),
    ]);
    // Back wall, with a corner at every band boundary on both vertical edges.
    let mut back = vec![Vec3::new(hx, -hy, -hz), Vec3::new(-hx, -hy, -hz)];
    for k in 0..n {
        back.push(Vec3::new(-hx, yhi(k), -hz));
    }
    back.push(Vec3::new(hx, hy, -hz));
    for k in (0..n - 1).rev() {
        back.push(Vec3::new(hx, yhi(k), -hz));
    }
    b.face(&back);
    for k in 0..n {
        let (front, behind) = (zf(k), zf(k + 1).max(-hz));
        // Riser (faces +Z) then tread (faces +Y).
        b.face(&[
            Vec3::new(-hx, ylo(k), front),
            Vec3::new(hx, ylo(k), front),
            Vec3::new(hx, yhi(k), front),
            Vec3::new(-hx, yhi(k), front),
        ]);
        b.face(&[
            Vec3::new(-hx, yhi(k), behind),
            Vec3::new(-hx, yhi(k), front),
            Vec3::new(hx, yhi(k), front),
            Vec3::new(hx, yhi(k), behind),
        ]);
        // Side bands: one horizontal slab per step, from the back wall out to
        // this step's front, with the next band's end as a collinear corner on
        // the top edge (that corner is where the step above meets this one).
        let mut right = vec![
            Vec3::new(hx, ylo(k), front),
            Vec3::new(hx, ylo(k), -hz),
            Vec3::new(hx, yhi(k), -hz),
        ];
        let mut left = vec![
            Vec3::new(-hx, ylo(k), -hz),
            Vec3::new(-hx, ylo(k), front),
            Vec3::new(-hx, yhi(k), front),
        ];
        // The collinear corner goes where the traversal reaches it — each top
        // edge runs monotonically, so it lands mid-edge, not as a kink.
        if k + 1 < n {
            right.push(Vec3::new(hx, yhi(k), behind));
            left.push(Vec3::new(-hx, yhi(k), behind));
        }
        right.push(Vec3::new(hx, yhi(k), front));
        left.push(Vec3::new(-hx, yhi(k), -hz));
        b.face(&right);
        b.face(&left);
    }
    b.mesh
}

/// Rectangular archway: overall half-extents `half` (X = width, Y = height,
/// Z = depth), with an opening `opening.x` half-wide and `opening.y` tall cut
/// through along Z — vertical jambs up to a springline, capped by a
/// semicircle of the opening's half-width, approximated with `segments`
/// spans. Two solid legs + a lintel whose underside follows the arc.
///
/// The opening is sized in the SHAPE's units rather than as a bare radius, so
/// a tall archway reads as a doorway instead of a mouse hole: `opening.y`
/// spans jamb + arc, and an opening shorter than its own half-width degrades
/// to a plain semicircle.
///
/// Every face that meets the arc is split at the same points the arc uses
/// (including the lintel's top and the legs' sides at the springline), so the
/// mesh has no T-junctions and face dragging stretches it instead of tearing
/// it open.
pub fn arch(half: Vec3, opening: Vec2, segments: u32) -> MapMesh {
    let (hx, hy, hz) = (half.x.max(0.1), half.y.max(0.1), half.z.max(0.05));
    // Order matters: the arc is a semicircle of the opening's half-width, so a
    // WIDE, LOW arch has to give up width or the cap wouldn't fit under the
    // ceiling. Capping `w` first also keeps the `h` clamp's bounds ordered —
    // `f32::clamp` panics outright when min > max, which is how a broad, short
    // arch used to take the whole editor down with it.
    let max_h = hy * 1.98;
    let w = opening.x.clamp(0.02, (hx * 0.98).min(max_h));
    let h = opening.y.clamp(w, max_h);
    let n = segments.clamp(2, 64) as usize;
    // Springline: where the jambs stop and the arc starts.
    let spring = -hy + (h - w);
    let mut b = Builder::new();
    // Legs, with every vertical face split at the springline so the arc's
    // endpoints are corners of them rather than points on their edges.
    let mut leg = |x0: f32, x1: f32| {
        let p = |x: f32, y: f32, z: f32| Vec3::new(x, y, z);
        for (lo, hi) in [(-hy, spring), (spring, hy)] {
            b.face(&[p(x0, lo, hz), p(x1, lo, hz), p(x1, hi, hz), p(x0, hi, hz)]);
            b.face(&[p(x1, lo, -hz), p(x0, lo, -hz), p(x0, hi, -hz), p(x1, hi, -hz)]);
            b.face(&[p(x1, lo, hz), p(x1, lo, -hz), p(x1, hi, -hz), p(x1, hi, hz)]);
            b.face(&[p(x0, lo, -hz), p(x0, lo, hz), p(x0, hi, hz), p(x0, hi, -hz)]);
        }
        b.face(&[p(x0, hy, -hz), p(x0, hy, hz), p(x1, hy, hz), p(x1, hy, -hz)]);
        b.face(&[p(x0, -hy, -hz), p(x1, -hy, -hz), p(x1, -hy, hz), p(x0, -hy, hz)]);
    };
    leg(-hx, -w);
    leg(w, hx);
    // Arc from the left springing over to the right one (ends placed exactly).
    let arc: Vec<Vec2> = (0..=n)
        .map(|i| {
            if i == 0 {
                return Vec2::new(-w, spring);
            }
            if i == n {
                return Vec2::new(w, spring);
            }
            let a = std::f32::consts::PI - i as f32 / n as f32 * std::f32::consts::PI;
            Vec2::new(w * a.cos(), spring + w * a.sin())
        })
        .collect();
    // Lintel: per-segment front/back spandrel + arc soffit + TOP (per segment
    // too — one big top quad would leave every arc vertex stranded on its
    // edge, which is what used to tear the mesh apart on a face drag).
    // (the arc runs LEFT to RIGHT, so a1 is the +X end of each span)
    for pair in arc.windows(2) {
        let (a0, a1) = (pair[0], pair[1]);
        // Front spandrel (+Z) and back spandrel (-Z).
        b.face(&[
            Vec3::new(a1.x, a1.y, hz),
            Vec3::new(a1.x, hy, hz),
            Vec3::new(a0.x, hy, hz),
            Vec3::new(a0.x, a0.y, hz),
        ]);
        b.face(&[
            Vec3::new(a0.x, a0.y, -hz),
            Vec3::new(a0.x, hy, -hz),
            Vec3::new(a1.x, hy, -hz),
            Vec3::new(a1.x, a1.y, -hz),
        ]);
        // Soffit: the arc's underside, facing into the opening.
        b.face(&[
            Vec3::new(a1.x, a1.y, hz),
            Vec3::new(a0.x, a0.y, hz),
            Vec3::new(a0.x, a0.y, -hz),
            Vec3::new(a1.x, a1.y, -hz),
        ]);
        // Top (+Y), split per segment for the same reason the spandrels are.
        b.face(&[
            Vec3::new(a0.x, hy, -hz),
            Vec3::new(a0.x, hy, hz),
            Vec3::new(a1.x, hy, hz),
            Vec3::new(a1.x, hy, -hz),
        ]);
    }
    b.mesh
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::face_normal;
    use std::collections::HashMap;

    /// Every directed edge appears exactly once (implies each undirected edge
    /// is shared by exactly 2 faces with consistent orientation).
    fn assert_watertight(m: &MapMesh) {
        let mut directed: HashMap<(u32, u32), u32> = HashMap::new();
        for f in &m.faces {
            let k = f.verts.len();
            for i in 0..k {
                *directed.entry((f.verts[i], f.verts[(i + 1) % k])).or_default() += 1;
            }
        }
        for (&(a, bv), &count) in &directed {
            assert_eq!(count, 1, "directed edge {a}->{bv} appears {count} times");
            assert_eq!(
                directed.get(&(bv, a)).copied().unwrap_or(0),
                1,
                "edge {a}<->{bv} not shared by an opposite-wound face"
            );
        }
    }

    /// No vertex may sit in the MIDDLE of another face's edge.
    ///
    /// A T-junction looks harmless in a render (the seam is watertight enough)
    /// and is anything but in an editor: the two faces along that seam share
    /// only the end vertices, so dragging one leaves the other behind and the
    /// shape tears open. Ty hit exactly this on the arch's lintel, whose top
    /// used to be a single quad against a segmented row of spandrels.
    fn assert_no_t_junctions(name: &str, m: &MapMesh) {
        for (fi, f) in m.faces.iter().enumerate() {
            let k = f.verts.len();
            for i in 0..k {
                let (ia, ib) = (f.verts[i], f.verts[(i + 1) % k]);
                let (a, bv) = (m.verts[ia as usize], m.verts[ib as usize]);
                let ab = bv - a;
                let len2 = ab.length_squared();
                if len2 < 1e-12 {
                    continue;
                }
                for (vi, &p) in m.verts.iter().enumerate() {
                    if vi as u32 == ia || vi as u32 == ib {
                        continue;
                    }
                    let t = (p - a).dot(ab) / len2;
                    assert!(
                        !(1e-4..=1.0 - 1e-4).contains(&t) || (a + ab * t).distance(p) > 1e-4,
                        "{name}: face {fi}'s edge {ia}->{ib} runs through vertex {vi} \
                         (T-junction: dragging that face would tear the mesh)"
                    );
                }
            }
        }
    }

    /// Convex solids: every face normal points away from the solid's vertex
    /// centroid through the face centroid (a face plane can pass through the
    /// origin — the wedge slope — so measure from the interior point).
    fn assert_outward(m: &MapMesh) {
        let inner = m.verts.iter().copied().sum::<glam::Vec3>() / m.verts.len() as f32;
        for (fi, f) in m.faces.iter().enumerate() {
            let c = f.verts.iter().map(|&v| m.verts[v as usize]).sum::<glam::Vec3>()
                / f.verts.len() as f32;
            assert!(
                face_normal(m, f).dot(c - inner) > 0.0,
                "face {fi} winds inward (normal {:?}, centroid {c:?})",
                face_normal(m, f)
            );
        }
    }

    /// Absurd proportions must not panic, produce NaNs, or come out invalid.
    ///
    /// A generator runs on whatever rectangle the user dragged, and a drag is
    /// frequently long and flat, or tall and thin, or almost nothing at all.
    /// (A wide, low arch used to panic inside `f32::clamp` and take the editor
    /// down with it — hence the sweep rather than one nice case.)
    #[test]
    fn every_shape_survives_hostile_proportions() {
        use crate::{ShapeKind, ShapeSpec};
        let extents = [0.0, 0.001, 0.05, 1.0, 40.0, 5000.0];
        let kinds = [
            ShapeKind::Box,
            ShapeKind::Plane,
            ShapeKind::Wedge,
            ShapeKind::Cylinder,
            ShapeKind::Sphere,
            ShapeKind::Stairs,
            ShapeKind::Arch,
        ];
        for kind in kinds {
            for &x in &extents {
                for &y in &extents {
                    for &z in &extents {
                        for (sides, steps, segs, aw, ah) in
                            [(3, 1, 2, 0.05, 0.05), (16, 8, 8, 0.6, 0.75), (128, 64, 32, 0.98, 0.98)]
                        {
                            let spec = ShapeSpec {
                                kind,
                                half: Vec3::new(x, y, z),
                                sides,
                                rings: sides.min(64),
                                steps,
                                arch_segments: segs,
                                arch_width: aw,
                                arch_height: ah,
                            };
                            let m = spec.build();
                            m.validate().unwrap_or_else(|e| {
                                panic!("{kind:?} at {x}x{y}x{z}: {e}")
                            });
                            assert!(
                                m.verts.iter().all(|v| v.is_finite()),
                                "{kind:?} at {x}x{y}x{z} produced a non-finite vertex"
                            );
                            assert!(!m.faces.is_empty(), "{kind:?} at {x}x{y}x{z} came out empty");
                        }
                    }
                }
            }
        }
    }

    /// Every primitive, over a spread of parameters, must be free of them.
    #[test]
    fn no_primitive_has_t_junctions() {
        use glam::Vec2;
        let cases: Vec<(&str, MapMesh)> = vec![
            ("box", box_mesh(Vec3::new(1.0, 2.0, 3.0))),
            ("plane", plane(Vec2::new(2.0, 3.0))),
            ("wedge", wedge(Vec3::new(1.0, 3.0, 2.0))),
            ("cylinder", cylinder(1.0, 2.0, 7)),
            ("sphere", sphere(1.5, 9, 5)),
            ("stairs-1", stairs(Vec3::new(2.0, 2.0, 3.0), 1)),
            ("stairs-8", stairs(Vec3::new(2.0, 4.0, 6.0), 8)),
            ("stairs-tall", stairs(Vec3::new(1.0, 12.0, 3.0), 17)),
            ("arch", arch(Vec3::new(2.0, 2.0, 0.5), Vec2::new(1.2, 3.0), 8)),
            ("arch-tall", arch(Vec3::new(2.0, 8.0, 1.0), Vec2::new(1.5, 12.0), 26)),
            ("arch-semicircle", arch(Vec3::new(2.0, 2.0, 0.5), Vec2::new(1.2, 0.1), 5)),
        ];
        for (name, m) in cases {
            m.validate().unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_no_t_junctions(name, &m);
        }
    }

    #[test]
    fn box_is_a_valid_watertight_outward_solid() {
        let m = box_mesh(glam::Vec3::new(1.0, 2.0, 3.0));
        m.validate().unwrap();
        assert_eq!(m.verts.len(), 8);
        assert_eq!(m.faces.len(), 6);
        assert_watertight(&m);
        assert_outward(&m);
    }

    #[test]
    fn plane_is_one_upward_quad() {
        let m = plane(glam::Vec2::new(2.0, 3.0));
        m.validate().unwrap();
        assert_eq!(m.faces.len(), 1);
        assert!(face_normal(&m, &m.faces[0]).y > 0.99);
    }

    #[test]
    fn wedge_is_a_valid_watertight_outward_solid() {
        let m = wedge(glam::Vec3::ONE);
        m.validate().unwrap();
        assert_eq!(m.verts.len(), 6);
        assert_eq!(m.faces.len(), 5);
        assert_watertight(&m);
        assert_outward(&m);
    }

    #[test]
    fn cylinder_is_a_valid_watertight_outward_solid() {
        let m = cylinder(1.0, 2.0, 12);
        m.validate().unwrap();
        assert_eq!(m.verts.len(), 24);
        assert_eq!(m.faces.len(), 14);
        assert_watertight(&m);
        assert_outward(&m);
    }

    #[test]
    fn sphere_is_a_valid_watertight_outward_solid() {
        let m = sphere(1.0, 8, 4);
        m.validate().unwrap();
        assert_watertight(&m);
        assert_outward(&m);
        // Poles are single shared verts: 8*(4-1) ring verts + 2 poles.
        assert_eq!(m.verts.len(), 8 * 3 + 2);
        // Two pole bands of triangles + two quad bands.
        assert_eq!(m.faces.len(), 8 * 4);
        let (lo, hi) = m.bounds().unwrap();
        assert!((hi.y - 1.0).abs() < 1e-5 && (lo.y + 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_generated_shape_stays_parametric_until_it_is_edited() {
        use crate::{resize, set_face_slot, translate_verts, ShapeKind, ShapeSpec};
        let mut spec = ShapeSpec::new(ShapeKind::Stairs, glam::Vec3::new(1.0, 1.0, 1.5));
        spec.steps = 4;
        let mut m = spec.build();
        m.validate().unwrap();
        assert_eq!(m.spec, Some(spec));
        assert_eq!(m.faces.len(), 2 + 4 * 4);

        // Re-generating with more steps is the same shape, more faces.
        let mut more = spec;
        more.steps = 9;
        let m2 = more.build();
        assert_eq!(m2.faces.len(), 2 + 9 * 4);
        assert_eq!(m2.bounds(), m.bounds(), "step count must not change the footprint");

        // Materials don't retire the parameters; a resize retunes them.
        set_face_slot(&mut m, &[0], 0);
        assert!(m.spec.is_some());
        resize(&mut m, glam::Vec3::new(4.0, 2.0, 6.0));
        assert_eq!(m.spec.unwrap().half, glam::Vec3::new(2.0, 1.0, 3.0));
        assert_eq!(m.spec.unwrap().steps, 4);

        // Moving a vertex does: the mesh is no longer that primitive.
        translate_verts(&mut m, &[0], glam::Vec3::X);
        assert_eq!(m.spec, None);
    }

    #[test]
    fn sphere_clamps_its_resolution() {
        let m = sphere(2.0, 1, 0);
        m.validate().unwrap();
        assert_watertight(&m);
        assert_eq!(m.faces.len(), 3 * 2); // 3 segments x 2 pole bands
    }

    #[test]
    fn cylinder_clamps_sides() {
        let m = cylinder(1.0, 1.0, 0);
        m.validate().unwrap();
        assert_eq!(m.faces.len(), 5); // 3 walls + 2 caps
    }

    #[test]
    fn stairs_spot_checks() {
        let m = stairs(glam::Vec3::new(2.0, 2.0, 3.0), 4);
        m.validate().unwrap();
        // bottom + back + 4 * (riser + tread + 2 sides)
        assert_eq!(m.faces.len(), 2 + 4 * 4);
        // Treads face +Y, risers face +Z; the topmost tread sits at y = +1.
        let up: Vec<_> =
            m.faces.iter().filter(|f| face_normal(&m, f).y > 0.99).collect();
        assert_eq!(up.len(), 4); // the treads are the only up-facing quads
        let top_y = m.verts.iter().map(|v| v.y).fold(f32::MIN, f32::max);
        assert!((top_y - 1.0).abs() < 1e-5);
        assert_eq!(
            m.faces.iter().filter(|f| face_normal(&m, f).z > 0.99).count(),
            4 // risers
        );
    }

    #[test]
    fn arch_is_valid_with_expected_extents() {
        use glam::Vec2;
        let m = arch(glam::Vec3::new(2.0, 2.0, 0.5), Vec2::new(1.2, 3.0), 8);
        m.validate().unwrap();
        let (lo, hi) = m.bounds().unwrap();
        assert!((lo - glam::Vec3::new(-2.0, -2.0, -0.5)).length() < 1e-4);
        assert!((hi - glam::Vec3::new(2.0, 2.0, 0.5)).length() < 1e-4);
        // The soffit exists: some face points essentially downward from the
        // opening (arc underside near the top of the arc).
        assert!(m.faces.iter().any(|f| face_normal(&m, f).y < -0.9
            && f.verts.iter().all(|&v| m.verts[v as usize].y > -2.0 + 1.0)));
    }

    /// The opening is sized in the SHAPE's units: a doorway 3 units tall in a
    /// 4-unit-tall arch really is 3 units tall, jamb plus arc — the old
    /// radius-only arch put a mouse hole at the foot of anything big.
    #[test]
    fn the_arch_opening_is_the_size_you_asked_for() {
        use glam::Vec2;
        let (hy, w, h) = (4.0f32, 1.5f32, 6.0f32);
        let m = arch(glam::Vec3::new(3.0, hy, 0.5), Vec2::new(w, h), 12);
        m.validate().unwrap();
        // The apex sits `h` above the floor, and the jambs are `w` wide.
        let apex = m
            .verts
            .iter()
            .filter(|v| v.x.abs() < 1e-3 && v.z > 0.0 && v.y < hy - 1e-4)
            .map(|v| v.y)
            .fold(f32::MIN, f32::max);
        assert!((apex - (-hy + h)).abs() < 1e-3, "apex {apex}");
        let jamb = m.verts.iter().filter(|v| (v.y + hy).abs() < 1e-4).map(|v| v.x.abs());
        assert!(jamb.clone().any(|x| (x - w).abs() < 1e-4), "the opening's foot is 2w wide");
        // An opening shorter than its own half-width degrades to a plain
        // semicircle instead of turning inside out.
        let squat = arch(glam::Vec3::new(3.0, 4.0, 0.5), Vec2::new(1.5, 0.2), 6);
        squat.validate().unwrap();
        assert_no_t_junctions("squat arch", &squat);
    }
}
