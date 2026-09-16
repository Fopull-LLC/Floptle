//! Headless probe for the runtime 3D line layer (`Lines`) — clears a target
//! through the raster pass (color + depth), draws two crossing colored lines
//! in clip space, and asserts both colors landed. Proves the pipeline, vertex
//! layout, blending and depth-load path end-to-end without a window.
//!
//! Run: cargo run --release -p floptle-render --example lines_probe

use floptle_render::{Globals, Gpu, LineVertex, Lines, Raster};
use glam::Mat4;
use floptle_render::probe::{readback};

const W: u32 = 256;
const H: u32 = 256;

fn main() {
    let gpu = Gpu::headless(W, H);
    let mut raster = Raster::new(&gpu);
    let mut lines = Lines::new(&gpu);

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

    // Clear color + depth through the raster pass (an empty scene draw).
    let globals = Globals { view_proj: Mat4::IDENTITY.to_cols_array_2d(), ..Default::default() };
    raster.draw_scene(&gpu, &color_view, gpu.depth_view(), globals, &[], Some([0.02, 0.02, 0.03, 1.0]), None);

    // An X in clip space: identity view_proj, positions already NDC.
    let verts = [
        LineVertex { pos: [-0.8, -0.8, 0.5], color: [1.0, 0.1, 0.1, 1.0] },
        LineVertex { pos: [0.8, 0.8, 0.5], color: [1.0, 0.1, 0.1, 1.0] },
        LineVertex { pos: [-0.8, 0.8, 0.5], color: [0.1, 1.0, 0.2, 1.0] },
        LineVertex { pos: [0.8, -0.8, 0.5], color: [0.1, 1.0, 0.2, 1.0] },
    ];
    lines.draw(&gpu, &color_view, gpu.depth_view(), Mat4::IDENTITY, &verts);

    let px = readback(&gpu, &color_tex);
    let red = px.iter().filter(|p| p[0] > 180 && p[1] < 120).count();
    let green = px.iter().filter(|p| p[1] > 180 && p[0] < 120).count();
    println!("lines: red px {red}, green px {green}");
    assert!(red > 60, "red diagonal missing ({red} px)");
    assert!(green > 60, "green diagonal missing ({green} px)");
    println!("line layer OK");
}
