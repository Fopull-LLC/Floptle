//! Hangar-corner lighting/texture probe — a stand-in for the solar builder
//! scene's shell (floor + wall + pad + emissive strip) with the REAL pattern
//! textures and hangar-ish lights, so material colors, triplanar densities and
//! light balance get tuned by LOOKING at a PNG instead of guessing in RON.
//! Also proves the scale-aware triplanar fix: tiles must repeat in WORLD
//! units on transform-scaled primitives (a 48-unit floor shows ~16 tiles at
//! scale 3, not one grotesquely stretched tile).
//!
//! Run: cargo run --release -p floptle-render --example hangar_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster,
    RenderCamera, TexId, TextureData,
};
use floptle_core::math::{DVec3, Quat, Vec3};

const W: u32 = 1100;
const H: u32 = 650;

fn load_png(path: &str) -> TextureData {
    let file = std::fs::File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().expect("png info");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png size")];
    let info = reader.next_frame(&mut buf).expect("png frame");
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf.chunks(3).flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => {
            buf.chunks(2).flat_map(|c| [c[0], c[0], c[0], c[1]]).collect()
        }
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        other => panic!("unhandled png color type {other:?}"),
    };
    TextureData { pixels, width: w, height: h }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "hangar.png".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hangar-color"),
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
    let unit = raster.register(&gpu, &cube(0.5), None);
    let root = "solar/textures/hangar";
    let floor_tex = raster.register_texture(&gpu, &load_png(&format!("{root}/floor_concrete.png")), Default::default());
    let wall_tex = raster.register_texture(&gpu, &load_png(&format!("{root}/wall_panels.png")), Default::default());
    let pad_tex = raster.register_texture(&gpu, &load_png(&format!("{root}/ceiling_plates.png")), Default::default());
    let hazard_tex = raster.register_texture(&gpu, &load_png(&format!("{root}/pad_hazard.png")), Default::default());

    // Camera: standing in the hangar looking at the back-left corner.
    let cam = RenderCamera::new(
        DVec3::new(6.0, 3.2, 14.0),
        Quat::from_rotation_y(0.35) * Quat::from_rotation_x(-0.08),
        Projection::Perspective { fov_y: 62f32.to_radians(), near: 0.1, far: 300.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);

    // Hangar-ish lights: warm ceiling pools + a cool door fill, dim blue sun.
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let lights = [
        ([-10.0, 10.5, -10.0], [1.0, 0.93, 0.8], 1.3, 20.0),
        ([10.0, 10.5, -10.0], [1.0, 0.93, 0.8], 1.3, 20.0),
        ([-10.0, 10.5, 10.0], [1.0, 0.93, 0.8], 1.2, 20.0),
        ([10.0, 10.5, 10.0], [1.0, 0.93, 0.8], 1.2, 20.0),
        ([0.0, 6.0, 0.0], [0.95, 0.98, 1.0], 0.9, 13.0),
        ([0.0, 7.0, 22.0], [0.55, 0.65, 0.85], 0.8, 26.0),
    ];
    for (i, (p, c, int, range)) in lights.iter().enumerate() {
        let rel = (DVec3::new(p[0], p[1], p[2]) - cam.world_position).as_vec3();
        point_pos[i] = [rel.x, rel.y, rel.z, *range];
        point_color[i] = [c[0] * int, c[1] * int, c[2] * int, 0.0];
    }

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.25, 0.8, 0.5, 0.0],
        light_color: [0.75 * 0.25, 0.8 * 0.25, 0.95 * 0.25, 0.0],
        ambient: [0.11, 0.115, 0.14, 0.0],
        point_count: [lights.len() as f32, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        ..Default::default()
    };

    let tri = |color: [f32; 3], scale: f32, spec: f32, shin: f32| MaterialParams {
        color,
        specular: [1.0, 1.0, 1.0],
        specular_strength: spec,
        shininess: shin,
        tile_mode: 2,
        tile: [scale, 4.0, 0.0, 0.0],
        ..MaterialParams::flat(color)
    };
    let place = |mesh: MeshId,
                 tex: Option<TexId>,
                 mat: &MaterialParams,
                 pos: [f64; 3],
                 scale: [f32; 3]| {
        let t = Transform {
            translation: DVec3::new(pos[0], pos[1], pos[2]),
            rotation: floptle_core::math::Quat::IDENTITY,
            scale: Vec3::new(scale[0], scale[1], scale[2]),
        };
        (mesh, tex, instance_of_mat(t.render_matrix(cam.world_position), mat))
    };

    let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        // Floor / pad / back wall / left wall / ceiling.
        place(unit, Some(floor_tex), &tri([0.42, 0.44, 0.48], 4.0, 0.12, 22.0), [0.0, -0.5, 0.0], [48.0, 1.0, 48.0]),
        place(unit, Some(hazard_tex), &tri([0.5, 0.5, 0.52], 8.0, 0.2, 30.0), [0.0, 0.05, 0.0], [8.0, 0.12, 8.0]),
        place(unit, Some(wall_tex), &tri([0.34, 0.36, 0.42], 4.5, 0.06, 10.0), [0.0, 6.5, -24.0], [48.0, 14.0, 1.0]),
        place(unit, Some(wall_tex), &tri([0.34, 0.36, 0.42], 4.5, 0.06, 10.0), [-24.0, 6.5, 0.0], [1.0, 14.0, 48.0]),
        place(unit, Some(pad_tex), &tri([0.22, 0.23, 0.27], 5.0, 0.03, 8.0), [0.0, 13.5, 0.0], [48.0, 1.0, 48.0]),
    ];
    // Emissive ceiling strip + a crate and a "part" stack for scale reference.
    let strip = MaterialParams {
        emissive: [1.0, 0.95, 0.82],
        emissive_strength: 1.4,
        unlit: true,
        ..MaterialParams::flat([1.0, 0.97, 0.9])
    };
    instances.push(place(unit, None, &strip, [-8.0, 12.9, 0.0], [1.4, 0.12, 30.0]));
    let crate_mat = MaterialParams { specular_strength: 0.1, ..MaterialParams::flat([0.5, 0.42, 0.3]) };
    instances.push(place(unit, None, &crate_mat, [-6.0, 0.85, -4.0], [1.5, 1.5, 1.5]));
    let hull = MaterialParams { specular_strength: 0.3, shininess: 40.0, ..MaterialParams::flat([0.85, 0.87, 0.9]) };
    instances.push(place(unit, None, &hull, [0.0, 1.6, 0.0], [1.0, 3.0, 1.0]));

    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, Some([0.04, 0.045, 0.06, 1.0]), None);
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out}");
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
    let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(H) } },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(encoder.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut flat = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            let p = if bgra { [p[2], p[1], p[0], p[3]] } else { p };
            flat.extend_from_slice(&p);
        }
    }
    drop(view);
    buf.unmap();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
