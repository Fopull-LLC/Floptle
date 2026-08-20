//! Does frustum culling change the picture? (`floptle/0075`)
//!
//! Run: `cargo run -p floptle-render --release --example cull_probe`
//! (add a path to write the two images: `… --example cull_probe -- cull`)
//!
//! The acceptance criterion for a cull is not "it is faster". A cull that drops
//! something visible is faster and wrong, and the symptom — geometry popping in
//! and out at the screen edge — is far worse than the cost it saved. So this
//! probe renders the same scene twice, submitting everything and then submitting
//! only what [`Frustum::contains_sphere`] accepts, and **asserts the two frames
//! are pixel-identical**.
//!
//! The scene is built to attack exactly that: for each of the six planes there
//! is a pair of props, one just inside and one just outside, plus a deliberately
//! nasty set — a long thin box rotated 45° at the screen edge (the case a
//! `size/2` radius gets wrong), a huge sphere behind the camera whose body pokes
//! into view, and a prop straddling the near plane.
//!
//! It reports the instance count both ways, which is the actual saving, and
//! prints the worst pixel difference so a regression says how bad it is rather
//! than only that it happened.

use floptle_render::{
    Frustum, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection, Raster, RenderCamera,
    cull, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 960;
const H: u32 = 540;
const FOV: f32 = 60.0;
const NEAR: f32 = 0.1;
const FAR: f32 = 300.0;
/// A dark, unmistakably non-default background, so "nothing rendered" cannot be
/// mistaken for "the two frames agree".
const CLEAR: [f64; 4] = [0.04, 0.05, 0.08, 1.0];

/// One prop: where it is (camera-relative), how it is turned, the longest edge
/// of its AABB, and a note for the report.
struct Prop {
    pos: Vec3,
    rot: Quat,
    /// As `ImportedModel::size` measures it — the longest edge of the box.
    size: f32,
    scale: f32,
    /// What this prop is here to catch.
    why: &'static str,
    /// Whether the cull is SUPPOSED to keep it. Written down per prop rather
    /// than inferred, so a change in the radius maths has to disagree with a
    /// stated intention rather than quietly redefine what correct means.
    keep: bool,
}

fn main() {
    let gpu = Gpu::headless(W, H);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cull-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    // A cube of half-extent 0.5, so its AABB's longest edge is exactly 1.0 and
    // `size` in the props below is the model's real measurement rather than a
    // number chosen to make the test pass.
    let cube = raster.register(&gpu, &mesh::cube(0.5), None);

    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::IDENTITY,
        Projection::Perspective { fov_y: FOV.to_radians(), near: NEAR, far: FAR },
    );
    let view_proj = cam.view_proj(W as f32 / H as f32);
    let frustum = Frustum::from_view_proj(view_proj);
    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_dir: [0.3, 0.8, 0.5, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.35, 0.35, 0.4, 0.0],
        ..Default::default()
    };

    let props = scene();
    let mut all: Vec<(MeshId, Option<floptle_render::TexId>, InstanceRaw)> = Vec::new();
    let mut kept: Vec<(MeshId, Option<floptle_render::TexId>, InstanceRaw)> = Vec::new();
    let mut dropped: Vec<&'static str> = Vec::new();
    let mut survived: Vec<&'static str> = Vec::new();
    let mut wrong: Vec<String> = Vec::new();
    for p in &props {
        let m = Mat4::from_scale_rotation_translation(Vec3::splat(p.scale), p.rot, p.pos);
        // A flat colour per prop, so a prop that appears in one image and not the
        // other shows as a solid block rather than a shading difference.
        let mp = MaterialParams::flat([0.85, 0.55, 0.35]);
        let inst = (cube, None, instance_of_mat(m, &mp));
        all.push(inst);
        let r = cull::radius_from_longest_edge(p.size, Vec3::splat(p.scale));
        let keep = frustum.contains_sphere(p.pos, r);
        if keep {
            kept.push(inst);
            survived.push(p.why);
        } else {
            dropped.push(p.why);
        }
        if keep != p.keep {
            wrong.push(format!(
                "{} — expected {}, got {}",
                p.why,
                if p.keep { "kept" } else { "dropped" },
                if keep { "kept" } else { "dropped" }
            ));
        }
    }

    println!("cull probe — {W}x{H}, {FOV}° fov, near {NEAR}, far {FAR}\n");
    println!("  submitted, no cull: {:>3}", all.len());
    println!("  submitted, culled:  {:>3}", kept.len());
    println!("  dropped:            {:>3}\n", dropped.len());
    println!("kept:");
    for w in &survived {
        println!("    · {w}");
    }
    println!("\ndropped:");
    for w in &dropped {
        println!("    · {w}");
    }

    let a = render(&gpu, &mut raster, &view, &color, globals, &all);
    let b = render(&gpu, &mut raster, &view, &color, globals, &kept);
    let worst = a.iter().zip(&b).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0);
    let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();

    if let Some(stem) = std::env::args().nth(1) {
        save_png(&a, &format!("{stem}_all.png"));
        save_png(&b, &format!("{stem}_culled.png"));
        println!("\nwrote {stem}_all.png and {stem}_culled.png — LOOK at them");
    }

    // THE HARNESS CHECK, before the comparison it guards. Two empty frames
    // compare equal, so "identical" is only evidence if there was something in
    // them — a probe that renders nothing passes a pixel-diff perfectly and
    // proves nothing at all. This repo has shipped exactly that mistake before.
    let painted = a.as_chunks::<4>().0.iter().filter(|p| !is_background(*p)).count();
    let coverage = painted as f32 / (W * H) as f32;
    println!("\ngeometry covers {:.1}% of the frame", coverage * 100.0);
    assert!(
        coverage > 0.05,
        "only {:.2}% of the frame is geometry — this probe is not rendering the \
         scene, so the pixel comparison below would prove nothing. Two blank \
         frames are identical.",
        coverage * 100.0
    );

    println!(
        "worst channel difference: {worst}   ({differing} of {} bytes differ)",
        a.len()
    );
    assert!(
        wrong.is_empty(),
        "the cull disagreed with what these props are here to test:\n  {}",
        wrong.join("\n  ")
    );
    assert!(
        dropped.len() >= 6,
        "the cull rejected only {} of {} props — this scene is supposed to have \
         something outside every plane, so either the scene or the test is wrong",
        dropped.len(),
        props.len()
    );
    assert_eq!(
        worst, 0,
        "CULLING CHANGED THE PICTURE. {differing} bytes differ, worst by {worst}. \
         Something visible was dropped — a bounding radius is too small. Run with an \
         output path and compare the two images."
    );
    println!("\nthe cull removed {} props and did not change one pixel.", dropped.len());
}

