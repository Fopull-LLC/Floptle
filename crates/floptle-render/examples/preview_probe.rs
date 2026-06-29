//! Headless probe for the Inspector asset preview: render the turntable subject the
//! way the editor does — an orbit camera looking at the origin, the subject drawn
//! camera-relative, into an offscreen target — and save a PNG. Uses a material
//! sphere (the exact material-preview subject); the model path shares this math.
//! Validates the preview camera math + draw_scene-to-offscreen path.
//!
//! Run: cargo run -p floptle-render --example preview_probe -- <out.png>

use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, RenderCamera, TexId,
};
use glam::{Mat3, Mat4, Quat, Vec3};

const S: u32 = 320;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "preview.png".into());
    let gpu = Gpu::headless(S, S);

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("preview-color"),
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
        label: Some("preview-depth"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    let sphere = raster.register(&gpu, &uv_sphere(0.85, 24, 36), None);
    let radius = 0.85f32;

    // A glossy orange material with rim light — exercises the material params path.
    let mat = MaterialParams {
        color: [0.85, 0.42, 0.12],
        emissive: [0.0; 3],
        emissive_strength: 0.0,
        specular: [1.0, 0.95, 0.85],
        shininess: 48.0,
        specular_strength: 1.0,
        rim: [0.2, 0.5, 0.9],
        rim_strength: 1.2,
        unlit: false,
        ambient: 1.0,
    };

    // The exact orbit-camera math the editor preview uses (spin angle 0.7 rad).
    let a = 0.7f32;
    let dist = (radius * 3.0).max(0.4);
    let eye = Vec3::new(a.cos() * dist, radius * 0.55, a.sin() * dist);
    let fwd = (Vec3::ZERO - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
    );
    let vp = cam.view_proj(1.0);
    let model_mat = Mat4::from_translation(-eye);
    let raw = instance_of_mat(model_mat, &mat);
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![(sphere, None, raw)];

    let l = Vec3::new(0.5, 0.8, 0.6).normalize();
    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [l.x, l.y, l.z, 0.0],
        light_color: [1.0, 0.98, 0.93, 0.0],
        ambient: [0.30, 0.32, 0.38, 0.0],
    };
    raster.draw_scene(&gpu, &color_view, &depth_view, globals, &instances, Some([0.07, 0.08, 0.10, 1.0]));

    save_png(&gpu, &color, &out);
    println!("wrote {out}");
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let bpp = 4u32;
    let unpadded = S * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
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
    let mut pixels = Vec::with_capacity((S * S * 4) as usize);
    for row in 0..S {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
