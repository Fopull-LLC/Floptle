//! **A screen shader knows what a pixel is seen through.**
//!
//! `sceneSurfaceDepth` in a `stage post` pass is the distance to the nearest
//! see-through surface over a pixel (a water sheet, glass, any blended
//! material) and a very large number over bare ground, so
//! `step(sceneSurfaceDepth(), sceneDepth())` is "seen through something": a
//! wet-shoreline film, a rain splash or a snow cover painted on the solid
//! scene stays off the bed of a river.
//!
//! The surfaces are drawn, depth only, into a target of their own
//! (`Raster::surface_depth_pass`) and handed to the chain
//! (`SsaoFrame::surface_depth`). A frame whose passes never read the op
//! draws nothing and holds nothing.

use floptle_core::{BlendMode, Material, Shading};
use floptle_render::particles::{ParticleBatch, ParticleBlend, ParticleGlobals, ParticleInstance, Particles};
use floptle_render::post::{PostSettings, PostStack};
use floptle_render::{
    instance_of, pass_prelude, plane, set_ext_index, FlslBlend, FlslDraw, Globals, Gpu, InstanceRaw, MeshId,
    PostPass, PostShaderId, PostShaders, Raster, SsaoFrame, SurfaceExtras, TexId,
};
use glam::{Mat4, Vec3};

const SIZE: u32 = 32;
/// How far away every see-through surface below is drawn.
const SHEET: f32 = 4.0;
/// How far away the ground behind them is.
const GROUND: f32 = 10.0;

/// What the pass under test says about the centre pixel.
#[derive(Debug, PartialEq, Clone, Copy)]
struct Seen {
    /// `step(sceneSurfaceDepth(), sceneDepth())`.
    through: bool,
    /// `sceneSurfaceDepth()` is within a quarter unit of [`SHEET`].
    at_sheet: bool,
    /// `sceneSurfaceDepth()` is the "nothing here" number.
    none: bool,
}

const THROUGH: Seen = Seen { through: true, at_sheet: true, none: false };
const BARE: Seen = Seen { through: false, at_sheet: false, none: true };

/// One channel per answer, each either 0 or 1, so the chain's tonemap cannot
/// blur which it was.
const READER: &str = "shader seen {\n  stage post\n  let s = sceneSurfaceDepth()\n  \
    output color = vec3(step(s, sceneDepth()), step(abs(s - 4.0), 0.25), step(100000.0, s)) + sceneColor().rgb * 0\n}\n";
/// A pass that reads nothing of it.
const PLAIN: &str = "shader plain {\n  stage post\n  output color = sceneColor().rgb * 0.5 + vec3(sceneDepth() * 0)\n}\n";

fn post_module(src: &str) -> (Vec<u8>, String) {
    let compiled = floptle_shader::compile_post(src).expect("the screen shader compiles");
    let prelude = format!("{}\n{}", floptle_shader::transpile::POST_PRELUDE, floptle_shader::transpile::POST_FIELD_SHIM);
    (compiled.pack_params(&|_| None), format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk))
}

