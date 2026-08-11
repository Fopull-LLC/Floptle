//! Does a mirror indoors reflect the ROOM, or the sky?
//!
//! Screen-space reflections show what is on screen, and the environment map
//! behind them holds the sky. Outdoors that pair is nearly complete. Indoors it
//! is badly wrong: a reflected ray that leaves the frame comes back as
//! **daylight**, through the walls, in a sealed room. That is what a reflection
//! probe fixes, and this is the measurement.
//!
//! The scene is a closed box: a **red** wall on one side, a **blue** wall on the
//! other, dark grey everywhere else, and a mirror ball in the middle. The sky is
//! bright **green** — a colour that appears nowhere in the room, so any green in
//! the reflection is the renderer reaching outside a sealed box for it.
//!
//! Four checks:
//!
//! 1. **Without a probe the ball is green.** The bug, reproduced. If it is not
//!    green, everything below is measuring something else.
//! 2. **With a probe it is not.** The room replaced the sky.
//! 3. **The left of the ball is blue and the right is red.** This is the one
//!    that matters most and the one most likely to be quietly wrong: a mirror
//!    ball reflects the wall on its left onto its left, so a face table that is
//!    rotated or mirrored by one face passes every brightness check ever written
//!    and puts the red wall on the wrong side of the room.
//! 4. **Nothing but the probe changed.** The two shots differ only in whether
//!    the capture was bound, so the room, the ball and the sky are identical.
//!
//! Run: cargo run -p floptle-render --example interior_reflection_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster, Raymarch,
    PROBE_FACE, PROBE_H, PROBE_W, RaymarchGlobals, ReflectionProbes, RenderCamera, SurfaceExtras,
    TexId, instance_of_mat, plane, uv_sphere,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;
