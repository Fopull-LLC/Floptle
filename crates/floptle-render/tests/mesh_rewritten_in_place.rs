//! **A mesh is rewritten in place and the picture follows.**
//!
//! A quad registered once is given new vertices, new vertex colours and a
//! different number of triangles through `Raster::rewrite_mesh`. Each draw
//! after shows the new values; the mesh keeps its id and its two GPU
//! buffers; a rewrite the mesh cannot hold is refused whole, leaving what
//! was drawn. Set `FLOPTLE_KEEP_SHOTS=<dir>` to keep the frames as PNGs.

use floptle_render::{instance_of_mat, Globals, Gpu, MaterialParams, MeshData, Raster, Vertex};
use glam::Mat4;

const SIZE: u32 = 64;

/// A quad from (x0, -0.8) to (x1, 0.8) in clip space, facing the camera.
fn quad(x0: f32, x1: f32) -> Vec<Vertex> {
    [[x0, -0.8], [x1, -0.8], [x0, 0.8], [x1, 0.8]]
        .iter()
        .map(|p| Vertex { pos: [p[0], p[1], 0.5], normal: [0.0, 0.0, 1.0], uv: [0.0, 0.0] })
        .collect()
}

#[test]
fn a_rewritten_mesh_draws_its_new_vertices_colours_and_triangles_from_the_same_buffers() {
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("mesh-rewrite"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.scene_format(),
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = Raster::new(&gpu);
    let red = vec![[255, 0, 0, 255]; 4];
    // The left half of the frame, one triangle of the two it has room for.
    let data = MeshData { vertices: quad(-0.9, -0.1), indices: vec![0, 1, 2], colors: Some(red.clone()) };
    let id = raster.register_rewritable(&gpu, &data, 6);
    let (vbuf, ibuf) = raster.mesh_buffers(id).map(|(v, i)| (v.clone(), i.clone())).unwrap();
    let bytes = raster.mesh_gpu_bytes(id);

    let shot = |raster: &mut Raster, name: &str| -> Vec<[u8; 4]> {
        let mut m = MaterialParams::flat([1.0; 3]);
        m.unlit = true;
        m.paint_base = raster.mesh_paint_base(id);
        let globals = Globals { view_proj: Mat4::IDENTITY.to_cols_array_2d(), ..Default::default() };
        raster.draw_scene(&gpu, &view, gpu.depth_view(), globals, &[(id, None, instance_of_mat(Mat4::IDENTITY, &m))], Some([0.0, 0.0, 0.0, 1.0]), None);
        if let Ok(dir) = std::env::var("FLOPTLE_KEEP_SHOTS") {
            std::fs::create_dir_all(&dir).ok();
            floptle_render::probe::save_png(&gpu, &out, &format!("{dir}/{name}.png"));
        }
        floptle_render::probe::readback(&gpu, &out)
    };
    // Clip x in -1..1 and y up; a pixel is (x, y) from the top-left.
    let at = |px: &[[u8; 4]], x: f32, y: f32| {
        let (c, r) = (((x + 1.0) * 0.5 * SIZE as f32) as usize, ((1.0 - y) * 0.5 * SIZE as f32) as usize);
        px[r * SIZE as usize + c]
    };
    let is_red = |p: [u8; 4]| p[0] > 150 && p[1] < 60 && p[2] < 60;
    let is_green = |p: [u8; 4]| p[1] > 150 && p[0] < 60 && p[2] < 60;
    let is_black = |p: [u8; 4]| p[0] < 30 && p[1] < 30 && p[2] < 30;

    // As built: the lower-left triangle of the left quad, red.
    let px = shot(&mut raster, "rewrite_0_built");
    assert!(is_red(at(&px, -0.7, -0.5)), "the built triangle is not drawn: {:?}", at(&px, -0.7, -0.5));
    assert!(is_black(at(&px, -0.3, 0.5)), "the triangle it was not given is drawn: {:?}", at(&px, -0.3, 0.5));
    assert!(is_black(at(&px, 0.5, 0.0)));

    // Moved to the right half, turned green, both triangles.
    let green = vec![[0, 255, 0, 255]; 4];
    raster.rewrite_mesh(&gpu, id, Some(&quad(0.1, 0.9)), Some(&green), Some(&[0, 1, 2, 1, 3, 2])).expect("it fits");
    let px = shot(&mut raster, "rewrite_1_moved");
    assert!(is_black(at(&px, -0.7, -0.5)), "the old vertices still draw: {:?}", at(&px, -0.7, -0.5));
    assert!(is_green(at(&px, 0.3, -0.5)), "the new vertices or colours do not draw: {:?}", at(&px, 0.3, -0.5));
    assert!(is_green(at(&px, 0.7, 0.5)), "the second triangle does not draw: {:?}", at(&px, 0.7, 0.5));
    let now = raster.mesh_buffers(id).unwrap();
    assert!(*now.0 == vbuf && *now.1 == ibuf, "the rewrite made new GPU buffers");
    assert_eq!(raster.mesh_gpu_bytes(id), bytes);

    // Colours alone, then triangles alone: each leaves the rest as it was.
    raster.rewrite_mesh(&gpu, id, None, Some(&red), None).unwrap();
    raster.rewrite_mesh(&gpu, id, None, None, Some(&[1, 3, 2])).unwrap();
    let px = shot(&mut raster, "rewrite_2_one_triangle");
    assert!(is_red(at(&px, 0.7, 0.5)), "{:?}", at(&px, 0.7, 0.5));
    assert!(is_black(at(&px, 0.3, -0.5)), "a triangle taken away still draws: {:?}", at(&px, 0.3, -0.5));

    // What the mesh cannot hold is refused whole: the picture stays.
    assert!(raster.rewrite_mesh(&gpu, id, Some(&quad(-0.9, -0.1)[..3]), None, None).unwrap_err().contains("3 vertices for a mesh of 4"));
    assert!(raster.rewrite_mesh(&gpu, id, Some(&quad(-0.9, -0.1)), None, Some(&[0, 1, 2, 1, 3, 2, 0, 1, 2])).unwrap_err().contains("room for 6"));
    assert!(raster.rewrite_mesh(&gpu, id, Some(&quad(-0.9, -0.1)), Some(&green[..2]), None).is_err());
    let again = shot(&mut raster, "rewrite_3_refused");
    assert!(px == again, "a refused rewrite changed what is drawn");
    assert_eq!(raster.mesh_rewrites(), 3, "a refused rewrite was counted");

    // No triangles: nothing drawn, and the mesh is still there to fill again.
    raster.rewrite_mesh(&gpu, id, None, None, Some(&[])).unwrap();
    let px = shot(&mut raster, "rewrite_4_dry");
    assert!(px.iter().all(|p| is_black(*p)), "a mesh with no triangles drew something");
    raster.rewrite_mesh(&gpu, id, None, None, Some(&[0, 1, 2, 1, 3, 2])).unwrap();
    assert!(is_red(at(&shot(&mut raster, "rewrite_5_wet"), 0.3, -0.5)));

    // A mesh registered without colours has no block to write them into.
    let plain = raster.register_rewritable(&gpu, &MeshData { vertices: quad(-0.5, 0.5), indices: vec![0, 1, 2], colors: None }, 3);
    assert!(raster.rewrite_mesh(&gpu, plain, None, Some(&green), None).unwrap_err().contains("without any"));
}
