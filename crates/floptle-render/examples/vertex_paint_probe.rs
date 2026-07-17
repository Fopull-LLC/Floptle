//! Headless probe for vertex painting (docs/vertex-paint-proposal.md phase 1).
//!
//! This exists to catch ONE specific bug the design is built around. `params.z` packs
//! the paint base beside the `unlit` bit, but `fs` reads that lane as `> 0.5` — a
//! THRESHOLD, not a bit test. Decode it in the fragment shader (or forget to decode it
//! at all) and every painted node silently renders unlit: the paint looks perfect, and
//! the lighting quietly vanishes. A screenshot of a painted cube would not reveal it.
//!
//! So the load-bearing assertion here is `lit != unlit` — if the packing ever leaks
//! into the fragment stage, the lit render collapses onto the unlit one byte for byte
//! and this fails.
//!
//! Run: cargo run -p floptle-render --example vertex_paint_probe -- <out.png>

use floptle_render::{
    cube, instance_of_mat, Globals, Gpu, InstanceRaw, MaterialParams, MeshData, MeshId,
    Projection, Raster, RenderCamera, TexId,
};
use glam::{Mat3, Mat4, Quat, Vec3};

const S: u32 = 256;

/// Render one cube and read the frame back as RGBA8 (channel-swapped if the surface
/// is BGRA, so the color assertions below can speak in plain RGB).
fn render(gpu: &Gpu, paint: Option<[u8; 4]>, unlit: bool) -> Vec<[u8; 4]> {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vpaint-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vpaint-depth"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(gpu);
    let mut data: MeshData = cube(0.5);
    if let Some(c) = paint {
        data.colors = Some(vec![c; data.vertices.len()]);
    }
    let mesh = raster.register(gpu, &data, None);

    // Material tint is WHITE, so anything colored in the output came from the paint
    // and nowhere else.
    let mat = MaterialParams { unlit, ..MaterialParams::flat([1.0, 1.0, 1.0]) };

    // Off-axis so several faces (with different normals) are visible — a lit cube then
    // has genuinely different per-face brightness.
    let eye = Vec3::new(1.6, 1.2, 1.9);
    let fwd = (Vec3::ZERO - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
    );

    let mut mp = mat;
    mp.paint_base = raster.mesh_paint_base(mesh);
    let raw = instance_of_mat(Mat4::from_translation(-eye), &mp);
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![(mesh, None, raw)];

    let l = Vec3::new(0.5, 0.8, 0.6).normalize();
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        light_dir: [l.x, l.y, l.z, 0.0],
        light_color: [1.0, 0.98, 0.93, 0.0],
        ambient: [0.25, 0.26, 0.30, 0.0],
        ..Default::default()
    };
    raster.draw_scene(
        gpu,
        &color_view,
        &depth_view,
        globals,
        &instances,
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );

    let raw = readback(gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    raw.into_iter()
        .map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p })
        .collect()
}

/// Mean color of the central block — always cube, never background.
fn center(px: &[[u8; 4]]) -> [f32; 3] {
    let (lo, hi) = (S / 2 - 12, S / 2 + 12);
    let mut acc = [0f64; 3];
    let mut n = 0f64;
    for y in lo..hi {
        for x in lo..hi {
            let p = px[(y * S + x) as usize];
            for c in 0..3 {
                acc[c] += p[c] as f64;
            }
            n += 1.0;
        }
    }
    [
        (acc[0] / n) as f32,
        (acc[1] / n) as f32,
        (acc[2] / n) as f32,
    ]
}

