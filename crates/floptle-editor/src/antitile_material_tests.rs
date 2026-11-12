//! A tiled material can hide its repeat (card 0626, part 2).
//!
//! `Tiling::Uv { antitile }` lays the tile in hexagons, each at its own random
//! offset and turn. Two things must hold: the picture's period goes, and the
//! surface maps follow the colour. The second is the easy one to get subtly
//! wrong. A normal map stores its tilts in the texture's own axes, so a
//! hexagon that shows the texture turned must turn each tilt by the same
//! amount, or every hexagon's bumps catch the light from a different side.

use floptle_core::math::{DVec3, Quat};
use floptle_core::{Matter, Tiling, Transform};
use floptle_render::{Projection, RenderCamera};

const W: u32 = 256;

/// The fixture's textures: light noise with one dark disc per tile, a flat
/// white, and a normal map of round bumps.
fn write_textures(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("textures")).unwrap();
    let mut seed = 0x2545_f491u32;
    let mut dots = image::RgbaImage::new(32, 32);
    for (x, y, p) in dots.enumerate_pixels_mut() {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let (dx, dy) = (x as f32 - 16.0, y as f32 - 16.0);
        let v = if dx * dx + dy * dy < 36.0 { 20 } else { 150 + (seed >> 24) as u8 / 4 };
        *p = image::Rgba([v, v, v, 255]);
    }
    dots.save(dir.join("textures/dots.png")).unwrap();
    image::RgbaImage::from_pixel(16, 16, image::Rgba([255, 255, 255, 255])).save(dir.join("textures/white.png")).unwrap();
    // White with a small dark dot at the tile's centre: marks where each bump is.
    let mut dot = image::RgbaImage::from_pixel(32, 32, image::Rgba([255, 255, 255, 255]));
    for (x, y, p) in dot.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
        if dx * dx + dy * dy < 6.0 {
            *p = image::Rgba([0, 0, 0, 255]);
        }
    }
    dot.save(dir.join("textures/dot.png")).unwrap();
    // One round bump per tile: the tilt points out from the tile's centre,
    // growing with the distance, flat outside a 12-texel radius. Round, so a
    // turned copy of it is the same bump.
    let mut bump = image::RgbaImage::new(32, 32);
    for (x, y, p) in bump.enumerate_pixels_mut() {
        let (dx, dy) = (x as f32 + 0.5 - 16.0, y as f32 + 0.5 - 16.0);
        let (tx, ty) = if dx * dx + dy * dy < 144.0 { (dx / 12.0 * 0.7, dy / 12.0 * 0.7) } else { (0.0, 0.0) };
        let tz = (1.0 - tx * tx - ty * ty).max(0.0).sqrt();
        let enc = |v: f32| ((v * 0.5 + 0.5) * 255.0).round() as u8;
        *p = image::Rgba([enc(tx), enc(ty), enc(tz), 255]);
    }
    bump.save(dir.join("textures/bump.png")).unwrap();
}

/// The 2 m panel of `imported_maps_tests`, head on, lit from the side, with
/// `mat` on its one material.
fn photograph(tag: &str, mat: floptle_core::Material) -> Option<Vec<u8>> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-antitile-mat-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    crate::imported_maps_tests::write_maps(&dir);
    crate::imported_maps_tests::write_gltf(&dir);
    write_textures(&dir);
    ed.project_root = dir.clone();
    assert!(ed.import_model("models/panel.gltf"), "the fixture model did not import");
    let panel = ed.world.spawn();
    ed.world.insert(panel, Transform::IDENTITY);
    ed.world.insert(panel, Matter::Mesh { asset_path: "models/panel.gltf".into() });
    let mut om = std::collections::BTreeMap::new();
    om.insert("Hull".to_string(), mat);
    ed.world.insert(panel, floptle_core::ObjectMaterials(om));
    let sun = ed.world.spawn();
    let turn = Quat::from_rotation_y(0.9) * Quat::from_rotation_x(-0.5);
    ed.world.insert(sun, Transform { rotation: turn, ..Transform::IDENTITY });
    ed.world.insert(sun, floptle_core::Light { intensity: 1.2, shadows: false, ..Default::default() });
    let cam = RenderCamera::new(DVec3::new(0.0, 0.0, 2.6), Quat::IDENTITY, Projection::of_camera(1.0, false, 6.0, 0.05, 1000.0));
    // Twice: the first frame uploads the material's textures.
    let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, W, W, !0, false)?;
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, W, W, !0, false)?;
    if let Ok(out) = std::env::var("FLOPTLE_ANTITILE_PNG") {
        let _ = image::RgbaImage::from_raw(W, W, px.clone()).map(|im| im.save(format!("{out}/antitile_mat_{tag}.png")));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(px)
}

