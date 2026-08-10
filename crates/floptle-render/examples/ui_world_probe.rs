//! Headless probe for a WORLD-SPACE UI layer — both kinds of element, drawn
//! through `Ui::draw_world` into a scene-format attachment.
//!
//! This exists because of a crash that reached a user. A screen layer draws
//! onto the window (8-bit sRGB); a world layer draws into the SCENE target,
//! which is HDR and a different format — and a render pipeline built for one
//! format is a hard validation error in a pass using the other. The built-in
//! element pipelines had a world variant with the right format; the pipeline
//! built for a `stage ui` SHADER shared one target description with its screen
//! twin, so opening a scene with a world-space layer whose element used a UI
//! shader killed the editor before it drew a frame.
//!
//! Two elements, because the two pipelines are built in different places and
//! only one of them was wrong:
//!
//!   * a plain coloured panel  → the built-in world pipeline
//!   * a `stage ui` shader     → `register_ui_shader`'s world pipeline
//!
//! Run: cargo run --release -p floptle-render --example ui_world_probe

use floptle_render::{Gpu, Raster, Ui, UiPlane};
use floptle_ui::{DrawList, Quad};

const W: u32 = 320;
const H: u32 = 320;

/// A shader with no uniforms that paints a flat, unmistakable green — the
/// point of the probe is which PIPELINE runs, not what it computes.
const FLAT: &str = "shader flat {\n  stage ui\n  output color = vec4(0.1, 0.9, 0.2, 1.0)\n}\n";

fn main() {
    // `headless_hdr`, NOT `headless`: the plain one keeps the 8-bit surface
    // format for the scene too, which makes both pipelines agree and the bug
    // this probe exists for unreproducible. The mismatch IS the subject.
    let gpu = Gpu::headless_hdr(W, H);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    let compiled = floptle_shader::compile_ui(FLAT).expect("compile_ui");
    let chunk = format!(
        "{}\n{}\n{}",
        floptle_shader::transpile::UI_FIELD_SHIM,
        floptle_shader::stdlib::SUPPORT_WGSL,
        compiled.chunk
    );
    let shader = ui.register_ui_shader(&gpu, &chunk, None);
    let binding = ui.set_ui_shader_binding(&gpu, &compiled.pack_params(&|_| None), None);

    // The scene target: `scene_format`, NOT the window's format. Getting this
    // wrong in the probe would hide the very bug it is here to catch.
    let color = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-scene-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.scene_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-scene-depth"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: Gpu::DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let list = DrawList {
        text_snap: 0.0,
        quads: vec![
            Quad { rect: [0.0, 0.0, 50.0, 100.0], color: [0.9, 0.2, 0.2, 1.0], ..Default::default() },
            Quad {
                rect: [50.0, 0.0, 50.0, 100.0],
                shader: Some(("flat".to_string(), 1)),
                ..Default::default()
            },
        ],
        texts: Vec::new(),
    };
    let mut instances = Vec::new();
    let mut batches = Vec::new();
    ui.pack(
        &gpu,
        &list,
        [0.0, 0.0],
        1.0,
        &mut |_| None,
        &|_| None,
        &mut |_, _| Some((shader, binding)),
        &mut instances,
        &mut batches,
    );

    // A canvas square on z = 0, viewed head-on by an identity-ish projection:
    // the 100×100 design space maps to the whole target.
    let plane =
        UiPlane { origin: [-1.0, 1.0, 0.0], right: [0.02, 0.0, 0.0], down: [0.0, -0.02, 0.0] };
    let view_proj = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.5, 1.0],
    ];
    // The call that used to abort the process.
    ui.draw_world(&gpu, &color_view, &depth_view, view_proj, plane, &instances, &batches, &raster);
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");

    println!("world-space UI drew both element kinds into a {:?} target", gpu.scene_format());
    println!("ui_world_probe OK");
}
