//! Water under cover probe: a pool that runs from the open into a cave mouth,
//! beside lit rock, with a lamp inside the cave lighting both.
//!
//! Three panels. On the left, the water shader as most were written: it
//! reflects `skyRadiance` whatever stands over it, so under the overhang it
//! glows pale between dark rock. In the middle, the same shader with its sky
//! and ambient multiplied by `skyVisibility` and the lamp added through
//! `localLight`, in a scene with SDF AO and a probe volume: it darkens under
//! cover as the rock beside it does, and the lamp lights it as it lights the
//! rock. On the right, the same shader in a scene with neither (screen-space
//! AO, no probes, as a generated world): the lit rock has no occlusion term,
//! and `skyVisibility` falls back to its cone of sky rays.
//!
//! Run: cargo run --release -p floptle-render --example cave_water_probe -- <out.png>
//! With FLOPTLE_GPU_TIMING=1 it prints what each variant costs instead.

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_gi::{BakedGi, Probe, ProbeGrid, Sh1};
use floptle_render::{
    cube, instance_of_mat, pass_prelude, FlslBlend, GiVolume, Globals, Gpu, MaterialParams, MeshData, Projection,
    Raster, Raymarch, RaymarchGlobals, RenderCamera, TextureData, Vertex,
};


/// The water shader. `COVER` is replaced by the factor its sky and ambient
/// take, and `LAMP` by the placed lights it adds.
const WATER: &str = "shader pool {
  stage fragment
  let n = normalize(normal + vec3(sin(worldPos.x * 3.1) * 0.04, 0, cos(worldPos.z * 2.3 + worldPos.x) * 0.04))
  let open = COVER
  let fres = 0.02 + 0.98 * pow(1 - saturate(dot(n, viewDir)), 5)
  let deep = vec3(0.03, 0.12, 0.14)
  let sky = skyRadiance(reflect(-viewDir, n)) * open
  let amb = deep * skyRadiance(n, 1) * open
  output color = vec4(amb + sky * fres + vec3(0.35, 0.4, 0.4) * LAMP, 1)
}
";

/// The rock overhang: a slab of field blobs, drawn as a box over them.
const SLAB_MIN: [f32; 3] = [-14.0, 2.2, -5.0];
const SLAB_MAX: [f32; 3] = [-1.0, 4.0, 5.0];
const LAMP_AT: [f32; 3] = [-4.5, 0.8, 1.5];

