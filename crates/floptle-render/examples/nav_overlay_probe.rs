//! **Look at the navmesh overlay.** Renders one level three ways and writes the
//! PNGs, because the change this overlay makes is a change to a picture and
//! there is no assertion that can tell you a picture reads better.
//!
//! Run: `cargo run --release -p floptle-render --example nav_overlay_probe`
//!
//! Writes, from the same camera and the same bake:
//!
//! - `nav_overlay_old.png` — every rectangle outlined. What it looked like.
//! - `nav_overlay_new.png` — filled surface, outline only where the ground
//!   actually ends, step ribbons where two heights are genuinely joined.
//! - `nav_overlay_slope.png` — the same level with `max_slope` dropped below the
//!   ramp's angle, so the connection comes apart. The two put side by side are
//!   the reactivity claim.
//!
//! The level is deliberately awkward: a floor with a hole, a ramp up to a
//! mezzanine, and a flight of steps. A plain floor bakes into ONE rectangle and
//! looks fine either way, which is exactly how the unreadable version survived
//! so long — every simple test scene hid it.

use floptle_nav::{bake, NavSettings, Overlay, Tri};
use floptle_render::{Globals, Gpu, LineVertex, Lines, Raster, TriVertex, Tris};
use glam::{Mat4, Vec3};

const W: u32 = 1280;
const H: u32 = 720;

fn quad(x0: f32, x1: f32, z0: f32, z1: f32, y: f32, out: &mut Vec<Tri>) {
    out.push(Tri::new([x0, y, z0], [x1, y, z0], [x0, y, z1]));
    out.push(Tri::new([x1, y, z0], [x1, y, z1], [x0, y, z1]));
}

/// A floor with a hole, a ramp to a mezzanine, and a flight of steps.
fn level() -> Vec<Tri> {
    let mut t = Vec::new();
    // Ground floor, 24 x 16, with a 6 x 6 hole in it.
    quad(0.0, 24.0, 0.0, 5.0, 0.0, &mut t);
    quad(0.0, 24.0, 11.0, 16.0, 0.0, &mut t);
    quad(0.0, 9.0, 5.0, 11.0, 0.0, &mut t);
    quad(15.0, 24.0, 5.0, 11.0, 0.0, &mut t);

    // A 25° ramp off the far edge, up to a mezzanine.
    let (x0, x1) = (24.0, 30.4);
    t.push(Tri::new([x0, 0.0, 0.0], [x1, 3.0, 0.0], [x0, 0.0, 16.0]));
    t.push(Tri::new([x1, 3.0, 0.0], [x1, 3.0, 16.0], [x0, 0.0, 16.0]));
    quad(30.4, 38.0, 0.0, 16.0, 3.0, &mut t);

    // A flight of steps down the near edge, 0.3 m each.
    for i in 0..5 {
        let z = -3.0 - i as f32 * 1.5;
        quad(0.0, 24.0, z, z + 1.5, (i + 1) as f32 * 0.3, &mut t);
    }
    t
}

