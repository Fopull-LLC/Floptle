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
    /// Per-vertex paint color (RGBA8), parallel to `vertices` — `None` for unpainted
    /// geometry. It is a SEPARATE stream (like `SkinStream`'s joints/weights) rather
    /// than a `Vertex` field for one hard reason: the raster vertex-attribute budget is
    /// FULL at 16/16 (`Vertex::ATTRS` 0..2 + `INSTANCE_ATTRS` 3..15, against
    /// `Limits::default()`'s 16), so a color attribute cannot exist. Colors reach the
    /// GPU through the `vpaint` storage buffer instead, indexed by `vertex_index`.
    /// Must be empty or exactly `vertices.len()` long.
    pub colors: Option<Vec<[u8; 4]>>,
}

/// Renderable geometry from an extracted terrain chunk ([`floptle_field::mesh_chunk`]).
///
/// Positions come out FIELD-space (`origin + chunk-local`), not chunk-local, so that
/// every chunk of one terrain can share a single instance matrix. That sharing is what
/// makes the triplanar material continuous: triplanar projects along `lpos`, the
/// OBJECT-space position, so per-chunk local coordinates would restart the texture at
/// every chunk boundary — a grid of seams every 48 units. Field-space coordinates cost
/// nothing in precision (a 4 km map is ±2000, ~1e-4 resolution in f32, against 1.5-unit
/// voxels); the floating origin is handled where it always is, by the model matrix being
/// camera-relative (ADR-0015).
///
/// UVs are zero: terrain has no meaningful unwrap, and its material is triplanar.
///
/// The colour's ALPHA byte carries the painted TEXTURE-SLOT INDEX (`Terrain::flat`: "0 =
/// untextured", 1 = palette layer 0, …), NOT opacity — the terrain splat shader reads it as
/// a slot and triplanar-samples the palette. The instance's `terrain_splat` flag tells the
/// fragment shader to interpret alpha this way and force the surface opaque; without the
/// flag a slot index would read as a near-zero alpha and the chunk would be discarded. The
/// rasterizer interpolates alpha across the triangle, so a boundary between two slots reads
/// a fractional value → a smooth crossfade between the two textures (matching the raymarch).
pub fn chunk_mesh_data(m: &floptle_field::ChunkMesh) -> MeshData {
    let o = m.origin;
    MeshData {
        vertices: m
            .positions
            .iter()
            .zip(&m.normals)
            .map(|(p, n)| Vertex {
                pos: [p[0] + o[0], p[1] + o[1], p[2] + o[2]],
                normal: *n,
                uv: [0.0, 0.0],
            })
            .collect(),
        indices: m.indices.clone(),
        colors: Some(m.colors.clone()),
    }
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

    /// Empty buffers sized for `cap_verts`/`cap_indices` — a mesh whose geometry is
    /// rewritten repeatedly ([`write`](Self::write)) rather than uploaded once. The
    /// terrain chunk mesher is the first citizen: a sculpt dab changes a chunk's
    /// triangle count every stroke, and re-creating buffers per dab would churn
    /// allocations in the interaction loop. Draws nothing until written.
    pub fn with_capacity(gpu: &Gpu, cap_verts: u32, cap_indices: u32) -> Self {
        let vbuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh-verts-dyn"),
            size: (cap_verts.max(1) as u64) * std::mem::size_of::<Vertex>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let ibuf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("mesh-indices-dyn"),
            size: (cap_indices.max(1) as u64) * 4,
            usage: wgpu::BufferUsages::INDEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self { vbuf, ibuf, index_count: 0 }
    }

    /// Overwrite the geometry of a [`with_capacity`](Self::with_capacity) mesh.
    /// Returns `false` (writing nothing) when `data` exceeds the buffers — the caller
    /// re-creates the slot at a larger capacity. Stale bytes past `index_count` are
    /// left as they are: the draw range is `index_count`, so they are unreachable.
    pub fn write(&mut self, gpu: &Gpu, data: &MeshData) -> bool {
        let vbytes = std::mem::size_of_val(data.vertices.as_slice()) as u64;
        let ibytes = std::mem::size_of_val(data.indices.as_slice()) as u64;
        if vbytes > self.vbuf.size() || ibytes > self.ibuf.size() {
            return false;
        }
        if vbytes > 0 {
            gpu.queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&data.vertices));
        }
        if ibytes > 0 {
            gpu.queue.write_buffer(&self.ibuf, 0, bytemuck::cast_slice(&data.indices));
        }
        self.index_count = data.indices.len() as u32;
        true
    }
}

/// Handle to a mesh registered with the render pass (index into its registry).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MeshId(pub u32);

