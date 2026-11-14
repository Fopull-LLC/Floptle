//! **A surface that lights itself darkens under cover as the lit ground does.**
//!
//! `skyVisibility(worldPos [, n])` is the share of the sky's ambient the
//! engine's own lit path lets reach a point, and `localLight(worldPos, n)` is
//! what the placed lights give it. This draws one floor twice, once as a
//! plain lit material and once through a shader that outputs the op, and
//! compares the two pictures pixel by pixel: under a blob that covers part of
//! the floor (the scene's SDF ambient occlusion), inside a light probe volume
//! baked dimmer than the open sky, and beside a lamp whose light the blob
//! shadows.
//!
//! A scene with occlusion switched off draws its lit floor at the full
//! ambient, covered or not: the lit path is unchanged. There the engine has no
//! occlusion term of its own, and `skyVisibility` falls back to
//! `skyOcclusion`, a cone of sky rays marched as a sun shadow is: the last two
//! tests stand a floor at the bottom of a shaft cut in collider proxies.

use floptle_gi::{BakedGi, Probe, ProbeGrid, Sh1};
use floptle_render::{
    instance_of_mat, pass_prelude, FlslBlend, GiVolume, Globals, Gpu, MaterialParams, MeshData, MeshId, Raster, Raymarch,
    RaymarchGlobals, SurfaceExtras, Vertex,
};
use glam::{Mat4, Vec3};

const SIZE: u32 = 64;
/// Half the floor's width, world units.
const HALF: f32 = 4.0;
/// The scene's flat ambient: what the open sky gives a matte white surface.
const AMBIENT: f32 = 0.5;

/// What a pixel's byte holds, as the shader wrote it.
fn decode(gpu: &Gpu, byte: u8) -> f32 {
    let v = byte as f32 / 255.0;
    if gpu.config.format.is_srgb() {
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    } else {
        v
    }
}

/// What the scene holds besides the floor.
#[derive(Clone, Copy, Default)]
struct Scene {
    /// SDF ambient occlusion on, with a blob standing on the left of the floor.
    occlusion: bool,
    /// A light probe volume over the floor, baked to this even radiance.
    probes: Option<f32>,
    /// A lamp over the right of the floor, the blob between it and the left.
    lamp: bool,
    /// The ambient colour's level (the lamp is measured with none).
    ambient: f32,
    /// Leave the blob out: nothing stands over the floor.
    no_blob: bool,
    /// Instead of the blob, a 7-unit shaft 3 units across with the floor at
    /// its bottom, cut in a slab of collider proxies (what the sun-shadow
    /// march sees of a mesh, as it sees a terrain).
    shaft: bool,
}

struct Rig {
    gpu: Gpu,
    raster: Raster,
    raymarch: Raymarch,
    out: wgpu::Texture,
    floor: MeshId,
}

