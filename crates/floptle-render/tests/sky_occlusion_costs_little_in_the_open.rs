//! **`skyOcclusion` costs next to nothing where nothing is in reach.**
//!
//! A sea on a planet sits inside the planet's terrain volume, so every ray of
//! the op's cone finds the terrain in its relevance sweep and marches. A
//! frame of open sea paid for four sweeps over every field piece and four
//! field reads a pixel, the same at a reach of 0.05 as at 24, and the water's
//! pass went from 1.3 ms to 4.5 ms. Now one sweep around the point covers all
//! four rays and one field read at the point settles it: the stored field
//! never reads farther than the true surface, so a point further from every
//! surface than the cone can reach is open sky.
//!
//! The answers keep their meaning: water under a rock roof in the same
//! terrain still reads dark at a reach that gets to the roof, and open water
//! reads 1.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_field::BakedSdf;
use floptle_render::{
    instance_of_mat, pass_prelude, FlslBindingId, FlslBlend, Globals, Gpu, MaterialParams, MeshData, MeshId, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, Vertex,
};

const W: u32 = 1280;
const H: u32 = 720;
/// The seabed's depth under the sea, which sits at y = 0.
const DEPTH: f32 = 15.0;
/// The rock roof: a slab from y = 3 to 8 over everything at z < -20.
const ROOF: (f32, f32, f32) = (3.0, 8.0, -20.0);

