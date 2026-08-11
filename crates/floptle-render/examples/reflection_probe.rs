//! Do surfaces actually reflect the sky?
//!
//! Until this landed the renderer had no environment term at all: the
//! metal-rough model computed a correct specular lobe and had nothing to put in
//! it but the sun, so a mirror came out as a sun dot on black. That is a
//! failure you cannot see in a screenshot of a shiny thing — a strong specular
//! highlight looks like a reflection to the eye — so the probe measures the one
//! thing a highlight cannot fake: that the surface takes on the COLOUR of the
//! sky it is pointed at, and a different colour when the sky changes.
//!
//! Four checks, each closing a way this could look right and be wrong:
//!
//! 1. **A mirror reflects the sky's colour.** Under a strongly coloured sky, a
//!    metallic roughness-0 sphere reads that colour. A sun highlight is white
//!    and would not.
//! 2. **It reflects THIS sky, not a remembered one.** Change the sky, capture
//!    again, and the sphere changes with it. This is what catches a capture
//!    that ran once, or a stale pipeline after a Sky shader is spliced in.
//! 3. **Roughness blurs it.** A rough sphere and a mirror sphere under the same
//!    sky must differ — otherwise the mip chain is decorative.
//! 4. **`reflectivity = 0` turns it off**, so the control is real.
//!
//! Run: cargo run -p floptle-render --example reflection_probe -- <out-dir>

use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster,
    RaymarchGlobals, Raymarch, RenderCamera, SurfaceExtras, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 192;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let mut rm = Raymarch::new(&gpu);
    let mesh = raster.register(&gpu, &uv_sphere(1.0, 48, 64), None);

    // A vividly coloured sky, and a second one nothing like it. Both are the
    // "solid vault" path (no skybox image, no sky shader), which is the one
    // every scene has by default.
    let teal = [0.0f32, 0.55, 0.75];
    let orange = [0.85f32, 0.35, 0.0];

    let mirror = shot(&gpu, &mut raster, &mut rm, mesh, teal, 0.0, 1.0, &format!("{dir}/reflect_mirror.png"));
    let other_sky =
        shot(&gpu, &mut raster, &mut rm, mesh, orange, 0.0, 1.0, &format!("{dir}/reflect_other_sky.png"));
    let rough =
        shot(&gpu, &mut raster, &mut rm, mesh, teal, 0.85, 1.0, &format!("{dir}/reflect_rough.png"));
    let off = shot(&gpu, &mut raster, &mut rm, mesh, teal, 0.0, 0.0, &format!("{dir}/reflect_off.png"));

    println!(
        "mirror rgb [{:.3}, {:.3}, {:.3}] spread {:.4}\n\
         other  rgb [{:.3}, {:.3}, {:.3}] spread {:.4}\n\
         rough  rgb [{:.3}, {:.3}, {:.3}] spread {:.4}\n\
         off    rgb [{:.3}, {:.3}, {:.3}] spread {:.4}",
        mirror.rgb[0], mirror.rgb[1], mirror.rgb[2], mirror.spread,
        other_sky.rgb[0], other_sky.rgb[1], other_sky.rgb[2], other_sky.spread,
        rough.rgb[0], rough.rgb[1], rough.rgb[2], rough.spread,
        off.rgb[0], off.rgb[1], off.rgb[2], off.spread,
    );

    // 1. The mirror takes the sky's HUE. Teal means blue+green well above red;
    //    a white sun highlight would have all three equal.
    let m = mirror.rgb;
    assert!(
        m[2] > m[0] + 0.05 && m[1] > m[0] + 0.03,
        "a mirror under a TEAL sky should read teal, got {m:?} — that is a \
         highlight, not a reflection"
    );

    // 2. …and it is THIS sky. Under the orange sky the balance must invert.
    let o = other_sky.rgb;
    assert!(
        o[0] > o[2] + 0.05,
        "under an ORANGE sky the mirror still reads {o:?} — the capture is stale, \
         so every scene would reflect whatever sky happened to be captured first"
    );

    // 3. Roughness BLURS it. Both spheres reflect the same sky, so their colours
    //    are close; what has to differ is how much the reflection VARIES across
    //    the surface. The vault's stars are the fine detail a blur destroys, and
    //    measuring their disappearance is a test of the mip chain itself rather
    //    than of the BRDF's roughness term (which would differ even if the chain
    //    were never sampled).
    assert!(
        mirror.spread > rough.spread * 1.3,
        "a rough sphere shows as much detail as a mirror ({:.4} vs {:.4}) — \
         roughness is not reaching a blurrier level, so the chain is decoration",
        rough.spread,
        mirror.spread
    );

    // 4. The control is real, and it is the reflection it removes.
    assert!(
        off.rgb[2] < m[2] * 0.25,
        "reflectivity = 0 still reflects ({:?} vs {m:?})",
        off.rgb
    );

    println!("reflection probe OK");
}

