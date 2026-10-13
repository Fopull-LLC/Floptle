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
