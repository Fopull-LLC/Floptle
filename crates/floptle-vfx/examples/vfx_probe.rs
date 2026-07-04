//! Headless particle probe — a two-track effect (alpha smoke + additive spark
//! fountain) over a small raster scene, rendered at three timeline positions plus
//! one retro-res frame, each a deterministic `simulate_to` scrub from `t = 0`.
//!
//! What the images should show: smoke puffs that grow/fade and sort correctly,
//! sparks that spray up and fall under gravity (waning over the effect via a Rate
//! automation lane), particles occluded by the box (shared depth buffer), and in
//! the retro frame particles pixelating on the same grid as the scene.
//!
//! Also proves determinism end-to-end: the t=0.9 frame is simulated + rendered
//! twice from scratch and byte-compared.
//!
//! Run: cargo run -p floptle-vfx --example vfx_probe -- <out_dir>

use floptle_core::math::{DVec3, Quat, Vec3};
use floptle_core::transform::Transform;
use floptle_render::particles::{ParticleBatch, ParticleGlobals};
use floptle_render::{
    Globals, Gpu, InstanceRaw, MaterialParams, MeshId, Particles, Projection, Raster,
    RenderCamera, Retro, TexId, TexSampling, TextureData, cube, instance_of_mat,
};
use floptle_vfx::{
    BillboardOrient, Blend, Burst, Clip, Curve, EffectInstance, EmitShape, Key, Lane, LaneTarget,
    Look, ParticleEffect, Playback, RenderMode, Track, Value, ValueOrCurve, collect_billboards,
};
use std::sync::Arc;

const W: u32 = 960;
const H: u32 = 540;
const RETRO_H: u32 = 135;
const GRAVITY: Vec3 = Vec3::new(0.0, -9.81, 0.0);

fn scalar_curve(keys: &[(f32, f32)]) -> ValueOrCurve {
    ValueOrCurve::Curve(Curve {
        keys: keys.iter().map(|&(t, v)| Key::new(t, Value::F32(v))).collect(),
        extrapolate: Default::default(),
    })
}

fn rgba_curve(keys: &[(f32, [f32; 4])]) -> ValueOrCurve {
    ValueOrCurve::Curve(Curve {
        keys: keys.iter().map(|&(t, v)| Key::new(t, Value::Rgba(v))).collect(),
        extrapolate: Default::default(),
    })
}

/// The probe effect: what a first real authored `.vfx.ron` will look like.
fn fountain_effect() -> ParticleEffect {
    let smoke = Track {
        name: "Smoke".into(),
        look: Look {
            render: RenderMode::Billboard { texture: Some("soft".into()) },
            blend: Blend::Alpha,
            ..Look::default()
        },
        clips: vec![Clip { start: 0.0, end: 1.6 }],
        rate: 26.0,
        shape: EmitShape::Cone { angle: 24.0, radius: 0.25 },
        particle_lifetime: 1.4,
        lifetime_jitter: 0.3,
        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 2.0, 0.0))),
        size: scalar_curve(&[(0.0, 0.35), (1.0, 1.3)]),
        // Roll (z) — the screen-facing spin for billboards, now that rotation is Euler.
        rotation: ValueOrCurve::Curve(Curve {
            keys: vec![
                Key::new(0.0, Value::Vec3(Vec3::ZERO)),
                Key::new(1.0, Value::Vec3(Vec3::new(0.0, 0.0, 1.4))),
            ],
            extrapolate: Default::default(),
        }),
        color: rgba_curve(&[
            (0.0, [0.85, 0.85, 0.92, 0.0]),
            (0.25, [0.8, 0.8, 0.88, 0.65]),
            (1.0, [0.7, 0.7, 0.78, 0.0]),
        ]),
        drag: 0.4,
        ..Track::default()
    };
    let sparks = Track {
        name: "Sparks".into(),
        look: Look {
            render: RenderMode::Billboard { texture: None },
            blend: Blend::Additive,
            ..Look::default()
        },
        clips: vec![Clip { start: 0.0, end: 1.5 }],
        bursts: vec![Burst { t: 0.1, count: 40 }],
        // The fountain wanes across the effect — one automation lane on Rate.
        automation: vec![Lane {
            target: LaneTarget::Rate,
            curve: Curve {
                keys: vec![Key::new(0.0, Value::F32(1.0)), Key::new(2.0, Value::F32(0.2))],
                extrapolate: Default::default(),
            },
        }],
        rate: 90.0,
        shape: EmitShape::Cone { angle: 32.0, radius: 0.05 },
        particle_lifetime: 0.8,
        lifetime_jitter: 0.5,
        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 5.0, 0.0))),
        size: scalar_curve(&[(0.0, 0.14), (1.0, 0.0)]),
        color: rgba_curve(&[
            (0.0, [1.0, 0.95, 0.6, 1.0]),
            (0.6, [1.0, 0.45, 0.12, 0.8]),
            (1.0, [0.8, 0.2, 0.05, 0.0]),
        ]),
        gravity: 1.0,
        ..Track::default()
    };
    ParticleEffect {
        name: "FountainProbe".into(),
        lifetime: 2.0,
        playback: Playback::OneShot,
        tracks: vec![smoke, sparks],
        seed: 7,
        ..ParticleEffect::default()
    }
}

