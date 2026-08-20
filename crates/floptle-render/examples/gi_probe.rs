//! Baked global illumination, on the GPU: does the probe volume actually reach
//! a surface, does it carry DIRECTION, does the leak test actually reject, and
//! does the WGSL agree with the Rust it was transliterated from.
//!
//! The last one is the point of this probe existing. `gi_bounce` in
//! `field.wgsl` is a hand transliteration of `BakedGi::sample` in the
//! `floptle-gi` crate, and that crate's unit tests are where "does not leak
//! through a wall" is actually specified. Tests on one side of a transliteration
//! prove nothing about the other side, so this renders the same situation
//! through the shader and compares the pixel to what the Rust says it should be.
//!
//! Every check is a CONTROL PAIR: the same frame rendered twice with one thing
//! changed. A GI implementation that quietly does nothing produces a perfectly
//! plausible picture — it produces the picture from before there was any GI.
//!
//! Run: cargo run -p floptle-render --example gi_probe -- <out-dir>

use floptle_gi::{BakedGi, Probe, ProbeGrid};
use floptle_render::{
    Globals, Gpu, MaterialParams, Projection, Raster, RaymarchGlobals, RenderCamera, TexId,
    cube, instance_of_mat,
};
use glam::{Mat4, Quat, Vec3};

const S: u32 = 192;
/// The cube the probes light. Its front face is at +Z, dead-on to the camera,
/// which makes "the pixel in the middle" a surface whose position and normal are
/// exactly known — and that is what lets the shader be compared to the Rust.
const HALF: f32 = 0.9;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    a_volume_lights_a_surface_that_had_no_light(&gpu, &dir);
    the_bounce_carries_a_direction(&gpu, &dir);
    a_buried_probe_lights_nothing(&gpu, &dir);
    the_shader_agrees_with_the_rust(&gpu);

    println!("gi probe OK");
}

// ---------------------------------------------------------------------------
// 1. A volume lights a surface that had no light.
//
// One white cube, no sun, no point lights, no ambient: without GI it is black,
// and that black IS the control. Switch a red volume on and it must go red —
// which proves the uniforms, the texture, the sampling and the ambient
// replacement all connect, and separates "GI works" from "the scene was
// already lit".
// ---------------------------------------------------------------------------
fn a_volume_lights_a_surface_that_had_no_light(gpu: &Gpu, dir: &str) {
    let off = render(gpu, None, 1.0, "gi-off");
    let red = filled(uniform_sh([0.6, 0.08, 0.08]), 10.0);
    let on = render(gpu, Some(&red), 1.0, "gi-on");

    let c_off = centre(&off);
    let c_on = centre(&on);
    save(&off, &format!("{dir}/gi_off.png"));
    save(&on, &format!("{dir}/gi_on.png"));
    println!("volume: off {c_off:?}  on {c_on:?}");

    assert!(
        lum(c_off) < 0.02,
        "the CONTROL must be dark before anything below means something — got {c_off:?}. \
         A lit control means some other light is reaching the cube and this probe is \
         measuring that instead."
    );
    assert!(
        c_on[0] > 0.25,
        "a red volume must light the cube red — got {c_on:?}. Equal to the control means \
         the bounce never reached the surface: the uniforms, the probe texture or the \
         ambient replacement."
    );
    assert!(
        c_on[0] > c_on[1] * 2.0,
        "and it must arrive in the COLOUR it was baked, not as grey — got {c_on:?}"
    );

    // Intensity is applied on upload, so it is the cheap knob — and it has to
    // actually be cheap AND actually work, which is one assertion.
    let dim = render(gpu, Some(&red), 0.25, "gi-dim");
    let c_dim = centre(&dim);
    println!("intensity: 1.0 → {:.3}   0.25 → {:.3}", c_on[0], c_dim[0]);
    assert!(
        c_dim[0] < c_on[0] * 0.6 && c_dim[0] > 0.01,
        "quarter intensity must be visibly dimmer and not zero — {:.3} vs {:.3}",
        c_dim[0],
        c_on[0]
    );
}

// ---------------------------------------------------------------------------
// 2. The bounce carries a direction.
//
// The whole reason to keep band 1 of the spherical harmonic. Probes holding
// light that arrives from ABOVE must light the cube's top face and leave its
// bottom face dark; a constant-only fit would light both identically, and would
// look completely fine in a screenshot.
// ---------------------------------------------------------------------------
fn the_bounce_carries_a_direction(gpu: &Gpu, dir: &str) {
    let mut sh = floptle_gi::Sh1::ZERO;
    // A single strong lobe straight up.
    sh.accumulate(Vec3::Y, [0.25; 3], 4.0 * std::f32::consts::PI);
    let above = filled(sh, 10.0);
    // The control: the same total light, spread evenly over the sphere.
    let even = filled(uniform_sh([0.25; 3]), 10.0);

    let lit = render_tilted(gpu, Some(&above), "gi-above");
    let flat = render_tilted(gpu, Some(&even), "gi-even");
    save(&lit, &format!("{dir}/gi_directional.png"));

    // The tilted camera sees the cube's top face in the upper part of the frame
    // and its front face in the middle.
    let top = |px: &[[u8; 4]]| lum(at(px, 0.5, 0.30));
    let front = |px: &[[u8; 4]]| lum(at(px, 0.5, 0.66));
    println!(
        "direction: from above  top {:.3} / front {:.3}   ·   even  top {:.3} / front {:.3}",
        top(&lit),
        front(&lit),
        top(&flat),
        front(&flat)
    );
    assert!(
        (top(&flat) - front(&flat)).abs() < 0.05,
        "the CONTROL is light from every side and must shade both faces the same — \
         {:.3} vs {:.3}",
        top(&flat),
        front(&flat)
    );
    assert!(
        top(&lit) > front(&lit) * 1.4,
        "light from above must favour the upward face — {:.3} vs {:.3}. Equal faces mean \
         only band 0 survived, which is a bounce with no direction in it.",
        top(&lit),
        front(&lit)
    );
}