/// Half the room, in metres. The probe's box is the same, because the box IS
/// the room — that is what makes a reflected wall land on the wall.
const ROOM: f32 = 5.0;
/// The sky, in a colour the room does not contain.
const SKY: [f32; 3] = [0.05, 1.4, 0.05];

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    let with = shot(&gpu, true, &format!("{dir}/interior_reflection_on.png"));
    let without = shot(&gpu, false, &format!("{dir}/interior_reflection_off.png"));

    // WHERE the side walls land on a mirror ball is not where intuition puts
    // them. A ball seen head-on reflects what is BEHIND the camera at its centre
    // and sweeps round to what is behind IT at the rim; the wall to its left
    // shows up half way between, at the point whose normal is 45° round — about
    // 0.7 of the way out to the silhouette. Sampling nearer the middle reads the
    // back wall, which is grey in this room and looks exactly like a probe that
    // is not working. `PROFILE=1` prints the row these came from.
    //
    // Check 1 doubles as the guarantee that these windows are ON THE BALL: the
    // room contains no green, so a window reading green in the no-probe shot
    // cannot be looking at a wall. That matters — a probe measuring the blue
    // wall directly would report a perfect blue reflection while reflecting
    // nothing at all. `DUMP=1` writes the captured map out to be looked at.
    let left = |px: &[u8]| mean_rgb(px, 0.12, 0.22, 0.45, 0.55);
    let right = |px: &[u8]| mean_rgb(px, 0.78, 0.88, 0.45, 0.55);
    // How much of a patch is a given channel, as a share of the whole — so a
    // dark reflection and a bright one are compared on hue rather than level.
    let share = |c: [f32; 3], i: usize| c[i] / (c[0] + c[1] + c[2]).max(1e-4);

    if std::env::var("PROFILE").is_ok() {
        for b in 0..20 {
            let (x0, x1) = (b as f32 / 20.0, (b + 1) as f32 / 20.0);
            let c = mean_rgb(&with, x0, x1, 0.45, 0.55);
            println!("  x {x0:.2}..{x1:.2}  probe {:?}", round3(c));
        }
    }
    for (name, px) in [("probe", &with), ("no probe", &without)] {
        let (l, r) = (left(px), right(px));
        println!(
            "{name:>9}: left {:?} (r {:.2} g {:.2} b {:.2})  right {:?} (r {:.2} g {:.2} b {:.2})",
            round3(l),
            share(l, 0),
            share(l, 1),
            share(l, 2),
            round3(r),
            share(r, 0),
            share(r, 1),
            share(r, 2),
        );
    }

    // 1. The bug, first: with no probe, a sealed room reflects daylight.
    let (nl, nr) = (left(&without), right(&without));
    assert!(
        share(nl, 1) > 0.55 && share(nr, 1) > 0.55,
        "with no probe the mirror reads {:?} / {:?} — it is supposed to be showing \
         the GREEN sky through the walls of a closed room, and if it is not, this \
         probe is not reproducing the thing a probe is for",
        round3(nl),
        round3(nr)
    );

    // 2. …and with one, it does not.
    let (l, r) = (left(&with), right(&with));
    assert!(
        share(l, 1) < 0.40 && share(r, 1) < 0.40,
        "with a probe the mirror still reads {:?} / {:?} — the sky is coming \
         through, so the capture is not reaching the surface",
        round3(l),
        round3(r)
    );

    // 3. The side that matters. Blue wall on the left, red on the right.
    assert!(
        share(l, 2) > share(l, 0) * 1.5,
        "the LEFT of the ball reads {:?} — it faces the blue wall, so blue should \
         dominate. Red there means the captured map is mirrored or rotated, which \
         is the failure that looks fine until you notice the room is inside out",
        round3(l)
    );
    assert!(
        share(r, 0) > share(r, 2) * 1.5,
        "the RIGHT of the ball reads {:?} — it faces the red wall. See the note on \
         the left-hand check",
        round3(r)
    );

    // 4. A NEARLY polished surface is nearly a mirror.
    //
    // **Roughness 0 was the one value the old blur got right**, which is why this
    // check is taken against 0.1 rather than at it. The mip a reflection was read
    // at came from `sqrt(roughness)`, and sqrt LIFTS small values: 0.1 came out
    // at 0.32 and landed a third of the way up a box-filtered chain — an
    // eightfold blur on a surface the author had asked to be a mirror. Every
    // slider but a hard zero read as frosted, and no probe noticed, because every
    // probe used a hard zero.
    //
    // The assertion is a COMPARISON rather than a threshold: whatever a mirror in
    // this room looks like, a surface at roughness 0.1 has to look almost the
    // same. Stated that way it needs no golden image and no tuned constant, and
    // it fails loudly on the old mapping, which put four mip levels between these
    // two renders.
    let mirror = shot_rough(&gpu, true, 0.0, &format!("{dir}/interior_reflection_r000.png"));
    let nearly = shot_rough(&gpu, true, 0.1, &format!("{dir}/interior_reflection_r010.png"));
    let frosted = shot_rough(&gpu, true, 0.7, &format!("{dir}/interior_reflection_r070.png"));
    // What is measured is CONTRAST across the bars, in the patch at the ball's
    // centre where they land. A blur does not move a reflection, it flattens it:
    // the bars stay where they are and stop being bars. Comparing each render's
    // contrast against the mirror's makes the check a ratio, so it needs no
    // golden image and no absolute constant.
    let bars = |px: &[u8]| contrast(px, 0.36, 0.64, 0.42, 0.58);
    let (c_mirror, c_nearly, c_frosted) = (bars(&mirror), bars(&nearly), bars(&frosted));
    println!(
        "bar contrast — mirror {c_mirror:.4}  rough 0.1 {c_nearly:.4}  rough 0.7 {c_frosted:.4}"
    );
    assert!(
        c_nearly > c_mirror * 0.75,
        "a mirror shows the bars at contrast {c_mirror:.4} and a surface at roughness 0.1 \
         shows them at {c_nearly:.4} — a surface this close to polished has to keep \
         nearly all of the detail. Losing it means the reflection is being read from a \
         mip far coarser than its lobe calls for, which is the difference between a \
         polished floor and a frosted one, and is why a mirror could be frosted and \
         not polished"
    );
    assert!(
        c_frosted < c_mirror * 0.5,
        "roughness 0.7 shows the bars at contrast {c_frosted:.4} against a mirror's \
         {c_mirror:.4} — a rough surface is supposed to LOSE them. If it does not, \
         roughness is not reaching the reflection at all and the check above proves \
         nothing"
    );

    println!("interior reflection probe OK");
}

