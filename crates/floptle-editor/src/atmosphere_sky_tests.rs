//! The sky seen from inside a world's air.
//!
//! An atmosphere seen from the ground used to be a faint tint over space (its
//! overhead opacity was about 0.08), so a game wanting a daytime sky painted
//! one with fog, which could only be on or off: a climb out of the air was a
//! coloured sky one frame and black space the next. These shoot a camera
//! climbing out of the air of a planet far from the origin and read the sky.

use floptle_core::math::{DVec3, Quat};
use floptle_core::{CelestialBody, Matter, Transform};
use floptle_render::{Projection, RenderCamera};

const RADIUS: f64 = 100.0;
const HEIGHT: f64 = 40.0;
/// Far from the origin on purpose: the altitude has to come out right in f64.
const CENTRE: DVec3 = DVec3::new(50_000.0, 0.0, 0.0);

fn world(ed: &mut crate::Editor) {
    let w = &mut ed.world;
    let sky = w.spawn();
    w.insert(sky, Transform::IDENTITY);
    w.insert(
        sky,
        Matter::Skybox {
            color: [0.02, 0.02, 0.03],
            size: 500.0,
            texture: None,
            tint: [1.0; 3],
            shader: None,
            shader_params: Default::default(),
        },
    );
    let planet = w.spawn();
    w.insert(planet, Transform { translation: CENTRE, ..Transform::IDENTITY });
    w.insert(
        planet,
        CelestialBody {
            body_radius: RADIUS,
            atmo_color: [0.45, 0.62, 0.95],
            atmo_height: HEIGHT,
            atmo_density: 1.0,
            ..Default::default()
        },
    );
}

/// The mean colour of the sky (the top two thirds of a view raised by `pitch`
/// degrees) from `alt` over the surface, 0..1 per channel.
fn sky_at(ed: &mut crate::Editor, alt: f64) -> [f32; 3] {
    sky_toward(ed, alt, 10.0)
}

fn sky_toward(ed: &mut crate::Editor, alt: f64, pitch: f32) -> [f32; 3] {
    let eye = CENTRE + DVec3::new(0.0, RADIUS + alt, 0.0);
    let cam = RenderCamera::new(
        eye,
        Quat::from_rotation_x(pitch.to_radians()),
        Projection::Perspective { fov_y: 1.2, near: 0.05, far: 300_000.0 },
    );
    let (w, h) = (48u32, 27u32);
    let tex = crate::capture::render_frame_texture(ed, &cam, None, w, h, u32::MAX, false, false)
        .expect("a GPU editor renders");
    let px = crate::offscreen::read_back_frame(ed.gpu.as_ref().unwrap(), &tex).expect("the frame reads back");
    let mut sum = [0.0f32; 3];
    let rows = h * 2 / 3;
    for y in 0..rows {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            for c in 0..3 {
                sum[c] += px[i + c] as f32 / 255.0;
            }
        }
    }
    sum.map(|s| s / (rows * w) as f32)
}

/// From the ground the sky is the air's colour, not space with a tint; at
/// twice the atmosphere's height it is space; and between the two, a climb a
/// metre a frame never moves the sky's brightness by more than a small step
/// — least of all at the top of the shell, where the old sky switched off.
#[test]
fn a_climb_out_of_the_air_fades_the_sky_into_space() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    world(&mut ed);
    let luma = |c: [f32; 3]| (c[0] + c[1] + c[2]) / 3.0;

    // Straight up, where the old sky was 8% air and 92% space.
    let zenith = sky_toward(&mut ed, 1.0, 90.0);
    assert!(
        luma(zenith) > 0.55 && zenith[2] > zenith[0] + 0.1,
        "from the ground the sky overhead should be the air's blue, not space: {zenith:?}"
    );
    let ground = sky_at(&mut ed, 1.0);
    let space = sky_at(&mut ed, HEIGHT * 2.0);
    assert!(luma(space) < 0.2, "at twice the atmosphere's height the sky should be space: {space:?}");

    let mut prev = luma(ground);
    let mut worst = (0.0f32, 0.0f64);
    let mut alt = 1.0;
    while alt <= HEIGHT * 2.0 {
        let l = luma(sky_at(&mut ed, alt));
        let step = (l - prev).abs();
        if step > worst.0 {
            worst = (step, alt);
        }
        prev = l;
        alt += 1.0;
    }
    assert!(
        worst.0 < 0.05,
        "the sky jumped by {:.3} in one metre of climb at {:.0} m (the shell's top is {HEIGHT} m)",
        worst.0,
        worst.1
    );
}

/// The colour at the centre of a `rot`-turned view from `eye`, 0..1.
fn centre_from(ed: &mut crate::Editor, eye: DVec3, rot: Quat) -> [f32; 3] {
    let cam = RenderCamera::new(eye, rot, Projection::Perspective { fov_y: 0.6, near: 0.05, far: 300_000.0 });
    let (w, h) = (32u32, 18u32);
    let tex = crate::capture::render_frame_texture(ed, &cam, None, w, h, u32::MAX, false, false)
        .expect("a GPU editor renders");
    let px = crate::offscreen::read_back_frame(ed.gpu.as_ref().unwrap(), &tex).expect("the frame reads back");
    let i = (((h / 2) * w + w / 2) * 4) as usize;
    [px[i] as f32 / 255.0, px[i + 1] as f32 / 255.0, px[i + 2] as f32 / 255.0]
}

