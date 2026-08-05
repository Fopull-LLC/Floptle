//! Render an authored `.vfx.ron` headlessly, so an effect can be LOOKED AT
//! without opening the editor.
//!
//! `validate` proves an effect parses. Nothing proved what it looked like — and
//! parsing is the easy half. The first draft of a set of ice effects for a game
//! built on this engine validated perfectly and read as a handful of glitter,
//! because the only ice texture in its pack was a snowflake that bursts and
//! dissolves. That is not a class of mistake a schema can catch; you have to see
//! the frame. (Same lesson as `flsl_prepass_probe`: a shader that compiles is
//! not a shader that looks right.)
//!
//! It renders through the REAL path — `load_vfx_effect` -> `effect_from_doc` ->
//! `compile` -> `EffectInstance::simulate_to` -> `collect_billboards` -> the
//! particle pipeline — so what comes out is what the game draws, not an
//! approximation of it. Textures resolve by path against a project root, the
//! same way the editor's registry does, and load Pixelated because that is what
//! flipbook sprite sheets are.
//!
//! A one-metre grid post and a 1.8 m figure stand at the origin for scale, which
//! is the whole point when the note you are working from says "it needs to be
//! HUGE".
//!
//! Run:
//!   cargo run -p floptle-editor --example vfx_shot -- \
//!       --root <project-dir> --out <dir> [--t 0.05,0.2,0.5] [--dist 18] \
//!       <effect.vfx.ron> [...]
//!
//! Each effect writes one contact sheet: the requested timeline positions in a
//! row, labelled by filename.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_render::particles::{ParticleBatch, ParticleGlobals};
use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Particles, Projection, Raster,
    RenderCamera, TexId, TexSampling, cube, instance_of_mat,
};
use floptle_vfx::{EffectInstance, collect_billboards};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// `floptle-editor` is a binary crate with no `[lib]`, so its modules are not
// importable — and `effect_from_doc`, the one and only doc -> runtime conversion
// in the engine, lives in one of them. Including the REAL source file is how
// this probe stays honest: re-implementing the conversion here would give a
// picture of what a second implementation draws, which is worth nothing. The
// module it pulls in wants exactly one four-line path helper from `anim`, and
// `effect_from_doc` does not call it.
mod anim {
    use std::path::Path;
    pub fn asset_key(path: &Path, project_root: &Path, ext: &str) -> String {
        let rel = path.strip_prefix(project_root).unwrap_or(path);
        let s = rel.to_string_lossy().replace('\\', "/");
        s.strip_suffix(ext).unwrap_or(&s).to_string()
    }
}
#[path = "../src/vfx.rs"]
mod vfx;

const W: u32 = 640;
const H: u32 = 640;
const GRAVITY: Vec3 = Vec3::new(0.0, -9.81, 0.0);

struct Args {
    root: PathBuf,
    out: PathBuf,
    times: Vec<f32>,
    dist: f32,
    height: f32,
    at: DVec3,
    effects: Vec<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        root: ".".into(),
        out: ".".into(),
        // Early, middle and late by default: an effect that only looks right at
        // its peak is an effect that flickers.
        times: vec![0.05, 0.2, 0.45, 0.8],
        dist: 16.0,
        height: 4.0,
        // Where the effect is spawned. Almost nothing in a game is spawned at
        // the origin — a hit spark is at chest height, an ice block is centred
        // on a fighter, a conjured mountain is in the sky — and an effect judged
        // at the wrong height is judged half-buried in the floor.
        at: DVec3::ZERO,
        effects: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--root" => a.root = it.next().expect("--root <dir>").into(),
            "--out" => a.out = it.next().expect("--out <dir>").into(),
            "--dist" => a.dist = it.next().expect("--dist <n>").parse().expect("number"),
            "--height" => a.height = it.next().expect("--height <n>").parse().expect("number"),
            "--at" => {
                let v: Vec<f64> = it
                    .next()
                    .expect("--at x,y,z")
                    .split(',')
                    .map(|s| s.trim().parse().expect("number"))
                    .collect();
                a.at = DVec3::new(v[0], *v.get(1).unwrap_or(&0.0), *v.get(2).unwrap_or(&0.0));
            }
            "--t" => {
                a.times = it
                    .next()
                    .expect("--t 0.1,0.3")
                    .split(',')
                    .map(|s| s.trim().parse().expect("number"))
                    .collect()
            }
            _ => a.effects.push(arg.into()),
        }
    }
    assert!(!a.effects.is_empty(), "give at least one .vfx.ron path");
    a
}