/// A flat quad on the XZ plane facing +y, one unit across.
fn floor_quad() -> MeshData {
    MeshData {
        vertices: [[-0.5, -0.5], [0.5, -0.5], [-0.5, 0.5], [0.5, 0.5]]
            .iter()
            .map(|p| Vertex { pos: [p[0], 0.0, p[1]], normal: [0.0, 1.0, 0.0], uv: [p[0] + 0.5, p[1] + 0.5] })
            .collect(),
        indices: vec![0, 2, 1, 1, 2, 3],
        colors: None,
    }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "cave_water.png".into());
    // FLOPTLE_GPU_TIMING=1 measures instead of drawing the picture: a large
    // frame looking straight down on the water under the overhang, where
    // both ops do the most work (occlusion, probes and the lamp all reach).
    let timing = std::env::var("FLOPTLE_GPU_TIMING").is_ok_and(|v| v == "1");
    let (w, h) = if timing { (2560u32, 1440u32) } else { (640, 400) };
    let gpu = Gpu::headless(w, h);
    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("cave-water-color"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let mut raster = Raster::new(&gpu);
    let mut raymarch = Raymarch::new(&gpu);
    let unit = raster.register(&gpu, &cube(0.5), None);
    let quad = raster.register(&gpu, &floor_quad(), None);

    let projection = Projection::Perspective { fov_y: 58f32.to_radians(), near: 0.1, far: 200.0 };
    let cam = if timing {
        RenderCamera::new(DVec3::new(-3.5, 1.9, 1.0), Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2), projection)
    } else {
        RenderCamera::new(DVec3::new(5.0, 1.4, 6.5), Quat::from_rotation_y(0.95) * Quat::from_rotation_x(-0.14), projection)
    };
    let eye = cam.world_position;
    let rel = |p: [f32; 3]| (DVec3::new(p[0] as f64, p[1] as f64, p[2] as f64) - eye).as_vec3();
    let view_proj = cam.view_proj(w as f32 / h as f32);

    // The field the lit path occludes with: blobs filling the slab.
    let mut blobs = [[0.0f32; 4]; 16];
    let mut k = 0;
    for x in [-11.5f32, -8.0, -4.5, -2.2] {
        for z in [-3.3f32, 0.0, 3.3] {
            let c = rel([x, 3.6, z]);
            blobs[k] = [c.x, c.y, c.z, 1.6];
            k += 1;
        }
    }
    // The slab as a collider proxy too: what a sun-shadow ray (and so
    // `skyOcclusion`) sees of a mesh. The blobs are only roughly the slab's
    // shape, and the SDF AO reads them.
    let slab_c = [0.5 * (SLAB_MIN[0] + SLAB_MAX[0]), 0.5 * (SLAB_MIN[1] + SLAB_MAX[1]), 0.5 * (SLAB_MIN[2] + SLAB_MAX[2])];
    let slab_s = [SLAB_MAX[0] - SLAB_MIN[0], SLAB_MAX[1] - SLAB_MIN[1], SLAB_MAX[2] - SLAB_MIN[2]];
    let mut prox_a = [[0.0f32; 4]; 32];
    let mut prox_b = [[0.0f32; 4]; 32];
    let c = rel(slab_c);
    prox_a[0] = [c.x, c.y, c.z, 0.0];
    prox_b[0] = [slab_s[0] * 0.5, slab_s[1] * 0.5, slab_s[2] * 0.5, 2.0];
    let lamp = rel(LAMP_AT);
    let mut point_pos = [[0.0f32; 4]; 16];
    let mut point_color = [[0.0f32; 4]; 16];
    point_pos[0] = [lamp.x, lamp.y, lamp.z, 6.0];
    point_color[0] = [1.6, 1.05, 0.55, 0.0];
    let ambient = [0.32, 0.36, 0.42, 0.0];
    let mut rm = RaymarchGlobals {
        view_proj: view_proj.to_cols_array_2d(),
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        light_color: [0.0; 4],
        ambient,
        params: [0.0, 1.0, 0.3, 0.0],
        blobs,
        ao_params: [1.0, 1.0, 3.0, 0.0],
        sky_params: [1.0, 0.0, 0.0, 0.0],
        sky_tint: [1.0, 1.0, 1.0, 0.0],
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        prox_count: [1.0, 0.0, 0.0, 0.0],
        prox_a,
        prox_b,
        prox_rot: [[0.0, 0.0, 0.0, 1.0]; 32],
        ..Default::default()
    };
    // A probe volume baked dim inside the cave, as a bake there would read.
    let centre = [-7.5f32, 1.0, 0.0];
    let mut sh = Sh1::ZERO;
    for d in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
        sh.accumulate(d, [0.06, 0.06, 0.07], 4.0 * std::f32::consts::PI / 6.0);
    }
    let mut baked = BakedGi::empty(ProbeGrid::from_spacing(centre, [5.5, 1.5, 4.5], 1.5));
    baked.probes.fill(Probe { sh, nearest: 10.0, mean: 10.0 });
    baked.bounces = 1;
    let volume = GiVolume::upload(&gpu, &baked, centre, 1.0, 1.0, false, 0.5);
    // The same scene as most generated worlds light it: screen-space AO (the
    // SDF AO off) and no probe volume, so the engine has no occlusion term of
    // its own and `skyVisibility` falls back to its cone of sky rays.
    let mut bare = RaymarchGlobals { ao_params: [0.0, 1.0, 3.0, 0.0], ..rm };
    let no_volume = GiVolume::empty(&gpu);
    no_volume.apply(&mut bare, [eye.x, eye.y, eye.z]);
    volume.apply(&mut rm, [eye.x, eye.y, eye.z]);
    // A daylight sky to reflect: blue overhead, pale at the horizon.
    let (sw, sh) = (64u32, 32u32);
    let mut sky = Vec::with_capacity((sw * sh * 4) as usize);
    for y in 0..sh {
        let up = 1.0 - (y as f32 + 0.5) / (sh as f32 * 0.5);
        let c = if up > 0.0 {
            let t = up.powf(0.6);
            [200.0 + (95.0 - 200.0) * t, 215.0 + (150.0 - 215.0) * t, 232.0 + (220.0 - 232.0) * t]
        } else {
            [80.0, 78.0, 74.0]
        };
        for _ in 0..sw {
            sky.extend_from_slice(&[c[0] as u8, c[1] as u8, c[2] as u8, 255]);
        }
    }
    raymarch.set_sky_texture(&gpu, Some(&TextureData { pixels: sky, width: sw, height: sh }));
    raymarch.set_gi(&gpu, volume);
    raymarch.upload_globals(&gpu, rm);
    raymarch.capture_env(&gpu);

    let globals = Globals {
        view_proj: view_proj.to_cols_array_2d(),
        light_color: [0.0; 4],
        ambient,
        point_count: [1.0, 0.0, 0.0, 0.0],
        point_pos,
        point_color,
        ..Default::default()
    };
    let place = |pos: [f32; 3], scale: [f32; 3]| {
        Transform {
            translation: DVec3::new(pos[0] as f64, pos[1] as f64, pos[2] as f64),
            rotation: Quat::IDENTITY,
            scale: Vec3::new(scale[0], scale[1], scale[2]),
        }
        .render_matrix(eye)
    };
    let rock = MaterialParams::flat([0.42, 0.4, 0.37]);
    let lamp_glow = MaterialParams { emissive: [1.0, 0.75, 0.45], emissive_strength: 2.0, unlit: true, ..MaterialParams::flat([1.0, 0.8, 0.5]) };
    let solid = [
        // The ground, with the pool's bed a strip across it.
        (unit, None, instance_of_mat(place([0.0, -0.5, 0.0], [40.0, 1.0, 30.0]), &rock)),
        (unit, None, instance_of_mat(place(slab_c, slab_s), &rock)),
        (unit, None, instance_of_mat(place(LAMP_AT, [0.15, 0.15, 0.15]), &lamp_glow)),
    ];

    let water = MaterialParams::flat([1.0; 3]);
    let sheet = instance_of_mat(place([-2.0, 0.04, 1.0], [14.0, 1.0, 4.0]), &water);
    let mut shader = |cover: &str, lamp: &str| {
        let src = WATER.replace("COVER", cover).replace("LAMP", lamp);
        let compiled = floptle_shader::compile_fragment(&src).unwrap_or_else(|e| panic!("{e}\n{src}"));
        floptle_shader::validate(pass_prelude(), &compiled.chunk).unwrap_or_else(|e| panic!("naga: {}\n{}", e.message, compiled.chunk));
        let chunk = format!("{}\n{}", floptle_shader::stdlib::SUPPORT_WGSL, compiled.chunk);
        let id = raster.register_flsl_shader(&gpu, &chunk, 0, FlslBlend::Opaque, None);
        let params = compiled.pack_params(&|_| None, &|_| None);
        raster.set_flsl_binding(&gpu, None, id, &params, &[])
    };
    let plain = shader("1.0", "vec3(0)");
    let sky_only = shader("skyVisibility(worldPos, n)", "vec3(0)");
    let covered = shader("skyVisibility(worldPos, n)", "localLight(worldPos, n)");
    let cone = shader("skyOcclusion(worldPos, n)", "vec3(0)");
    let draw = |raster: &mut Raster, raymarch: &Raymarch, bind| {
        raster.draw_scene_with(
            &gpu,
            &color_view,
            gpu.depth_view(),
            globals,
            &solid,
            &[(quad, None, bind, sheet)],
            &[],
            Some([0.0, 0.0, 0.0, 1.0]),
            Some(raymarch.field_bind()),
        );
    };
    // What each variant costs, on the GPU's own clock: the same frame drawn a
    // number of times between two marks, twice over after a warm-up round.
    let time = |raster: &mut Raster, raymarch: &Raymarch, setup: &str, variants: &[(&str, floptle_render::FlslBindingId)]| {
        let Some(mut timer) = floptle_render::GpuTimer::new(&gpu) else {
            println!("this device has no timestamp queries");
            return;
        };
        const FRAMES: usize = 40;
        for round in 0..3 {
            timer.begin();
            for (label, bind) in variants {
                timer.mark(&gpu, label);
                for _ in 0..FRAMES {
                    draw(raster, raymarch, *bind);
                }
                timer.mark_gap(&gpu);
            }
            timer.end(&gpu);
            let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
            timer.poll();
            if round > 0 {
                for s in timer.spans() {
                    println!("{setup:>22} {:>28}: {:.3} ms a frame", s.label, s.ms / FRAMES as f32);
                }
            }
        }
    };

    let (before, after) = if timing {
        time(
            &mut raster,
            &raymarch,
            "SDF AO + probes",
            &[("plain", plain), ("skyVisibility", sky_only), ("skyVisibility + localLight", covered)],
        );
        (Vec::new(), Vec::new())
    } else {
        draw(&mut raster, &raymarch, plain);
        let before = floptle_render::probe::readback(&gpu, &color_tex);
        draw(&mut raster, &raymarch, covered);
        (before, floptle_render::probe::readback(&gpu, &color_tex))
    };

    // Screen-space AO and no probes: the fallback.
    raymarch.set_gi(&gpu, no_volume);
    raymarch.upload_globals(&gpu, bare);
    raymarch.capture_env(&gpu);
    if timing {
        time(
            &mut raster,
            &raymarch,
            "no AO term, no probes",
            &[("plain", plain), ("skyVisibility (fallback)", sky_only), ("skyOcclusion", cone)],
        );
        return;
    }
    draw(&mut raster, &raymarch, covered);
    let fallback = floptle_render::probe::readback(&gpu, &color_tex);

    // Side by side: the sky alone; under cover with the scene's SDF AO and
    // probes; under cover with neither, through the fallback.
    let mut px = Vec::with_capacity((w * 3 * h * 4) as usize);
    for y in 0..h as usize {
        for panel in [&before, &after, &fallback] {
            for p in &panel[y * w as usize..(y + 1) * w as usize] {
                px.extend_from_slice(p);
            }
        }
    }
    floptle_render::probe::write_png(&px, w * 3, h, &out);
    println!("wrote {out}");
}
