//! Does a mirror show the SCENE, or only the sky?
//!
//! `reflection_probe` proves a surface reflects the captured sky. This is the
//! other half: an object standing in the world has to appear in the mirror below
//! it, which the environment map cannot do at any quality — the sky knows
//! nothing about what is standing in front of it.
//!
//! The scene is built so that the answer cannot come from anywhere else. The
//! floor is a roughness-0 metal, so it has no diffuse and no colour of its own.
//! The sky is a deep RED and the only object is an unlit GREEN block held above
//! the floor, never touching it. Green on the floor can therefore have arrived
//! by exactly one route.
//!
//! Four checks:
//!
//! 1. **Green appears on the floor** when reflections are on, and over a real
//!    area rather than a stray pixel or two.
//! 2. **It is not there with reflections off.** The same frame, the same
//!    everything, one flag — so the difference IS the feature and not the
//!    lighting, the sky or the material.
//! 3. **It lands under the block, not somewhere else.** A reflection that
//!    appears in the wrong place is a bug that a "some green arrived" test would
//!    pass; the mirrored image belongs below the thing it mirrors.
//! 4. **The far floor still reflects the sky.** A screen-space ray that finds
//!    nothing must fall back to the environment, not to black — the failure that
//!    would put a dark band across every reflective floor in the engine.
//!
//! Two frames are rendered on purpose. Shading is forward, so a reflection reads
//! the PREVIOUS frame's picture (see `ssr.rs`); a probe that rendered once would
//! be reading an empty history and measuring the fallback.
//!
//! Run: cargo run -p floptle-render --example ssr_probe -- <out-dir>

use floptle_render::{
    cube, instance_of_mat, plane, Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, SceneHistory, SurfaceExtras, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;

/// The sky, and the block. Far apart in hue so neither can be mistaken for the
/// other after a tonemap, a blur or an 8-bit round trip.
const SKY: [f32; 3] = [0.55, 0.05, 0.05];
const BLOCK: [f32; 3] = [0.0, 1.0, 0.15];

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    std::fs::create_dir_all(&dir).ok();
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);
    let mut rm = Raymarch::new(&gpu);
    let floor_mesh = raster.register(&gpu, &plane(1.0), None);
    let block_mesh = raster.register(&gpu, &cube(0.5), None);

    let on = shot(&gpu, &mut raster, &mut rm, floor_mesh, block_mesh, true, &format!("{dir}/ssr_on.png"));
    let off =
        shot(&gpu, &mut raster, &mut rm, floor_mesh, block_mesh, false, &format!("{dir}/ssr_off.png"));

    // "Reflected green": green clearly ahead of red, which under a red sky can
    // only be the block. Counted per pixel over the FLOOR half of the frame, so
    // the block itself never votes for its own reflection.
    let green_on = green_pixels(&on);
    let green_off = green_pixels(&off);
    println!(
        "reflective floor: {} green px with reflections on, {} with them off",
        green_on.len(),
        green_off.len()
    );

    // 1 + 2. The reflection exists, and it is the flag that makes it exist.
    assert!(
        green_on.len() > 200,
        "reflections are on and the mirror shows {} green pixels — the block is \
         not appearing in the floor below it",
        green_on.len()
    );
    assert!(
        green_off.len() * 8 < green_on.len(),
        "the floor is nearly as green with reflections OFF ({} vs {} px) — whatever \
         is being measured, it is not the screen-space reflection",
        green_off.len(),
        green_on.len()
    );

    // 3. It is in the right PLACE: the mirrored block sits under the real one, so
    //    the reflection's horizontal centre must line up with the block's.
    let block_x = mean_x(&block_pixels(&on));
    let refl_x = mean_x(&green_on);
    assert!(
        (block_x - refl_x).abs() < 0.12,
        "the reflection is at x={refl_x:.3} but the block is at x={block_x:.3} — \
         it is landing somewhere the block is not"
    );

    // 4. Where the ray finds nothing, the sky still comes through. Sampled at the
    //    very top of the floor (the far distance), away from the block.
    let far = mean_rgb(&on, 0.02, 0.30, 0.42, 0.46);
    assert!(
        far[0] > far[1] + 0.05 && far[0] > 0.08,
        "the far floor reads {far:?} — a screen-space ray that finds nothing must \
         fall back to the sky, and this one has fallen back to darkness"
    );

    println!("ssr probe OK  (far floor rgb [{:.3}, {:.3}, {:.3}])", far[0], far[1], far[2]);
}

