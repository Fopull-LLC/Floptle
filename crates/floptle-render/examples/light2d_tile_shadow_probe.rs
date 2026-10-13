//! **2D shadows from a tilemap's solid squares, without streaks**.
//!
//! A flat floor under an orthographic camera, a 2D light to its left, and a wall
//! two pixels thick laid over the floor as an occluder: the shape a tilemap's
//! solid squares become, in the floor's own plane, marking where light stops
//! and drawing nothing. Below the wall's end is a decor tile, filled into the
//! G-buffer like any tile and not solid.
//!
//! The march used to take a fixed 28 samples whatever the distance, so a light
//! a few hundred pixels away sampled every dozen pixels and stepped over a thin
//! wall for some pixels and not their neighbours: thin lines of light fanning
//! out from the lamp across what should have been shadow. It steps a pixel at a
//! time now.
//!
//! Three claims, at three distances from the light to the wall:
//!
//! * Behind the wall, **every** pixel is in shadow. A count, not an average —
//!   an average hides exactly the one-pixel lines this is about.
//! * Behind the decor tile, the floor is lit: only what is solid casts.
//! * With no occluder at all the same pixels are lit, so the dark is the wall's.
//!
//! Writes `tile_shadow_<distance>.png` for each distance, to look at.
//!
//! Run: cargo run -p floptle-render --example light2d_tile_shadow_probe -- <outdir>

use floptle_render::probe::readback;
use floptle_render::{
    Globals, Gpu, Light2dInstance, Light2dUniform, MaterialParams, MeshId, Projection, Raster,
    RenderCamera, TexId, TexSampling, TextureData, instance_of_mat, mesh,
};
use glam::{DVec3, Mat4, Quat, Vec3};