/// A planet with next to no air, a dark sky, and magenta fog from the Lighting
/// node.
fn foggy_planet(ed: &mut crate::Editor, edit: impl FnOnce(&mut floptle_core::Light)) {
    let w = &mut ed.world;
    let sky = w.spawn();
    w.insert(sky, Transform::IDENTITY);
    w.insert(
        sky,
        Matter::Skybox {
            color: [0.02, 0.02, 0.03],
            size: 500.0,
            texture: None,
            tint: [1.0; 3],
            shader: None,
            shader_params: Default::default(),
        },
    );
    let planet = w.spawn();
    w.insert(planet, Transform { translation: CENTRE, ..Transform::IDENTITY });
    // A whisper of air, black and far too thin to see: it is what makes the
    // sky pass run, which is the pass that fogs the sky.
    w.insert(
        planet,
        CelestialBody {
            body_radius: RADIUS,
            atmo_color: [0.0; 3],
            atmo_height: 1.0,
            atmo_density: 0.002,
            ..Default::default()
        },
    );
    let mut l = floptle_core::Light { fog: true, fog_color: [1.0, 0.0, 1.0], ..Default::default() };
    edit(&mut l);
    let lighting = w.spawn();
    w.insert(lighting, l);
}

fn magenta(c: [f32; 3]) -> bool {
    c[0] > 0.5 && c[2] > 0.5 && c[1] < 0.3
}

/// Standing on the side of a planet, the local "up" is world +X. Looking that
/// way is looking at the zenith, which the fog's sky blend leaves alone — on a
/// flat world it is a horizontal ray, which the blend fogs in full.
#[test]
fn the_fog_sky_blend_follows_the_planets_horizon() {
    let eye = CENTRE + DVec3::new(RADIUS + 1.0, 0.0, 0.0);
    // Forward (−Z) turned to +X: straight up, for someone standing here.
    let up = Quat::from_rotation_y(-std::f32::consts::FRAC_PI_2);
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    foggy_planet(&mut ed, |l| l.fog_follows_body = true);
    let round = centre_from(&mut ed, eye, up);
    assert!(!magenta(round), "on a round world the zenith took the horizon's fog: {round:?}");

    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    foggy_planet(&mut ed, |l| l.fog_follows_body = false);
    let flat = centre_from(&mut ed, eye, up);
    assert!(magenta(flat), "on a flat world a horizontal ray takes the fog in full: {flat:?}");
}

/// A thin volumetric layer five metres deep, and a white block fifteen metres
/// away at head height on the planet's surface. On a round world the block is
/// inside the layer and fogs; measured in world y (the planet's top is at
/// y = 100) the layer is far below and the block stays white.
#[test]
fn volumetric_fog_is_a_shell_around_the_planet() {
    let eye = CENTRE + DVec3::new(0.0, RADIUS + 1.5, 0.0);
    let thick = |l: &mut floptle_core::Light, round: bool| {
        l.fog_volumetric = true;
        l.fog_density = 0.4;
        l.fog_height = 5.0;
        l.fog_falloff = 1.0;
        l.fog_noise = 0.0;
        l.fog_light = 0.0;
        l.fog_follows_body = round;
    };
    let block = |ed: &mut crate::Editor| {
        let b = ed.world.spawn();
        ed.world.insert(
            b,
            Transform {
                translation: CENTRE + DVec3::new(0.0, RADIUS + 1.5, -15.0),
                scale: floptle_core::math::Vec3::splat(3.0),
                ..Transform::IDENTITY
            },
        );
        ed.world.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0; 3] });
    };
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    foggy_planet(&mut ed, |l| thick(l, true));
    block(&mut ed);
    let round = centre_from(&mut ed, eye, Quat::IDENTITY);
    assert!(round[1] < 0.35 && round[0] > 0.4, "the block in a round world's fog layer is not fogged: {round:?}");

    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    foggy_planet(&mut ed, |l| thick(l, false));
    block(&mut ed);
    let flat = centre_from(&mut ed, eye, Quat::IDENTITY);
    assert!(flat[1] > 0.5, "measured in world y the layer is far below, so the block should be clear: {flat:?}");
}

/// The colour `n` pixels right of centre in a `rot`-turned view from `eye`,
/// 0..1, with a narrow lens (so a far disc covers the middle).
fn pixel_from(ed: &mut crate::Editor, eye: DVec3, rot: Quat, n: u32) -> [f32; 3] {
    let cam = RenderCamera::new(eye, rot, Projection::Perspective { fov_y: 0.5, near: 0.05, far: 300_000.0 });
    let (w, h) = (64u32, 36u32);
    let tex = crate::capture::render_frame_texture(ed, &cam, None, w, h, u32::MAX, false, false)
        .expect("a GPU editor renders");
    let px = crate::offscreen::read_back_frame(ed.gpu.as_ref().unwrap(), &tex).expect("the frame reads back");
    let i = (((h / 2) * w + w / 2 + n) * 4) as usize;
    [px[i] as f32 / 255.0, px[i + 1] as f32 / 255.0, px[i + 2] as f32 / 255.0]
}

/// The view from the top of the planet turned up by `deg` toward −Z.
fn raised(deg: f32) -> Quat {
    Quat::from_rotation_x(deg.to_radians())
}

/// **Flat fog on a round world stays in the air.** A bright disc out in space
/// at 30° elevation — a moon, a star — seen through Low (flat) fog that
/// follows the planet keeps its own colour: the ramp runs over the stretch of
/// ray inside the air, not over every metre out to the disc, which used to
/// paint it the fog's magenta.
#[test]
fn flat_fog_on_a_round_world_leaves_a_disc_in_space_its_colour() {
    let eye = CENTRE + DVec3::new(0.0, RADIUS + 1.0, 0.0);
    let shoot = |fog: bool| -> Option<[f32; 3]> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        world(&mut ed);
        let l = floptle_core::Light {
            fog,
            fog_color: [1.0, 0.0, 1.0],
            // Low fog: it starts past the 78 m of air a 30° ray crosses.
            fog_start: 100.0,
            fog_end: 300.0,
            fog_follows_body: true,
            ..Default::default()
        };
        let lighting = ed.world.spawn();
        ed.world.insert(lighting, l);
        let (s, c) = 30f64.to_radians().sin_cos();
        let disc = ed.world.spawn();
        ed.world.insert(
            disc,
            Transform {
                translation: eye + DVec3::new(0.0, s, -c) * 20_000.0,
                scale: floptle_core::math::Vec3::splat(900.0),
                ..Transform::IDENTITY
            },
        );
        ed.world.insert(disc, Matter::Primitive { shape: floptle_core::Shape::Sphere, color: [1.0, 0.9, 0.5] });
        ed.world.insert(disc, floptle_core::Material { unlit: true, color: [1.0, 0.9, 0.5], ..Default::default() });
        Some(pixel_from(&mut ed, eye, raised(30.0), 0))
    };
    let Some(clear) = shoot(false) else { return };
    let Some(fogged) = shoot(true) else { return };
    assert!(clear[0] > 0.6 && clear[1] > 0.5, "the fixture: the disc is in the middle of the view: {clear:?}");
    let off = (0..3).map(|c| (clear[c] - fogged[c]).abs()).fold(0.0f32, f32::max);
    assert!(off < 0.08, "flat fog moved the disc in space from {clear:?} to {fogged:?}");
}

