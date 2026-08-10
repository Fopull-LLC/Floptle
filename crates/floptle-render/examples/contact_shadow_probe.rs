//! Contact shadows: does a mesh that casts NOTHING through the field cast a
//! shadow where it touches the floor.
//!
//! That framing is the point. A moving mesh is not in the SDF field, so it casts
//! through a collider proxy — a box or a capsule — and a character's shadow is a
//! capsule's. The place that reads worst is the contact between a foot and the
//! floor, which is exactly where the proxy is least like the thing it stands
//! for. So this probe deliberately gives the caster **no proxy and no volume**:
//! the field has nothing to say about it, and anything that appears under it
//! came from the screen-space trace.
//!
//! Every check is a control pair on one knob, because a shadow that is really a
//! darkened ambient term, or an AO fringe, would pass a visual inspection and
//! fail all of these.
//!
//! Run: cargo run -p floptle-render --example contact_shadow_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, MaterialParams, MeshId, Projection, Raster, Raymarch, RaymarchGlobals,
    RenderCamera, SurfaceExtras, TexId, cube, instance_of_mat, plane,
};
use glam::{Mat4, Quat, Vec3};

const W: u32 = 320;
const H: u32 = 320;
/// The caster: a pillar standing on the floor, lit from a low angle, so the
/// shadow it should cast runs a long way across the floor beside it — long
/// enough that the trace's REACH is what decides where it stops.
const SLAB: Vec3 = Vec3::new(0.0, 0.9, 0.0);
/// Toward the sun — low and to the right, so the shadow falls to the LEFT.
const SUN: Vec3 = Vec3::new(0.72, 0.60, 0.35);

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(W, H);

    let off = render(&gpu, Contact { on: false, ..Contact::default() }, "off");
    let on = render(&gpu, Contact::default(), "on");
    save(&off, &format!("{dir}/contact_off.png"));
    save(&on, &format!("{dir}/contact_on.png"));

    // Measured as a DIFFERENCE against the control frame rather than at chosen
    // pixels: where the shadow lands depends on the sun angle and the camera, and
    // a probe that hunts for it at hardcoded coordinates is a probe that starts
    // passing for the wrong reason the moment either is nudged.
    let (n_on, box_on) = darkened(&off, &on);
    println!("contact on:  {n_on} px darkened, spanning {box_on:?}");
    assert!(
        n_on > 300,
        "a mesh the field knows nothing about must still darken the floor it stands \
         on — only {n_on} px changed. There is no proxy and no volume in this scene, \
         so the marched shadow has nothing to hit and this is the only thing that \
         could have drawn one."
    );

    // Strength 0 must land back on the control exactly. The trace still runs and
    // still finds the occluder, so this is a check on the knob and not on the
    // early-out above it.
    let none = render(&gpu, Contact { strength: 0.0, ..Contact::default() }, "zero");
    let (n_zero, _) = darkened(&off, &none);
    println!("strength 0:  {n_zero} px darkened");
    assert!(n_zero < 30, "strength 0 must be the control frame — {n_zero} px changed");

    // Reach sets how far the shadow runs across the floor. This is what separates
    // a traced shadow from a patch painted under the object.
    let far_reach = render(&gpu, Contact { length: 1.6, ..Contact::default() }, "reach");
    save(&far_reach, &format!("{dir}/contact_reach.png"));
    let (n_far, box_far) = darkened(&off, &far_reach);
    println!("1.6 m reach: {n_far} px darkened, spanning {box_far:?}");
    assert!(
        n_far > n_on * 2,
        "a longer trace must reach further across the floor — {n_far} px vs {n_on} px. \
         The pillar's real shadow runs several metres; the reach is what truncates it, \
         so raising the reach has to give more of it back."
    );
    assert!(
        box_far.2 > box_on.2,
        "…and the shadow must get LONGER, not just denser — {} px wide vs {} px",
        box_far.2,
        box_on.2
    );

    // And it must stay SHORT-range by default: with a 0.35 m reach the shadow
    // cannot run the width of the frame, or the knob means nothing.
    assert!(
        box_on.2 < W * 3 / 4,
        "a 0.35 m trace darkened {} px of width — that is not a contact shadow",
        box_on.2
    );

    println!("contact shadow probe OK");
}

