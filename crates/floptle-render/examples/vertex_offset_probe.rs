//! Probe for `.flsl` `output offset`: two spheres wearing the same striped
//! surface, the left one heaving under a noise offset, the right one still.
//!
//! What to look for:
//!
//! * The left sphere's outline is lumpy and the right one's is round: the
//!   vertices moved.
//! * Its stripes follow the lumps rather than sliding across them: the
//!   fragment stage's `objectPos` is the position before the move.
//! * No holes or speckle where it dents inward: the depth prepass drew the
//!   moved surface. A prepass drawing the unmoved sphere would depth-reject
//!   every dent.
//!
//! The editor's frame order: depth prepass copied into the main depth, then
//! the colour pass loading it under LessEqual.
//!
//! Run: cargo run -p floptle-render --example vertex_offset_probe -- <out.png>

use floptle_core::transform::Transform;
use floptle_render::probe::{readback, save_png};
use floptle_render::{
    instance_of_mat, pass_prelude, uv_sphere, FlslBlend, Globals, Gpu, MaterialParams, Projection, Raster,
    RenderCamera,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 540;

/// The surface both spheres wear; the moving one adds `output offset`.
fn flesh(offset: bool) -> String {
    let out = if offset { "  output offset = normal * fbm(objectPos * 1.5 + vec3(0, 0, time)) * 0.35\n" } else { "" };
    format!(
        r#"
shader flesh {{
  stage fragment

  let stripe = smoothstep(0.4, 0.6, fract(objectPos.y * 6))
  let base = mix(vec3(0.55, 0.12, 0.16), vec3(0.95, 0.75, 0.7), stripe)
  output color = vec4(litSurface(base), 1)
{out}}}
"#
    )
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "vertex_offset.png".into());
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
    let sphere = raster.register(&gpu, &uv_sphere(1.0, 96, 128), None);

    let mut bind_for = |offset: bool| {
        let compiled = floptle_shader::compile_fragment(&flesh(offset)).expect("compiles");
        assert_eq!(compiled.displaced, offset, "the transpiler disagrees about `output offset`");
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}", e.message));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        raster.set_flsl_binding(&gpu, None, id, &params, &[])
    };
    let moving = bind_for(true);
    let still = bind_for(false);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.4, 6.5),
        Quat::from_rotation_x(-0.06),
        Projection::Perspective { fov_y: 45f32.to_radians(), near: 0.05, far: 100.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.5, 0.8, 0.6).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.97, 0.92, 0.0],
        ambient: [0.28, 0.26, 0.3, 0.0],
        time: [1.7, 0.0, 0.0, 0.0],
        ..Default::default()
    };
    let mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    let at = |x: f64| Transform::from_translation(DVec3::new(x, 0.0, 0.0)).render_matrix(cam.world_position);
    let flsl: Vec<floptle_render::FlslDraw> = vec![
        (sphere, None, moving, instance_of_mat(at(-1.35), &mp)),
        (sphere, None, still, instance_of_mat(at(1.35), &mp)),
    ];

    raster.depth_prepass_with(&gpu, globals, &[], &flsl, &[], gpu.depth_texture());
    clear_color(&gpu, &color_view, [0.03, 0.03, 0.05, 1.0]);
    raster.draw_scene_with(&gpu, &color_view, gpu.depth_view(), globals, &[], &flsl, &[], None, None);
    save_png(&gpu, &color_tex, &out);

    // Self-checks a software rasteriser answers the same way: coverage of
    // each half, against the background.
    let px = readback(&gpu, &color_tex);
    let bg = px[0];
    let covered = |x0: u32, x1: u32| {
        (0..H)
            .flat_map(|y| (x0..x1).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                let p = px[(y * W + x) as usize];
                (0..3).map(|c| (p[c] as i32 - bg[c] as i32).abs()).max().unwrap_or(0) > 12
            })
            .count() as f32
    };
    let (moving_px, still_px) = (covered(0, W / 2), covered(W / 2, W));
    println!("covered: moving {moving_px} px, still {still_px} px");
    // The offset is centred on zero, so the moved sphere bulges out about as
    // far as it dents in and covers about as much (here ~1.1×). A prepass
    // that drew it unmoved depth-rejects every dent, which drops it to ~0.77×.
    assert!(moving_px > 0.95 * still_px, "the moving sphere is mostly missing: the depth prepass drew it unmoved");
    // An offset that never applied leaves two identical spheres.
    assert!((moving_px - still_px).abs() > 0.03 * still_px, "both spheres cover the same: nothing moved");
    println!("wrote {out} — left lumpy with stuck stripes and no holes, right round");
}

fn clear_color(gpu: &Gpu, view: &wgpu::TextureView, c: [f64; 4]) {
    let mut encoder = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("clear") });
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
