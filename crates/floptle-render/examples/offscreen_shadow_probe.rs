//! Does a lamp stop at a wall the camera **cannot see**?
//!
//! `point_shadow_probe` proves the screen-space half of a local shadow: a wall
//! that is in frame blocks the lamp behind it. That method has one structural
//! limit — it reads the depth prepass, so it only knows about surfaces that were
//! actually drawn. Turn away from the wall and the shadow it was casting stops
//! existing, which is the single most alarming way a shadow can behave.
//!
//! This probe measures the other half. **The occluder is never drawn.** It
//! exists only as a shadow proxy — the same oriented box the sun's march reads,
//! and the same one a static collider mesh bakes itself into — and it sits well
//! outside the camera's frustum. Nothing in the depth buffer knows it is there.
//! So any darkness that appears on the floor came from the field march and from
//! nowhere else, which is the entire claim.
//!
//! The scene is a floor and a lamp, both lit by nothing else: no sun, no
//! ambient, no sky. Every photon on that floor came from the lamp.
//!
//! Four checks:
//!
//! 1. **The band goes dark** when the proxy is there and the lamp casts.
//! 2. **The floor either side of it does not** — a shadow, not a dimmer.
//! 3. **The proxy alone changes nothing.** Same box, lamp not casting: fully
//!    lit. So the proxy is not darkening the floor through some other path.
//! 4. **The flag alone changes nothing.** Same casting lamp, no proxy: fully
//!    lit. So the darkness is the occluder rather than the flag costing light.
//!
//! 3 and 4 together are what make this a measurement instead of a coincidence:
//! the shadow needs BOTH halves, and either one on its own leaves the floor
//! exactly as it was.
//!
//! Run: cargo run -p floptle-render --example offscreen_shadow_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId, instance_of_mat, plane,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;

/// The lamp, above and BEHIND the camera. Behind, so that the straight line from
/// it to the floor passes through space the camera is not looking at — which is
/// where the occluder goes.
const LAMP: Vec3 = Vec3::new(0.0, 4.0, 2.0);
/// The occluder: a wide, thin bar at y = 0, off the top of the frame. If it were
/// drawn it would not be in the picture; it is not drawn either way.
const BAR_AT: Vec3 = Vec3::new(0.0, 0.0, -0.33);
const BAR_HALF: Vec3 = Vec3::new(12.0, 0.3, 0.6);

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let mut rm = Raymarch::new(&gpu);
    let floor_mesh = raster.register(&gpu, &plane(1.0), None);

    let both = shot(&gpu, &mut raster, &mut rm, floor_mesh, true, true, &format!("{dir}/offscreen_shadow_on.png"));
    let proxy_only = shot(&gpu, &mut raster, &mut rm, floor_mesh, false, true, &format!("{dir}/offscreen_shadow_noflag.png"));
    let flag_only = shot(&gpu, &mut raster, &mut rm, floor_mesh, true, false, &format!("{dir}/offscreen_shadow_noproxy.png"));

    // Bands in frame coordinates. Distance runs UPWARD under a camera looking
    // down, so the shadow the bar throws lands as a horizontal stripe and the
    // floor above and below it stays lit. `PROFILE=1` prints the profile these
    // were read off.
    let band = |px: &[u8]| mean_lum(px, 0.35, 0.65, 0.30, 0.45);
    let above = |px: &[u8]| mean_lum(px, 0.35, 0.65, 0.02, 0.12);
    let below = |px: &[u8]| mean_lum(px, 0.35, 0.65, 0.70, 0.85);

    if std::env::var("PROFILE").is_ok() {
        for b in 0..20 {
            let (y0, y1) = (b as f32 / 20.0, (b + 1) as f32 / 20.0);
            println!(
                "  y {:.2}..{:.2}  both {:.3}  proxy-only {:.3}  flag-only {:.3}",
                y0,
                y1,
                mean_lum(&both, 0.35, 0.65, y0, y1),
                mean_lum(&proxy_only, 0.35, 0.65, y0, y1),
                mean_lum(&flag_only, 0.35, 0.65, y0, y1),
            );
        }
    }
    println!(
        "both:       band {:.3}  above {:.3}  below {:.3}\n\
         proxy only: band {:.3}\n\
         flag only:  band {:.3}",
        band(&both),
        above(&both),
        below(&both),
        band(&proxy_only),
        band(&flag_only),
    );

    // 3 + 4 first: the two controls. If either one is already dark, everything
    // below is measuring the framing rather than the shadow.
    assert!(
        band(&proxy_only) > 0.10,
        "with the occluder present but the lamp NOT casting, the band reads {:.3} — \
         the lamp is not lighting that stretch of floor in the first place, so this \
         probe cannot show anything being blocked",
        band(&proxy_only)
    );
    assert!(
        band(&flag_only) > 0.10,
        "with the lamp casting and NO occluder, the band reads {:.3} — something \
         other than the proxy is taking the light away",
        band(&flag_only)
    );

    // 1. Both halves together: the off-screen bar casts.
    assert!(
        band(&both) < band(&flag_only) * 0.5,
        "the band reads {:.3} with the off-screen occluder and {:.3} without it — a \
         proxy the camera cannot see is not casting, which is the whole point of \
         marching the field",
        band(&both),
        band(&flag_only)
    );

    // 2. And it is a shadow: the floor either side of the band keeps its light.
    assert!(
        above(&both) > band(&both) * 2.0 && below(&both) > band(&both) * 2.0,
        "band {:.3} vs above {:.3} / below {:.3} — the whole floor went down \
         together, so this is a lamp getting dimmer rather than a bar casting",
        band(&both),
        above(&both),
        below(&both)
    );

    println!("offscreen shadow probe OK");
}

fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &mut Raymarch,
    floor_mesh: MeshId,
    casts: bool,
    proxy: bool,
    out: &str,
) -> Vec<u8> {
    // Looking steeply down at a floor. The camera is at the ORIGIN and
    // everything is camera-relative (ADR-0015) — the view matrix has no
    // translation, so these positions ARE the shader's coordinates.
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::from_rotation_x(-1.15),
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 200.0 },
    );
    let vp = cam.view_proj(1.0);

    let floor = Mat4::from_translation(Vec3::new(0.0, -8.0, -5.0))
        * Mat4::from_rotation_x(-std::f32::consts::FRAC_PI_2)
        * Mat4::from_scale(Vec3::splat(40.0));
    let floor_mp = MaterialParams::flat([0.85, 0.85, 0.85]);
    // ONE surface, and it is the one being measured. Nothing else is drawn —
    // not the lamp, and above all not the occluder.
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        vec![(floor_mesh, None, instance_of_mat(floor, &floor_mp))];

    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let mut point_shape = [[0.0f32; 4]; 16];
    let point_rot = [[0.0f32, 0.0, 0.0, 1.0]; 16];
    point_pos[0] = [LAMP.x, LAMP.y, LAMP.z, 40.0];
    point_color[0] = [1.3, 1.26, 1.18, 0.0];
    // `[kind, a, b, flags]` — a bare point, bit 1 of the flags is "casts".
    point_shape[0] = [0.0, 0.0, 0.0, if casts { 2.0 } else { 0.0 }];

    // The occluder, as a proxy and ONLY as a proxy: `prox_b.w = 2` is an
    // oriented box, `prox_a.xyz` its centre, `prox_b.xyz` its half-extents.
    let mut prox_a = [[0.0f32; 4]; 32];
    let mut prox_b = [[0.0f32; 4]; 32];
    let prox_rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
    if proxy {
        prox_a[0] = [BAR_AT.x, BAR_AT.y, BAR_AT.z, 0.0];
        prox_b[0] = [BAR_HALF.x, BAR_HALF.y, BAR_HALF.z, 2.0];
    }

    // No sun and no ambient: every lit pixel on that floor came from the lamp.
    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0; 4],
        ambient: [0.0; 4],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        point_shape,
        point_rot,
        ..Default::default()
    };
    let rmg = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_color: [0.0; 4],
        ambient: [0.0; 4],
        bg: [0.0, 0.0, 0.0, 1.0],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        point_shape,
        point_rot,
        prox_count: [if proxy { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
        prox_a,
        prox_b,
        prox_rot,
        ..Default::default()
    };

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    rm.upload_globals(gpu, rmg);
    // The prepass runs and is bound, exactly as a real frame does it — so the
    // screen-space half is fully armed and still has nothing to find. That is
    // the point: it can only see the floor, and the floor is not the occluder.
    raster.depth_prepass_with(gpu, globals, &instances, &[], &[], gpu.depth_texture());
    rm.set_depth_prime(gpu, raster.prepass_view());
    raster.draw_scene_with(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &instances,
        &[],
        &[],
        Some([0.0, 0.0, 0.0, 1.0]),
        Some(rm.field_bind()),
    );
    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    px
}

/// Mean luminance of a rectangle given in 0..1 frame coordinates.
fn mean_lum(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> f32 {
    let (xa, xb) = ((x0 * S as f32) as u32, (x1 * S as f32) as u32);
    let (ya, yb) = ((y0 * S as f32) as u32, (y1 * S as f32) as u32);
    let mut sum = 0f64;
    let mut n = 0u32;
    for y in ya..yb.min(S) {
        for x in xa..xb.min(S) {
            let i = ((y * S + x) * 4) as usize;
            sum += (px[i] as f64 + px[i + 1] as f64 + px[i + 2] as f64) / (3.0 * 255.0);
            n += 1;
        }
    }
    (sum / n.max(1) as f64) as f32
}

fn read_back(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let unpadded = S * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
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
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((S * S * 4) as usize);
    for row in 0..S {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    pixels
}

fn save_png(pixels: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