fn texture(gpu: &Gpu, label: &str, format: wgpu::TextureFormat) -> (wgpu::Texture, wgpu::TextureView) {
    let tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d { width: SIZE, height: SIZE, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
    (tex, view)
}

fn proj() -> Mat4 {
    Mat4::perspective_rh(1.0, 1.0, 0.1, 100.0)
}

/// The depth buffer's value for a surface `dist` straight ahead.
fn depth_of(dist: f32) -> f32 {
    let clip = proj() * glam::Vec4::new(0.0, 0.0, -dist, 1.0);
    clip.z / clip.w
}

/// Clear a depth target to one value: a flat surface that far away.
fn fill_depth(gpu: &Gpu, view: &wgpu::TextureView, depth: f32) {
    let mut enc = gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fill-depth") });
    enc.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("fill-depth"),
        color_attachments: &[],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(depth), store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([enc.finish()]);
}

/// Run one screen pass over `depth` and `surface` and read the centre pixel.
fn run_pass(
    gpu: &Gpu,
    post: &PostStack,
    shaders: &mut PostShaders,
    (pass, params): (PostShaderId, &[u8]),
    depth: &wgpu::TextureView,
    surface: Option<&wgpu::TextureView>,
) -> Seen {
    let (out, view) = texture(gpu, "surface-depth-out", gpu.config.format);
    let p = proj();
    let identity = Mat4::IDENTITY.to_cols_array_2d();
    let frame = SsaoFrame {
        depth,
        proj: p.to_cols_array_2d(),
        inv_proj: p.inverse().to_cols_array_2d(),
        fog: [0.0; 4],
        inv_view: identity,
        eye: [0.0; 3],
        water_depth: 0.0,
        sky: None,
        surface_depth: surface,
    };
    shaders.set_pass_list(gpu, &[PostPass { shader: pass, params: params.to_vec(), textures: Vec::new() }]);
    post.run_with(gpu, &PostSettings::default(), Some(&frame), &view, Some(shaders));
    let px = floptle_render::probe::readback(gpu, &out)[(SIZE as usize / 2) * SIZE as usize + SIZE as usize / 2];
    Seen { through: px[0] > 128, at_sheet: px[1] > 128, none: px[2] > 128 }
}

/// The op against depth targets filled by hand, with no raster pass at all:
/// this half runs on every device, the OpenGL one included, where the depth
/// is read through a float binding.
#[test]
fn a_screen_shader_reads_the_surface_depth_it_is_handed_and_nothing_without_one() {
    let gpu = Gpu::headless(SIZE, SIZE);
    let mut shaders = PostShaders::new(&gpu);
    let (params, text) = post_module(READER);
    let reader = shaders.register(&gpu, &text, None);
    let (plain_params, plain_text) = post_module(PLAIN);
    let plain = shaders.register(&gpu, &plain_text, None);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let (_scene, scene) = texture(&gpu, "scene-depth", Gpu::DEPTH_FORMAT);
    let (_surface, surface) = texture(&gpu, "surface-depth", Gpu::DEPTH_FORMAT);
    fill_depth(&gpu, &scene, depth_of(GROUND));

    // A sheet in front of the ground: its own distance, and seen through.
    fill_depth(&gpu, &surface, depth_of(SHEET));
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, Some(&surface));
    assert_eq!(seen, THROUGH, "a sheet {SHEET} away over ground {GROUND} away");
    assert!(shaders.reads_surface_depth(), "a pass list that calls sceneSurfaceDepth says it reads it");

    // Handed nothing: seen through nothing, everywhere.
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, None);
    assert_eq!(seen, BARE, "no surface depth handed to the chain");

    // A target nothing was drawn into: the same.
    fill_depth(&gpu, &surface, 1.0);
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, Some(&surface));
    assert_eq!(seen, BARE, "an empty surface depth");

    // A sheet behind the ground is hidden by it: not something the ground
    // is seen through.
    fill_depth(&gpu, &surface, depth_of(GROUND + 2.0));
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, Some(&surface));
    assert_eq!(seen, BARE, "a sheet behind the ground");

    // Glass writes the scene's depth too: the two are equal, and it is
    // still seen through.
    fill_depth(&gpu, &scene, depth_of(SHEET));
    fill_depth(&gpu, &surface, depth_of(SHEET));
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, Some(&surface));
    assert_eq!(seen, THROUGH, "a surface that wrote the scene's depth as well");

    // Bare sky is seen through nothing either: "no surface" reads past the
    // sky's own depth, or the two would tie and the step would pass.
    fill_depth(&gpu, &scene, 1.0);
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, None);
    assert_eq!(seen, BARE, "bare sky");
    // A sheet with only sky behind it is.
    fill_depth(&gpu, &surface, depth_of(SHEET));
    let seen = run_pass(&gpu, &post, &mut shaders, (reader, &params), &scene, Some(&surface));
    assert_eq!(seen, THROUGH, "sky behind a sheet");

    // A pass list that never calls the op asks for nothing.
    run_pass(&gpu, &post, &mut shaders, (plain, &plain_params), &scene, None);
    assert!(!shaders.reads_surface_depth(), "a pass list without sceneSurfaceDepth asked for the surface depth");
}

