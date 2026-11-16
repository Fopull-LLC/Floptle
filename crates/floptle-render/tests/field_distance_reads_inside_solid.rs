//! **`fieldDistance` reads negative inside rock, deep under the surface too.**
//!
//! The terrain's field on the GPU is a dense stand-in baked from the voxel
//! field (`ChunkField::to_dense`): signed, with the solid side held at minus
//! the band (four of the terrain's voxels) however deep the rock goes, and
//! the air side grown out to a true distance so a shadow march can take long
//! steps. A point several metres inside a slab reads below zero, and a point
//! over open ground reads its height.
//!
//! This draws a sheet at a chosen height through a shader that writes the
//! op's value, over a slab baked the way the editor bakes a terrain.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_field::{BakedSdf, ChunkField};
use floptle_render::{
    instance_of_mat, pass_prelude, FlslBindingId, FlslBlend, Globals, Gpu, MaterialParams, MeshData, MeshId, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, Vertex,
};

const SIZE: u32 = 64;
/// The longest axis of the editor's terrain stand-in, in cells
/// (`TERRAIN_SHADOW_MAX_DIM`).
const PROXY_CELLS: u32 = 192;
/// The written value is `0.5 + d / RANGE`, so it reads ±RANGE/2.
const RANGE: f32 = 40.0;

/// A 10-unit slab, its top at y = 0, `half` across on x and z.
fn slab(half: f32, voxel: f32) -> BakedSdf {
    let mut field = ChunkField::new(voxel);
    field.fill_slab(Vec3::new(-half, -10.0, -half), Vec3::new(half, 0.0, half), 0.0, [0.5, 0.45, 0.4]);
    field.to_dense(PROXY_CELLS).expect("a slab has bounds")
}

/// A flat quad on the XZ plane facing +y, one unit across.
fn quad() -> MeshData {
    MeshData {
        vertices: [[-0.5, -0.5], [0.5, -0.5], [-0.5, 0.5], [0.5, 0.5]]
            .iter()
            .map(|p| Vertex { pos: [p[0], 0.0, p[1]], normal: [0.0, 1.0, 0.0], uv: [p[0] + 0.5, p[1] + 0.5] })
            .collect(),
        indices: vec![0, 2, 1, 1, 2, 3],
        colors: None,
    }
}

struct Rig {
    gpu: Gpu,
    raster: Raster,
    raymarch: Raymarch,
    out: wgpu::Texture,
    sheet: MeshId,
}