fn tiled(texture: &str, antitile: f32) -> floptle_core::Material {
    floptle_core::Material {
        texture: Some(texture.into()),
        tiling: Some(Tiling::Uv { count: [8.0, 8.0], offset: [0.0, 0.0], rotation: 0.0, antitile }),
        ..Default::default()
    }
}

/// Luminance of the middle `n` x `n` of the shot: inside the panel.
fn lum(px: &[u8], n: u32) -> Vec<f32> {
    let o = (W - n) / 2;
    let mut v = Vec::with_capacity((n * n) as usize);
    for y in o..o + n {
        for x in o..o + n {
            let i = ((y * W + x) * 4) as usize;
            v.push(px[i] as f32 * 0.3 + px[i + 1] as f32 * 0.59 + px[i + 2] as f32 * 0.11);
        }
    }
    v
}

/// **The repeat goes**, in both tiling modes. Plain, a grid of dots
/// correlates strongly with itself one tile (about 22 px) across; hex-tiled,
/// that peak is gone. The triplanar panel tiles 8 times across its 2 m too
/// (it faces +Z, so it is the XY projection that shows).
#[test]
fn a_hex_tiled_material_hides_its_repeat() {
    let tri = |antitile: f32| floptle_core::Material {
        tiling: Some(Tiling::Triplanar { scale: 0.25, blend: 4.0, antitile }),
        ..tiled("textures/dots.png", antitile)
    };
    for (mode, plain_mat, hex_mat) in [
        ("uv", tiled("textures/dots.png", 0.0), tiled("textures/dots.png", 1.0)),
        ("triplanar", tri(0.0), tri(1.0)),
    ] {
        let Some(plain) = photograph(&format!("{mode}_plain"), plain_mat) else { return };
        let Some(hex) = photograph(&format!("{mode}_hex"), hex_mat) else { return };
        repeat_goes(mode, &plain, &hex);
    }
}

fn repeat_goes(mode: &str, plain: &[u8], hex: &[u8]) {
    const N: usize = 140;
    let repeat = |px: &[u8]| -> (f32, usize) {
        let l = lum(px, N as u32);
        let mean = l.iter().sum::<f32>() / l.len() as f32;
        let c: Vec<f32> = l.iter().map(|v| v - mean).collect();
        let var = c.iter().map(|v| v * v).sum::<f32>().max(1e-6);
        (8..60usize)
            .map(|d| {
                let mut s = 0.0;
                for y in 0..N {
                    for x in 0..N - d {
                        s += c[y * N + x] * c[y * N + x + d];
                    }
                }
                (s / var * N as f32 / (N - d) as f32, d)
            })
            .fold((f32::MIN, 0), |a, b| if b.0 > a.0 { b } else { a })
    };
    let (p, pd) = repeat(plain);
    let (x, xd) = repeat(hex);
    assert!(p > 0.5, "the fixture ({mode}): the plain tile barely repeats ({p:.2} at {pd} px), so this tests nothing");
    assert!(x < p * 0.5, "{mode}: hex tiling left the repeat: {x:.2} at {xd} px against {p:.2} at {pd} px plain");
}