/// Is this pixel the cleared background rather than a prop?
///
/// Compared with a tolerance because the target may be an sRGB format, so the
/// clear colour does not arrive back as the exact bytes it went in as.
fn is_background(px: &[u8]) -> bool {
    let want = |c: f64| (c.powf(1.0 / 2.2) * 255.0) as i32;
    let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
    (r - want(CLEAR[0])).abs() < 12
        && (g - want(CLEAR[1])).abs() < 12
        && (b - want(CLEAR[2])).abs() < 12
}

/// Props placed to attack each plane, plus the cases a naive radius gets wrong.
///
/// `x` right, `y` up, `-z` forward; the camera is at the origin looking down −Z,
/// so these positions are already camera-relative.
fn scene() -> Vec<Prop> {
    let unit = |pos: Vec3, keep: bool, why: &'static str| Prop {
        pos,
        rot: Quat::IDENTITY,
        size: 1.0,
        scale: 1.0,
        why,
        keep,
    };
    // At 30° half-fov and 20 units out, the frustum is ±11.5 vertically and
    // ±20.5 horizontally. These sit comfortably either side of that.
    vec![
        unit(Vec3::new(0.0, 0.0, -20.0), true, "dead centre, 20 out"),
        unit(Vec3::new(-18.0, 0.0, -20.0), true, "just inside the LEFT plane"),
        unit(Vec3::new(-40.0, 0.0, -20.0), false, "outside the LEFT plane"),
        unit(Vec3::new(18.0, 0.0, -20.0), true, "just inside the RIGHT plane"),
        unit(Vec3::new(40.0, 0.0, -20.0), false, "outside the RIGHT plane"),
        unit(Vec3::new(0.0, -9.0, -20.0), true, "just inside the BOTTOM plane"),
        unit(Vec3::new(0.0, -30.0, -20.0), false, "outside the BOTTOM plane"),
        unit(Vec3::new(0.0, 9.0, -20.0), true, "just inside the TOP plane"),
        unit(Vec3::new(0.0, 30.0, -20.0), false, "outside the TOP plane"),
        unit(Vec3::new(0.0, 0.0, -FAR - 20.0), false, "beyond the FAR plane"),
        unit(Vec3::new(2.0, 0.0, 8.0), false, "behind the camera"),
        // The case a `size/2` radius gets wrong: a long thin box turned 45°, so
        // its corner reaches much further than half its longest edge. Sitting
        // ON the right plane, where an under-sized sphere would cull it and the
        // corner would visibly vanish.
        Prop {
            pos: Vec3::new(19.5, 0.0, -20.0),
            rot: Quat::from_rotation_z(std::f32::consts::FRAC_PI_4)
                * Quat::from_rotation_y(std::f32::consts::FRAC_PI_4),
            size: 1.0,
            scale: 6.0,
            why: "a long box rotated 45° ON the right plane (the radius trap)",
            keep: true,
        },
        // A big prop centred behind the camera whose SPHERE still reaches in
        // front of it. Rejecting on the centre alone would delete it. Kept
        // modest on purpose: at scale 30 it filled the entire frame and the
        // picture stopped being able to show anything else, which is its own
        // kind of useless verification.
        Prop {
            pos: Vec3::new(0.0, 0.0, 6.0),
            rot: Quat::IDENTITY,
            size: 1.0,
            scale: 8.0,
            why: "big, centred BEHIND the camera, sphere reaches in front",
            keep: true,
        },
        // Straddling the near plane — half in front, half behind.
        Prop {
            pos: Vec3::new(0.0, -1.0, -0.05),
            rot: Quat::IDENTITY,
            size: 1.0,
            scale: 1.5,
            why: "straddling the NEAR plane",
            keep: true,
        },
    ]
}

/// Draw a set and read the frame back as raw RGBA.
fn render(
    gpu: &Gpu,
    raster: &mut Raster,
    view: &wgpu::TextureView,
    tex: &wgpu::Texture,
    globals: Globals,
    set: &[(MeshId, Option<floptle_render::TexId>, InstanceRaw)],
) -> Vec<u8> {
    // CLEAR, not Load. Passing `None` here loads whatever was in the target,
    // which on a fresh headless texture is a flat white with no depth — and two
    // blank frames compare equal, so the assertion below would pass while
    // proving nothing. That is the lying-harness failure this repo has hit
    // before; the sanity check in `main` (there must be visible geometry) is
    // what makes it impossible to hit again.
    raster.draw_scene(gpu, view, gpu.depth_view(), globals, set, Some(CLEAR), None);
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    readback(gpu, tex)
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
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
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    pixels
}

fn save_png(pixels: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