impl Rig {
    /// `None` on a device that cannot build the raster pass.
    fn new() -> Option<Rig> {
        let gpu = Gpu::headless(SIZE, SIZE);
        if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
            // No raster pass on this device, so there is nothing to draw with.
            return None;
        }
        let out = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("sky-visibility-out"),
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
        // A floor that fills the frame, facing +z: its own x and y are the
        // screen's, in world units, and the camera sits at the origin.
        let quad = MeshData {
            vertices: [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]]
                .iter()
                .map(|p| Vertex { pos: [p[0] * HALF, p[1] * HALF, 0.5], normal: [0.0, 0.0, 1.0], uv: [0.5, 0.5] })
                .collect(),
            indices: vec![0, 1, 2, 1, 3, 2],
            colors: None,
        };
        let floor = raster.register(&gpu, &quad, None);
        Some(Rig { gpu, raster, raymarch, out, floor })
    }

    /// Both passes' globals for a scene, with its field and probes bound.
    fn stage(&mut self, scene: Scene) -> Globals {
        let view_proj = Mat4::from_scale(Vec3::new(1.0 / HALF, 1.0 / HALF, 1.0));
        let mut point_pos = [[0.0f32; 4]; 16];
        let mut point_color = [[0.0f32; 4]; 16];
        let mut point_shape = [[0.0f32; 4]; 16];
        let point_rot = [[0.0f32, 0.0, 0.0, 1.0]; 16];
        if scene.lamp {
            point_pos[0] = [2.0, 0.5, 2.0, 9.0];
            point_color[0] = [0.9, 0.7, 0.5, 0.0];
            // A bare point that casts: bit 1 of the flags.
            point_shape[0] = [0.0, 0.0, 0.0, 2.0];
        }
        let count = if scene.lamp { 1.0 } else { 0.0 };
        let mut blobs = [[0.0f32; 4]; 16];
        // The cover: a blob standing on the left of the floor, on the side
        // the floor faces.
        if !scene.no_blob && !scene.shaft {
            blobs[0] = [-1.6, 0.3, 1.5, 2.0];
        }
        // The slab's four walls, up from the floor (z = 0.5) to z = 7.5, out
        // well past the frame.
        let mut prox_a = [[0.0f32; 4]; 32];
        let mut prox_b = [[0.0f32; 4]; 32];
        let prox_rot = [[0.0f32, 0.0, 0.0, 1.0]; 32];
        if scene.shaft {
            let walls = [
                ([6.75, 0.0], [5.25, 12.0]),
                ([-6.75, 0.0], [5.25, 12.0]),
                ([0.0, 6.75], [1.5, 5.25]),
                ([0.0, -6.75], [1.5, 5.25]),
            ];
            for (i, (c, h)) in walls.iter().enumerate() {
                prox_a[i] = [c[0], c[1], 4.0, 0.0];
                prox_b[i] = [h[0], h[1], 3.5, 2.0];
            }
        }
        let mut rm = RaymarchGlobals {
            view_proj: view_proj.to_cols_array_2d(),
            inv_view_proj: view_proj.inverse().to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [scene.ambient, scene.ambient, scene.ambient, 0.0],
            params: [0.0, 1.0, 0.3, 0.0],
            blobs,
            ao_params: [if scene.occlusion { 1.0 } else { 0.0 }, 1.0, 1.5, 0.0],
            point_count: [count, 0.0, 0.0, 0.0],
            point_pos,
            point_color,
            point_shape,
            point_rot,
            prox_count: [if scene.shaft { 4.0 } else { 0.0 }, 0.0, 0.0, 0.0],
            prox_a,
            prox_b,
            prox_rot,
            ..Default::default()
        };
        let volume = match scene.probes {
            Some(level) => {
                // Radiance arriving evenly from every side: six opposing
                // deltas, so only the constant band is left.
                let mut sh = Sh1::ZERO;
                for d in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
                    sh.accumulate(d, [level; 3], 4.0 * std::f32::consts::PI / 6.0);
                }
                // A box narrower than the floor, so its faded edge and the
                // open floor past it are both in the picture.
                let mut baked = BakedGi::empty(ProbeGrid::from_spacing([0.0, 0.0, 0.5], [3.0, 5.0, 3.0], 1.5));
                baked.probes.fill(Probe { sh, nearest: 10.0, mean: 10.0 });
                baked.bounces = 1;
                GiVolume::upload(&self.gpu, &baked, [0.0, 0.0, 0.5], 1.0, 1.0, false, 0.5)
            }
            None => GiVolume::empty(&self.gpu),
        };
        volume.apply(&mut rm, [0.0; 3]);
        self.raymarch.set_gi(&self.gpu, volume);
        self.raymarch.upload_globals(&self.gpu, rm);
        Globals {
            view_proj: view_proj.to_cols_array_2d(),
            light_color: [0.0; 4],
            ambient: [scene.ambient, scene.ambient, scene.ambient, 0.0],
            point_count: [count, 0.0, 0.0, 0.0],
            point_pos,
            point_color,
            point_shape,
            point_rot,
            ..Default::default()
        }
    }

    fn read(&self) -> Vec<[f32; 3]> {
        floptle_render::probe::readback(&self.gpu, &self.out)
            .iter()
            .map(|p| [decode(&self.gpu, p[0]), decode(&self.gpu, p[1]), decode(&self.gpu, p[2])])
            .collect()
    }

    /// The floor as a plain white lit material draws it.
    fn lit(&mut self, scene: Scene) -> Vec<[f32; 3]> {
        let globals = self.stage(scene);
        let mut m = MaterialParams::flat([1.0; 3]);
        m.ambient = 1.0;
        m.ext_index = self.raster.push_surface_extras(SurfaceExtras::default());
        let view = self.out.create_view(&wgpu::TextureViewDescriptor::default());
        self.raster.draw_scene(
            &self.gpu,
            &view,
            self.gpu.depth_view(),
            globals,
            &[(self.floor, None, instance_of_mat(Mat4::IDENTITY, &m))],
            Some([0.0, 0.0, 0.0, 1.0]),
            Some(self.raymarch.field_bind()),
        );
        self.read()
    }

    /// The floor as a shader whose colour is `expr` draws it.
    fn shaded(&mut self, scene: Scene, expr: &str) -> Vec<[f32; 3]> {
        let src = format!("shader s {{\n  stage fragment\n  output color = vec4({expr}, 1)\n}}\n");
        let compiled = floptle_shader::compile_fragment(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}\n{}", e.message, compiled.chunk));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = self.raster.register_flsl_shader(&self.gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        let bind = self.raster.set_flsl_binding(&self.gpu, None, id, &params, &[]);
        let globals = self.stage(scene);
        let m = MaterialParams::flat([1.0; 3]);
        let view = self.out.create_view(&wgpu::TextureViewDescriptor::default());
        self.raster.draw_scene_with(
            &self.gpu,
            &view,
            self.gpu.depth_view(),
            globals,
            &[],
            &[(self.floor, None, bind, instance_of_mat(Mat4::IDENTITY, &m))],
            &[],
            Some([0.0, 0.0, 0.0, 1.0]),
            Some(self.raymarch.field_bind()),
        );
        self.read()
    }
}

