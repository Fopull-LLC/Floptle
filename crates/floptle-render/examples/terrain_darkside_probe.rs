//! Dark-side / cave lighting probe on the REAL solar planetoid field — reproduces
//! Ty's 2026-07-20 report: pale light blobs on the night side, leaked-light discs on
//! cave walls, rigid banded shading at the terminator. Renders with the IN-GAME
//! lighting path (stars mode: inverse-square luminous body + marched shadows against
//! the coarse `to_dense(192)` shadow proxy, ambient 0.014 like solar's Lighting node).
//!
//! Views: `dark` (night-side surface), `cave` (inside a freshly dug chamber, star
//! roughly overhead — any light on the walls leaked through solid rock), `term`
//! (across the terminator).
//!
//! Usage: cargo run --release -p floptle-render --example terrain_darkside_probe \
//!            [-- <cfield> <out-prefix>]

use floptle_field::{Brush, BrushProfile, ChunkField};
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 900;
const H: u32 = 560;

fn white() -> TextureData {
    TextureData { pixels: vec![255, 255, 255, 255], width: 1, height: 1 }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().unwrap_or_else(|| "solar/terrain/planetoid.1.cfield".into());
    let prefix = args.next().unwrap_or_else(|| "darkside".into());
    let bytes = std::fs::read(&path).expect("read cfield (run gen_planetoid first)");
    let mut field = ChunkField::from_bytes(&bytes).expect("parse cfield");

    let chunk_units = floptle_field::CHUNK as f32 * field.voxel();
    let radius = field
        .chunk_coords()
        .iter()
        .map(|c| (c[0].abs().max(c[1].abs()).max(c[2].abs())) as f32 * chunk_units)
        .fold(0.0f32, f32::max)
        .max(chunk_units)
        * 0.82;
    println!("radius ≈ {radius:.0}");

    // The star sits along +L; the DARK pole is -L. Dig a cave chamber just under the
    // surface at the SUB-STELLAR point (star straight overhead): every wall inside is
    // in the planet's own shadow, so any light in there leaked through the roof.
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let surf = light * radius;
    let cave_c = light * (radius - 9.0);
    for i in 0..3 {
        let off = Vec3::new(i as f32 * 2.0 - 2.0, 0.0, 0.0);
        field.sculpt(Brush::Lower, cave_c + off, 5.0, 1.0, BrushProfile::default());
    }
    println!("dug cave at r-9 under {surf:?}");

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

    // Shadow proxy from the DUG field, capped exactly like the editor (192).
    let shadow = field.to_dense(192).expect("shadow proxy");
    let vox = [
        2.0 * shadow.half_extent[0] / shadow.dims[0] as f32,
        2.0 * shadow.half_extent[1] / shadow.dims[1] as f32,
        2.0 * shadow.half_extent[2] / shadow.dims[2] as f32,
    ];
    println!("proxy dims {:?}, voxel {:.2}x{:.2}x{:.2} units", shadow.dims, vox[0], vox[1], vox[2]);
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&shadow]), 1);

    let chunks = floptle_field::mesh_field(&field, 1);
    let mut raster = Raster::new(&gpu);
    let mut slots = Vec::new();
    for (_, cm) in &chunks {
        let data = chunk_mesh_data(cm);
        let id =
            raster.register_dynamic(&gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
        assert!(raster.replace_dynamic(&gpu, id, &data), "chunk upload");
        slots.push(id);
    }

    let r = radius as f64;
    let dark_pole = (-light).as_dvec3();
    // Tangent frame at the dark pole for a grazing surface view.
    let t1 = light.cross(Vec3::Y).normalize_or(Vec3::X);
    struct View {
        name: &'static str,
        cam: DVec3,
        target: Vec3,
    }
    let views = [
        View {
            name: "dark",
            cam: dark_pole * (r * 1.02) + (t1 * 30.0).as_dvec3(),
            target: (-light) * radius * 0.97,
        },
        View {
            name: "cave",
            cam: (cave_c - t1 * 3.0).as_dvec3(),
            target: cave_c + t1 * 8.0,
        },
        View {
            name: "term",
            cam: (t1 * radius * 1.05 + light * radius * 0.15).as_dvec3(),
            target: t1 * radius * 0.9 - light * radius * 0.1,
        },
    ];

    for v in &views {
        let fwd = (v.target - v.cam.as_vec3()).normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            v.cam,
            rot,
            Projection::Perspective { fov_y: 62f32.to_radians(), near: 0.05, far: 2000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let cr = (DVec3::ZERO - v.cam).as_vec3();
        let bc = Vec3::from(shadow.center);
        let hf = shadow.half_extent;

        // IN-GAME star lighting: luminous body 25k out, intensity 1.0 at the surface.
        let star_world = light * 25000.0;
        let star_rel = (star_world.as_dvec3() - v.cam).as_vec3();
        let mut star_pos = [[0.0f32; 4]; 4];
        let mut star_color = [[0.0f32; 4]; 4];
        star_pos[0] = [star_rel.x, star_rel.y, star_rel.z, 0.0];
        star_color[0] = [1.0, 0.97, 0.9, 625.0 * 1.0e6];
        let light_dir = [star_rel.x, star_rel.y, star_rel.z, 1.0];

        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir,
            star_meta: [1.0, 0.0, 0.0, 0.0],
            star_pos,
            star_color,
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.014, 0.014, 0.02, 0.0], // solar's actual night ambient
            bg: [0.006, 0.007, 0.016, 1.0],
            params: [0.0, 0.0, 0.0, 1.0],
            vol_center: {
                let mut a = [[0.0f32; 4]; 16];
                a[0] = [cr.x + bc.x, cr.y + bc.y, cr.z + bc.z, 3.0];
                a
            },
            vol_half: {
                let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16];
                a[0] = [hf[0], hf[1], hf[2], 0.6];
                a
            },
            terrain_tint: [1.0, 1.0, 1.0, 0.0],
            terrain_params: [16.0, 0.0, 0.0, 1.0],
            // solar Lighting node: softness 0.35 → k≈19, strength 1.0, distance 400.
            shadow_params: [1.0, 19.0, 1.0, 400.0],
            shadow_tint: [0.0, 0.0, 0.0, 0.0],
            ao_params: [1.0, 0.85, 1.5, 0.0],
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);

        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir,
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.014, 0.014, 0.02, 0.0],
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
            ..Default::default()
        };
        let model = Mat4::from_translation(cr);
        let instances: Vec<_> = slots
            .iter()
            .map(|&id| {
                let mut m = MaterialParams::flat([1.0, 1.0, 1.0]);
                m.terrain_paint_base = raster.dyn_paint_base(id);
                m.terrain_splat = true;
                (id, None, instance_of_mat(model, &m))
            })
            .collect();
        raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, Some(raymarch.field_bind()));

        let px = readback(&gpu, &color_tex);
        let out = format!("{prefix}_{}.png", v.name);
        save_png(&px, &out);
        // Report the brightness profile: how much of the frame exceeds what pure
        // night ambient could produce (albedo*ambient ≈ 0.014*1.6 → ~14/255 after
        // the 1.6 splat gain — call anything >40 "lit").
        let lit = px.iter().filter(|p| p[0] > 40 || p[1] > 40 || p[2] > 40).count();
        println!("{}: {} of {} px lit >40", v.name, lit, px.len());
    }
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
