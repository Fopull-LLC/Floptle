//! **How small a sorting step the depth buffer can actually keep apart.**
//!
//! Run: `cargo run -p floptle-render --release --example sort_precision_probe`
//!
//! Sorting layers resolve to a Z nudge on the drawn transform — one
//! [`floptle_core::SORT_LAYER_STEP`] per layer, one
//! [`floptle_core::SORT_ORDER_STEP`] per `order` within it. For an OPAQUE
//! surface that nudge is settled by the depth buffer, and under an orthographic
//! camera the depth buffer spans `±ORTHO_DEPTH` — ten thousand world units each
//! way — so its resolution near the play plane is coarse in world terms even
//! though the format is `Depth32Float`.
//!
//! Two constants that were each chosen sensibly on their own therefore need
//! checking against each other, which is what this does: it draws two
//! overlapping opaque quads whose Z differs by a swept amount, reads the pixel
//! where they overlap, and reports the smallest difference that still puts the
//! right one in front — **both ways round**, because a step that always shows
//! the same quad has not been resolved, it has merely been drawn second.
//!
//! It prints the answer against the two constants and writes
//! `target/sort_precision_probe.png` showing the sweep, because a number that
//! disagrees with a picture is usually the number that is wrong.

use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, Projection, Raster, RenderCamera, instance_of_mat,
    mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 512;
const H: u32 = 512;

/// The world height an orthographic 2D camera covers — a 240-row design at
/// 32-pixel tiles is about this many units tall.
const ORTHO_HEIGHT: f32 = 15.0;

fn main() {
    let gpu = Gpu::headless(W, H);
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sort-precision"),
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
    let quad = raster.register(&gpu, &mesh::plane(0.5), None);

    // The camera the engine builds for a 2D scene — including the ±ORTHO_DEPTH
    // range, which is the whole subject here. Asking `of_camera` rather than
    // building a projection by hand is the point: a probe that used a tighter
    // depth range would measure a precision the engine never has.
    const CAM_Z: f32 = 10.0;
    let proj = Projection::of_camera(60f32.to_radians(), true, ORTHO_HEIGHT, 0.05, 500.0);
    let cam = RenderCamera::new(DVec3::new(0.0, 0.0, CAM_Z as f64), Quat::IDENTITY, proj);
    let globals = Globals {
        // Camera-relative (ADR-0015) — every model matrix below has CAM_Z taken
        // out of it for the same reason.
        view_proj: cam.view_proj(W as f32 / H as f32).to_cols_array_2d(),
        light_dir: [0.0, 0.0, 1.0, 0.0],
        light_color: [0.0, 0.0, 0.0, 0.0],
        ambient: [1.0, 1.0, 1.0, 0.0],
        ..Default::default()
    };

    // Both quads are UNLIT and fully opaque, so the only thing deciding the
    // pixel is depth. A blended surface would be settled by draw order instead
    // and would answer a different question.
    let flat = |rgb: [f32; 3]| {
        let mut mp = MaterialParams::flat(rgb);
        mp.unlit = true;
        mp
    };
    let back = flat([0.85, 0.25, 0.2]);
    let front = flat([0.25, 0.8, 0.35]);

    // The sweep: from a whole sorting layer down past a single order step.
    let layer = floptle_core::SORT_LAYER_STEP;
    let order = floptle_core::SORT_ORDER_STEP;
    let steps: Vec<(String, f32)> = vec![
        ("1 layer".into(), layer),
        ("1/2 layer".into(), layer * 0.5),
        ("8 orders".into(), order * 8.0),
        ("4 orders".into(), order * 4.0),
        ("2 orders".into(), order * 2.0),
        ("1 order".into(), order),
        ("1/2 order".into(), order * 0.5),
        ("1/4 order".into(), order * 0.25),
    ];

    println!("orthographic camera, height {ORTHO_HEIGHT}, depth ±{}", floptle_render::ORTHO_DEPTH);
    println!("  SORT_LAYER_STEP = {layer}  SORT_ORDER_STEP = {order}\n");
    println!("  {:<12} {:>12}   verdict", "step", "world Z");

    let mut smallest_ok: Option<(String, f32)> = None;
    let mut shot: Vec<(floptle_render::MeshId, Option<floptle_render::TexId>, InstanceRaw)> =
        Vec::new();

    for (i, (name, dz)) in steps.iter().enumerate() {
        // Where this pair sits in the picture — a column per step, so the PNG
        // reads left to right in the same order as the printout.
        let x = (i as f32 - (steps.len() as f32 - 1.0) * 0.5) * 1.6;

        // Drawn back-first and then front-first. If BOTH orders put the same
        // colour on top, the depth test decided; if the top colour follows the
        // draw order, it did not and we are looking at submission order.
        let a = resolves(&gpu, &mut raster, &view, &color, globals, quad, x, *dz, back, front, true);
        let b = resolves(&gpu, &mut raster, &view, &color, globals, quad, x, *dz, back, front, false);
        let ok = a && b;
        println!(
            "  {name:<12} {dz:>12.7}   {}",
            if ok { "separated" } else { "TIED — the nearer quad did not win both ways" }
        );
        if ok {
            smallest_ok = Some((name.clone(), *dz));
        }

        // …and one instance pair per column for the picture.
        for (dz_i, mp) in [(0.0, back), (*dz, front)] {
            let t = Mat4::from_scale_rotation_translation(
                Vec3::new(1.4, 1.4, 1.0),
                Quat::IDENTITY,
                // The two overlap by half a quad, so the seam between them is
                // where the answer is visible.
                Vec3::new(x + dz_i.signum() * 0.35, dz_i.signum() * -0.35, dz_i - CAM_Z),
            );
            shot.push((quad, None, instance_of_mat(t, &mp)));
        }
    }

    println!();
    match &smallest_ok {
        Some((name, dz)) => println!("smallest step the depth buffer separates: {name} ({dz})"),
        None => println!("NOTHING in the sweep separated — check the probe before the engine"),
    }
    if let Some((_, dz)) = &smallest_ok
        && *dz > order
    {
        println!(
            "\n⚠ `order` steps of 1 are FINER than the depth buffer resolves here.\n\
             Two nodes one order apart on the same layer are settled by submission\n\
             order, not by what the author asked for."
        );
    }

    raster.draw_scene(
        &gpu,
        &view,
        gpu.depth_view(),
        globals,
        &shot,
        Some([0.06, 0.07, 0.09, 1.0]),
        None,
    );
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let out = "target/sort_precision_probe.png";
    save_png(&gpu, &color, out);
    println!("wrote {out}");
}