struct Scene {
    gpu: Gpu,
    raster: Raster,
    particles: Particles,
    post: PostStack,
    shaders: PostShaders,
    reader: (PostShaderId, Vec<u8>),
    quad: MeshId,
}

type Instance = (MeshId, Option<TexId>, InstanceRaw);

impl Scene {
    fn globals(&self) -> Globals {
        Globals {
            view_proj: proj().to_cols_array_2d(),
            light_dir: [0.0, 0.0, -1.0, 0.0],
            light_color: [1.0, 1.0, 1.0, 0.0],
            ambient: [0.5, 0.5, 0.5, 0.0],
            ..Default::default()
        }
    }

    /// A quad filling the view, `dist` ahead, facing the camera.
    fn sheet(&mut self, dist: f32, alpha: f32, material: &Material) -> Instance {
        let ext = self.raster.push_surface_extras(SurfaceExtras::from_material(material));
        let mut raw = instance_of(Mat4::from_translation(Vec3::new(0.0, 0.0, -dist)) * Mat4::from_scale(Vec3::splat(20.0)), [1.0; 3]);
        raw.color[3] = alpha;
        set_ext_index(&mut raw, ext);
        (self.quad, None, raw)
    }

    fn ground(&mut self) -> Instance {
        self.sheet(GROUND, 1.0, &Material::default())
    }

    /// Draw the frame the way a host does (the solid and blended scene, the
    /// glass, the see-through depth, then the screen pass) and read the
    /// centre pixel.
    fn seen(&mut self, instances: &[Instance], flsl: &[FlslDraw], particle: bool) -> Seen {
        let globals = self.globals();
        let (_color, color) = texture(&self.gpu, "surface-depth-scene", self.gpu.scene_format());
        let gpu = &self.gpu;
        self.raster.draw_scene_with(gpu, &color, gpu.depth_view(), globals, instances, flsl, &[], Some([0.0, 0.0, 0.0, 1.0]), None);
        self.raster.draw_transmissive(gpu, &color, gpu.depth_view(), globals, instances, &[], None, &[], 0);
        if particle {
            let p = proj();
            self.particles.draw(
                gpu,
                &color,
                gpu.depth_view(),
                ParticleGlobals {
                    view_proj: p.to_cols_array_2d(),
                    cam_right: [1.0, 0.0, 0.0, 0.0],
                    cam_up: [0.0, 1.0, 0.0, 0.0],
                    proj_z: ParticleGlobals::proj_z(&p, false),
                    ..Default::default()
                },
                &[ParticleInstance {
                    pos_rot: [0.0, 0.0, -SHEET, 0.0],
                    size: [SHEET, SHEET, 0.0, 0.0],
                    color: [1.0, 1.0, 1.0, 0.5],
                    basis_right: [1.0, 0.0, 0.0, 1.0],
                    basis_up: [0.0, 1.0, 0.0, 1.0],
                    ..Default::default()
                }],
                &[ParticleBatch { texture: None, blend: ParticleBlend::Alpha, range: 0..1 }],
                &self.raster,
                None,
            );
        }
        self.raster.surface_depth_pass(gpu, globals, instances, flsl, &[], gpu.depth_texture().size());
        let surface = self.raster.surface_depth_view();
        run_pass(gpu, &self.post, &mut self.shaders, (self.reader.0, &self.reader.1), gpu.depth_view(), surface)
    }
}

