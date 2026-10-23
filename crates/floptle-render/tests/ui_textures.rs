//! **UI pictures draw well at any size.** Three things a hand-made UI style
//! ran into: a large panel texture drawn small shimmered (no mip chain), grain
//! read strong on dark fills and invisible on light ones (added in linear
//! light, in square cells), and a 9-slice's corners could only be drawn at one
//! texture pixel per UI unit.
//!
//! Each guard draws through the production path (`DrawList` → `Ui::pack` →
//! `Ui::draw` → `ui.wgsl`) and reads the pixels back.

use floptle_render::probe::readback;
use floptle_render::{Gpu, Raster, TexId, TexSampling, TextureData, Ui};
use floptle_ui::{DrawList, GrainSpec, Quad};

const W: u32 = 320;
const H: u32 = 320;

fn can_draw(gpu: &Gpu) -> bool {
    gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS)
}

/// Draw `quads` over black and read the frame back, sRGB bytes.
fn render(gpu: &Gpu, raster: &Raster, ui: &mut Ui, quads: Vec<Quad>, tex: Option<TexId>) -> Vec<[u8; 4]> {
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ui-textures"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let list = DrawList { quads, ..Default::default() };
    let (mut instances, mut batches) = (Vec::new(), Vec::new());
    ui.clear_backdrop();
    ui.pack(
        gpu,
        &list,
        [0.0, 0.0],
        1.0,
        &mut |p| (p == "tex").then_some(tex).flatten(),
        &|id| raster.texture_size(id),
        &mut |_, _| None,
        &mut instances,
        &mut batches,
    );
    ui.draw(gpu, &view, [W as f32, H as f32], &instances, &batches, raster);
    readback(gpu, &target)
}

fn region(px: &[[u8; 4]], x0: u32, y0: u32, w: u32, h: u32) -> Vec<f64> {
    let mut out = Vec::new();
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            out.push(f64::from(px[(y * W + x) as usize][0]));
        }
    }
    out
}

fn mean_sd(v: &[f64]) -> (f64, f64) {
    let m = v.iter().sum::<f64>() / v.len() as f64;
    (m, (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len() as f64).sqrt())
}

/// **Grain reads the same on white and on black.** Grain 0.5 on a white fill
/// and on a black one varies by a similar amount on screen; added in linear
/// light it was a whisper on white and a roar on black. Coarse grain is soft
/// speckle, not a grid of flat squares.
#[test]
fn grain_reads_the_same_on_light_and_dark_fills() {
    let gpu = Gpu::headless(W, H);
    if !can_draw(&gpu) {
        return;
    }
    let raster = Raster::new(&gpu);
    let mut ui = Ui::new(&gpu);
    let grained = |rect: [f32; 4], c: f32, scale: f32| Quad {
        rect,
        color: [c, c, c, 1.0],
        grain: Some(GrainSpec { amount: 0.5, scale }),
        ..Default::default()
    };
    let px = render(
        &gpu,
        &raster,
        &mut ui,
        vec![grained([0.0, 0.0, 160.0, 160.0], 1.0, 1.0), grained([160.0, 0.0, 160.0, 160.0], 0.0, 1.0)],
        None,
    );
    let (_, white) = mean_sd(&region(&px, 10, 10, 140, 140));
    let (_, black) = mean_sd(&region(&px, 170, 10, 140, 140));
    assert!(white > 10.0 && black > 10.0, "grain is missing: {white} on white, {black} on black");
    let ratio = white.max(black) / white.min(black);
    assert!(ratio < 1.5, "grain is {white:.1} on white and {black:.1} on black, {ratio:.2}× apart");

    // Coarse grain: neighbouring pixels are rarely identical, as they were
    // across every flat square cell.
    let px = render(&gpu, &raster, &mut ui, vec![grained([0.0, 0.0, 320.0, 320.0], 0.5, 6.0)], None);
    let mut same = 0;
    let mut pairs = 0;
    for y in 20..300 {
        for x in 20..300 {
            pairs += 1;
            if px[(y * W + x) as usize][0] == px[(y * W + x + 1) as usize][0] {
                same += 1;
            }
        }
    }
    let flat = same as f64 / pairs as f64;
    assert!(flat < 0.5, "{:.0}% of neighbouring pixels are equal: square cells", flat * 100.0);
}