/// Pixels meaningfully darker than the control, and the bounding box they fill:
/// `(count, (x0, y0, width, height))`.
fn darkened(control: &[[u8; 4]], test: &[[u8; 4]]) -> (u32, (u32, u32, u32, u32)) {
    let (mut n, mut x0, mut x1, mut y0, mut y1) = (0u32, W, 0u32, H, 0u32);
    for y in 0..H {
        for x in 0..W {
            let i = (y * W + x) as usize;
            let (c, t) = (control[i][1] as i32, test[i][1] as i32);
            if c > 20 && t < c - 12 {
                n += 1;
                x0 = x0.min(x);
                x1 = x1.max(x);
                y0 = y0.min(y);
                y1 = y1.max(y);
            }
        }
    }
    if n == 0 { (0, (0, 0, 0, 0)) } else { (n, (x0, y0, x1 - x0 + 1, y1 - y0 + 1)) }
}

#[derive(Clone, Copy)]
struct Contact {
    on: bool,
    length: f32,
    steps: f32,
    strength: f32,
}

impl Default for Contact {
    fn default() -> Self {
        Self { on: true, length: 0.35, steps: 16.0, strength: 0.9 }
    }
}

fn render(gpu: &Gpu, c: Contact, label: &str) -> Vec<[u8; 4]> {
    let mut raster = Raster::new(gpu);
    let mut raymarch = Raymarch::new(gpu);

    let floor = raster.register(gpu, &plane(1.0), None);
    let slab = raster.register(gpu, &cube(0.5), None);
    let mut mat = MaterialParams::flat([0.85, 0.85, 0.85]);
    mat.ambient = 0.25;
    mat.ext_index = raster.push_surface_extras(SurfaceExtras::default());

    // Close and low, on the shadow side: a contact shadow is a small thing near
    // the ground, and a probe framed from across the room measures a handful of
    // pixels and calls it a result.
    let eye = Vec3::new(-0.9, 1.05, 1.9);
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        Quat::from_rotation_y(-0.30) * Quat::from_rotation_x(-0.42),
        Projection::Perspective { fov_y: 0.9, near: 0.05, far: 100.0 },
    );
    let vp = cam.view_proj(W as f32 / H as f32);
    let sun = SUN.normalize();

    // The floor: the plane faces +Z, so tip it onto its back.
    let floor_m = Mat4::from_translation(-eye)
        * Mat4::from_scale_rotation_translation(
            Vec3::splat(12.0),
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            Vec3::ZERO,
        );
    // The caster: a pillar, standing on the floor.
    let slab_m = Mat4::from_translation(-eye)
        * Mat4::from_scale_rotation_translation(
            Vec3::new(0.45, 1.8, 0.45),
            Quat::IDENTITY,
            SLAB,
        );
    let instances: Vec<(MeshId, Option<TexId>, InstanceTriple)> = vec![
        (floor, None, instance_of_mat(floor_m, &mat)),
        (slab, None, instance_of_mat(slab_m, &mat)),
    ];

    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [sun.x, sun.y, sun.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.10, 0.11, 0.14, 0.0],
        ..Default::default()
    };

    // The prepass. Without it the shader reads a 1×1 stand-in and every contact
    // shadow is a no-op — which is the correct behaviour offscreen, and would
    // also be a very quiet way for this probe to prove nothing.
    let primed = raster.depth_prepass_with(gpu, globals, &instances, &[], &[], gpu.depth_texture());
    assert!(primed, "the depth prepass has to run — the trace reads it and nothing else");
    raymarch.set_depth_prime(gpu, raster.prepass_view());

    // The field globals: no volumes, no blobs, NO PROXIES. The marched shadow
    // has nothing to hit, so whatever appears on the floor is the screen trace.
    raymarch.upload_globals(gpu, RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_dir: [sun.x, sun.y, sun.z, 0.0],
        light_color: [1.0, 0.98, 0.92, 0.0],
        ambient: [0.10, 0.11, 0.14, 0.0],
        shadow_params: [1.0, 24.0, 1.0, 60.0],
        contact: [
            if c.on { 1.0 } else { 0.0 },
            c.length,
            c.steps,
            c.strength,
        ],
        ..Default::default()
    });

    let (tex, view) = target(gpu, label);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &instances,
        Some([0.02, 0.02, 0.03, 1.0]),
        Some(raymarch.field_bind()),
    );
    read_rgba(gpu, &tex)
}

type InstanceTriple = floptle_render::InstanceRaw;

// ---- reading the frame --------------------------------------------------------

fn target(gpu: &Gpu, label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn save(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flatten().copied().collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("header").write_image_data(&flat).expect("write");
}

fn read_rgba(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("contact-readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("contact-readback") });
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
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}