/// Wind every triangle so that `(v1 - v0) × (v2 - v0)` points the same way as its
/// own vertex normals, flipping the two that disagree.
///
/// ## Why every builder ends with this
///
/// Nothing in this renderer culls back faces — single-sided geometry has to
/// rasterize from both sides — so a triangle's winding looks like it does not
/// matter. It does, in exactly one place: `facing_normal` in raster.wgsl decides
/// whether a fragment is being seen from behind by asking the hardware for
/// `@builtin(front_facing)`, and flips the shading normal when it is. That test
/// is by WINDING, deliberately: it is exact, where testing the interpolated
/// normal's own sign puts a black rim around every smooth silhouette.
///
/// So a mesh wound against its normals is lit **inside out**. Its visible side
/// takes the inward normal, the key light lands on the face pointing away, and
/// what you see is a dark surface with a bright rim — which reads as a strange
/// material rather than as a bug, and is why this survived so long.
///
/// A scan of the built-in shapes found `cube` correct and **every other one
/// wrong**: `uv_sphere` and `capsule` entirely inverted, `pyramid`, `cone` and
/// `cylinder` inverted in part, so one shape lit from both sides at once.
///
/// Doing it here, from the data, rather than by hand-correcting six index
/// loops: the loops are readable as written, the rule is one sentence, and a
/// seventh shape gets it right for free.
///
/// Degenerate triangles (a UV sphere's pole rows) have no winding to correct and
/// are left exactly as they are.
fn oriented(vertices: Vec<Vertex>, indices: Vec<u32>) -> MeshData {
    let mut m = MeshData { vertices, indices, colors: None };
    orient_faces(&mut m);
    m
}

fn orient_faces(m: &mut MeshData) {
    for t in m.indices.as_chunks_mut::<3>().0 {
        let p = |i: u32| glam::Vec3::from(m.vertices[i as usize].pos);
        let n: glam::Vec3 =
            t.iter().map(|&i| glam::Vec3::from(m.vertices[i as usize].normal)).sum();
        let cross = (p(t[1]) - p(t[0])).cross(p(t[2]) - p(t[0]));
        if cross.length_squared() > 1e-12 && cross.dot(n) < 0.0 {
            t.swap(1, 2);
        }
    }
}

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
    oriented(vertices, indices)
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
    oriented(vertices, indices)
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
    oriented(vertices, indices)
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
    oriented(vertices, vec![0, 1, 2, 0, 2, 3])
}

