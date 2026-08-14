//! **Render a model to a picture**, headlessly, so a pack of assets can show
//! itself.
//!
//! A package listing is a thumbnail and a gallery, and until now the only way to
//! get either was to open the editor, frame the thing by hand and screenshot it
//! — per model, every time the model changed. For anybody assembling a kit of
//! twenty pieces that is the whole afternoon, and it is the reason art packages
//! ship with no pictures.
//!
//! Every model is framed the same way: the camera is placed from the model's own
//! bounds, so a sword and a building come out the same size in frame and a
//! contact sheet of them reads as a set rather than as a pile.
//!
//! Run:
//!   cargo run --release -p floptle-render --example model_shot -- \
//!       --out <dir> [--size 768] [--yaw 35] [--pitch 18] [--bg 0.09,0.10,0.12] \
//!       <model.glb> [...]
//!
//! Writes `<out>/<model stem>.png`, one per model.

use floptle_render::{
    instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Raster, TexId,
};
use glam::{Mat4, Vec3};
use std::path::{Path, PathBuf};

struct Args {
    out: PathBuf,
    size: u32,
    yaw: f32,
    pitch: f32,
    bg: [f64; 3],
    models: Vec<PathBuf>,
}

fn parse_args() -> Args {
    let mut a = Args {
        out: PathBuf::from("."),
        size: 768,
        // Three-quarters and slightly above: the angle a character or a prop is
        // recognisable from. Straight on flattens it, straight down is a map.
        yaw: 35.0,
        pitch: 18.0,
        bg: [0.09, 0.10, 0.12],
        models: Vec::new(),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--out" => a.out = PathBuf::from(it.next().expect("--out wants a directory")),
            "--size" => a.size = it.next().and_then(|s| s.parse().ok()).expect("--size wants px"),
            "--yaw" => a.yaw = it.next().and_then(|s| s.parse().ok()).expect("--yaw wants degrees"),
            "--pitch" => {
                a.pitch = it.next().and_then(|s| s.parse().ok()).expect("--pitch wants degrees")
            }
            "--bg" => {
                let s = it.next().expect("--bg wants r,g,b");
                let v: Vec<f64> = s.split(',').filter_map(|c| c.trim().parse().ok()).collect();
                a.bg = [v[0], v[1], v[2]];
            }
            _ => a.models.push(PathBuf::from(arg)),
        }
    }
    if a.models.is_empty() {
        eprintln!("nothing to render — pass one or more .glb/.gltf paths");
        std::process::exit(2);
    }
    a
}

fn main() {
    let args = parse_args();
    std::fs::create_dir_all(&args.out).expect("create --out");
    let gpu = Gpu::headless(args.size, args.size);
    let mut raster = Raster::new(&gpu);

    for path in &args.models {
        match shot(&gpu, &mut raster, &args, path) {
            Ok(out) => println!("wrote {}", out.display()),
            Err(e) => eprintln!("ERR {}: {e}", path.display()),
        }
    }
}