/// A showcase of the non-camera-facing orientation modes, all at once, so one frame
/// proves each looks right: a ring of FLAT decals on the ground, a row of UPRIGHT
/// cards down the middle, and a fountain of VELOCITY-stretched sparks. Viewed from a
/// near-level camera, the ground ring reads as thin ellipses (proving it's flat, not
/// facing us), the cards stay vertical, and the sparks elongate along their motion.
fn orient_showcase() -> ParticleEffect {
    let card = |name: &str, orient, blend, aspect, stretch, color: [f32; 4]| Track {
        name: name.into(),
        look: Look {
            render: RenderMode::Billboard { texture: Some("soft".into()) },
            blend,
            orient,
            aspect,
            stretch,
            ..Look::default()
        },
        clips: vec![Clip { start: 0.0, end: 4.0 }],
        rate: 0.0,
        particle_lifetime: 20.0,
        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::ZERO)),
        size: ValueOrCurve::Const(Value::F32(0.5)),
        color: ValueOrCurve::Const(Value::Rgba(color)),
        ..Track::default()
    };
    // A ring of flat decals lying on the floor (big + bright so the flat ellipses
    // read against the floor even at a shallow viewing angle).
    let ground = Track {
        bursts: vec![Burst { t: 0.0, count: 10 }],
        shape: EmitShape::Ring { radius: 2.2 },
        ..card("Ground", BillboardOrient::Horizontal, Blend::Additive, 1.0, 1.0, [0.2, 0.9, 1.0, 1.0])
    };
    // A row of tall upright cards along X.
    let upright = Track {
        bursts: vec![Burst { t: 0.0, count: 5 }],
        shape: EmitShape::Edge { length: 3.6 },
        ..card("Upright", BillboardOrient::Vertical, Blend::Additive, 0.45, 1.0, [0.4, 1.0, 0.4, 1.0])
    };
    // A spray of velocity-stretched sparks shooting up.
    let sparks = Track {
        rate: 60.0,
        shape: EmitShape::Cone { angle: 14.0, radius: 0.04 },
        particle_lifetime: 0.9,
        lifetime_jitter: 0.3,
        velocity: ValueOrCurve::Const(Value::Vec3(Vec3::new(0.0, 6.0, 0.0))),
        size: ValueOrCurve::Const(Value::F32(0.12)),
        color: ValueOrCurve::Const(Value::Rgba([1.0, 0.95, 0.6, 1.0])),
        gravity: 1.0,
        ..card("Sparks", BillboardOrient::Velocity, Blend::Additive, 0.4, 3.0, [1.0, 0.9, 0.5, 1.0])
    };
    ParticleEffect {
        name: "OrientShowcase".into(),
        lifetime: 4.0,
        playback: Playback::OneShot,
        tracks: vec![ground, upright, sparks],
        seed: 3,
        ..ParticleEffect::default()
    }
}

