//! **The swell a shader draws is the swell the solver floats things on.**
//!
//! `waterSwell(objectPos)` and `waterSwellNormal(objectPos)` read a body of
//! water's waves from the block `Swell::pack` writes. This draws both across
//! a sheet, 80 m of sea in 64 pixels, and compares what the GPU computed at
//! each pixel against `Swell::offset`, the CPU's own sum: on a flat sea and
//! on a small round one, where the waves are laid round the sphere.
//!
//! A colour is 8 bits, so each value is drawn twice: once whole, coarsely,
//! and once wrapped every quarter metre, which is fine to a few millimetres.

use floptle_core::math::{DMat3, DVec3};
use floptle_core::swell::{Lay, Swell, Wave};
use floptle_render::{instance_of_mat, pass_prelude, FlslBlend, Globals, Gpu, MaterialParams, MeshData, Raster, Vertex};
use glam::Mat4;

const SIZE: u32 = 64;
/// Half the sheet's width, metres.
const HALF: f32 = 40.0;

fn waves() -> Vec<Wave> {
    (0..8)
        .map(|i| {
            let a = 0.4 + i as f32 * 0.83;
            Wave {
                amplitude: 0.9 / (1.0 + i as f32),
                wavelength: 60.0 / (1.0 + i as f32 * 0.5),
                direction: [a.cos(), a.sin()],
                steepness: 0.7,
                speed: None,
                phase: i as f32 * 1.3,
            }
        })
        .collect()
}

/// What a pixel's byte holds, as the shader wrote it.
fn decode(gpu: &Gpu, byte: u8) -> f64 {
    let v = byte as f64 / 255.0;
    if gpu.config.format.is_srgb() {
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    } else {
        v
    }
}

/// How far apart two values wrapped into 0..1 are.
fn wrapped(a: f64, b: f64) -> f64 {
    let d = (a - b).rem_euclid(1.0);
    d.min(1.0 - d)
}

