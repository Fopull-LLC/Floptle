//! **`skyOcclusion` over shallow water by a beach.**
//!
//! A sea near a shore sits inside the terrain's volume with its floor a few
//! metres down, and collider proxies (boats, rocks, a dock) stand around it.
//! The cone's rays all go up, but a test that does not know which way they
//! point sees the floor within reach and the proxies within its sphere, and
//! marches four rays a pixel for water that is plainly open.
//!
//! The fixture is a generated terrain the way a game makes one (a chunk field
//! sampled into its coarse shadow stand-in), with thirty proxies along the
//! shore, seen from eye level and from eighteen metres up.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_field::BakedSdf;
use floptle_render::{
    instance_of_mat, pass_prelude, FlslBindingId, FlslBlend, Globals, Gpu, MaterialParams, MeshData, MeshId, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, Vertex,
};

const W: u32 = 1280;
const H: u32 = 720;
/// Where the shore crosses the sea level (y = 0), along x.
const SHORE: f32 = 40.0;
/// Collider proxies along the shore.
const PROXIES: usize = 30;

/// The ground's height at (x, z): a seabed shelving up to the shore at about
/// one in eight, and land rising past it.
fn ground(x: f32, z: f32) -> f32 {
    let u = x + 8.0 * (z / 23.0).sin();
    let g = if u < SHORE { -(0.12 * (SHORE - u)).min(25.0) } else { (0.3 * (u - SHORE)).min(14.0) };
    g + 0.6 * (x / 7.0).sin() * (z / 9.0).sin()
}

/// The terrain, generated into a chunk field and taken down to its shadow
/// stand-in exactly as a terrain node's is: a seabed, a beach, and a slab of
/// rock overhanging the shallows.
fn terrain() -> BakedSdf {
    let mut field = floptle_field::ChunkField::new(2.0);
    let (lo, hi) = (Vec3::new(-200.0, -30.0, -200.0), Vec3::new(200.0, 24.0, 200.0));
    field.fill_with(
        lo,
        hi,
        |p| {
            let bed = (p.y - ground(p.x, p.z)) * 0.7;
            // The overhang: a slab from y = 4 to 7 over x 18..34, z -34..-18.
            let q = (p - Vec3::new(26.0, 5.5, -26.0)).abs() - Vec3::new(8.0, 1.5, 8.0);
            let slab = q.max(Vec3::ZERO).length() + q.max_element().min(0.0);
            bed.min(slab)
        },
        |_| [0.5, 0.45, 0.4],
    );
    field.to_dense(192).expect("the terrain has content")
}

/// Collider proxy slots as the globals hold them: ends or centres, sizes
/// and kinds, rotations.
type Proxies = ([[f32; 4]; 32], [[f32; 4]; 32], [[f32; 4]; 32]);

/// Thirty collider proxies along the shore: boxes, spheres and capsules,
/// half of them boats on the water and half rocks on the land, placed by a
/// fixed sequence so every run sees the same ones.
fn proxies() -> Proxies {
    let mut a = [[0.0f32; 4]; 32];
    let mut b = [[0.0f32; 4]; 32];
    let mut rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
    let mut seed = 0x2545_f491u32;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        (seed >> 8) as f32 / (1u32 << 24) as f32
    };
    for i in 0..PROXIES {
        let x = -40.0 + 100.0 * next();
        let z = -60.0 + 120.0 * next();
        let y = ground(x, z).max(0.0) + 1.0;
        let size = 1.0 + 2.5 * next();
        match i % 3 {
            0 => {
                let yaw = std::f32::consts::TAU * next();
                a[i] = [x, y, z, 0.0];
                b[i] = [size * 1.5, size * 0.6, size * 0.8, 2.0];
                rot[i] = [0.0, (yaw * 0.5).sin(), 0.0, (yaw * 0.5).cos()];
            }
            1 => {
                a[i] = [x, y, z, size];
                b[i] = [0.0, 0.0, 0.0, 0.0];
            }
            _ => {
                a[i] = [x - size, y, z, size * 0.4];
                b[i] = [x + size, y + 0.5 * size, z + 0.3 * size, 1.0];
            }
        }
    }
    (a, b, rot)
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

