//! Headless probe for GPU skinning (`floptle/0080`).
//!
//! A skinning bug is invisible in a timing number and obvious in a picture: one
//! joint's weight landing on the wrong vertex still runs at exactly the frame
//! rate you were hoping for. So this renders the SAME posed mesh twice —
//!
//!   * deformed on the CPU — `cpu_skin_part`'s arithmetic, baked into the vertex
//!     buffer and drawn through the ordinary pipeline;
//!   * the bind pose drawn through `vs_skin` with the same bone palette.
//!
//! — and asserts they agree. That is the load-bearing check: the two paths must
//! stay one behaviour, because the CPU one is still the fallback when the
//! skinning store is full, and a game must not look different depending on which
//! it got.
//!
//! It also checks the pose actually MOVED anything: a `vs_skin` that quietly fell
//! through to the bind pose would match a CPU path that did the same, and two
//! identically-wrong renders agree perfectly.
//!
//! Run: cargo run -p floptle-render --example skin_probe -- <out.png>

use floptle_render::{
    instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshData, MeshId, Projection,
    Raster, RenderCamera, SkinDraw, TexId, Vertex,
};
use glam::{Mat3, Mat4, Quat, Vec3};

const S: u32 = 320;
/// Segments up the bar; each ring is weighted between two joints, so a bend
/// smoothly blends rather than hinging — which is what a real limb does and what
/// a per-vertex weighting mistake breaks visibly.
const RINGS: usize = 24;
const SIDES: usize = 12;
const JOINTS: usize = 4;

/// A tapered bar along +Y, ring by ring, with each ring's vertices weighted
/// between the two nearest joints. A rig in miniature.
fn bar() -> (MeshData, Vec<[u16; 4]>, Vec<[f32; 4]>) {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    let mut joints = Vec::new();
    let mut weights = Vec::new();
    for r in 0..=RINGS {
        let t = r as f32 / RINGS as f32;
        let y = t * 3.0;
        let radius = 0.35 * (1.0 - 0.5 * t);
        // Which two joints this ring sits between, and how far along.
        let jf = t * (JOINTS - 1) as f32;
        let j0 = (jf.floor() as usize).min(JOINTS - 1);
        let j1 = (j0 + 1).min(JOINTS - 1);
        let f = jf - j0 as f32;
        for s in 0..SIDES {
            let a = s as f32 / SIDES as f32 * std::f32::consts::TAU;
            let (sn, cs) = a.sin_cos();
            vertices.push(Vertex {
                pos: [cs * radius, y, sn * radius],
                normal: [cs, 0.0, sn],
                uv: [s as f32 / SIDES as f32, t],
            });
            joints.push([j0 as u16, j1 as u16, 0, 0]);
            weights.push([1.0 - f, f, 0.0, 0.0]);
        }
    }
    for r in 0..RINGS {
        for s in 0..SIDES {
            let a = (r * SIDES + s) as u32;
            let b = (r * SIDES + (s + 1) % SIDES) as u32;
            let c = a + SIDES as u32;
            let d = b + SIDES as u32;
            indices.extend_from_slice(&[a, c, b, b, c, d]);
        }
    }
    (MeshData { vertices, indices, colors: None }, joints, weights)
}

/// The pose: each joint above the first rotates a little about Z, accumulated up
/// the chain, so the bar curls. Returns `(fallback, palette)` — the palette being
/// `nodeWorld · inverseBind` per slot, which is what both paths consume.
fn pose(bend: f32) -> (Mat4, Vec<Mat4>) {
    // Bind: joints evenly spaced up the bar, no rotation.
    let bind: Vec<Mat4> = (0..JOINTS)
        .map(|j| Mat4::from_translation(Vec3::new(0.0, j as f32 * (3.0 / (JOINTS - 1) as f32), 0.0)))
        .collect();
    let mut world = Vec::with_capacity(JOINTS);
    let mut acc = Mat4::IDENTITY;
    for j in 0..JOINTS {
        if j > 0 {
            acc *= Mat4::from_translation(Vec3::new(0.0, 3.0 / (JOINTS - 1) as f32, 0.0))
                * Mat4::from_rotation_z(bend);
        }
        world.push(acc);
    }
    let palette = world.iter().zip(&bind).map(|(w, b)| *w * b.inverse()).collect();
    (Mat4::IDENTITY, palette)
}

/// `cpu_skin_part`'s arithmetic, spelled here so this probe measures the RENDER
/// paths against each other without depending on the editor crate.
fn cpu_skin(base: &[Vertex], joints: &[[u16; 4]], weights: &[[f32; 4]], fallback: Mat4, palette: &[Mat4]) -> Vec<Vertex> {
    base.iter()
        .enumerate()
        .map(|(i, v)| {
            let j = joints[i];
            let w = weights[i];
            let sum = w[0] + w[1] + w[2] + w[3];
            let m = if sum > 1e-4 {
                let mut acc = Mat4::ZERO;
                for k in 0..4 {
                    if w[k] > 0.0
                        && let Some(p) = palette.get(j[k] as usize)
                    {
                        acc += *p * (w[k] / sum);
                    }
                }
                acc
            } else {
                fallback
            };
            let p = m.transform_point3(Vec3::from(v.pos));
            let n = (Mat3::from_mat4(m) * Vec3::from(v.normal)).normalize_or_zero();
            Vertex { pos: p.to_array(), normal: n.to_array(), uv: v.uv }
        })
        .collect()
}

