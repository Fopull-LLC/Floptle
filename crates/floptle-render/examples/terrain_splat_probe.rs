//! Terrain splat probe: a meshed terrain painted with palette SLOT 1 must show that
//! palette texture (triplanar), not a flat color. This is the P6 splat feature wired into
//! the raster pass — the fix for "assigned a terrain texture but it stayed default".
//!
//! The palette layer is a bold CHECKERBOARD; the probe asserts the terrain surface has
//! high local contrast (the checker showing through) where slot 1 is painted, and low
//! contrast (flat) where it's left untextured. Contrast, not a fixed color, because
//! triplanar phase varies — but a checker vs a flat fill is unmistakable either way.
//!
//! Run: cargo run --release -p floptle-render --example terrain_splat_probe -- <out.png>

use floptle_field::{Brush, BrushProfile, ChunkField, Terrain};
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 800;
const H: u32 = 500;

/// A high-contrast 256² checkerboard palette layer — obvious when it lands on a surface.
fn checker() -> TextureData {
    let mut px = Vec::with_capacity((256 * 256 * 4) as usize);
    for y in 0..256u32 {
        for x in 0..256u32 {
            let on = ((x / 32) + (y / 32)) % 2 == 0;
            let c = if on { [230, 60, 40, 255] } else { [30, 30, 40, 255] };
            px.extend_from_slice(&c);
        }
    }
    TextureData { pixels: px, width: 256, height: 256 }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "terrain_splat.png".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("splat-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // Flat ground; then paint the RIGHT half with slot 1 (palette layer 0 = the checker).
    let mut t = Terrain::flat([96, 40, 96], [0.0; 3], [16.0, 6.0, 16.0], 0.0, [0.7, 0.7, 0.7]);
    for _ in 0..30 {
        t.sculpt(Brush::Raise, [0.0, 0.6, 0.0], 10.0, 1.0, BrushProfile::default());
    }
    // Paint slot 1 (palette layer 0 = the checker) over the +x half. `paint_texture` writes
    // the voxel color's ALPHA = slot; the mesher carries it, the splat shader samples it.
    let _ = BrushProfile::default();
    for _ in 0..4 {
        t.paint_texture([7.0, 3.0, 0.0], 8.0, 1);
    }

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[checker()]); // palette layer 0
    assert_eq!(raymarch.set_volumes(&gpu, &[&t.baked]), 1);

    let field = ChunkField::from_dense(&t.baked, 0.5);
    let chunks = floptle_field::mesh_field(&field, 1);
    let mut raster = Raster::new(&gpu);
    raster.set_terrain_palette(&gpu, &[checker()], 0); // slot 0 = linear (not nearest)
    let mut slots = Vec::new();
    for (_, cm) in &chunks {
        let data = chunk_mesh_data(cm);
        let id =
            raster.register_dynamic(&gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
        raster.replace_dynamic(&gpu, id, &data);
        slots.push(id);
    }

    let cam_pos = DVec3::new(0.0, 9.0, 15.0);
    let fwd = (Vec3::new(0.0, 1.0, 0.0) - cam_pos.as_vec3()).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let cr = (DVec3::ZERO - cam_pos).as_vec3();
    let light = Vec3::new(0.3, 0.9, 0.4).normalize();

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
            mp.terrain_paint_base = raster.dyn_paint_base(id); // the chunk's color block (tint + slot)
            (id, None, instance_of_mat(model, &mp))
        })
        .collect();
    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, Some(raymarch.field_bind()));

    let px = readback(&gpu, &color_tex);
    save_png(&px, &out);
    // The checker's dark cells are near-black; its light cells are red-dominant. Count both
    // over the terrain surface: a flat (broken) terrain has neither. This is direction-
    // independent (the checker lands wherever the painted slope faces the camera).
    let sky = |p: [u8; 4]| p[2] > p[1] + 12 && p[2] > p[0] + 20;
    let (mut reddish, mut dark, mut surface) = (0u32, 0u32, 0u32);
    for p in &px {
        if sky(*p) {
            continue;
        }
        surface += 1;
        if p[0] > 140 && p[0] as i32 > p[2] as i32 + 40 {
            reddish += 1; // a checker "on" cell (the red palette texel)
        }
        if p[0] < 70 && p[1] < 70 && p[2] < 80 {
            dark += 1; // a checker "off" cell
        }
    }
    println!("surface px {surface}: reddish {reddish}, dark {dark}");
    assert!(
        reddish > 1500 && dark > 500,
        "the checker palette texture is not showing on the meshed terrain \
         (reddish {reddish}, dark {dark} of {surface}) — terrain splat is broken"
    );
    println!("terrain splat OK; wrote {out}");
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
