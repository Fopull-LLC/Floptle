//! Viewport overlay visualization: world→screen projection and the wireframe
//! line builders for camera frustums, collider outlines (mesh, terrain
//! iso-surface, boxes, spheres, capsules), point-light and gravity-volume
//! gizmos. Everything returns plain screen-space (or model-space) line
//! segments; main.rs paints them with egui.

use floptle_core::math::{DVec3, Mat4, Quat, Vec2, Vec3, Vec4};

/// Project an absolute world point to physical-pixel screen space (camera-relative,
/// ADR-0015). Returns `None` when the point is behind the camera.
pub(crate) fn project(world: DVec3, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Option<Vec2> {
    let rel = (world - cam_world).as_vec3();
    let clip = vp * rel.extend(1.0);
    if clip.w <= 1e-4 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    Some(Vec2::new((ndc.x * 0.5 + 0.5) * w, (1.0 - (ndc.y * 0.5 + 0.5)) * h))
}

/// The world point under `cursor` (physical px) — its ray's hit on the ground
/// plane y=0, or ~6 units ahead of the camera if the ray misses. `inv_vp` is the
/// inverse of the camera's view-projection at this `w`/`h` aspect.
pub(crate) fn cursor_ground(
    cam_world: DVec3,
    cam_rot: Quat,
    inv_vp: Mat4,
    w: f32,
    h: f32,
    cursor: Option<Vec2>,
) -> DVec3 {
    let fallback = cam_world + (cam_rot * Vec3::NEG_Z * 6.0).as_dvec3();
    let Some(cursor) = cursor else { return fallback };
    let ndc = Vec2::new(cursor.x / w * 2.0 - 1.0, 1.0 - cursor.y / h * 2.0);
    let near = inv_vp * Vec4::new(ndc.x, ndc.y, 0.0, 1.0);
    let far = inv_vp * Vec4::new(ndc.x, ndc.y, 1.0, 1.0);
    let ro = near.truncate() / near.w; // camera-relative
    let rd = (far.truncate() / far.w - ro).normalize();
    if rd.y.abs() > 1e-4 {
        let t = -(cam_world.y as f32 + ro.y) / rd.y;
        if (0.1..1000.0).contains(&t) {
            return cam_world + (ro + rd * t).as_dvec3();
        }
    }
    fallback
}

/// The on-screen brush telegraph: a projected ring at the terrain hit point + a
/// surface-normal line, so you can see exactly where (and on what facing) a stroke
/// will land. Points are full-window physical pixels (divided by ppp when drawn).
#[derive(Default)]
pub(crate) struct TerrainViz {
    pub(crate) ring: Vec<Vec2>,
    pub(crate) normal: Option<(Vec2, Vec2)>,
}

/// A camera's frustum drawn in the viewport (screen-space px line pairs) so you can
/// see + position cameras. `active` is the camera holding play-mode authority.
pub(crate) struct CameraGizmo {
    pub(crate) lines: Vec<(Vec2, Vec2)>,
    pub(crate) active: bool,
}

/// Build a camera frustum's projected screen-space line segments (apex → 4 far
/// corners + the far rectangle), or empty if it doesn't project.
#[allow(clippy::too_many_arguments)]
pub(crate) fn camera_frustum_lines(
    pos: DVec3,
    rot: Quat,
    fov_y: f32,
    aspect: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let fwd = rot * Vec3::NEG_Z;
    let up = rot * Vec3::Y;
    let right = rot * Vec3::X;
    let far = 2.2f32; // a compact visualization length, not the real far plane
    let hh = far * (fov_y * 0.5).tan();
    let hw = hh * aspect.max(0.1);
    let apex = pos;
    let center = pos + (fwd * far).as_dvec3();
    let corners = [
        center + ((right * hw + up * hh).as_dvec3()),
        center + ((-right * hw + up * hh).as_dvec3()),
        center + ((-right * hw - up * hh).as_dvec3()),
        center + ((right * hw - up * hh).as_dvec3()),
    ];
    let pa = project(apex, cam_world, vp, w, h);
    let pc: Vec<Option<Vec2>> = corners.iter().map(|&c| project(c, cam_world, vp, w, h)).collect();
    let mut lines = Vec::new();
    for i in 0..4 {
        if let (Some(a), Some(b)) = (pa, pc[i]) {
            lines.push((a, b)); // apex → corner
        }
        if let (Some(a), Some(b)) = (pc[i], pc[(i + 1) % 4]) {
            lines.push((a, b)); // far-rect edge
        }
    }
    lines
}

/// MODEL-LOCAL deduped triangle edges of an imported model — the mesh collider's
/// wireframe. Edges are deduped per part (shared triangle edges collapse) and a global
/// budget caps a dense map so the overlay stays a sane line count.
pub(crate) fn mesh_collider_wire_local(model: &floptle_assets::gltf_import::ImportedModel) -> Vec<(Vec3, Vec3)> {
    const MAX_EDGES: usize = 6000;
    let mut edges = Vec::new();
    for part in &model.parts {
        let vs = &part.mesh.vertices;
        let mut seen = std::collections::HashSet::new();
        for tri in part.mesh.indices.chunks_exact(3) {
            for (a, b) in [(tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0])] {
                if seen.insert((a.min(b), a.max(b))) {
                    edges.push((Vec3::from(vs[a as usize].pos), Vec3::from(vs[b as usize].pos)));
                    if edges.len() >= MAX_EDGES {
                        return edges;
                    }
                }
            }
        }
    }
    edges
}