fn main() {
    let gpu = Gpu::headless(W, H);
    let mut raster = Raster::new(&gpu);
    let mut lines = Lines::new(&gpu);
    let mut tris = Tris::new(&gpu);

    let geometry = level();
    // Looking down the length of the level from above and to one side — the
    // angle somebody actually judges a navmesh from.
    let eye = Vec3::new(-14.0, 22.0, -22.0);
    let view = Mat4::look_at_rh(Vec3::ZERO, Vec3::new(19.0, 1.0, 7.0) - eye, Vec3::Y);
    let proj = Mat4::perspective_rh(0.9, W as f32 / H as f32, 0.1, 500.0);
    let view_proj = proj * view;
    let rel = |p: [f32; 3]| [p[0] - eye.x, p[1] - eye.y, p[2] - eye.z];

    for (name, settings, cells) in [
        ("nav_overlay_old", NavSettings::default(), true),
        ("nav_overlay_new", NavSettings::default(), false),
        // Below the ramp's 25°, so the mezzanine comes off the ground floor.
        ("nav_overlay_slope", NavSettings { max_slope: 15.0, ..Default::default() }, false),
    ] {
        let mesh = bake(&geometry, &settings).expect("this level bakes");
        let overlay = Overlay::build(&mesh, settings.cell_size * 0.5);
        let hue = |r: u32| hue_rgb((r as f32 * 0.618_034).fract());

        let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("nav-probe"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());
        let globals =
            Globals { view_proj: view_proj.to_cols_array_2d(), ..Default::default() };
        raster.draw_scene(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &[],
            Some([0.06, 0.07, 0.08, 1.0]),
            None,
        );

        // The level itself, as a faint wireframe, so the overlay can be judged
        // against the ground it claims to describe.
        let mut wire: Vec<LineVertex> = Vec::new();
        for t in &geometry {
            for (a, b) in [(t.a, t.b), (t.b, t.c), (t.c, t.a)] {
                let c = [0.30, 0.32, 0.35, 1.0];
                wire.push(LineVertex { pos: rel(a), color: c });
                wire.push(LineVertex { pos: rel(b), color: c });
            }
        }
        lines.draw(&gpu, &color_view, gpu.depth_view(), view_proj, &wire);

        // The overlay proper.
        let mut fill: Vec<TriVertex> = Vec::new();
        let mut edges: Vec<LineVertex> = Vec::new();

        if cells {
            // The OLD picture: every rectangle outlined, nothing filled.
            for e in &overlay.cells {
                let c = hue(e.region);
                let c = [c[0], c[1], c[2], 1.0];
                edges.push(LineVertex { pos: rel(e.a), color: c });
                edges.push(LineVertex { pos: rel(e.b), color: c });
            }
        } else {
            for t in &overlay.tris {
                let c = hue(t.region);
                let c = [c[0], c[1], c[2], 0.22];
                for p in [t.a, t.b, t.c] {
                    fill.push(TriVertex { pos: rel(p), color: c });
                }
            }
            for e in &overlay.boundary {
                let c = hue(e.region);
                let c = [c[0], c[1], c[2], 1.0];
                edges.push(LineVertex { pos: rel(e.a), color: c });
                edges.push(LineVertex { pos: rel(e.b), color: c });
            }
            for s in &overlay.steps {
                let c = hue(s.region);
                let f = [c[0], c[1], c[2], 0.40];
                for p in [s.low[0], s.low[1], s.high[1], s.low[0], s.high[1], s.high[0]] {
                    fill.push(TriVertex { pos: rel(p), color: f });
                }
                let c = [c[0], c[1], c[2], 1.0];
                for (a, b) in [
                    (s.low[0], s.high[0]),
                    (s.low[1], s.high[1]),
                    (s.high[0], s.high[1]),
                ] {
                    edges.push(LineVertex { pos: rel(a), color: c });
                    edges.push(LineVertex { pos: rel(b), color: c });
                }
            }
        }
        if !fill.is_empty() {
            tris.draw(&gpu, &color_view, gpu.depth_view(), view_proj, &fill);
        }
        lines.draw(&gpu, &color_view, gpu.depth_view(), view_proj, &edges);

        let regions: std::collections::HashSet<u32> =
            mesh.polys.iter().map(|p| p.region).collect();
        let (drawn, naive) = overlay.outline_saving();
        println!(
            "{name}: {} rectangles in {} region(s) — outline {drawn} segments vs {naive} \
             the old way, {} step ribbon(s)",
            mesh.polys.len(),
            regions.len(),
            overlay.steps.len(),
        );
        write_png(&gpu, &color_tex, &format!("{name}.png"));
    }
    println!("look at nav_overlay_old.png, nav_overlay_new.png and nav_overlay_slope.png");
}

/// The editor's own per-region hue, so the probe and the Scene view agree.
fn hue_rgb(h: f32) -> [f32; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let x = 1.0 - (h % 2.0 - 1.0).abs();
    let (r, g, b) = match h as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    [r, g, b]
}

fn write_png(gpu: &Gpu, tex: &wgpu::Texture, path: &str) {
    let padded =
        (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc =
        gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
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
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(
        gpu.config.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    let mut rgba = Vec::with_capacity((W * H * 4) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            let p = if bgra { [p[2], p[1], p[0], p[3]] } else { p };
            rgba.extend_from_slice(&p);
        }
    }
    drop(view);
    buf.unmap();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&rgba).expect("write png");
}
