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
/// of the ground taking the shared march (the rest are the blocks' edges,
/// where no texel beside a pixel is at its depth). The fog is thick enough that
/// marching it is most of what every pixel shows.
#[test]
fn volumetric_fog_marched_once_per_four_pixels_draws_the_same_picture() {
    use floptle_core::math::Vec3;
    // `None` with no GPU; `pass` marches the fog once for the view, `debug`
    // paints what reads it magenta.
    let shot = |pass: bool, debug: bool| -> Option<Vec<u8>> {
        let mut ed = crate::offscreen::test_editor_with_gpu()?;
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
