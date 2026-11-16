//! **`skyOcclusion` read from the sky cache.**
//!
//! The beach of `sky_occlusion_by_a_beach.rs`, crowded the way a game fills
//! its proxy slots (`crowd`): crew, props, a dock, a hut and boats spread
//! over the sand and the water. Nearly every sea pixel is near a proxy, so
//! the op's early outs settle little and most of the sea marches its four
//! rays.

mod beach;

use beach::*;
use floptle_core::math::DVec3;

/// What the op costs on the crowded beach today, over the plain shader,
/// beside six field reads: printed, for the record and for comparing
/// against the cache.
#[test]
fn a_crowded_beach_marches() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    if beach.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye, _) = beach.stage_with(view, Layout::Crowd, DVec3::ZERO);
        let plain = beach.shader("1.0");
        let six = beach.shader(SIX_READS);
        let r24 = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
        let r8 = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 8)");
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, r24, r8, plain, six, r24, r8]);
        let base = t[0].min(t[4]);
        let over = |k: usize| t[k].min(t[k + 4]) - base;
        println!("{view:?} crowd: plain {base:.3} ms; over it, six reads {:.3}, reach 24 {:.3}, reach 8 {:.3}", over(1), over(2), over(3));
    }
}

/// What the cache holds for the cell round `p` against the cone at that
/// cell's centre, for the lobe `n` points along: the difference times
/// four (under 0.5), or 0.6 where the cell's centre is in solid matter
/// or the cell cannot answer for that lobe,
/// 0.8 where the cell is not filled, 1 outside every level.
const FILL_CHECK: &str = r#"
fn sky_fill_check(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    for (var l = 0u; l < SKYC_LEVELS; l = l + 1u) {
        let cell = skyc_f(SKYC_LEVEL + l * SKYC_LEVEL_WORDS + 3u);
        let u = floor((p - skyc_corner(l)) / cell);
        if (any(u < vec3<f32>(0.0)) || any(u > vec3<f32>(f32(SKYC_DIMS) - 1.0))) {
            continue;
        }
        let s = (vec3<u32>(u) + skyc_mod(l)) % vec3<u32>(SKYC_DIMS);
        let c = skyc_cell(l, s);
        let flags = c.y >> 24u;
        if ((flags & 0xFDu) != skyc_tag(l, s)) {
            return 0.8;
        }
        if ((flags & 2u) == 0u) {
            return 0.6;
        }
        let centre = skyc_corner(l) + (u + vec3<f32>(0.5)) * cell;
        let a = select(select(2u, 1u, abs(n.y) > abs(n.z)), 0u, abs(n.x) > max(abs(n.y), abs(n.z)));
        let idx = a * 2u + select(0u, 1u, n[a] < 0.0);
        let word = select(c.y, c.x, idx < 4u);
        let byte = (word >> (8u * (idx % 4u))) & 255u;
        if (byte == SKYC_UNSURE) {
            return 0.6;
        }
        let got = f32(byte) / 254.0;
        let want = sky_cone(centre, n, reach, skyc_static(skyc_word(SKYC_SETTLED)));
        return min(abs(got - want) * 4.0, 0.49);
    }
    return 1.0;
}
"#;