/// A soft radial puff — the classic smoke sprite, generated so the probe needs
/// no asset files.
fn soft_puff(size: u32) -> TextureData {
    let mut pixels = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 + 0.5) / size as f32 - 0.5;
            let dy = (y as f32 + 0.5) / size as f32 - 0.5;
            let d = (dx * dx + dy * dy).sqrt() * 2.0;
            let k = (1.0 - d).clamp(0.0, 1.0);
            let a = (k * k * (3.0 - 2.0 * k) * 255.0) as u8; // smoothstep falloff
            pixels.extend_from_slice(&[255, 255, 255, a]);
        }
    }
    TextureData { pixels, width: size, height: size }
}


struct Harness {
    gpu: Gpu,
    color_tex: wgpu::Texture,
    color_view: wgpu::TextureView,
    raster: Raster,
    particles: Particles,
    retro: Retro,
    scene: Vec<(MeshId, Option<TexId>, InstanceRaw)>,
    globals: Globals,
    pglobals: ParticleGlobals,
    cam: RenderCamera,
    cam_right: Vec3,
    cam_up: Vec3,
    fx: Arc<floptle_vfx::CompiledEffect>,
    registry: std::collections::HashMap<String, TexId>,
    emitter: Transform,
}

impl Harness {
    fn render(&mut self, t: f32, retro_mode: bool) -> Vec<u8> {
        // Deterministic scrub: a fresh instance simulated from zero every frame.
        let mut inst = EffectInstance::new(Arc::clone(&self.fx), 1);
        inst.simulate_to(t, GRAVITY);

        let xf = self.emitter.render_matrix(self.cam.world_position);
        let fwd = self.cam.rotation * Vec3::NEG_Z;
        let mut packed = Vec::new();
        let mut draws = Vec::new();
        collect_billboards(
            &inst, xf, xf, fwd, self.cam_right, self.cam_up, &mut packed, &mut draws,
        );
        println!(
            "t={t}: alive={} packed={} draws={:?}",
            inst.alive(),
            packed.len(),
            draws.iter().map(|d| (d.blend, d.range.clone())).collect::<Vec<_>>()
        );
        // Resolve textures EXACTLY as the editor does: by path, through a registry
        // map (not a direct TexId). This exercises the real editor resolve path.
        let batches: Vec<ParticleBatch> = draws
            .iter()
            .map(|d| ParticleBatch {
                texture: d.texture.as_deref().and_then(|p| self.registry.get(p).copied()),
                blend: d.blend,
                range: d.range.clone(),
            })
            .collect();

        let clear = Some([0.10, 0.12, 0.16, 1.0]);
        let (color, depth): (&wgpu::TextureView, &wgpu::TextureView) = if retro_mode {
            (self.retro.color_view(), self.retro.depth_view())
        } else {
            (&self.color_view, self.gpu.depth_view())
        };
        self.raster.draw_scene(&self.gpu, color, depth, self.globals, &self.scene, clear, None);
        self.particles.draw(&self.gpu, color, depth, self.pglobals, &packed, &batches, &self.raster);
        if retro_mode {
            self.retro.blit_to(&self.gpu, &self.color_view);
        }
        read_pixels(&self.gpu, &self.color_tex)
    }
}

