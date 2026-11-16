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
        let off = vec3<u32>(u);
        let c = skyc_cell(l, (off + skyc_mod(l)) % vec3<u32>(SKYC_DIMS));
        if (!skyc_filled(l, off, c)) {
            return 0.8;
        }
        if ((c.y & (2u << 24u)) == 0u) {
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

/// The crowded beach with every proxy held still long enough to be baked
/// in: the same bars against the four rays as the still terrain.
#[test]
fn a_crowded_beach_reads_from_the_cache_once_it_holds_still() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let png = std::env::var("SKY_CACHE_PNG").ok();
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye, rm) = beach.stage_with(view, Layout::Crowd, DVec3::ZERO);
        beach.settle_all(rm, eye);
        let st = beach.raymarch.sky_cache_stats(0).unwrap();
        assert_eq!(st.settled.count_ones(), PROXIES as u32, "{view:?}: {st:?}");
        assert_eq!(st.moving, 0, "{view:?}: {st:?}");
        let call = "skyOcclusion(worldPos, n, 24)";
        let path = png.as_ref().map(|d| format!("{d}/crowd-{view:?}.png"));
        let (sea, mean, p95, p999, worst, served) = against_the_march(&mut beach, globals, eye, call, path.as_deref());
        println!("{view:?} crowd {call}: {sea} sea px; served {:.1}%; difference mean {mean:.4}, 95% {p95:.3}, 99.9% {p999:.3}, worst {worst:.3}", served * 100.0);
        hold(&format!("{view:?} crowd"), served, mean, p95, p999, worst);
    }
}

/// A baked deck pushed four units along: the water where it now stands
/// darkens on the very next frame, from the short-range term for proxies
/// that move, and once its refill is done the cache agrees with the march
/// again, its old place open.
#[test]
fn a_proxy_that_moves_darkens_where_it_goes_at_once_and_leaves_its_old_place() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let (globals, eye, mut rm) = beach.stage_with(View::EyeLevel, Layout::Crowd, DVec3::ZERO);
    // A raised deck in the spare slot, sixteen units ahead of the camera
    // and well inside the moving proxies' mask grid (a dock out past it
    // casts no cover while it moves, by design), and higher over the water
    // than the op's rays lift off it over this coarse terrain (up to 2.3).
    let deck = (DVec3::new(6.0, 3.5, 2.0) - eye).as_vec3();
    rm.prox_a[30] = [deck.x, deck.y, deck.z, 0.0];
    rm.prox_b[30] = [2.0, 0.1, 2.0, 2.0];
    rm.prox_rot[30] = [0.0, 0.0, 0.0, 1.0];
    rm.prox_count[0] = 31.0;
    beach.raymarch.upload_globals(&beach.gpu, rm);
    beach.settle_all(rm, eye);
    // Pushed four units across the view.
    let mut moved = rm;
    moved.prox_a[30][2] += 4.0;
    let call = "skyOcclusion(worldPos, vec3(0, 1, 0), 24)";
    let (before, _) = beach.both(globals, eye, call);
    beach.raymarch.upload_globals(&beach.gpu, moved);
    beach.refresh(moved, eye);
    let st = beach.raymarch.sky_cache_stats(0).unwrap();
    assert_eq!(st.moving, 1 << 30, "{st:?}");
    let (now, full) = beach.both(globals, eye, call);
    // Where the march now reads the dock's cover, darker by over a
    // quarter than the cache read before it moved; on those, the cache
    // must already read darker by at least a tenth.
    let (mut fresh, mut followed) = (0usize, 0usize);
    for ((b, a), f) in before.iter().zip(&now).zip(&full) {
        if let (Some(b), Some(a), Some(f)) = (b, a, f)
            && b - f > 0.25
        {
            fresh += 1;
            if b - a > 0.1 {
                followed += 1;
            }
        }
    }
    let dark = |v: &[Option<f32>]| v.iter().flatten().filter(|x| **x < 0.5).count();
    let most = before.iter().zip(&full).filter_map(|(b, f)| Some(b.as_ref()? - f.as_ref()?)).fold(0.0f32, f32::max);
    println!("{fresh} px newly under the deck, {followed} of them darker on the first frame; dark before {}, now {}, march {}; most darkened {most:.3}", dark(&before), dark(&now), dark(&full));
    assert!(fresh > 200, "the fixture: only {fresh} pixels newly covered");
    assert!(followed * 4 >= fresh * 3, "only {followed} of {fresh} newly covered pixels darkened on the first frame");
    beach.settle_all(moved, eye);
    let (sea, mean, p95, p999, worst, served) = against_the_march(&mut beach, globals, eye, call, None);
    println!("after: {sea} sea px; served {:.1}%; difference mean {mean:.4}, 95% {p95:.3}, 99.9% {p999:.3}, worst {worst:.3}", served * 100.0);
    // The deck is a crisp caster a few cells across: at its shadow's edge
    // the four rays step by up to three quarters, which the cells blur, so
    // the worst pixel is not held here; the rest of the bars are.
    assert!(served >= 0.9 && mean <= 0.03 && p95 <= 0.1, "after the deck moved: served {served}, mean {mean}, 95% {p95}");
    // Where it stood is open again.
    let (after, full) = beach.both(globals, eye, call);
    let (mut left, mut open) = (0usize, 0usize);
    for ((b, a), f) in before.iter().zip(&after).zip(&full) {
        if let (Some(b), Some(a), Some(f)) = (b, a, f)
            && *b < 0.5 && *f > 0.9
        {
            left += 1;
            if *a > 0.8 {
                open += 1;
            }
        }
    }
    println!("{left} px the deck left, {open} of them open again");
    assert!(left > 200, "the fixture: the deck left only {left} pixels");
    assert!(open * 20 >= left * 19, "only {open} of the {left} pixels the deck left read open again");
}

