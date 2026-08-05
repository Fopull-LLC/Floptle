//! Sky-shader **contact sheet**: one uniform swept across its range, every step
//! rendered and tiled into a single PNG (`floptle/0119`).
//!
//! `sky_shader_probe` renders one frame with one set of values, which is the
//! right test for "does the splice work" and the wrong one for a sky whose
//! entire purpose is that it **progresses**. A sky that catches fire over four
//! minutes cannot be judged from a single frame, and the editor viewport shows
//! one value at a time — so tuning one meant rebuilding, dragging a slider,
//! squinting, and repeating.
//!
//! Point it at a shader and a uniform and you get the whole range at once:
//!
//! ```text
//! cargo run -p floptle-render --example sky_sweep_probe -- <outdir> [shader.flsl] [uniform] [steps]
//! ```
//!
//! With no shader it sweeps a built-in demo, so it runs with just an output
//! directory and can be gated in CI. Steps default to 8, laid out four across.
//! The swept range is the uniform's own `range(lo, hi)` when it declares one,
//! else 0..1.

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera};
use glam::{DVec3, Quat, Vec3};

/// One cell of the sheet.
const CW: u32 = 240;
const CH: u32 = 150;
const ACROSS: u32 = 4;

/// A sky that visibly PROGRESSES, so the sheet has something to show: a dark
/// void that catches fire from the horizon up as `burn` runs 0 → 1. Uses
/// `atan2` for the azimuth, which is the thing `floptle/0119` added and the
/// reason a shader can lay anything out around a horizon at all.
const DEMO_FLSL: &str = r#"shader ashfallDemo {
  stage sky
  uniform burn: float = 0.0 range(0, 1)
  uniform emberColor: color = #ff6a1e
  uniform voidColor: color = #0b0a16

  let az = atan2(skyDir.z, skyDir.x) / 6.2831853 + 0.5
  let up = clamp(skyDir.y, 0.0, 1.0)
  // A ragged fire line that climbs with `burn`, broken up around the horizon.
  let ridge = valueNoise(vec2(mod(az * 8, 8), 0.5)) * 0.15
  let line = burn * 1.2 - up + ridge
  let fire = smoothstep(0.0, 0.25, line)
  let glow = pow(clamp(1 - up, 0.0, 1.0), 3.0) * burn
  output color = mix(voidColor.rgb, emberColor.rgb, fire) + emberColor.rgb * glow * 0.5
}
"#;

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| ".".into());
    let shader_path = args.next();
    let want_uniform = args.next();
    let steps: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(8);
    let steps = steps.clamp(2, 32);

    let src = match &shader_path {
        Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| panic!("read {p}: {e}")),
        None => DEMO_FLSL.to_string(),
    };
    let sky = floptle_shader::compile_sky(&src).expect("sky shader compiles");

    // Which uniform to sweep: the one asked for, else the first scalar with a
    // declared range, else the first scalar. A colour is not swept — there is no
    // meaningful one-dimensional path through one.
    let idx = match &want_uniform {
        Some(name) => sky
            .uniforms
            .iter()
            .position(|u| u.name == *name)
            .unwrap_or_else(|| panic!("`{name}` is not a uniform of `{}`", sky.name)),
        None => sky
            .uniforms
            .iter()
            .position(|u| !u.is_color && u.range.is_some())
            .or_else(|| sky.uniforms.iter().position(|u| !u.is_color))
            .expect("the shader has no scalar uniform to sweep"),
    };
    let u = &sky.uniforms[idx];
    let (lo, hi) = u.range.unwrap_or((0.0, 1.0));
    println!("sweeping `{}` of `{}` across {lo}..{hi} in {steps} steps", u.name, sky.name);

    let down = steps.div_ceil(ACROSS);
    let (sw, sh) = (CW * ACROSS.min(steps), CH * down);

    let gpu = Gpu::headless(CW, CH);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky-sweep-cell"),
        size: wgpu::Extent3d { width: CW, height: CH, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raymarch = Raymarch::new(&gpu);
    let module = Raymarch::preview_sky_source(&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL);
    floptle_shader::validate_module(&module).expect("spliced sky module is valid WGSL");
    raymarch.set_sky_shader(&gpu, Some((&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)));

    let mut defaults = [[0.0f32; 4]; 16];
    for (i, u) in sky.uniforms.iter().enumerate().take(16) {
        defaults[i] = u.default;
    }

    // Level, looking at the horizon: the band a sky shader does its work in.
    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.0, 0.0),
        Quat::from_rotation_arc(Vec3::NEG_Z, Vec3::new(0.0, 0.15, -1.0).normalize()),
        Projection::Perspective { fov_y: 70f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(CW as f32 / CH as f32);

    let mut sheet = vec![[0u8, 0, 0, 255]; (sw * sh) as usize];
    let mut means: Vec<f32> = Vec::with_capacity(steps as usize);
    for s in 0..steps {
        let t = s as f32 / (steps - 1) as f32;
        let v = lo + (hi - lo) * t;
        let mut uniforms = defaults;
        // Lane 0 for a scalar — the same packing the editor's knobs use.
        uniforms[idx][0] = v;
        raymarch.draw_into(
            &gpu,
            &color_view,
            gpu.depth_view(),
            RaymarchGlobals {
                view_proj: view_proj.to_cols_array_2d(),
                inv_view_proj: view_proj.inverse().to_cols_array_2d(),
                // Deliberately grey: if a cell comes back grey the sky shader
                // did not drive it, and the sheet says so at a glance.
                bg: [0.5, 0.5, 0.5, 1.0],
                sky_meta: [1.0, 0.0, 0.0, 0.0],
                sky_uniforms: uniforms,
                ..Default::default()
            },
        );
        let cell = readback(&gpu, &color_tex);
        means.push(cell.iter().map(|p| luma(*p)).sum::<f32>() / cell.len() as f32);
        blit(&mut sheet, sw, &cell, (s % ACROSS) * CW, (s / ACROSS) * CH);
        // A tick along the top edge, so a cell can be told from its neighbours
        // without counting: it grows left to right with the swept value.
        let ticks = ((t * (CW - 8) as f32) as u32).max(2);
        for x in 0..ticks {
            for y in 0..3 {
                let px = (s % ACROSS) * CW + 4 + x;
                let py = (s / ACROSS) * CH + 3 + y;
                sheet[(py * sw + px) as usize] = [255, 255, 255, 255];
            }
        }
        println!("  {} = {v:.3} → mean luma {:.3}", u.name, means[s as usize]);
    }

    let out = format!("{dir}/sky_sweep_{}_{}.png", sky.name, u.name);
    save_png(&sheet, sw, sh, &out);
    println!("wrote {out} ({sw}x{sh}, {steps} steps)");

    // The sheet is for looking at; these two assertions are what a machine can
    // honestly claim about an arbitrary shader it was handed.
    assert!(
        means.iter().any(|m| (m - means[0]).abs() > 0.01),
        "sweeping `{}` changed nothing across {lo}..{hi} — either the uniform is \
         unused or its value is not reaching the shader",
        u.name
    );
    assert!(
        means.iter().all(|m| (m - 0.5).abs() > 0.002 || *m == means[0]),
        "a cell came back at the clear colour, so the sky shader did not draw it"
    );
    println!("sky sweep OK");
}

fn luma(p: [u8; 4]) -> f32 {
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn blit(sheet: &mut [[u8; 4]], sheet_w: u32, cell: &[[u8; 4]], ox: u32, oy: u32) {
    for y in 0..CH {
        for x in 0..CW {
            sheet[((oy + y) * sheet_w + ox + x) as usize] = cell[(y * CW + x) as usize];
        }
    }
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (CW * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * CH) as u64,
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
                rows_per_image: Some(CH),
            },
        },
        wgpu::Extent3d { width: CW, height: CH, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    // The swapchain format is the BACKEND'S choice, not ours — it is RGBA on
    // some and BGRA on others. A probe that assumes one gets a picture with red
    // and blue swapped, which does not read as "wrong channel order", it reads
    // as "this shader's palette is inverted". Copy this check, do not drop it.
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((CW * CH) as usize);
    for y in 0..CH {
        let row = (y * padded) as usize;
        for x in 0..CW {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], w: u32, h: u32, path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