impl Rig {
    /// `None` on a device that cannot build the raster pass.
    fn new() -> Option<Rig> {
        let gpu = Gpu::headless(SIZE, SIZE);
        if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            return None;
        }
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("field-distance"),
            size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut raster = Raster::new(&gpu);
        let raymarch = Raymarch::new(&gpu);
        let sheet = raster.register(&gpu, &quad(), None);
        Some(Rig { gpu, raster, raymarch, out, sheet })
    }

    fn shader(&mut self, expr: &str) -> FlslBindingId {
        let src = format!("shader probe {{\n  stage fragment\n  output color = vec4(vec3({expr}), 1)\n}}\n");
        let compiled = floptle_shader::compile_fragment(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}\n{}", e.message, compiled.chunk));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = self.raster.register_flsl_shader(&self.gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        self.raster.set_flsl_binding(&self.gpu, None, id, &params, &[])
    }

    /// The value `expr` writes on a sheet at height `y`, read at the middle
    /// of the frame, with `bake` bound as a meshed terrain (kind 3).
    fn read(&mut self, bake: &BakedSdf, y: f64, expr: &str) -> f32 {
        assert_eq!(self.raymarch.set_volumes(&self.gpu, &[bake]), 1);
        let eye = DVec3::new(0.0, y + 6.0, 0.0);
        let cam = RenderCamera::new(
            eye,
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            Projection::Perspective { fov_y: 40f32.to_radians(), near: 0.1, far: 500.0 },
        );
        let view_proj = cam.view_proj(1.0);
        let mut vol_center = [[0.0f32; 4]; floptle_render::MAX_VOLUMES];
        let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; floptle_render::MAX_VOLUMES];
        let c = (DVec3::from(bake.center.map(f64::from)) - eye).as_vec3();
        vol_center[0] = [c.x, c.y, c.z, 3.0];
        let h = bake.half_extent;
        vol_half[0] = [h[0], h[1], h[2], 0.6];
        let rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [0.3, 0.3, 0.3, 0.0],
            params: [0.0, 0.0, 0.1, 0.0],
            vol_center,
            vol_half,
            ..Default::default()
        };
        self.raymarch.upload_globals(&self.gpu, rm);
        let globals = Globals { view_proj: view_proj.to_cols_array_2d(), light_color: [0.0; 4], ..Default::default() };
        let bind = self.shader(expr);
        let place = Transform { translation: DVec3::new(0.0, y, 0.0), rotation: Quat::IDENTITY, scale: Vec3::new(40.0, 1.0, 40.0) };
        let sheet = instance_of_mat(place.render_matrix(eye), &MaterialParams::flat([1.0; 3]));
        let view = self.out.create_view(&wgpu::TextureViewDescriptor::default());
        self.raster.draw_scene_with(
            &self.gpu,
            &view,
            self.gpu.depth_view(),
            globals,
            &[],
            &[(self.sheet, None, bind, sheet)],
            &[],
            Some([0.0, 0.0, 0.0, 1.0]),
            Some(self.raymarch.field_bind()),
        );
        let px = floptle_render::probe::readback(&self.gpu, &self.out);
        let v = px[(SIZE / 2 * SIZE + SIZE / 2) as usize][0] as f32 / 255.0;
        if self.gpu.config.format.is_srgb() {
            if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
        } else {
            v
        }
    }

    /// `fieldDistance` at height `y` over the middle of `bake`.
    fn distance(&mut self, bake: &BakedSdf, y: f64) -> f32 {
        (self.read(bake, y, &format!("saturate(0.5 + fieldDistance(worldPos) / {RANGE:.1})")) - 0.5) * RANGE
    }
}

/// Three units inside a ten-unit slab reads below zero, at the terrain's own
/// voxel size and on a terrain wide enough that its stand-in's cells are
/// several voxels across; three units over it reads about three.
#[test]
fn inside_a_slab_reads_negative_and_over_it_reads_its_height() {
    let Some(mut rig) = Rig::new() else { return };
    for (what, bake) in [("a small terrain", slab(24.0, 0.5)), ("a wide terrain", slab(400.0, 2.0))] {
        let cell = 2.0 * bake.half_extent[0] / bake.dims[0] as f32;
        let inside = rig.distance(&bake, -3.0);
        let over = rig.distance(&bake, 3.0);
        assert!(inside < -1.0, "{what} (cells {cell:.1} units): 3 units inside a 10-unit slab reads {inside:.2}");
        assert!((over - 3.0).abs() < 1.0, "{what} (cells {cell:.1} units): 3 units over open ground reads {over:.2}");
    }
}

/// A cave: the floor's top at y = 0 and a 5-unit roof from y = 6 to 11.
/// Inside the roof reads below zero, the cave's air above it — at the
/// terrain's voxel size, where the stand-in's cells are the voxels.
///
/// (On a terrain so wide that a stand-in cell is as thick as the roof, the
/// roof is finer than the field can hold and reads as air; the op's doc
/// says so. That is resolution, not sign: nothing there is to recover.)
#[test]
fn inside_a_cave_roof_reads_negative_and_the_cave_reads_air() {
    let Some(mut rig) = Rig::new() else { return };
    let mut field = ChunkField::new(0.5);
    field.fill_slab(Vec3::new(-24.0, -10.0, -24.0), Vec3::new(24.0, 0.0, 24.0), 0.0, [0.5, 0.45, 0.4]);
    field.fill_slab(Vec3::new(-24.0, 6.0, -24.0), Vec3::new(24.0, 11.0, 24.0), 11.0, [0.5, 0.45, 0.4]);
    let bake = field.to_dense(PROXY_CELLS).expect("the cave has bounds");
    let air = rig.distance(&bake, 3.0);
    assert!((air - 3.0).abs() < 1.0, "the middle of the cave, 3 units from floor and roof, reads {air:.2}");
    for y in [7.0, 8.5, 10.0] {
        let d = rig.distance(&bake, y);
        assert!(d < -0.5, "{y} units up, inside the roof, reads {d:.2}");
    }
}