#[test]
fn the_shaders_swell_is_the_solvers() {
    assert_eq!(floptle_shader::transpile::SWELL_LANES, floptle_core::swell::SWELL_BLOCK, "the shader's block and the engine's differ in size");
    let gpu = Gpu::headless(SIZE, SIZE);
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        // No raster pass on this device, so there is nothing to draw with.
        return;
    }
    let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("swell-out"),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = out.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = Raster::new(&gpu);
    // A sheet 80 m across that fills the frame: its own x and y are the
    // screen's, in metres.
    let quad = MeshData {
        vertices: [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]]
            .iter()
            .map(|p| Vertex { pos: [p[0] * HALF, p[1] * HALF, 0.5], normal: [0.0, 0.0, 1.0], uv: [0.5, 0.5] })
            .collect(),
        indices: vec![0, 1, 2, 1, 3, 2],
        colors: None,
    };
    let mesh = raster.register(&gpu, &quad, None);
    let view_proj = Mat4::from_scale(glam::Vec3::new(1.0 / HALF, 1.0 / HALF, 1.0));

    // The sheet's x is the water's x, its y the water's -z, its z the
    // water's up; and the water's frame is turned in the world.
    let node_to_water = DMat3::from_cols(DVec3::X, DVec3::NEG_Z, DVec3::Y);
    let water_to_world = DMat3::from_rotation_x(0.3) * DMat3::from_rotation_y(-0.7);
    let t = 12.5;

    // One shader draws all four pictures, chosen by a uniform, so there is
    // one pipeline to build: the offset or the normal, whole or wrapped.
    let src = "shader s {\n  stage fragment\n  uniform normals: float = 0\n  uniform fine: float = 0\n  \
               let d = waterSwell(objectPos)\n  let n = waterSwellNormal(objectPos)\n  \
               let whole = mix(d * 0.125 + vec3(0.5), n * 0.5 + vec3(0.5), normals)\n  \
               let wrapped = mix(fract(d * 4), fract(n * 8), normals)\n  \
               output color = vec4(mix(whole, wrapped, fine), 1)\n}\n";
    let compiled = floptle_shader::compile_fragment(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
    floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}", e.message));
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    let id = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Opaque, None);
    let mut draw = |normals: bool, fine: bool, lanes: &[[f32; 4]; floptle_core::swell::SWELL_BLOCK]| -> Vec<[f64; 3]> {
        let mut params = compiled.pack_params(
            &|name| Some([if (name == "normals" && normals) || (name == "fine" && fine) { 1.0 } else { 0.0 }, 0.0, 0.0, 0.0]),
            &|_| None,
        );
        assert_eq!(params.len(), 2 * 16 + 16 * floptle_core::swell::SWELL_BLOCK, "the swell's lanes follow the shader's own");
        compiled.write_swell(&mut params, lanes);
        let bind = raster.set_flsl_binding(&gpu, None, id, &params, &[]);
        let globals = Globals { view_proj: view_proj.to_cols_array_2d(), ambient: [1.0, 1.0, 1.0, 0.0], ..Default::default() };
        let mut m = MaterialParams::flat([1.0; 3]);
        m.unlit = true;
        let draws = [(mesh, None, bind, instance_of_mat(Mat4::IDENTITY, &m))];
        raster.draw_scene_with(&gpu, &view, gpu.depth_view(), globals, &[], &draws, &[], Some([0.0, 0.0, 0.0, 1.0]), None);
        floptle_render::probe::readback(&gpu, &out).iter().map(|p| [decode(&gpu, p[0]), decode(&gpu, p[1]), decode(&gpu, p[2])]).collect()
    };

    let swell = Swell::new(&waves(), DVec3::Y).unwrap();
    for (name, lay, at) in [
        ("a flat sea", Lay::Flat { level: 0.0 }, DVec3::new(13.0, 0.0, -8.0)),
        ("a round sea", Lay::Round { radius: 500.0 }, DVec3::new(30.0, 500.0, 20.0)),
    ] {
        let lanes = swell.pack(lay, t, (node_to_water, at), water_to_world);
        // The CPU's answer at a pixel: the offset in the sheet's own space,
        // and the normal in the world's.
        let cpu = |i: u32, j: u32| -> (DVec3, DVec3) {
            let p = DVec3::new(
                ((i as f64 + 0.5) / SIZE as f64 * 2.0 - 1.0) * HALF as f64,
                (1.0 - (j as f64 + 0.5) / SIZE as f64 * 2.0) * HALF as f64,
                0.5,
            );
            let (off, n) = swell.offset(node_to_water * p + at, lay, t);
            (node_to_water.transpose() * off, water_to_world * n)
        };
        let off_coarse = draw(false, false, &lanes);
        let off_fine = draw(false, true, &lanes);
        let n_coarse = draw(true, false, &lanes);
        let n_fine = draw(true, true, &lanes);
        let (mut worst_off, mut worst_n, mut tallest) = (0.0f64, 0.0f64, 0.0f64);
        for j in (2..SIZE - 2).step_by(3) {
            for i in (2..SIZE - 2).step_by(3) {
                let k = (j * SIZE + i) as usize;
                let (off, n) = cpu(i, j);
                tallest = tallest.max(off.length());
                for c in 0..3 {
                    // Whole, to within what 8 bits can say of ±4 m…
                    assert!(
                        (off_coarse[k][c] - (off[c] / 8.0 + 0.5)).abs() < 0.012,
                        "{name}: the offset at pixel {i}, {j} lane {c} is {:.3} m on the GPU and {:.3} m on the CPU",
                        (off_coarse[k][c] - 0.5) * 8.0,
                        off[c]
                    );
                    assert!((n_coarse[k][c] - (n[c] * 0.5 + 0.5)).abs() < 0.012, "{name}: the normal at {i}, {j}: {:?} against {n:?}", n_coarse[k]);
                    // …and wrapped, to millimetres.
                    worst_off = worst_off.max(wrapped(off_fine[k][c], (off[c] * 4.0).rem_euclid(1.0)) / 4.0);
                    worst_n = worst_n.max(wrapped(n_fine[k][c], (n[c] * 8.0).rem_euclid(1.0)) / 8.0);
                }
            }
        }
        assert!(tallest > 1.0, "the fixture: {name} moves its surface by {tallest:.2} m at most");
        assert!(worst_off < 0.006, "{name}: the GPU's surface is {:.1} mm off the CPU's", worst_off * 1000.0);
        assert!(worst_n < 0.004, "{name}: the GPU's normal is {worst_n:.4} off the CPU's");
    }

    // On no water the shader draws a still surface: no offset, and the
    // mesh's own normal.
    let still = [[0.0f32; 4]; floptle_core::swell::SWELL_BLOCK];
    let off = draw(false, false, &still);
    let n = draw(true, false, &still);
    let k = (SIZE * SIZE / 2 + SIZE / 2) as usize;
    assert!(off[k].iter().all(|c| (c - 0.5).abs() < 0.012), "{:?}", off[k]);
    assert!((n[k][2] - 1.0).abs() < 0.012 && (n[k][0] - 0.5).abs() < 0.012, "the mesh's normal is +z: {:?}", n[k]);
}