/// The whole path: surfaces drawn by the raster pass, their depth drawn by
/// `surface_depth_pass`, read by a screen shader.
#[test]
fn the_scene_s_see_through_surfaces_are_what_a_screen_shader_reads() {
    let gpu = Gpu::headless(SIZE, SIZE);
    let particles = Particles::new(&gpu);
    // CI's OpenGL device cannot build the raster pass at all (no view
    // formats); the test above covers the screen shader's half there.
    if !gpu.adapter.get_downlevel_capabilities().flags.contains(wgpu::DownlevelFlags::VIEW_FORMATS) {
        eprintln!("skipped — this device cannot build the raster pass, so nothing can be drawn");
        return;
    }
    let mut raster = Raster::new(&gpu);
    let mut shaders = PostShaders::new(&gpu);
    let (params, text) = post_module(READER);
    let reader = (shaders.register(&gpu, &text, None), params);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let quad = raster.register(&gpu, &plane(1.0), None);
    let mut s = Scene { gpu, raster, particles, post, shaders, reader, quad };
    let ground = s.ground();

    // ---- nothing to see through: no pass, no target ---------------------
    assert_eq!(s.seen(&[ground], &[], false), BARE, "bare ground");
    assert_eq!(s.raster.surface_depth_stats(), (0, 0, 0), "a frame with no see-through surface allocated or drew");
    // A solid sheet in front of the ground is the scene, not a window on it.
    let wall = s.sheet(SHEET, 1.0, &Material::default());
    assert_eq!(s.seen(&[ground, wall], &[], false), BARE, "a solid wall in front of the ground");
    // A particle is not a surface.
    assert_eq!(s.seen(&[ground], &[], true), BARE, "a particle over bare ground");
    assert_eq!(s.raster.surface_depth_stats(), (0, 0, 0), "none of those is a see-through surface");

    // ---- a water sheet: a blended material ------------------------------
    let water_material = Material { blend: BlendMode::Blend, alpha: 0.5, ..Default::default() };
    let water = s.sheet(SHEET, 0.5, &water_material);
    assert_eq!(s.seen(&[ground, water], &[], false), THROUGH, "ground under a blended sheet");
    assert_eq!(s.raster.surface_depth_stats(), (1, 1, 1), "one target, one pass");
    // The same sheet, left out by its material.
    let decal = s.sheet(SHEET, 0.5, &Material { surface_depth: false, ..water_material.clone() });
    assert_eq!(s.seen(&[ground, decal], &[], false), BARE, "a blended sheet whose material says surfaceDepth = false");
    // A solid material faded by its node blends, and is seen through.
    let faded = s.sheet(SHEET, 0.5, &Material::default());
    assert_eq!(s.seen(&[ground, faded], &[], false), THROUGH, "ground behind a faded solid");
    // Behind the ground it is hidden, and the ground is seen through nothing.
    let sunk = s.sheet(GROUND + 2.0, 0.5, &water_material);
    assert_eq!(s.seen(&[ground, sunk], &[], false), BARE, "a sheet behind the ground");

    // ---- glass: the refraction pass, which writes the scene's depth too --
    let glass = s.sheet(SHEET, 1.0, &Material { shading: Shading::Physical, transmission: 0.9, ..Default::default() });
    assert_eq!(s.seen(&[ground, glass], &[], false), THROUGH, "ground behind glass");

    // ---- a blended shader that moves its vertices: drawn where they went --
    let src = "shader swell {\n  stage fragment\n  blend alpha\n  output offset = normal * 1.0\n  output color = vec4(0, 0.3, 0.6, 0.5)\n}\n";
    let compiled = floptle_shader::compile_fragment(src).expect("the surface shader compiles");
    floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}", e.message));
    let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
    let id = s.raster.register_flsl_shader(&s.gpu, &chunk, 0, FlslBlend::Alpha, None);
    let bind = s.raster.set_flsl_binding(&s.gpu, None, id, &compiled.pack_params(&|_| None, &|_| None), &[]);
    // A step behind the sheet's distance; the offset carries it one unit
    // toward the camera, onto it.
    let behind = s.sheet(SHEET + 1.0, 1.0, &Material { shader: Some("swell.flsl".into()), ..Default::default() });
    assert_eq!(s.seen(&[ground], &[(behind.0, None, bind, behind.2)], false), THROUGH, "ground under a moved, blended shader surface");

    // Every frame above was one size: still the one target.
    assert_eq!(s.raster.surface_depth_stats().0, 1, "one target for one size");
    s.raster.drop_surface_depth();
    assert_eq!(s.raster.surface_depth_stats().0, 0, "the targets are given back when nothing reads them");
    assert!(s.raster.surface_depth_view().is_none());
}