/// World-space line segments tracing the terrain's collision iso-surface (where the SDF
/// crosses zero — exactly what the player collides with), via coarse Surface Nets: one
/// vertex per straddling cell (averaged edge crossings), connected to its +X/+Y/+Z
/// neighbors. `stride` sets coarseness (bigger = fewer lines). Cached by the caller and
/// projected to screen each frame.
pub(crate) fn terrain_collider_wire(b: &floptle_field::BakedSdf, stride: u32) -> Vec<(Vec3, Vec3)> {
    let [w, h, d] = b.dims;
    let s = stride.max(1);
    if w < 2 || h < 2 || d < 2 {
        return Vec::new();
    }
    let dist = |x: u32, y: u32, z: u32| -> f32 {
        b.distance[((z.min(d - 1) * h + y.min(h - 1)) * w + x.min(w - 1)) as usize]
    };
    let gpos = |x: u32, y: u32, z: u32| -> Vec3 {
        let f = |i: u32, n: u32, c: f32, hf: f32| c - hf + (i as f32 + 0.5) / n as f32 * 2.0 * hf;
        Vec3::new(
            f(x.min(w - 1), w, b.center[0], b.half_extent[0]),
            f(y.min(h - 1), h, b.center[1], b.half_extent[1]),
            f(z.min(d - 1), d, b.center[2], b.half_extent[2]),
        )
    };
    // Coarse cell grid (each cell spans `s` voxels). One optional vertex per cell.
    let (cx_n, cy_n, cz_n) = ((w - 1) / s, (h - 1) / s, (d - 1) / s);
    let ci = |cx: u32, cy: u32, cz: u32| ((cz * cy_n + cy) * cx_n + cx) as usize;
    let mut verts: Vec<Option<Vec3>> = vec![None; (cx_n * cy_n * cz_n) as usize];
    // The 12 edges of a cube (corner index pairs), corners ordered (x,y,z) bit = 1<<axis.
    const EDGES: [(usize, usize); 12] =
        [(0, 1), (0, 2), (0, 4), (1, 3), (1, 5), (2, 3), (2, 6), (4, 5), (4, 6), (3, 7), (5, 7), (6, 7)];
    for cz in 0..cz_n {
        for cy in 0..cy_n {
            for cx in 0..cx_n {
                let (x0, y0, z0) = (cx * s, cy * s, cz * s);
                let corner = |k: usize| {
                    (x0 + (k as u32 & 1) * s, y0 + ((k as u32 >> 1) & 1) * s, z0 + ((k as u32 >> 2) & 1) * s)
                };
                let ds: [f32; 8] = std::array::from_fn(|k| {
                    let (x, y, z) = corner(k);
                    dist(x, y, z)
                });
                if ds.iter().all(|&v| v > 0.0) || ds.iter().all(|&v| v <= 0.0) {
                    continue; // doesn't straddle the surface
                }
                let cp: [Vec3; 8] = std::array::from_fn(|k| {
                    let (x, y, z) = corner(k);
                    gpos(x, y, z)
                });
                let mut acc = Vec3::ZERO;
                let mut n = 0.0f32;
                for (a, c) in EDGES {
                    if (ds[a] > 0.0) != (ds[c] > 0.0) {
                        let f = (ds[a] / (ds[a] - ds[c])).clamp(0.0, 1.0);
                        acc += cp[a].lerp(cp[c], f);
                        n += 1.0;
                    }
                }
                if n > 0.0 {
                    verts[ci(cx, cy, cz)] = Some(acc / n);
                }
            }
        }
    }
    // Connect each cell's vertex to its +X/+Y/+Z neighbour (a surface-conforming net).
    let mut segs = Vec::new();
    for cz in 0..cz_n {
        for cy in 0..cy_n {
            for cx in 0..cx_n {
                let Some(v) = verts[ci(cx, cy, cz)] else { continue };
                if cx + 1 < cx_n
                    && let Some(v2) = verts[ci(cx + 1, cy, cz)] {
                        segs.push((v, v2));
                    }
                if cy + 1 < cy_n
                    && let Some(v2) = verts[ci(cx, cy + 1, cz)] {
                        segs.push((v, v2));
                    }
                if cz + 1 < cz_n
                    && let Some(v2) = verts[ci(cx, cy, cz + 1)] {
                        segs.push((v, v2));
                    }
            }
        }
    }
    segs
}

