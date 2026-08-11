//! Does a lamp stop at a wall?
//!
//! Every placeable light in this engine used to shine through everything: the
//! comment in the fog march said so out loud — "no march (they are unshadowed
//! fill everywhere else in the engine too)". A torch in a doorway lit the room
//! behind the door as brightly as the one it was in, which is the single most
//! conspicuous way local lighting can be wrong.
//!
//! The scene is a floor, a wall standing on it, and a lamp on ONE side of that
//! wall. The floor is plain and unlit by anything else — no sun, no ambient, no
//! sky — so every photon on it came from the lamp, and the wall's shadow is the
//! only thing that can take it away.
//!
//! Four checks:
//!
//! 1. **The far side of the wall goes dark** when the lamp casts.
//! 2. **The near side does not.** A "shadow" that dimmed the whole floor would
//!    be a lamp that got weaker, not a lamp that got blocked.
//! 3. **Without the flag, both sides are lit** — so the flag is the cause, and
//!    the darkness is not the falloff, the wall's own geometry or the framing.
//! 4. **A lamp that casts still lights its own side as brightly.** The shadow
//!    must cost the scene the light behind the wall and nothing else.
//!
//! Run: cargo run -p floptle-render --example point_shadow_probe -- <out-dir>

use floptle_render::{
    cube, instance_of_mat, plane, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let mut rm = Raymarch::new(&gpu);
    let floor_mesh = raster.register(&gpu, &plane(1.0), None);
    let wall_mesh = raster.register(&gpu, &cube(0.5), None);

    let casting = shot(&gpu, &mut raster, &mut rm, floor_mesh, wall_mesh, true, &format!("{dir}/point_shadow_on.png"));
    let through = shot(&gpu, &mut raster, &mut rm, floor_mesh, wall_mesh, false, &format!("{dir}/point_shadow_off.png"));

    // The lamp is on the NEAR side, which under a camera looking down is the
    // BOTTOM of the frame; distance runs upward, so the floor beyond the wall is
    // the top band and the wall itself is the dark line between them. Both
    // rectangles stay clear of that line — a sample that catches the wall reads
    // the wall's own albedo in BOTH renders and reports no difference, which is
    // exactly how the first cut of this probe managed to fail.
    // `PROFILE=1` prints the vertical profile these came from.
    let near_on = mean_lum(&casting, 0.35, 0.65, 0.60, 0.85);
    let far_on = mean_lum(&casting, 0.35, 0.65, 0.03, 0.20);
    let near_off = mean_lum(&through, 0.35, 0.65, 0.60, 0.85);
    let far_off = mean_lum(&through, 0.35, 0.65, 0.03, 0.20);
    println!(
        "casting: near {near_on:.3} far {far_on:.3}\n\
         through: near {near_off:.3} far {far_off:.3}"
    );
    if std::env::var("PROFILE").is_ok() {
        for band in 0..20 {
            let y0 = band as f32 / 20.0;
            let y1 = (band + 1) as f32 / 20.0;
            println!(
                "  y {:.2}..{:.2}  casting {:.3}  through {:.3}",
                y0,
                y1,
                mean_lum(&casting, 0.35, 0.65, y0, y1),
                mean_lum(&through, 0.35, 0.65, y0, y1)
            );
        }
    }

    // 3 (first, because it is the control). With no flag the lamp reaches both
    // sides — if it does not, everything below measures the framing instead.
    assert!(
        far_off > 0.10,
        "with shadows OFF the far side of the wall reads {far_off:.3} — the lamp is \
         not reaching it in the first place, so this probe cannot show anything \
         being blocked"
    );

    // 1. The wall blocks it.
    assert!(
        far_on < far_off * 0.4,
        "the far side is {far_on:.3} with the lamp casting and {far_off:.3} without — \
         the wall is not stopping the light"
    );

    // 2 + 4. …and only it. The lit side is untouched.
    assert!(
        near_on > near_off * 0.8,
        "the lamp's OWN side dropped from {near_off:.3} to {near_on:.3} — this is a \
         light getting dimmer, not a light being blocked"
    );
    assert!(
        near_on > far_on * 2.5,
        "lit {near_on:.3} vs shadowed {far_on:.3} — there is no shadow here, only an \
         evenly darker floor"
    );

    println!("point shadow probe OK");
}

fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &mut Raymarch,
    floor_mesh: MeshId,
    wall_mesh: MeshId,
    shadows: bool,
    out: &str,
) -> Vec<u8> {
    // Looking down at the floor from above and behind, so the wall runs across
    // the middle of the frame with lit floor on one side of it and shadow on the
    // other. The camera is at the ORIGIN and everything is camera-relative
    // (ADR-0015) — the view matrix carries no translation.
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::from_rotation_x(-1.15),
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.05, far: 200.0 },
    );
    let vp = cam.view_proj(1.0);

    let floor = Mat4::from_translation(Vec3::new(0.0, -8.0, -5.0))
        * Mat4::from_rotation_x(-std::f32::consts::FRAC_PI_2)
        * Mat4::from_scale(Vec3::splat(40.0));
    // A wall standing on the floor, across the view — LOW, so the camera sees
    // over it to the floor beyond. That floor is the whole measurement, and a
    // wall tall enough to hide it would leave nothing to look at.
    let wall = Mat4::from_translation(Vec3::new(0.0, -7.5, -5.5))
        * Mat4::from_scale(Vec3::new(14.0, 1.0, 0.3));

    let floor_mp = MaterialParams::flat([0.85, 0.85, 0.85]);
    let wall_mp = MaterialParams::flat([0.2, 0.2, 0.22]);
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        (floor_mesh, None, instance_of_mat(floor, &floor_mp)),
        (wall_mesh, None, instance_of_mat(wall, &wall_mp)),
    ];

    // The lamp: on the NEAR side of the wall, just above the floor.
    // BELOW the top of the wall, so everything behind it at floor level is in
    // shadow — a big unambiguous region rather than a thin band whose edge the
    // sampling rectangles would have to chase.
    let lamp = Vec3::new(0.0, -7.2, -3.5);
    // `[kind, a, b, flags]` — a bare point, and bit 1 of the flags is "casts".
    let shape = [0.0f32, 0.0, 0.0, if shadows { 2.0 } else { 0.0 }];
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let mut point_shape = [[0.0f32; 4]; 16];
    let mut point_rot = [[0.0f32, 0.0, 0.0, 1.0]; 16];
    point_pos[0] = [lamp.x, lamp.y, lamp.z, 14.0];
    point_color[0] = [3.0, 2.9, 2.7, 0.0];
    point_shape[0] = shape;
    point_rot[0] = [0.0, 0.0, 0.0, 1.0];

    // No sun and no ambient: every lit pixel on that floor came from the lamp.
    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
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
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        point_shape,
        point_rot,
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
    // The prepass, and the BIND — a local shadow marches it, so without both
    // halves `point_vis` reports "no prepass, nothing blocks anything" and this
    // probe would measure the unshadowed lamp twice.
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