/// **The normal map's light stays on the right side.** A round bump per tile,
/// lit from one side, with a dark dot on its centre in the base colour (the
/// colour and the normal map share their hexagons, so the dot marks the bump
/// in either picture). Each bump is bright toward the light and dark away
/// from it; its brightness-weighted offset from its dot points at the light.
/// Plain, every bump's arrow points the same way. A round bump turned is the
/// same bump, so hex-tiled, with each tilt turned along with its hexagon, the
/// arrows still agree. With the tilts left in the texture's axes, each
/// hexagon's bumps catch the light from their own angle and the arrows
/// scatter.
///
/// Measured as |sum of arrows| / sum of |arrows|: 1 when they all agree, near
/// 0 when they point every which way.
#[test]
fn a_hex_tiled_normal_map_is_lit_from_the_same_side() {
    let bumps = |antitile: f32| floptle_core::Material { normal_map: Some("textures/bump.png".into()), ..tiled("textures/dot.png", antitile) };
    let Some(plain) = photograph("bump_plain", bumps(0.0)) else { return };
    let Some(hex) = photograph("bump_hex", bumps(1.0)) else { return };
    const N: usize = 140;
    let agreement = |px: &[u8]| -> (f64, usize) {
        let l = lum(px, N as u32);
        // The dots: groups of dark pixels, found by flood fill.
        let mut seen = vec![false; N * N];
        let mut centres = Vec::new();
        for start in 0..N * N {
            if seen[start] || l[start] > 40.0 {
                continue;
            }
            let (mut stack, mut sum, mut n) = (vec![start], (0.0f64, 0.0f64), 0usize);
            seen[start] = true;
            while let Some(i) = stack.pop() {
                let (x, y) = (i % N, i / N);
                sum = (sum.0 + x as f64, sum.1 + y as f64);
                n += 1;
                for (nx, ny) in [(x.wrapping_sub(1), y), (x + 1, y), (x, y.wrapping_sub(1)), (x, y + 1)] {
                    if nx < N && ny < N && !seen[ny * N + nx] && l[ny * N + nx] <= 40.0 {
                        seen[ny * N + nx] = true;
                        stack.push(ny * N + nx);
                    }
                }
            }
            if n >= 2 {
                centres.push((sum.0 / n as f64, sum.1 / n as f64));
            }
        }
        // Each bump's arrow: brightness about its own mean, weighted by offset,
        // over the ring between the dot and the bump's edge.
        let (mut total, mut lengths, mut count) = ((0.0f64, 0.0f64), 0.0f64, 0usize);
        for &(cx, cy) in &centres {
            let ring: Vec<(f64, f64, f64)> = (-8i32..=8)
                .flat_map(|dy| (-8i32..=8).map(move |dx| (dx, dy)))
                .filter_map(|(dx, dy)| {
                    let r2 = (dx * dx + dy * dy) as f64;
                    let (x, y) = ((cx.round() as i32 + dx) as usize, (cy.round() as i32 + dy) as usize);
                    ((9.0..=49.0).contains(&r2) && x < N && y < N).then(|| (dx as f64, dy as f64, l[y * N + x] as f64))
                })
                .collect();
            if ring.len() < 100 {
                continue;
            }
            let mean = ring.iter().map(|r| r.2).sum::<f64>() / ring.len() as f64;
            let a = ring.iter().fold((0.0, 0.0), |a, r| (a.0 + r.0 * (r.2 - mean), a.1 + r.1 * (r.2 - mean)));
            total = (total.0 + a.0, total.1 + a.1);
            lengths += (a.0 * a.0 + a.1 * a.1).sqrt();
            count += 1;
        }
        ((total.0 * total.0 + total.1 * total.1).sqrt() / lengths.max(1e-6), count)
    };
    let ((p, pn), (h, hn)) = (agreement(&plain), agreement(&hex));
    assert!(pn >= 20 && hn >= 20, "the fixture: too few bumps found ({pn} plain, {hn} hex)");
    assert!(p > 0.8, "the fixture: one light should light every plain bump from one side (agreement {p:.2})");
    assert!(h > 0.8, "hex tiling lit each hexagon's bumps from its own side: agreement {h:.2} over {hn} bumps, {p:.2} plain");
}