/// After the cache settles, every cell round the sea holds the cone the op
/// marches, at the cell's centre and along each axis, to within its 8-bit
/// rounding: the fill pass, the scrolled storage and the tags all agree
/// with where the cell is.
#[test]
fn the_fill_holds_the_cone_at_every_cell_centre() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    // The second step moves the camera 30 units forward, so most of what
    // it sees is read from cells a scroll brought in, stored where cells
    // from behind the camera were.
    for (view, shift) in [(View::EyeLevel, 0.0), (View::EyeLevel, 30.0), (View::Above, 0.0)] {
        let (globals, eye, rm) = beach.stage_with(view, Layout::Empty, DVec3::new(shift, 0.0, 0.0));
        beach.settle(rm, eye);
        for n in ["vec3(0, 1, 0)", "vec3(1, 0, 0)", "vec3(0, 0, -1)"] {
            let call = format!("skyOcclusion(worldPos, {n}, 24)");
            let bind = beach.shader_with(&call, Some(("sky_occlusion(", "sky_fill_check(")), FILL_CHECK);
            beach.draw(globals, eye, bind);
            let (mut sea, mut held, mut solid, mut unfilled, mut outside, mut worst) = (0usize, 0usize, 0usize, 0usize, 0usize, 0.0f32);
            for v in beach.read().into_iter().flatten() {
                sea += 1;
                if v < 0.5 {
                    held += 1;
                    worst = worst.max(v / 4.0);
                } else if v < 0.7 {
                    solid += 1;
                } else if v < 0.9 {
                    unfilled += 1;
                } else {
                    outside += 1;
                }
            }
            println!("{view:?} +{shift} n {n}: {sea} sea px; {held} held, {solid} in solid, {unfilled} unfilled, {outside} outside; worst {worst:.4}");
            assert_eq!(unfilled, 0, "{view:?} n {n}: {unfilled} of {sea} pixels read an unfilled cell");
            // Cells by the beach cannot answer for the lobes facing it
            // (`skyc_fill`), and say so: the rest hold the cone.
            assert!(held + solid > sea * 9 / 10 && held > sea / 2, "{view:?} n {n}: only {held} of {sea} pixels read a filled cell");
            assert!(worst <= 0.006, "{view:?} n {n}: a cell differs from the cone at its centre by {worst:.4}");
        }
    }
}

/// 1 where the op is answered by the cache, 0 where it would march.
const SERVED: &str = r#"
fn sky_served(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    if (skyc_word(SKYC_ON) == 0u || abs(reach - skyc_f(SKYC_REACH)) >= 0.5) {
        return 0.0;
    }
    return select(0.0, 1.0, sky_cached(p, n).y > 0.0);
}
"#;

/// How the op through the cache compares with the full march over the
/// sea: (sea pixels, mean difference, the 95th and 99.9th percentile
/// difference, the worst, the share of the sea the cache served).
pub fn against_the_march(beach: &mut Beach, globals: floptle_render::Globals, eye: DVec3, call: &str, png: Option<&str>) -> (usize, f32, f32, f32, f32, f32) {
    let fast = beach.shader_with(call, Some(("sky_occlusion(", "sky_occlusion(")), "");
    beach.draw(globals, eye, fast);
    let a = beach.read();
    let full = beach.shader_with(call, Some(("sky_occlusion(", "sky_full(")), FULL_MARCH);
    beach.draw(globals, eye, full);
    let b = beach.read();
    let served = beach.shader_with(call, Some(("sky_occlusion(", "sky_served(")), SERVED);
    beach.draw(globals, eye, served);
    let c = beach.read();
    let mut d: Vec<f32> = Vec::new();
    let mut hit = 0usize;
    for ((x, y), s) in a.iter().zip(&b).zip(&c) {
        if let (Some(x), Some(y), Some(s)) = (x, y, s) {
            d.push((x - y).abs());
            if *s > 0.5 {
                hit += 1;
            }
        }
    }
    if let Some(path) = png {
        // Cache, march and difference side by side, for looking at.
        let (w, h) = (W as usize, H as usize);
        let mut img = vec![0u8; w * 3 * h];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let v = |p: &Option<f32>| p.map_or(0, |v| (v.clamp(0.0, 1.0) * 255.0) as u8);
                img[y * w * 3 + x] = v(&a[i]);
                img[y * w * 3 + w + x] = v(&b[i]);
                img[y * w * 3 + 2 * w + x] = match (a[i], b[i]) {
                    (Some(p), Some(q)) => ((p - q).abs() * 4.0).min(1.0).mul_add(255.0, 0.0) as u8,
                    _ => 0,
                };
            }
        }
        let f = std::fs::File::create(path).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), (w * 3) as u32, h as u32);
        e.set_color(png::ColorType::Grayscale);
        e.write_header().unwrap().write_image_data(&img).unwrap();
    }
    let n = d.len();
    let mean = d.iter().sum::<f32>() / n.max(1) as f32;
    d.sort_by(f32::total_cmp);
    let pct = |q: f64| d[((n as f64 - 1.0) * q) as usize];
    (n, mean, pct(0.95), pct(0.999), d[n - 1], hit as f32 / n.max(1) as f32)
}

