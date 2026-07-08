//! Headless render-performance probe — times the heavy scene passes (raymarch,
//! raster-with-field-shadows) at a full-res gameplay framing: a sculpted terrain,
//! a couple of blobs, and a field of shadow-casting cubes seen from eye height.
//! This is the "retro OFF" cost the editor/game pays per frame.
//!
//! Run: cargo run -p floptle-render --release --example perf_probe

use floptle_field::{Brush, Terrain};
use floptle_render::{
    cube, instance_of, Globals, Gpu, InstanceRaw, MeshId, Projection, Raster, Raymarch,
    RaymarchGlobals, RenderCamera, TexId,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const W: u32 = 1920;
const H: u32 = 1080;
const WARMUP: u32 = 8;
const FRAMES: u32 = 48;

fn main() {
    let gpu = Gpu::headless(W, H);

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("perf-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    // A big sculpted terrain — the common "high res game" ground. Moderate hills
    // in a generous box, like real scenes: measured user terrains keep 30+ world
    // units of empty air above the content (a third or more of the box height) —
    // the tight content bound is what makes that air free to march through.
    let mut terrain =
        Terrain::flat([144, 96, 144], [0.0, 0.0, 0.0], [40.0, 27.0, 40.0], 0.0, [0.35, 0.6, 0.28]);
    for i in 0..6 {
        let spread = i as f32 * 1.4;
        terrain.sculpt(Brush::Raise, [-14.0 + spread, 0.0, -9.0], 3.4, 1.0);
        terrain.sculpt(Brush::Raise, [12.0, 0.0, 6.0 + spread], 3.0, 1.0);
        terrain.sculpt(Brush::Raise, [18.0 - spread, 0.3, -18.0], 2.6, 1.0);
        terrain.sculpt(Brush::Lower, [-6.0, 0.5, 18.0 - spread], 3.0, 1.0);
    }

    // Report the terrain's real content height (what the tight bound sees): how
    // much of the brick is empty air the marches can now skip.
    {
        let b = &terrain.baked;
        let [w, h, d] = b.dims;
        let mut top = 0u32;
        for z in 0..d {
            for y in 0..h {
                for x in 0..w {
                    if b.distance[((z * h + y) * w + x) as usize] <= 0.5 {
                        top = top.max(y);
                    }
                }
            }
        }
        let world_top = -b.half_extent[1] + (top as f32 + 0.5) * 2.0 * b.half_extent[1] / h as f32;
        println!("terrain content top: voxel {top}/{h} (world y ~ {world_top:.2} of +{})", b.half_extent[1]);
    }

    let mut raymarch = Raymarch::new(&gpu);
    raymarch.set_volume(&gpu, &terrain.baked);
    let mut raster = Raster::new(&gpu);
    let cube_id = raster.register(&gpu, &cube(0.7), None);

    let light = Vec3::new(0.5, 0.75, 0.4).normalize();

    // Build the whole scene's globals + instances for one camera position/target
    // (everything the shaders see is camera-relative, ADR-0015, so a new camera
    // means rebuilding it all).
    let build = |cam_pos: DVec3, target: DVec3| {
        let fwd = (target - cam_pos).as_vec3().normalize();
        let rot = Quat::from_rotation_arc(Vec3::NEG_Z, fwd);
        let cam = RenderCamera::new(
            cam_pos,
            rot,
            Projection::Perspective { fov_y: 60f32.to_radians(), near: 0.1, far: 2000.0 },
        );
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let rel =
            |p: [f32; 3]| (DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) - cam_pos).as_vec3();

        // 30 shadow-casting cubes scattered over the ground (each with a box proxy).
        let mut instances: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
        let mut prox_a = [[0.0f32; 4]; 32];
        let mut prox_b = [[0.0f32; 4]; 32];
        let prox_rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
        let mut n = 0usize;
        for gx in 0..6 {
            for gz in 0..5 {
                let p = [gx as f32 * 6.0 - 15.0, 0.7, gz as f32 * 6.0 - 5.0];
                let c = rel(p);
                instances.push((
                    cube_id,
                    None,
                    instance_of(Mat4::from_translation(Vec3::new(c.x, c.y, c.z)), [0.7, 0.5, 0.4]),
                ));
                prox_a[n] = [c.x, c.y, c.z, 0.0];
                prox_b[n] = [0.7, 0.7, 0.7, 2.0];
                n += 1;
            }
        }

        let cr = rel([0.0, 0.0, 0.0]);
        let rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.22, 0.24, 0.3, 0.0],
            bg: [0.5, 0.62, 0.78, 1.0],
            params: [0.0, 2.0, 0.35, 0.0], // two blobs
            vol_center: {
                let mut a = [[0.0f32; 4]; 16];
                a[0] = [cr.x, cr.y, cr.z, 1.0];
                a
            },
            vol_half: {
                let mut a = [[1.0f32, 1.0, 1.0, 0.5]; 16];
                a[0] = [40.0, 9.0, 40.0, 0.1];
                a
            },
            blobs: {
                let mut b = [[0.0f32; 4]; 16];
                let p = rel([-4.0, 1.5, 8.0]);
                b[0] = [p.x, p.y, p.z, 1.4];
                let p = rel([5.0, 1.2, 14.0]);
                b[1] = [p.x, p.y, p.z, 1.0];
                b
            },
            shadow_params: [1.0, 12.0, 1.0, 60.0], // sun shadows ON (the default)
            prox_count: [n as f32, 0.0, 0.0, 0.0],
            prox_a,
            prox_b,
            prox_rot,
            ..Default::default()
        };
        let rg = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_dir: [light.x, light.y, light.z, 0.0],
            light_color: [1.0, 0.98, 0.92, 0.0],
            ambient: [0.22, 0.24, 0.3, 0.0],
            ..Default::default()
        };
        (rm, rg, instances)
    };

    // Scenario A: eye-height camera near the terrain edge looking across it — the
    // original probe framing (kept for perf-history continuity).
    let (rm, rg, instances) = build(DVec3::new(0.0, 2.2, 30.0), DVec3::new(0.0, 1.0, -10.0));
    // Scenario B: STANDING on the terrain, deep inside the volume box, looking at
    // the horizon — the "walking around in game" framing where every ray grazes
    // the ground or crosses the box's empty air, the reported worst case.
    let (rm_b, rg_b, instances_b) = build(DVec3::new(0.0, 1.7, 10.0), DVec3::new(0.0, 2.0, -70.0));
    // Shadows-off variant of B, to attribute march cost vs shadow-march cost.
    let mut rm_b_nosh = rm_b;
    rm_b_nosh.shadow_params[0] = 0.0;

    // Wall-clock a closure over FRAMES frames (poll-wait per frame ⏵ GPU-bound).
    let time_pass = |label: &str, f: &mut dyn FnMut()| {
        for _ in 0..WARMUP {
            f();
        }
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let t0 = std::time::Instant::now();
        for _ in 0..FRAMES {
            f();
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / FRAMES as f64;
        println!("{label}: {ms:.2} ms/frame");
    };

    println!("-- scenario A: eye height at the terrain edge --");
    time_pass("raymarch  (fullscreen SDF)", &mut || {
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
    });
    time_pass("raster    (meshes + field shadows)", &mut || {
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm);
        raster.draw_scene(&gpu, &color_view, gpu.depth_view(), rg, &instances, None, Some(raymarch.field_bind()));
    });
    time_pass("full      (depth prepass + raymarch + raster)", &mut || {
        if raster.depth_prepass(&gpu, rg, &instances, gpu.depth_texture()) {
            raymarch.set_depth_prime(&gpu, raster.prepass_view());
        }
        raymarch.draw_into_primed(&gpu, &color_view, gpu.depth_view(), rm);
        raster.draw_scene(&gpu, &color_view, gpu.depth_view(), rg, &instances, None, Some(raymarch.field_bind()));
    });
    // Optional PNG of scenario A's full frame: perf_probe -- <out.png>
    let png_a = std::env::args().nth(1);
    if let Some(out) = &png_a {
        save_png(&gpu, &color_tex, out);
        println!("wrote {out}");
    }

    println!("-- scenario B: standing ON the terrain, horizon view --");
    time_pass("raymarch  (shadows off)", &mut || {
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm_b_nosh);
    });
    time_pass("raymarch  (shadows on)", &mut || {
        raymarch.draw_into(&gpu, &color_view, gpu.depth_view(), rm_b);
    });
    time_pass("full      (depth prepass + raymarch + raster)", &mut || {
        if raster.depth_prepass(&gpu, rg_b, &instances_b, gpu.depth_texture()) {
            raymarch.set_depth_prime(&gpu, raster.prepass_view());
        }
        raymarch.draw_into_primed(&gpu, &color_view, gpu.depth_view(), rm_b);
        raster.draw_scene(&gpu, &color_view, gpu.depth_view(), rg_b, &instances_b, None, Some(raymarch.field_bind()));
    });
    // Optional PNG of scenario B's full frame: perf_probe -- <a.png> <b.png>
    if let Some(out) = std::env::args().nth(2) {
        save_png(&gpu, &color_tex, &out);
        println!("wrote {out}");
    }
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
    let mut encoder =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
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