/// Build a point light's projected gizmo: a small 3-axis cross at its position plus a
/// horizontal ring at its `range` (so its reach on the ground is visible). Empty if
/// it doesn't project in front of the camera.
pub(crate) fn point_light_lines(pos: DVec3, range: f32, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    let s = 0.5; // cross half-size (world units)
    for a in [DVec3::X, DVec3::Y, DVec3::Z] {
        if let (Some(p0), Some(p1)) = (
            project(pos - a * s, cam_world, vp, w, h),
            project(pos + a * s, cam_world, vp, w, h),
        ) {
            lines.push((p0, p1));
        }
    }
    let r = range.clamp(0.2, 500.0) as f64;
    let segs = 28;
    let mut prev = project(pos + DVec3::new(r, 0.0, 0.0), cam_world, vp, w, h);
    for i in 1..=segs {
        let a = (i as f64 / segs as f64) * std::f64::consts::TAU;
        let p = project(pos + DVec3::new(a.cos() * r, 0.0, a.sin() * r), cam_world, vp, w, h);
        if let (Some(pp), Some(cp)) = (prev, p) {
            lines.push((pp, cp));
        }
        prev = p;
    }
    lines
}

/// Directional ("sun") light gizmo: a small sun disc with radiating spokes, plus a
/// bundle of parallel rays flowing along −`dir` (the way the light travels) to `anchor`,
/// each capped with an arrowhead. `dir` points TOWARD the sun (matches `Light.direction`).
/// The directional light has no world position, so callers anchor it in front of the
/// camera. All-`DVec3` so it stays precise under floating origin (ADR-0015). Empty if the
/// direction is degenerate or nothing projects in front of the camera.
pub(crate) fn light_dir_lines(
    anchor: DVec3,
    dir: Vec3,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let d = dir.normalize_or_zero().as_dvec3();
    if d.length_squared() < 1e-9 {
        return Vec::new();
    }
    let mut lines: Vec<(Vec2, Vec2)> = Vec::new();
    let push = |a: DVec3, b: DVec3, lines: &mut Vec<(Vec2, Vec2)>| {
        if let (Some(p0), Some(p1)) =
            (project(a, cam_world, vp, w, h), project(b, cam_world, vp, w, h))
        {
            lines.push((p0, p1));
        }
    };
    // Orthonormal basis around the light direction.
    let refv = if d.y.abs() > 0.9 { DVec3::X } else { DVec3::Y };
    let side = d.cross(refv).normalize();
    let up = side.cross(d);
    // Sun disc sits a few units toward the sun from the anchor.
    let sun = anchor + d * 3.0;
    let r = 0.5;
    let segs = 24;
    let disc = |a: f64| -> DVec3 { sun + side * (a.cos() * r) + up * (a.sin() * r) };
    let mut prev = disc(0.0);
    for i in 1..=segs {
        let p = disc(i as f64 / segs as f64 * std::f64::consts::TAU);
        push(prev, p, &mut lines);
        prev = p;
    }
    // Short spokes radiating from the disc (the "sun" read).
    for i in 0..8 {
        let a = i as f64 / 8.0 * std::f64::consts::TAU;
        let dirp = side * a.cos() + up * a.sin();
        push(sun + dirp * (r * 1.2), sun + dirp * (r * 1.7), &mut lines);
    }
    // Parallel rays flowing sun → anchor (along −d), each with a 4-line arrowhead.
    let flow = -d;
    let barb = 0.22;
    for (ox, oy) in [(0.0, 0.0), (0.7, 0.0), (-0.7, 0.0), (0.0, 0.7), (0.0, -0.7)] {
        let off = side * ox + up * oy;
        let base = sun + off;
        let tip = anchor + off;
        push(base, tip, &mut lines);
        for hd in [side, -side, up, -up] {
            push(tip, tip - flow * barb + hd * (barb * 0.6), &mut lines);
        }
    }
    lines
}