/// Draw one overlapping pair and report whether the nearer quad won the overlap.
///
/// `front_last` submits the nearer quad second; calling it both ways is what
/// separates "the depth test resolved this" from "the last one drawn won".
#[allow(clippy::too_many_arguments)]
fn resolves(
    gpu: &Gpu,
    raster: &mut Raster,
    view: &wgpu::TextureView,
    target: &wgpu::Texture,
    globals: Globals,
    quad: floptle_render::MeshId,
    x: f32,
    dz: f32,
    back: MaterialParams,
    front: MaterialParams,
    front_last: bool,
) -> bool {
    const CAM_Z: f32 = 10.0;
    let at = |z: f32, mp: MaterialParams| {
        let t = Mat4::from_scale_rotation_translation(
            Vec3::splat(1.4),
            Quat::IDENTITY,
            Vec3::new(x, 0.0, z - CAM_Z),
        );
        (quad, None, instance_of_mat(t, &mp))
    };
    let mut items = vec![at(0.0, back), at(dz, front)];
    if !front_last {
        items.reverse();
    }
    // Cleared every time: `None` would mean LoadOp::Load and this would be
    // measured against the previous pair's depth.
    raster.draw_scene(gpu, view, gpu.depth_view(), globals, &items, Some([0.0, 0.0, 0.0, 1.0]), None);
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    // The pixel where both quads are — dead centre of the column.
    let px = ((x / (ORTHO_HEIGHT * W as f32 / H as f32) + 0.5) * W as f32) as u32;
    let c = read_pixel(gpu, target, px.min(W - 1), H / 2);
    // Green in front means the nearer quad won.
    c[1] > c[0]
}

fn read_pixel(gpu: &Gpu, tex: &wgpu::Texture, x: u32, y: u32) -> [u8; 4] {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("pixel"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pixel") });
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
    let at = (y * padded + x * bpp) as usize;
    [data[at], data[at + 1], data[at + 2], data[at + 3]]
}

fn save_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
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
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&pixels).unwrap();
}