/// The pixel at a world x on the floor's middle row.
fn at(px: &[[f32; 3]], x: f32) -> [f32; 3] {
    let i = (((x / HALF) * 0.5 + 0.5) * SIZE as f32) as u32;
    px[(SIZE * (SIZE / 2) + i.min(SIZE - 1)) as usize]
}

/// The largest difference between two pictures, and where.
fn worst(a: &[[f32; 3]], b: &[[f32; 3]]) -> (f32, usize) {
    let mut worst = (0.0f32, 0usize);
    for (k, (p, q)) in a.iter().zip(b).enumerate() {
        for c in 0..3 {
            if (p[c] - q[c]).abs() > worst.0 {
                worst = ((p[c] - q[c]).abs(), k);
            }
        }
    }
    worst
}

fn assert_same(what: &str, lit: &[[f32; 3]], shaded: &[[f32; 3]]) {
    let (d, k) = worst(lit, shaded);
    assert!(
        d < 0.012,
        "{what}: at pixel {}, {} the lit floor is {:?} and the shader's is {:?}",
        k as u32 % SIZE,
        k as u32 / SIZE,
        lit[k],
        shaded[k]
    );
}

#[test]
fn sky_visibility_is_the_lit_paths_share_of_the_ambient() {
    let Some(mut rig) = Rig::new() else { return };
    let share = format!("vec3(skyVisibility(worldPos) * {AMBIENT})");

    // Under the scene's ambient occlusion.
    let scene = Scene { occlusion: true, ambient: AMBIENT, ..Default::default() };
    let lit = rig.lit(scene);
    // The fixture: dark under the blob, the full ambient in the open, and a
    // falloff between them.
    let (covered, edge, open) = (at(&lit, -1.6)[0], at(&lit, 0.2)[0], at(&lit, 3.5)[0]);
    assert!(covered < 0.02, "the floor under the blob is lit {covered:.3}: the fixture has no cover");
    assert!((open - AMBIENT).abs() < 0.012, "the open floor is lit {open:.3}, the ambient is {AMBIENT}");
    assert!(edge > 0.05 && edge < AMBIENT - 0.05, "beside the blob the floor is lit {edge:.3}: no falloff to compare");
    let shaded = rig.shaded(scene, &share);
    assert_same("under ambient occlusion", &lit, &shaded);
    assert!(at(&shaded, -1.6)[0] < 0.02 && (at(&shaded, 3.5)[0] - AMBIENT).abs() < 0.012, "0 under cover and 1 in the open");
    // A normal given is the normal used: the surface's own draws the same
    // picture, and one facing away from the blob sees the whole sky.
    assert_same("with the surface's normal given", &lit, &rig.shaded(scene, &format!("vec3(skyVisibility(worldPos, normal * 3) * {AMBIENT})")));
    let away = rig.shaded(scene, &format!("vec3(skyVisibility(worldPos + vec3(0, 0, -0.5), -normal) * {AMBIENT})"));
    assert!((at(&away, 0.2)[0] - AMBIENT).abs() < 0.012, "facing away from the cover, half a unit clear of it: {:?}", at(&away, 0.2));

    // Inside a light probe volume baked dimmer than the open sky, alone and
    // with the occlusion over it.
    // With the occlusion off nothing else stands over the floor, so where the
    // volume fades out the share has no cover to fall back on (see the shaft
    // test for that).
    for occlusion in [false, true] {
        let scene = Scene { occlusion, probes: Some(0.2), ambient: AMBIENT, no_blob: !occlusion, ..Default::default() };
        let lit = rig.lit(scene);
        let (inside, outside) = (at(&lit, 1.5)[0], at(&lit, 3.8)[0]);
        assert!((inside - 0.2).abs() < 0.03, "the floor inside the volume is lit {inside:.3}, the bake holds 0.2");
        assert!((outside - AMBIENT).abs() < 0.012, "the floor outside the volume is lit {outside:.3}");
        assert_same(if occlusion { "in a probe volume, occluded" } else { "in a probe volume" }, &lit, &rig.shaded(scene, &share));
    }

    // A volume baked brighter than the open sky lights its floor more, and
    // the share stops at all of the sky.
    let scene = Scene { probes: Some(0.8), ambient: AMBIENT, no_blob: true, ..Default::default() };
    let lit = rig.lit(scene);
    assert!(at(&lit, 1.5)[0] > 0.7, "the bright bake reaches the floor: {:?}", at(&lit, 1.5));
    let shaded = rig.shaded(scene, &share);
    assert!((at(&shaded, 1.5)[0] - AMBIENT).abs() < 0.012, "a share over 1: {:?}", at(&shaded, 1.5));
}

