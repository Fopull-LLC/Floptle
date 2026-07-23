//! Headless `stage ui` shader probe — compiles the solar demo's UI instruments
//! through the FULL production path (parse → check → transpile_ui → naga
//! against the real ui.wgsl + field shim → `register_ui_shader` → params
//! binding → `Ui::pack`/`draw`) and renders each to a PNG. Proves UI shaders
//! end-to-end, no window.
//!
//!   navball.flsl → navball_probe.png  (45°-pitched, north-facing attitude)
//!   map.flsl     → map_probe.png     (e=0.35 transfer ellipse, ship at
//!                                     θ=120°, moon + SOI ring at r=780)
//!
//! Run: cargo run --release -p floptle-render --example ui_shader_probe

use floptle_render::{Gpu, Raster, Ui};
use floptle_ui::{DrawList, Quad};

const W: u32 = 440;
const H: u32 = 440;

fn render_ui_shader(
    gpu: &Gpu,
    raster: &Raster,
    ui: &mut Ui,
    path: &str,
    params_of: &dyn Fn(&str) -> Option<[f32; 4]>,
    out: &str,
) -> Vec<[u8; 4]> {
    let src = std::fs::read_to_string(path).expect("read flsl");
    let compiled = floptle_shader::compile_ui(&src).expect("compile_ui");
    let prelude =
        format!("{}\n{}", Ui::ui_prelude(), floptle_shader::transpile::UI_FIELD_SHIM);
    floptle_shader::validate(&prelude, &compiled.chunk)
        .unwrap_or_else(|e| panic!("naga rejects {path}: {}", e.message));

    let chunk_full = format!(
        "{}\n{}\n{}",
        floptle_shader::transpile::UI_FIELD_SHIM,
        floptle_shader::stdlib::SUPPORT_WGSL,
        compiled.chunk
    );
    let shader = ui.register_ui_shader(gpu, &chunk_full, None);
    let params = compiled.pack_params(params_of);
    let binding = ui.set_ui_shader_binding(gpu, &params, None);

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
            shader: Some((path.to_string(), 1)),
            feather: 0.0,
        }],
        texts: Vec::new(),
    };
    let mut instances = Vec::new();
    let mut batches = Vec::new();
    ui.pack(
        gpu,
        &list,
        [0.0, 0.0],
        1.0,
        &mut |_| None,
        &mut |_, _| Some((shader, binding)),
        &mut instances,
        &mut batches,
    );
    ui.draw(gpu, &color_view, [W as f32, H as f32], &instances, &batches, raster);

    let px = readback(gpu, &color_tex);
    save_png(&px, out);
    px
}

fn main() {
    let gpu = Gpu::headless(W, H);
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);

    // ---- the navball: 45°-pitched, north-facing, prograde above the nose ----
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let px = render_ui_shader(
        &gpu,
        &raster,
        &mut ui,
        "solar/shaders/navball.flsl",
        &|name| match name {
            "right" => Some([1.0, 0.0, 0.0, 0.0]),
            "up" => Some([0.0, s, -s, 0.0]),
            "nose" => Some([0.0, s, s, 0.0]),
            "prograde" => Some([0.0, 0.35, 0.937, 0.0]),
            _ => None,
        },
        "navball_probe.png",
    );
    let at = |x: u32, y: u32| px[(y * W + x) as usize];
    let sky = at(220, 120);
    let ground = at(220, 395);
    println!("sky px {sky:?}, ground px {ground:?}");
    assert!(sky[2] > sky[0], "upper half should be sky-blue, got {sky:?}");
    assert!(ground[0] > ground[2], "lower half should be ground-brown, got {ground:?}");
    println!("navball ui shader OK; wrote navball_probe.png");

    // ---- the map: a transfer ellipse out toward the moon's SOI ring ---------
    // Focus body r=120 at center; conic a=600 e=0.35 (p=526.5, pe=390,
    // ap=810); ship at true anomaly 120° = (-319, 552.6) heading prograde;
    // moon r=40 soi=260 on its 780 ring at 40°.
    let px = render_ui_shader(
        &gpu,
        &raster,
        &mut ui,
        "solar/shaders/map.flsl",
        &|name| match name {
            "view" => Some([900.0, 1.0, 1.0, 0.0]),
            "conic" => Some([526.5, 0.35, 1.0, 0.0]),
            "shipm" => Some([-319.0, 552.6, 0.0, 0.0]),
            "velm" => Some([-0.985, -0.171, 0.0, 0.0]),
            "focusb" => Some([120.0, -1.0, 0.0, 0.0]),
            "otherb" => Some([597.5, 501.4, 40.0, 0.0]),
            "otherb2" => Some([260.0, 780.0, 1.0, 0.0]),
            _ => None,
        },
        "map_probe.png",
    );
    // Count feature pixels rather than hitting exact coordinates: the conic
    // stroke (cyan), the ship marker (near-white), the Pe marker (gold), and
    // the focus-body disc (the warm tint at panel center).
    let mut cyan = 0;
    let mut white = 0;
    let mut gold = 0;
    for p in &px {
        if p[2] > 190 && p[1] > 150 && p[0] < 160 {
            cyan += 1;
        }
        if p[0] > 225 && p[1] > 225 && p[2] > 225 {
            white += 1;
        }
        // Gold Pe core (1, 0.8, 0.25) lands at ~(249, 222, 130) after the
        // 0.92-alpha blend + sRGB encode — blue stays well under red/green.
        if p[0] > 220 && p[1] > 170 && p[2] < 180 && p[2] < p[1] {
            gold += 1;
        }
    }
    let center = at(220, 220);
    let corner = at(30, 30);
    println!("map: cyan {cyan}, white {white}, gold {gold}, center {center:?}, corner {corner:?}");
    assert!(cyan > 120, "expected a drawn conic (cyan stroke), got {cyan} px");
    assert!(white >= 2, "expected a ship marker, got {white} px");
    assert!(gold >= 2, "expected a Pe marker, got {gold} px");
    assert!(center[0] > corner[0] + 30, "focus disc should tint the center: {center:?} vs {corner:?}");
    assert!(corner[0] < 45 && corner[1] < 55 && corner[2] < 70, "corner should be deep space, got {corner:?}");
    println!("map ui shader OK; wrote map_probe.png");

    // ---- the G5 tape: speed 42 over a ±40 window, ticks every 5 -------------
    let px = render_ui_shader(
        &gpu,
        &raster,
        &mut ui,
        "solar/shaders/tape.flsl",
        &|name| match name {
            "tape" => Some([42.0, 40.0, 5.0, 0.0]),
            "side" => Some([0.0, 0.0, 0.0, 0.0]),
            _ => None,
        },
        "tape_probe.png",
    );
    // Ticks are light gray lines; the center reference + wedge use the accent
    // (default #FFB13B → orange, red > blue).
    let mut ticks = 0;
    let mut acc = 0;
    for p in &px {
        if p[0] > 170 && p[1] > 180 && p[2] > 190 {
            ticks += 1;
        }
        if p[0] > 200 && p[2] < 150 && p[0] > p[2] + 80 {
            acc += 1;
        }
    }
    println!("tape: tick px {ticks}, accent px {acc}");
    assert!(ticks > 60, "expected scrolling tick marks, got {ticks} px");
    assert!(acc > 40, "expected the accent reference line/wedge, got {acc} px");
    println!("tape ui shader OK; wrote tape_probe.png");
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
