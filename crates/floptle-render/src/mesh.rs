//! CPU mesh geometry and its GPU residency.
//!
//! `MeshData` is pure CPU geometry — interleaved `Vertex` (position + normal + uv)
//! plus `u32` indices. It is exactly the type a future glTF/OBJ importer
//! (`floptle-assets`) will produce, so import never has to know about wgpu.
//! `GpuMesh` is the uploaded vertex/index buffer pair. Meshes are referenced by a
//! `MeshId` — an index into the render pass's registry (a deliberately minimal
//! stand-in for the asset-id / pool handle that lands with the asset database).

use crate::device::Gpu;

/// One mesh vertex: object-space position, normal, and texture coordinate.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    /// Per-vertex attributes (vertex buffer 0): pos@0, normal@1, uv@2.
    pub const ATTRS: [wgpu::VertexAttribute; 3] = [
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 12, shader_location: 1 },
        wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 24, shader_location: 2 },
    ];

    /// The vertex-buffer layout for the per-vertex stream.
    pub const LAYOUT: wgpu::VertexBufferLayout<'static> = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &Self::ATTRS,
    };
}

/// Pure CPU geometry — also the target a mesh importer fills.
#[derive(Clone, Debug, Default)]
pub struct MeshData {
    pub vertices: Vec<Vertex>,
    pub indices: Vec<u32>,
}

/// CPU image data for a material's base-color texture: tightly-packed `RGBA8`,
/// row-major, `width * height * 4` bytes. The importer decodes glTF images into
/// this; the renderer uploads it.
#[derive(Clone, Debug)]
pub struct TextureData {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// A mesh resident on the GPU: an interleaved vertex buffer + a `u32` index buffer.
pub struct GpuMesh {
    pub(crate) vbuf: wgpu::Buffer,
    pub(crate) ibuf: wgpu::Buffer,
    pub(crate) index_count: u32,
}

impl GpuMesh {
    /// Upload `data` to immutable GPU buffers (written once). Buffer sizes are
    /// floored to one element so an empty `MeshData` (e.g. a degenerate import)
    /// yields a valid, drawable-as-nothing mesh instead of a zero-size buffer (which
    /// wgpu rejects); `index_count` of 0 then draws nothing.
    pub fn upload(gpu: &Gpu, data: &MeshData) -> Self {
        let vsize = (std::mem::size_of_val(data.vertices.as_slice()) as u64)
            .max(std::mem::size_of::<Vertex>() as u64);
        let vbuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh-verts"),
            size: vsize,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&vbuf, 0, bytemuck::cast_slice(&data.vertices));

        let isize = (std::mem::size_of_val(data.indices.as_slice()) as u64).max(4);
        let ibuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh-indices"),
            size: isize,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue.write_buffer(&ibuf, 0, bytemuck::cast_slice(&data.indices));

        Self { vbuf, ibuf, index_count: data.indices.len() as u32 }
    }
}

/// Handle to a mesh registered with the render pass (index into its registry).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshId(pub u32);