/// A `size`-square picture: a `border`-pixel red frame round a green middle.
fn framed(size: u32, border: u32) -> TextureData {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let edge = x < border || y < border || x >= size - border || y >= size - border;
            pixels.extend_from_slice(if edge { &[255, 0, 0, 255] } else { &[0, 255, 0, 255] });
        }
    }
    TextureData { pixels, width: size, height: size }
}

/// **A 9-slice's corners can be drawn finer than one texture pixel per unit.**
/// The same 64 px sheet with a 16 px frame draws a 16-unit edge at one pixel
/// per unit and a 4-unit edge at four.
#[test]
fn a_nine_slice_at_four_pixels_per_unit_draws_quarter_corners() {
    let gpu = Gpu::headless(W, H);
    if !can_draw(&gpu) {
        return;
    }
    let mut raster = Raster::new(&gpu);
    let tex = raster.register_texture(&gpu, &framed(64, 16), TexSampling::default());
    let mut ui = Ui::new(&gpu);
    let edge = |ui: &mut Ui, ppu: f32| {
        let q = Quad {
            rect: [20.0, 20.0, 280.0, 280.0],
            texture: "tex".into(),
            slice: [0.25; 4],
            slice_ppu: ppu,
            ..Default::default()
        };
        let px = render(&gpu, &raster, ui, vec![q], Some(tex));
        // Red pixels along the middle row, from the left edge in.
        (20..160).take_while(|&x| {
            let p = px[(160 * W + x) as usize];
            p[0] > 128 && p[1] < 128
        })
        .count()
    };
    let (one, four) = (edge(&mut ui, 0.0), edge(&mut ui, 4.0));
    assert!((15..=17).contains(&one), "the edge at one pixel per unit is {one} wide, not 16");
    assert!((3..=5).contains(&four), "the edge at four pixels per unit is {four} wide, not 4");
}

/// **A large picture drawn small is steady.** A 1024 px checker of single
/// pixels drawn 200 px across, at crisp sampling with no mip chain, picked a
/// different handful of its texels at every size: noise that changed as the
/// panel resized by a pixel. Drawn through its UI mip chain it is an even grey
/// at either size.
#[test]
fn a_large_picture_drawn_small_does_not_shimmer() {
    let gpu = Gpu::headless(W, H);
    if !can_draw(&gpu) {
        return;
    }
    let mut raster = Raster::new(&gpu);
    let n = 1024u32;
    let mut pixels = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let v = if (x + y) % 2 == 0 { 255 } else { 0 };
            pixels.extend_from_slice(&[v, v, v, 255]);
        }
    }
    let tex = raster.register_texture(&gpu, &TextureData { pixels, width: n, height: n }, TexSampling::default());
    let mut ui = Ui::new(&gpu);
    let mut at = |size: f32| {
        let q = Quad { rect: [20.0, 20.0, size, size], texture: "tex".into(), ..Default::default() };
        mean_sd(&region(&render(&gpu, &raster, &mut ui, vec![q], Some(tex)), 40, 40, 150, 150))
    };
    let ((m0, sd0), (m1, sd1)) = (at(200.0), at(201.0));
    assert!(sd0 < 12.0 && sd1 < 12.0, "a 1024 px checker at 200 px is noise: spread {sd0:.1}, {sd1:.1}");
    assert!((m0 - m1).abs() < 4.0, "one pixel of resize moved its brightness {m0:.1} → {m1:.1}");
    assert_eq!(ui.mipped_pictures(), (1, 11), "the picture was not given its eleven-level chain");
}