/// The views the game measured from: eye level, and eighteen metres up,
/// both looking toward the shore; and twelve up over the knolls
/// (`knolls`), which has no proxies.
#[derive(Clone, Copy, Debug)]
enum View {
    EyeLevel,
    Above,
    Knolls,
}

/// A fine terrain (quarter-unit cells) whose box ends 2.3 units above the
/// water: a bed 0.6 down and a grid of knolls standing out
/// of the water. The reads above a point by these knolls land just over the
/// box, where the stand-in's distance runs ahead of one unit per unit moved
/// by up to the height over the box, so a knoll can sit within reach of a
/// ray's near half while the read says it is not. Distances are exact.
fn knolls() -> BakedSdf {
    let (dims, center, half) = ([128u32, 21, 128], [0.0f32, -0.35, 0.0], [16.0f32, 2.65, 16.0]);
    let n = (dims[0] * dims[1] * dims[2]) as usize;
    let mut distance = Vec::with_capacity(n);
    for iz in 0..dims[2] {
        for iy in 0..dims[1] {
            for ix in 0..dims[0] {
                let at = |i: u32, a: usize| center[a] - half[a] + (i as f32 + 0.5) / dims[a] as f32 * 2.0 * half[a];
                let p = Vec3::new(at(ix, 0), at(iy, 1), at(iz, 2));
                let mut d = p.y + 0.6;
                for kz in -2i32..=2 {
                    for kx in -2i32..=2 {
                        let (r, y) = if (kx + kz) % 2 == 0 { (0.9, 1.0) } else { (0.6, 1.5) };
                        let c = Vec3::new(kx as f32 * 6.0 + 0.7 * kz as f32, y, kz as f32 * 6.0);
                        d = d.min((p - c).length() - r);
                    }
                }
                distance.push(d);
            }
        }
    }
    BakedSdf { dims, center, half_extent: half, distance, color: vec![[120, 110, 90, 0]; n] }
}

struct Beach {
    gpu: Gpu,
    raster: Raster,
    raymarch: Raymarch,
    out: wgpu::Texture,
    sheet: MeshId,
    bed: BakedSdf,
}