/// GPU milliseconds a frame of the sea through `bind` takes while the
/// camera walks forward a quarter unit a frame from `view`, the cache
/// refreshed every frame (its fill included): the least of several rounds.
fn walking_cost(beach: &mut Beach, timer: &mut floptle_render::GpuTimer, view: View, bind: floptle_render::FlslBindingId, refresh: bool) -> f32 {
    const FRAMES: usize = 20;
    let mut best = f32::MAX;
    let mut k = 0usize;
    for _ in 0..8 {
        if !timer.begin() {
            continue;
        }
        timer.mark(&beach.gpu, "walk");
        for _ in 0..FRAMES {
            k += 1;
            let shift = DVec3::new(0.25 * (k % 120) as f64, 0.0, 0.0);
            let (g, eye, rm) = beach.stage_with(view, Layout::Crowd, shift);
            if refresh {
                beach.refresh(rm, eye);
            }
            beach.draw(g, eye, bind);
        }
        timer.mark_gap(&beach.gpu);
        timer.end(&beach.gpu);
        for _ in 0..200 {
            beach.gpu.device.poll(wgpu::PollType::wait_indefinitely()).ok();
            timer.poll();
            if timer.begin() {
                break;
            }
        }
        if let Some(s) = timer.spans().iter().find(|s| s.label == "walk") {
            best = best.min(s.ms / FRAMES as f32);
        }
    }
    best
}

/// What `skyOcclusion(worldPos, up, 24)` costs on the crowded beach, over
/// the plain shader, beside six field reads: marched (the cache off),
/// read from the settled cache, while the camera walks (the fill's
/// refills counted in), and with every proxy moving (read through the
/// short-range term). Held as ratios to six reads, so a slower or busier
/// GPU moves both sides: the settled cache no more than two and a half
/// times six reads (it measures 1.2 to 2 times on an RTX 4060, where the
/// march costs 6 to 8 times),
/// and thirty moving proxies no more than half again what thirty baked
/// ones cost.
#[test]
fn a_crowded_beach_costs_a_few_field_reads_through_the_cache() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    if beach.gpu.adapter.get_info().device_type == wgpu::DeviceType::Cpu {
        return;
    }
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    let plain = beach.shader("1.0");
    let six = beach.shader(SIX_READS);
    let sky = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
    for view in [View::EyeLevel, View::Above] {
        beach.raymarch.set_sky_cache(&beach.gpu, None);
        let (globals, eye, rm) = beach.stage_with(view, Layout::Crowd, DVec3::ZERO);
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, sky, plain, six, sky]);
        let base = t[0].min(t[3]);
        let (reads, march) = (t[1].min(t[4]) - base, t[2].min(t[5]) - base);
        // Every proxy moving: each nudged two centimetres every frame, so
        // none ever holds still long enough to be baked in.
        beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
        for k in 0..160 {
            let mut jig = rm;
            for a in jig.prox_a.iter_mut().take(PROXIES) {
                a[1] += 0.02 * (k % 2) as f32;
            }
            beach.raymarch.upload_globals(&beach.gpu, jig);
            beach.refresh(jig, eye);
        }
        let st = beach.raymarch.sky_cache_stats(0).unwrap();
        assert_eq!(st.settled, 0, "{st:?}");
        assert_eq!(st.moving.count_ones(), PROXIES as u32, "{st:?}");
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, sky, plain, sky]);
        let moving = t[1].min(t[3]) - t[0].min(t[2]);
        beach.raymarch.upload_globals(&beach.gpu, rm);
        beach.settle_all(rm, eye);
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, sky, plain, six, sky]);
        let base2 = t[0].min(t[3]);
        let (reads2, steady) = (t[1].min(t[4]) - base2, t[2].min(t[5]) - base2);
        let walk_plain = walking_cost(&mut beach, &mut timer, view, plain, false);
        let walk = walking_cost(&mut beach, &mut timer, view, sky, true) - walk_plain;
        let fill = walking_cost(&mut beach, &mut timer, view, plain, true) - walk_plain;
        println!(
            "{view:?} crowd: plain {base:.3} ms; over it, six reads {reads:.3} / {reads2:.3}; skyOcclusion reach 24 marched {march:.3}, cache settled {steady:.3}, all proxies moving {moving:.3}, camera walking {walk:.3} (of which the fill {fill:.3})"
        );
        let r = reads.min(reads2).max(0.01);
        assert!(steady <= 2.5 * r, "{view:?}: through the settled cache skyOcclusion costs {steady:.3} ms, {:.1} times six reads ({r:.3} ms)", steady / r);
        assert!(moving <= 1.5 * steady.max(0.02), "{view:?}: with every proxy moving it costs {moving:.3} ms, {:.1} times the settled {steady:.3} ms", moving / steady.max(0.02));
    }
}