/// **The sun low in thick air is the sun, dimmed and orange — not the haze.**
/// The star 5° over the horizon from the ground of a dense atmosphere, against
/// the same star with no air: its disc keeps at least 40% of its brightness
/// over the sky around it, and comes out warmer than it is. The space behind
/// the air was drawn at the sky's own transparency, about a tenth near the
/// horizon, and the disc went with it.
#[test]
fn the_sun_low_in_thick_air_keeps_its_disc_and_warms() {
    let eye = CENTRE + DVec3::new(0.0, RADIUS + 1.0, 0.0);
    // `air` 0 is a whisper of air too thin to see, which is what keeps the sky
    // pass running: the same disc with nothing in front of it.
    let shoot = |air: f64| -> Option<([f32; 3], [f32; 3])> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        let w = &mut ed.world;
        let sky = w.spawn();
        w.insert(sky, Transform::IDENTITY);
        w.insert(
            sky,
            Matter::Skybox {
                color: [0.02, 0.02, 0.03],
                size: 500.0,
                texture: None,
                tint: [1.0; 3],
                shader: None,
                shader_params: Default::default(),
            },
        );
        let planet = w.spawn();
        w.insert(planet, Transform { translation: CENTRE, ..Transform::IDENTITY });
        w.insert(
            planet,
            CelestialBody {
                body_radius: RADIUS,
                atmo_color: [0.2, 0.28, 0.45], // a dim sky, so the disc over it stays under white
                atmo_height: if air > 0.0 { air } else { 1.0 },
                atmo_density: if air > 0.0 { 1.0 } else { 0.002 },
                ..Default::default()
            },
        );
        // Far past the far plane, so what is seen is the sky's own disc. A
        // dim grey star, so its disc stays under white and its colour shows.
        let (s, c) = 5f64.to_radians().sin_cos();
        let star = w.spawn();
        w.insert(star, Transform { translation: eye + DVec3::new(0.0, s, -c) * 5.0e6, ..Transform::IDENTITY });
        w.insert(star, CelestialBody { body_radius: 1000.0, luminosity: 2.0e7, star_color: [0.2, 0.2, 0.2], ..Default::default() });
        let lighting = w.spawn();
        w.insert(lighting, floptle_core::Light { stars: true, ..Default::default() });
        let rot = raised(5.0);
        // The disc, and the sky a lens-width to the side of it.
        Some((pixel_from(&mut ed, eye, rot, 0), pixel_from(&mut ed, eye, rot, 28)))
    };
    let Some((space_disc, space_sky)) = shoot(0.0) else { return };
    let Some((air_disc, air_sky)) = shoot(HEIGHT) else { return };
    let luma = |c: [f32; 3]| (c[0] + c[1] + c[2]) / 3.0;
    let in_space = luma(space_disc) - luma(space_sky);
    let in_air = luma(air_disc) - luma(air_sky);
    assert!(in_space > 0.2, "the fixture: the disc shows in space ({space_disc:?} over {space_sky:?})");
    assert!(
        in_air >= 0.4 * in_space,
        "low in thick air the disc stands {in_air:.3} over its sky, against {in_space:.3} in space"
    );
    // The light the disc adds over its sky has more red in it than blue —
    // measured where nothing clips, or a white block would read as warm
    // against a blue sky.
    assert!(air_disc.iter().all(|&c| c < 0.99), "the fixture: the disc clips, so its colour cannot show: {air_disc:?}");
    let warm = |c: [f32; 3], s: [f32; 3]| (c[0] - s[0]) - (c[2] - s[2]);
    assert!(warm(air_disc, air_sky) > 0.05, "the low sun is not warmer than white: {air_disc:?} over {air_sky:?}");
}

