//! **Anti-aliasing smooths an edge and leaves flat ground alone.**
//!
//! A bright card at a slant across a dark frame, drawn once and passed through
//! the post chain with FXAA off and on. Off, the edge is a hard stair: every
//! pixel is the card or the background. On, the stair's steps are blended, so
//! most rows the edge crosses gain an in-between pixel. Away from the edge
//! nothing changes: a pass that blurred the whole frame would soften the edge
//! too, and must not pass.

use floptle_render::post::{PostSettings, PostStack};
use floptle_render::{instance_of_mat, plane, Globals, Gpu, MaterialParams, Raster};
use glam::{Mat4, Vec3};

const SIZE: u32 = 64;

fn shot(gpu: &Gpu, raster: &mut Raster, post: &PostStack, out: &wgpu::Texture, fxaa: bool) -> Vec<f32> {
    // Big enough that only its left edge is in the frame.
    let mesh = raster.register(gpu, &plane(4.0), None);
    let mut m = MaterialParams::flat([1.0; 3]);
    m.emissive = [1.0; 3];
    m.emissive_strength = 1.0;
    // The card's left edge, turned 20 degrees off vertical, through the middle
    // of the frame.
    let model = Mat4::from_translation(Vec3::new(4.0 / 0.35f32.cos(), 0.0, 0.5)) * Mat4::from_rotation_z(0.35);
    let globals = Globals {
        view_proj: Mat4::IDENTITY.to_cols_array_2d(),
        light_dir: [0.0, 0.0, -1.0, 0.0],
        light_color: [0.0; 4],
        ambient: [0.0; 4],
        ..Default::default()
    };
    raster.draw_scene(gpu, post.input_view(), gpu.depth_view(), globals, &[(mesh, None, instance_of_mat(model, &m))], Some([0.0, 0.0, 0.0, 1.0]), None);
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    post.run(gpu, &PostSettings { fxaa, ..Default::default() }, None, &view);
    floptle_render::probe::readback(gpu, out).iter().map(|p| p[1] as f32 / 255.0).collect()
}

#[test]
fn fxaa_blends_the_steps_of_an_edge_and_leaves_flat_ground_alone() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("fxaa"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut raster = Raster::new(&gpu);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let hard = shot(&gpu, &mut raster, &post, &out, false);
    let smooth = shot(&gpu, &mut raster, &post, &out, true);

    let top = hard.iter().cloned().fold(0.0f32, f32::max);
    assert!(top > 0.5, "the card did not draw (brightest pixel {top})");
    // Rows the edge crosses with a pixel part-way between card and background.
    // FXAA leaves the middle row of each step whole and blends the rows either
    // side of it, so about two in three.
    let blended_rows = |px: &[f32]| {
        (0..SIZE as usize)
            .filter(|y| px[y * SIZE as usize..(y + 1) * SIZE as usize].iter().any(|v| *v > top * 0.05 && *v < top * 0.95))
            .count()
    };
    let (before, after) = (blended_rows(&hard), blended_rows(&smooth));
    assert!(before <= 2, "without anti-aliasing the edge should be a hard stair; {before} rows were blended");
    assert!(after >= SIZE as usize / 2, "FXAA blended only {after} of {SIZE} rows the edge crosses");

    // Far from the edge, both renders agree exactly.
    let far = |i: usize| {
        let (x, y) = ((i % SIZE as usize) as i32, (i / SIZE as usize) as i32);
        // Distance from the column the edge crosses at this row, generously.
        let edge_x = SIZE as i32 / 2 + ((y - SIZE as i32 / 2) as f32 * 0.35f32.tan()) as i32;
        (x - edge_x).abs() > 6
    };
    let moved = (0..hard.len()).filter(|&i| far(i) && hard[i] != smooth[i]).count();
    assert_eq!(moved, 0, "FXAA changed {moved} pixels nowhere near the edge");
}