/// Where the op's time goes through the cache on the crowded beach: the
/// share of the sea it serves, and the cost of the cache's read alone and
/// of its finest level alone beside the op and six reads. A probe for
/// tuning, not a guard: `--ignored` to run it.
#[test]
#[ignore]
fn where_the_time_goes() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    let Some(mut timer) = floptle_render::GpuTimer::new(&beach.gpu) else { return };
    const ONLY: &str = r#"
fn sky_only(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    return sky_cached(p, n).x;
}
fn sky_lvl0(p: vec3<f32>, n: vec3<f32>, reach: f32) -> f32 {
    return skyc_level(0u, p, n).x;
}
"#;
    let plain = beach.shader("1.0");
    let six = beach.shader(SIX_READS);
    let sky = beach.shader("skyOcclusion(worldPos, vec3(0, 1, 0), 24)");
    let only = beach.shader_with("skyOcclusion(worldPos, vec3(0, 1, 0), 24)", Some(("sky_occlusion(", "sky_only(")), ONLY);
    let l0 = beach.shader_with("skyOcclusion(worldPos, vec3(0, 1, 0), 24)", Some(("sky_occlusion(", "sky_lvl0(")), ONLY);
    let served = beach.shader_with("skyOcclusion(worldPos, vec3(0, 1, 0), 24)", Some(("sky_occlusion(", "sky_served(")), SERVED);
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    for view in [View::EyeLevel, View::Above] {
        let (globals, eye, rm) = beach.stage_with(view, Layout::Crowd, DVec3::ZERO);
        beach.settle_all(rm, eye);
        beach.draw(globals, eye, served);
        let v = beach.read();
        let n = v.iter().flatten().count();
        let s = v.iter().flatten().filter(|x| **x > 0.5).count();
        let t = cost(&mut beach, &mut timer, globals, eye, &[plain, six, sky, only, l0, plain, six, sky, only, l0]);
        let b = t[0].min(t[5]);
        println!("{view:?}: served {:.1}%; six {:.3} op {:.3} cache-only {:.3} level0-only {:.3}", 100.0 * s as f32 / n as f32, t[1].min(t[6]) - b, t[2].min(t[7]) - b, t[3].min(t[8]) - b, t[4].min(t[9]) - b);
    }
}

/// Cells written so far: those with their filled bit, in a read-back buffer.
fn written(words: &[u32]) -> usize {
    use floptle_render::sky_cache::{SKY_CELLS, SKY_HEADER_WORDS};
    (0..SKY_CELLS as usize).filter(|i| (words[SKY_HEADER_WORDS + i * 4 + 1] >> 24) & 1 == 1).count()
}

