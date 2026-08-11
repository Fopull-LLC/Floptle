//! Does `surfaceGap()` see ordinary MESH geometry?
//!
//! This is the question behind shoreline foam, soft particles and contact glow,
//! and it is one you cannot answer by looking at a screenshot of water: foam
//! looks like foam whether it is measuring the scene or painting a fixed band
//! near the camera. So the probe renders the measurement ITSELF — a translucent
//! sheet whose colour IS `surfaceGap`, white where it touches something and
//! black where the scene is wide open — over a mesh box sunk through it.
//!
//! Three things get checked, and each of them is a way the feature has
//! previously been able to look fine while being useless:
//!
//! 1. **The gap varies across the sheet.** A constant reading means the depth
//!    texture was never populated, which is exactly what happens when the
//!    prepass does not run — the silent failure this probe exists for.
//! 2. **It is bright where the box is and dark where it is not.** A gradient
//!    alone proves nothing; it has to be the RIGHT gradient, keyed to geometry
//!    that only exists as a mesh (no terrain, no SDF field, nothing
//!    `fieldDistance` could have found).
//! 3. **With nothing behind the sheet at all, it reads wide open.** The "no
//!    prepass / sky / off screen" answer must be the one that makes
//!    `saturate(gap / width)` come out as open water, not as foam everywhere.
//!
//! Everything the camera can see here is the measuring sheet. That is the point:
//! an earlier cut of this probe let the box poke through the sheet, and every
//! assertion below passed on the box's own grey albedo while the sheet rendered
//! flat black — which is precisely what `surfaceGap` returns when the prepass it
//! reads was never bound. A probe that can see any surface but the one doing the
//! measuring is not measuring the thing it is named after.
//!
//! Run: cargo run -p floptle-render --example surface_gap_probe -- <out-dir>

use floptle_core::transform::Transform;
use floptle_render::{
    cube, instance_of_mat, pass_prelude, plane, FlslBlend, Globals, Gpu, InstanceRaw,
    MaterialParams, MeshId, Projection, Raster, RenderCamera, TexId,
};
use glam::{DVec3, Quat, Vec3};

const W: u32 = 256;
const H: u32 = 256;

/// The measurement, painted straight out as grey. `reach` is the window: 0 at
/// the contact point, 1 once the scene behind is `reach` metres away.
const GAP_VIEW: &str = r#"
shader gapView {
  stage fragment
  uniform reach: float = 2 range(0.1, 20)

  let g = saturate(surfaceGap(worldPos) / reach)
  output color = vec4(vec3(1 - g), 1)
}
"#;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(W, H);
    let mut raster = Raster::new(&gpu);
    let box_mesh = raster.register(&gpu, &cube(0.5), None);
    let sheet_mesh = raster.register(&gpu, &plane(0.7), None);

    let compiled = floptle_shader::compile_fragment(GAP_VIEW).expect("compiles");
    floptle_shader::validate(pass_prelude(), &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga: {}", e.message));
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    // BLENDED, not opaque — and that is a fact about the feature, not a detail
    // of the probe. The prepass records the opaque surfaces, so an opaque sheet
    // is IN it and every sample finds itself: the gap reads zero everywhere and
    // a shader would foam over its whole surface. Water, soft particles and
    // contact glow are all translucent for exactly this reason; a surface has to
    // be absent from the prepass to be able to measure it.
    let shader = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Alpha, None);
    let params = compiled.pack_params(&|_| None, &|_| None);
    let bind = raster.set_flsl_binding(&gpu, None, shader, &params, &[]);

    // Straight down at a horizontal sheet, with a box pushed up through its
    // middle. Looking down means the gap under the sheet is what varies, and
    // the box is the only thing that can vary it.
    let cam = RenderCamera::new(
        DVec3::new(0.0, 6.0, 0.0),
        Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.05, far: 400.0 },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let light = Vec3::new(0.4, 0.9, 0.45).normalize();
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.3, 0.3, 0.35, 0.0],
        ..Default::default()
    };

    let mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    // The sheet sits at y = 0, spanning far more than the view.
    let sheet = Transform {
        translation: DVec3::ZERO,
        rotation: Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        scale: Vec3::new(20.0, 20.0, 1.0),
    };
    let flsl: Vec<floptle_render::FlslDraw> = vec![(
        sheet_mesh,
        None,
        bind,
        instance_of_mat(sheet.render_matrix(cam.world_position), &mp),
    )];

    // A box under the middle of the sheet, its top ENTIRELY below it. Nothing
    // here is terrain or an SDF blob: `fieldDistance` cannot see any of it.
    //
    // Entirely below matters. An earlier version had the box poking THROUGH the
    // sheet, so the bright patch in the middle of the frame was the box's own
    // grey albedo drawn over the top — and every assertion here passed on that
    // while the sheet itself rendered flat black, which is what `surfaceGap`
    // returns when it cannot see anything at all. The probe has to be unable to
    // see any surface but the one doing the measuring.
    let floor_mat = MaterialParams::flat([0.4, 0.4, 0.45]);
    let box_xf = Transform {
        translation: DVec3::new(0.0, -0.8, 0.0),
        rotation: Quat::IDENTITY,
        scale: Vec3::new(2.0, 1.0, 2.0),
    };
    let with_box: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![(
        box_mesh,
        None,
        instance_of_mat(box_xf.render_matrix(cam.world_position), &floor_mat),
    )];

    let near = render(&gpu, &mut raster, globals, &with_box, &flsl, &format!("{dir}/gap_mesh.png"));
    let empty = render(&gpu, &mut raster, globals, &[], &flsl, &format!("{dir}/gap_open.png"));

    // 1. The reading actually VARIES. A flat field is what "the prepass never
    //    ran" looks like, and it is indistinguishable from working by eye.
    let (lo, hi) = (min_of(&near), max_of(&near));
    assert!(
        hi - lo > 0.25,
        "the gap reads flat across the sheet ({lo:.3}..{hi:.3}) — the depth prepass \
         almost certainly did not run, so surfaceGap saw nothing"
    );

    // 2. It is bright ON the box and dark off it. The box is at the centre; the
    //    corners of the frame are open sheet.
    let centre = sample(&near, W / 2, H / 2);
    let corner = sample(&near, W / 8, H / 8);
    assert!(
        centre > corner + 0.25,
        "the sheet is not brighter over the box (centre {centre:.3} vs corner {corner:.3}) — \
         surfaceGap is not keyed to the mesh behind it"
    );

    // 3. With nothing behind it, the whole sheet reads wide open. This is the
    //    value every "no answer" case returns, and it has to be the harmless one.
    let open = max_of(&empty);
    assert!(
        open < 0.15,
        "with nothing behind the sheet it should read as open water, got {open:.3} — \
         a shader would foam over the whole surface in an empty scene"
    );

    println!(
        "surface gap probe OK  (over the box {centre:.3}, open sheet {corner:.3}, \
         empty scene {open:.3})"
    );
}

