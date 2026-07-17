//! Skybox-shader probe: a Sky-stage `.flsl` compiled and spliced into the raymarch's
//! `sky_color`, rendering a procedural sky. Proves the whole path — parse → check →
//! transpile_sky → splice → render — end to end.
//!
//! The shader is a vertical gradient (horizon → zenith) plus a warm sun disc, driven by
//! `skyDir`. The probe asserts the top of the frame is BLUER than the bottom (the gradient)
//! and that a bright warm spot exists (the sun) — direction-driven, so a broken splice
//! (flat bg) fails both.
//!
//! Run: cargo run -p floptle-render --example sky_shader_probe -- <out.png>

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 640;
const H: u32 = 400;

// A procedural sky: dark-blue zenith → pale horizon, plus a soft warm sun toward +x/up.
const SKY_FLSL: &str = r#"shader proceduralSky {
  stage sky
  uniform horizon: color = #b0c8e0
  uniform zenith: color = #20304f
  uniform sunColor: color = #fff0c0

  let up = clamp(skyDir.y, 0.0, 1.0)
  let grad = mix(horizon.rgb, zenith.rgb, up)
  let sun = pow(clamp(dot(skyDir, normalize(vec3(0.6, 0.5, 0.2))), 0.0, 1.0), 200.0)
  output color = grad + sunColor.rgb * sun
}
"#;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sky_shader.png".into());
    let gpu = Gpu::headless(W, H);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // Compile the Sky shader through the production path.
    let sky = floptle_shader::compile_sky(SKY_FLSL).expect("sky shader compiles");
    println!("compiled sky shader `{}` with {} uniforms", sky.name, sky.uniforms.len());

    let mut raymarch = Raymarch::new(&gpu);
    // Validate + splice.
    let src = Raymarch::preview_sky_source(&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL);
    floptle_shader::validate_module(&src).expect("spliced sky module is valid WGSL");
    raymarch.set_sky_shader(&gpu, Some((&sky.sky_fn, floptle_shader::stdlib::SUPPORT_WGSL)));

    // Pack the shader's uniform defaults into sky_uniforms (what the editor does).
    let mut sky_uniforms = [[0.0f32; 4]; 16];
    for (i, u) in sky.uniforms.iter().enumerate().take(16) {
        sky_uniforms[i] = u.default;
    }

    let cam_pos = DVec3::new(0.0, 1.0, 0.0);
    let target = Vec3::new(0.6, 0.5, 0.2); // look toward the sun
    let fwd = (target - Vec3::ZERO).normalize();
    let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
    let cam = RenderCamera::new(
        cam_pos,
        rot,
        Projection::Perspective { fov_y: 70f32.to_radians(), near: 0.05, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);

    let rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        bg: [0.5, 0.5, 0.5, 1.0], // must be OVERRIDDEN by the sky shader
        sky_meta: [1.0, 0.0, 0.0, 0.0],
        sky_uniforms,
        ..Default::default()
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);

    let px = readback(&gpu, &color_tex);
    save_png(&px, &out);
    let lum = |p: [u8; 4]| p[0] as f32 * 0.3 + p[1] as f32 * 0.59 + p[2] as f32 * 0.11;
    let sample = |fx: f32, fy: f32| px[((fy * H as f32) as u32 * W + (fx * W as f32) as u32) as usize];
    // Gradient: the top band should be BLUER (blue >> the bottom's blue is less than top's).
    let top = sample(0.5, 0.08);
    let bot = sample(0.5, 0.92);
    // Sun: the brightest pixel should be markedly brighter than the plain gradient.
    let mut bright = 0.0f32;
    for p in &px {
        bright = bright.max(lum(*p));
    }
    println!("top {top:?} bot {bot:?}  brightest lum {bright:.0}");
    assert!(
        top[2] as i32 - top[0] as i32 > bot[2] as i32 - bot[0] as i32,
        "the sky gradient is missing — top {top:?} is not bluer than bottom {bot:?} \
         (the sky shader didn't drive the color)"
    );
    assert!(bright > 220.0, "the sun disc is missing (brightest {bright:.0}) — sky shader not applied");

    // Knob override: re-render with `horizon` forced to a strong RED (what the editor
    // does when the Skybox Inspector overrides a uniform via `shader_params`). The
    // horizon band must actually turn red — proof that per-uniform overrides reach the
    // shader, i.e. the built-in skies are tweakable templates, not fixed pictures.
    let horizon_idx = sky.uniforms.iter().position(|u| u.name == "horizon").expect("has horizon knob");
    let mut overridden = sky_uniforms;
    overridden[horizon_idx] = [1.0, 0.0, 0.0, 1.0]; // red
    let rm2 = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        bg: [0.5, 0.5, 0.5, 1.0],
        sky_meta: [1.0, 0.0, 0.0, 0.0],
        sky_uniforms: overridden,
        ..Default::default()
    };
    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm2);
    let px2 = readback(&gpu, &color_tex);
    // Bottom band is closest to the horizon color (up ≈ 0 → grad ≈ horizon).
    let horizon_px = px2[((0.92 * H as f32) as u32 * W + (W / 2)) as usize];
    println!("overridden horizon band {horizon_px:?}");
    assert!(
        horizon_px[0] as i32 > horizon_px[2] as i32 + 40 && horizon_px[0] > 120,
        "the horizon override didn't take — band {horizon_px:?} should be reddish \
         (per-uniform sky overrides aren't reaching the shader)"
    );
    println!("sky shader OK (defaults + knob override); wrote {out}");
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
