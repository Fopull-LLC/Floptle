//! Does the glass BEHIND a piece of glass still show?
//!
//! Refraction works by sampling a picture of everything behind the surface, and
//! that picture has to be taken before the surface is drawn. Take it once and
//! you get exactly one correct layer — the nearest. Everything behind that was
//! never in a picture anybody took, so a fish tank's back wall stopped existing
//! the moment you looked through its front one, and the honest advice was "make
//! it one box rather than six".
//!
//! The fix is to draw glass far to near and re-take the picture between groups.
//! This probe measures whether that happened.
//!
//! The scene is a white card, a **green** pane in front of it, and a **clear**
//! pane in front of that. Look through both:
//!
//! - at one layer, the clear pane samples a picture with no green pane in it, so
//!   the overlap comes out white — the green pane is simply gone;
//! - at two, the green pane is drawn and captured first, so the overlap comes
//!   out green.
//!
//! Four checks:
//!
//! 1. **The overlap is green at two layers.** The pane behind survived.
//! 2. **The overlap is NOT green at one.** So the difference is the layering
//!    rather than anything else about the scene, and this probe would have
//!    failed before the change.
//! 3. **Green pane alone reads green in BOTH.** The control: the far pane is
//!    drawing and tinting correctly either way, so check 2's white overlap is
//!    the near pane hiding it and not the far pane being missing.
//! 4. **The bare card reads white in both.** The other control: nothing in this
//!    scene tints anything by itself.
//!
//! Run: cargo run -p floptle-render --example glass_layers_probe -- <out-dir>

use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, SceneHistory, SurfaceExtras, TexId, instance_of_mat, plane,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);

    let two = shot(&gpu, 2, &format!("{dir}/glass_layers_two.png"));
    let one = shot(&gpu, 1, &format!("{dir}/glass_layers_one.png"));

    // The three regions, in frame coordinates. The clear pane covers the LEFT
    // half of the green one, so left = both panes, middle = green only, right =
    // bare card. All three are read from the same row.
    let both = |px: &[u8]| mean_rgb(px, 0.16, 0.30, 0.42, 0.58);
    let green_only = |px: &[u8]| mean_rgb(px, 0.56, 0.68, 0.42, 0.58);
    let bare = |px: &[u8]| mean_rgb(px, 0.86, 0.96, 0.42, 0.58);
    // How green a patch is: green over the mean of the other two. 1 is neutral.
    let greenness = |c: [f32; 3]| c[1] / ((c[0] + c[2]) * 0.5).max(1e-4);

    for (name, px) in [("two layers", &two), ("one layer", &one)] {
        let (b, g, w) = (both(px), green_only(px), bare(px));
        println!(
            "{name:>10}: overlap {:?} (green {:.2})  far-pane-only {:?} (green {:.2})  card {:?}",
            round3(b),
            greenness(b),
            round3(g),
            greenness(g),
            round3(w),
        );
    }

    // 4 + 3, the controls, first: if the card is already tinted or the far pane
    // is not tinting, everything below is measuring the wrong thing.
    for (name, px) in [("two layers", &two), ("one layer", &one)] {
        let w = bare(px);
        assert!(
            greenness(w) < 1.10 && w[1] > 0.25,
            "at {name} the bare card reads {:?} — it is meant to be a plain lit \
             white card, so something in this scene is tinting on its own",
            round3(w)
        );
        let g = green_only(px);
        assert!(
            greenness(g) > 1.40,
            "at {name} the stretch covered by the GREEN pane alone reads {:?} \
             (green {:.2}) — the far pane is not tinting the card, so a white \
             overlap would prove nothing about layering",
            round3(g),
            greenness(g)
        );
    }

    // 1. Two layers: the pane behind the pane survives.
    let b2 = both(&two);
    assert!(
        greenness(b2) > 1.40,
        "looking through BOTH panes at two layers reads {:?} (green {:.2}) — the \
         green pane behind is not reaching the clear pane in front, which is the \
         whole of what a second layer is for",
        round3(b2),
        greenness(b2)
    );

    // 2. One layer: it does not. The control that makes check 1 a measurement.
    let b1 = both(&one);
    assert!(
        greenness(b1) < 1.15,
        "looking through both panes at ONE layer reads {:?} (green {:.2}) — it is \
         supposed to lose the pane behind, so if it does not, check 1 is passing \
         for some reason other than the layering",
        round3(b1),
        greenness(b1)
    );

    println!("glass layers probe OK");
}

