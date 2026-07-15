//! Headless Field Shape probe (ADR-0007 Sdf stage) — compiles two sdf-stage
//! `.flsl` shaders through the production path (check_sdf → transpile_sdf per
//! slot → naga on BOTH assembled pass modules → set_custom_field), then draws
//! them raymarched beside/above a raster ground slab that must RECEIVE their
//! sun shadows through the shared field module.
//!
//! Run: cargo run -p floptle-render --example field_shape_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, raster_custom_source, Globals, Gpu, InstanceRaw, MaterialParams, MeshId,
    Projection, Raster, Raymarch, RaymarchGlobals, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 540;

const WOBBLE: &str = r#"
// A twisted union of a sphere and a box, banded by noise.
shader wobble {
  stage sdf
  uniform twistAmt: float = 0.8
  uniform fuse: float = 0.35

  let p = twist(worldPos, twistAmt)
  let d = smoothMin(sphere(p, radius: 0.85), box(p, vec3(0.6, 0.9, 0.6), rounding: 0.1), k: fuse)

  output sdf = d
  output color = vec3(0.9, 0.55, 0.25) + 0.25 * noise(worldPos * 3)
}
"#;

const RINGS: &str = r#"
shader rings {
  stage sdf
  uniform minor: float = 0.16

  let d = torus(repeat(worldPos, vec3(0, 0.9, 0)), major: 0.7, minor: minor)

  output sdf = opIntersect(d, sphere(worldPos, radius: 1.4))
  output color = vec3(0.35, 0.7, 0.9)
}
"#;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "field_shapes.png".into());
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
    let mut raymarch = Raymarch::new(&gpu);
    let cube_id = raster.register(&gpu, &floptle_render::cube(1.0), None);

    // ---- compile + splice exactly like the editor does ----
    let mut dist_fns = String::new();
    let mut col_fns = String::new();
    for (slot, src) in [WOBBLE, RINGS].iter().enumerate() {
        let (ir, ck) = floptle_shader::check_sdf(src).expect("checks");
        let c = floptle_shader::transpile_sdf(&ir, &ck, slot).expect("transpiles");
        dist_fns.push_str(&c.dist_fn);
        col_fns.push_str(&c.col_fn);
    }
    let mut field_code = dist_fns;
    field_code.push_str(
        "fn custom_d(p: vec3<f32>) -> f32 {\n    return min(flsl_shape0_d(p), flsl_shape1_d(p));\n}\n",
    );
    let mut color_code = col_fns;
    color_code.push_str(
        "fn custom_col(p: vec3<f32>) -> Matter {\n    let d0 = flsl_shape0_d(p);\n    let d1 = flsl_shape1_d(p);\n    if (d0 < d1) { return Matter(d0, flsl_shape0_col(p)); }\n    return Matter(d1, flsl_shape1_col(p));\n}\nfn nearest_shape(p: vec3<f32>) -> i32 {\n    if (flsl_shape0_d(p) < flsl_shape1_d(p)) { return 0; }\n    return 1;\n}\n",
    );
    let support = floptle_shader::stdlib::SUPPORT_WGSL;
    floptle_shader::validate_module(&Raymarch::preview_custom_source(Some((
        &field_code,
        &color_code,
        support,
    ))))
    .unwrap_or_else(|e| panic!("raymarch module rejected: {} (line {:?})", e.message, e.chunk_line));
    floptle_shader::validate_module(&raster_custom_source(Some((&field_code, support))))
        .unwrap_or_else(|e| panic!("raster module rejected: {}", e.message));
    raymarch.set_custom_field(&gpu, Some((&field_code, &color_code, support)));
    raster.set_custom_field(&gpu, Some((&field_code, support)));

    // ---- scene: two shapes over a ground slab, sun shadows ON ----
    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.6, 6.5),
        Quat::from_rotation_x(-0.18),
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.35, 0.85, 0.4).normalize();

    let mut rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.22, 0.28, 0.0],
        bg: [0.08, 0.09, 0.13, 1.0],
        shadow_params: [1.0, 12.0, 0.85, 150.0],
        ..Default::default()
    };
    rm.shape_meta = [2.0, 0.0, 0.0, 0.0];
    let cam_rel = |p: DVec3| (p - cam.world_position).as_vec3();
    let s0 = cam_rel(DVec3::new(-1.6, 0.6, 0.0));
    let s1 = cam_rel(DVec3::new(1.8, 0.7, 0.0));
    rm.shape_pos[0] = [s0.x, s0.y, s0.z, 1.0];
    rm.shape_aux[0] = [1.6, 0.0, 0.0, 0.0];
    let q = Quat::from_rotation_z(0.4).inverse();
    rm.shape_rot[1] = [q.x, q.y, q.z, q.w];
    rm.shape_pos[1] = [s1.x, s1.y, s1.z, 1.0];
    rm.shape_aux[1] = [1.6, 0.0, 0.0, 0.0];
    // Shape 0's exposed knobs: twistAmt = 1.6 (override), fuse default.
    rm.shape_uniforms[0] = [1.6, 0.0, 0.0, 0.0];
    // Shape 1 material: shiny.
    rm.shape_specular[1] = [1.0, 1.0, 1.0, 0.9];
    rm.shape_params[1] = [48.0, 0.2, 0.0, 1.0];
    rm.shape_rim[1] = [0.3, 0.8, 1.0, 0.0];

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.22, 0.22, 0.28, 0.0],
        ..Default::default()
    };
    // The ground: a wide flat cube the shapes must cast onto.
    let mut ground = Transform::from_translation(DVec3::new(0.0, -1.6, 0.0));
    ground.scale = Vec3::new(8.0, 0.1, 6.0);
    let mp = MaterialParams::flat([0.75, 0.75, 0.8]);
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        vec![(cube_id, None, instance_of_mat(ground.render_matrix(cam.world_position), &mp))];

    raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    raster.draw_scene(
        &gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        &instances,
        None,
        Some(raymarch.field_bind()),
    );

    save_png(&gpu, &color_tex, &out);
    println!("wrote {out} — twisted wobble + ring stack, raymarched, shadowing the raster ground");
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
