//! Headless render probe — a textured skybox in a scene with NO terrain and NO
//! blobs (plus two reference meshes), to a PNG. This is the empty-march sky path:
//! every ray misses every bound (zero steps) and samples the equirect, which is
//! how a terrain-less scene draws its sky. The equirect is a generated latitude
//! gradient with longitude stripes, so orientation and wrapping are inspectable.
//!
//! Run:
//!   cargo run -p floptle-render --example sky_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of, uv_sphere, Globals, Gpu, InstanceRaw, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId, TextureData,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1024;
const H: u32 = 768;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let out = args.get(1).cloned().unwrap_or_else(|| "sky.png".into());

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
    let cube_id = raster.register(&gpu, &cube(0.7), None);
    let sphere_id = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
    let mut raymarch = Raymarch::new(&gpu);

    // A generated equirect: sky-to-horizon latitude gradient + 8 longitude hue
    // stripes (orientation is obvious, seams would be too).
    let (tw, th) = (512u32, 256u32);
    let mut pixels = Vec::with_capacity((tw * th * 4) as usize);
    for y in 0..th {
        let v = y as f32 / (th - 1) as f32; // 0 = zenith, 1 = nadir
        for x in 0..tw {
            let stripe = (x * 8 / tw) % 2 == 0;
            let top = if stripe { [0.25, 0.45, 0.95] } else { [0.15, 0.3, 0.8] };
            let bot = if stripe { [0.95, 0.6, 0.3] } else { [0.85, 0.45, 0.2] };
            let c = [
                top[0] + (bot[0] - top[0]) * v,
                top[1] + (bot[1] - top[1]) * v,
                top[2] + (bot[2] - top[2]) * v,
            ];
            pixels.extend_from_slice(&[
                (c[0] * 255.0) as u8,
                (c[1] * 255.0) as u8,
                (c[2] * 255.0) as u8,
                255,
            ]);
        }
    }
    raymarch.set_sky_texture(&gpu, Some(&TextureData { pixels, width: tw, height: th }));

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 6.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let aspect = W as f32 / H as f32;
    let view_proj = cam.view_proj(aspect);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        ..Default::default()
    };
    let mesh = |id: MeshId, pos: [f64; 3], color: [f32; 3]| -> (MeshId, Option<TexId>, InstanceRaw) {
        let t = Transform::from_translation(DVec3::from(pos));
        (id, None, instance_of(t.render_matrix(cam.world_position), color))
    };
    let instances = vec![
        mesh(cube_id, [-1.8, 0.0, 0.0], [0.9, 0.45, 0.35]),
        mesh(sphere_id, [1.8, 0.0, 0.0], [0.4, 0.7, 0.95]),
    ];

    // ZERO blobs (params[1] = 0) and NO terrain volumes — the whole raymarch pass
    // is sky. sky_params[0] = 1 (textured), identity rotation.
    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        bg: [1.0, 0.0, 1.0, 1.0], // magenta: if this shows, the sky path failed
        center: [0.0; 4],
        params: [0.0, 0.0, 0.0, 0.0],
        sky_params: [1.0, 0.0, 0.0, 0.0],
        ..Default::default()
    };

    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, None);

    save_png(&gpu, &color_tex, &out);
    println!("wrote {out}");
}

/// Read the color texture back and write it as an RGBA8 PNG.
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