fn shot(gpu: &Gpu, layers: u32, out: &str) -> Vec<u8> {
    let mut raster = Raster::new(gpu);
    let card = raster.register(gpu, &plane(1.0), None);
    let pane = raster.register(gpu, &plane(1.0), None);

    // Straight on, camera at the ORIGIN (ADR-0015: the view matrix has no
    // translation, so these positions ARE the shader's coordinates).
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.05, far: 200.0 },
    );
    let vp = cam.view_proj(1.0);

    // A plain white card filling the frame, lit by ambient alone so its
    // brightness is a constant and not a lighting result.
    let card_mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    // `plane(half)` spans [-half, half] — `plane(1.0)` is TWO units across, so
    // every scale below is half the width it looks like.
    let card_at = Mat4::from_translation(Vec3::new(0.0, 0.0, -14.0)) * Mat4::from_scale(Vec3::splat(9.0));

    // The far pane: green glass, over the middle and left of the card.
    let mut green_mp = MaterialParams::flat([0.15, 1.0, 0.15]);
    green_mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness: 0.0,
        metallic: 0.0,
        physical: true,
        reflectivity: 0.0, // no sky in this probe; it is about what comes THROUGH
        transmission: 1.0,
        ior: 1.0, // straight through: this probe measures TINT, not displacement
        thickness: 0.1,
        ..SurfaceExtras::default()
    });
    let green_at =
        Mat4::from_translation(Vec3::new(-1.4, 0.0, -8.0)) * Mat4::from_scale(Vec3::splat(2.6));

    // The near pane: clear glass, over the LEFT half of the green one.
    let mut clear_mp = MaterialParams::flat([1.0, 1.0, 1.0]);
    clear_mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness: 0.0,
        metallic: 0.0,
        physical: true,
        reflectivity: 0.0,
        transmission: 1.0,
        ior: 1.0,
        thickness: 0.1,
        ..SurfaceExtras::default()
    });
    let clear_at =
        Mat4::from_translation(Vec3::new(-1.9, 0.0, -4.0)) * Mat4::from_scale(Vec3::splat(1.5));

    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        (card, None, instance_of_mat(card_at, &card_mp)),
        (pane, None, instance_of_mat(green_at, &green_mp)),
        (pane, None, instance_of_mat(clear_at, &clear_mp)),
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

    // Ambient only: every surface reads its own albedo, so a tint in the picture
    // came from glass rather than from a light landing at an angle.
    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [0.0; 4],
        ambient: [0.6, 0.6, 0.6, 0.0],
        ..Default::default()
    };
    let rm = RaymarchGlobals {
        view_proj: vp.to_cols_array_2d(),
        inv_view_proj: vp.inverse().to_cols_array_2d(),
        light_color: [0.0; 4],
        ambient: [0.6, 0.6, 0.6, 0.0],
        bg: [0.0, 0.0, 0.0, 1.0],
        ..Default::default()
    };

    let mut rmr = Raymarch::new(gpu);
    rmr.upload_globals(gpu, rm);

    // The scene without the glass — the prepass and the colour pass both skip it.
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

    // …then the glass, far to near, re-capturing between layers. Exactly the
    // loop the editor runs, because a probe that ran a tidier one would not be
    // measuring the thing that ships.
    let mut behind = SceneHistory::new(&gpu.device, S, S, gpu.config.format);
    let mut glass_rm = rm;
    glass_rm.ssr_prev_vp = vp.to_cols_array_2d();
    rmr.upload_globals(gpu, glass_rm);
    let cuts = raster.transmissive_cuts(&instances, &[], layers);
    assert_eq!(
        cuts.len() + 1,
        layers as usize,
        "asked for {layers} layer(s) and the two panes were split into {} — the \
         cut finder is not separating them, so this probe would compare a \
         setting against itself",
        cuts.len() + 1
    );
    for layer in 0..=cuts.len() {
        behind.capture(gpu, &view, vp, cam.world_position);
        if layer == 0 {
            rmr.bind_frame_targets(
                gpu,
                raster.prepass_view(),
                Some((behind.view(), behind.sampler())),
            );
        }
        raster.draw_transmissive(
            gpu,
            &view,
            gpu.depth_view(),
            globals,
            &instances,
            &[],
            Some(rmr.field_bind()),
            &cuts,
            layer,
        );
    }

    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    px
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
