//! Reproduce the runtime dig path headlessly and LOOK at the crater — chasing
//! "digging carves massive squares, not spheres" on the generated planetoid.
//!
//! Applies the exact `terrain.dig` pipeline (Brush::Lower, dab spacing like
//! dig_tool.lua) at a surface point, then renders a close-up of the crater.
//! Also prints |∇d| stats near the surface: surface-nets interpolates the zero
//! crossing per edge, so a field whose stored distances aren't 1-Lipschitz
//! (the generator's displaced sphere overshoots) drags crossings toward the
//! inflated side and voxel-steps every carve.
//!
//! Usage: cargo run --release -p floptle-render --example dig_probe [-- <cfield>]
//! Writes dig_before.png / dig_after.png.

use floptle_field::{Brush, BrushProfile, ChunkField};
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 900;
const H: u32 = 560;

fn grad_stats(field: &ChunkField, around: Vec3, extent: f32) {
    let mut n = 0usize;
    let mut sum = 0.0f32;
    let mut worst = 0.0f32;
    let step = field.voxel();
    let mut p = around - Vec3::splat(extent);
    while p.x < around.x + extent {
        p.y = around.y - extent;
        while p.y < around.y + extent {
            p.z = around.z - extent;
            while p.z < around.z + extent {
                if field.d(p).abs() < field.band() * 0.5 {
                    let g = field.grad(p).length();
                    n += 1;
                    sum += g;
                    if g > worst {
                        worst = g;
                    }
                }
                p.z += step;
            }
            p.y += step;
        }
        p.x += step;
    }
    println!(
        "|∇d| near surface around {around:?}: mean {:.2}, worst {:.2} ({n} samples)",
        sum / n.max(1) as f32,
        worst
    );
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "solar/terrain/planetoid.1.cfield".into());
    let bytes = std::fs::read(&path).expect("read cfield (run gen_planetoid first)");
    let mut field = ChunkField::from_bytes(&bytes).expect("parse cfield");

    // A surface point: march inward from well outside the planet.
    let dir = Vec3::new(0.62, 0.55, 0.56).normalize();
    let hit = field.raycast(dir * 80.0, -dir, 120.0).expect("surface hit");
    println!("surface at {hit:?} (r = {:.1})", hit.length());
    grad_stats(&field, hit, 4.0);

    // The dig_tool stroke: dabs spaced radius*0.45 along a short surface drag,
    // exactly the runtime op (Brush::Lower, default profile).
    let radius = 1.3f32;
    let strength = 0.6f32;
    let along = dir.cross(Vec3::Y).normalize();
    for i in 0..4 {
        let center = hit + along * (radius * 0.45 * i as f32);
        let touched = field.sculpt(Brush::Lower, center, radius, strength, BrushProfile::default());
        println!("dab {i}: {} chunks touched", touched.len());
    }
    // Crater extent = farthest point that FLIPPED solid→air vs the pristine field
    // (raw d>0 along a tangent line is confounded by planet curvature).
    let pristine_ref = ChunkField::from_bytes(&bytes).expect("parse cfield");
    let mut carved_reach = 0.0f32;
    let stroke_mid = hit + along * (radius * 0.45 * 1.5);
    let mut p = stroke_mid - Vec3::splat(8.0);
    while p.x < stroke_mid.x + 8.0 {
        p.y = stroke_mid.y - 8.0;
        while p.y < stroke_mid.y + 8.0 {
            p.z = stroke_mid.z - 8.0;
            while p.z < stroke_mid.z + 8.0 {
                if pristine_ref.d(p) < -0.05 && field.d(p) > 0.05 {
                    carved_reach = carved_reach.max((p - stroke_mid).length());
                }
                p.z += 0.25;
            }
            p.y += 0.25;
        }
        p.x += 0.25;
    }
    println!(
        "solid→air flips reach {carved_reach:.2} units from the stroke (ball r_eff = {:.2})",
        radius * strength
    );
    grad_stats(&field, hit, 4.0);

    // Render before/after close-ups of the same spot.
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

    let pristine = ChunkField::from_bytes(&bytes).expect("parse cfield");
    for (name, f) in [("dig_before", &pristine), ("dig_after", &field)] {
        // Terrain-splat instances sample their palette textures through the raymarch
        // FIELD bind group — without it the surface draws black (solar_probe does this).
        let shadow = f.to_dense(96).expect("shadow proxy");
        let mut raymarch = Raymarch::new(&gpu);
        raymarch.set_terrain_textures(&gpu, &[TextureData {
            pixels: vec![255, 255, 255, 255],
            width: 1,
            height: 1,
        }]);
        assert_eq!(raymarch.set_volumes(&gpu, &[&shadow]), 1);
        let mut raster = Raster::new(&gpu);
        let chunks = floptle_field::mesh_field(f, 1);
        let mut slots = Vec::new();
        for (_, cm) in &chunks {
            let data = chunk_mesh_data(cm);
            let id = raster.register_dynamic(
                &gpu,
                data.vertices.len() as u32,
                data.indices.len() as u32,
                true,
            );
            assert!(raster.replace_dynamic(&gpu, id, &data), "chunk upload");
            slots.push(id);
        }
        // Camera hovers above the dig site looking down the surface normal.
        let cam_pos = (hit + dir * 7.0 + along * 2.0).as_dvec3();
        let fwd = (hit - cam_pos.as_vec3()).normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            cam_pos,
            rot,
            Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 500.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let light = (dir + Vec3::new(0.3, 0.5, -0.2)).normalize();
        let cr = (DVec3::ZERO - cam_pos).as_vec3();
        // Sky/depth-clear pass, exactly as the editor frames a terrain scene.
        let bc = Vec3::from(shadow.center);
        let hf = shadow.half_extent;
        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.25, 0.25, 0.28, 0.0],
            bg: [0.02, 0.02, 0.05, 1.0],
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
            shadow_params: [1.0, 12.0, 0.85, 220.0],
            shadow_tint: [0.02, 0.02, 0.05, 0.0],
            ao_params: [1.0, 0.85, 1.5, 0.0],
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.25, 0.25, 0.28, 0.0],
            ..Default::default()
        };
        let model = Mat4::from_translation(cr);
        let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        let instances: Vec<_> = slots
            .iter()
            .map(|&id| {
                let mut m = mat;
                m.terrain_paint_base = raster.dyn_paint_base(id);
                m.terrain_splat = true;
                (id, None, instance_of_mat(model, &m))
            })
            .collect();
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &instances,
            None,
            Some(raymarch.field_bind()),
        );
        let px = readback(&gpu, &color_tex);
        save_png(&px, &format!("{name}.png"));
        println!("wrote {name}.png");
    }
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let padded =
        (W * bpp).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("probe-readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    enc.copy_texture_to_buffer(
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
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = &data[(y * padded) as usize..];
        for x in 0..W {
            let i = (x * bpp) as usize;
            out.push([row[i + 2], row[i + 1], row[i], 255]);
        }
    }
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("png header").write_image_data(&flat).expect("png data");
}
