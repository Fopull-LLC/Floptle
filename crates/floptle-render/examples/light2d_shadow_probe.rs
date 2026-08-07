//! **2D shadows and falloff shaping** (`floptle/0125`, `floptle/0126`).
//!
//! A flat grey floor under an orthographic camera, one 2D light off to the left,
//! and a one-tile-wide wall standing between the light and the right-hand side.
//!
//! Before this the wall did nothing at all. The `blocks light` control existed,
//! validated its three values, round-tripped through the scene file and
//! explained itself in the Inspector — and no part of the renderer read it, so a
//! light lit the floor, the wall, and the floor behind the wall by the same
//! distance function. What reached the screen was a disc of brightness with the
//! room drawn on top of it, which is a decal. Light is what you get when
//! something *interrupts* it.
//!
//! Five shots, each one a claim:
//!
//! * `shadow_off`  — the wall set not to cast. The far side is lit. This is the
//!   reference, and it is also what every release before this one did.
//! * `shadow_on`   — the wall casting. The far side drops to the base light and
//!   the near side does not move.
//! * `shadow_mask` — the wall on a sorting layer this light does not reach. It
//!   must not shadow either: a light that skips a background and is *blocked* by
//!   it would be the worst of both.
//! * `falloff_flat` — an inner radius of 0.9 × range. The ramp now lives in the
//!   outer tenth, which is how a posterized game lands a whole light inside one
//!   band instead of drawing concentric rings.
//! * `falloff_steep` — an exponent of 5, the other direction.
//!
//! The numbers say *whether* the light stopped. Only the pictures say whether it
//! reads as a wall with a dark side, which is the thing that was actually asked
//! for — so it writes all five.
//!
//! Run: cargo run -p floptle-render --example light2d_shadow_probe -- <outdir>

