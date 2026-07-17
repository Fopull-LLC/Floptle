//! Planetoid probe: render the ACTUAL solar-demo cfields (planet + moon) with the
//! ACTUAL palette sidecar, from four vantage points — orbit, surface, inside a cave,
//! and the moon — and save PNGs for eyeball verification. This is the visual test
//! for the textured-planets pass: biome splats on the surface, strata in dig walls,
//! and GLOWING magma/crystal slots that must stay readable in an unlit cave.
//!
//! Run: cargo run --release -p floptle-render --example planetoid_probe
//!      [-- <terrain_dir> <out_prefix>]   (defaults: solar/terrain, planetoid_probe)

use floptle_field::ChunkField;
use floptle_render::{
    chunk_mesh_data, instance_of_mat, Globals, Gpu, MaterialParams, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TextureData,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 600;

/// Load one palette PNG as a 256² RGBA layer (the packs are 256² already; RGB gets
/// an alpha channel appended). Missing/unreadable → white, same as the editor.
fn load_layer(path: &str) -> TextureData {
    let white = || TextureData { pixels: vec![255; 256 * 256 * 4], width: 256, height: 256 };
    let Ok(file) = std::fs::File::open(path) else { return white() };
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let Ok(mut reader) = decoder.read_info() else { return white() };
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(0)];
    let Ok(info) = reader.next_frame(&mut buf) else { return white() };
    if info.width != 256 || info.height != 256 {
        eprintln!("palette texture {path} is {}x{}, expected 256x256 — using white", info.width, info.height);
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
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(String::as_str).unwrap_or("solar/terrain");
    let prefix = args.get(2).map(String::as_str).unwrap_or("planetoid_probe");

    let planet = ChunkField::from_bytes(&std::fs::read(format!("{dir}/planetoid.1.cfield")).expect("planet cfield"))
        .expect("parse planet");
    let moon = ChunkField::from_bytes(&std::fs::read(format!("{dir}/planetoid.2.cfield")).expect("moon cfield"))
        .expect("parse moon");

    // The palette sidecar: path per slot, `|glow` marks self-lit slots (bit i = slot i).
    let text = std::fs::read_to_string(format!("{dir}/planetoid.palette")).expect("palette");
    let mut glow_mask = 0u32;
    let layers: Vec<TextureData> = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let path = match line.strip_suffix("|glow") {
                Some(p) => {
                    glow_mask |= 1 << i;
                    p
                }
                None => line,
            };
            load_layer(path)
        })
        .collect();
    println!("palette: {} layers, glow mask {glow_mask:#b}", layers.len());

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

    let mut raymarch = Raymarch::new(&gpu);
    let mut raster = Raster::new(&gpu);
    raster.set_terrain_palette(&gpu, &layers, 0);

    let register = |field: &ChunkField, raster: &mut Raster| -> Vec<floptle_render::MeshId> {
        let t0 = std::time::Instant::now();
        let chunks = floptle_field::mesh_field(field, 1);
        let mut ids = Vec::new();
        let mut verts = 0usize;
        for (_, cm) in &chunks {
            if cm.indices.is_empty() {
                continue;
            }
            let data = chunk_mesh_data(cm);
            verts += data.vertices.len();
            let id = raster.register_dynamic(&gpu, data.vertices.len() as u32, data.indices.len() as u32, true);
            raster.replace_dynamic(&gpu, id, &data);
            ids.push(id);
        }
        println!("meshed {} chunks, {verts} verts in {} ms", ids.len(), t0.elapsed().as_millis());
        ids
    };
    let planet_ids = register(&planet, &mut raster);
    let moon_ids = register(&moon, &mut raster);

    // Find a CAVE worth photographing: collect air-with-headroom points below the
    // surface, prefer one whose walls carry a GLOWING slot (6/7 — magma/amethyst),
    // and aim the camera down the longest open direction instead of into a wall.
    let dirs26: Vec<Vec3> = {
        let mut v = Vec::new();
        for x in -1..=1 {
            for y in -1..=1 {
                for z in -1..=1 {
                    if (x, y, z) != (0, 0, 0) {
                        v.push(Vec3::new(x as f32, y as f32, z as f32).normalize());
                    }
                }
            }
        }
        v
    };
    let mut best: Option<(Vec3, Vec3, i32)> = None; // (pos, look dir, score)
    for iy in -20..=20 {
        for ix in -40..40 {
            let th = ix as f32 * 0.157;
            let ph = iy as f32 * 0.07;
            let dirv = Vec3::new(th.cos() * ph.cos(), ph.sin(), th.sin() * ph.cos());
            for rr in 0..14 {
                let p = dirv * (300.0 - 46.0 + rr as f32 * 1.5);
                if planet.d(p) <= 3.0 {
                    continue;
                }
                // Score: glowing material on any nearby wall + open space to look down.
                let mut score = 0;
                let mut look = Vec3::X;
                let mut longest = 0.0f32;
                for d in &dirs26 {
                    if let Some(hit) = planet.raycast(p, *d, 40.0) {
                        let slot = planet.color(hit)[3];
                        if slot == 6 || slot == 7 {
                            score += 10;
                        }
                        let free = (hit - p).length();
                        if free > longest {
                            longest = free;
                            look = *d;
                        }
                    }
                }
                score += longest as i32;
                if best.as_ref().is_none_or(|(_, _, s)| score > *s) {
                    best = Some((p, look, score));
                }
            }
        }
        if best.as_ref().is_some_and(|(_, _, s)| *s >= 40) {
            break; // glowing walls + a long gallery: good enough, stop scanning
        }
    }
    let (cave, cave_look, cave_score) = best.expect("no cave air found — did the cave gate change?");
    println!("cave found at {cave} (r = {:.1}, score {cave_score})", cave.length());

    let shot = |name: &str,
                cam_pos: DVec3,
                target: Vec3,
                ids: &[floptle_render::MeshId],
                ambient: [f32; 3],
                bg: [f32; 4],
                sun: f32,
                raster: &mut Raster,
                raymarch: &mut Raymarch| {
        let fwd = (target - cam_pos.as_vec3()).normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            cam_pos,
            rot,
            Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 4000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let cr = (DVec3::ZERO - cam_pos).as_vec3();
        let light = Vec3::new(0.45, 0.75, 0.35).normalize();

        let rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0 * sun, 0.97 * sun, 0.9 * sun, 0.0],
            ambient: [ambient[0], ambient[1], ambient[2], 0.0],
            bg,
            params: [0.0, 0.0, 0.0, 1.0],
            ..Default::default()
        };
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);

        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0 * sun, 0.97 * sun, 0.9 * sun, 0.0],
            ambient: [ambient[0], ambient[1], ambient[2], 0.0],
            terrain_mask: [0.0, 0.22, glow_mask as f32, 0.0],
            ..Default::default()
        };
        let model = Mat4::from_translation(cr);
        let instances: Vec<_> = ids
            .iter()
            .map(|&id| {
                let mut mp =
                    MaterialParams { terrain_splat: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
                mp.terrain_paint_base = raster.dyn_paint_base(id);
                (id, None, instance_of_mat(model, &mp))
            })
            .collect();
        raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &instances, None, Some(raymarch.field_bind()));

        let px = readback(&gpu, &color_tex);
        let out = format!("{prefix}_{name}.png");
        save_png(&px, &out);
        println!("wrote {out}");
        px
    };

    let space = [0.01, 0.01, 0.03, 1.0];
    // Orbit: the whole planet in frame.
    shot("orbit", DVec3::new(0.0, 260.0, 780.0), Vec3::ZERO, &planet_ids, [0.25, 0.25, 0.28], space, 1.0, &mut raster, &mut raymarch);
    // Surface: standing height near the north-pole spawn, looking at the horizon.
    let eye = Vec3::new(6.0, 312.0, 10.0);
    shot("surface", eye.as_dvec3(), Vec3::new(60.0, 296.0, 90.0), &planet_ids, [0.25, 0.25, 0.28], space, 1.0, &mut raster, &mut raymarch);
    // Cave: sun fully OFF (the probe binds no field, so sun_shadow can't occlude —
    // with sun on, direct light would fake its way through 30 m of rock) plus a
    // whisper of ambient: the glow slots have to carry the image on their own.
    let cave_px = shot("cave", cave.as_dvec3(), cave + cave_look * 20.0, &planet_ids, [0.04, 0.04, 0.05], [0.0, 0.0, 0.0, 1.0], 0.0, &mut raster, &mut raymarch);
    // Moon: full disc.
    shot("moon", DVec3::new(0.0, 30.0, 110.0), Vec3::ZERO, &moon_ids, [0.25, 0.25, 0.28], space, 1.0, &mut raster, &mut raymarch);

    // The cave must not be pitch black: glowing slots bypass lighting, so SOME pixels
    // should be clearly bright even with ambient 0.04.
    let bright = cave_px.iter().filter(|p| p[0].max(p[1]).max(p[2]) > 90).count();
    println!("cave bright pixels: {bright}");
    assert!(bright > 400, "cave is unreadable — glow slots are not lighting it ({bright} bright px)");
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