/// One unlit cube with a MID-GREY material tint, painted `paint` with the modulate flag
/// set to `modulate`. Mid-grey tint (not white) is the point: it leaves headroom above,
/// so paint that BRIGHTENS is visible. Returns the cube's mean color.
fn render_modulate(gpu: &Gpu, paint: [u8; 4], modulate: bool) -> [f32; 3] {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mod-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mod-depth"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(gpu);
    let data: MeshData = cube(0.5);
    let n = data.vertices.len() as u32;
    let mesh = raster.register(gpu, &data, None);
    let base = raster.paint_alloc(gpu, n, paint);

    // Mid-grey material, unlit: albedo = 0.5 * vcolor. Under modulate, vcolor doubles.
    let mut mp = MaterialParams { unlit: true, ..MaterialParams::flat([0.5, 0.5, 0.5]) };
    mp.paint_base = base;
    mp.paint_modulate = modulate;

    let eye = Vec3::new(0.0, 0.0, 4.0);
    let rot = Quat::IDENTITY;
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
    );
    let raw = instance_of_mat(Mat4::from_translation(-eye), &mp);
    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        ..Default::default()
    };
    raster.draw_scene(
        gpu,
        &color_view,
        &depth_view,
        globals,
        &[(mesh, None, raw)],
        Some([0.0, 0.0, 0.0, 1.0]),
        None,
    );
    let raw = readback(gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px: Vec<[u8; 4]> = raw
        .into_iter()
        .map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p })
        .collect();
    center(&px)
}

