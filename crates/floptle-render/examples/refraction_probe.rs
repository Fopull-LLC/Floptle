//! Does glass bend what is behind it?
//!
//! Reflections gave a crystal ball its surroundings. This is the other half:
//! seeing THROUGH it, distorted. The difference between the two on screen is
//! stark — a sphere that only reflects reads as a chrome bearing however clear
//! you make it — but it is easy to fake in a way a naive test would accept, so
//! the scene is built to make one answer possible.
//!
//! Behind the glass are two cards of strong, opposite colours, butted together
//! so their seam projects to the exact middle of the frame. A sphere in front of
//! that which merely tinted, or merely blurred, or merely showed the background
//! straight through, would leave the left of the ball red and the right of it
//! blue. Moving them is the whole effect.
//!
//! Five checks:
//!
//! 1. **The scene shows through at all.** With transmission on, the sphere
//!    carries the backdrop's colours rather than the material's.
//! 2. **It is displaced.** The pattern seen through the ball does not line up
//!    with the pattern beside it — which is what "refraction" means and what
//!    transparency alone would not do. A solid ball is a lens, so at a real
//!    index of refraction it INVERTS what is behind it: the half of the ball in
//!    front of the red card comes out blue. The checks below do not care which
//!    way it moves, only that it moves, so a change of lens model does not have
//!    to come with a change of test.
//! 3. **The index of refraction is the control.** At `ior = 1` light does not
//!    bend, so the pattern lines up again. Same scene, same material, one
//!    number: the displacement has to follow it.
//! 4. **With transmission off, none of it happens** — the sphere is opaque.
//! 5. **The glass is not sampling itself.** Rendered twice, a ball that
//!    refracted the picture it was already in would drift frame to frame as its
//!    own tint compounded. Two frames must be identical.
//!
//! Run: cargo run -p floptle-render --example refraction_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, plane, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId,
    Projection, Raster, Raymarch, RaymarchGlobals, RenderCamera, SceneHistory, SurfaceExtras, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let ball = raster.register(&gpu, &uv_sphere(1.0, 48, 64), None);
    let card = raster.register(&gpu, &plane(1.0), None);

    let glass = shot(&gpu, &mut raster, ball, card, 1.0, 1.8, &format!("{dir}/refract_glass.png"));
    let straight = shot(&gpu, &mut raster, ball, card, 1.0, 1.0, &format!("{dir}/refract_ior1.png"));
    let solid = shot(&gpu, &mut raster, ball, card, 0.0, 1.8, &format!("{dir}/refract_off.png"));

    // Two small windows INSIDE the ball, either side of its centre. With the
    // backdrop's seam projecting to the middle of the frame, "what colour is the
    // left half of the ball" is the whole measurement — and it is immune to
    // which way a lens happens to flip the image, which an edge-finder is not.
    let (lx, rx, y0, y1) = (0.40, 0.52, 0.46, 0.54);
    let look = |px: &Vec<u8>| {
        (mean_rgb(px, lx, lx + 0.08, y0, y1), mean_rgb(px, rx, rx + 0.08, y0, y1))
    };
    let (g_left, g_right) = look(&glass);
    let (s_left, s_right) = look(&straight);
    let (o_left, o_right) = look(&solid);
    println!(
        "inside the ball — glass L{g_left:.2?} R{g_right:.2?}  \
         ior=1 L{s_left:.2?} R{s_right:.2?}  transmission=0 L{o_left:.2?} R{o_right:.2?}"
    );

    // 4 (first, because it is the control). Opaque: unlit, no ambient, nothing
    // reflected — so an opaque ball is black, and anything the others show came
    // through it rather than off it.
    let solid_lum = (o_left[0] + o_left[1] + o_left[2] + o_right[0] + o_right[1] + o_right[2]) / 6.0;
    assert!(
        solid_lum < 0.06,
        "with transmission OFF the ball reads {solid_lum:.3} rather than black — it is not \
         opaque, so whatever the other checks measure is not light passing through it"
    );

    // 1 + 2. At ior = 1 light does not bend: the backdrop shows through the ball
    // exactly as it is, red on the left and blue on the right.
    assert!(
        s_left[0] > s_left[2] + 0.15,
        "at ior = 1 the left of the ball should show the RED card straight through, got \
         {s_left:.2?} — nothing is coming through"
    );
    assert!(
        s_right[2] > s_right[0] + 0.15,
        "…and the right the BLUE one, got {s_right:.2?}"
    );

    // 3. Turn the index of refraction up and the picture moves. Which WAY it
    // moves is the lens's business — a solid ball inverts — so this asks only
    // that it is no longer the undistorted view, which is what refraction means.
    let moved = (g_left[0] - s_left[0]).abs()
        + (g_left[2] - s_left[2]).abs()
        + (g_right[0] - s_right[0]).abs()
        + (g_right[2] - s_right[2]).abs();
    assert!(
        moved > 0.3,
        "the same ball at ior 1.8 shows L{g_left:.2?} R{g_right:.2?} and at ior 1.0 shows \
         L{s_left:.2?} R{s_right:.2?} — total change {moved:.3}. The index of refraction is \
         not bending anything, so this is transparency, not refraction"
    );

    // 5. Stable across frames: nothing is refracting itself.
    let again = shot(&gpu, &mut raster, ball, card, 1.0, 1.8, &format!("{dir}/refract_glass2.png"));
    let drift = glass
        .iter()
        .zip(&again)
        .map(|(a, b)| (*a as i32 - *b as i32).abs())
        .max()
        .unwrap_or(0);
    assert!(
        drift == 0,
        "the same frame rendered twice differs by {drift}/255 — the glass is sampling a \
         picture it is already in, so its tint compounds for as long as it is on screen"
    );

    println!("refraction probe OK  (the view through the ball moved by {moved:.3})");
}

