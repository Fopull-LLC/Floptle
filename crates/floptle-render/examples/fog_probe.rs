//! Volumetric fog with light injection: does the media actually take the
//! scene's light, does the phase function point it, does an occluder carve a
//! beam out of it, and does the amount at 0 still land exactly on the flat fog
//! it replaced.
//!
//! That last one is the reason for the whole design. Lit fog is a rewrite of the
//! expression every existing volumetric scene is already looking at, so "amount
//! 0 looks about the same" is not good enough — it has to BE the same, and the
//! check here is against closed-form arithmetic, not against a control frame,
//! because a control frame rendered by the same rewritten code would agree with
//! itself no matter what it did.
//!
//! Everything else is a CONTROL PAIR: the same frame twice with one thing
//! changed. Fog is the easiest effect in a renderer to fake convincingly — a
//! grey wash over the screen passes every visual inspection and carries no
//! light at all.
//!
//! Run: cargo run -p floptle-render --example fog_probe -- <out-dir>

use floptle_render::{Gpu, Projection, Raymarch, RaymarchGlobals, RenderCamera, TextureData};
use glam::{DVec3, Quat, Vec3};

const S: u32 = 192;
/// How far a sky ray marches fog — the `fog_end` fence. Every expected value
/// below is derived from this and [`DENSITY`].
const FAR: f32 = 20.0;
/// Media density per world unit. 0.05 over 20 units is an optical depth of
/// exactly 1, so the fog covers 1 - e⁻¹ = 63.2% of the frame: thick enough to
/// measure, thin enough that nothing saturates.
const DENSITY: f32 = 0.05;
const FOG_COLOR: [f32; 3] = [0.5, 0.6, 0.7];

/// The fog settings for one frame: (amount, anisotropy, shafts).
type Fog = (f32, f32, bool);

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    let mut rm = Raymarch::new(&gpu);
    // A black 4×4 equirect sky, so the background behind the fog is a KNOWN
    // zero. The built-in sky is stars and nebula — a fine picture and a useless
    // control.
    rm.set_sky_texture(&gpu, Some(&TextureData { pixels: vec![0; 4 * 4 * 4], width: 4, height: 4 }));

    unlit_fog_is_exactly_the_fog_it_replaced(&gpu, &rm, &dir);
    the_media_takes_the_scene_s_light(&gpu, &rm, &dir);
    the_phase_function_points_the_light(&gpu, &rm);
    an_occluder_carves_a_beam(&gpu, &rm, &dir);
    a_lamp_glows_where_it_stands(&gpu, &rm, &dir);

    println!("fog probe OK");
}

// ---------------------------------------------------------------------------
// 1. Amount 0 is the fog that was there before, to the third decimal.
//
// With a constant in-scattered radiance the marched sum telescopes: every slab
// contributes `T·(1-e^-σdt)` of the same colour, so the total is
// `C·(1-T_final)` and `T_final = e^(-σ·t)` exactly, independent of the step
// count and of the per-pixel jitter. That means there IS a closed form to check
// against, and checking against it is the only way to know the rewrite did not
// quietly change every scene that already had volumetric fog on.
// ---------------------------------------------------------------------------
fn unlit_fog_is_exactly_the_fog_it_replaced(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    let px = render(gpu, rm, Frame { fog: (0.0, 0.0, false), ..Frame::default() }, "fog-flat");
    save(&px, &format!("{dir}/fog_flat.png"));

    let cover = 1.0 - (-DENSITY * FAR).exp();
    let want = [FOG_COLOR[0] * cover, FOG_COLOR[1] * cover, FOG_COLOR[2] * cover];
    let got = centre(&px);
    println!("flat fog: want {want:.3?}  got {got:.3?}  (coverage {cover:.4})");
    for c in 0..3 {
        assert!(
            (got[c] - want[c]).abs() < 0.012,
            "amount 0 must land on the flat volumetric fog EXACTLY, not near it — \
             channel {c}: want {:.4}, got {:.4}. Every scene that already had \
             volumetric fog on is looking at this expression.",
            want[c],
            got[c]
        );
    }

    // And the step count must not change the answer, which is what makes the
    // quality slider a quality slider rather than a brightness slider.
    for steps in [4.0, 8.0, 48.0] {
        let p = render(gpu, rm, Frame { steps, fog: (0.0, 0.0, false), ..Frame::default() }, "fog-steps");
        let c = centre(&p);
        assert!(
            (c[1] - want[1]).abs() < 0.012,
            "{steps} steps changed the unlit fog: {:.4} vs {:.4}. A step count that \
             moves the result means the slab integral is wrong, and raising quality \
             would re-expose every fogged scene.",
            c[1],
            want[1]
        );
    }
}

