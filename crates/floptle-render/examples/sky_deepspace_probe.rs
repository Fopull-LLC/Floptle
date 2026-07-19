//! Visual verification of the DYNAMIC deep-space background (the built-in
//! `sky_color` flat-bg fallback: `deep_space()` → `atmo_composite`). Renders the
//! SAME atmospheric body from four framings to prove the context-aware behavior:
//!
//! - surface: deep inside the daytime shell — stars washed out, sky tinted.
//! - orbit: above the shell — the limb halo AND a visible starfield.
//! - deep: far out, the planet a tiny dot — a star vault + a faint milky-way band.
//! - sunward: aimed at the star across empty sky (no shell) — the DIM sun glint.
//!
//! Stars mode is on (the in-game path). Usage:
//!   cargo run -q -p floptle-render --example sky_deepspace_probe [-- <cfield>]

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
    target: DVec3,
}

fn main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "solar/terrain/planetoid.1.cfield".into());
    let bytes = std::fs::read(&path).expect("read cfield");
    let field = ChunkField::from_bytes(&bytes).expect("parse cfield");
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

    let shadow = field.to_dense(192).expect("shadow proxy");
    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_terrain_textures(&gpu, &[white()]);
    assert_eq!(raymarch.set_volumes(&gpu, &[&shadow]), 1);

    let chunks = floptle_field::mesh_field(&field, 1);
    let mut raster = Raster::new(&gpu);
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
    let light = Vec3::new(0.55, 0.6, 0.35).normalize();
    let s = light.as_dvec3();
    let views = [
        View {
            name: "surface",
            cam: DVec3::new(0.05 * r, r * 1.02 + 6.0, 10.0),
            target: DVec3::new(-0.15 * r, r * 0.92, -0.35 * r),
        },
        View {
            name: "orbit",
            cam: DVec3::new(1.55 * r, 0.85 * r, 1.55 * r),
            target: DVec3::ZERO,
        },
        View {
            name: "deep",
            cam: DVec3::new(1.55, 0.85, 1.55).normalize() * (32.0 * r),
            target: DVec3::ZERO,
        },
        View {
            // Above the planet on the sun side, looking OUT at the star across
            // empty sky (the shell is behind us → atmo off along the ray).
            name: "sunward",
            cam: s * (2.6 * r),
            target: s * 20000.0,
        },
    ];

    let star_world = light * 25000.0; // world-space star position
    for v in &views {
        let fwd = (v.target - v.cam).as_vec3().normalize();
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
        let star_rel = (star_world.as_dvec3() - v.cam).as_vec3();

        let mut pos = [[0.0f32; 4]; 4];
        let mut col = [[0.0f32; 4]; 4];
        pos[0] = [star_rel.x, star_rel.y, star_rel.z, 0.0];
        col[0] = [1.0, 0.97, 0.9, 625.0 * 1.0e6];

        let rg = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [star_rel.x, star_rel.y, star_rel.z, 1.0],
            star_meta: [1.0, 0.0, 0.0, 0.0],
            star_pos: pos,
            star_color: col,
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.16, 0.17, 0.22, 0.0],
            bg: [0.006, 0.007, 0.016, 1.0], // space void tint
            params: [2.0, 0.0, 0.0, 1.0],   // x = time (twinkle phase)
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
            light_dir: [star_rel.x, star_rel.y, star_rel.z, 1.0],
            light_color: [1.0, 0.96, 0.9, 0.0],
            ambient: [0.16, 0.17, 0.22, 0.0],
            terrain_mask: [0.0, 0.22, 0.0, 0.0],
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
        let path = format!("sky_ds_{}.png", v.name);
        save_png(&px, &path);
        // A quick numeric summary: mean luminance + brightest pixel (stars).
        let mut sum = 0.0f64;
        let mut maxl = 0u32;
        for p in &px {
            let l = p[0] as u32 + p[1] as u32 + p[2] as u32;
            sum += l as f64;
            maxl = maxl.max(l);
        }
        let mean = sum / (px.len() as f64 * 3.0);
        let top = px[(40 * W + W / 2) as usize];
        println!("wrote {path:<18} mean_lum {mean:6.2} max_lum {maxl:3} top-center {top:?}");
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
