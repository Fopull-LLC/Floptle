//! Two questions this probe exists to answer, both of which are easy to believe
//! the wrong answer about by looking at a screenshot:
//!
//! **Does a surface exempt from fog actually stay out of it?** A fog opt-out is
//! the kind of feature that passes inspection while doing nothing — a surface
//! near the camera is barely fogged anyway, so "it looks right" proves nothing.
//! Here the surfaces sit deep in thick fog, where a fogged one is almost
//! entirely fog colour, and the control is the identical frame with the fog
//! switched off: if the exempt surface were being drawn differently for some
//! other reason, the control would show it.
//!
//! **Does vertex jitter move at RUNTIME, or only when the slider does?** It
//! moves — the snap runs in the vertex shader every frame. But it is a SNAP,
//! not an oscillation: a still camera on a still object lands in the same grid
//! cell every frame and holds perfectly still, which is what makes it look
//! broken to anyone testing it by staring at a paused scene. The proof is to
//! move a surface in even, sub-cell steps and watch it: unjittered it creeps
//! every frame, jittered it holds for several and then jumps. Both halves
//! matter — holding still is the artefact, and the jump is the evidence the
//! picture is being recomputed rather than frozen.
//!
//! The third check is that the PROJECT-wide artefacts reach a draw that names
//! no material at all (terrain, tilemaps, a plain primitive — everything that
//! carries surface-extras index 0), and that a material marked exempt takes
//! none of them.
//!
//! Run: cargo run -p floptle-render --example retro_fog_probe -- <out-dir>

use floptle_render::{
    cube, instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster,
    RaymarchGlobals, Raymarch, RenderCamera, SurfaceExtras, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 192;
/// Where the fog starts and where it is total. The surfaces sit at the far end.
const FOG_START: f32 = 1.0;
const FOG_END: f32 = 26.0;
const FOG_COLOR: [f32; 3] = [0.85, 0.15, 0.15];
/// A magenta surface in red fog: no channel of one is the other, so "it went
/// grey-ish" cannot be mistaken for either.
const SURFACE: [f32; 3] = [0.9, 0.0, 0.9];

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let mesh = raster.register(&gpu, &cube(1.0), None);
    let rm = Raymarch::new(&gpu);

    an_exempt_surface_stays_out_of_the_fog(&gpu, &mut raster, &rm, mesh, &dir);
    the_snap_is_recomputed_every_frame(&gpu, &mut raster, &rm, mesh, &dir);
    the_project_reaches_a_draw_that_names_no_material(&gpu, &mut raster, &rm, mesh);

    println!("retro/fog probe OK");
}