/// The room, in WORLD coordinates: (transform, colour). Rendered relative to
/// whichever eye is looking, which is the whole of what ADR-0015 asks of a
/// caller — subtract the eye and the model translation IS the camera-relative
/// position.
fn room() -> Vec<(Mat4, [f32; 3])> {
    // `plane(half)` spans [-half, half], so `plane(1.0)` scaled by ROOM is a
    // wall 2·ROOM across — exactly the box.
    let face = |t: Vec3, rot: Quat| {
        Mat4::from_translation(t) * Mat4::from_quat(rot) * Mat4::from_scale(Vec3::splat(ROOM))
    };
    let hp = std::f32::consts::FRAC_PI_2;
    vec![
        // +X wall, facing inward: RED.
        (face(Vec3::new(ROOM, 0.0, 0.0), Quat::from_rotation_y(-hp)), [1.4, 0.03, 0.03]),
        // −X wall, facing inward: BLUE.
        (face(Vec3::new(-ROOM, 0.0, 0.0), Quat::from_rotation_y(hp)), [0.03, 0.03, 1.4]),
        // The rest of the box, dark and neutral, so it contributes almost
        // nothing to the hue the checks read.
        (face(Vec3::new(0.0, 0.0, -ROOM), Quat::IDENTITY), [0.06, 0.06, 0.06]),
        (face(Vec3::new(0.0, 0.0, ROOM), Quat::from_rotation_y(std::f32::consts::PI)), [0.06, 0.06, 0.06]),
        (face(Vec3::new(0.0, -ROOM, 0.0), Quat::from_rotation_x(-hp)), [0.06, 0.06, 0.06]),
        (face(Vec3::new(0.0, ROOM, 0.0), Quat::from_rotation_x(hp)), [0.06, 0.06, 0.06]),
    ]
    .into_iter()
    // …and a row of narrow bright bars on the back wall.
    //
    // **Flat walls cannot measure a blur.** Averaging a wall of one colour with
    // itself gives that colour back, so a room made only of flat faces reads the
    // same through a mirror as through a frosted pane — which is why the old
    // over-blurring survived every check here for as long as it did. Detail
    // finer than the blur is the only thing that can tell them apart, and these
    // bars are it: a mirror shows five bars, a frosted surface shows one grey
    // smear, and the difference is a contrast measurement.
    // They go on the wall BEHIND the eye. A ball seen head-on reflects what is
    // behind the camera at its centre — the one part of it every measurement here
    // already looks through — so anywhere else and the bars would sit at the rim
    // where the silhouette compresses them to nothing.
    .chain((0..9).map(|i| {
        let x = (i as f32 - 4.0) * ROOM * 0.19;
        (
            Mat4::from_translation(Vec3::new(x, 0.0, ROOM * 0.98))
                * Mat4::from_rotation_y(std::f32::consts::PI)
                * Mat4::from_scale(Vec3::new(ROOM * 0.032, ROOM, 1.0)),
            [2.2, 2.2, 2.2],
        )
    }))
    .collect()
}

/// Standard deviation of luminance over a fractional window — how much detail
/// a patch of the picture actually holds.
fn contrast(px: &[u8], x0: f32, x1: f32, y0: f32, y1: f32) -> f32 {
    let mut v = Vec::new();
    for y in (y0 * S as f32) as u32..(y1 * S as f32) as u32 {
        for x in (x0 * S as f32) as u32..(x1 * S as f32) as u32 {
            let i = ((y * S + x) * 4) as usize;
            if i + 2 < px.len() {
                v.push((px[i] as f32 + px[i + 1] as f32 + px[i + 2] as f32) / (3.0 * 255.0));
            }
        }
    }
    let m = v.iter().sum::<f32>() / v.len().max(1) as f32;
    (v.iter().map(|x| (x - m) * (x - m)).sum::<f32>() / v.len().max(1) as f32).sqrt()
}


fn shot(gpu: &Gpu, use_probe: bool, out: &str) -> Vec<u8> {
    shot_rough(gpu, use_probe, 0.0, out)
}

