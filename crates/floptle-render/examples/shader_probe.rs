//! Headless `.flsl` shader probe — compiles four custom shaders through the
//! FULL production path (parse → check → transpile → naga against the real
//! pass sources → `register_flsl_shader` → group(3) bindings →
//! `draw_scene_with`) and renders them beside a built-in material to a PNG.
//! Proves ADR-0007 Phase 2 end-to-end without a window.
//!
//! Run: cargo run -p floptle-render --example shader_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, pass_prelude, uv_sphere, FlslBlend, Globals, Gpu, InstanceRaw,
    MaterialParams, MeshId, Projection, Raster, RenderCamera, TexId, TextureData,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1440;
const H: u32 = 400;

const PLASMA: &str = r#"
shader plasma {
  stage fragment
  uniform speed: float = 0.35
  uniform steps: float = 6

  let warped = domainWarp(uv * 4, scale: 2.0, time: time)
  let n = fbm(warped, octaves: 5)
  let hue = hueShift(palette(n, "sunset"), time * speed)

  output color = vec4(posterize(hue, steps: steps), 1)
}
"#;

const LIT_TEXTURED: &str = r#"
// The learn-by-reading example: the built-in look, spelled in flsl.
shader litTextured {
  stage fragment
  uniform tint: color = #FFFFFF

  let base = baseTexture() * tint * instanceColor

  output color = vec4(litSurface(base.rgb), base.a)
}
"#;

const RIM_GLOW: &str = r#"
shader rimGlow {
  stage fragment
  blend additive
  uniform glow: color = #40C8FF
  uniform power: float = 3

  let rim = pow(1 - max(dot(normal, viewDir), 0), power)

  output color = vec4(glow.rgb * rim, 1)
}
"#;

const VEIL: &str = r#"
shader veil {
  stage fragment
  blend alpha
  uniform body: color = #B040FF

  let bands = 0.5 + 0.5 * sin(worldPos.y * 18 + time * 2)

  output color = vec4(body.rgb, 0.25 + 0.5 * bands)
}
"#;

fn checker() -> TextureData {
    let n = 64u32;
    let mut px = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let on = ((x / 8) + (y / 8)) % 2 == 0;
            let c = if on { [220, 160, 70, 255] } else { [120, 80, 40, 255] };
            px.extend_from_slice(&c);
        }
    }
    TextureData { pixels: px, width: n, height: n }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "shaders.png".into());
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

    let mut raster = Raster::new(&gpu);
    let sphere = raster.register(&gpu, &uv_sphere(0.9, 32, 48), None);
    let tex = raster.register_texture(&gpu, &checker(), Default::default());

    // Compile each shader exactly like the editor does.
    let mut compile = |src: &str| {
        let compiled = floptle_shader::compile_fragment(src).expect("compiles");
        floptle_shader::validate(pass_prelude(), &compiled.chunk)
            .unwrap_or_else(|e| panic!("naga: {} (chunk line {:?})", e.message, e.chunk_line));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let blend = match compiled.blend {
            floptle_shader::Blend::Opaque => FlslBlend::Opaque,
            floptle_shader::Blend::Alpha => FlslBlend::Alpha,
            floptle_shader::Blend::Additive => FlslBlend::Additive,
        };
        let id = raster.register_flsl_shader(&gpu, &chunk, compiled.textures.len(), blend, None);
        (compiled, id)
    };
    let (plasma_c, plasma) = compile(PLASMA);
    let (littex_c, littex) = compile(LIT_TEXTURED);
    let (rim_c, rim) = compile(RIM_GLOW);
    let (veil_c, veil) = compile(VEIL);

    // One binding per material, defaults except where overridden.
    let mut bind = |compiled: &floptle_shader::CompiledFragment,
                    id,
                    overrides: &[(&str, [f32; 4])],
                    textures: &[Option<TexId>]| {
        let params = compiled.pack_params(&|name| {
            overrides.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
        });
        raster.set_flsl_binding(&gpu, None, id, &params, textures)
    };
    let b_plasma = bind(&plasma_c, plasma, &[], &[]);
    let b_littex = bind(&littex_c, littex, &[], &[]);
    let b_rim = bind(&rim_c, rim, &[("power", [2.5, 0.0, 0.0, 0.0])], &[]);
    let b_veil = bind(&veil_c, veil, &[], &[]);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 7.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.12, 0.12, 0.16, 0.0],
        ..Default::default()
    };

    let at = |i: usize| {
        let x = -5.5 + i as f64 * 2.2;
        Transform::from_translation(DVec3::new(x, 0.0, 0.0)).render_matrix(cam.world_position)
    };
    let mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    // Slot 0: a BUILT-IN material sphere for side-by-side comparison.
    let mut shiny = MaterialParams::flat([0.75, 0.18, 0.18]);
    shiny.specular_strength = 1.0;
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        vec![(sphere, None, instance_of_mat(at(0), &shiny))];
    // The rim-glow sphere overlaps the veil one so additive-over-alpha shows.
    let flsl: Vec<floptle_render::FlslDraw> = vec![
        (sphere, None, b_plasma, instance_of_mat(at(1), &mp)),
        (sphere, Some(tex), b_littex, instance_of_mat(at(2), &mp)),
        (sphere, None, b_veil, instance_of_mat(at(3), &mp)),
        (sphere, None, b_rim, instance_of_mat(at(4), &mp)),
        (sphere, Some(tex), b_littex, instance_of_mat(at(5), &mp)),
    ];

    raster.draw_scene_with(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &instances,
        &flsl,
        Some([0.02, 0.02, 0.05, 1.0]),
        None,
    );
    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — built-in / plasma / litTextured / veil(alpha) / rimGlow(additive) / litTextured");
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
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
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
