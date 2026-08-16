//! Headless spot-light probe — one wall, lit only by aimed lamps, so the thing
//! that has to be right is visible rather than argued about.
//!
//! Four panels across one image, left to right:
//!
//! 1. **no cone** — the omnidirectional lamp, unchanged. This is the regression
//!    guard: adding spots must not move it.
//! 2. **hard edge** — a 40° cone at softness 0: a crisp circle.
//! 3. **soft edge** — the same 40° cone at softness 0.6, faded from the middle
//!    out. Softness is a fraction OF the cone, not an addition to it.
//! 4. **aimed away** — the same spot rotated 90°. Must be black. A cone whose
//!    cosine test ran the wrong way round would light this panel and darken
//!    every other one, and the first three would still look plausible alone.
//!
//! Run: cargo run -p floptle-render --example spot_light_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshData, MeshId, Projection,
    Raster, RenderCamera, TexId, Vertex,
};
use glam::{DVec3, Quat};

const W: u32 = 1400;
const H: u32 = 380;

/// The cone lane, packed exactly as `shading::cone_lane` packs it. Duplicated
/// here rather than imported because the editor is a *bin* crate — and stated
/// out loud because two copies of a conversion is the shape that drifts.
fn cone(full_degrees: f32, softness: f32) -> [f32; 4] {
    if full_degrees >= 180.0 {
        return [-1.0, -1.0, 0.0, 0.0];
    }
    let outer = full_degrees * 0.5;
    let inner = outer * (1.0 - softness.clamp(0.0, 0.999));
    [outer.to_radians().cos(), inner.to_radians().cos(), 0.0, 0.0]
}

/// A flat square in the XY plane facing +Z, `n` quads a side.
fn grid_wall(half: f32, n: usize) -> MeshData {
    let mut vertices = Vec::with_capacity((n + 1) * (n + 1));
    for y in 0..=n {
        for x in 0..=n {
            let (u, v) = (x as f32 / n as f32, y as f32 / n as f32);
            vertices.push(Vertex {
                pos: [(u * 2.0 - 1.0) * half, (v * 2.0 - 1.0) * half, 0.0],
                normal: [0.0, 0.0, 1.0],
                uv: [u, 1.0 - v],
            });
        }
    }
    let mut indices = Vec::with_capacity(n * n * 6);
    let w = (n + 1) as u32;
    for y in 0..n as u32 {
        for x in 0..n as u32 {
            let i = y * w + x;
            indices.extend_from_slice(&[i, i + 1, i + w + 1, i, i + w + 1, i + w]);
        }
    }
    MeshData { vertices, indices, colors: None }
}

fn main() {
    // **A directory OR a file**, because the probes genuinely disagree about
    // which their argument is — `skin_probe` is handed a file, `area_light_probe`
    // right beside this one in CI is handed a directory, and CI passes the same
    // `$P` to both. Taking either means this cannot fail on the difference.
    let arg = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let path = std::path::Path::new(&arg);
    let out = if path.is_dir() || path.extension().is_none() {
        std::fs::create_dir_all(path).ok();
        path.join("spot_light.png").display().to_string()
    } else {
        arg
    };
    let gpu = Gpu::headless(W, H);

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    // A wall to catch the beams. Built subdivided rather than with `plane`,
    // which is two triangles: a cone across four vertices is four numbers
    // interpolated, and would draw a diamond whatever the shader did.
    let wall = raster.register(&gpu, &grid_wall(1.0, 160), None);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 12.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 46f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);

    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let mut point_cone = [[-1.0f32, -1.0, 0.0, 0.0]; 16];
    let mut point_rot = [[0.0f32, 0.0, 0.0, 1.0]; 16];

    // Four lamps, each two metres in front of its own wall panel, each aimed
    // down its own -Z at the wall behind it.
    let panels: [(f64, [f32; 4], Quat); 4] = [
        (-7.5, cone(180.0, 0.0), Quat::IDENTITY),
        (-2.5, cone(40.0, 0.0), Quat::IDENTITY),
        (2.5, cone(40.0, 0.6), Quat::IDENTITY),
        // Rotated a quarter turn about Y: the beam now runs along the wall
        // rather than into it, and this panel has to go dark.
        (7.5, cone(40.0, 0.0), Quat::from_rotation_y(std::f32::consts::FRAC_PI_2)),
    ];
    for (i, (x, c, q)) in panels.iter().enumerate() {
        let lpos = DVec3::new(*x, 0.0, 2.6);
        let lrel = (lpos - cam.world_position).as_vec3();
        // A range that stops short of the next panel along. Otherwise the
        // omnidirectional lamp on the left washes over all four and there is
        // nothing to compare.
        point_pos[i] = [lrel.x, lrel.y, lrel.z, 3.7];
        point_color[i] = [4.5, 4.2, 3.6, 0.0];
        point_cone[i] = *c;
        point_rot[i] = [q.x, q.y, q.z, q.w];
    }

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        // No directional and almost no ambient: whatever is lit here is lit by
        // the cones and by nothing else.
        light_color: [0.0; 4],
        ambient: [0.015, 0.015, 0.02, 0.0],
        point_count: [4.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        point_cone,
        point_rot,
        ..Default::default()
    };

    let mat = MaterialParams::flat([0.85, 0.85, 0.88]);
    // One wall panel per lamp, each a plane facing the camera.
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = panels
        .iter()
        .map(|(x, _, _)| {
            let t = Transform {
                translation: DVec3::new(*x, 0.0, 0.0),
                rotation: Quat::IDENTITY,
                scale: glam::Vec3::splat(2.3),
            };
            (wall, None, instance_of_mat(t.render_matrix(cam.world_position), &mat))
        })
        .collect();

    raster.draw_scene(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &instances,
        Some([0.01, 0.01, 0.02, 1.0]),
        None,
    );
    save_png(&gpu, &color_tex, &out);
    println!(
        "wrote {out} — left to right: no cone · hard 40° · soft 40° · aimed away (must be black)"
    );
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
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
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