// ---------------------------------------------------------------------------
// 1. Fog reaches the surface that is in it, and not the one that opted out.
// ---------------------------------------------------------------------------
fn an_exempt_surface_stays_out_of_the_fog(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &Raymarch,
    mesh: MeshId,
    dir: &str,
) {
    // ONE cube, dead centre, deep in the fog — rendered three times with
    // nothing moved. Three frames of the same geometry rather than two cubes
    // side by side, because then no part of the answer depends on where either
    // of them happened to land.
    let shot = |raster: &mut Raster, fog_flag: bool, fog_on: bool| {
        let mut mat = MaterialParams::flat(SURFACE);
        mat.unlit = true;
        mat.ext_index = raster
            .push_surface_extras(SurfaceExtras { fog: fog_flag, ..SurfaceExtras::default() });
        let instances = [(mesh, None, instance_of_mat(at(0.0, 0.0, -25.0, 4.0), &mat))];
        render(gpu, raster, rm, &instances, fog_on)
    };

    let foggy = shot(raster, true, true);
    let opted = shot(raster, false, true);
    let no_fog = shot(raster, true, false);
    save(&foggy, &format!("{dir}/retro_fog_on.png"));
    save(&opted, &format!("{dir}/retro_fog_exempt.png"));
    let (fogged, exempt, clear) = (centre(&foggy), centre(&opted), centre(&no_fog));
    println!("fogged {fogged:.3?}   exempt {exempt:.3?}   fog off {clear:.3?}");
    println!("(surface is {SURFACE:.3?}, fog is {FOG_COLOR:.3?})");

    // The fogged one has been taken well into the fog — measured against the
    // SAME surface with the fog off, not against a distance ramp computed here.
    // Where exactly the ramp lands is `fog_probe`'s question; this one only has
    // to establish that the fog reached this surface at all, or the exemption
    // below would be comparing two unfogged frames and always pass.
    assert!(
        fogged[2] < clear[2] * 0.5,
        "the fog barely touched the surface ({fogged:.3?} against {clear:.3?} with the fog \
         off). Blue is the channel the fog has none of, so it is the one that has to \
         drain — and if it does not, everything below this line passes for free."
    );
    assert!(
        fogged[1] > 0.05,
        "the surface never picked up the fog's colour ({fogged:.3?}). Green is the channel \
         the SURFACE has none of, so anything there arrived from the fog."
    );

    // THE ONE THAT MATTERS: an exempt surface standing in fog is the same
    // picture as that surface with the fog switched off. Not "close to its own
    // colour" — the same frame, because a half-applied exemption would still
    // pass a looser test and would still be a bug.
    for c in 0..3 {
        assert!(
            (exempt[c] - clear[c]).abs() < 0.01,
            "an exempt surface in fog ({exempt:.3?}) is not the same as that surface with \
             the fog off ({clear:.3?}). The opt-out did not reach the distance ramp, the \
             volumetric composite, or one of the three shading paths."
        );
    }
}

// ---------------------------------------------------------------------------
// 2. The snap runs every frame — it holds, and then it jumps.
// ---------------------------------------------------------------------------
fn the_snap_is_recomputed_every_frame(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &Raymarch,
    mesh: MeshId,
    dir: &str,
) {
    // A coarse grid (cells a few pixels wide at this size) against a surface
    // that creeps by a fraction of one per frame. Unlit, so the ONLY thing that
    // can change between frames is where the silhouette lands.
    //
    // The SURFACE moves rather than the camera, because the world here is
    // camera-relative: the view matrix carries no translation at all (the world
    // is offset to the eye instead), so a probe that moved the camera would
    // render the same frame twelve times and prove nothing.
    const JITTER: f32 = 10.0;
    const FRAMES: usize = 12;
    const STEP: f32 = 0.1;

    let pan = |raster: &mut Raster, jitter: f32| -> Vec<Vec<u8>> {
        let mut mat = MaterialParams::flat([0.95, 0.95, 0.95]);
        mat.unlit = true;
        mat.ext_index = raster.push_surface_extras(SurfaceExtras {
            retro: floptle_core::Retro { jitter, ..Default::default() },
            ..SurfaceExtras::default()
        });
        (0..FRAMES)
            .map(|i| {
                let x = i as f32 * STEP;
                let instances = [(mesh, None, instance_of_mat(at(x, 0.0, -12.0, 1.0), &mat))];
                render(gpu, raster, rm, &instances, false)
            })
            .collect()
    };

    let smooth = pan(raster, 0.0);
    let snapped = pan(raster, JITTER);
    save(snapped.last().unwrap(), &format!("{dir}/retro_jitter.png"));

    let steps = |frames: &[Vec<u8>]| -> Vec<usize> {
        frames.windows(2).map(|w| changed(&w[0], &w[1])).collect()
    };
    let (smooth_steps, snap_steps) = (steps(&smooth), steps(&snapped));
    println!("unjittered per-frame change: {smooth_steps:?}");
    println!("jittered   per-frame change: {snap_steps:?}");

    // Without jitter the silhouette creeps: every frame differs from the last.
    // If this fails the motion is below the measurement floor, and then nothing
    // else here means what it says.
    assert!(
        smooth_steps.iter().all(|&n| n > 0),
        "the control never moved ({smooth_steps:?}) — the pan is too small to measure."
    );
    // With it, the surface HOLDS. A held frame is a thing that CANNOT happen
    // without a snap: the object moved and the picture did not, which is only
    // possible if its vertices landed back in the cells they were already in.
    assert!(
        snap_steps.contains(&0),
        "a jittered surface moved on every frame ({snap_steps:?}). Every frame differing \
         by a little is the un-snapped creep — the grid is not being applied."
    );
    // …and it also MOVES. Held frames alone would be a surface frozen at one
    // position; the two together are a picture that is recomputed every frame
    // and quantised every frame, which is the whole answer to "is this actually
    // running, or does the slider have to move?"
    assert!(
        snap_steps.iter().any(|&n| n > 0),
        "a jittered surface never changed at all ({snap_steps:?}) — that is a frozen \
         picture, not a snapped one."
    );

    // The quantisation, stated directly: the same pan produces FEWER distinct
    // frames through the grid than without it. Counting distinct pictures is
    // the measurement that does not care how the artefact is implemented —
    // whether the silhouette translates rigidly or each vertex crosses its own
    // cell boundary on its own frame, which is what actually happens.
    let distinct = |frames: &[Vec<u8>]| {
        let mut n = 1;
        for w in frames.windows(2) {
            if changed(&w[0], &w[1]) > 0 {
                n += 1;
            }
        }
        n
    };
    let (d_smooth, d_snap) = (distinct(&smooth), distinct(&snapped));
    println!("distinct frames over the pan: unjittered {d_smooth}, jittered {d_snap}");
    assert!(
        d_snap < d_smooth,
        "the grid quantised nothing: {d_snap} distinct frames with it against {d_smooth} \
         without. A jitter that produces as many distinct positions as no jitter at all \
         is not snapping to anything."
    );
}