/// **`scenePosition` is the world point at every pixel, not only down the
/// middle of the view.** In a screen shader `sceneDepth` is measured along
/// the camera's axis, so `cameraPosition() + sceneRay() * sceneDepth()` falls
/// short of the surface everywhere but the centre line: a band painted by
/// height came out as an arc round the view. `scenePosition` is the point
/// itself, and `sceneDistance` the distance along the pixel's ray that makes
/// the sum right.
///
/// A wall ten units ahead of a turned camera standing away from the origin,
/// read near a corner of the frame and at its centre. Every point of the wall
/// is ten units along the camera's forward axis.
#[test]
fn scene_position_is_the_world_point_off_the_centre_of_the_view() {
    let gpu = Gpu::headless(SIZE, SIZE);
    let mut shaders = PostShaders::new(&gpu);
    let post = PostStack::new(&gpu, SIZE, SIZE);
    let (_scene, scene) = texture(&gpu, "scene-depth", Gpu::DEPTH_FORMAT);
    fill_depth(&gpu, &scene, depth_of(GROUND));
    let eye = Vec3::new(100.0, 50.0, 20.0);
    let turn = Mat4::from_rotation_y(0.7) * Mat4::from_rotation_x(-0.3);
    let forward = turn.transform_vector3(Vec3::NEG_Z);
    let p = proj();

    // Red: the point lies on the wall. Green: it lies on the pixel's ray.
    // Blue: `sceneDistance` is how far along that ray it is.
    let checks = |point: &str| {
        format!(
            "shader where {{\n  stage post\n  uniform forward: vec3\n  let p = {point}\n  let off = p - cameraPosition()\n  \
             output color = vec3(step(abs(dot(off, forward) - 10.0), 0.02), step(length(normalize(off) - sceneRay()), 0.002), \
             step(abs(sceneDistance() - length(off)), 0.02)) + sceneColor().rgb * 0\n}}\n"
        )
    };
    let mut read = |point: &str, (x, y): (u32, u32)| {
        let compiled = floptle_shader::compile_post(&checks(point)).expect("the screen shader compiles");
        let prelude = format!("{}\n{}", floptle_shader::transpile::POST_PRELUDE, floptle_shader::transpile::POST_FIELD_SHIM);
        let text = format!("{prelude}\n{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let params = compiled.pack_params(&|name| (name == "forward").then_some([forward.x, forward.y, forward.z, 0.0]));
        let pass = shaders.register(&gpu, &text, None);
        let (out, view) = texture(&gpu, "scene-position-out", gpu.config.format);
        let frame = SsaoFrame {
            depth: &scene,
            proj: p.to_cols_array_2d(),
            inv_proj: p.inverse().to_cols_array_2d(),
            fog: [0.0; 4],
            inv_view: turn.to_cols_array_2d(),
            eye: eye.to_array(),
            water_depth: 0.0,
            sky: None,
            surface_depth: None,
        };
        shaders.set_pass_list(&gpu, &[PostPass { shader: pass, params, textures: Vec::new() }]);
        post.run_with(&gpu, &PostSettings::default(), Some(&frame), &view, Some(&shaders));
        let px = floptle_render::probe::readback(&gpu, &out)[(y * SIZE + x) as usize];
        [px[0] > 128, px[1] > 128, px[2] > 128]
    };
    let (centre, corner) = ((SIZE / 2, SIZE / 2), (2, 2));
    assert_eq!(read("scenePosition()", centre), [true; 3], "scenePosition at the centre of the view");
    assert_eq!(read("scenePosition()", corner), [true; 3], "scenePosition near a corner of the view");
    assert_eq!(
        read("cameraPosition() + sceneRay() * sceneDistance()", corner),
        [true; 3],
        "the camera's position plus the ray times sceneDistance, near a corner"
    );
    // The sum with `sceneDepth` is the one that is only right down the
    // middle: on the ray, short of the wall.
    assert!(read("cameraPosition() + sceneRay() * sceneDepth()", centre)[0], "the centre line");
    assert_eq!(
        read("cameraPosition() + sceneRay() * sceneDepth()", corner),
        [false, true, false],
        "sceneDepth is depth along the camera's axis: times the ray it stops short of the wall off-centre"
    );
}