fn shot(gpu: &Gpu, raster: &mut Raster, args: &Args, path: &Path) -> Result<PathBuf, String> {
    let model = floptle_assets::import(path).map_err(|e| e.to_string())?;
    if model.parts.is_empty() {
        return Err("no geometry in it".into());
    }

    // Import recentres about the origin and reports the bounds it ended up with.
    // Framing off those rather than off a fixed distance is what makes a set of
    // models come out as a set.
    let extent = (model.max[0] - model.min[0])
        .max(model.max[1] - model.min[1])
        .max(model.max[2] - model.min[2])
        .max(1e-3);
    let fov = 0.6f32;
    // Far enough that the whole thing fits, with a little air around it. The
    // 0.82 is the margin — full-bleed reads as cropped in a grid cell, and too
    // much air reads as a small model.
    let dist = (extent * 0.5) / (fov * 0.5).tan() / 0.82;
    let (y, p) = (args.yaw.to_radians(), args.pitch.to_radians());
    let dir = Vec3::new(y.sin() * p.cos(), p.sin(), y.cos() * p.cos());
    let centre = Vec3::new(
        (model.min[0] + model.max[0]) * 0.5,
        (model.min[1] + model.max[1]) * 0.5,
        (model.min[2] + model.max[2]) * 0.5,
    );
    let eye = centre + dir * dist;
    let view = Mat4::look_at_rh(Vec3::ZERO, centre - eye, Vec3::Y);
    let proj = Mat4::perspective_rh(fov, 1.0, dist * 0.01, dist * 4.0);
    let view_proj = proj * view;

    let meshes: Vec<(MeshId, MaterialParams)> = model
        .parts
        .iter()
        .map(|p| {
            let id = raster.register(gpu, &p.mesh, p.texture.map(|i| &model.textures[i]));
            (id, MaterialParams::flat(p.base_color))
        })
        .collect();
    // Everything is drawn camera-relative, so the model matrix carries only the
    // eye offset — the same convention every other probe and the editor use.
    let to_eye = Mat4::from_translation(-eye);
    let scene: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        meshes.iter().map(|(id, m)| (*id, None, instance_of_mat(to_eye, m))).collect();

    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("model-shot"),
        size: wgpu::Extent3d { width: args.size, height: args.size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("model-shot-depth"),
        size: wgpu::Extent3d { width: args.size, height: args.size, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    // A key from over the camera's shoulder and a generous ambient. Product
    // lighting, deliberately: a dramatic rig makes a better picture and a worse
    // catalogue, because half the model is then a decision rather than a shape.
    let key = (dir + Vec3::new(-0.35, 0.55, 0.1)).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [key.x, key.y, key.z, 0.0],
        light_color: [1.0, 0.98, 0.94, 1.0],
        ambient: [0.42, 0.44, 0.50, 1.0],
        ..Default::default()
    };
    // `--bg` is the colour a person wants to SEE. A clear value is written
    // straight into the target, and the target is sRGB, so handing it the
    // number as typed makes every backdrop three shades paler than asked for —
    // which is exactly what the first sheet out of this tool looked like.
    let lin = |c: f64| if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
    let bg = if gpu.surface_format().is_srgb() {
        [lin(args.bg[0]), lin(args.bg[1]), lin(args.bg[2]), 1.0]
    } else {
        [args.bg[0], args.bg[1], args.bg[2], 1.0]
    };
    raster.draw_scene(gpu, &color_view, &depth_view, globals, &scene, Some(bg), None);

    // Printed because it is the first thing that explains a picture that looks
    // wrong: a model lying on its side in frame is usually a model authored
    // lying on its side, and the numbers say so where the render only hints.
    println!(
        "  {} — {:.2} x {:.2} x {:.2}, {} part(s)",
        model.name,
        model.max[0] - model.min[0],
        model.max[1] - model.min[1],
        model.max[2] - model.min[2],
        model.parts.len()
    );
    let stem = path.file_stem().map_or("model".into(), |s| s.to_string_lossy().to_string());
    let out = args.out.join(format!("{stem}.png"));
    write_png(gpu, &color, args.size, &out);
    Ok(out)
}

fn write_png(gpu: &Gpu, tex: &wgpu::Texture, s: u32, path: &Path) {
    let padded =
        (s * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * s) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
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
                rows_per_image: Some(s),
            },
        },
        wgpu::Extent3d { width: s, height: s, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut rgba = Vec::with_capacity((s * s * 4) as usize);
    for y in 0..s {
        let row = (y * padded) as usize;
        for x in 0..s {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            let p = if bgra { [p[2], p[1], p[0], p[3]] } else { p };
            rgba.extend_from_slice(&p);
        }
    }
    drop(view);
    buf.unmap();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), s, s);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgba).expect("write png");
}