/// Lit volumetric fog over a field of blocks, drawn with the fog marched once
/// per four pixels and drawn with every fragment marching its own: the same
/// picture to within two steps of 8-bit colour on average, and 95 % or more
/// of the ground taking the shared march, a field of thin poles in front of
/// it included: each texel holds the nearest and the farthest surface of its
/// pixels, so a pole and the ground behind it both find theirs. The fog is thick enough that
/// marching it is most of what every pixel shows.
#[test]
fn volumetric_fog_marched_once_per_four_pixels_draws_the_same_picture() {
    use floptle_core::math::Vec3;
    // `None` with no GPU; `pass` marches the fog once for the view, `debug`
    // paints what reads it magenta.
    let shot = |pass: bool, debug: bool| -> Option<Vec<u8>> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        // The march, as a project from before the grid draws it.
        ed.project.fog_method = Some(floptle_scene::FogMethodDoc::March);
        let w = &mut ed.world;
        let floor = w.spawn();
        w.insert(floor, Transform { translation: DVec3::new(0.0, -1.0, -40.0), scale: Vec3::new(200.0, 1.0, 200.0), ..Transform::IDENTITY });
        w.insert(floor, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.4, 0.5, 0.3] });
        for k in 0..12 {
            let b = w.spawn();
            let (x, z) = ((k % 4) as f64 * 9.0 - 13.0, -8.0 - (k / 4) as f64 * 14.0);
            w.insert(b, Transform { translation: DVec3::new(x, 1.0, z), scale: Vec3::new(2.0, 3.0 + k as f32 * 0.4, 2.0), ..Transform::IDENTITY });
            w.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.8, 0.75, 0.7] });
        }
        // A field of thin poles, a pixel or two wide: like the stems and
        // leaves of plants, nearly every pixel of one is an edge with the
        // ground behind it.
        for k in 0..300 {
            let b = w.spawn();
            let (x, z) = ((k % 30) as f64 * 0.6 - 9.0, -3.0 - (k / 30) as f64 * 1.2);
            w.insert(b, Transform { translation: DVec3::new(x, 0.5, z), scale: Vec3::new(0.06, 3.0, 0.06), ..Transform::IDENTITY });
            w.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.3, 0.5, 0.2] });
        }
        let lighting = w.spawn();
        w.insert(
            lighting,
            floptle_core::Light {
                fog: true,
                fog_color: [0.85, 0.85, 0.9],
                fog_volumetric: true,
                fog_density: 0.06,
                fog_height: 6.0,
                fog_falloff: 4.0,
                fog_noise: 0.5,
                fog_light: 1.0,
                fog_steps: 16,
                ..Default::default()
            },
        );
        let rm = ed.raymarch.as_mut()?;
        rm.set_fog_pass(pass);
        rm.set_fog_debug(debug);
        // Looking down the field, the ground filling the frame.
        let cam = RenderCamera::new(
            DVec3::new(0.0, 6.0, 6.0),
            Quat::from_rotation_x(-0.45),
            Projection::Perspective { fov_y: 1.0, near: 0.05, far: 1000.0 },
        );
        let tex = crate::capture::render_frame_texture(&mut ed, &cam, None, 160, 90, u32::MAX, false, false)?;
        crate::offscreen::read_back_frame(ed.gpu.as_ref()?, &tex)
    };
    let (Some(shared), Some(own), Some(served)) = (shot(true, false), shot(false, false), shot(true, true)) else { return };
    let n = shared.len() / 4;
    let px = |f: &[u8], i: usize| [f[i * 4], f[i * 4 + 1], f[i * 4 + 2]];
    let mean = (0..n)
        .map(|i| (0..3).map(|c| (px(&shared, i)[c] as i32 - px(&own, i)[c] as i32).abs()).max().unwrap() as f64)
        .sum::<f64>()
        / n as f64;
    let fogged = (0..n).filter(|&i| px(&own, i) != px(&shared, i) || px(&served, i) == [255, 0, 255]).count();
    assert!(fogged == n || mean > 0.0, "fixture: the shared march changed nothing");
    assert!(mean <= 2.0, "marched once per four pixels, the picture is {mean:.2}/255 from every pixel marching its own");
    // The ground below the horizon: above it are the sky (fogged by the
    // raymarch pass, which a scene of meshes alone does not draw) and the far
    // ground at a grazing angle, where neighbouring pixels are too far apart
    // in depth to share a march.
    let ground = (n / 5)..n;
    let magenta = ground.clone().filter(|&i| px(&served, i) == [255, 0, 255]).count();
    assert!(magenta * 100 >= ground.len() * 95, "only {magenta} of {} ground pixels took the shared march", ground.len());
}