#[test]
fn local_light_is_the_lit_paths_lamp() {
    let Some(mut rig) = Rig::new() else { return };
    // No sun and no ambient: every lit pixel of the floor came from the lamp.
    let scene = Scene { lamp: true, ..Default::default() };
    let lit = rig.lit(scene);
    let (near, shadowed) = (at(&lit, 2.0), at(&lit, -3.4));
    assert!(near[0] > 0.4 && near[0] > near[2] * 1.5, "the floor under the lamp is lit {near:?}, in the lamp's colour");
    assert!(shadowed[0] < 0.02, "the blob shadows the floor behind it: {shadowed:?}");
    assert_same("a lamp", &lit, &rig.shaded(scene, "localLight(worldPos, normal)"));
    // The lamp is not in the sky's share, nor the sky in the lamp's.
    let scene = Scene { lamp: true, ambient: AMBIENT, ..Default::default() };
    assert_same("a lamp, with an ambient", &lit, &rig.shaded(scene, "localLight(worldPos, normal)"));
    let sky = rig.shaded(scene, "vec3(skyVisibility(worldPos))");
    let unlit = rig.shaded(Scene { lamp: false, ..scene }, "vec3(skyVisibility(worldPos))");
    assert_same("the sky's share, with and without a lamp", &unlit, &sky);
    // The scene's ambient occlusion darkens a lamp on the lit floor too, and
    // so on the shader's.
    let scene = Scene { lamp: true, occlusion: true, ..Default::default() };
    let occluded = rig.lit(scene);
    let (d, k) = worst(&lit, &occluded);
    assert!(d > 0.05, "the occlusion leaves the lamp on the floor as it was: the fixture has no cover in the lamp's light (pixel {k})");
    assert_same("a lamp, under occlusion", &occluded, &rig.shaded(scene, "localLight(worldPos, normal)"));
}

#[test]
fn a_scene_with_no_occlusion_draws_its_floor_at_the_full_ambient() {
    let Some(mut rig) = Rig::new() else { return };
    // The blob is still there; the scene's occlusion is switched off.
    let scene = Scene { ambient: AMBIENT, ..Default::default() };
    let lit = rig.lit(scene);
    for p in lit.iter().step_by(7) {
        assert!((p[0] - AMBIENT).abs() < 0.012, "a lit floor with no occlusion and no probes reads {p:?}");
    }
    // The share is not the lit floor's here: with no term of the engine's
    // own it falls back to the cone of sky rays, which finds the blob.
    let shaded = rig.shaded(scene, "vec3(skyVisibility(worldPos))");
    assert!(at(&shaded, -1.6)[0] < 0.3 && at(&shaded, 3.8)[0] > 0.97, "under the blob {:?}, in the open {:?}", at(&shaded, -1.6), at(&shaded, 3.8));
    // A shader that calls none of the ops holds none of their functions.
    let plain = floptle_shader::compile_fragment("shader s {\n  stage fragment\n  output color = vec4(litSurface(vec3(1)), 1)\n}\n").unwrap();
    assert!(
        !plain.chunk.contains("sky_visibility(") && !plain.chunk.contains("local_light(") && !plain.chunk.contains("sky_occlusion("),
        "{}",
        plain.chunk
    );
}

