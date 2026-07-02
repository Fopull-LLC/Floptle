//! Headless point-light probe — a row of white spheres lit ONLY by a single point
//! light (near-zero directional/ambient), so the smooth range falloff + lit
//! hemisphere are obvious: the sphere nearest the light is bright, distant ones fade
//! to black past the light's range. Validates the raster point_diffuse path.
//!
//! Run: cargo run -p floptle-render --example point_light_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, RenderCamera, TexId,
};
use glam::{DVec3, Quat};

const W: u32 = 1300;
const H: u32 = 360;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "point_light.png".into());
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
    let sphere = raster.register(&gpu, &uv_sphere(0.85, 32, 48), None);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 9.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);

    // One point light at world (0, 1.5, 1.5), bright; range 7. Directional ~off.
    let lpos = DVec3::new(0.0, 1.5, 1.5);
    let lrel = (lpos - cam.world_position).as_vec3();
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    point_pos[0] = [lrel.x, lrel.y, lrel.z, 7.0];
    point_color[0] = [3.0, 2.7, 2.2, 0.0]; // warm, bright

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0, 0.0, 0.0, 0.0], // no directional — isolate the point light
        ambient: [0.04, 0.04, 0.05, 0.0],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
    };

    let mat = MaterialParams::flat([0.9, 0.9, 0.92]);
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = (0..7)
        .map(|i| {
            let x = -6.0 + i as f64 * 2.0;
            let t = Transform::from_translation(DVec3::new(x, 0.0, 0.0));
            (sphere, None, instance_of_mat(t.render_matrix(cam.world_position), &mat))
        })
        .collect();

    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, Some([0.02, 0.02, 0.04, 1.0]), None);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — one point light at center; spheres fade with distance/range");
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
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