const S: u32 = 256;
const ORTHO_HEIGHT: f32 = 16.0;
/// 16 units over 256 pixels: one unit is 16 px and world x = 0 is screen x = 128.
const PX_PER_UNIT: f32 = S as f32 / ORTHO_HEIGHT;
const RANK: u32 = 1;
const RANGE: f32 = 14.0;
/// The wall's thickness: two pixels.
const WALL: f32 = 2.0 / PX_PER_UNIT;
/// The wall runs from here to the top of the frame; the decor tile sits below.
const WALL_FROM_Y: f32 = 0.0;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(S, S);
    let mut raster = Raster::new(&gpu);

    let n = 8u32;
    let pixels: Vec<u8> = (0..n * n).flat_map(|_| [150u8, 150, 150, 255]).collect();
    let tex = raster.register_texture(&gpu, &TextureData { pixels, width: n, height: n }, TexSampling::default());
    let floor = {
        let data: Vec<u32> = vec![0; 16 * 16];
        raster.register(&gpu, &mesh::tilemap(16, 16, 1.0, 1, 1, [0.0, 0.0], &data), None)
    };
    // The decor tile: one square below where the wall ends, a hair in front of
    // the floor so it wins the fill, and never an occluder.
    let decor = raster.register(&gpu, &mesh::tilemap(1, 1, 1.0, 1, 1, [0.0, 0.0], &[0]), None);
    let wall = raster.register(&gpu, &mesh::flat_shapes(&[[0.0, WALL_FROM_Y, WALL, 8.0]], &[]), None);

    let unlit = MaterialParams { unlit: true, ..MaterialParams::flat([1.0, 1.0, 1.0]) };
    let floor_raw = instance_of_mat(Mat4::IDENTITY, &unlit);
    let decor_raw = instance_of_mat(Mat4::from_translation(Vec3::new(WALL * 0.5, -1.5, 0.01)), &unlit);

    let cam = RenderCamera::new(
        DVec3::new(0.0, 0.0, 10.0),
        Quat::IDENTITY,
        Projection::of_camera(1.0, true, ORTHO_HEIGHT, 0.05, 300_000.0),
    );
    let view_proj = cam.view_proj(1.0);
    let lights_at = |x: f32| {
        let mut u = Light2dUniform {
            count: [1.0, 0.0, 0.0, 0.0],
            ambient: [0.25, 0.25, 0.25, 0.0],
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            ..Default::default()
        };
        u.pos[0] = [x, 2.0, 0.0, RANGE];
        u.color[0] = [1.5, 1.5, 1.5, 0.0];
        u.mask[0] = [1 << RANK, 0, 0, 0];
        // A flat ramp, so everything inside the range is plainly lit and a
        // shadowed pixel is plainly not.
        u.falloff[0] = [RANGE * 0.95, 2.0, 1.0, 0.0];
        u
    };
    let px_x = |x: f32| (S as f32 * 0.5 + x * PX_PER_UNIT) as u32;
    let px_y = |y: f32| (S as f32 * 0.5 - y * PX_PER_UNIT) as u32;

    let mut failures = Vec::new();
    for distance in [2.0f32, 5.0, 9.0] {
        let light_x = -distance;
        let mut flat: Vec<(MeshId, Option<TexId>, Light2dInstance)> = vec![
            (floor, Some(tex), Light2dInstance::from_raster(&floor_raw, RANK, false)),
            (decor, Some(tex), Light2dInstance::from_raster(&decor_raw, RANK, false)),
        ];
        // What the floor is with no light on it — the line a shadowed pixel
        // sits on. Measured rather than worked out from the ambient, because
        // the frame is sRGB-encoded on the way out.
        let unlit_frame = {
            let mut l = lights_at(light_x);
            l.count[0] = 0.0;
            shot(&gpu, &mut raster, view_proj, &[(floor, floor_raw), (decor, decor_raw)], tex, &flat, &l)
        };
        let lit_floor = luma(&unlit_frame, px_x(1.0), px_y(4.0)) + 0.1;
        let open = shot(&gpu, &mut raster, view_proj, &[(floor, floor_raw), (decor, decor_raw)], tex, &flat, &lights_at(light_x));
        flat.push((wall, None, Light2dInstance::occluder(Mat4::IDENTITY.to_cols_array_2d(), RANK)));
        let walled = shot(&gpu, &mut raster, view_proj, &[(floor, floor_raw), (decor, decor_raw)], tex, &flat, &lights_at(light_x));
        save_png(&walled, &format!("{dir}/tile_shadow_{distance}.png"));

        // The shadow behind the wall: from four pixels past it to three units
        // on, over the rows the wall's own shadow certainly covers (the light
        // is at y = 2, so the shadow of a wall from y = 0 up fans upwards).
        let (x0, x1) = (px_x(WALL) + 4, px_x(WALL + 3.0));
        let (y0, y1) = (px_y(7.5), px_y(2.5));
        let mut leaks = 0;
        let mut open_lit = 0;
        let mut total = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                total += 1;
                if luma(&walled, x, y) > lit_floor {
                    leaks += 1;
                }
                if luma(&open, x, y) > lit_floor {
                    open_lit += 1;
                }
            }
        }
        // Behind the decor tile: two units on from it along the light's ray
        // through it, a ray that passes under the wall's foot.
        let (dx, dy) = (WALL * 0.5 - light_x, -1.5 - 2.0);
        let len = (dx * dx + dy * dy).sqrt();
        let (bx, by) = (WALL * 0.5 + dx / len * 2.0, -1.5 + dy / len * 2.0);
        let decor_side = luma(&walled, px_x(bx), px_y(by));
        println!(
            "distance {distance}: {leaks} of {total} shadowed pixels lit ({open_lit} lit with no wall), \
             floor behind the decor tile at {decor_side:.3}"
        );
        if open_lit < total * 9 / 10 {
            failures.push(format!(
                "distance {distance}: only {open_lit} of {total} pixels lit with no wall at all — this \
                 probe cannot see a shadow if the light never reached there"
            ));
        }
        if leaks > 0 {
            failures.push(format!(
                "distance {distance}: {leaks} of {total} pixels behind a two-pixel wall are lit. Light \
                 leaking through a thin wall in lines is the march stepping over it"
            ));
        }
        if decor_side < lit_floor {
            failures.push(format!(
                "distance {distance}: the floor behind a decor tile is dark ({decor_side:.3}) — a tile \
                 that is not solid cast a shadow"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("2D tile shadows OK — now LOOK at tile_shadow_9.png: the far side of the wall must be one clean dark wedge");
}

fn shot(
    gpu: &Gpu,
    raster: &mut Raster,
    view_proj: Mat4,
    scene: &[(MeshId, floptle_render::InstanceRaw)],
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
    let color = make("tile-shadow-color", gpu.surface_format(), wgpu::TextureUsages::COPY_SRC);
    let depth = make("tile-shadow-depth", Gpu::DEPTH_FORMAT, wgpu::TextureUsages::empty());
    let view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let dview = depth.create_view(&wgpu::TextureViewDescriptor::default());
    let draws: Vec<(MeshId, Option<TexId>, floptle_render::InstanceRaw)> =
        scene.iter().map(|&(m, raw)| (m, Some(tex), raw)).collect();
    let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(gpu, &view, &dview, globals, &draws, Some([0.02, 0.02, 0.04, 1.0]), None);
    raster.light2d_pass(gpu, &view, &dview, (S, S), view_proj.to_cols_array_2d(), lights, flat);
    readback(gpu, &color)
}

fn luma(px: &[[u8; 4]], x: u32, y: u32) -> f32 {
    let p = px[(y * S + x) as usize];
    (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), S, S);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
