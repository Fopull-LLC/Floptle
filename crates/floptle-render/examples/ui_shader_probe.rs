//! Headless `stage ui` shader probe — compiles the solar demo's navball.flsl
//! through the FULL production path (parse → check → transpile_ui → naga
//! against the real ui.wgsl + field shim → `register_ui_shader` → params
//! binding → `Ui::pack`/`draw`) and renders the instrument at a 45°-pitched,
//! north-facing attitude to a PNG. Proves UI shaders end-to-end, no window.
//!
//! Run: cargo run --release -p floptle-render --example ui_shader_probe -- [out.png] [flsl]

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{DrawList, Quad};

const W: u32 = 440;
const H: u32 = 440;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "navball_probe.png".into());
    let path =
        std::env::args().nth(2).unwrap_or_else(|| "solar/shaders/navball.flsl".into());
    let src = std::fs::read_to_string(&path).expect("read flsl");

    let compiled = floptle_shader::compile_ui(&src).expect("compile_ui");
    let prelude =
        format!("{}\n{}", Ui::ui_prelude(), floptle_shader::transpile::UI_FIELD_SHIM);
    floptle_shader::validate(&prelude, &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga rejects: {}", e.message));

    let gpu = Gpu::headless(W, H);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);
    let chunk_full = format!(
        "{}\n{}\n{}",
        floptle_shader::transpile::UI_FIELD_SHIM,
        floptle_shader::stdlib::SUPPORT_WGSL,
        compiled.chunk
    );
    let shader = ui.register_ui_shader(&gpu, &chunk_full, None);

    // A 45°-pitched, north-facing attitude in the horizon frame (x=east,
    // y=up, z=north), prograde slightly above the nose.
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let params = compiled.pack_params(&|name| match name {
        "right" => Some([1.0, 0.0, 0.0, 0.0]),
        "up" => Some([0.0, s, -s, 0.0]),
        "nose" => Some([0.0, s, s, 0.0]),
        "prograde" => Some([0.0, 0.35, 0.937, 0.0]),
        _ => None,
    });
    let binding = ui.set_ui_shader_binding(&gpu, &params, None);

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

    let list = DrawList {
        quads: vec![Quad {
            rect: [20.0, 20.0, 400.0, 400.0],
            color: [1.0, 1.0, 1.0, 1.0],
            radius: 0.0,
            border: 0.0,
            border_color: [0.0; 4],
            texture: String::new(),
            uv: [0.0, 0.0, 1.0, 1.0],
            clip: None,
            shader: Some((path.clone(), 1)),
        }],
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
        &mut |_, _| Some((shader, binding)),
        &mut instances,
        &mut batches,
    );
    ui.draw(&gpu, &color_view, [W as f32, H as f32], &instances, &batches, &raster);

    let px = readback(&gpu, &color_tex);
    save_png(&px, &out);

    // The ball must show BOTH hemispheres at 45° pitch: sky-ish (blue-dominant)
    // above center, ground-ish (red-dominant) below, and not be empty.
    let at = |x: u32, y: u32| px[(y * W + x) as usize];
    let sky = at(220, 120);
    let ground = at(220, 395);
    println!("sky px {sky:?}, ground px {ground:?}");
    assert!(sky[2] > sky[0], "upper half should be sky-blue, got {sky:?}");
    assert!(ground[0] > ground[2], "lower half should be ground-brown, got {ground:?}");
    println!("navball ui shader OK; wrote {out}");
}

fn readback(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<[u8; 4]> {
    let padded = (W * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo { texture: tex, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
        wgpu::TexelCopyBufferInfo { buffer: &buf, layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(padded), rows_per_image: Some(H) } },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit(Some(enc.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let view = buf.slice(..).get_mapped_range();
    let bgra = matches!(gpu.config.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb);
    let mut outp = Vec::with_capacity((W * H) as usize);
    for y in 0..H {
        let row = (y * padded) as usize;
        for x in 0..W {
            let i = row + (x * 4) as usize;
            let p = [view[i], view[i + 1], view[i + 2], view[i + 3]];
            outp.push(if bgra { [p[2], p[1], p[0], p[3]] } else { p });
        }
    }
    drop(view);
    buf.unmap();
    outp
}

fn save_png(px: &[[u8; 4]], path: &str) {
    let flat: Vec<u8> = px.iter().flat_map(|p| *p).collect();
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&flat).unwrap();
}