impl Beach {
    /// `None` on a device that cannot build the raster pass.
    fn new(bed: BakedSdf) -> Option<Beach> {
        let gpu = Gpu::headless(W, H);
        if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            return None;
        }
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("beach"),
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
        assert_eq!(raymarch.set_volumes(&gpu, &[&bed]), 1);
        Some(Beach { gpu, raster, raymarch, out, sheet, bed })
    }

    /// A water shader from FLSL whose cover is `open`, with `wgsl` (extra
    /// functions) appended and every `from` call in it renamed to `to`. A
    /// renamed shader writes `open` straight out, for reading back.
    fn shader_with(&mut self, open: &str, rename: Option<(&str, &str)>, wgsl: &str) -> FlslBindingId {
        let out = if rename.is_some() { "vec4(vec3(open), 1)" } else { "vec4(vec3(open) * 0.9 + vec3(0.05) * pow(1 - saturate(dot(n, viewDir)), 5), 1)" };
        let src = format!(
            "shader sea {{\n  stage fragment\n  \
             let n = normalize(normal + vec3(sin(worldPos.x * 3.1) * 0.04, 0, cos(worldPos.z * 2.3) * 0.04))\n  \
             let open = {open}\n  \
             output color = {out}\n}}\n"
        );
        let compiled = floptle_shader::compile_fragment(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        let mut chunk = compiled.chunk.clone();
        if let Some((from, to)) = rename {
            assert!(chunk.contains(from), "{chunk}");
            chunk = chunk.replace(from, to);
        }
        chunk.push_str(wgsl);
        floptle_shader::validate(pass_prelude(), &chunk).unwrap_or_else(|e| panic!("naga: {}\n{}", e.message, chunk));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, chunk);
        let id = self.raster.register_flsl_shader(&self.gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        self.raster.set_flsl_binding(&self.gpu, None, id, &params, &[])
    }

    fn shader(&mut self, open: &str) -> FlslBindingId {
        self.shader_with(open, None, "")
    }

    /// Bind the terrain and the proxies for a camera at `view`.
    fn stage(&mut self, view: View) -> (Globals, DVec3) {
        let (eye, pitch) = match view {
            View::EyeLevel => (DVec3::new(-10.0, 2.0, 0.0), -24f32),
            View::Above => (DVec3::new(-30.0, 18.0, 0.0), -42f32),
            View::Knolls => (DVec3::new(-4.0, 12.0, 0.0), -80f32),
        };
        let bound = if matches!(view, View::Knolls) { 0 } else { PROXIES };
        // Looking along +x, toward the shore, pitched down.
        let rot = Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2) * Quat::from_rotation_x(pitch.to_radians());
        let cam = RenderCamera::new(eye, rot, Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.1, far: 2000.0 });
        let rel = |p: [f32; 3]| (DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) - eye).as_vec3();
        let view_proj = cam.view_proj(W as f32 / H as f32);
        let mut vol_center = [[0.0f32; 4]; floptle_render::MAX_VOLUMES];
        let mut vol_half = [[1.0f32, 1.0, 1.0, 0.5]; floptle_render::MAX_VOLUMES];
        let c = rel(self.bed.center);
        // Kind 3: meshed terrain, which casts and occludes and is not marched.
        vol_center[0] = [c.x, c.y, c.z, 3.0];
        let h = self.bed.half_extent;
        vol_half[0] = [h[0], h[1], h[2], 0.6];
        let (mut prox_a, mut prox_b, prox_rot) = proxies();
        for i in 0..PROXIES {
            let a = rel([prox_a[i][0], prox_a[i][1], prox_a[i][2]]);
            prox_a[i] = [a.x, a.y, a.z, prox_a[i][3]];
            if prox_b[i][3] > 0.5 && prox_b[i][3] < 1.5 {
                let b = rel([prox_b[i][0], prox_b[i][1], prox_b[i][2]]);
                prox_b[i] = [b.x, b.y, b.z, 1.0];
            }
        }
        let rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [0.3, 0.3, 0.3, 0.0],
            params: [0.0, 0.0, 0.3, 0.0],
            vol_center,
            vol_half,
            ao_params: [0.0, 1.0, 3.0, 0.0],
            prox_count: [bound as f32, 0.0, 0.0, 0.0],
            prox_a,
            prox_b,
            prox_rot,
            ..Default::default()
        };
        self.raymarch.upload_globals(&self.gpu, rm);
        let globals = Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [0.3, 0.3, 0.3, 0.0],
            ..Default::default()
        };
        (globals, eye)
    }

    /// Draw the sea through `bind`: a sheet at y = 0 over everything seaward
    /// of the shore (the land is drawn by its meshes, which this leaves out).
    fn draw(&mut self, globals: Globals, eye: DVec3, bind: FlslBindingId) {
        let place = Transform {
            translation: DVec3::new(SHORE as f64 - 1000.0, 0.0, 0.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::new(2000.0, 1.0, 2000.0),
        };
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
            Some([0.0, 0.0, 0.0, 0.0]),
            Some(self.raymarch.field_bind()),
        );
    }

    /// Each pixel the sea covers, as the shader wrote its red channel
    /// (linear); `None` where the sea is not drawn.
    fn read(&self) -> Vec<Option<f32>> {
        let srgb = self.gpu.config.format.is_srgb();
        floptle_render::probe::readback(&self.gpu, &self.out)
            .iter()
            .map(|p| {
                let v = p[0] as f32 / 255.0;
                let v = if srgb { if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } } else { v };
                (p[3] > 128).then_some(v)
            })
            .collect()
    }
}