/// One sphere under one sky, returning the mean colour of the sphere's own
/// pixels (the background is excluded by construction — see below).
#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &mut Raymarch,
    mesh: MeshId,
    sky: [f32; 3],
    roughness: f32,
    reflectivity: f32,
    out: &str,
) -> Shot {
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 45f32.to_radians(), near: 0.05, far: 100.0 },
    );
    let vp = cam.view_proj(1.0);
    // The sun points AWAY from the camera, so its highlight lands on the far
    // side of the sphere and cannot be mistaken for the reflection we measure.
    let light = Vec3::new(0.0, 0.0, -1.0);

    // The raymarch globals carry the sky. `sky_params.x < 0.5` selects the
    // built-in vault, whose void colour is `bg` — a flat, known sky.
    let mut rmg = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
        bg: [sky[0], sky[1], sky[2], 1.0],
        ..Default::default()
    };
    rmg.params[0] = 0.0; // time
    rm.upload_globals(gpu, rmg);
    // THE capture. Without it the environment map holds whatever was there
    // before, which is check 2's whole point.
    rm.capture_env(gpu);

    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        // No ambient: anything the sphere shows is the reflection or the sun.
        ambient: [0.0, 0.0, 0.0, 0.0],
        ..Default::default()
    };

    // A SILVER metal. It has to be a bright one: for a metal, albedo IS `f0` —
    // its reflectance — so a black metal reflects nothing by definition. An
    // earlier version of this probe used one and measured only the grazing
    // sheen the analytic BRDF adds, which looked like a passing test and proved
    // nothing about the environment at all.
    //
    // With no ambient and the sun pointing away from the camera, a metal has no
    // diffuse and no visible highlight, so everything on the sphere's lit face
    // arrives from the environment.
    let mut mp = MaterialParams::flat([0.9, 0.9, 0.9]);
    mp.alpha = 1.0;
    mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness,
        metallic: 1.0,
        physical: true,
        reflectivity,
        ..SurfaceExtras::default()
    });
    // CAMERA-RELATIVE (ADR-0015): the view matrix carries no translation, so an
    // instance's model translation IS its position relative to the eye. A model
    // at the origin sits AT the camera — which put the eye inside the sphere and
    // quietly rendered its interior.
    let m = Mat4::from_translation(Vec3::new(0.0, 0.0, -4.0));
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        vec![(mesh, None, instance_of_mat(m, &mp))];

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
    // Clear to pure red: the sphere is never red here, so "not the background"
    // is an exact test rather than a threshold.
    raster.draw_scene_with(
        gpu,
        &view,
        gpu.depth_view(),
        globals,
        &instances,
        &[],
        &[],
        Some([1.0, 0.0, 0.0, 1.0]),
        Some(rm.field_bind()),
    );

    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    let mut sum = [0f64; 3];
    let mut lum: Vec<f32> = Vec::new();
    for p in px.chunks_exact(4) {
        // The background is (255, 0, 0); anything with green or blue in it, or
        // with red below full, is the sphere.
        if p[0] == 255 && p[1] == 0 && p[2] == 0 {
            continue;
        }
        for c in 0..3 {
            sum[c] += p[c] as f64 / 255.0;
        }
        lum.push((p[0] as f32 + p[1] as f32 + p[2] as f32) / (3.0 * 255.0));
    }
    let n = lum.len();
    assert!(n > 500, "the sphere barely covered the frame ({n} px) — check the framing");
    let mean = (lum.iter().sum::<f32>()) / n as f32;
    let spread =
        (lum.iter().map(|l| (l - mean) * (l - mean)).sum::<f32>() / n as f32).sqrt();
    Shot {
        rgb: [
            (sum[0] / n as f64) as f32,
            (sum[1] / n as f64) as f32,
            (sum[2] / n as f64) as f32,
        ],
        spread,
    }
}

/// What one render tells us: the sphere's mean colour, and how much its
/// brightness VARIES across it — which is what a blur destroys.
struct Shot {
    rgb: [f32; 3],
    spread: f32,
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
