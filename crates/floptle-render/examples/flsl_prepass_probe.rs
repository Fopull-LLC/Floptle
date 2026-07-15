//! Regression probe for the flsl × depth-prepass seam: a big floor with a
//! custom `.flsl` material, drawn EXACTLY like the editor frame — depth
//! prepass (which includes opaque flsl draws) copied over the main depth,
//! then the color pass loading that depth under LessEqual.
//!
//! If the flsl pipeline's vertex stage doesn't reproduce the prepass depth
//! bit-for-bit, the floor breaks into criss-crossing triangle shards (some
//! fragments lose the depth test). A clean image = the seam holds.
//!
//! Run: cargo run -p floptle-render --example flsl_prepass_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, pass_prelude, plane, FlslBlend, Globals, Gpu, InstanceRaw,
    MaterialParams, MeshId, Projection, Raster, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 1280;
const H: u32 = 720;

const CLOUDS: &str = r#"
shader clouds {
  stage fragment

  let n = fbm(domainWarp(uv * 6, scale: 2.0, time: time), octaves: 5)
  let glow = vec3(1.0, 0.85, 0.45) * smoothstep(0.35, 0.9, n) * 2.0
  let base = vec3(0.25, 0.3, 0.32) + glow

  output color = vec4(litSurface(base), 1)
}
"#;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "flsl_prepass.png".into());
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
    let box_mesh = raster.register(&gpu, &cube(0.5), None);
    // The user-reported habitat: a big flat PLANE primitive with the shader.
    let plane_mesh = raster.register(&gpu, &plane(0.7), None);

    let compiled = floptle_shader::compile_fragment(CLOUDS).expect("compiles");
    floptle_shader::validate(pass_prelude(), &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga: {}", e.message));
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    let id = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Opaque, None);
    let params = compiled.pack_params(&|_| None, &|_| None);
    let bind = raster.set_flsl_binding(&gpu, None, id, &params, &[]);

    // Grazing view down a big floor slab (the artifact's natural habitat) +
    // a couple of built-in boxes for comparison.
    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.6, 9.0),
        Quat::from_rotation_x(-0.18),
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 4000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.25, 0.25, 0.3, 0.0],
        ..Default::default()
    };

    // The plane primitive lies in XY facing ±Z — rotate it flat like a floor,
    // scaled big (mirrors the user's scene: a ~38×41 plane with the shader).
    let floor = Transform {
        translation: DVec3::new(0.0, 0.0, 0.0),
        rotation: Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        scale: glam::Vec3::new(40.0, 40.0, 1.0),
    };
    let mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    let mut red = MaterialParams::flat([0.75, 0.2, 0.2]);
    red.specular_strength = 0.6;
    let at = |x: f64, z: f64| {
        Transform::from_translation(DVec3::new(x, 0.5, z)).render_matrix(cam.world_position)
    };
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        (box_mesh, None, instance_of_mat(at(-2.0, 2.0), &red)),
        (box_mesh, None, instance_of_mat(at(2.0, -1.0), &red)),
    ];
    let flsl: Vec<floptle_render::FlslDraw> = vec![(
        plane_mesh,
        None,
        bind,
        instance_of_mat(floor.render_matrix(cam.world_position), &mp),
    )];

    // The editor's frame order: prepass (instances + opaque flsl) primes the
    // main depth, then the color pass LOADS it (raster_clear = None) — except
    // here nothing raymarches in between, isolating the mesh seam.
    raster.depth_prepass_with(&gpu, globals, &instances, &flsl, gpu.depth_texture());
    // draw_scene_with clears color itself when given a clear; pass one for
    // color but keep the primed depth by clearing... the editor passes None —
    // mirror that, and pre-clear color separately.
    clear_color(&gpu, &color_view, [0.02, 0.02, 0.05, 1.0]);
    raster.draw_scene_with(&gpu, &color_view, gpu.depth_view(), globals, &instances, &flsl, None, None);

    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — a clean glowing-clouds floor = no prepass seam");
}

fn clear_color(gpu: &Gpu, view: &wgpu::TextureView, c: [f64; 4]) {
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color { r: c[0], g: c[1], b: c[2], a: c[3] }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([encoder.finish()]);
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