/// `skyOcclusion` with none of its early outs: every ray swept over every
/// piece and marched from its lift, as the op did before it had any.
const FULL_MARCH: &str = r#"
fn sky_full(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    const K: f32 = 8.0;
    let far = max(reach, 0.01);
    let base = shadow_lift_base(p);
    let side = select(vec3<f32>(1.0, 0.0, 0.0), vec3<f32>(0.0, 1.0, 0.0), abs(n.x) > 0.9);
    let t = normalize(cross(side, n));
    let b = cross(n, t);
    var vis = 0.0;
    for (var i = 0u; i < 4u; i = i + 1u) {
        let lift = sky_ray_lift(i, base);
        let ro = p + n * lift;
        let dir = sky_ray_dir(i, t, b, n);
        vis = vis + field_march(ro, dir, K, base * 3.0, lift, lift, 24, field_sweep(ro, dir, far, K, FIELD_ALL));
    }
    return vis / 4.0;
}
"#;

/// The op through `call` against the same call with no early outs at
/// all: (sea pixels, pixels under some cover, worst difference, pixels
/// differing by over 0.02).
fn compare(beach: &mut Beach, view: View, call: &str) -> (usize, usize, f32, usize) {
    let (globals, eye) = beach.stage(view);
    let fast = beach.shader_with(call, Some(("sky_occlusion(", "sky_occlusion(")), "");
    beach.draw(globals, eye, fast);
    let a = beach.read();
    let full = beach.shader_with(call, Some(("sky_occlusion(", "sky_full(")), FULL_MARCH);
    beach.draw(globals, eye, full);
    let b = beach.read();
    let mut worst = 0.0f32;
    let (mut over, mut sea, mut dark, mut lighter) = (0usize, 0usize, 0usize, 0usize);
    for (x, y) in a.iter().zip(&b) {
        if let (Some(x), Some(y)) = (x, y) {
            sea += 1;
            if x - y > 0.02 {
                lighter += 1;
            }
            let d = (x - y).abs();
            worst = worst.max(d);
            if d > 0.02 {
                over += 1;
            }
            if *y < 0.99 {
                dark += 1;
            }
        }
    }
    println!("{view:?} {call}: {sea} sea px, {dark} under some cover; worst difference {worst:.3}, {over} px over 0.02 ({lighter} lighter)");
    (sea, dark, worst, over)
}

/// The op with its early outs reads what the full march reads, on both
/// views at reaches of 8, 24 and 32 (skyVisibility's). Where a march is
/// started past the stretch the point read proved clear, its samples fall
/// in other places than the full march's, and its penumbra can read a
/// little differently: never more than 0.15, and on no more than one pixel
/// in ten thousand by more than 0.02.
#[test]
fn early_outs_read_what_the_full_march_reads() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    for view in [View::EyeLevel, View::Above] {
        for reach in ["8", "24", "32"] {
            let call = format!("skyOcclusion(worldPos, vec3(0, 1, 0), {reach})");
            let (sea, dark, worst, over) = compare(&mut beach, view, &call);
            // The fixture: some of the sea is under cover at every reach.
            assert!(dark > sea / 100, "{view:?} reach {reach}: only {dark} of {sea} pixels are under any cover");
            assert!(
                worst <= 0.15 && over <= sea / 10_000,
                "{view:?} reach {reach}: the early outs differ from the full march by up to {worst:.3}, and by over 0.02 on {over} of {sea} pixels"
            );
        }
    }
}