/// **FSR 1 brings two thirds of the screen up nearer the native picture
/// than bilinear does.** Thin dark bars at many angles over pale ground,
/// drawn at the screen's own resolution and at a render scale of 0.67,
/// brought up bilinearly and through FSR 1. Measured: off the native
/// picture by 1.22 levels of luma on average bilinear, 0.83 through FSR.
/// Bilinear's sharpening makes its steepest edges steeper than FSR's, but
/// staircased; FSR keeps an edge an edge. `FLOPTLE_FSR_PNG=<dir>` writes
/// all three.
#[cfg(feature = "editor-ui")]
#[test]
fn fsr1_brings_a_scaled_frame_nearer_native_than_bilinear() {
    use floptle_core::math::Vec3;
    let (w, h) = (960u32, 540u32);
    let shot = |scale: f32, up: floptle_scene::UpscaleDoc| -> Option<Vec<u8>> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        // A render scale, not the retro look (a test editor starts retro).
        ed.project.retro = false;
        ed.project.render_scale = scale;
        ed.project.upscale = Some(up);
        let wd = &mut ed.world;
        let floor = wd.spawn();
        wd.insert(floor, Transform { translation: DVec3::new(0.0, -0.5, -8.0), scale: Vec3::new(40.0, 0.2, 40.0), ..Transform::IDENTITY });
        wd.insert(floor, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.9, 0.88, 0.85] });
        for k in 0..18 {
            let b = wd.spawn();
            let ang = k as f32 * 0.17;
            wd.insert(
                b,
                Transform {
                    translation: DVec3::new((k % 6) as f64 * 1.2 - 3.0, -0.3, -5.0 - (k / 6) as f64 * 1.5),
                    rotation: Quat::from_rotation_y(ang),
                    scale: Vec3::new(1.4, 0.05, 0.06),
                },
            );
            wd.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.05, 0.05, 0.08] });
        }
        let sun = wd.spawn();
        wd.insert(sun, Transform { rotation: Quat::from_rotation_x(-1.0), ..Transform::IDENTITY });
        wd.insert(sun, floptle_core::Light { intensity: 1.0, shadows: false, ..Default::default() });
        let cam = RenderCamera::new(DVec3::new(0.0, 2.0, 0.0), Quat::from_rotation_x(-0.5), Projection::Perspective { fov_y: 1.0, near: 0.05, far: 200.0 });
        let tex = crate::capture::render_frame_texture(&mut ed, &cam, None, w, h, u32::MAX, false, false)?;
        crate::offscreen::read_back_frame(ed.gpu.as_ref()?, &tex)
    };
    use floptle_scene::UpscaleDoc as U;
    let Some(native) = shot(1.0, U::Bilinear) else { return };
    let Some(bilinear) = shot(0.67, U::Bilinear) else { return };
    let Some(fsr) = shot(0.67, U::Fsr1) else { return };
    if let Ok(out) = std::env::var("FLOPTLE_FSR_PNG") {
        for (name, px) in [("native", &native), ("bilinear", &bilinear), ("fsr", &fsr)] {
            let tex = floptle_render::TextureData { pixels: px.clone(), width: w, height: h };
            std::fs::write(std::path::Path::new(&out).join(format!("up_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
    }
    let luma = |px: &[u8], i: usize| px[i * 4] as f32 * 0.3 + px[i * 4 + 1] as f32 * 0.59 + px[i * 4 + 2] as f32 * 0.11;
    let err = |px: &[u8]| (0..(w * h) as usize).map(|i| (luma(px, i) - luma(&native, i)).abs()).sum::<f32>() / (w * h) as f32;
    let (eb, ef) = (err(&bilinear), err(&fsr));
    assert!(eb > 0.3, "the fixture: a scaled frame differs from the native one ({eb:.2})");
    assert!(ef < eb * 0.85, "through FSR the scaled frame is {ef:.2} off native, bilinear {eb:.2}");
}

/// **The froxel fog draws the march's fog.** The field of poles (stems,
/// every pixel an edge) under lit, noisy volumetric fog, marched as before
/// and built in a froxel grid: within two levels in 255 of each other on
/// average (measured 0.35). Its cost is `tests/froxel_fog_cost.rs`'s.
/// `FLOPTLE_FROXEL_PNG=<dir>` writes both.
#[cfg(feature = "editor-ui")]
#[test]
fn froxel_fog_draws_the_marched_fog() {
    use floptle_core::math::Vec3;
    let shot = |grid: Option<([u32; 3], f32)>, w: u32, h: u32, frames: u32| -> Option<(Vec<u8>, f64)> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        let wd = &mut ed.world;
        let floor = wd.spawn();
        wd.insert(floor, Transform { translation: DVec3::new(0.0, -1.0, -40.0), scale: Vec3::new(200.0, 1.0, 200.0), ..Transform::IDENTITY });
        wd.insert(floor, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.4, 0.5, 0.3] });
        for k in 0..12 {
            let b = wd.spawn();
            let (x, z) = ((k % 4) as f64 * 9.0 - 13.0, -8.0 - (k / 4) as f64 * 14.0);
            wd.insert(b, Transform { translation: DVec3::new(x, 1.0, z), scale: Vec3::new(2.0, 3.0 + k as f32 * 0.4, 2.0), ..Transform::IDENTITY });
            wd.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.8, 0.75, 0.7] });
        }
        for k in 0..300 {
            let b = wd.spawn();
            let (x, z) = ((k % 30) as f64 * 0.6 - 9.0, -3.0 - (k / 30) as f64 * 1.2);
            wd.insert(b, Transform { translation: DVec3::new(x, 0.5, z), scale: Vec3::new(0.06, 3.0, 0.06), ..Transform::IDENTITY });
            wd.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.3, 0.5, 0.2] });
        }
        let sun = wd.spawn();
        wd.insert(sun, Transform { rotation: Quat::from_rotation_x(-0.6), ..Transform::IDENTITY });
        wd.insert(
            sun,
            floptle_core::Light {
                intensity: 1.0,
                fog: true,
                fog_color: [0.85, 0.85, 0.9],
                fog_volumetric: true,
                fog_density: 0.06,
                fog_height: 6.0,
                fog_falloff: 4.0,
                fog_noise: 0.5,
                fog_light: 1.0,
                fog_steps: 16,
                ..Default::default()
            },
        );
        // No grid: the march, as a project from before the grid draws it.
        ed.project.fog_method = Some(if grid.is_some() { floptle_scene::FogMethodDoc::Grid } else { floptle_scene::FogMethodDoc::March });
        ed.raymarch.as_mut()?.set_fog_froxels(grid);
        let cam = RenderCamera::new(
            DVec3::new(0.0, 6.0, 6.0),
            Quat::from_rotation_x(-0.45),
            Projection::Perspective { fov_y: 1.0, near: 0.05, far: 1000.0 },
        );
        let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false)?;
        let t = std::time::Instant::now();
        let mut px = Vec::new();
        for _ in 0..frames {
            px = crate::shot::render_frame_pixels(&mut ed, &cam, None, w, h, !0, false)?;
        }
        Some((px, t.elapsed().as_secs_f64() / frames as f64))
    };
    let grid = Some(([160, 90, 64], 400.0));
    let Some((march, t_march)) = shot(None, 1280, 720, 6) else { return };
    let Some((froxel, t_froxel)) = shot(grid, 1280, 720, 6) else { return };
    if let Ok(out) = std::env::var("FLOPTLE_FROXEL_PNG") {
        for (name, px) in [("march", &march), ("froxel", &froxel)] {
            let tex = floptle_render::TextureData { pixels: px.clone(), width: 1280, height: 720 };
            std::fs::write(std::path::Path::new(&out).join(format!("fog_{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
        }
    }
    let n = march.len() / 4;
    let mean = (0..n).map(|i| (0..3).map(|c| march[i * 4 + c].abs_diff(froxel[i * 4 + c]) as f64).fold(0.0, f64::max)).sum::<f64>() / n as f64;
    let _ = (t_march, t_froxel);
    assert!(mean > 0.0, "the fixture: the grid drew exactly the march, so it never ran");
    assert!(mean < 2.0, "the froxel fog is {mean:.2}/255 from the marched fog on average");
}

/// Eighty cut-out cards one behind another, for the two tests below: each a
/// picture whose alpha runs from holes through half-transparent texels (which
/// the colour pass draws solid) to solid, each shifted so its holes show
/// different cards behind. The editor, the cards, and the folder holding the
/// picture.
#[cfg(feature = "editor-ui")]
fn cutout_card_stack(tag: &str) -> Option<(crate::Editor, Vec<floptle_core::Entity>, std::path::PathBuf)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle_cutstack_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
    // Half holes, three in ten half-transparent (drawn solid), the rest
    // solid, in cells two texels across, so most of each card shows the
    // cards behind it.
    let mut leaf = Vec::with_capacity(64 * 64 * 4);
    let mut seed = 0x9e37_79b9u32;
    let mut cells = [0u8; 32 * 32];
    for c in &mut cells {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *c = match seed >> 24 {
            0..=127 => 0,
            128..=204 => 140,
            _ => 255,
        };
    }
    for y in 0..64u32 {
        for x in 0..64u32 {
            let a = cells[((y / 2) * 32 + x / 2) as usize];
            leaf.extend_from_slice(&[60 + (x * 3) as u8, 140, 70 + (y * 2) as u8, a]);
        }
    }
    let tex = floptle_render::TextureData { pixels: leaf, width: 64, height: 64 };
    std::fs::write(dir.join("textures/leaf.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
    ed.project_root = dir.clone();
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.0), ..Transform::IDENTITY });
    // Contact shadows read the prepass, so the frame runs one.
    ed.world.insert(sun, floptle_core::Light { intensity: 1.0, contact_shadows: true, ..Default::default() });
    let card = floptle_core::Material {
        texture: Some("textures/leaf.png".into()),
        blend: floptle_core::BlendMode::Cutout,
        alpha_cutoff: 0.5,
        ..Default::default()
    };
    let mut cards = Vec::new();
    for k in (0..80).rev() {
        let b = ed.world.spawn();
        ed.world.insert(
            b,
            Transform {
                translation: DVec3::new((k % 7) as f64 * 0.37, (k % 5) as f64 * 0.29, -4.0 - k as f64 * 0.125),
                scale: floptle_core::math::Vec3::new(12.0, 12.0, 0.02),
                ..Transform::IDENTITY
            },
        );
        ed.world.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [1.0, 1.0, 1.0] });
        ed.world.insert(b, card.clone());
        cards.push(b);
    }
    ed.camera.position = DVec3::ZERO;
    ed.camera.pitch = 0.0;
    ed.camera.yaw = 0.0;
    Some((ed, cards, dir))
}