// ---------------------------------------------------------------------------
// 3. The project's artefacts reach a draw with no material, and stop at one
//    that opted out.
// ---------------------------------------------------------------------------
fn the_project_reaches_a_draw_that_names_no_material(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &Raymarch,
    mesh: MeshId,
) {
    let project = floptle_core::Retro { jitter: 20.0, ..Default::default() };
    // A position deliberately off a grid line, so the snap has somewhere to
    // move the silhouette TO. Landing exactly on one would make "nothing moved"
    // the right answer for the wrong reason.
    const OFF_GRID: f32 = 0.037;

    // Two frames per project setting, from one white unlit material: one drawn
    // with surface-extras index 0 — what a terrain chunk, a tilemap or an
    // untinted primitive carries, having named no material at all — and one
    // that asked to be left out of the project's look.
    let shot = |raster: &mut Raster, retro: floptle_core::Retro| -> (Vec<u8>, Vec<u8>) {
        raster.set_retro_defaults(retro);
        let mut base = MaterialParams::flat([0.95, 0.95, 0.95]);
        base.unlit = true;
        let bare = MaterialParams { ext_index: 0, ..base };
        let opted = MaterialParams {
            ext_index: raster.push_surface_extras(SurfaceExtras {
                retro: floptle_core::Retro { exempt: true, ..Default::default() },
                ..SurfaceExtras::default()
            }),
            ..base
        };
        let one =
            |m: &MaterialParams| [(mesh, None, instance_of_mat(at(OFF_GRID, 0.0, -12.0, 1.0), m))];
        (
            render(gpu, raster, rm, &one(&bare), false),
            render(gpu, raster, rm, &one(&opted), false),
        )
    };

    let (bare_off, opted_off) = shot(raster, floptle_core::Retro::default());
    let (bare_on, opted_on) = shot(raster, project);

    let bare_moved = changed(&bare_off, &bare_on);
    let opted_moved = changed(&opted_off, &opted_on);
    println!("project jitter on: bare draw moved {bare_moved} px, opted-out moved {opted_moved} px");

    assert!(
        bare_moved > 0,
        "the project's artefacts never reached a draw that names no material. That is \
         every terrain chunk, every tilemap and every untinted primitive in the scene — \
         a project-wide look that skips them is not project-wide."
    );
    assert_eq!(
        opted_moved, 0,
        "a surface marked exempt took the project's jitter anyway. The opt-out is what \
         holds a viewmodel steady in a world that wobbles; 'mostly exempt' is no use."
    );
}

