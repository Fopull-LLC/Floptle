//! **Render a Sky `.flsl` to a picture**, headlessly — the sky half of
//! `model_shot`.
//!
//! A procedural sky is the hardest thing in a package to describe in words and
//! the easiest to show, and one driven by a knob is really several skies. So
//! this can sweep a uniform: give it a name and a list of values and it writes
//! one frame per value, which is what turns "an ashfall sky with a burn
//! parameter" into a strip somebody can look at and understand instantly.
//!
//! It compiles through the production path — `compile_sky` → validate → splice —
//! so a shader that renders here is a shader the editor will accept, and one
//! that fails here fails with the same message.
//!
//! Run:
//!   cargo run --release -p floptle-render --example sky_shot -- \
//!       --out <dir> [--size 960x540] [--yaw 0] [--pitch 6] \
//!       [--set name=value] [--sweep name=0,0.25,0.5,1] <shader.flsl>

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera};
use glam::{DVec3, Quat, Vec3};
use std::path::{Path, PathBuf};

fn main() {
    let mut out = PathBuf::from(".");
    let (mut w, mut h) = (960u32, 540u32);
    let (mut yaw, mut pitch) = (0.0f32, 6.0f32);
    let mut sets: Vec<(String, f32)> = Vec::new();
    let mut sweep: Option<(String, Vec<f32>)> = None;
    let mut shader: Option<PathBuf> = None;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(it.next().expect("--out wants a directory")),
            "--size" => {
                let s = it.next().expect("--size wants WxH");
                let (a, b) = s.split_once('x').expect("--size looks like 960x540");
                w = a.parse().expect("width");
                h = b.parse().expect("height");
            }
            "--yaw" => yaw = it.next().and_then(|s| s.parse().ok()).expect("--yaw wants degrees"),
            "--pitch" => {
                pitch = it.next().and_then(|s| s.parse().ok()).expect("--pitch wants degrees")
            }
            "--set" => {
                let s = it.next().expect("--set wants name=value");
                let (n, v) = s.split_once('=').expect("--set looks like name=0.5");
                sets.push((n.to_string(), v.parse().expect("a number")));
            }
            "--sweep" => {
                let s = it.next().expect("--sweep wants name=a,b,c");
                let (n, v) = s.split_once('=').expect("--sweep looks like burn=0,0.5,1");
                sweep = Some((
                    n.to_string(),
                    v.split(',').map(|c| c.trim().parse().expect("a number")).collect(),
                ));
            }
            _ => shader = Some(PathBuf::from(a)),
        }
    }
    let shader = shader.expect("pass a .flsl path");
    std::fs::create_dir_all(&out).expect("create --out");

    let src = std::fs::read_to_string(&shader).expect("read the shader");
    let sky = match floptle_shader::compile_sky(&src) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{}: {e}", shader.display());
            std::process::exit(1);
        }
    };
    println!("compiled `{}` — {} uniform(s)", sky.name, sky.uniforms.len());

    let gpu = Gpu::headless(w, h);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky-shot"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raymarch = Raymarch::new(&gpu);
    let spliced = Raymarch::preview_sky_source(&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL);
    floptle_shader::validate_module(&spliced).expect("the spliced module is valid WGSL");
    raymarch.set_sky_shader(&gpu, Some((&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)));

    // Start from the shader's own declared defaults — the same thing the editor
    // shows before anybody drags a slider — and override from there.
    let mut base = [[0.0f32; 4]; 16];
    for (i, u) in sky.uniforms.iter().enumerate().take(16) {
        base[i] = u.default;
    }
    let index_of = |name: &str| {
        sky.uniforms.iter().position(|u| u.name == name).unwrap_or_else(|| {
            let known: Vec<&str> = sky.uniforms.iter().map(|u| u.name.as_str()).collect();
            eprintln!("`{name}` is not a uniform of this shader. It has: {}", known.join(", "));
            std::process::exit(2);
        })
    };
    for (n, v) in &sets {
        base[index_of(n)][0] = *v;
    }

    let (y, p) = (yaw.to_radians(), pitch.to_radians());
    let fwd = Vec3::new(y.sin() * p.cos(), p.sin(), -y.cos() * p.cos()).normalize();
    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.0, 0.0),
        Quat::from_rotation_arc(Vec3::NEG_Z, fwd),
        Projection::Perspective { fov_y: 70f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(w as f32 / h as f32);

    let stem = shader.file_stem().map_or("sky".into(), |s| s.to_string_lossy().to_string());
    let frames: Vec<(String, [[f32; 4]; 16])> = match &sweep {
        Some((name, values)) => {
            let i = index_of(name);
            values
                .iter()
                .map(|v| {
                    let mut u = base;
                    u[i][0] = *v;
                    // `0.25` in a filename, not `0_25` — a person reading the
                    // directory should see the value they asked for.
                    (format!("{stem}_{name}{v}"), u)
                })
                .collect()
        }
        None => vec![(stem.clone(), base)],
    };

    for (name, uniforms) in frames {
        raymarch.draw_into(
            &gpu,
            &color_view,
            gpu.depth_view(),
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                // Deliberately grey: if a frame comes out this colour the sky
                // shader did not drive it, and that is worth seeing rather than
                // blending into a plausible black.
                bg: [0.5, 0.5, 0.5, 1.0],
                sky_meta: [1.0, 0.0, 0.0, 0.0],
                sky_uniforms: uniforms,
                ..Default::default()
            },
        );
        let path = out.join(format!("{name}.png"));
        save_png(&readback(&gpu, &color_tex, w, h), w, h, &path);
        println!("wrote {}", path.display());
    }
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32) -> Vec<[u8; 4]> {
    let padded =
        (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * h) as u64,
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
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut px = Vec::with_capacity((w * h) as usize);
    for y in 0..h {
        let row = (y * padded) as usize;
        for x in 0..w {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            px.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    px
}

fn save_png(px: &[[u8; 4]], w: u32, h: u32, path: &Path) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).expect("write png");
}