/// **A stack of cut-out cards shades each visible pixel once.** Eighty
/// cards one behind another, each a picture whose alpha runs from holes
/// through half-transparent texels (which the colour pass draws solid) to
/// solid, each shifted so its holes show different cards behind. The depth
/// prepass primes exactly the texels the colour pass draws, and the colour
/// pass then tests depth EQUAL with no cut of its own, so the GPU tests
/// depth before shading and a hidden card is never shaded. The picture is
/// the same, pixel for pixel, as the old pipeline over the same primed
/// depth and as a colour pass that clears it; with the prepass priming only
/// the certainly-opaque texels, as it did, the half-transparent ones vanish
/// under EQUAL and the pictures differ. And the eighty cost about one.
///
/// The time asserted here is the whole frame's wall clock, about 22 ms of it
/// reading the picture back, which hides most of what the pipeline saves
/// (one card 22.4 ms; eighty 22.2 with this pipeline, 23.0 with the old one
/// over the same primed depth, 28.9 clearing it). By the GPU's own clock the
/// old pipeline shades the hidden cards: `tests/cutout_cards_cost.rs` holds
/// that ratio.
#[cfg(feature = "editor-ui")]
#[test]
fn a_stack_of_cutout_cards_shades_each_visible_pixel_once() {
    let Some((mut ed, cards, dir)) = cutout_card_stack("plain") else { return };
    let (w, h) = (1600u32, 1200u32);
    let cam = ed.camera.render_camera();
    let shoot = |ed: &mut crate::Editor, keep: bool, equal: bool, frames: u32| {
        let r = ed.raster.as_mut().unwrap();
        r.set_keep_primed_depth(keep);
        r.set_use_primed_pipeline(equal);
        let mut px = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..frames {
            px = crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame");
        }
        (px, t.elapsed().as_secs_f64() / frames as f64)
    };
    // Warm every path up (the first frames upload the picture), then measure.
    for (keep, equal) in [(true, true), (true, false), (false, false)] {
        let _ = shoot(&mut ed, keep, equal, 2);
    }
    let (new, t_new) = shoot(&mut ed, true, true, 8);
    let (old, t_old) = shoot(&mut ed, true, false, 8);
    let (cleared, t_cleared) = shoot(&mut ed, false, false, 8);
    // One card alone, for the cost of a single layer.
    for b in cards.iter().skip(1) {
        ed.world.despawn(*b);
    }
    let (_, t_one) = shoot(&mut ed, true, true, 8);
    let _ = std::fs::remove_dir_all(&dir);
    if let Ok(out) = std::env::var("FLOPTLE_CUTSTACK_PNG") {
        let tex = floptle_render::TextureData { pixels: new.clone(), width: w, height: h };
        std::fs::write(std::path::Path::new(&out).join("cutstack.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
    }
    let differ = |a: &[u8], b: &[u8]| a.iter().zip(b).filter(|(x, y)| x.abs_diff(**y) > 1).count();
    assert_eq!(differ(&new, &old), 0, "the depth-Equal pipeline drew a different picture from the old one");
    assert_eq!(differ(&new, &cleared), 0, "the depth-Equal pipeline drew a different picture from a cleared depth");
    assert!(
        t_new < t_one * 1.15,
        "eighty cut-out cards took {:.2} ms against {:.2} for one (the old pipeline {:.2}, a cleared depth {:.2})",
        t_new * 1e3,
        t_one * 1e3,
        t_old * 1e3,
        t_cleared * 1e3
    );
}

/// **A frame the depth prepass primed keeps that depth in its colour pass.**
/// The colour pass cleared the depth whenever it cleared the colour, throwing
/// the priming away, so every hidden layer of a stack of surfaces (a meadow
/// of cut-out grass cards) was shaded, however many lay behind the nearest.
/// Eighty cubes one behind another, the farthest made first, lit by a sun with
/// contact shadows (which read the prepass): the pass keeps the primed depth, draws exactly the
/// picture it drew clearing it, and takes much less time.
#[cfg(feature = "editor-ui")]
#[test]
fn a_primed_depth_buffer_is_kept_and_hidden_layers_cost_nothing() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    let sun = ed.world.spawn();
    ed.world.insert(sun, Transform { rotation: floptle_core::math::Quat::from_rotation_x(-1.0), ..Transform::IDENTITY });
    // Contact shadows read the prepass, so the frame runs one.
    ed.world.insert(sun, floptle_core::Light { intensity: 1.0, contact_shadows: true, ..Default::default() });
    for k in (0..80).rev() {
        let b = ed.world.spawn();
        ed.world.insert(
            b,
            Transform {
                translation: DVec3::new(0.0, 0.0, -4.0 - k as f64 * 0.125),
                scale: floptle_core::math::Vec3::new(40.0, 40.0, 0.1),
                ..Transform::IDENTITY
            },
        );
        ed.world.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.2 + 0.01 * k as f32, 0.5, 0.3] });
    }
    ed.camera.position = DVec3::ZERO;
    ed.camera.pitch = 0.0;
    ed.camera.yaw = 0.0;
    let (w, h) = (1600u32, 1200u32);
    let cam = ed.camera.render_camera();
    let shoot = |ed: &mut crate::Editor, keep: bool, frames: u32| {
        ed.raster.as_mut().unwrap().set_keep_primed_depth(keep);
        let mut px = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..frames {
            px = crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame");
        }
        (px, t.elapsed().as_secs_f64() / frames as f64, ed.raster.as_ref().unwrap().last_pass_kept_primed_depth())
    };
    // Warm both paths up, then measure.
    let _ = shoot(&mut ed, true, 2);
    let _ = shoot(&mut ed, false, 2);
    let (kept, t_kept, did_keep) = shoot(&mut ed, true, 8);
    let (cleared, t_cleared, _) = shoot(&mut ed, false, 8);
    assert!(did_keep, "the colour pass cleared the depth the prepass primed");
    let differ = kept.iter().zip(&cleared).filter(|(a, b)| a.abs_diff(**b) > 1).count();
    assert_eq!(differ, 0, "keeping the primed depth changed {differ} channel values of the picture");
    assert!(t_kept < t_cleared * 0.8, "keeping the primed depth took {:.2} ms against {:.2} clearing it", t_kept * 1e3, t_cleared * 1e3);
}