// ---------------------------------------------------------------------------
// 3. A buried probe lights nothing.
//
// The leak test, on the GPU. The same probes, the same light, the same
// everything — except that the clearance recorded at each probe says it is
// inside geometry. That must switch the bounce off, and turning the volume's
// `leak` knob to zero must switch it back on, which is what proves the knob is
// what did it rather than the data being empty.
// ---------------------------------------------------------------------------
fn a_buried_probe_lights_nothing(gpu: &Gpu, dir: &str) {
    let clear = filled(uniform_sh([0.5; 3]), 10.0);
    let buried = filled(uniform_sh([0.5; 3]), 0.0);

    let a = render_leak(gpu, &clear, 1.0);
    let b = render_leak(gpu, &buried, 1.0);
    let c = render_leak(gpu, &buried, 0.0);
    save(&b, &format!("{dir}/gi_buried.png"));
    println!(
        "leak: clear probes {:.3}   buried {:.3}   buried with rejection off {:.3}",
        lum(centre(&a)),
        lum(centre(&b)),
        lum(centre(&c))
    );

    assert!(lum(centre(&a)) > 0.2, "the CONTROL must be lit — {:?}", centre(&a));
    assert!(
        lum(centre(&b)) < 0.02,
        "probes with no clearance are inside geometry and must light nothing — got {:?}. \
         This is the artefact everybody recognises: the lit room next door glowing \
         through the wall.",
        centre(&b)
    );
    assert!(
        lum(centre(&c)) > 0.2,
        "…and with rejection turned off the same data must light again — got {:?}. If it \
         stays dark, something other than the leak test made it dark.",
        centre(&c)
    );
}

// ---------------------------------------------------------------------------
// 4. The shader agrees with the Rust.
//
// `gi_bounce` (WGSL) and `BakedGi::sample` (Rust) are the same algorithm typed
// twice. The Rust one has the unit tests; this is the only thing that says the
// GPU is running the same thing. The cube's front face is at a known point with
// a known normal, its albedo is white and every other light is off, so the
// centre pixel IS the sampler's output and nothing else.
// ---------------------------------------------------------------------------
fn the_shader_agrees_with_the_rust(gpu: &Gpu) {
    // Deliberately not uniform and not centred: a lopsided volume with a lobe
    // off-axis exercises the trilinear weights, the facing term and the
    // per-channel evaluation at once. A symmetric case can agree by accident.
    let mut baked = grid();
    for (i, p) in baked.probes.iter_mut().enumerate() {
        let k = i as f32 * 0.37;
        p.sh.accumulate(
            Vec3::new(0.4, 1.0, -0.3).normalize(),
            [0.8 + k * 0.05, 0.5, 0.2 + k * 0.02],
            4.0 * std::f32::consts::PI,
        );
        p.nearest = 4.0 + k;
    }
    let px = render(gpu, Some(&baked), 1.0, "gi-agree");
    let got = centre(&px);

    const BIAS: f32 = 0.5;
    let (want, coverage) =
        baked.sample(Vec3::new(0.0, 0.0, HALF), Vec3::Z, 1.0, BIAS);
    println!(
        "agreement: shader {got:?}  rust {:?} (coverage {coverage:.2})",
        [want[0], want[1], want[2]]
    );
    assert!(coverage > 0.99, "the sample point must be well inside the volume");
    for ch in 0..3 {
        let w = want[ch] * coverage;
        assert!(
            (got[ch] - w).abs() < 0.03,
            "channel {ch}: the shader says {:.3}, the Rust says {:.3}. The WGSL in \
             field.wgsl is a transliteration of BakedGi::sample — if one of them changed, \
             both have to.",
            got[ch],
            w
        );
    }
}

// ---- the scene ----------------------------------------------------------------

/// A 3×3×3 lattice over a box comfortably bigger than the cube, so the sample
/// point is well inside and the coverage fade is not part of what is measured.
fn grid() -> BakedGi {
    BakedGi::empty(ProbeGrid::from_spacing([0.0; 3], [4.0, 4.0, 4.0], 4.0))
}