/// A grid of spritesheet cells as ONE mesh, centred on the origin in the XY
/// plane, facing +Z (`floptle/0058`).
///
/// `data` is row-major from the TOP-LEFT, `cols * rows` long; a cell of
/// [`floptle_core::EMPTY_TILE`] (or any index past the end of the sheet's
/// `cols * rows`) emits no geometry, so a map can have holes.
///
/// ## Why this is one mesh and not one quad per tile
///
/// The seam this fixes is not a texture-bleed problem, it is a *geometry*
/// problem. Give every tile its own transform and tile `i`'s right edge is
/// computed as `origin + (i + 0.5) * tile + half`, while tile `i + 1`'s left
/// edge is `origin + (i + 1.5) * tile - half`. Those are different float
/// expressions for the same number, they disagree in the last bit, and as the
/// camera moves the two edges land either side of a pixel boundary
/// independently — a hairline of background that flickers in and out.
///
/// Here both edges are the single value `(i + 1) * tile - w`, written once into
/// one vertex buffer. Two triangles that share an edge coordinate exactly are
/// watertight under the rasterizer's fill rule: there is no gap to show
/// through, at any zoom, from any camera position. Tiles still get their own
/// four vertices — they must, because they have different UVs — but the
/// coordinates along a shared edge are bit-identical, which is the part that
/// matters.
///
/// UVs come from the sheet grid with a half-texel inset (see
/// [`floptle_core::Material::cell_uv_inset`]), so a cell can never sample its
/// neighbour under linear filtering.
pub fn tilemap(
    cols: u32,
    rows: u32,
    tile: f32,
    sheet_cols: u32,
    sheet_rows: u32,
    texel: [f32; 2],
    data: &[u32],
) -> MeshData {
    let (sc, sr) = (sheet_cols.max(1), sheet_rows.max(1));
    let cells = sc * sr;
    // Centre the grid on the node's origin, so the transform places its middle.
    let (w, h) = (cols as f32 * tile * 0.5, rows as f32 * tile * 0.5);
    let (du, dv) = (1.0 / sc as f32, 1.0 / sr as f32);
    // Half a texel, in the sheet's UV space. Zero when the caller doesn't know
    // the texture size — an inset guessed from nothing would shrink the art.
    let (iu, iv) = (texel[0] * 0.5, texel[1] * 0.5);

    let mut vertices = Vec::with_capacity(data.len() * 4);
    let mut indices = Vec::with_capacity(data.len() * 6);
    for row in 0..rows {
        for col in 0..cols {
            let Some(&packed) = data.get((row * cols + col) as usize) else { continue };
            if floptle_core::tile_is_empty(packed, cells) {
                continue; // EMPTY_TILE, or past the end of the sheet
            }
            let cell = floptle_core::tile_index(packed);
            let xf = floptle_core::tile_xform(packed);
            // The two expressions below are the ONLY place a tile edge is
            // computed, which is what makes neighbouring edges identical.
            let (x0, x1) = (col as f32 * tile - w, (col + 1) as f32 * tile - w);
            // Row 0 is the TOP of the map, so y descends as row grows.
            let (y1, y0) = (h - row as f32 * tile, h - (row + 1) as f32 * tile);

            let (cx, cy) = (cell % sc, cell / sc);
            let (u0, u1) = (cx as f32 * du + iu, (cx + 1) as f32 * du - iu);
            let (v0, v1) = (cy as f32 * dv + iv, (cy + 1) as f32 * dv - iv);

            let base = vertices.len() as u32;
            // The quad's four corners in (s, t) — s left→right, t bottom→top —
            // paired with the position they sit at. The UV comes from asking the
            // orientation which corner of the ART lands here, so a rotated tile
            // is the SAME geometry with permuted UVs: shared edges stay
            // bit-identical and the seam fix survives rotation.
            for (s, t, px, py) in [(0u8, 0u8, x0, y0), (1, 0, x1, y0), (1, 1, x1, y1), (0, 1, x0, y1)]
            {
                let (a, b) = floptle_core::tile_corner(s, t, xf);
                let u = if a == 0 { u0 } else { u1 };
                // Texture v runs DOWN, so the art's top (b = 1) is the smaller v.
                let v = if b == 0 { v1 } else { v0 };
                vertices.push(Vertex { pos: [px, py, 0.0], normal: [0.0, 0.0, 1.0], uv: [u, v] });
            }
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
    }
    oriented(vertices, indices)
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
    oriented(vertices, indices)
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
    oriented(vertices, indices)
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
    oriented(vertices, indices)
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

#[cfg(test)]
mod tilemap_tests {
    use super::*;

    /// **The whole point of the primitive.** Two neighbouring tiles must give
    /// the same coordinate for the edge they share — not "close", the same
    /// bits. A difference in the last bit is exactly what opens the hairline
    /// this replaces.
    #[test]
    fn neighbouring_tiles_share_an_exact_edge() {
        // A tile size that is NOT a round binary number, which is the case the
        // real project hit (32 px at 240p works out to 1.4364 world units).
        let tile = 1.436_4_f32;
        let m = tilemap(4, 3, tile, 2, 2, [0.0, 0.0], &[0; 12]);
        assert_eq!(m.indices.len(), 12 * 6);

        // Tile (col, row) occupies vertices [i*4, i*4+4): bottom-left,
        // bottom-right, top-right, top-left.
        let xs = |col: u32, row: u32| {
            let base = ((row * 4 + col) * 4) as usize;
            (m.vertices[base].pos[0], m.vertices[base + 1].pos[0])
        };
        for row in 0..3 {
            for col in 0..3 {
                let (_, right) = xs(col, row);
                let (left, _) = xs(col + 1, row);
                assert_eq!(
                    right.to_bits(),
                    left.to_bits(),
                    "tile ({col},{row})'s right edge and ({},{row})'s left edge differ",
                    col + 1
                );
            }
        }

        // …and the same vertically, where the rows meet.
        let ys = |col: u32, row: u32| {
            let base = ((row * 4 + col) * 4) as usize;
            (m.vertices[base].pos[1], m.vertices[base + 3].pos[1])
        };
        for row in 0..2 {
            for col in 0..4 {
                let (bottom, _) = ys(col, row);
                let (_, top) = ys(col, row + 1);
                assert_eq!(bottom.to_bits(), top.to_bits(), "rows {row}/{} disagree", row + 1);
            }
        }
    }

    /// An empty square emits nothing, so a map can have holes without giving up
    /// cell 0 of its sheet.
    #[test]
    fn an_empty_cell_draws_no_triangles() {
        let data = [0, floptle_core::EMPTY_TILE, 3, 0];
        let m = tilemap(2, 2, 1.0, 2, 2, [0.0, 0.0], &data);
        assert_eq!(m.vertices.len(), 3 * 4, "one square is a hole");
        assert_eq!(m.indices.len(), 3 * 6);

        // A cell index past the end of the sheet is a hole too, rather than
        // wrapping round to a tile the author never chose.
        let m = tilemap(1, 1, 1.0, 2, 2, [0.0, 0.0], &[99]);
        assert!(m.vertices.is_empty());
    }

    /// The grid is centred on the node's origin, and row 0 is the TOP.
    #[test]
    fn the_grid_is_centred_and_row_zero_is_the_top() {
        let m = tilemap(2, 2, 2.0, 1, 1, [0.0, 0.0], &[0; 4]);
        let xs: Vec<f32> = m.vertices.iter().map(|v| v.pos[0]).collect();
        let ys: Vec<f32> = m.vertices.iter().map(|v| v.pos[1]).collect();
        assert_eq!(xs.iter().cloned().fold(f32::MAX, f32::min), -2.0);
        assert_eq!(xs.iter().cloned().fold(f32::MIN, f32::max), 2.0);
        assert_eq!(ys.iter().cloned().fold(f32::MAX, f32::min), -2.0);
        assert_eq!(ys.iter().cloned().fold(f32::MIN, f32::max), 2.0);
        // The first tile written is row 0, and it sits in the upper half.
        assert!(m.vertices[0].pos[1] >= 0.0, "row 0 must be the top of the map");
    }

    /// A rotated or mirrored tile is the SAME geometry with permuted UVs.
    ///
    /// That is the whole reason the orientation rides in the cell value rather
    /// than being a per-tile transform: if a turned tile moved its own corners,
    /// its edges would stop being bit-identical to its neighbours' and the
    /// hairline seam this mesh exists to prevent would come back for exactly the
    /// tiles somebody rotated.
    #[test]
    fn an_orientation_permutes_uvs_and_never_moves_a_vertex() {
        use floptle_core::{tile_pack, TileXform};
        let plain = tilemap(3, 3, 1.0, 4, 4, [0.0, 0.0], &[5; 9]);
        for xf in TileXform::ALL {
            let data: Vec<u32> = (0..9).map(|_| tile_pack(5, xf)).collect();
            let turned = tilemap(3, 3, 1.0, 4, 4, [0.0, 0.0], &data);
            assert_eq!(turned.indices, plain.indices, "{xf:?} changed the topology");
            for (i, (a, b)) in plain.vertices.iter().zip(&turned.vertices).enumerate() {
                assert_eq!(a.pos.map(f32::to_bits), b.pos.map(f32::to_bits), "{xf:?} moved vertex {i}");
            }
            // The UVs of one tile are the same FOUR corners, reordered — never a
            // different window, and never fewer than four distinct corners.
            let uvs = |m: &MeshData| {
                let mut v: Vec<[u32; 2]> =
                    m.vertices[..4].iter().map(|x| [x.uv[0].to_bits(), x.uv[1].to_bits()]).collect();
                v.sort_unstable();
                v
            };
            assert_eq!(uvs(&turned), uvs(&plain), "{xf:?} sampled outside its own cell");
        }
    }

    /// The one orientation whose effect is easy to state: a half-turn puts the
    /// art's bottom-left corner at the quad's top-right.
    #[test]
    fn a_half_turn_swaps_opposite_corners() {
        use floptle_core::{tile_pack, TileXform};
        let m = tilemap(1, 1, 1.0, 2, 1, [0.0, 0.0], &[tile_pack(0, TileXform::new(2, false))]);
        // Cell 0 of a 2x1 sheet is u in [0, 0.5], v in [0, 1].
        // Vertex 0 is the quad's bottom-left; under a half-turn it samples the
        // art's top-right, i.e. u = 0.5 and (v runs down) v = 0.
        assert_eq!(m.vertices[0].uv, [0.5, 0.0], "bottom-left must sample the art's top-right");
        assert_eq!(m.vertices[2].uv, [0.0, 1.0], "…and top-right the art's bottom-left");
    }

    /// The UV window comes from the sheet, and the inset pulls it in.
    #[test]
    fn cells_index_the_sheet_and_the_inset_pulls_them_in() {
        // Cell 3 of a 2x2 sheet is the bottom-right quarter.
        let m = tilemap(1, 1, 1.0, 2, 2, [0.0, 0.0], &[3]);
        let us: Vec<f32> = m.vertices.iter().map(|v| v.uv[0]).collect();
        assert_eq!(us.iter().cloned().fold(f32::MAX, f32::min), 0.5);

        // With a known texel size the window starts inside the cell instead.
        let m = tilemap(1, 1, 1.0, 2, 2, [1.0 / 32.0, 1.0 / 32.0], &[3]);
        let u_min = m.vertices.iter().map(|v| v.uv[0]).fold(f32::MAX, f32::min);
        assert!(u_min > 0.5, "the inset must pull the window off the cell boundary");
        assert!(u_min < 0.52, "…but only by half a texel");
    }
}
