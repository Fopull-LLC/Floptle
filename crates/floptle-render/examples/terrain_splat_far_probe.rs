//! Far-slot splat transition probe: a boundary between palette slots 1 and 7 must
//! crossfade the TWO textures actually painted — never march through the layers in
//! between. The old shader lerped the interpolated slot INDEX (`floor(a)`↔`ceil(a)`),
//! so a 1↔7 seam sampled layers 2–6 across the transition ("two other textures
//! transitioning between them" — Ty, 2026-07-20).
//!
//! Palette: slot 1 = solid RED, slots 2–6 = solid GREEN (the tell), slot 7 = solid
//! BLUE. The left half is painted slot 1, the right half slot 7. Any green on the
//! surface means the shader touched an in-between layer; the probe fails on it.
//!
//! Run: cargo run --release -p floptle-render --example terrain_splat_far_probe -- <out.png>

use floptle_field::{Brush, BrushProfile, ChunkField, Terrain};
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 800;
const H: u32 = 500;

fn solid(rgb: [u8; 3]) -> TextureData {
    let mut px = Vec::with_capacity((256 * 256 * 4) as usize);
    for _ in 0..256 * 256 {
        px.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
    }
    TextureData { pixels: px, width: 256, height: 256 }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "terrain_splat_far.png".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("splat-far-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // Flat ground with a little relief so lighting varies; then paint slot 1 over the
    // whole slab and slot 7 over the +x half — the seam runs down the middle.
    let mut t = Terrain::flat([96, 40, 96], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.9, 0.9, 0.9]);
    for _ in 0..12 {
        t.sculpt(Brush::Raise, [0.0, 0.6, 0.0], 10.0, 1.0, BrushProfile::default());
    }
    for _ in 0..4 {
        t.paint_texture([-8.0, 2.0, 0.0], 12.0, 1);
        t.paint_texture([-8.0, -2.0, 0.0], 12.0, 1);
        t.paint_texture([8.0, 2.0, 0.0], 12.0, 7);
        t.paint_texture([8.0, -2.0, 0.0], 12.0, 7);
    }

    // Palette: RED / 5×GREEN / BLUE. Green anywhere on the surface = the shader
    // sampled a layer between the two painted slots.
    let layers = [
        solid([235, 40, 30]),
        solid([40, 230, 40]),
        solid([40, 230, 40]),
        solid([40, 230, 40]),
        solid([40, 230, 40]),
        solid([40, 230, 40]),
        solid([40, 40, 235]),
    ];

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &layers);
    assert_eq!(raymarch.set_volumes(&gpu, &[&t.baked]), 1);

    let field = ChunkField::from_dense(&t.baked, 0.5);
    let chunks = floptle_field::mesh_field(&field, 1);
    let mut raster = Raster::new(&gpu);
    raster.set_terrain_palette(&gpu, &layers, 0);
    let mut slots = Vec::new();
    for (_, cm) in &chunks {
        let data = chunk_mesh_data(cm);
        let id =
            raster.register_dynamic(&gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
        raster.replace_dynamic(&gpu, id, &data);
        slots.push(id);
    }

    let cam_pos = DVec3::new(0.0, 14.0, 12.0);
    let fwd = (Vec3::new(0.0, 0.0, 0.0) - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let cr = (DVec3::ZERO - cam_pos).as_vec3();
    let light = Vec3::new(0.3, 0.9, 0.4).normalize();

    // Kind 3 (meshed terrain): the raster draws the mesh; the field supplies the
    // color atlas the weight-blended splat reads its exact slots from.
    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.3, 0.3, 0.3, 0.0],
        bg: [0.5, 0.6, 0.75, 1.0],
        params: [0.0, 0.0, 0.0, 1.0],
        vol_center: { let mut a = [[0.0f32; 4]; 16]; a[0] = [cr.x, cr.y, cr.z, 3.0]; a },
        vol_half: { let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16]; a[0] = [16.0, 6.0, 16.0, 0.6]; a },
        ..Default::default()
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.3, 0.3, 0.3, 0.0],
        terrain_mask: [0.0, 0.22, 0.0, 0.0],
        ..Default::default()
    };
    let model = Mat4::from_translation(cr);
    let instances: Vec<_> = slots
        .iter()
        .map(|&id| {
            let mut mp = MaterialParams { terrain_splat: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
            mp.terrain_paint_base = raster.dyn_paint_base(id);
            (id, None, instance_of_mat(model, &mp))
        })
        .collect();
    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, Some(raymarch.field_bind()));

    let px = readback(&gpu, &color_tex);
    save_png(&px, &out);

    // Green = the tell. Red/blue must both be present (the two painted slots).
    let (mut red, mut blue, mut green) = (0u32, 0u32, 0u32);
    for p in &px {
        let (r, gc, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
        if r > gc + 40 && r > b + 40 {
            red += 1;
        } else if b > gc + 40 && b > r + 40 && r < 120 {
            blue += 1;
        } else if gc > r + 40 && gc > b + 40 {
            green += 1;
        }
    }
    println!("red {red}, blue {blue}, green {green}");
    assert!(red > 3000 && blue > 3000, "both painted slots must show (red {red}, blue {blue})");
    assert!(
        green < 50,
        "{green} green pixels: the splat sampled palette layers BETWEEN the two painted \
         slots — the transition must blend only the slots actually present"
    );
    println!("far-slot splat transition OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded = (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(H) } },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