/// Two instances of ONE mesh, given DIFFERENT paint blocks. This is the whole point of
/// per-node paint: every primitive of a shape shares a single `MeshId`, so if paint were
/// keyed by mesh these two cubes could not differ. Returns their sampled colors.
fn render_two_painted_instances(gpu: &Gpu) -> ([f32; 3], [f32; 3]) {
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vpaint2-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.surface_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("vpaint2-depth"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(gpu);
    let data: MeshData = cube(0.5);
    let n = data.vertices.len() as u32;
    let mesh = raster.register(gpu, &data, None); // ONE mesh, registered unpainted
    // Two independent blocks — what the brush allocates per node.
    let red = raster.paint_alloc(gpu, n, [255, 0, 0, 255]);
    let blue = raster.paint_alloc(gpu, n, [0, 0, 255, 255]);
    assert_ne!(red, blue, "each node must get its own block");

    let eye = Vec3::new(0.0, 0.5, 5.0);
    let fwd = (Vec3::ZERO - eye).normalize();
    let right = fwd.cross(Vec3::Y).normalize();
    let up = right.cross(fwd);
    let rot = Quat::from_mat3(&Mat3::from_cols(right, up, -fwd));
    let cam = RenderCamera::new(
        eye.as_dvec3(),
        rot,
        Projection::Perspective { fov_y: 0.7, near: 0.02, far: 1000.0 },
    );

    let mk = |x: f32, base: u32| {
        let mut mp = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
        mp.paint_base = base;
        instance_of_mat(Mat4::from_translation(Vec3::new(x, 0.0, 0.0) - eye), &mp)
    };
    // Same MeshId for both → they land in ONE instanced batch, differing only in params.z.
    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> =
        vec![(mesh, None, mk(-1.2, red)), (mesh, None, mk(1.2, blue))];

    let globals = Globals {
        view_proj: cam.view_proj(1.0).to_cols_array_2d(),
        light_dir: [0.0, 1.0, 0.0, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.3, 0.3, 0.3, 0.0],
        ..Default::default()
    };
    raster.draw_scene(gpu, &color_view, &depth_view, globals, &instances, Some([0.0, 0.0, 0.0, 1.0]), None);

    let raw = readback(gpu, &color);
    let bgra = matches!(
        gpu.surface_format(),
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let px: Vec<[u8; 4]> = raw
        .into_iter()
        .map(|p| if bgra { [p[2], p[1], p[0], p[3]] } else { p })
        .collect();
    let at = |cx: u32| {
        let mut acc = [0f64; 3];
        let mut k = 0f64;
        for y in (S / 2 - 8)..(S / 2 + 8) {
            for x in (cx - 8)..(cx + 8) {
                let p = px[(y * S + x) as usize];
                for c in 0..3 {
                    acc[c] += p[c] as f64;
                }
                k += 1.0;
            }
        }
        [(acc[0] / k) as f32, (acc[1] / k) as f32, (acc[2] / k) as f32]
    };
    (at(S / 4), at(S * 3 / 4))
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "vertex_paint.png".into());
    let gpu = Gpu::headless(S, S);

    let red = Some([255, 0, 0, 255]);
    let painted_unlit = render(&gpu, red, true);
    let painted_lit = render(&gpu, red, false);
    let plain_unlit = render(&gpu, None, true);

    let c_pu = center(&painted_unlit);
    let c_pl = center(&painted_lit);
    let c_nu = center(&plain_unlit);
    println!("painted+unlit rgb {c_pu:?}\npainted+lit   rgb {c_pl:?}\nunpainted     rgb {c_nu:?}");

    // 1. Paint reaches the pixels at all. The material tint is white, so a red result
    //    can only have come through the vpaint store.
    assert!(
        c_pu[0] > 100.0 && c_pu[1] < 40.0 && c_pu[2] < 40.0,
        "painted+unlit cube should be RED, got {c_pu:?} — paint never reached the shader"
    );

    // 2. THE TRAP. A painted cube with unlit=false must still be LIT. If the params.z
    //    packing leaks into the fragment stage, `in.params.z > 0.5` is true for every
    //    painted instance and this render becomes byte-identical to the unlit one.
    assert!(
        painted_lit != painted_unlit,
        "painted+lit is byte-identical to painted+unlit — the params.z packing leaked \
         into the fragment stage and silently unlit every painted node (proposal §2.1)"
    );
    assert!(
        c_pl[0] > 20.0 && c_pl[0] < c_pu[0],
        "painted+lit should be red SHADED (dimmer than unlit, not black), got {c_pl:?}"
    );

    // 3. No regression: unpainted geometry still renders as the plain white tint —
    //    paint_base 0 must resolve to the white identity, not black or garbage.
    assert!(
        c_nu[0] > 200.0 && c_nu[1] > 200.0 && c_nu[2] > 200.0,
        "unpainted+unlit cube should be WHITE, got {c_nu:?} — the unpainted path is \
         reading the paint store when it should be returning identity white"
    );

    // 4. PER-NODE paint: two instances of ONE mesh, two blocks, two colors — in a
    //    single instanced batch. If paint were mesh-keyed (or if params.z didn't carry
    //    a per-instance base) these would be identical.
    let (left, right) = render_two_painted_instances(&gpu);
    println!("two instances of ONE mesh: left rgb {left:?}  right rgb {right:?}");
    assert!(
        left[0] > 100.0 && left[2] < 40.0,
        "left instance should be RED, got {left:?}"
    );
    assert!(
        right[2] > 100.0 && right[0] < 40.0,
        "right instance should be BLUE, got {right:?} — two nodes sharing a MeshId must \
         still paint independently (proposal §9.1)"
    );

    // 5. MODULATE 2×: brush paint must paint LIGHT, not only shadow. The regression this
    //    guards is the actual bug report — "if you set it to white nothing shows up".
    let grey_mod = render_modulate(&gpu, [128, 128, 128, 255], true);
    let white_mod = render_modulate(&gpu, [255, 255, 255, 255], true);
    let white_plain = render_modulate(&gpu, [255, 255, 255, 255], false);
    let black_mod = render_modulate(&gpu, [0, 0, 0, 255], true);
    println!(
        "modulate: grey(neutral) {grey_mod:?}  white {white_mod:?}  white-no-modulate {white_plain:?}  black {black_mod:?}"
    );
    // White paint under modulate BRIGHTENS the mid-grey surface well past the plain
    // multiply (which can only ever hold it AT grey — the "nothing shows up" bug).
    assert!(
        white_mod[0] > white_plain[0] + 30.0,
        "white paint must BRIGHTEN under modulate ({white_mod:?}) vs the darken-only \
         multiply ({white_plain:?}) — this is the whole point of Modulate 2×"
    );
    // Mid-grey is the neutral point: it lands on the plain-multiply white value (both are
    // the untouched 0.5 surface), so a fresh block looks unpainted.
    assert!(
        (grey_mod[0] - white_plain[0]).abs() < 20.0,
        "mid-grey paint should be NEUTRAL under modulate ({grey_mod:?} vs {white_plain:?})"
    );
    // And it can still darken — black paint drives the surface toward black.
    assert!(
        black_mod[0] < grey_mod[0] - 30.0,
        "black paint must still DARKEN under modulate ({black_mod:?} vs neutral {grey_mod:?})"
    );

    save_png(&painted_lit, &out);
    println!("all vertex-paint assertions passed; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let bpp = 4u32;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = (S * bpp).div_ceil(align) * align;
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
    gpu.queue.submit(Some(encoder.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");

    let view = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * bpp) as usize;
            out.push([view[i], view[i + 1], view[i + 2], view[i + 3]]);
        }
    }
    drop(view);
    buf.unmap();
    out
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