/// Render one frame exactly as the editor does when a shader wants depth: the
/// opaque prepass first, then the colour pass. Returns the luminance field.
fn render(
    gpu: &Gpu,
    raster: &mut Raster,
    globals: Globals,
    instances: &[(MeshId, Option<TexId>, InstanceRaw)],
    flsl: &[floptle_render::FlslDraw],
    out: &str,
) -> Vec<f32> {
    assert!(
        raster.flsl_draws_want_depth(flsl),
        "the shader reads surfaceGap, so the renderer must know it needs the prepass — \
         without this the editor would never run one and the whole effect is a no-op"
    );
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
    raster.depth_prepass_with(gpu, globals, instances, flsl, &[], gpu.depth_texture());
    // …and BIND it, which is the half that was missing. `surfaceGap` reads the
    // prepass through the shared field bind group, and passing `None` here hands
    // the shader the 1×1 "no prepass" stand-in — under which it correctly
    // reports that there is nothing behind anything, forever. Building the bind
    // the way the editor builds it is the only way this probe measures the
    // feature rather than the scenery.
    let mut raymarch = floptle_render::Raymarch::new(gpu);
    raymarch.set_depth_prime(gpu, raster.prepass_view());
    raymarch.upload_globals(gpu, rm_globals(globals));
    raster.draw_scene_with(
        gpu,
        &color_view,
        gpu.depth_view(),
        globals,
        instances,
        flsl,
        &[],
        Some([0.0, 0.0, 0.0, 1.0]),
        Some(raymarch.field_bind()),
    );
    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    px.chunks_exact(4).map(|p| p[0] as f32 / 255.0).collect()
}

fn sample(lum: &[f32], x: u32, y: u32) -> f32 {
    lum[(y * W + x) as usize]
}

fn min_of(lum: &[f32]) -> f32 {
    lum.iter().copied().fold(f32::INFINITY, f32::min)
}

fn max_of(lum: &[f32]) -> f32 {
    lum.iter().copied().fold(f32::NEG_INFINITY, f32::max)
}

fn read_back(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
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

fn save_png(pixels: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}

/// The field globals `flsl_surface_gap` reads. It reprojects the shaded point to
/// find its texel in the prepass and then un-projects the depth it finds there,
/// so it needs BOTH matrices — and an identity `inv_view_proj` turns every gap
/// into a number with no relation to the scene.
fn rm_globals(g: Globals) -> floptle_render::RaymarchGlobals {
    let vp = glam::Mat4::from_cols_array_2d(&g.view_proj);
    floptle_render::RaymarchGlobals {
        view_proj: g.view_proj,
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        ..Default::default()
    }
}