/// **The sun's shadow map draws only what lies in its box.** The camera's
/// list reaches as far as it sees, and the map's box only to the shadow
/// distance: the far blocks in view are given to the map and left out, and
/// the poles round the camera and three long beams whose middles lie far
/// outside the box but whose shadows cross the view are all drawn. The
/// picture is the same, byte for byte, as a map drawn from every opaque
/// instance the frame gave it, and `perf.counts()` reports both numbers.
#[cfg(feature = "editor-ui")]
#[test]
fn sun_map_draws_stop_at_the_shadow_distance() {
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    ed.project.retro = false;
    // Counts publish with collection on, as a game's `perf.counts()` reads them.
    ed.script_host.profile().borrow_mut().enable(true);
    let wd = &mut ed.world;
    let sun = wd.spawn();
    wd.insert(sun, Transform { rotation: Quat::from_rotation_x(-0.5) * Quat::from_rotation_y(0.7), ..Transform::IDENTITY });
    wd.insert(
        sun,
        floptle_core::Light { intensity: 1.0, shadows: true, shadow_map: true, shadow_distance: 40.0, ..Default::default() },
    );
    let floor = wd.spawn();
    wd.insert(floor, Transform { translation: DVec3::new(0.0, -0.5, 0.0), scale: floptle_core::math::Vec3::new(400.0, 1.0, 400.0), ..Transform::IDENTITY });
    wd.insert(floor, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.7, 0.7, 0.65] });
    // Poles from beside the camera out past the box's reach, across the
    // view and to either side of it.
    let near = 48;
    for k in 0..near {
        let (a, d) = ((k % 8) as f64 * 0.35 - 1.2, 6.0 + (k / 8) as f64 * 11.0);
        let p = wd.spawn();
        wd.insert(
            p,
            Transform {
                translation: DVec3::new(a.sin() * d, 6.0, -a.cos() * d),
                scale: floptle_core::math::Vec3::new(0.6, 12.0, 0.6),
                ..Transform::IDENTITY
            },
        );
        wd.insert(p, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.5, 0.4, 0.3] });
    }
    // Long beams whose middles lie far outside the box while they reach
    // across the view: only their length puts them in the map, and their
    // shadows cross the ground in front of the camera.
    for k in 0..3 {
        let beam = wd.spawn();
        wd.insert(
            beam,
            Transform {
                translation: DVec3::new(70.0, 3.0, -8.0 - k as f64 * 6.0),
                scale: floptle_core::math::Vec3::new(150.0, 0.5, 0.5),
                ..Transform::IDENTITY
            },
        );
        wd.insert(beam, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.4, 0.4, 0.45] });
    }
    for k in 0..30 {
        let a = k as f64 * 0.21 - 3.0;
        let b = wd.spawn();
        wd.insert(b, Transform { translation: DVec3::new(a.sin() * 700.0, 4.0, -a.cos() * 700.0), scale: floptle_core::math::Vec3::splat(8.0), ..Transform::IDENTITY });
        wd.insert(b, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.3, 0.3, 0.35] });
    }
    let cam = RenderCamera::new(
        DVec3::new(0.0, 3.0, 0.0),
        Quat::from_rotation_x(-0.25),
        Projection::Perspective { fov_y: 1.1, near: 0.05, far: 2000.0 },
    );
    let shoot = |ed: &mut crate::Editor, cull: bool| {
        ed.raster.as_mut().unwrap().set_sun_map_cull(cull);
        let _ = crate::shot::render_frame_pixels(ed, &cam, None, 1280, 720, !0, false).expect("a frame");
        let px = crate::shot::render_frame_pixels(ed, &cam, None, 1280, 720, !0, false).expect("a frame");
        (px, ed.raster.as_ref().unwrap().sun_map_draws())
    };
    let (all, (drawn_all, given)) = shoot(&mut ed, false);
    let (culled, (drawn, given2)) = shoot(&mut ed, true);
    assert_eq!(given, given2);
    assert_eq!(drawn_all, given, "the map without the cull left something out");
    // Eight of the far blocks are in view, so the camera's list holds them
    // (measured: 57 given, 47 drawn).
    assert!(given - drawn >= 8, "the sun map drew {drawn} of {given} instances: the far blocks in view were drawn into it");
    assert!(drawn >= near / 2, "the sun map drew only {drawn} of {given}: the poles round the camera went missing");
    // What a game reads in `perf.counts()`: the same two numbers, and the
    // colour pass's triangles, twelve a cube.
    let c = ed.script_host.profile().borrow().counts();
    assert_eq!((c.sun_map_draws, c.sun_map_culled), (drawn, given - drawn), "perf.counts says otherwise");
    assert!(c.triangles >= 12 * near as u64 / 2 && c.triangles % 12 == 0, "perf.counts triangles {} for a frame of cubes", c.triangles);
    let differ = all.iter().zip(&culled).filter(|(a, b)| a != b).count();
    assert_eq!(differ, 0, "leaving out what lies outside the sun's box changed {differ} channel values of the picture");
}