// ---------------------------------------------------------------------------
// 2. Turning the amount up puts the SCENE'S light in the air.
//
// The control is the frame above: the same fog, same density, same colour, with
// nothing but the amount changed. If lit fog were secretly still painting the
// flat colour, this would be the identical picture.
// ---------------------------------------------------------------------------
fn the_media_takes_the_scene_s_light(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    // Sun straight ahead, so the forward lobe faces the camera.
    let sun = Vec3::new(0.0, 0.0, -1.0);
    let f = |amount| Frame { sun, light: [0.2; 3], fog: (amount, 0.6, false), ..Frame::default() };
    let flat = centre(&render(gpu, rm, f(0.0), "fog-off"));
    let px = render(gpu, rm, f(1.0), "fog-lit");
    let lit = centre(&px);
    save(&px, &format!("{dir}/fog_lit.png"));
    println!("amount: 0 → {flat:.3?}   1 → {lit:.3?}");

    assert!(
        lit[1] > flat[1] * 1.5,
        "the sun standing behind the fog must brighten it — {:.3} vs {:.3}. Equal \
         means the injected radiance never reached the composite and the fog is \
         still a painted colour.",
        lit[1],
        flat[1]
    );

    // A cold sun in warm fog: the result must carry BOTH, because the fog colour
    // is the media's albedo once light is injected and albedo multiplies.
    let cold = centre(&render(
        gpu,
        rm,
        Frame { sun, light: [0.04, 0.07, 0.16], fog: (1.0, 0.6, false), ..Frame::default() },
        "fog-cold",
    ));
    println!("cold sun in warm fog: {cold:.3?}");
    assert!(
        cold[2] > cold[0] * 1.5,
        "a blue sun must make the fog blue — {cold:.3?}. Fog that ignores the light's \
         colour is fog that is not being lit by it."
    );
}

// ---------------------------------------------------------------------------
// 3. The phase function points the light.
//
// A mote of fog has no normal, so anisotropy is what stands in for `N·L`
// everywhere else in the renderer — and it is checked against the arithmetic,
// not against a picture. Henyey-Greenstein at g, normalised so isotropic is 1,
// is (1-g²)/(1+g²-2g·cosθ)^1.5; the ratio between looking INTO the light and
// away from it is a number this file can compute.
// ---------------------------------------------------------------------------
fn the_phase_function_points_the_light(gpu: &Gpu, rm: &Raymarch) {
    let g = 0.4;
    let toward = Vec3::new(0.0, 0.0, -1.0); // the camera looks down -Z
    let away = Vec3::new(0.0, 0.0, 1.0);
    let f = |sun, g| Frame { sun, fog: (1.0, g, false), light: [0.3; 3], ..Frame::default() };

    let a = centre(&render(gpu, rm, f(toward, g), "fog-toward"))[1];
    let b = centre(&render(gpu, rm, f(away, g), "fog-away"))[1];
    let want = hg(1.0, g) / hg(-1.0, g);
    println!("anisotropy {g}: toward {a:.4}  away {b:.4}  ratio {:.2} (want {want:.2})", a / b);
    assert!(
        (a / b - want).abs() / want < 0.12,
        "forward scattering must follow the phase function — measured {:.2}×, the \
         arithmetic says {want:.2}×",
        a / b
    );

    // The control: at g = 0 the media throws light evenly, so the sun's position
    // stops mattering entirely.
    let a0 = centre(&render(gpu, rm, f(toward, 0.0), "fog-iso-t"))[1];
    let b0 = centre(&render(gpu, rm, f(away, 0.0), "fog-iso-a"))[1];
    println!("isotropic: toward {a0:.4}  away {b0:.4}");
    assert!(
        (a0 - b0).abs() < 0.01,
        "at anisotropy 0 the fog must look the same in both directions — {a0:.4} vs \
         {b0:.4}. A difference means something other than the phase term is reading \
         the light direction."
    );
}