/// The terrain the sea lies in: a seabed, an island rising out of it far
/// off, and a slab of rock overhanging the water on one side. Distances are
/// understated (scaled by 0.6), as a generated field's are.
fn terrain() -> BakedSdf {
    let (dims, center, half) = ([96u32, 40, 96], [0.0f32, -10.0, 0.0], [80.0f32, 25.0, 80.0]);
    let n = (dims[0] * dims[1] * dims[2]) as usize;
    let mut distance = Vec::with_capacity(n);
    for iz in 0..dims[2] {
        for iy in 0..dims[1] {
            for ix in 0..dims[0] {
                let at = |i: u32, a: usize| center[a] - half[a] + (i as f32 + 0.5) / dims[a] as f32 * 2.0 * half[a];
                let (x, y, z) = (at(ix, 0), at(iy, 1), at(iz, 2));
                let island = 12.0 - 0.4 * ((x - 60.0).powi(2) + (z - 60.0).powi(2)).sqrt();
                let ground = y - (-DEPTH).max(island);
                // The roof, as a box out past the volume on three sides.
                let q = [x.abs() - 200.0, (y - 0.5 * (ROOF.0 + ROOF.1)).abs() - 0.5 * (ROOF.1 - ROOF.0), (z + 200.0).abs() - (200.0 + ROOF.2)];
                let outside = (q.iter().map(|v| v.max(0.0).powi(2)).sum::<f32>()).sqrt();
                let roof = outside + q[0].max(q[1]).max(q[2]).min(0.0);
                distance.push(ground.min(roof) * 0.6);
            }
        }
    }
    BakedSdf { dims, center, half_extent: half, distance, color: vec![[120, 110, 90, 0]; n] }
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

struct Sea {
    gpu: Gpu,
    raster: Raster,
    raymarch: Raymarch,
    out: wgpu::Texture,
    sheet: MeshId,
    bed: BakedSdf,
}

impl Sea {
    /// `None` on a device that cannot build the raster pass.
    fn new() -> Option<Sea> {
        let gpu = Gpu::headless(W, H);
        if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            return None;
        }
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sea"),
            size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: gpu.config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let mut raster = Raster::new(&gpu);
        let mut raymarch = Raymarch::new(&gpu);
        let sheet = raster.register(&gpu, &quad(), None);
        let bed = terrain();
        assert_eq!(raymarch.set_volumes(&gpu, &[&bed]), 1);
        Some(Sea { gpu, raster, raymarch, out, sheet, bed })
    }

    /// A water shader whose cover is `open`.
    fn shader(&mut self, open: &str) -> FlslBindingId {
        let src = format!(
            "shader sea {{\n  stage fragment\n  \
             let n = normalize(normal + vec3(sin(worldPos.x * 3.1) * 0.04, 0, cos(worldPos.z * 2.3) * 0.04))\n  \
             let open = {open}\n  \
             output color = vec4(vec3(open) * 0.9 + vec3(0.05) * pow(1 - saturate(dot(n, viewDir)), 5), 1)\n}}\n"
        );
        let compiled = floptle_shader::compile_fragment(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}\n{}", e.message, compiled.chunk));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = self.raster.register_flsl_shader(&self.gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        self.raster.set_flsl_binding(&self.gpu, None, id, &params, &[])
    }

    /// The camera at `eye` looking straight down, with the terrain and a row
    /// of collider proxies (a dock of boxes far off) bound. Returns the
    /// frame's globals and its view-projection.
    fn stage(&mut self, eye: DVec3) -> (Globals, glam::Mat4, DVec3) {
        let cam = RenderCamera::new(
            eye,
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
            Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.1, far: 2000.0 },
        );
        let rel = |p: [f32; 3]| (DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) - eye).as_vec3();
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let mut vol_center = [[0.0f32; 4]; floptle_render::MAX_VOLUMES];
        let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; floptle_render::MAX_VOLUMES];
        let c = rel(self.bed.center);
        // Kind 3: meshed terrain, which casts and occludes and is not marched.
        vol_center[0] = [c.x, c.y, c.z, 3.0];
        let h = self.bed.half_extent;
        vol_half[0] = [h[0], h[1], h[2], 0.6];
        let mut prox_a = [[0.0f32; 4]; 32];
        let mut prox_b = [[0.0f32; 4]; 32];
        const DOCK: usize = 4;
        for i in 0..DOCK {
            let c = rel([-30.0 + 20.0 * i as f32, 3.0, 150.0]);
            prox_a[i] = [c.x, c.y, c.z, 0.0];
            prox_b[i] = [4.0, 3.0, 6.0, 2.0];
        }
        let rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [0.3, 0.3, 0.3, 0.0],
            params: [0.0, 0.0, 0.3, 0.0],
            vol_center,
            vol_half,
            // Screen-space AO and no probes: `skyVisibility` would fall back
            // to the cone too.
            ao_params: [0.0, 1.0, 3.0, 0.0],
            prox_count: [DOCK as f32, 0.0, 0.0, 0.0],
            prox_a,
            prox_b,
            prox_rot: [[0.0, 0.0, 0.0, 1.0]; 32],
            ..Default::default()
        };
        self.raymarch.upload_globals(&self.gpu, rm);
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [0.3, 0.3, 0.3, 0.0],
            ..Default::default()
        };
        (globals, view_proj, eye)
    }

    /// Draw the sea, a sheet far wider than the frame, through `bind`.
    fn draw(&mut self, globals: Globals, eye: DVec3, bind: FlslBindingId) {
        let place = Transform { translation: DVec3::ZERO, rotation: Quat::IDENTITY, scale: Vec3::new(2000.0, 1.0, 2000.0) };
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
    }

    /// The red channel as the shader wrote it, linear.
    fn read(&self) -> Vec<f32> {
        let srgb = self.gpu.config.format.is_srgb();
        floptle_render::probe::readback(&self.gpu, &self.out)
            .iter()
            .map(|p| {
                let v = p[0] as f32 / 255.0;
                if srgb { if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } } else { v }
            })
            .collect()
    }
}

/// The pixel that shows the sea at world (x, 0, z).
fn pixel(view_proj: glam::Mat4, eye: DVec3, x: f32, z: f32) -> usize {
    let rel = (DVec3::new(x as f64, 0.0, z as f64) - eye).as_vec3();
    let c = view_proj * rel.extend(1.0);
    let (nx, ny) = (c.x / c.w, c.y / c.w);
    let px = ((nx * 0.5 + 0.5) * W as f32) as u32;
    let py = ((0.5 - ny * 0.5) * H as f32) as u32;
    assert!(px < W && py < H, "({x}, {z}) is out of the frame");
    (py * W + px) as usize
}