/// From a cache starting from nothing, the GPU fills no more than the
/// budget's worth of cells in any frame (half again for the dither's
/// unevenness), fills every cell in the end, and once full, a camera
/// standing still writes nothing at all.
#[test]
fn the_fill_keeps_to_its_budget_and_a_still_camera_writes_nothing() {
    use floptle_render::sky_cache::{SKY_BUDGET, SKY_CELLS, SKY_HEADER_WORDS};
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let (_, eye, rm) = beach.stage_with(View::EyeLevel, Layout::Empty, DVec3::ZERO);
    let (mut last, mut most) = (0usize, 0usize);
    for _ in 0..200 {
        beach.refresh(rm, eye);
        let n = written(&beach.raymarch.read_sky_cache(&beach.gpu, 0).unwrap());
        most = most.max(n - last);
        last = n;
    }
    println!("most cells written in a frame {most} (budget {SKY_BUDGET}); {last} of {SKY_CELLS} written");
    assert_eq!(last, SKY_CELLS as usize, "not every cell was filled");
    assert!(most <= SKY_BUDGET as usize * 3 / 2, "a frame wrote {most} cells, over the budget of {SKY_BUDGET}");
    let before = beach.raymarch.read_sky_cache(&beach.gpu, 0).unwrap();
    for _ in 0..24 {
        beach.refresh(rm, eye);
    }
    let after = beach.raymarch.read_sky_cache(&beach.gpu, 0).unwrap();
    assert!(before[SKY_HEADER_WORDS..] == after[SKY_HEADER_WORDS..], "a still camera rewrote cells");
}

/// A walk of a hundred finest cells in steps of twenty-five, each under a
/// level's width, so no level starts again and the cells a scroll exposes
/// wait for their refill: every cell read as filled, after every step,
/// holds the cone at its own centre. A cell's storage is reused every 48
/// cells; one that only knew which wrap it was in would be read as filled
/// 96 cells on.
#[test]
fn a_cell_is_never_read_for_a_place_it_was_not_filled_for() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let (_, eye, rm) = beach.stage_with(View::EyeLevel, Layout::Empty, DVec3::new(-60.0, 0.0, 0.0));
    beach.settle(rm, eye);
    for step in 1..=4 {
        let shift = DVec3::new(-60.0 + 25.0 * step as f64, 0.0, 0.0);
        let (globals, eye, rm) = beach.stage_with(View::EyeLevel, Layout::Empty, shift);
        beach.refresh(rm, eye);
        let bind = beach.shader_with("skyOcclusion(worldPos, vec3(0, 1, 0), 24)", Some(("sky_occlusion(", "sky_fill_check(")), FILL_CHECK);
        beach.draw(globals, eye, bind);
        let (mut held, mut worst) = (0usize, 0.0f32);
        for v in beach.read().into_iter().flatten() {
            if v < 0.5 {
                held += 1;
                worst = worst.max(v / 4.0);
            }
        }
        println!("step {step}: {held} px read a filled cell; worst {worst:.4}");
        assert!(held > 1000, "step {step}: the fixture reads almost no filled cells");
        assert!(worst <= 0.006, "step {step}: a cell read as filled differs from the cone at its centre by {worst:.4}");
    }
}

/// One view keeps the cache. Another view refreshing binds none, so it
/// marches and reads the march's answer, never the first view's cells;
/// once the first view has stopped refreshing for long enough, the other
/// takes the cache over.
#[test]
fn a_second_view_marches_until_the_first_has_gone() {
    let Some(mut beach) = Beach::new(terrain()) else { return };
    beach.raymarch.set_sky_cache(&beach.gpu, Some(24.0));
    let (_, eye0, rm0) = beach.stage_with(View::EyeLevel, Layout::Empty, DVec3::ZERO);
    beach.settle(rm0, eye0);
    let call = "skyOcclusion(worldPos, vec3(0, 1, 0), 24)";
    let served_by = |beach: &mut Beach, globals, eye| {
        let s = beach.shader_with(call, Some(("sky_occlusion(", "sky_served(")), SERVED);
        beach.draw(globals, eye, s);
        let v = beach.read();
        v.iter().flatten().filter(|x| **x > 0.5).count() as f32 / v.iter().flatten().count().max(1) as f32
    };
    let (g1, eye1, rm1) = beach.stage_with(View::Above, Layout::Empty, DVec3::ZERO);
    beach.refresh_view(rm1, eye1, 1);
    let served = served_by(&mut beach, g1, eye1);
    let (fast, full) = beach.both(g1, eye1, call);
    let worst = fast.iter().zip(&full).filter_map(|(a, b)| Some((a.as_ref()? - b.as_ref()?).abs())).fold(0.0f32, f32::max);
    println!("second view: served {:.1}%, worst difference from the march {worst:.3}", served * 100.0);
    assert_eq!(served, 0.0, "the second view read the first view's cache");
    assert!(worst <= 0.15, "the second view's op differs from the march by {worst:.3}");
    for _ in 0..floptle_render::SKY_EVICT + 1 {
        beach.refresh_view(rm1, eye1, 1);
    }
    for _ in 0..200 {
        beach.refresh_view(rm1, eye1, 1);
    }
    let served = served_by(&mut beach, g1, eye1);
    println!("second view after taking over: served {:.1}%", served * 100.0);
    assert!(served >= 0.9, "the second view never took the cache over: served {:.1}%", served * 100.0);
}
