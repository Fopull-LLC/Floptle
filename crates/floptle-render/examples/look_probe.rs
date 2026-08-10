//! Headless probe for the LOOK CHAIN — grade, lens, sharpen, denoise, grain and
//! depth of field (`floptle/0130`).
//!
//! Renders one scene into the post input and then runs it through each effect on
//! its own, ASSERTING what each one is supposed to do to the numbers. Every
//! assertion has a CONTROL: the same measurement with the effect off, so "the
//! grade darkened the frame" cannot be satisfied by a chain that darkens
//! everything, and a pass that silently stopped running fails here rather than
//! looking slightly different to a human six weeks later.
//!
//! It also writes a contact sheet, because a number that moved the right way and
//! a picture that looks right are different claims and this engine has been
//! caught by the gap before.
//!
//! Run: cargo run -p floptle-render --example look_probe -- <out-dir>

use floptle_core::transform::Transform;
use floptle_render::{
    instance_of_mat, uv_sphere, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, PostSettings,
    PostStack, Projection, Raster, RenderCamera, SsaoFrame, TexId,
};
use glam::{DVec3, Quat};

const W: u32 = 480;
const H: u32 = 270;

/// Mean linear luminance of the frame.
fn mean_luma(px: &[u8]) -> f32 {
    let mut sum = 0.0;
    for c in px.chunks_exact(4) {
        sum += 0.2126 * c[0] as f32 + 0.7152 * c[1] as f32 + 0.0722 * c[2] as f32;
    }
    sum / (px.len() / 4) as f32
}

/// Mean distance of each pixel from grey — a saturation measure that does not
/// care which hue anything is.
fn mean_chroma(px: &[u8]) -> f32 {
    let mut sum = 0.0;
    for c in px.chunks_exact(4) {
        let (r, g, b) = (c[0] as f32, c[1] as f32, c[2] as f32);
        let m = (r + g + b) / 3.0;
        sum += ((r - m).abs() + (g - m).abs() + (b - m).abs()) / 3.0;
    }
    sum / (px.len() / 4) as f32
}

/// Mean SQUARED difference between horizontally adjacent pixels — how much local
/// detail (or noise) the frame carries. Sharpening raises it, blurring lowers it.
///
/// Squared, and that is load-bearing. Mean *absolute* difference is very nearly
/// CONSERVED when you blur a hard edge: the same total step is just spread over
/// more pixels, so a 10-texel defocus measured that way reads as "no change" —
/// which is exactly what it did, and it looked like depth of field was broken
/// when the circle of confusion was already correct. Squaring makes the measure
/// fall as 1/n with the width the edge is spread over, which is the thing being
/// asked about.
fn local_contrast(px: &[u8], w: u32, h: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for y in 0..h {
        for x in 1..w {
            let i = ((y * w + x) * 4) as usize;
            let j = ((y * w + x - 1) * 4) as usize;
            for k in 0..3 {
                let d = px[i + k] as f32 - px[j + k] as f32;
                sum += d * d;
            }
            n += 3.0;
        }
    }
    sum / n.max(1.0)
}

/// Local contrast inside a rectangle, in 0..1 fractions of the frame.
fn local_contrast_in(px: &[u8], w: u32, h: u32, x0: f32, x1: f32) -> f32 {
    let (a, b) = ((x0 * w as f32) as u32, (x1 * w as f32) as u32);
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    for y in 0..h {
        for x in (a + 1)..b.min(w) {
            let i = ((y * w + x) * 4) as usize;
            let j = ((y * w + x - 1) * 4) as usize;
            for k in 0..3 {
                let d = px[i + k] as f32 - px[j + k] as f32;
                sum += d * d;
            }
            n += 3.0;
        }
    }
    sum / n.max(1.0)
}