/// Every texture path any track in `fx` names, resolved against `root` and
/// uploaded Pixelated. Missing files are reported rather than silently drawn as
/// untextured quads — a flipbook whose sheet did not load is a white square, and
/// a white square is easy to mistake for a design decision.
fn load_textures(
    gpu: &Gpu,
    raster: &mut Raster,
    root: &Path,
    fx: &floptle_vfx::ParticleEffect,
) -> HashMap<String, TexId> {
    let mut map = HashMap::new();
    for track in &fx.tracks {
        let floptle_vfx::RenderMode::Billboard { texture: Some(path) } = &track.look.render else {
            continue;
        };
        if map.contains_key(path) {
            continue;
        }
        let full = root.join(path);
        match floptle_assets::texture::load_texture(&full) {
            Some(tex) => {
                let id = raster.register_texture(gpu, &tex, TexSampling::default());
                map.insert(path.clone(), id);
            }
            None => eprintln!("  !! texture not found: {}", full.display()),
        }
    }
    map
}

fn main() {
    let args = parse_args();
    let gpu = Gpu::headless(W, H);

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vfx-shot"),
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
    let mut particles = Particles::new(&gpu);
    let box_mesh = raster.register(&gpu, &cube(1.0), None);

    // A fighting-game camera: level with the action, far enough back to hold a
    // stage. Effects in this engine are authored for a 2.5D side-on view and
    // judging one from a three-quarter angle tells you very little.
    let cam = RenderCamera::new(
        DVec3::new(0.0, args.height as f64, args.dist as f64),
        Quat::from_rotation_x(-0.12),
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let aspect = W as f32 / H as f32;
    let globals = Globals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        light_dir: [0.4, 0.8, 0.5, 0.0],
        light_color: [0.9, 0.88, 0.85, 0.0],
        ambient: [0.30, 0.32, 0.38, 0.0],
        ..Default::default()
    };
    let (r, u) = (cam.rotation * Vec3::X, cam.rotation * Vec3::Y);
    let pglobals = ParticleGlobals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        cam_right: [r.x, r.y, r.z, 0.0],
        cam_up: [u.x, u.y, u.z, 0.0],
        fog_color: [0.0; 4],
        fog_params: [0.0; 4],
    };

    // THE SCALE REFERENCE, and the reason this probe is worth having. A floor, a
    // 1.8-unit figure at the origin, and a post marked off every metre: "huge"
    // and "tiny" are not properties of an effect, they are properties of an
    // effect next to a fighter.
    let mut scene: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
    // `size` is the box's FULL extent in world units. `cube(1.0)` spans -1..+1,
    // so the scale that produces it is half of it — and getting that wrong is not
    // a cosmetic error in a tool whose entire job is answering "how big is this":
    // the first version drew a "1.8-unit fighter" 3.6 units tall and made every
    // effect measured against it look half the size it really was.
    let mut place = |pos: [f64; 3], size: [f32; 3], color: [f32; 3]| {
        let m = MaterialParams::flat(color);
        let mut t = Transform::from_translation(DVec3::from_array(pos));
        t.scale = Vec3::from_array(size) * 0.5;
        scene.push((box_mesh, None, instance_of_mat(t.render_matrix(cam.world_position), &m)));
    };
    place([0.0, -0.25, 0.0], [60.0, 0.5, 24.0], [0.13, 0.14, 0.17]);
    // The fighter: 1.8 tall, 0.5 wide, standing on the floor at the origin.
    place([0.0, 0.9, 0.0], [0.5, 1.8, 0.5], [0.42, 0.34, 0.30]);
    // A metre stick standing at the origin, alternating bands, so vertical
    // extent can be COUNTED rather than estimated.
    for m in 0..14 {
        let shade = if m % 2 == 0 { 0.62 } else { 0.24 };
        place([1.2, m as f64 + 0.5, 0.0], [0.12, 1.0, 0.12], [shade, shade, shade * 1.1]);
    }
    // ...and one lying along the floor, banded the same way, for horizontal
    // extent. Between them, "is this effect four units wide or nine" stops being
    // a question you answer by squinting — which matters because a splash ring
    // is a PROMISE about a hitbox and has to be drawn at the width of one.
    for m in -14..15 {
        let shade = if m % 2 == 0 { 0.62 } else { 0.24 };
        place([m as f64 + 0.5, 0.02, -1.4], [1.0, 0.06, 0.12], [shade, shade, shade * 1.1]);
    }

    let mut sheets: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    for path in &args.effects {
        let doc = match floptle_scene::load_vfx_effect(path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("ERR {}: {e}", path.display());
                continue;
            }
        };
        let effect = vfx::effect_from_doc(&doc);
        let registry = load_textures(&gpu, &mut raster, &args.root, &effect);
        let compiled = Arc::new(effect.compile());
        let emitter = Transform::from_translation(args.at);

        let mut frames = Vec::new();
        for &t in &args.times {
            // Deterministic scrub: a fresh instance simulated from zero each
            // time, exactly as vfx_probe does, so two runs of this tool over an
            // unchanged file produce identical images and a diff means a change.
            let mut inst = EffectInstance::new(Arc::clone(&compiled), 1);
            inst.simulate_to(t, GRAVITY);
            let xf = emitter.render_matrix(cam.world_position);
            let fwd = cam.rotation * Vec3::NEG_Z;
            let (mut packed, mut draws) = (Vec::new(), Vec::new());
            collect_billboards(&inst, xf, xf, fwd, r, u, &mut packed, &mut draws);
            let batches: Vec<ParticleBatch> = draws
                .iter()
                .map(|d| ParticleBatch {
                    texture: d.texture.as_deref().and_then(|p| registry.get(p).copied()),
                    blend: d.blend,
                    range: d.range.clone(),
                })
                .collect();
            println!(
                "  {} t={t}: alive={} packed={}",
                doc.name,
                inst.alive(),
                packed.len()
            );
            raster.draw_scene(
                &gpu,
                &color_view,
                gpu.depth_view(),
                globals,
                &scene,
                Some([0.06, 0.07, 0.10, 1.0]),
                None,
            );
            particles.draw(&gpu, &color_view, gpu.depth_view(), pglobals, &packed, &batches, &raster);
            frames.push(read_pixels(&gpu, &color_tex));
        }
        sheets.push((doc.name.clone(), frames));
    }

    std::fs::create_dir_all(&args.out).ok();
    for (name, frames) in &sheets {
        let path = args.out.join(format!("{name}.png"));
        save_sheet(frames, &path);
        println!("wrote {}", path.display());
    }
}

/// One row of frames, side by side, as a single PNG.
fn save_sheet(frames: &[Vec<u8>], path: &Path) {
    let n = frames.len() as u32;
    let (tw, th) = (W * n, H);
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for (i, f) in frames.iter().enumerate() {
        for y in 0..H {
            let src = (y * W * 4) as usize;
            let dst = ((y * tw + i as u32 * W) * 4) as usize;
            out[dst..dst + (W * 4) as usize].copy_from_slice(&f[src..src + (W * 4) as usize]);
        }
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), tw, th);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&out).unwrap();
}

fn read_pixels(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
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
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    pixels
}