/// Build a gravity-volume gizmo: a radial well is a 3-ring sphere wireframe at its
/// `radius`; a Down volume is a downward arrow. Empty if it doesn't project.
pub(crate) fn gravity_volume_lines(
    pos: DVec3,
    radial: bool,
    radius: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    if radial {
        let r = radius.clamp(0.2, 500.0) as f64;
        let segs = 28;
        for plane in 0..3 {
            let ring = |a: f64| -> DVec3 {
                let (c, s) = (a.cos() * r, a.sin() * r);
                match plane {
                    0 => DVec3::new(c, 0.0, s), // XZ (ground)
                    1 => DVec3::new(c, s, 0.0), // XY
                    _ => DVec3::new(0.0, c, s), // YZ
                }
            };
            let mut prev = project(pos + ring(0.0), cam_world, vp, w, h);
            for i in 1..=segs {
                let p = project(pos + ring((i as f64 / segs as f64) * std::f64::consts::TAU), cam_world, vp, w, h);
                if let (Some(pp), Some(cp)) = (prev, p) {
                    lines.push((pp, cp));
                }
                prev = p;
            }
        }
    } else {
        let top = project(pos + DVec3::new(0.0, 1.0, 0.0), cam_world, vp, w, h);
        let bot = project(pos + DVec3::new(0.0, -1.2, 0.0), cam_world, vp, w, h);
        if let (Some(a), Some(b)) = (top, bot) {
            lines.push((a, b));
        }
        for dx in [-0.35, 0.35] {
            let head = project(pos + DVec3::new(dx, -0.55, 0.0), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (bot, head) {
                lines.push((a, b));
            }
        }
    }
    lines
}

/// Build a world-axis-aligned box wireframe (12 edges) centered at `center` with the
/// given world half-extents — the outline of a `BodyKind::Box` collider (which the
/// solver treats as axis-aligned).
pub(crate) fn box_lines(
    center: DVec3,
    half: Vec3,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let hd = DVec3::new(half.x.max(0.01) as f64, half.y.max(0.01) as f64, half.z.max(0.01) as f64);
    let signs = [
        (-1.0, -1.0, -1.0), (1.0, -1.0, -1.0), (1.0, -1.0, 1.0), (-1.0, -1.0, 1.0), // bottom
        (-1.0, 1.0, -1.0), (1.0, 1.0, -1.0), (1.0, 1.0, 1.0), (-1.0, 1.0, 1.0), // top
    ];
    let corners: Vec<Option<Vec2>> = signs
        .iter()
        .map(|&(sx, sy, sz)| {
            project(center + DVec3::new(sx * hd.x, sy * hd.y, sz * hd.z), cam_world, vp, w, h)
        })
        .collect();
    // bottom loop, top loop, and the four vertical edges connecting them.
    let edges = [
        (0, 1), (1, 2), (2, 3), (3, 0),
        (4, 5), (5, 6), (6, 7), (7, 4),
        (0, 4), (1, 5), (2, 6), (3, 7),
    ];
    let mut lines = Vec::new();
    for &(a, b) in &edges {
        if let (Some(pa), Some(pb)) = (corners[a], corners[b]) {
            lines.push((pa, pb));
        }
    }
    lines
}

/// Oriented box wireframe (12 edges): the 8 corners of a ±`half` cube transformed by
/// `m` (a node's world matrix), so the box follows the node's rotation + scale — the
/// outline of a static `Collidable` Cube's box collider.
pub(crate) fn oriented_box_lines(
    m: Mat4,
    half: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let signs: [(f32, f32, f32); 8] = [
        (-1.0, -1.0, -1.0), (1.0, -1.0, -1.0), (1.0, -1.0, 1.0), (-1.0, -1.0, 1.0),
        (-1.0, 1.0, -1.0), (1.0, 1.0, -1.0), (1.0, 1.0, 1.0), (-1.0, 1.0, 1.0),
    ];
    let corners: Vec<Option<Vec2>> = signs
        .iter()
        .map(|&(sx, sy, sz)| {
            let lp = Vec3::new(sx * half, sy * half, sz * half);
            project(m.transform_point3(lp).as_dvec3(), cam_world, vp, w, h)
        })
        .collect();
    let edges = [
        (0, 1), (1, 2), (2, 3), (3, 0),
        (4, 5), (5, 6), (6, 7), (7, 4),
        (0, 4), (1, 5), (2, 6), (3, 7),
    ];
    let mut lines = Vec::new();
    for &(a, b) in &edges {
        if let (Some(pa), Some(pb)) = (corners[a], corners[b]) {
            lines.push((pa, pb));
        }
    }
    lines
}

/// Build a rigidbody collider outline: a 3-ring wireframe sphere, or a capsule (two
/// end rings + side connectors + cap arcs). Y-up (the editor doesn't tilt the gizmo).
#[allow(clippy::too_many_arguments)]
pub(crate) fn rigidbody_lines(
    pos: DVec3,
    capsule: bool,
    radius: f32,
    height: f32,
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
) -> Vec<(Vec2, Vec2)> {
    let mut lines = Vec::new();
    let r = radius.max(0.02) as f64;
    let segs = 24;
    let ring = |center: DVec3, plane: u8, out: &mut Vec<(Vec2, Vec2)>| {
        let at = |a: f64| -> DVec3 {
            let (c, s) = (a.cos() * r, a.sin() * r);
            match plane {
                0 => DVec3::new(c, 0.0, s), // XZ
                1 => DVec3::new(c, s, 0.0), // XY
                _ => DVec3::new(0.0, c, s), // YZ
            }
        };
        let mut prev = project(center + at(0.0), cam_world, vp, w, h);
        for i in 1..=segs {
            let p = project(center + at((i as f64 / segs as f64) * std::f64::consts::TAU), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (prev, p) {
                out.push((a, b));
            }
            prev = p;
        }
    };
    if capsule {
        let half = ((height.max(2.0 * radius) as f64) * 0.5 - r).max(0.0);
        let top = pos + DVec3::new(0.0, half, 0.0);
        let bot = pos - DVec3::new(0.0, half, 0.0);
        ring(top, 0, &mut lines);
        ring(bot, 0, &mut lines);
        ring(top, 1, &mut lines);
        ring(bot, 1, &mut lines);
        for (dx, dz) in [(r, 0.0), (-r, 0.0), (0.0, r), (0.0, -r)] {
            let a = project(top + DVec3::new(dx, 0.0, dz), cam_world, vp, w, h);
            let b = project(bot + DVec3::new(dx, 0.0, dz), cam_world, vp, w, h);
            if let (Some(a), Some(b)) = (a, b) {
                lines.push((a, b));
            }
        }
    } else {
        ring(pos, 0, &mut lines);
        ring(pos, 1, &mut lines);
        ring(pos, 2, &mut lines);
    }
    lines
}

// ---- particle emitter + force gizmos ------------------------------------
// Visualize the SELECTED particle track: where particles are born (the emit shape,
// warm) and which way they head / what forces push them (arrows). Geometry mirrors
// `floptle_vfx::sim::sample_shape` exactly so the gizmo matches what emits.

/// A particle emitter's birth shape, decoupled from the scene doc types. Cone `angle`
/// is in DEGREES (half-angle of the spread cone about +Y), matching `EmitShape::Cone`.
pub(crate) enum EmitterViz {
    Point,
    Cone { angle: f32, radius: f32 },
    Sphere { radius: f32 },
    Edge { length: f32 },
    Ring { radius: f32 },
}

/// A steady force on a track's particles, for the viewport arrow(s).
pub(crate) enum ForceViz {
    Directional { dir: Vec3 },
    Point { center: Vec3, attract: bool },
    Vortex { center: Vec3, axis: Vec3 },
}

const PG_SHAPE: [f32; 3] = [0.98, 0.62, 0.25]; // emitter birth shape (warm orange)
const PG_EMIT: [f32; 3] = [0.40, 0.95, 0.75]; // emit direction (cyan-green)
const PG_FORCE: [f32; 3] = [0.95, 0.50, 0.90]; // force fields (magenta)

/// Project a node-local point through world matrix `m` (camera-relative, ADR-0015).
fn plocal(m: Mat4, p: Vec3, cam_world: DVec3, vp: Mat4, w: f32, h: f32) -> Option<Vec2> {
    project(m.transform_point3(p).as_dvec3(), cam_world, vp, w, h)
}

/// Push one local segment (a→b) if both ends project.
#[allow(clippy::too_many_arguments)]
fn seg3(
    out: &mut Vec<(Vec2, Vec2, [f32; 3])>, m: Mat4, a: Vec3, b: Vec3, col: [f32; 3],
    cam_world: DVec3, vp: Mat4, w: f32, h: f32,
) {
    if let (Some(pa), Some(pb)) = (plocal(m, a, cam_world, vp, w, h), plocal(m, b, cam_world, vp, w, h)) {
        out.push((pa, pb, col));
    }
}

/// Push a ring of `radius` in a local plane (0=XZ, 1=XY, 2=YZ) centered at local `c`.
#[allow(clippy::too_many_arguments)]
fn push_ring(
    out: &mut Vec<(Vec2, Vec2, [f32; 3])>, m: Mat4, c: Vec3, radius: f32, plane: u8, col: [f32; 3],
    cam_world: DVec3, vp: Mat4, w: f32, h: f32,
) {
    let segs = 28;
    let at = |a: f32| -> Vec3 {
        let (s, co) = (a.sin() * radius, a.cos() * radius);
        c + match plane {
            0 => Vec3::new(co, 0.0, s),
            1 => Vec3::new(co, s, 0.0),
            _ => Vec3::new(0.0, co, s),
        }
    };
    let mut prev = plocal(m, at(0.0), cam_world, vp, w, h);
    for i in 1..=segs {
        let p = plocal(m, at(i as f32 / segs as f32 * std::f32::consts::TAU), cam_world, vp, w, h);
        if let (Some(a), Some(b)) = (prev, p) {
            out.push((a, b, col));
        }
        prev = p;
    }
}

/// Push a shafted arrow from local `base` along local `dir` of `len`, with a 4-line head.
#[allow(clippy::too_many_arguments)]
fn push_arrow(
    out: &mut Vec<(Vec2, Vec2, [f32; 3])>, m: Mat4, base: Vec3, dir: Vec3, len: f32, col: [f32; 3],
    cam_world: DVec3, vp: Mat4, w: f32, h: f32,
) {
    if dir.length_squared() < 1e-10 {
        return;
    }
    let d = dir.normalize();
    let tip = base + d * len;
    let refv = if d.y.abs() > 0.9 { Vec3::X } else { Vec3::Y };
    let side = d.cross(refv).normalize();
    let up = side.cross(d);
    let barb = (len * 0.25).max(0.04);
    seg3(out, m, base, tip, col, cam_world, vp, w, h);
    for hd in [
        tip - d * barb + side * barb * 0.6,
        tip - d * barb - side * barb * 0.6,
        tip - d * barb + up * barb * 0.6,
        tip - d * barb - up * barb * 0.6,
    ] {
        seg3(out, m, tip, hd, col, cam_world, vp, w, h);
    }
}

/// Build the selected particle track's emitter-shape + emit-direction + force gizmos,
/// as colored screen-space line segments. `m_shape` is the emitter node's world matrix
/// (birth is always emitter-local); `m_force` is the frame the forces act in
/// (emitter-local for `Space::Local`, translation-only world/anchor for `Space::World`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn particle_gizmo_lines(
    shape: &EmitterViz, forces: &[ForceViz], m_shape: Mat4, m_force: Mat4,
    cam_world: DVec3, vp: Mat4, w: f32, h: f32,
) -> Vec<(Vec2, Vec2, [f32; 3])> {
    use std::f32::consts::TAU;
    let mut out = Vec::new();
    let m = m_shape;
    match *shape {
        EmitterViz::Point => {
            let s = 0.12;
            for a in [Vec3::X, Vec3::Y, Vec3::Z] {
                seg3(&mut out, m, -a * s, a * s, PG_SHAPE, cam_world, vp, w, h);
            }
            push_arrow(&mut out, m, Vec3::ZERO, Vec3::Y, 0.6, PG_EMIT, cam_world, vp, w, h);
        }
        EmitterViz::Sphere { radius } => {
            let r = radius.max(0.01);
            for plane in 0..3 {
                push_ring(&mut out, m, Vec3::ZERO, r, plane, PG_SHAPE, cam_world, vp, w, h);
            }
        }
        EmitterViz::Ring { radius } => {
            let r = radius.max(0.01);
            push_ring(&mut out, m, Vec3::ZERO, r, 0, PG_SHAPE, cam_world, vp, w, h);
            for i in 0..8 {
                let a = i as f32 / 8.0 * TAU;
                let d = Vec3::new(a.cos(), 0.0, a.sin());
                seg3(&mut out, m, d * r, d * (r + 0.2), PG_EMIT, cam_world, vp, w, h);
            }
        }
        EmitterViz::Edge { length } => {
            let hx = (length * 0.5).max(0.01);
            seg3(&mut out, m, Vec3::new(-hx, 0.0, 0.0), Vec3::new(hx, 0.0, 0.0), PG_SHAPE, cam_world, vp, w, h);
            for x in [-hx, 0.0, hx] {
                push_arrow(&mut out, m, Vec3::new(x, 0.0, 0.0), Vec3::Z, 0.35, PG_EMIT, cam_world, vp, w, h);
            }
        }
        EmitterViz::Cone { angle, radius } => {
            let r = radius.max(0.0);
            if r > 0.001 {
                push_ring(&mut out, m, Vec3::ZERO, r, 0, PG_SHAPE, cam_world, vp, w, h);
            }
            let l = (r * 2.5).max(0.7);
            let half = angle.to_radians().clamp(0.0, std::f32::consts::PI * 0.5);
            let (st, ct) = (half.sin(), half.cos());
            for i in 0..4 {
                let ph = i as f32 / 4.0 * TAU;
                let d = Vec3::new(st * ph.cos(), ct, st * ph.sin());
                seg3(&mut out, m, Vec3::ZERO, d * l, PG_EMIT, cam_world, vp, w, h);
            }
            push_ring(&mut out, m, Vec3::new(0.0, l * ct, 0.0), l * st, 0, PG_EMIT, cam_world, vp, w, h);
        }
    }
    for f in forces {
        match *f {
            ForceViz::Directional { dir } => {
                push_arrow(&mut out, m_force, Vec3::ZERO, dir, 0.9, PG_FORCE, cam_world, vp, w, h);
            }
            ForceViz::Point { center, attract } => {
                let s = 0.12;
                for a in [Vec3::X, Vec3::Y, Vec3::Z] {
                    seg3(&mut out, m_force, center - a * s, center + a * s, PG_FORCE, cam_world, vp, w, h);
                }
                for a in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z] {
                    // Attractor: arrows point IN toward the center; repeller: OUT.
                    let (base, dir) =
                        if attract { (center + a * 0.6, -a) } else { (center + a * 0.28, a) };
                    push_arrow(&mut out, m_force, base, dir, 0.32, PG_FORCE, cam_world, vp, w, h);
                }
            }
            ForceViz::Vortex { center, axis } => {
                push_arrow(&mut out, m_force, center, axis, 0.8, PG_FORCE, cam_world, vp, w, h);
                push_ring(&mut out, m_force, center, 0.4, 0, PG_FORCE, cam_world, vp, w, h);
            }
        }
    }
    out
}