fn main() {
    let out_dir = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let gpu = Gpu::headless(W, H);

    let color_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("probe-color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color_tex.create_view(&wgpu::TextureViewDescriptor::default());

    let mut raster = Raster::new(&gpu);
    let particles = Particles::new(&gpu);
    let retro = Retro::new(&gpu, RETRO_H);
    let box_mesh = raster.register(&gpu, &cube(1.0), None);
    // Registry keyed by PATH, exactly like the editor's texture_registry — the
    // probe resolves batches through this map, exercising the real resolve path.
    let mut registry = std::collections::HashMap::new();
    registry.insert("soft".to_string(), raster.register_texture(&gpu, &soft_puff(64), TexSampling::default()));

    let cam = RenderCamera::new(
        DVec3::new(0.0, 1.7, 5.2),
        Quat::from_rotation_x(-0.1),
        Projection::Perspective { fov_y: 55f32.to_radians(), near: 0.1, far: 2000.0 },
    );
    let aspect = W as f32 / H as f32;
    let globals = Globals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        light_dir: [0.4, 0.8, 0.5, 0.0],
        light_color: [0.9, 0.88, 0.85, 0.0],
        ambient: [0.35, 0.35, 0.4, 0.0],
        ..Default::default()
    };
    let (r, u) = (cam.rotation * Vec3::X, cam.rotation * Vec3::Y);
    let pglobals = ParticleGlobals {
        view_proj: cam.view_proj(aspect).to_cols_array_2d(),
        cam_right: [r.x, r.y, r.z, 0.0],
        cam_up: [u.x, u.y, u.z, 0.0],
        fog_color: [0.0; 4],
        fog_params: [0.0; 4], // fog off in the probe
    };
    let (cam_right, cam_up) = (r, u);

    // Floor + an occluder box in front of the fountain: particles passing behind
    // it must clip against the shared depth buffer.
    let mut scene: Vec<(MeshId, Option<TexId>, InstanceRaw)> = Vec::new();
    let mut place = |pos: [f64; 3], scale: [f32; 3], color: [f32; 3]| {
        let m = MaterialParams::flat(color);
        let mut t = Transform::from_translation(DVec3::from_array(pos));
        t.scale = Vec3::from_array(scale);
        scene.push((box_mesh, None, instance_of_mat(t.render_matrix(cam.world_position), &m)));
    };
    // Dark floor so light smoke and bright sparks read against it.
    place([0.0, -0.25, 0.0], [24.0, 0.5, 24.0], [0.16, 0.17, 0.2]);
    // Off to the right, overlapping only the fountain's right edge on screen —
    // particles drifting behind it must clip, the rest stay visible.
    place([1.1, 0.5, 1.6], [1.0, 1.0, 1.0], [0.55, 0.38, 0.3]);

    let fx = Arc::new(fountain_effect().compile());
    let emitter = Transform::from_translation(DVec3::new(0.0, 0.15, 0.0));

    let mut h = Harness {
        gpu,
        color_tex,
        color_view,
        raster,
        particles,
        retro,
        scene,
        globals,
        pglobals,
        cam,
        cam_right,
        cam_up,
        fx,
        registry,
        emitter,
    };

    for (t, name) in [(0.35, "vfx_t035.png"), (0.9, "vfx_t090.png"), (1.6, "vfx_t160.png")] {
        let px = h.render(t, false);
        save_png(&px, &format!("{out_dir}/{name}"));
    }
    let px = h.render(0.9, true);
    save_png(&px, &format!("{out_dir}/vfx_retro.png"));

    // Orientation showcase: swap in the multi-mode effect and grab one frame.
    h.fx = Arc::new(orient_showcase().compile());
    let px = h.render(0.6, false);
    save_png(&px, &format!("{out_dir}/vfx_orient.png"));
    h.fx = Arc::new(fountain_effect().compile());

    // Determinism: same t, fresh sim, fresh render — must be byte-identical.
    let a = h.render(0.9, false);
    let b = h.render(0.9, false);
    println!("determinism (t=0.9 re-sim + re-render): {}", if a == b { "BIT-IDENTICAL" } else { "DIVERGED" });
    assert_eq!(a, b, "deterministic sim must render byte-identical frames");
    println!("wrote vfx_t035/t090/t160 + vfx_retro to {out_dir}");
}

fn read_pixels(gpu: &Gpu, tex: &wgpu::Texture) -> Vec<u8> {
    let bpp = 4u32;
    let unpadded = W * bpp;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;
    let buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (padded * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("readback") });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(H),
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([encoder.finish()]);
    let slice = buf.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((W * H * 4) as usize);
    for row in 0..H {
        let start = (row * padded) as usize;
        pixels.extend_from_slice(&data[start..start + unpadded as usize]);
    }
    pixels
}

fn save_png(pixels: &[u8], path: &str) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), W, H);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(pixels).unwrap();
}
