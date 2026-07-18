//! Render the Floptle Solar planetoid headlessly — visual verification of the whole
//! Terrain 2.0 stack on a generated field: `.cfield` load → surface-nets chunk meshes
//! through the dynamic raster arena, shadow proxy (`to_dense`) feeding the SDF atlas
//! as a `w = 3` volume (sun shadows + AO, not drawn), sky from the raymarch pass.
//!
//! Usage: cargo run --release -p floptle-render --example solar_probe [-- <cfield>]
//! Writes solar_orbit.png (whole planet) and solar_surface.png (astronaut's view).

use floptle_field::ChunkField;
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

struct View {
    name: &'static str,
    cam: DVec3,
    target: Vec3,
}

/// Load one palette PNG as a 256² RGBA layer (missing/unreadable → white).
fn load_layer(path: &str) -> TextureData {
    let white = || TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 };
    let Ok(file) = std::fs::File::open(path) else { return white() };
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let Ok(mut reader) = decoder.read_info() else { return white() };
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let Ok(info) = reader.next_frame(&mut buf) else { return white() };
    if info.width != 256 || info.height != 256 {
        return white();
    }
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf[..(256 * 256 * 4)].to_vec(),
        png::ColorType::Rgb => buf[..(256 * 256 * 3)]
            .chunks_exact(3)
            .flat_map(|c| [c[0], c[1], c[2], 255])
            .collect(),
        _ => return white(),
    };
    TextureData { pixels, width: 256, height: 256 }
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "solar/terrain/planetoid.1.cfield".into());
    let bytes = std::fs::read(&path).expect("read cfield (run gen_planetoid first)");
    let field = ChunkField::from_bytes(&bytes).expect("parse cfield");
    println!(
        "field: {} chunks, {:.1} MB resident",
        field.data_chunks(),
        field.memory_bytes() as f64 / 1e6
    );
    // Body radius estimate from the field's chunk footprint — frames the views
    // for any body size (the old constants assumed the tiny first planetoid).
    let chunk_units = floptle_field::CHUNK as f32 * field.voxel();
    let radius = field
        .chunk_coords()
        .iter()
        .map(|c| {
            let m = c[0].abs().max(c[1].abs()).max(c[2].abs());
            (m as f32) * chunk_units
        })
        .fold(0.0f32, f32::max)
        .max(chunk_units)
        * 0.82;
    println!("radius ≈ {radius:.0}");
    // The scene palette sidecar next to the cfield: <dir>/<scene>.palette.
    let ppath = {
        let pb = std::path::Path::new(&path);
        let scene = pb.file_stem().and_then(|s| s.to_str()).unwrap_or("planetoid");
        let scene = scene.split('.').next().unwrap_or(scene);
        pb.with_file_name(format!("{scene}.palette"))
    };
    let mut glow_mask = 0u32;
    let mut layers: Vec<TextureData> = Vec::new();
    if let Ok(text) = std::fs::read_to_string(&ppath) {
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let tex = match line.strip_suffix("|glow") {
                Some(p2) => {
                    glow_mask |= 1 << i;
                    p2
                }
                None => line,
            };
            layers.push(load_layer(tex));
        }
        println!("palette: {} layers from {} (glow {glow_mask:#b})", layers.len(), ppath.display());
    } else {
        println!("no palette sidecar at {} — untextured", ppath.display());
    }

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

    // Shadow proxy → the SDF atlas, exactly as the editor derives it (P3).
    let shadow = field.to_dense(192).expect("shadow proxy");
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&shadow]), 1);

    // Chunk meshes through the dynamic arena, exactly as the editor uploads them.
    let chunks = floptle_field::mesh_field(&field, 1);
    let tris: usize = chunks.iter().map(|(_, m)| m.tri_count()).sum();
    println!("mesh : {} chunks, {tris} tris", chunks.len());
    let mut raster = Raster::new(&gpu);
    if !layers.is_empty() {
        raster.set_terrain_palette(&gpu, &layers, 0);
    }
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

    let r = radius as f64;
    let views = [
        View {
            name: "orbit",
            cam: DVec3::new(1.55 * r, 0.85 * r, 1.55 * r),
            target: Vec3::ZERO,
        },
        View {
            name: "surface",
            cam: DVec3::new(0.05 * r, r * 1.02 + 6.0, 0.25 * r * 0.0 + 10.0),
            target: Vec3::new(-0.15 * radius, radius * 0.92, -0.35 * radius),
        },
    ];
    let light = Vec3::new(0.55, 0.6, 0.35).normalize();

    for v in &views {
        let fwd = (v.target - v.cam.as_vec3()).normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            v.cam,
            rot,
            Projection::Perspective { fov_y: 62f32.to_radians(), near: 0.05, far: 2000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let cr = (DVec3::ZERO - v.cam).as_vec3(); // planet at the world origin

        let bc = Vec3::from(shadow.center);
        let hf = shadow.half_extent;
        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.16, 0.17, 0.22, 0.0],
            bg: [0.006, 0.007, 0.016, 1.0], // space
            params: [0.0, 0.0, 0.0, 1.0],
            vol_center: {
                let mut a = [[0.0f32; 4]; 16];
                a[0] = [cr.x + bc.x, cr.y + bc.y, cr.z + bc.z, 3.0]; // w = 3: shadow + AO
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
            // S8 atmosphere (slot 0): a rusty sky shell half a radius deep with
            // a 45% cloud deck. The orbit view now sees its limb halo + clouds
            // from SPACE; the surface view gets the tinted sky + clouds overhead.
            atmo_meta: [1.0, 0.0, 0.0, 0.0],
            atmo_color: {
                let mut a = [[0.0f32; 4]; 4];
                a[0] = [0.78, 0.42, 0.3, 0.85];
                a
            },
            atmo_body: {
                let mut a = [[0.0f32, 0.0, 0.0, 1.0]; 4];
                a[0] = [cr.x, cr.y, cr.z, radius];
                a
            },
            atmo_params: {
                let mut a = [[0.0f32; 4]; 4];
                a[0] = [radius * 0.5, 0.45, 0.0, 0.0];
                a
            },
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rg);

        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.16, 0.17, 0.22, 0.0],
            terrain_mask: [0.0, 0.22, glow_mask as f32, 0.0],
            ..Default::default()
        };
        let model = Mat4::from_translation(cr);
        let mat = MaterialParams::flat([1.0, 1.0, 1.0]);
        let instances: Vec<_> = slots
            .iter()
            .map(|&id| {
                let mut m = mat;
                m.terrain_paint_base = raster.dyn_paint_base(id);
                m.terrain_splat = true; // vertex alpha = palette slot, not coverage
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
        let path = format!("solar_{}.png", v.name);
        save_png(&px, &path);
        println!("wrote {path}");
        if v.name == "surface" {
            // The atmosphere must tint the sky above the horizon: warm (r > b),
            // clearly brighter than raw space. Sampled at top-center.
            let sky = px[(40 * W + W / 2) as usize];
            println!("surface sky px {sky:?}");
            assert!(
                sky[0] > sky[2] && sky[0] > 40,
                "atmosphere missing from the surface sky: {sky:?}"
            );
        }
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
    // Swap only when the surface really is BGRA (headless commonly gives RGBA).
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = &data[(y * padded) as usize..];
        for x in 0..W {
            let i = (x * bpp) as usize;
            if bgra {
                out.push([row[i + 2], row[i + 1], row[i], 255]);
            } else {
                out.push([row[i], row[i + 1], row[i + 2], 255]);
            }
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