// --- the harness -----------------------------------------------------------

fn at(x: f32, y: f32, z: f32, s: f32) -> Mat4 {
    Mat4::from_scale_rotation_translation(Vec3::splat(s), Quat::IDENTITY, Vec3::new(x, y, z))
}

/// One frame into an offscreen target, read back as RGBA8.
///
/// The raymarch pass never runs — only its GLOBALS are uploaded, because that
/// is where the fog lives and the raster pass reads them through the field bind
/// group. A black clear is the background, so every measured pixel is either
/// the surface or nothing.
///
/// The camera never moves and takes no argument: the view matrix in this
/// renderer carries no translation (the world is offset to the eye instead), so
/// an eye position here would be a parameter that quietly did nothing. An
/// instance's model translation IS its position relative to the camera.
fn render(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &Raymarch,
    instances: &[(MeshId, Option<TexId>, InstanceRaw)],
    fog: bool,
) -> Vec<u8> {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("retro-fog-probe"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());

    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 500.0 },
    );
    let view_proj = cam.view_proj(1.0);
    rm.upload_globals(
        gpu,
        RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            fog_color: [FOG_COLOR[0], FOG_COLOR[1], FOG_COLOR[2], 0.0],
            fog_params: [FOG_START, FOG_END, if fog { 1.0 } else { 0.0 }, 0.0],
            ..Default::default()
        },
    );
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() },
        instances,
        Some([0.0, 0.0, 0.0, 1.0]),
        Some(rm.field_bind()),
    );
    read_back(gpu, &tex)
}

/// The average colour of a small patch at the middle of the frame — an average
/// rather than one texel so a single dithered pixel cannot decide the answer.
fn centre(px: &[u8]) -> [f32; 3] {
    let (cx, cy) = (S / 2, S / 2);
    let mut acc = [0.0f32; 3];
    let mut n = 0.0;
    for y in cy.saturating_sub(4)..(cy + 4).min(S) {
        for x in cx.saturating_sub(4)..(cx + 4).min(S) {
            let i = ((y * S + x) * 4) as usize;
            for c in 0..3 {
                acc[c] += srgb_to_linear(px[i + c] as f32 / 255.0);
            }
            n += 1.0;
        }
    }
    [acc[0] / n, acc[1] / n, acc[2] / n]
}

/// How many pixels differ between two frames, past a threshold that ignores
/// the last-bit wobble of the sRGB round trip.
fn changed(a: &[u8], b: &[u8]) -> usize {
    a.chunks_exact(4)
        .zip(b.chunks_exact(4))
        .filter(|(p, q)| (0..3).any(|c| p[c].abs_diff(q[c]) > 8))
        .count()
}

/// The probe measures in LINEAR light: the target is sRGB, so a raw byte is
/// not the value the shader wrote and comparing bytes against a colour the
/// shader was handed would be comparing two different quantities.
fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn read_back(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let row = (S * 4).div_ceil(256) * 256;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (row * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&Default::default());
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
                bytes_per_row: Some(row),
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((S * S * 4) as usize);
    for y in 0..S {
        let start = (y * row) as usize;
        out.extend_from_slice(&data[start..start + (S * 4) as usize]);
    }
    drop(data);
    buf.unmap();
    out
}

fn save(px: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("header").write_image_data(px).expect("write");
    println!("wrote {path}");
}