/// Which path draws the bar.
#[derive(Clone, Copy, PartialEq)]
enum How {
    /// Bake the pose into the vertex buffer, draw through the ordinary pipeline.
    Cpu,
    /// Draw the bind pose through `vs_skin` with the palette.
    Gpu,
    /// Draw the bind pose through the ordinary pipeline — no deform at all. The
    /// control: if either path above matches THIS, the pose never applied.
    Bind,
}

fn render(gpu: &Gpu, how: How, bend: f32) -> (wgpu::Texture, Vec<[u8; 4]>) {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("skin-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("skin-depth"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let (mesh_data, joints, weights) = bar();
    let (fallback, palette) = pose(bend);

    let mut raster = Raster::new(gpu);
    let data = match how {
        How::Cpu => MeshData {
            vertices: cpu_skin(&mesh_data.vertices, &joints, &weights, fallback, &palette),
            ..mesh_data.clone()
        },
        _ => mesh_data.clone(),
    };
    let mesh = raster.register(gpu, &data, None);
    let skin_base = raster.register_skin(gpu, &joints, &weights);
    assert!(skin_base != 0, "the skinning store refused the part");

    let eye = Vec3::new(3.4, 1.6, 4.6);
    let target = Vec3::new(0.0, 1.5, 0.0);
    let fwd = (target - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
    );

    let mp = MaterialParams::flat([0.85, 0.82, 0.75]);
    let raw = instance_of_mat(Mat4::from_translation(-eye), &mp);
    let l = Vec3::new(0.5, 0.7, 0.55).normalize();
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        light_dir: [l.x, l.y, l.z, 0.0],
        light_color: [1.0, 0.98, 0.93, 0.0],
        ambient: [0.14, 0.15, 0.20, 0.0],
        ..Default::default()
    };

    let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
    let mut skins: Vec<SkinDraw> = Vec::new();
    raster.begin_skin_frame();
    match how {
        How::Gpu => {
            let p = raster.push_skin_pose(skin_base, fallback, &palette);
            skins.push(SkinDraw { mesh, tex: None, instance: raw, pose: p });
        }
        How::Cpu | How::Bind => instances.push((mesh, None, raw)),
    }
    raster.draw_scene_with(
        gpu,
        &color_view,
        &depth_view,
        globals,
        &instances,
        &[],
        &skins,
        Some([0.02, 0.02, 0.05, 1.0]),
        None,
    );

    let px = readback(gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px = px.into_iter().map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p }).collect();
    (color, px)
}

/// Fraction of pixels where two renders differ by more than a hair. Not a byte
/// comparison: the CPU path hands the rasterizer pre-transformed floats while the
/// GPU one multiplies in the vertex shader, so the last bit of a position is
/// allowed to disagree — a silhouette is not.
fn disagreement(a: &[[u8; 4]], b: &[[u8; 4]]) -> f32 {
    let mut n = 0usize;
    for (p, q) in a.iter().zip(b) {
        let d = (0..3).map(|c| (p[c] as i32 - q[c] as i32).abs()).max().unwrap_or(0);
        if d > 8 {
            n += 1;
        }
    }
    n as f32 / a.len() as f32
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "skin_probe.png".into());
    let gpu = Gpu::headless(S, S);
    const BEND: f32 = 0.45;

    let (_, cpu) = render(&gpu, How::Cpu, BEND);
    let (gpu_tex, gpu_px) = render(&gpu, How::Gpu, BEND);
    let (_, bind) = render(&gpu, How::Bind, BEND);

    let vs_bind = disagreement(&gpu_px, &bind);
    let vs_cpu = disagreement(&gpu_px, &cpu);
    println!("GPU-skinned vs bind pose: {:.2}% of pixels differ", vs_bind * 100.0);
    println!("GPU-skinned vs CPU skin:  {:.2}% of pixels differ", vs_cpu * 100.0);

    // 1. The pose APPLIED. Without this the next assertion passes for a `vs_skin`
    //    that ignored the palette entirely — matching a CPU path that did too.
    assert!(
        vs_bind > 0.02,
        "the GPU render is the bind pose ({:.3}% differ) — vs_skin never read the palette",
        vs_bind * 100.0
    );

    // 2. THE ONE THAT MATTERS. The two paths are one behaviour: a character must
    //    not look different depending on whether its part made it into the
    //    skinning store.
    assert!(
        vs_cpu < 0.01,
        "GPU and CPU skinning disagree on {:.2}% of pixels — the vertex shader's \
         deform has drifted from `cpu_skin_part`",
        vs_cpu * 100.0
    );

    save_png(&gpu, &gpu_tex, &out);
    println!("wrote {out} — a smoothly curling tapered bar, deformed in the vertex shader");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded = (S * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(encoder.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let mut px = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            px.push([view[i], view[i + 1], view[i + 2], view[i + 3]]);
        }
    }
    drop(view);
    buf.unmap();
    px
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let bpp = 4u32;
    let unpadded = S * bpp;
    let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("png"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("png") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut pixels = Vec::with_capacity((S * S * 4) as usize);
    for row in 0..S {
        let start = (row * padded) as usize;
        for x in 0..S {
            let i = start + (x * bpp) as usize;
            if bgra {
                pixels.extend_from_slice(&[data[i + 2], data[i + 1], data[i], data[i + 3]]);
            } else {
                pixels.extend_from_slice(&data[i..i + 4]);
            }
        }
    }
    drop(data);
    buf.unmap();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