use floptle_render::{
    Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, MeshId, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
/// World units per screen pixel's reciprocal: the camera frames 16 units over
/// 256 pixels, so one unit is 16 px and world x = 0 is screen x = 128.
const PX_PER_UNIT: f32 = S as f32 / ORTHO_HEIGHT;
const MAP_RANK: u32 = 1;
/// The rank the masked shot puts the wall on — one the light does not name.
const OTHER_RANK: u32 = 2;
const RANGE: f32 = 12.0;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    // Flat mid-grey, so every difference between the shots is the lighting.
    let n = 8u32;
    let pixels: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(
        &gpu,
        &TextureData { pixels, width: n, height: n },
        TexSampling::default(),
    );

    let floor = {
        let data: Vec<u32> = (0..16 * 16).map(|_| 0).collect();
        raster.register(&gpu, &mesh::tilemap(16, 16, 1.0, 1, 1, [0.0, 0.0], &data), None)
    };
    // One column, sixteen rows: a wall from the top of the frame to the bottom.
    let wall_mesh = {
        let data: Vec<u32> = (0..16).map(|_| 0).collect();
        raster.register(&gpu, &mesh::tilemap(1, 16, 1.0, 1, 1, [0.0, 0.0], &data), None)
    };

    let unlit = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let floor_raw = instance_of_mat(Mat4::IDENTITY, &unlit);
    // A hair closer to the camera than the floor. Both are flat and coplanar
    // otherwise, and the fill pass depth-tests `Less` — so without this the wall
    // would lose to whichever of them drew first and never reach the G-buffer at
    // all. (Which would make this probe pass for the wrong reason: no caster.)
    let wall_raw = instance_of_mat(Mat4::from_translation(Vec3::new(0.0, 0.0, 0.01)), &unlit);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.0, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);

    // The light sits four units to the LEFT of the wall, so the wall's shadow
    // falls across the whole right-hand side of the frame.
    let base = |inner: f32, exponent: f32| {
        let mut u = Light2dUniform {
            count: [1.0, 0.0, 0.0, 0.0],
            // A dim base, so "reached by no light" is visibly dark rather than
            // black — the same choice the engine makes.
            ambient: [0.28, 0.28, 0.32, 0.0],
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            ..Default::default()
        };
        u.pos[0] = [-4.0, 0.0, 0.0, RANGE];
        u.color[0] = [1.5, 1.4, 1.2, 0.0];
        u.mask[0] = [1 << MAP_RANK, 0, 0, 0];
        u.falloff[0] = [inner, exponent, 1.0, 0.0];
        u
    };
    let lights = base(0.0, 2.0);

    // Screen columns to sample, at the vertical middle. Both are floor, one on
    // each side of the wall, and both are inside the light's radius.
    let col = |world_x: f32| (S as f32 * 0.5 + world_x * PX_PER_UNIT) as u32;
    let near = col(-2.0); // between the light and the wall
    let far = col(3.0); //  behind the wall

    let flat = |wall: Option<(bool, u32)>| {
        let mut v: Vec<(MeshId, Option<TexId>, Light2dInstance)> =
            vec![(floor, Some(tex), Light2dInstance::from_raster(&floor_raw, MAP_RANK, false))];
        if let Some((casts, rank)) = wall {
            v.push((wall_mesh, Some(tex), Light2dInstance::from_raster(&wall_raw, rank, casts)));
        }
        v
    };

    let take = |name: &str,
                    raster: &mut Raster,
                    wall: Option<(bool, u32)>,
                    l: &Light2dUniform|
     -> (f32, f32) {
        let f = flat(wall);
        let px = shot(&gpu, raster, view_proj, &floor_raw, &wall_raw, wall.is_some(), floor,
                      wall_mesh, tex, &f, l);
        let (a, b) = (luma(&px, near, S / 2), luma(&px, far, S / 2));
        let out = format!("{dir}/light2d_{name}.png");
        save_png(&px, &out);
        println!("{name}: near {a:.3}, far {b:.3} — wrote {out}");
        (a, b)
    };

    let off = take("shadow_off", &mut raster, Some((false, MAP_RANK)), &lights);
    let on = take("shadow_on", &mut raster, Some((true, MAP_RANK)), &lights);
    let masked = take("shadow_mask", &mut raster, Some((true, OTHER_RANK)), &lights);
    // No wall at all, to read what "the base light and nothing else" measures as
    // out at the far column — the number the shadowed side has to land on.
    let dark = {
        let mut l = lights;
        l.count[0] = 0.0;
        take("shadow_none", &mut raster, None, &l)
    };

    // ---- the claim ---------------------------------------------------------
    assert!(
        off.1 > dark.1 + 0.1,
        "the far side was not lit even with the wall set NOT to cast ({:.3} against the \
         unlit {:.3}) — this probe cannot tell you anything about shadows if the light \
         never reached there in the first place",
        off.1,
        dark.1
    );
    assert!(
        (on.1 - dark.1).abs() < 0.03,
        "the far side is at {:.3} where the base light alone measures {:.3}: the wall did \
         not stop the light",
        on.1,
        dark.1
    );
    assert!(
        (on.0 - off.0).abs() < 0.03,
        "the NEAR side moved when the wall started casting ({:.3} → {:.3}). The lit side of \
         a wall is lit; only what is behind it goes dark.",
        off.0,
        on.0
    );
    // The mask governs the occluder as well as the receiver.
    assert!(
        (masked.1 - off.1).abs() < 0.03,
        "a caster on a layer this light does not reach still shadowed it: {:.3} against \
         {:.3} with no caster at all. A light that skips a background and is BLOCKED by it \
         is the worst of both.",
        masked.1,
        off.1
    );

    // ---- falloff shaping (`floptle/0126`) ----------------------------------
    //
    // The far column is six units from the light against a range of twelve, so
    // the default ramp has it at a quarter brightness. `inner` and the exponent
    // are how an author *shapes* a light — a hard pool of light with a defined
    // edge, or a soft one that reaches further.
    //
    // They were also, briefly, the recommended way to dodge posterize banding:
    // squash the whole falloff inside one band and it cannot draw rings. That
    // recommendation is withdrawn (`floptle/0127`) — it replaced N soft rings
    // with one hard disc edge, and the edge survived turning posterize off,
    // which is how you know a workaround was never the fix. The knobs stay
    // because shaping a light is a real thing to want.
    let flat_ramp = take("falloff_flat", &mut raster, Some((false, MAP_RANK)),
                         &base(RANGE * 0.9, 2.0));
    let steep = take("falloff_steep", &mut raster, Some((false, MAP_RANK)), &base(0.0, 5.0));
    assert!(
        flat_ramp.1 > off.1 + 0.1,
        "an inner radius did not flatten the core: {:.3} against the default ramp's {:.3}. \
         A light whose falloff cannot be shaped is one shape of light.",
        flat_ramp.1,
        off.1
    );
    assert!(
        steep.1 < off.1 - 0.05,
        "a steeper exponent did not dive away from the core: {:.3} against {:.3}",
        steep.1,
        off.1
    );

    println!(
        "2D shadows + falloff OK — now LOOK at shadow_on.png: only the picture says whether \
         it reads as a wall with a dark side"
    );
}

/// Draw the floor (and the wall, when there is one) and run the 2D lighting pass
/// over the result.
#[allow(clippy::too_many_arguments)]
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    view_proj: Mat4,
    floor_raw: &floptle_render::InstanceRaw,
    wall_raw: &floptle_render::InstanceRaw,
    has_wall: bool,
    floor: MeshId,
    wall: MeshId,
    tex: TexId,
    flat: &[(MeshId, Option<TexId>, Light2dInstance)],
    lights: &Light2dUniform,
) -> Vec<[u8; 4]> {
    let size = wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 };
    let make = |label: &str, format: wgpu::TextureFormat, extra: wgpu::TextureUsages| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | extra,
            view_formats: &[],
        })
    };
    let color = make("shadow-color", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    let depth = make("shadow-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let mut scene: Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)> =
        vec![(floor, Some(tex), *floor_raw)];
    if has_wall {
        scene.push((wall, Some(tex), *wall_raw));
    }
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(gpu, &view, &dview, globals, &scene, Some([0.02, 0.02, 0.04, 1.0]), None);
    raster.light2d_pass(gpu, &view, &dview, (S, S), view_proj.to_cols_array_2d(), lights, flat);
    readback(gpu, &color)
}

fn luma(px: &[[u8; 4]], x: u32, y: u32) -> f32 {
    let p = px[(y * S + x) as usize];
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded =
        (S * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * S) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
    let mut o = Vec::with_capacity((S * S) as usize);
    for y in 0..S {
        let row = (y * padded) as usize;
        for x in 0..S {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            o.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    o
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