fn shot_rough(gpu: &Gpu, use_probe: bool, rough: f32, out: &str) -> Vec<u8> {
    let mut raster = Raster::new(gpu);
    let wall = raster.register(gpu, &plane(1.0), None);
    let ball = raster.register(gpu, &uv_sphere(1.0, 48, 24), None);
    let mut rm = Raymarch::new(gpu);
    let probes = ReflectionProbes::new(gpu);

    // Unlit walls: their colour in the reflection is their albedo and nothing
    // else, so check 3 reads hue rather than which wall the sun happens to be on.
    let wall_mp = |c: [f32; 3]| {
        let mut mp = MaterialParams::flat(c);
        mp.unlit = true;
        mp
    };
    // A bright silver mirror. For a metal the albedo IS f0, so a dark one
    // reflects almost nothing and would measure the analytic grazing sheen
    // instead of the environment.
    let mut ball_mp = MaterialParams::flat([0.95, 0.95, 0.95]);
    ball_mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness: rough,
        metallic: 1.0,
        physical: true,
        reflectivity: 1.0,
        ..SurfaceExtras::default()
    });

    // ---- the capture -------------------------------------------------------
    //
    // Six 90° renders from the middle of the room, folded into one
    // equirectangular map. The ball is deliberately NOT in them: a mirror in its
    // own reflection is a separate question, and this probe is about the walls.
    let probe_at = Vec3::ZERO;
    let face_instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = room()
        .into_iter()
        .map(|(m, c)| {
            (wall, None, instance_of_mat(Mat4::from_translation(-probe_at) * m, &wall_mp(c)))
        })
        .collect();
    for f in 0..6 {
        let cam = RenderCamera::new(
            DVec3::ZERO,
            floptle_render::reflect::face_rotation(f),
            // A cube face IS a 90° square frustum — anything else and the
            // directions the conversion assumes stop matching the pixels.
            Projection::Perspective { fov_y: std::f32::consts::FRAC_PI_2, near: 0.05, far: 200.0 },
        );
        let fvp = cam.view_proj(1.0);
        let fg = Globals {
            view_proj: fvp.to_cols_array_2d(),
            light_dir: [0.0, 1.0, 0.0, 0.0],
            light_color: [0.0; 4],
            ambient: [0.0; 4],
            ..Default::default()
        };
        raster.draw_scene_with(
            gpu,
            probes.face_target(f),
            probes.face_depth(f),
            fg,
            &face_instances,
            &[],
            &[],
            // The capture's own background is the sky it would see, and a sealed
            // room sees none of it — clearing to black keeps a leak between two
            // walls from reading as an extra light source.
            Some([0.0, 0.0, 0.0, 1.0]),
            None,
        );
    }
    probes.resolve(gpu, 0);
    if use_probe && std::env::var("DUMP").is_ok() {
        dump(gpu, probes.texture(), PROBE_W, PROBE_H, &out.replace(".png", "_map.png"));
        dump(gpu, probes.faces_texture(), PROBE_FACE, PROBE_FACE, &out.replace(".png", "_face0.png"));
    }

    // ---- the view ----------------------------------------------------------
    //
    // Inside the room, looking at the ball, with the walls to left and right.
    let eye = Vec3::new(0.0, 0.0, 4.0);
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 45f32.to_radians(), near: 0.05, far: 100.0 },
    );
    let vp = cam.view_proj(1.0);

    let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = room()
        .into_iter()
        .map(|(m, c)| (wall, None, instance_of_mat(Mat4::from_translation(-eye) * m, &wall_mp(c))))
        .collect();
    instances.push((
        ball,
        None,
        instance_of_mat(Mat4::from_translation(-eye) * Mat4::from_scale(Vec3::splat(1.6)), &ball_mp),
    ));

    // The probe's box, camera-relative like everything else the shader reads.
    let probe_rel = probe_at - eye;
    let mut probe_pos = [[0.0f32; 4]; floptle_render::MAX_PROBES];
    let mut probe_half = [[1.0f32; 4]; floptle_render::MAX_PROBES];
    probe_pos[0] = [probe_rel.x, probe_rel.y, probe_rel.z, 1.0];
    probe_half[0] = [ROOM, ROOM, ROOM, 1.0];

    // Sky: `sky_params.x < 0.5` selects the built-in vault, whose void colour is
    // `bg`. Green, and nowhere in the room.
    let rmg = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0; 4],
        ambient: [0.0; 4],
        bg: [SKY[0], SKY[1], SKY[2], 1.0],
        // No screen-space reflections: the walls ARE on screen here, and the
        // point of this probe is what happens when the environment has to
        // answer. SSR is `ssr_probe`'s subject.
        ssr: [0.0, 30.0, 32.0, 0.5],
        probe_meta: [if use_probe { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
        probe_pos,
        probe_half,
        ..Default::default()
    };
    rm.upload_globals(gpu, rmg);
    rm.capture_env(gpu);
    // The ONLY difference between the two shots.
    if use_probe {
        rm.set_reflection_probes(gpu, Some((probes.view(), probes.sampler())));
    }

    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0; 4],
        ambient: [0.0; 4],
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

/// Read layer 0 of a texture back and write it out, so a capture can be looked
/// at rather than inferred from the numbers a mirror reports.
fn dump(gpu: &Gpu, tex: &wgpu::Texture, w: u32, h: u32, path: &str) {
    let unpadded = w * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("dump"),
        size: (padded * h) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("dump") });
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
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for row in 0..h {
        let start = (row * padded) as usize;
        px.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    let file = std::fs::File::create(path).expect("create png");
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().unwrap().write_image_data(&px).unwrap();
}

fn round3(c: [f32; 3]) -> [f32; 3] {
    [
        (c[0] * 1000.0).round() / 1000.0,
        (c[1] * 1000.0).round() / 1000.0,
        (c[2] * 1000.0).round() / 1000.0,
    ]
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
    let d = n.max(1) as f64;
    [(sum[0] / d) as f32, (sum[1] / d) as f32, (sum[2] / d) as f32]
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