/// The op through the cache reads what the four rays read, over the still
/// world (the beach with no proxies): the cache's cells are the cone at
/// their centres, read between eight of them, so cover's edges are softer
/// than four rays' but the cover is the same.
#[test]
fn the_cache_reads_what_the_march_reads() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let png = std::env::var("SKY_CACHE_PNG").ok();
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye, rm) = beach.stage_with(view, Layout::Empty, DVec3::ZERO);
        beach.settle(rm, eye);
        for n in ["vec3(0, 1, 0)", "n"] {
            let call = format!("skyOcclusion(worldPos, {n}, 24)");
            let path = png.as_ref().map(|d| format!("{d}/cache-{view:?}-{}.png", if n == "n" { "wavy" } else { "up" }));
            let (sea, mean, p95, p999, worst, served) = against_the_march(&mut beach, globals, eye, &call, path.as_deref());
            println!("{view:?} {call}: {sea} sea px; served {:.1}%; difference mean {mean:.4}, 95% {p95:.3}, 99.9% {p999:.3}, worst {worst:.3}", served * 100.0);
            hold(&format!("{view:?} {call}"), served, mean, p95, p999, worst);
        }
    }
}

/// The bars the cache holds against the four rays: nearly all the sea
/// served, a small mean difference, 95% of pixels within 0.1 and none
/// over 0.6. Where four hard rays step from open to covered, eight cells
/// read between blur the step, which is all the bigger differences are.
fn hold(what: &str, served: f32, mean: f32, p95: f32, p999: f32, worst: f32) {
    assert!(served >= 0.9, "{what}: the cache served only {:.1}% of the sea", served * 100.0);
    assert!(
        mean <= 0.03 && p95 <= 0.1 && worst <= 0.6,
        "{what}: the cache differs from the march by {mean:.4} on average, {p95:.3} at the 95th percentile, {p999:.3} at the 99.9th and {worst:.3} at worst"
    );
}

/// Under a plank thinner than a cell, 0.6 over the water: the cells round
/// a point under it straddle it, the upper ones open to the sky. They must
/// not let the light under.
#[test]
fn a_thin_plank_keeps_the_water_under_it_covered() {
    let Some(mut beach) = Beach::new(pier()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(8.0));
    let png = std::env::var("SKY_CACHE_PNG").ok();
    let (globals, eye, rm) = beach.stage_with(View::Knolls, Layout::Empty, DVec3::ZERO);
    beach.settle(rm, eye);
    let call = "skyOcclusion(worldPos, vec3(0, 1, 0), 8)";
    let path = png.as_ref().map(|d| format!("{d}/cache-pier.png"));
    let (sea, mean, p95, p999, worst, served) = against_the_march(&mut beach, globals, eye, call, path.as_deref());
    println!("pier {call}: {sea} sea px; served {:.1}%; difference mean {mean:.4}, 95% {p95:.3}, 99.9% {p999:.3}, worst {worst:.3}", served * 100.0);
    // A six-unit plank is a few cells across: its edges blur over a cell
    // either side, more of the frame than the beach's cover is. What must
    // hold is the middle: where the rays read the water covered, so does
    // the cache, and where they read it open, so does the cache.
    let (fast, full) = beach.both(globals, eye, call);
    let (mut covered, mut leaks, mut open, mut shut) = (0usize, 0usize, 0usize, 0usize);
    for (a, b) in fast.iter().zip(&full) {
        if let (Some(a), Some(b)) = (a, b) {
            if *b < 0.1 {
                covered += 1;
                if *a > 0.3 {
                    leaks += 1;
                }
            } else if *b > 0.95 {
                open += 1;
                if *a < 0.7 {
                    shut += 1;
                }
            }
        }
    }
    println!("pier: {covered} px covered, {leaks} of them read lit; {open} open, {shut} of them read covered");
    assert!(served >= 0.9 && mean <= 0.05, "pier: served {served}, mean difference {mean}");
    assert!(covered > sea / 50, "the fixture: only {covered} of {sea} pixels are under the plank");
    assert!(leaks <= covered / 20, "light leaks under the plank: {leaks} of {covered} covered pixels read over 0.3");
    assert!(shut <= open / 20, "{shut} of {open} open pixels read under 0.7");
}