/// One render of a ball in front of a two-colour backdrop.
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    ball: MeshId,
    card: MeshId,
    transmission: f32,
    ior: f32,
    out: &str,
) -> Vec<u8> {
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 45f32.to_radians(), near: 0.05, far: 100.0 },
    );
    let vp = cam.view_proj(1.0);

    // Two big cards edge to edge, filling the frame behind the ball. Unlit, so
    // their colours are exact and the only thing that can change them is the
    // glass in front. Camera-relative (ADR-0015): the view matrix carries no
    // translation, so these ARE positions relative to the eye.
    let mut left_mp = MaterialParams::flat([0.9, 0.1, 0.05]);
    left_mp.unlit = true;
    let mut right_mp = MaterialParams::flat([0.05, 0.35, 0.9]);
    right_mp.unlit = true;
    // `plane(half)` spans [-half, half], so `plane(1.0)` is TWO units across and
    // scale 8 makes each card 16 wide. Offsetting by 8 butts them edge to edge
    // with the seam at x = 0, which projects to the middle of the frame.
    let back = |x: f32| {
        Mat4::from_translation(Vec3::new(x, 0.0, -12.0)) * Mat4::from_scale(Vec3::splat(8.0))
    };

    // The ball: a physical surface that is nothing but glass — no metal, and a
    // white tint so the backdrop comes through its own colour rather than one
    // this probe chose.
    let mut ball_mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    ball_mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness: 0.0,
        metallic: 0.0,
        physical: true,
        reflectivity: 0.0, // no sky here; this probe is about what comes THROUGH
        transmission,
        ior,
        thickness: 1.6,
        ..SurfaceExtras::default()
    });

    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        (card, None, instance_of_mat(back(-8.0), &left_mp)),
        (card, None, instance_of_mat(back(8.0), &right_mp)),
        (
            ball,
            None,
            instance_of_mat(Mat4::from_translation(Vec3::new(0.0, 0.0, -4.0)), &ball_mp),
        ),
    ];

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
        ..Default::default()
    };
    let rm = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };

    let mut rmr = Raymarch::new(gpu);
    rmr.upload_globals(gpu, rm);

    // The scene WITHOUT the glass: prepass (which excludes it), then the colour
    // pass (which excludes it too).
    raster.depth_prepass_with(gpu, globals, &instances, &[], &[], gpu.depth_texture());
    rmr.set_depth_prime(gpu, raster.prepass_view());
    raster.draw_scene_with(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &instances,
        &[],
        &[],
        Some([0.0, 0.0, 0.0, 1.0]),
        Some(rmr.field_bind()),
    );

    // …captured, and handed back as "what is behind". This is the whole trick,
    // and doing it in the probe exactly as the editor does is the point: a
    // refraction pass fed the previous frame instead would pass checks 1–4 and
    // fail check 5.
    if raster.any_transmissive(&instances) {
        let mut behind = SceneHistory::new(&gpu.device, S, S, gpu.config.format);
        behind.capture(gpu, &view, vp, cam.world_position);
        rmr.bind_frame_targets(
            gpu,
            raster.prepass_view(),
            Some((behind.view(), behind.sampler())),
        );
        let mut glass_rm = rm;
        glass_rm.ssr_prev_vp = vp.to_cols_array_2d();
        rmr.upload_globals(gpu, glass_rm);
        raster.draw_transmissive(
            gpu,
            &view,
            gpu.depth_view(),
            globals,
            &instances,
            &[],
            Some(rmr.field_bind()),
            // One layer, one capture: this probe has a single piece of glass in
            // it, and depth layering is `glass_layers_probe`'s subject.
            &[],
            0,
        );
    }

    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    px
}

fn mean_rgb(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> [f32; 3] {
    let (xa, xb) = ((x0 * S as f32) as u32, (x1 * S as f32) as u32);
    let (ya, yb) = ((y0 * S as f32) as u32, (y1 * S as f32) as u32);
    let mut sum = [0f64; 3];
    let mut n = 0u32;
    for y in ya..yb.min(S) {
        for x in xa..xb.min(S) {
            let i = ((y * S + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += px[i + c] as f64 / 255.0;
            }
            n += 1;
        }
    }
    let n = n.max(1) as f64;
    [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
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