/// Water under the rock roof reads dark at a reach that gets to the roof,
/// and open water beside it reads 1 at any reach.
#[test]
fn water_under_a_roof_reads_dark_and_open_water_reads_open() {
    let Some(mut sea) = Sea::new() else { return };
    let eye = DVec3::new(0.0, 60.0, -10.0);
    let (globals, view_proj, eye) = sea.stage(eye);
    let under = pixel(view_proj, eye, 0.0, -38.0);
    let open = pixel(view_proj, eye, 0.0, 18.0);
    let read = |sea: &mut Sea, expr: &str| {
        let bind = sea.shader(expr);
        sea.draw(globals, eye, bind);
        let px = sea.read();
        (px[under], px[open])
    };
    // The fixture: the plain shader draws 0.9 over the whole sea.
    let (u, o) = read(&mut sea, "1.0");
    assert!(u > 0.85 && o > 0.85, "the plain water reads {u:.3} under the roof and {o:.3} in the open");
    for reach in ["24", "6"] {
        let (u, o) = read(&mut sea, &format!("skyOcclusion(worldPos, vec3(0, 1, 0), {reach})"));
        assert!(u < 0.25, "at a reach of {reach}, water three units under a rock roof reads {u:.3}");
        assert!(o > 0.85, "at a reach of {reach}, open water reads {o:.3}");
    }
    // The shortest reach still reads open water as open. (It does not read
    // the roof as open: on a field this coarse every ray starts a voxel and
    // more off the surface, and its first step reaches the roof.)
    let (_, o) = read(&mut sea, "skyOcclusion(worldPos, vec3(0, 1, 0), 0.05)");
    assert!(o > 0.85, "at a reach of 0.05 open water reads {o:.3}");
    // `skyVisibility` falls back to the cone here (no AO term, no probes).
    let (u, o) = read(&mut sea, "skyVisibility(worldPos)");
    assert!(u < 0.25 && o > 0.85, "skyVisibility reads {u:.3} under the roof and {o:.3} in the open");
}

/// GPU milliseconds the sea's draw takes through `bind`: the least of
/// several rounds of a batch of frames, on the GPU's own clock.
fn cost(sea: &mut Sea, timer: &mut floptle_render::GpuTimer, globals: Globals, eye: DVec3, binds: &[FlslBindingId]) -> Vec<f32> {
    const FRAMES: usize = 20;
    let mut best = vec![f32::MAX; binds.len()];
    for _ in 0..8 {
        if !timer.begin() {
            continue;
        }
        for (k, bind) in binds.iter().enumerate() {
            timer.mark(&sea.gpu, &format!("v{k}"));
            for _ in 0..FRAMES {
                sea.draw(globals, eye, *bind);
            }
            timer.mark_gap(&sea.gpu);
        }
        timer.end(&sea.gpu);
        for _ in 0..200 {
            sea.gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok();
            timer.poll();
            if timer.begin() {
                break;
            }
        }
        for (k, b) in best.iter_mut().enumerate() {
            if let Some(s) = timer.spans().iter().find(|s| s.label == format!("v{k}")) {
                *b = b.min(s.ms / FRAMES as f32);
            }
        }
    }
    best
}

/// On a frame of open sea, `skyOcclusion(worldPos, up, 0.05)` costs within
/// a tenth of a millisecond of the same shader without it.
#[test]
fn sky_occlusion_over_open_sea_costs_next_to_nothing() {
    let Some(mut sea) = Sea::new() else { return };
    // A software rasteriser's clock says nothing about a GPU's.
    if sea.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&sea.gpu) else { return };
    // Twelve units over open water, looking down: sea fills the frame, all of
    // it inside the terrain's volume and its content's bound.
    let (globals, _, eye) = sea.stage(DVec3::new(0.0, 12.0, 30.0));
    let plain = sea.shader("1.0");
    let near = sea.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 0.05)");
    // The fixture: open water reads open, so the op is doing what it should.
    sea.draw(globals, eye, near);
    let px = sea.read();
    let lo = px.iter().copied().fold(f32::MAX, f32::min);
    assert!(lo > 0.85, "open sea reads as low as {lo:.3}");
    let t = cost(&mut sea, &mut timer, globals, eye, &[plain, near, plain, near]);
    let (without, with) = (t[0].min(t[2]), t[1].min(t[3]));
    assert!(without < f32::MAX && with < f32::MAX, "no timing came back");
    assert!(
        with - without <= 0.1,
        "skyOcclusion at a reach of 0.05 over open sea costs {:.3} ms a {W}x{H} frame ({without:.3} ms without it, {with:.3} ms with)",
        with - without
    );
}
