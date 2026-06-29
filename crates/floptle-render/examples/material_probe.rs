//! Headless material probe — renders a row of spheres, each with a different
//! [`MaterialParams`], to a PNG. Validates the extended raster shader (emissive,
//! Blinn-Phong specular, rim/fresnel, unlit) and lets the retro material look be
//! inspected WITHOUT a window.
//!
//! Run: cargo run -p floptle-render --example material_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, RenderCamera, TexId, TextureData,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1440;
const H: u32 = 400;

/// A procedural checker texture (so the probe needs no image file).
fn checker() -> TextureData {
    let n = 64u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let c = if on { [60, 200, 90, 255] } else { [30, 120, 50, 255] };
            px.extend_from_slice(&c);
        }
    }
    TextureData { pixels: px, width: n, height: n }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "materials.png".into());
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
    let sphere = raster.register(&gpu, &uv_sphere(0.9, 32, 48), None);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 7.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
    };

    // Five distinct retro materials, left → right.
    let matte = MaterialParams::flat([0.4, 0.7, 0.95]);
    let mut emissive = MaterialParams::flat([0.15, 0.0, 0.15]);
    emissive.emissive = [1.0, 0.1, 0.9];
    emissive.emissive_strength = 1.4;
    let mut shiny = MaterialParams::flat([0.75, 0.18, 0.18]);
    shiny.specular = [1.0, 1.0, 1.0];
    shiny.shininess = 48.0;
    shiny.specular_strength = 1.0;
    let mut unlit = MaterialParams::flat([0.3, 0.9, 0.4]);
    unlit.unlit = true;
    let mut rim = MaterialParams::flat([0.18, 0.18, 0.22]);
    rim.rim = [0.3, 0.9, 1.0];
    rim.rim_strength = 1.6;

    // A sixth sphere: textured (a procedural grass-like checker) + lit.
    let grass_tex = raster.register_texture(&gpu, &checker());
    let grass = MaterialParams::flat([1.0, 1.0, 1.0]);

    let mats = [matte, emissive, shiny, unlit, rim];
    let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = mats
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let x = -5.5 + i as f64 * 2.2;
            let t = Transform::from_translation(DVec3::new(x, 0.0, 0.0));
            (sphere, None, instance_of_mat(t.render_matrix(cam.world_position), m))
        })
        .collect();
    let t = Transform::from_translation(DVec3::new(-5.5 + 5.0 * 2.2, 0.0, 0.0));
    instances.push((sphere, Some(grass_tex), instance_of_mat(t.render_matrix(cam.world_position), &grass)));

    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, Some([0.02, 0.02, 0.05, 1.0]));
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — matte / emissive / specular / unlit / rim / textured");
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