/// The share of a frame's pixels inside the shaft's mouth (|x|, |y| < 1.2)
/// and of those well clear of it (|x| or |y| > 2), as (lowest inside,
/// highest inside, lowest outside).
fn shaft_spread(px: &[[f32; 3]]) -> (f32, f32, f32) {
    let (mut lo_in, mut hi_in, mut lo_out) = (f32::MAX, 0.0f32, f32::MAX);
    for (k, p) in px.iter().enumerate() {
        let w = |i: u32| ((i as f32 + 0.5) / SIZE as f32 * 2.0 - 1.0) * HALF;
        let (x, y) = (w(k as u32 % SIZE), w(k as u32 / SIZE));
        if x.abs() < 1.2 && y.abs() < 1.2 {
            lo_in = lo_in.min(p[0]);
            hi_in = hi_in.max(p[0]);
        } else if x.abs() > 2.0 || y.abs() > 2.0 {
            lo_out = lo_out.min(p[0]);
        }
    }
    (lo_in, hi_in, lo_out)
}

/// **A pool at the bottom of a shaft reads covered with no AO term at all.**
/// Screen-space AO (the SDF AO off) and no probe volume: the lit path darkens
/// nothing, so `skyVisibility` falls back to a cone of sky rays marched as a
/// sun shadow is, against the shaft's walls. The floor at the bottom reads
/// well under 1; the ground on top of the slab beside the shaft reads 1.
#[test]
fn a_pool_at_the_bottom_of_a_shaft_reads_covered_without_an_ao_term() {
    let Some(mut rig) = Rig::new() else { return };
    let scene = Scene { shaft: true, ambient: AMBIENT, ..Default::default() };
    let bottom = rig.shaded(scene, "vec3(skyVisibility(worldPos))");
    let (lo, hi, _) = shaft_spread(&bottom);
    assert!(hi < 0.35, "the floor at the bottom of the shaft reads between {lo:.3} and {hi:.3}: open sky");
    // The same floor lifted to the slab's top: the ground beside the shaft,
    // and the air in its mouth.
    let top = rig.shaded(scene, "vec3(skyVisibility(worldPos + vec3(0, 0, 7.02)))");
    let (_, _, lo_out) = shaft_spread(&top);
    assert!(lo_out > 0.97, "the open ground beside the shaft reads as low as {lo_out:.3}");
}

/// `skyOcclusion(p, n [, reach])` is that cone itself: dark at the bottom of
/// the shaft, open beside it, and open again when its reach stops short of
/// the walls.
#[test]
fn sky_occlusion_marches_the_sky_against_what_shadows_the_sun() {
    let Some(mut rig) = Rig::new() else { return };
    let scene = Scene { shaft: true, ambient: AMBIENT, ..Default::default() };
    let (_, hi, _) = shaft_spread(&rig.shaded(scene, "vec3(skyOcclusion(worldPos, normal))"));
    assert!(hi < 0.35, "the bottom of the shaft reads up to {hi:.3}");
    let (_, _, lo_out) = shaft_spread(&rig.shaded(scene, "vec3(skyOcclusion(worldPos + vec3(0, 0, 7.02), normal))"));
    assert!(lo_out > 0.97, "the ground beside the shaft reads as low as {lo_out:.3}");
    // At the shaft's middle, a unit and a half from every wall.
    let short = at(&rig.shaded(scene, "vec3(skyOcclusion(worldPos, normal, 0.5))"), 0.0)[0];
    assert!(short > 0.97, "half a unit of reach, a unit and a half from the walls, reads {short:.3}");
    // Where the engine has its own term (the SDF AO on, over a field it can
    // resolve) the share is the built-in one and the cone is not asked: the
    // lit floor and the share agree, walls or no walls.
    let scene = Scene { occlusion: true, ambient: AMBIENT, ..Default::default() };
    assert_same("the SDF AO's own term", &rig.lit(scene), &rig.shaded(scene, &format!("vec3(skyVisibility(worldPos) * {AMBIENT})")));
}