/// One two-frame render. `reflections` switches ONLY the SSR flag; everything
/// else — the geometry, the sky, the material, the two frames — is identical.
fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    rm: &mut Raymarch,
    floor_mesh: MeshId,
    block_mesh: MeshId,
    reflections: bool,
    out: &str,
) -> Vec<u8> {
    // Pitched down so the floor fills the lower frame and the block sits in the
    // upper. The camera is at the ORIGIN and everything is placed relative to it
    // (ADR-0015): the view matrix carries no translation, so an instance's model
    // translation IS its position relative to the eye.
    let cam = RenderCamera::new(
        DVec3::ZERO,
        Quat::from_rotation_x(-0.28),
        Projection::Perspective { fov_y: 50f32.to_radians(), near: 0.05, far: 200.0 },
    );
    let vp = cam.view_proj(1.0);

    // A big horizontal mirror, and a block held well clear of it.
    let floor = Mat4::from_translation(Vec3::new(0.0, -2.0, -9.0))
        * Mat4::from_rotation_x(-std::f32::consts::FRAC_PI_2)
        * Mat4::from_scale(Vec3::splat(30.0));
    let block = Mat4::from_translation(Vec3::new(0.0, -0.55, -8.0)) * Mat4::from_scale(Vec3::splat(1.6));

    // The floor: a polished metal. Metallic 1 with roughness 0 has no diffuse
    // term at all, so it cannot produce a colour on its own — anything it shows
    // arrived from the environment or from the scene.
    let mut floor_mp = MaterialParams::flat([0.85, 0.85, 0.88]);
    floor_mp.ext_index = raster.push_surface_extras(SurfaceExtras {
        roughness: 0.0,
        metallic: 1.0,
        physical: true,
        reflectivity: 1.0,
        ..SurfaceExtras::default()
    });
    // The block: UNLIT, so its colour is exactly `BLOCK` in every frame and does
    // not depend on where the sun is or on the reflection being measured.
    let mut block_mp = MaterialParams::flat(BLOCK);
    block_mp.unlit = true;

    let instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = vec![
        (floor_mesh, None, instance_of_mat(floor, &floor_mp)),
        (block_mesh, None, instance_of_mat(block, &block_mp)),
    ];

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: S, height: S, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        // TEXTURE_BINDING as well: the history samples this very texture.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut history = SceneHistory::new(&gpu.device, S, S, gpu.config.format);
    rm.set_scene_history(gpu, Some((history.view(), history.sampler())));

    let light = Vec3::new(0.2, 0.9, 0.3).normalize();
    let globals = Globals {
        view_proj: vp.to_cols_array_2d(),
        light_dir: [light.x, light.y, light.z, 0.0],
        light_color: [1.0, 1.0, 1.0, 0.0],
        ambient: [0.0, 0.0, 0.0, 0.0],
        ..Default::default()
    };

    // Frame 1 fills the history; frame 2 reflects it. `primed` follows the same
    // rule the editor uses — off until there is a picture to read.
    for frame in 0..2 {
        let primed = frame > 0;
        let mut rmg = RaymarchGlobals {
            view_proj: vp.to_cols_array_2d(),
            inv_view_proj: vp.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 1.0, 1.0, 0.0],
            ambient: [0.0, 0.0, 0.0, 0.0],
            bg: [SKY[0], SKY[1], SKY[2], 1.0],
            ssr: [
                if reflections && primed { 1.0 } else { 0.0 },
                40.0,
                48.0,
                1.0,
            ],
            ..Default::default()
        };
        // The camera has not moved, so the stored picture's matrix is this one.
        rmg.ssr_prev_vp = history
            .prev_view_proj(cam.world_position)
            .unwrap_or(vp)
            .to_cols_array_2d();
        rm.upload_globals(gpu, rmg);
        rm.capture_env(gpu);

        // The prepass is what the reflection marches, and the editor now runs
        // and BINDS it whether or not anything is raymarched. Skipping either
        // half here would make the probe measure the sky fallback.
        raster.depth_prepass_with(gpu, globals, &instances, &[], &[], gpu.depth_texture());
        rm.set_depth_prime(gpu, raster.prepass_view());

        raster.draw_scene_with(
            gpu,
            &view,
            gpu.depth_view(),
            globals,
            &instances,
            &[],
            &[],
            Some([SKY[0] as f64, SKY[1] as f64, SKY[2] as f64, 1.0]),
            Some(rm.field_bind()),
        );
        history.capture(gpu, &view, vp, cam.world_position);
    }

    let px = read_back(gpu, &color_tex);
    save_png(&px, out);
    px
}

/// Pixels in the FLOOR half of the frame that read distinctly green. The block
/// itself lives in the upper frame and is excluded by the row bound, so it can
/// never be counted as its own reflection.
fn green_pixels(px: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for y in (S / 2)..S {
        for x in 0..S {
            let i = ((y * S + x) * 4) as usize;
            let (r, g) = (px[i] as f32 / 255.0, px[i + 1] as f32 / 255.0);
            if g > r + 0.12 && g > 0.18 {
                out.push((x, y));
            }
        }
    }
    out
}

/// The block's own pixels, for the "is the reflection under it" check.
fn block_pixels(px: &[u8]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    for y in 0..(S / 2) {
        for x in 0..S {
            let i = ((y * S + x) * 4) as usize;
            let (r, g) = (px[i] as f32 / 255.0, px[i + 1] as f32 / 255.0);
            if g > r + 0.3 && g > 0.5 {
                out.push((x, y));
            }
        }
    }
    out
}

/// Mean x of a pixel set, in 0..1 across the frame.
fn mean_x(p: &[(u32, u32)]) -> f32 {
    if p.is_empty() {
        return f32::NAN;
    }
    p.iter().map(|&(x, _)| x as f32).sum::<f32>() / p.len() as f32 / S as f32
}

/// Mean colour of a rectangle given in 0..1 frame coordinates.
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
    let n = n.max(1) as f64;
    [(sum[0] / n) as f32, (sum[1] / n) as f32, (sum[2] / n) as f32]
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