/// Radiance arriving evenly from every direction, projected exactly.
fn uniform_sh(rgb: [f32; 3]) -> floptle_gi::Sh1 {
    let mut sh = floptle_gi::Sh1::ZERO;
    // Six opposing deltas: band 1 cancels, band 0 lands on the sphere's area.
    let dw = 4.0 * std::f32::consts::PI / 6.0;
    for d in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
        sh.accumulate(d, rgb, dw);
    }
    sh
}

fn filled(sh: floptle_gi::Sh1, nearest: f32) -> BakedGi {
    let mut b = grid();
    b.probes.fill(Probe { sh, nearest, mean: nearest });
    b.bounces = 1;
    b
}

fn render(gpu: &Gpu, baked: Option<&BakedGi>, intensity: f32, label: &str) -> Vec<[u8; 4]> {
    draw(gpu, baked, intensity, 1.0, Quat::IDENTITY, Vec3::new(0.0, 0.0, 4.0), label)
}

/// The same cube seen from above and in front, so one frame shows both its top
/// face and its front face.
fn render_tilted(gpu: &Gpu, baked: Option<&BakedGi>, label: &str) -> Vec<[u8; 4]> {
    draw(
        gpu,
        baked,
        1.0,
        1.0,
        Quat::from_rotation_x(-0.5),
        Vec3::new(0.0, 2.0, 3.6),
        label,
    )
}

fn render_leak(gpu: &Gpu, baked: &BakedGi, leak: f32) -> Vec<[u8; 4]> {
    draw(gpu, Some(baked), 1.0, leak, Quat::IDENTITY, Vec3::new(0.0, 0.0, 4.0), "gi-leak")
}

#[allow(clippy::too_many_arguments)]
fn draw(
    gpu: &Gpu,
    baked: Option<&BakedGi>,
    intensity: f32,
    leak: f32,
    rot: Quat,
    eye: Vec3,
    label: &str,
) -> Vec<[u8; 4]> {
    let mut raster = Raster::new(gpu);
    // A whole Raymarch, because the GI probe texture and the `G` uniform the
    // shader reads both live in the SHARED field bind group. A probe that
    // passed `field: None` would bind the zeroed fallback and measure nothing —
    // which is exactly the shape of bug this file exists to catch.
    let mut raymarch = floptle_render::Raymarch::new(gpu);
    let volume = match baked {
        Some(b) => floptle_render::GiVolume::upload(gpu, b, [0.0; 3], leak, intensity, false, 0.5),
        None => floptle_render::GiVolume::empty(gpu),
    };
    let mut rm = RaymarchGlobals::default();
    volume.apply(&mut rm, [eye.x as f64, eye.y as f64, eye.z as f64]);
    raymarch.set_gi(gpu, volume);
    raymarch.upload_globals(gpu, rm);

    let mesh = raster.register(gpu, &cube(HALF), None);
    // White, fully ambient-lit, nothing else: the pixel is the bounce.
    let mut mat = MaterialParams::flat([1.0, 1.0, 1.0]);
    mat.ambient = 1.0;
    // Register real surface extras. Without an entry the shader's `ext_at`
    // reads past the end of an empty storage buffer, and the normal-map
    // strength it comes back with tilts the shading normal off the face — which
    // shows up here as the bounce arriving from slightly the wrong direction and
    // absolutely nowhere else.
    mat.ext_index = raster.push_surface_extras(floptle_render::SurfaceExtras::default());
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.9, near: 0.05, far: 100.0 },
    );
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        // No flat ambient, no lights. Whatever appears came from the volume.
        ambient: [0.0; 4],
        ..Default::default()
    };
    let (tex, view) = target(gpu, label);
    let inst = instance_of_mat(Mat4::from_translation(-eye), &mat);
    raster.draw_scene(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &[(mesh, None::<TexId>, inst)],
        Some([0.0, 0.0, 0.0, 1.0]),
        Some(raymarch.field_bind()),
    );
    read_rgba(gpu, &tex)
}

// ---- reading the frame --------------------------------------------------------

fn target(gpu: &Gpu, label: &str) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
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

/// The pixel at fractional position `(fx, fy)`, in LINEAR light.
///
/// The target is 8-bit sRGB, so the hardware encoded on write; comparing the
/// bytes to a linear number without undoing that is a 2.2-power error that looks
/// like the GI being uniformly too bright.
fn at(px: &[[u8; 4]], fx: f32, fy: f32) -> [f32; 3] {
    let i = ((fy * S as f32) as u32 * S + (fx * S as f32) as u32) as usize;
    let p = px[i.min(px.len() - 1)];
    [srgb(p[0]), srgb(p[1]), srgb(p[2])]
}

fn centre(px: &[[u8; 4]]) -> [f32; 3] {
    at(px, 0.5, 0.5)
}

fn srgb(b: u8) -> f32 {
    let c = b as f32 / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn save(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flatten().copied().collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().expect("header").write_image_data(&flat).expect("write");
}

fn read_rgba(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("gi-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("gi-readback") });
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
                rows_per_image: Some(S),
            },
        },
        wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            out.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    out
}