// ---------------------------------------------------------------------------
// 4. An occluder carves a beam out of the fog.
//
// This is the whole feature. The sun is straight up and a shadow proxy — a box
// that is never drawn, only marched — is laid over the camera's whole view. The
// air under it must go dark.
//
// The second pair is the one that matters: with shafts OFF the very same box
// must change NOTHING. Fog dimming for any other reason (the box occluding the
// sky, an unrelated shading path) would survive the first assertion and die on
// this one.
// ---------------------------------------------------------------------------
fn an_occluder_carves_a_beam(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    let sun = Vec3::Y;
    let f = |roof, shafts| Frame {
        sun,
        light: [0.5; 3],
        // A little ambient, because the assertion below is that shadowed air is
        // DARK and not GONE — and with a zero ambient "gone" would be right.
        ambient: [0.03, 0.03, 0.04],
        roof,
        fog: (1.0, 0.0, shafts),
        ..Frame::default()
    };

    let open = centre(&render(gpu, rm, f(false, true), "fog-open"))[1];
    let shaded = render(gpu, rm, f(true, true), "fog-roofed");
    save(&shaded, &format!("{dir}/fog_shaft.png"));
    let under = centre(&shaded)[1];
    println!("shafts on:  open air {open:.4}  under the roof {under:.4}");
    assert!(
        under < open * 0.5,
        "air in shadow must be darker than air in the sun — {under:.4} vs {open:.4}. \
         Equal means the fog march never asked whether the light reaches it, which is \
         the difference between lit fog and a beam."
    );
    assert!(
        under > 0.001,
        "and it must not go black — {under:.4}. Shadowed air still gets ambient and \
         bounce; fog that goes to zero in shadow reads as a hole cut in the world."
    );

    let open_off = centre(&render(gpu, rm, f(false, false), "fog-open-noshaft"))[1];
    let under_off = centre(&render(gpu, rm, f(true, false), "fog-roofed-noshaft"))[1];
    println!("shafts off: open air {open_off:.4}  under the roof {under_off:.4}");
    assert!(
        (open_off - under_off).abs() < 0.006,
        "with shafts off the occluder must make NO difference — {open_off:.4} vs \
         {under_off:.4}. If it still darkens, the darkening above was not the fog's \
         own shadow march and this probe is measuring the wrong thing."
    );
}

// ---------------------------------------------------------------------------
// 5. A lamp glows where it stands.
//
// Point lights are the reason interiors want this at all — a torch with no
// visible cone is a torch that stops at its own bulb. The light sits off to the
// left, so the left of the frame must be brighter than the right; the control is
// the same frame with the amount at 0, where the two sides are identical.
// ---------------------------------------------------------------------------
fn a_lamp_glows_where_it_stands(gpu: &Gpu, rm: &Raymarch, dir: &str) {
    let lamp = Some(([-3.0, 0.0, -8.0], 9.0, [0.9, 0.35, 0.15]));
    let px = render(
        gpu,
        rm,
        Frame { lamp, light: [0.0; 3], fog: (1.0, 0.0, false), ..Frame::default() },
        "fog-lamp",
    );
    save(&px, &format!("{dir}/fog_lamp.png"));
    let left = at(&px, 0.22, 0.5);
    let right = at(&px, 0.78, 0.5);
    println!("lamp: left {left:.3?}  right {right:.3?}");
    assert!(
        left[0] > right[0] * 1.6,
        "the air beside a lamp must be brighter than the air across the room — \
         {:.3} vs {:.3}",
        left[0],
        right[0]
    );
    assert!(
        left[0] > left[2] * 1.3,
        "and it must glow in the LAMP'S colour, not the fog's — {left:.3?}"
    );

    let flat = render(
        gpu,
        rm,
        Frame { lamp, light: [0.0; 3], fog: (0.0, 0.0, false), ..Frame::default() },
        "fog-lamp-off",
    );
    let (fl, fr) = (at(&flat, 0.22, 0.5)[0], at(&flat, 0.78, 0.5)[0]);
    println!("lamp, amount 0: left {fl:.3?}  right {fr:.3?}");
    assert!(
        (fl - fr).abs() < 0.01,
        "with no injection the lamp must not touch the fog at all — {fl:.4} vs {fr:.4}"
    );
}