/// **…and does so with raymarched matter in the frame.** The same eighty
/// cards with a blob in front of part of them: the blob is marched between
/// the depth prepass and the colour pass, which then loads the colour
/// rather than clearing it. The cards still draw through the depth-Equal
/// pipeline (the march writes depth only where its matter is in front, so
/// the prepass depth stands under every card it did not cover), and the
/// picture is the same as the ordinary test's.
///
/// What it costs is `tests/cutout_cards_cost.rs`'s to measure, by the GPU's
/// clock: whole frames here are mostly the readback (24 ms either way).
/// The offscreen path marching over a cleared depth, which left every hidden
/// card to be shaded, showed even through that: 31.9 ms.
#[cfg(feature = "editor-ui")]
#[test]
fn cutout_cards_shade_once_under_raymarched_matter() {
    let Some((mut ed, _cards, dir)) = cutout_card_stack("marched") else { return };
    let blob = ed.world.spawn();
    ed.world.insert(blob, Transform { translation: DVec3::new(-1.5, 0.0, -3.0), ..Transform::IDENTITY });
    ed.world.insert(blob, Matter::Blob { scale: 1.2 });
    let (w, h) = (1600u32, 1200u32);
    let cam = ed.camera.render_camera();
    let shoot = |ed: &mut crate::Editor, equal: bool, frames: u32| {
        ed.raster.as_mut().unwrap().set_use_primed_pipeline(equal);
        let mut px = Vec::new();
        let t = std::time::Instant::now();
        for _ in 0..frames {
            px = crate::shot::render_frame_pixels(ed, &cam, None, w, h, !0, false).expect("a frame");
        }
        let r = ed.raster.as_ref().unwrap();
        (px, t.elapsed().as_secs_f64() / frames as f64, (r.last_pass_kept_primed_depth(), r.last_pass_equal_buckets()))
    };
    let _ = shoot(&mut ed, true, 2);
    let _ = shoot(&mut ed, false, 2);
    let (new, t_new, (owned, equal)) = shoot(&mut ed, true, 8);
    let (old, t_old, _) = shoot(&mut ed, false, 8);
    let _ = std::fs::remove_dir_all(&dir);
    if let Ok(out) = std::env::var("FLOPTLE_CUTSTACK_PNG") {
        let tex = floptle_render::TextureData { pixels: new.clone(), width: w, height: h };
        std::fs::write(std::path::Path::new(&out).join("cutstack_marched.png"), floptle_assets::encode_png(&tex).unwrap()).unwrap();
    }
    let _ = (t_new, t_old);
    assert!(!owned, "the fixture: no raymarch pass drew between the prepass and the colour pass");
    assert!(equal > 0, "under raymarched matter no card bucket drew depth-Equal");
    let differ = new.iter().zip(&old).filter(|(x, y)| x.abs_diff(**y) > 1).count();
    assert_eq!(differ, 0, "under raymarched matter the depth-Equal pipeline drew a different picture from the old one");
}

/// **Holding the sun moves the light, in a picture.** A floor under a star 5°
/// above the horizon is lit at a graze; with the Lighting node's key light
/// held straight overhead the same floor is lit square on, and is brighter by
/// the cosine the shader applies. Releasing the hold gives back the graze.
#[test]
fn a_held_key_light_lights_the_floor_from_where_it_is_held() {
    use floptle_core::math::Vec3;
    let shoot = |held: bool| -> Option<[f32; 3]> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
        let w = &mut ed.world;
        let floor = w.spawn();
        w.insert(floor, Transform { translation: DVec3::new(0.0, -1.0, 0.0), scale: Vec3::new(200.0, 1.0, 200.0), ..Transform::IDENTITY });
        w.insert(floor, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.6, 0.6, 0.6] });
        let (s, c) = 5f64.to_radians().sin_cos();
        let star = w.spawn();
        w.insert(star, Transform { translation: DVec3::new(c, s, 0.0) * 5.0e6, ..Transform::IDENTITY });
        w.insert(star, CelestialBody { body_radius: 1000.0, luminosity: 2.0e7, star_color: [1.0, 1.0, 1.0], ..Default::default() });
        let lighting = w.spawn();
        w.insert(
            lighting,
            floptle_core::Light { stars: true, key_override: held, direction: [0.0, 1.0, 0.0], ambient: [0.0; 3], ..Default::default() },
        );
        // Looking down at the floor.
        let rot = Quat::from_rotation_x(-1.2);
        let px = pixel_from(&mut ed, DVec3::new(0.0, 4.0, 2.0), rot, 0);
        Some(px)
    };
    let Some(graze) = shoot(false) else { return };
    let Some(held) = shoot(true) else { return };
    let luma = |c: [f32; 3]| (c[0] + c[1] + c[2]) / 3.0;
    assert!(luma(held) > luma(graze) + 0.15, "held overhead the floor reads {held:?}, at a 5° graze {graze:?}");
}