/// A hex-tiled sprite in a flat scene, photographed from an orthographic
/// camera, with or without a 2D lamp over it.
fn photograph_2d(tag: &str, antitile: f32, lamp: bool) -> Option<Vec<u8>> {
    use floptle_core::math::Vec3;
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle-antitile-2d-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    write_textures(&dir);
    ed.project_root = dir.clone();
    let e = ed.world.spawn();
    ed.world.insert(e, Transform { scale: Vec3::new(20.0, 20.0, 1.0), ..Transform::IDENTITY });
    ed.world.insert(e, Matter::Sprite { ppu: 32.0, size: 1.0, cell: 0, flip_x: false, flip_y: false, pivot: [0.5, 0.5] });
    ed.world.insert(e, floptle_core::Material {
        tiling: Some(Tiling::Uv { count: [10.0, 10.0], offset: [0.0, 0.0], rotation: 0.0, antitile }),
        ..tiled("textures/dots.png", antitile)
    });
    // An active orthographic camera node is what makes the scene flat, so
    // the lamp (left on Auto) is a 2D lamp, lit through the 2D pass.
    let c = ed.world.spawn();
    ed.world.insert(c, Transform::from_translation(DVec3::new(0.0, 0.0, 10.0)));
    ed.world.insert(c, Matter::Camera {
        fov_y: 1.0,
        active: true,
        target: String::new(),
        cull_mask: u32::MAX,
        target_w: 0,
        target_h: 0,
        target_hz: 0.0,
        ortho: true,
        ortho_height: 24.0,
        stack: Vec::new(),
        draws: floptle_core::CameraDraws::default(),
        post_after: false,
        near: 0.05,
        far: 1000.0,
    });
    if lamp {
        let l = ed.world.spawn();
        ed.world.insert(l, Transform { translation: DVec3::new(0.0, 0.0, 1.0), ..Transform::IDENTITY });
        ed.world.insert(l, Matter::PointLight {
            color: [1.0, 1.0, 1.0],
            intensity: 1.5,
            range: 60.0,
            shadows: Default::default(),
            shape: Default::default(),
            spot_angle: floptle_core::OMNI_ANGLE,
            spot_softness: 0.25,
            fog: 1.0,
        });
    }
    let cam = RenderCamera::new(DVec3::new(0.0, 0.0, 10.0), Quat::IDENTITY, Projection::of_camera(1.0, true, 24.0, 0.05, 1000.0));
    let _ = ed.ensure_texture("textures/dots.png");
    let _ = crate::shot::render_frame_pixels(&mut ed, &cam, None, W, W, u32::MAX, false)?;
    let px = crate::shot::render_frame_pixels(&mut ed, &cam, None, W, W, u32::MAX, false)?;
    if let Ok(out) = std::env::var("FLOPTLE_ANTITILE_PNG") {
        let _ = image::RgbaImage::from_raw(W, W, px.clone()).map(|im| im.save(format!("{out}/antitile_2d_{tag}.png")));
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(px)
}

/// **A 2D lamp lights the picture that was drawn.** The 2D light pass reads
/// its own G-buffer of the scene's colours; the light it adds is that colour
/// times the lamp. If the G-buffer read the plain tile while the colour pass
/// drew hexagons, the light would lay the plain grid of dots over the hex
/// picture. So the light a lamp adds (lit minus unlit) must follow the hex
/// picture, not the plain one.
#[test]
fn a_2d_lamp_lights_the_hex_tiled_picture() {
    let Some(dark) = photograph_2d("dark", 1.0, false) else { return };
    let Some(lit) = photograph_2d("lit", 1.0, true) else { return };
    let Some(plain) = photograph_2d("plain", 0.0, false) else { return };
    const N: u32 = 140;
    // Each pixel less the mean of the 15 x 15 around it: the dots stay, the
    // lamp's broad pool of light (which matches neither picture) goes.
    let centred = |v: Vec<f32>| {
        let n = N as i32;
        (0..n * n)
            .map(|i| {
                let (x, y) = (i % n, i / n);
                let (mut s, mut c) = (0.0, 0.0);
                for yy in (y - 7).max(0)..=(y + 7).min(n - 1) {
                    for xx in (x - 7).max(0)..=(x + 7).min(n - 1) {
                        s += v[(yy * n + xx) as usize];
                        c += 1.0;
                    }
                }
                v[i as usize] - s / c
            })
            .collect::<Vec<f32>>()
    };
    let corr = |a: &[f32], b: &[f32]| {
        let ab: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        ab / (a.iter().map(|x| x * x).sum::<f32>() * b.iter().map(|x| x * x).sum::<f32>()).sqrt().max(1e-6)
    };
    let (ld, ll, lp) = (lum(&dark, N), lum(&lit, N), lum(&plain, N));
    let added = centred(ll.iter().zip(&ld).map(|(l, d)| l - d).collect());
    let gain = ll.iter().sum::<f32>() / ld.iter().sum::<f32>().max(1.0);
    assert!(gain > 1.2, "the fixture: the lamp should brighten the sprite (x{gain:.2})");
    let (with_hex, with_plain) = (corr(&added, &centred(ld)), corr(&added, &centred(lp)));
    assert!(with_hex > 0.6, "the lamp's light does not follow the hex picture: correlation {with_hex:.2} (plain {with_plain:.2})");
    assert!(with_hex > with_plain + 0.3, "the lamp lit the plain tile under the hex one: {with_hex:.2} hex, {with_plain:.2} plain");
}