/// Henyey-Greenstein, normalised so isotropic reads 1 — the same convention the
/// shader uses, and written out here so the two are independent statements of it.
fn hg(cos_t: f32, g: f32) -> f32 {
    let d = 1.0 + g * g - 2.0 * g * cos_t;
    (1.0 - g * g) / d.powf(1.5)
}

// ---- rendering ----------------------------------------------------------------

struct Frame {
    sun: Vec3,
    light: [f32; 3],
    ambient: [f32; 3],
    fog: Fog,
    steps: f32,
    /// A shadow-proxy box laid over the whole view — never drawn, only marched.
    roof: bool,
    /// (camera-relative position, range, colour)
    lamp: Option<([f32; 3], f32, [f32; 3])>,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            sun: Vec3::new(0.0, 1.0, 0.0),
            light: [0.3; 3],
            ambient: [0.0; 3],
            fog: (0.0, 0.0, false),
            steps: 16.0,
            roof: false,
            lamp: None,
        }
    }
}

fn render(gpu: &Gpu, rm: &Raymarch, f: Frame, label: &str) -> Vec<[u8; 4]> {
    // Camera at the world origin looking down -Z, so a centre ray is horizontal
    // and its whole marched span sits at the same height in the layer: the fog's
    // density is then a constant along it, which is what makes the closed form
    // in check 1 a closed form.
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 0.9, near: 0.05, far: 100.0 },
    );
    let vp = cam.view_proj(1.0);

    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    let mut point_count = [0.0f32; 4];
    if let Some((p, range, c)) = f.lamp {
        point_pos[0] = [p[0], p[1], p[2], range];
        point_color[0] = [c[0], c[1], c[2], 0.0];
        point_count[0] = 1.0;
    }

    // The roof: a box proxy over the camera, wide and long enough that every fog
    // step along a centre ray is under it.
    let mut prox_a = [[0.0f32; 4]; 32];
    let mut prox_b = [[0.0f32; 4]; 32];
    prox_a[0] = [0.0, 6.0, -FAR * 0.5, 0.0];
    prox_b[0] = [FAR, 0.5, FAR, 2.0]; // half-extents, kind 2 = box

    let (amount, g, shafts) = f.fog;
    let globals = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_dir: [f.sun.x, f.sun.y, f.sun.z, 0.0],
        light_color: [f.light[0], f.light[1], f.light[2], 0.0],
        // No flat ambient: whatever brightness appears in the air was put there
        // by a light, not by a floor value.
        ambient: [f.ambient[0], f.ambient[1], f.ambient[2], 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        // Equirect sky mode, pointed at the black texture bound above.
        sky_params: [1.0, 1.0, 0.0, 0.0],
        sky_tint: [1.0, 1.0, 1.0, 1.0],
        shadow_params: [1.0, 32.0, 1.0, 150.0],
        prox_count: [if f.roof { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0],
        prox_a,
        prox_b,
        point_count,
        point_pos,
        point_color,
        // Dither off (fog_color.w = 0): a ±6% nudge is invisible in a picture and
        // fatal to a third-decimal comparison.
        fog_color: [FOG_COLOR[0], FOG_COLOR[1], FOG_COLOR[2], 0.0],
        fog_params: [0.0, FAR, 1.0, 0.0],
        // A layer top a kilometre up with no noise: density is exactly DENSITY
        // everywhere the camera can see, so the arithmetic above is exact rather
        // than approximate.
        vol_fog_a: [DENSITY, 1000.0, 1.0, 0.0],
        vol_fog_b: [24.0, 0.0, 0.0, 1.0],
        vol_fog_c: [amount, g, f.steps, if shafts { 1.0 } else { 0.0 }],
        ..Default::default()
    };

    let (tex, view) = target(gpu, label);
    rm.draw_into(gpu, &view, gpu.depth_view(), globals);
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

/// The pixel at fractional position `(fx, fy)`, in LINEAR light — the target is
/// 8-bit sRGB, and comparing those bytes to a linear expectation is a 2.2-power
/// error that reads as "the fog is uniformly too thick".
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
        label: Some("fog-readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fog-readback") });
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