/// A unit-ish cube of half-extent `half`, centered at the origin. Each of the six
/// faces has its own four vertices so normals stay flat (sharing corners would
/// average them and round the cube) and each face carries a clean 0..1 UV square.
pub fn cube(half: f32) -> MeshData {
    // (outward normal, tangent = +u axis, bitangent = +v axis) per face.
    let faces: [([f32; 3], [f32; 3], [f32; 3]); 6] = [
        ([0.0, 0.0, 1.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]),   // +Z front
        ([0.0, 0.0, -1.0], [-1.0, 0.0, 0.0], [0.0, 1.0, 0.0]), // -Z back
        ([1.0, 0.0, 0.0], [0.0, 0.0, -1.0], [0.0, 1.0, 0.0]),  // +X right
        ([-1.0, 0.0, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),  // -X left
        ([0.0, 1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, -1.0]),  // +Y top
        ([0.0, -1.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]),  // -Y bottom
    ];
    let mut vertices = Vec::with_capacity(24);
    let mut indices = Vec::with_capacity(36);
    for (normal, tan, bit) in faces {
        let base = vertices.len() as u32;
        // corners in (u, v) ∈ {0,1}², mapped to [-1,1] across the face.
        for (su, sv) in [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)] {
            let su1 = su * 2.0 - 1.0;
            let sv1 = sv * 2.0 - 1.0;
            let pos = [
                (normal[0] + tan[0] * su1 + bit[0] * sv1) * half,
                (normal[1] + tan[1] * su1 + bit[1] * sv1) * half,
                (normal[2] + tan[2] * su1 + bit[2] * sv1) * half,
            ];
            vertices.push(Vertex { pos, normal, uv: [su, sv] });
        }
        indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
    MeshData { vertices, indices }
}

/// A latitude/longitude UV-sphere of the given `radius`. Normals are smooth (the
/// normalized position), and uv wraps θ→u, φ→v. Pole rows produce zero-area
/// triangles, which rasterize harmlessly.
pub fn uv_sphere(radius: f32, rings: u32, sectors: u32) -> MeshData {
    use std::f32::consts::{PI, TAU};
    let rings = rings.max(2);
    let sectors = sectors.max(3);
    let mut vertices = Vec::with_capacity(((rings + 1) * (sectors + 1)) as usize);
    for i in 0..=rings {
        let phi = PI * i as f32 / rings as f32; // 0 at the north pole, π at the south
        let (sp, cp) = phi.sin_cos();
        for j in 0..=sectors {
            let theta = TAU * j as f32 / sectors as f32;
            let (st, ct) = theta.sin_cos();
            let n = [sp * ct, cp, sp * st];
            vertices.push(Vertex {
                pos: [n[0] * radius, n[1] * radius, n[2] * radius],
                normal: n,
                uv: [j as f32 / sectors as f32, i as f32 / rings as f32],
            });
        }
    }
    let stride = sectors + 1;
    let mut indices = Vec::with_capacity((rings * sectors * 6) as usize);
    for i in 0..rings {
        for j in 0..sectors {
            let a = i * stride + j;
            let b = a + stride;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    MeshData { vertices, indices }
}

/// A capsule (a cylinder of length `2·half_height` capped by two hemispheres of
/// `radius`) standing along Y. Built like [`uv_sphere`] but split into a top + bottom
/// hemisphere offset by `±half_height`, with the equator rings duplicated so the rows
/// between them form the cylinder wall. Smooth (position-derived) normals.
pub fn capsule(radius: f32, half_height: f32, rings: u32, sectors: u32) -> MeshData {
    use std::f32::consts::{FRAC_PI_2, TAU};
    let hr = rings.max(2); // rings per hemisphere
    let sectors = sectors.max(3);
    let half = half_height.max(0.0);
    // (phi, y-offset) per ring row: top hemisphere then bottom hemisphere; the two
    // equator rows (phi = π/2) sit at +half and −half, forming the cylinder.
    let mut rows: Vec<(f32, f32)> = Vec::with_capacity((2 * hr + 2) as usize);
    for i in 0..=hr {
        rows.push((FRAC_PI_2 * i as f32 / hr as f32, half));
    }
    for i in 0..=hr {
        rows.push((FRAC_PI_2 + FRAC_PI_2 * i as f32 / hr as f32, -half));
    }
    let nrows = rows.len() as u32;
    let mut vertices = Vec::with_capacity((nrows * (sectors + 1)) as usize);
    for (ri, &(phi, yoff)) in rows.iter().enumerate() {
        let (sp, cp) = phi.sin_cos();
        for j in 0..=sectors {
            let theta = TAU * j as f32 / sectors as f32;
            let (st, ct) = theta.sin_cos();
            let n = [sp * ct, cp, sp * st];
            vertices.push(Vertex {
                pos: [n[0] * radius, n[1] * radius + yoff, n[2] * radius],
                normal: n,
                uv: [j as f32 / sectors as f32, ri as f32 / (nrows - 1) as f32],
            });
        }
    }
    let stride = sectors + 1;
    let mut indices = Vec::with_capacity(((nrows - 1) * sectors * 6) as usize);
    for i in 0..(nrows - 1) {
        for j in 0..sectors {
            let a = i * stride + j;
            let b = a + stride;
            indices.extend_from_slice(&[a, b, a + 1, a + 1, b, b + 1]);
        }
    }
    MeshData { vertices, indices }
}

/// A flat square of half-extent `half` in the XY plane, facing +Z. ONE face:
/// no pass culls, so the same two triangles rasterize from either side, and
/// the fragment paths flip the shading normal toward the viewer
/// (`facing_normal` in raster.wgsl). A second, coplanar back face — the old
/// approach — z-fights the front one (same depth, per-pixel ULP winner):
/// with its mirrored UV and away normal, every uv-driven custom shader broke
/// into criss-crossing unlit triangle shards.
pub fn plane(half: f32) -> MeshData {
    // (u,v) corners of the square, mapped to [-1,1] in X and Y.
    let corners = [(0.0f32, 1.0f32), (1.0, 1.0), (1.0, 0.0), (0.0, 0.0)];
    let vertices = corners
        .iter()
        .map(|&(u, v)| Vertex {
            pos: [(u * 2.0 - 1.0) * half, (v * 2.0 - 1.0) * half, 0.0],
            normal: [0.0, 0.0, 1.0],
            uv: [u, 1.0 - v],
        })
        .collect();
    MeshData { vertices, indices: vec![0, 1, 2, 0, 2, 3] }
}

// Small f32 vec helpers for the flat-shaded primitives below.
fn vsub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn vcross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}
fn vnorm(v: [f32; 3]) -> [f32; 3] {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt().max(1e-6);
    [v[0] / l, v[1] / l, v[2] / l]
}

/// A square-based pyramid: base of half-extent `half` in the XZ plane, apex `height`
/// above it, centered on the origin (base at y=−height/2, apex at y=+height/2). Flat
/// per-face normals like [`cube`]. Culling is off, so face winding is cosmetic.
pub fn pyramid(half: f32, height: f32) -> MeshData {
    let hy = height * 0.5;
    let apex = [0.0f32, hy, 0.0];
    let b = [
        [-half, -hy, -half],
        [half, -hy, -half],
        [half, -hy, half],
        [-half, -hy, half],
    ];
    let mut vertices = Vec::with_capacity(16);
    let mut indices = Vec::with_capacity(18);
    // Four triangular sides, each with its own flat normal (no shared corners).
    for i in 0..4 {
        let p0 = b[i];
        let p1 = b[(i + 1) % 4];
        // Outward+up normal: cross(apex-p0, p1-p0). (The reverse order points inward/down,
        // which lit the sloped faces backwards.)
        let n = vnorm(vcross(vsub(apex, p0), vsub(p1, p0)));
        let base = vertices.len() as u32;
        vertices.push(Vertex { pos: p0, normal: n, uv: [0.0, 0.0] });
        vertices.push(Vertex { pos: p1, normal: n, uv: [1.0, 0.0] });
        vertices.push(Vertex { pos: apex, normal: n, uv: [0.5, 1.0] });
        indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    // Base quad (normal down).
    let n = [0.0, -1.0, 0.0];
    let base = vertices.len() as u32;
    for &p in &b {
        vertices.push(Vertex { pos: p, normal: n, uv: [p[0] / (2.0 * half) + 0.5, p[2] / (2.0 * half) + 0.5] });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    MeshData { vertices, indices }
}

/// A cone of base `radius` and `height` along Y, apex up, centered (base at y=−height/2,
/// apex at y=+height/2). Smooth side normals tilted up by the slant; a flat base cap.
pub fn cone(radius: f32, height: f32, sectors: u32) -> MeshData {
    use std::f32::consts::TAU;
    let sectors = sectors.max(3);
    let hy = height * 0.5;
    let apex = [0.0f32, hy, 0.0];
    let slope = radius / height.max(1e-6); // side normal tilts up by dr/dy
    let ring = |t: f32| [t.cos() * radius, -hy, t.sin() * radius];
    let sidenorm = |t: f32| vnorm([t.cos(), slope, t.sin()]);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for j in 0..sectors {
        let t0 = TAU * j as f32 / sectors as f32;
        let t1 = TAU * (j + 1) as f32 / sectors as f32;
        let tm = (t0 + t1) * 0.5;
        let base = vertices.len() as u32;
        vertices.push(Vertex { pos: ring(t0), normal: sidenorm(t0), uv: [j as f32 / sectors as f32, 0.0] });
        vertices.push(Vertex { pos: ring(t1), normal: sidenorm(t1), uv: [(j + 1) as f32 / sectors as f32, 0.0] });
        vertices.push(Vertex { pos: apex, normal: sidenorm(tm), uv: [(j as f32 + 0.5) / sectors as f32, 1.0] });
        indices.extend_from_slice(&[base, base + 1, base + 2]);
    }
    // Base cap: fan from the center (normal down).
    let n = [0.0, -1.0, 0.0];
    let center = vertices.len() as u32;
    vertices.push(Vertex { pos: [0.0, -hy, 0.0], normal: n, uv: [0.5, 0.5] });
    let rim = vertices.len() as u32;
    for j in 0..=sectors {
        let t = TAU * j as f32 / sectors as f32;
        vertices.push(Vertex { pos: ring(t), normal: n, uv: [t.cos() * 0.5 + 0.5, t.sin() * 0.5 + 0.5] });
    }
    for j in 0..sectors {
        indices.extend_from_slice(&[center, rim + j, rim + j + 1]);
    }
    MeshData { vertices, indices }
}

/// A cylinder of `radius` and half-height `half_height` along Y, centered on the origin.
/// Smooth side normals (radial); flat top and bottom caps.
pub fn cylinder(radius: f32, half_height: f32, sectors: u32) -> MeshData {
    use std::f32::consts::TAU;
    let sectors = sectors.max(3);
    let hy = half_height.max(0.0);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    // Wall: a quad strip with radial normals.
    let stride = sectors + 1;
    for j in 0..=sectors {
        let t = TAU * j as f32 / sectors as f32;
        let n = [t.cos(), 0.0, t.sin()];
        let u = j as f32 / sectors as f32;
        vertices.push(Vertex { pos: [n[0] * radius, hy, n[2] * radius], normal: n, uv: [u, 1.0] });
        vertices.push(Vertex { pos: [n[0] * radius, -hy, n[2] * radius], normal: n, uv: [u, 0.0] });
    }
    for j in 0..sectors {
        let a = j * 2;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
    }
    let _ = stride;
    // Top + bottom caps (fans).
    for (sy, ny) in [(hy, 1.0f32), (-hy, -1.0f32)] {
        let n = [0.0, ny, 0.0];
        let center = vertices.len() as u32;
        vertices.push(Vertex { pos: [0.0, sy, 0.0], normal: n, uv: [0.5, 0.5] });
        let rim = vertices.len() as u32;
        for j in 0..=sectors {
            let t = TAU * j as f32 / sectors as f32;
            vertices.push(Vertex { pos: [t.cos() * radius, sy, t.sin() * radius], normal: n, uv: [t.cos() * 0.5 + 0.5, t.sin() * 0.5 + 0.5] });
        }
        for j in 0..sectors {
            indices.extend_from_slice(&[center, rim + j, rim + j + 1]);
        }
    }
    MeshData { vertices, indices }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_is_well_formed() {
        let m = capsule(0.5, 0.6, 8, 12);
        assert!(!m.vertices.is_empty() && m.indices.len().is_multiple_of(3));
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        // total half-height along Y is radius + half_height
        let max_y = m.vertices.iter().fold(f32::MIN, |a, v| a.max(v.pos[1]));
        assert!((max_y - (0.5 + 0.6)).abs() < 1e-5, "top y {max_y}");
        // normals are unit length
        for v in &m.vertices {
            let l = (v.normal[0].powi(2) + v.normal[1].powi(2) + v.normal[2].powi(2)).sqrt();
            assert!((l - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn cube_is_well_formed() {
        let m = cube(0.5);
        assert_eq!(m.vertices.len(), 24); // 6 faces × 4 verts, flat normals
        assert_eq!(m.indices.len(), 36); // 6 faces × 2 tris × 3
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        // every vertex sits on the cube surface (max |coord| == half)
        for v in &m.vertices {
            let m = v.pos.iter().fold(0.0f32, |acc, c| acc.max(c.abs()));
            assert!((m - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn plane_is_one_face_and_flat() {
        let m = plane(0.7);
        // ONE face: a coplanar back face z-fights the front (uv shard glitch).
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(m.indices.len(), 6);
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        // Flat in Z; corners span ±half in X and Y; all normals +Z (the
        // fragment paths flip toward the viewer — `facing_normal`).
        for v in &m.vertices {
            assert_eq!(v.pos[2], 0.0);
            assert!((v.pos[0].abs() - 0.7).abs() < 1e-6 && (v.pos[1].abs() - 0.7).abs() < 1e-6);
            assert_eq!(v.normal, [0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn sphere_normals_are_unit_and_radial() {
        let m = uv_sphere(2.0, 8, 12);
        assert!(!m.indices.is_empty());
        assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()));
        for v in &m.vertices {
            let len = (v.normal[0].powi(2) + v.normal[1].powi(2) + v.normal[2].powi(2)).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "normal not unit: {len}");
            // position is the normal scaled by radius
            for k in 0..3 {
                assert!((v.pos[k] - v.normal[k] * 2.0).abs() < 1e-4);
            }
        }
    }

    /// The built-in particle primitives must be drawable: triangle-count multiple of 3,
    /// every index in range, unit normals, and centered within their nominal extent.
    #[test]
    fn extra_primitives_are_well_formed() {
        let meshes = [pyramid(0.5, 1.0), cone(0.5, 1.0, 16), cylinder(0.5, 0.5, 16)];
        for m in &meshes {
            assert!(!m.vertices.is_empty());
            assert!(m.indices.len().is_multiple_of(3), "index count not tri-aligned");
            assert!(m.indices.iter().all(|&i| (i as usize) < m.vertices.len()), "index out of range");
            for v in &m.vertices {
                let l = (v.normal[0].powi(2) + v.normal[1].powi(2) + v.normal[2].powi(2)).sqrt();
                assert!((l - 1.0).abs() < 1e-4, "normal not unit: {l}");
                // centered: |y| ≤ half-height (+ε), radial extent ≤ ~0.71 for r=0.5.
                assert!(v.pos[1].abs() <= 0.5 + 1e-4, "y out of extent: {}", v.pos[1]);
            }
        }
        // The pyramid's sloped side faces must face OUTWARD: on a side vertex the normal's
        // horizontal component points the same way as the vertex, and its Y is up.
        let py = pyramid(0.5, 1.0);
        for v in &py.vertices {
            let horiz = v.pos[0] * v.normal[0] + v.pos[2] * v.normal[2];
            let is_base_or_apex = v.normal[1] < -0.9 || (v.pos[0] == 0.0 && v.pos[2] == 0.0);
            if !is_base_or_apex {
                assert!(v.normal[1] > 0.0, "side normal points down: {:?}", v.normal);
                assert!(horiz >= -1e-4, "side normal points inward: pos {:?} n {:?}", v.pos, v.normal);
            }
        }
    }
}