/// Mean luminance of the four corners — what a vignette and a barrel distortion
/// both push toward black.
fn corner_luma(px: &[u8], w: u32, h: u32) -> f32 {
    let mut sum = 0.0f32;
    let mut n = 0.0f32;
    let m = 8u32;
    for (cx, cy) in [(0, 0), (w - m, 0), (0, h - m), (w - m, h - m)] {
        for y in cy..cy + m {
            for x in cx..cx + m {
                let i = ((y * w + x) * 4) as usize;
                sum += 0.2126 * px[i] as f32 + 0.7152 * px[i + 1] as f32 + 0.0722 * px[i + 2] as f32;
                n += 1.0;
            }
        }
    }
    sum / n
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(W, H);

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    let sphere = raster.register(&gpu, &uv_sphere(0.7, 24, 32), None);
    let post = PostStack::new(&gpu, W, H);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 8.0),
        Quat::IDENTITY,
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let proj = cam.proj_matrix(W as f32 / H as f32);
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.4, 0.8, 0.5, 0.0],
        light_color: [0.9, 0.9, 0.95, 0.0],
        ambient: [0.10, 0.10, 0.12, 0.0],
        ..Default::default()
    };

    // A saturated near sphere and a saturated far one, so depth of field has two
    // depths to tell apart and the grade has colour to move.
    //
    // Several per depth, spread across the frame: the detail measures below are
    // means over whole columns, and three spheres on a flat background give so
    // little local contrast that the numbers are mostly measuring the
    // background. Near ones on the LEFT third, far ones on the RIGHT third, so
    // depth of field has a side to soften and a side to leave alone.
    let mut setup: Vec<(f64, f64, f64, [f32; 3])> = Vec::new();
    for i in 0..4 {
        let t = i as f64;
        // near cluster, left
        setup.push((-3.6 + t * 0.55, 1.4 - t * 0.9, 3.0, [0.95, 0.25, 0.15]));
        // mid
        setup.push((-0.7 + t * 0.5, -1.2 + t * 0.8, 0.0, [0.20, 0.85, 0.35]));
        // far cluster, right
        setup.push((3.0 + t * 0.8, 1.6 - t * 1.1, -16.0, [0.25, 0.35, 0.95]));
    }
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = setup
        .iter()
        .map(|(x, y, z, c)| {
            let mut m = MaterialParams::flat(*c);
            m.emissive = [c[0] * 0.6, c[1] * 0.6, c[2] * 0.6];
            m.emissive_strength = 0.5;
            let t = Transform::from_translation(DVec3::new(*x, *y, *z));
            (sphere, None, instance_of_mat(t.render_matrix(cam.world_position), &m))
        })
        .collect();

    // One render, reused for every effect: each `post.run` reads the same input
    // target, so the ONLY difference between shots is the settings.
    let redraw = |raster: &mut Raster| {
        raster.draw_scene(
            &gpu,
            post.input_view(),
            gpu.depth_view(),
            globals,
            &instances,
            Some([0.03, 0.03, 0.05, 1.0]),
            None,
        );
    };
    let depth_frame = SsaoFrame {
        depth: gpu.depth_view(),
        proj: proj.to_cols_array_2d(),
        inv_proj: proj.inverse().to_cols_array_2d(),
    };

    let shot = |raster: &mut Raster, name: &str, s: &PostSettings| -> Vec<u8> {
        redraw(raster);
        post.run(&gpu, s, Some(&depth_frame), &color_view);
        let px = read_back(&gpu, &color_tex);
        save_png(&px, W, H, &format!("{dir}/look_{name}.png"));
        px
    };

    // ---- the control: nothing on -------------------------------------------
    let base = PostSettings::default();
    let plain = shot(&mut raster, "plain", &base);
    let l0 = mean_luma(&plain);
    let c0 = mean_chroma(&plain);
    let d0 = local_contrast(&plain, W, H);
    let k0 = corner_luma(&plain, W, H);
    println!("plain:    luma {l0:.2}  chroma {c0:.2}  detail {d0:.2}  corners {k0:.2}");

    // ---- grade --------------------------------------------------------------
    // Exposure is in STOPS, so -1 must be visibly darker and +1 brighter. The
    // pair is the point: a pass that clamped everything to black would pass the
    // first assertion on its own.
    let dark = shot(&mut raster, "grade_dark", &PostSettings { exposure: -1.0, ..base });
    let bright = shot(&mut raster, "grade_bright", &PostSettings { exposure: 1.0, ..base });
    let (ld, lb) = (mean_luma(&dark), mean_luma(&bright));
    println!("exposure: -1 -> {ld:.2}   +1 -> {lb:.2}");
    assert!(ld < l0 * 0.75, "exposure -1 must darken: {ld:.2} vs {l0:.2}");
    assert!(lb > l0 * 1.25, "exposure +1 must brighten: {lb:.2} vs {l0:.2}");

    // Saturation 0 must leave a GREY frame — and leave its brightness roughly
    // where it was, which is the half that a naive `mix(0, c)` gets wrong.
    let grey = shot(&mut raster, "grade_grey", &PostSettings { saturation: 0.0, ..base });
    let cg = mean_chroma(&grey);
    let lg = mean_luma(&grey);
    println!("saturation 0: chroma {cg:.2} (was {c0:.2}), luma {lg:.2} (was {l0:.2})");
    assert!(c0 > 3.0, "the control scene must have real colour in it: {c0:.2}");
    assert!(cg < 1.0, "saturation 0 must be grey: chroma {cg:.2}");
    assert!((lg - l0).abs() < l0 * 0.15, "desaturating must not change brightness much: {lg:.2} vs {l0:.2}");

    // ---- sharpen / denoise ---------------------------------------------------
    // Opposite directions on the same measure, from the same frame.
    let sharp = shot(&mut raster, "sharpen", &PostSettings { sharpen: 1.5, ..base });
    let soft = shot(&mut raster, "denoise", &PostSettings { denoise: 1.0, ..base });
    let (ds, dn) = (local_contrast(&sharp, W, H), local_contrast(&soft, W, H));
    println!("detail:   sharpen {ds:.2}   denoise {dn:.2}   (plain {d0:.2})");
    assert!(ds > d0 * 1.05, "sharpen must raise local contrast: {ds:.2} vs {d0:.2}");
    assert!(dn < d0 * 0.9, "denoise must lower it: {dn:.2} vs {d0:.2}");
    // …but it must still be a DENOISE and not a blur: the frame's real edges
    // have to survive. A 3×3 box blur over this scene lands near 0.05·d0, so the
    // floor here is the assertion that the range weighting is actually running.
    assert!(dn > d0 * 0.2, "denoise must preserve edges, not just blur: {dn:.2} vs {d0:.2}");

    // ---- lens ----------------------------------------------------------------
    // Barrel distortion pushes the picture off its own corners, so the corners
    // go black. Asserted against the plain frame's corners, which are dark but
    // not zero.
    let barrel = shot(&mut raster, "lens_barrel", &PostSettings { distortion: 0.45, ..base });
    let kb = corner_luma(&barrel, W, H);
    println!("corners:  barrel {kb:.2}   (plain {k0:.2})");
    assert!(kb < k0 * 0.6 + 0.01, "barrel distortion must empty the corners: {kb:.2} vs {k0:.2}");

    // Chromatic aberration splits the channels APART at the edges and leaves the
    // centre alone — that difference is the whole effect, and measuring only the
    // whole frame would let a global tint pass.
    let ca = shot(&mut raster, "lens_aberration", &PostSettings { aberration: 1.5, ..base });
    let edge_ca = local_contrast_in(&ca, W, H, 0.0, 0.18);
    let edge_0 = local_contrast_in(&plain, W, H, 0.0, 0.18);
    let mid_ca = local_contrast_in(&ca, W, H, 0.42, 0.58);
    let mid_0 = local_contrast_in(&plain, W, H, 0.42, 0.58);
    println!("aberration: edge {edge_0:.2}->{edge_ca:.2}   centre {mid_0:.2}->{mid_ca:.2}");
    assert!(
        (mid_ca - mid_0).abs() < mid_0 * 0.15 + 0.5,
        "aberration must leave the CENTRE alone: {mid_ca:.2} vs {mid_0:.2}"
    );

    // ---- grain ---------------------------------------------------------------
    // Grain adds high-frequency detail; a static image gains local contrast. The
    // control that matters is that it is NOT a global brightness change.
    let grainy =
        shot(&mut raster, "grain", &PostSettings { grain: 0.6, grain_size: 2.0, time: 1.0, ..base });
    let dg = local_contrast(&grainy, W, H);
    let lgr = mean_luma(&grainy);
    println!("grain:    detail {dg:.2} (was {d0:.2})   luma {lgr:.2} (was {l0:.2})");
    assert!(dg > d0 * 1.1, "grain must add high-frequency detail: {dg:.2} vs {d0:.2}");
    assert!(
        (lgr - l0).abs() < l0 * 0.12,
        "grain is multiplicative about the midtones — it must not lift the whole frame: {lgr:.2} vs {l0:.2}"
    );

    // ---- depth of field ------------------------------------------------------
    // The scene's near cluster sits at view depth 5 and its far cluster at 24
    // (camera at z = +8; spheres at z = +3 and z = -16). Focus on 24: the near
    // cluster must lose its detail and the far cluster must keep it. BOTH halves
    // are asserted, because a pass that simply blurred the whole frame would
    // satisfy the first one on its own.
    let dof = shot(
        &mut raster,
        "dof_far",
        &PostSettings { dof_focus: 24.0, dof_range: 3.0, dof_max_blur: 10.0, ..base },
    );
    let near_0 = local_contrast_in(&plain, W, H, 0.02, 0.35);
    let near_d = local_contrast_in(&dof, W, H, 0.02, 0.35);
    let far_0 = local_contrast_in(&plain, W, H, 0.62, 0.98);
    let far_d = local_contrast_in(&dof, W, H, 0.62, 0.98);
    // Measured as the FRACTION of detail each side keeps, not as an absolute.
    // The right-hand window is mostly background, and the background is at the
    // far plane, so it is genuinely out of focus and genuinely does blur — an
    // absolute "the far side must stay above X" would be asking depth of field
    // not to work. What distinguishes a real CoC from a global blur is that the
    // side at the focus distance keeps a much larger share of what it had.
    let near_keep = near_d / near_0.max(1e-3);
    let far_keep = far_d / far_0.max(1e-3);
    println!(
        "dof(far): near {near_0:.2}->{near_d:.2} ({:.0}% kept)   far {far_0:.2}->{far_d:.2} ({:.0}% kept)",
        near_keep * 100.0,
        far_keep * 100.0
    );
    assert!(near_keep < 0.5, "the out-of-focus NEAR cluster must soften: kept {:.0}%", near_keep * 100.0);
    assert!(
        far_keep > near_keep * 3.0,
        "the side AT the focus distance must keep far more of its detail than the side that is \
         out of focus — otherwise this is a global blur, not depth of field (far kept {:.0}%, \
         near kept {:.0}%)",
        far_keep * 100.0,
        near_keep * 100.0
    );

    // …and the mirror image. Focusing NEAR must swap which one is soft, which is
    // the assertion that proves the depth is being read rather than a screen
    // position.
    let dof_near = shot(
        &mut raster,
        "dof_near",
        &PostSettings { dof_focus: 5.0, dof_range: 2.0, dof_max_blur: 10.0, ..base },
    );
    let far_n = local_contrast_in(&dof_near, W, H, 0.62, 0.98);
    let near_n = local_contrast_in(&dof_near, W, H, 0.02, 0.35);
    println!(
        "dof(near): near kept {:.0}%   far kept {:.0}%",
        near_n / near_0.max(1e-3) * 100.0,
        far_n / far_0.max(1e-3) * 100.0
    );
    assert!(
        near_n / near_0.max(1e-3) > near_keep * 3.0,
        "moving the focus to the NEAR cluster must un-blur it — this is what proves the pass \
         reads depth and not screen position (kept {:.0}%, was {:.0}%)",
        near_n / near_0.max(1e-3) * 100.0,
        near_keep * 100.0
    );
    assert!(
        far_n / far_0.max(1e-3) < far_keep,
        "…and must blur the FAR cluster more than focusing on it did ({:.0}% vs {:.0}%)",
        far_n / far_0.max(1e-3) * 100.0,
        far_keep * 100.0
    );

    // ---- the identity --------------------------------------------------------
    // Nothing on must be byte-identical to nothing on. This is what stops a new
    // pass being added that quietly runs at its default and costs every project
    // a full-screen read for no change at all.
    let again = shot(&mut raster, "plain2", &base);
    assert_eq!(plain, again, "the default settings must be a stable identity");

    println!("\nall look-chain assertions passed — sheets in {dir}/look_*.png");
}

fn read_back(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = 4u32;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = (W * bpp).div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut out = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        out.extend_from_slice(&data[row..row + (W * bpp) as usize]);
    }
    drop(data);
    buf.unmap();
    // The swapchain format may be Bgra — normalise so the measurements above
    // are talking about the channels they name.
    if matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb)
    {
        for px in out.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    out
}

fn save_png(px: &[u8], w: u32, h: u32, path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(px).unwrap();
}