/// The same over fine knolls just under a terrain box's top (`knolls`), at
/// short reaches and with the normal tilted as well as up: where the reads
/// above the point land just over the box, they must not call open a half
/// a knoll reaches into.
#[test]
fn early_outs_read_what_the_full_march_reads_just_over_a_box() {
    let Some(mut beach) = Beach::new(knolls()) else { return };
    for n in ["vec3(0, 1, 0)", "normalize(vec3(0.35, 1, 0.2))"] {
        for reach in ["2", "3", "4", "8", "16"] {
            let call = format!("skyOcclusion(worldPos, {n}, {reach})");
            let (sea, dark, worst, over) = compare(&mut beach, View::Knolls, &call);
            assert!(dark > sea / 100, "reach {reach}, n {n}: only {dark} of {sea} pixels are under any cover");
            assert!(
                worst <= 0.15 && over <= sea / 10_000,
                "reach {reach}, n {n}: the early outs differ from the full march by up to {worst:.3}, and by over 0.02 on {over} of {sea} pixels"
            );
        }
    }
}

/// GPU milliseconds the sea's draw takes through each of `binds`: the least
/// of several rounds of a batch of frames, each bind in turn, on the GPU's
/// own clock.
fn cost(beach: &mut Beach, timer: &mut floptle_render::GpuTimer, globals: Globals, eye: DVec3, binds: &[FlslBindingId]) -> Vec<f32> {
    const FRAMES: usize = 20;
    // Bring the GPU's clocks up before anything counts.
    for _ in 0..10 {
        for bind in binds {
            beach.draw(globals, eye, *bind);
        }
    }
    beach.gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok();
    let mut best = vec![f32::MAX; binds.len()];
    for _ in 0..12 {
        if !timer.begin() {
            continue;
        }
        for (k, bind) in binds.iter().enumerate() {
            timer.mark(&beach.gpu, &format!("v{k}"));
            for _ in 0..FRAMES {
                beach.draw(globals, eye, *bind);
            }
            timer.mark_gap(&beach.gpu);
        }
        timer.end(&beach.gpu);
        for _ in 0..200 {
            beach.gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok();
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

/// The game's own roof term: six field reads up and around the point.
const SIX_READS: &str = "saturate((fieldDistance(worldPos + vec3(0, 2, 0)) + fieldDistance(worldPos + vec3(0, 6, 0)) \
    + fieldDistance(worldPos + vec3(4, 4, 0)) + fieldDistance(worldPos + vec3(-4, 4, 0)) \
    + fieldDistance(worldPos + vec3(0, 4, 4)) + fieldDistance(worldPos + vec3(0, 4, -4))) / 30)";

/// On both views, `skyOcclusion` at a reach of 24 costs no more than nine
/// times what six `fieldDistance` reads cost on the same frame (each over
/// the plain shader). A ratio, not a time, so a slower or busier GPU moves
/// both sides. Measured on an RTX 4060 shared with other work: 4 to 7
/// times; with the reads above the point never saying open, 12 to 20; before
/// any of this, 23 and 17.
#[test]
fn sky_occlusion_by_a_beach_costs_a_few_field_reads() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    // A software rasteriser's clock says nothing about a GPU's.
    if beach.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye) = beach.stage(view);
        let plain = beach.shader("1.0");
        let six = beach.shader(SIX_READS);
        let sky = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, sky, plain, six, sky]);
        assert!(t.iter().all(|t| *t < f32::MAX), "no timing came back");
        let base = t[0].min(t[3]);
        let (reads, op) = (t[1].min(t[4]) - base, t[2].min(t[5]) - base);
        println!("{view:?}: plain {base:.3} ms; over it, six reads {reads:.3} ms, skyOcclusion reach 24 {op:.3} ms");
        assert!(
            op <= 9.0 * reads.max(0.01),
            "{view:?}: skyOcclusion at a reach of 24 costs {op:.3} ms a {W}x{H} frame over the plain shader, {:.1} times six field reads ({reads:.3} ms)",
            op / reads.max(0.01)
        );
    }
}
