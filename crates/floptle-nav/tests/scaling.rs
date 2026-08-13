//! Complexity guards for the bake.
//!
//! A navmesh is baked over a level's whole footprint, so anything that costs a
//! scan per cell costs a scan per square metre and the bill arrives as the level
//! gets big — long after the code was written against a test room. That is
//! exactly the shape of the three quadratics that reached players before
//! (`crates/floptle-core/tests/scaling.rs` has the history), and this crate
//! started life with two of the same kind: a `Vec` allocated per neighbour
//! query, and a list per column — 2.9 million of them on a 256 m level. Neither
//! was quadratic, but together they were 70% of the bake.
//!
//! These assert a **growth ratio**, never a duration, for the reason that file
//! sets out at length: a millisecond threshold on a shared runner is a coin
//! flip, whereas four times the input costing four times the time is a property
//! of the algorithm and cancels out how fast the machine is.
//!
//! Sizes are kept small and the cell coarse deliberately — this runs in the
//! ordinary debug test suite, so it has to prove a property, not benchmark
//! anything. `cargo run --release -p floptle-nav --example nav_probe` is where
//! absolute bake times are measured.

use std::time::{Duration, Instant};

use floptle_nav::{bake, NavSettings, Tri};

/// Time `f(n)` at `n` and `4n` and hand back the growth ratio.
///
/// Best of several runs, and the two sizes **interleaved** — both taken from
/// `floptle-core`'s harness, where alternating them turned out to be
/// load-bearing: run all the small ones first and they all happen on a cold core
/// at base clock, which flatters the large ones and made a deliberately
/// quadratic loop measure 7.4x instead of 15.7x.
/// `None` when the work was too short to time, which is not a pass — see
/// [`assert_linearish`].
fn growth(n: usize, mut f: impl FnMut(usize)) -> Option<f64> {
    let time = |size: usize, f: &mut dyn FnMut(usize)| {
        let t = Instant::now();
        f(size);
        t.elapsed()
    };
    f(n);
    f(n * 4);
    let (mut small, mut large) = (Duration::MAX, Duration::MAX);
    for _ in 0..3 {
        small = small.min(time(n, &mut f));
        large = large.min(time(n * 4, &mut f));
    }
    // Work too short to time says nothing about its own complexity. The
    // original harness returned a passing 4.0 here, and this crate's pathing
    // guard hit it immediately and read as green while measuring nothing.
    if small.as_nanos() < 10_000 {
        return None;
    }
    Some(large.as_secs_f64() / small.as_secs_f64())
}

/// What linear and quadratic work actually measure like **right now**, on this
/// machine, under whatever else is running.
///
/// A fixed ceiling does not survive contention. `cargo test --workspace` runs
/// every crate's test binary at once, and under that load this file measured a
/// plain summing loop at **8.2x** — over the 8.0 bound that is supposed to mean
/// "you have gained a scan". Nothing was wrong with the code; the machine was
/// busy, and a busy machine slows the large side more than the small one.
///
/// So the guards below are judged against these two references instead, taken
/// in the same conditions moments earlier. Contention inflates all three
/// together and cancels, which is the same trick that makes a ratio beat a
/// duration in the first place — carried one step further.
struct Yardstick {
    linear: f64,
    quadratic: f64,
}

impl Yardstick {
    /// The line between "grew like its input" and "gained a scan": the
    /// geometric middle of the two references. On a quiet machine that lands at
    /// 8.0, which is exactly where the fixed bound was.
    fn ceiling(&self) -> f64 {
        (self.linear * self.quadratic).sqrt()
    }

    #[track_caller]
    fn assert_linearish(&self, what: &str, ratio: Option<f64>) {
        let Some(ratio) = ratio else {
            panic!(
                "{what}: the work was too short to time, so this guard proved nothing. \
                 Raise the size or the repeat count."
            );
        };
        assert!(
            ratio < self.ceiling(),
            "{what}: 4x the input cost {ratio:.1}x the time. Measured here and now, \
             linear work costs {:.1}x and quadratic work {:.1}x — so this is on the \
             wrong side of the line and has gained a scan it did not have.",
            self.linear,
            self.quadratic
        );
    }
}

/// Everything this file measures, in ONE test.
///
/// Not three, which is what it was. `cargo test` runs a file's tests on
/// separate threads, so three timing tests in one binary spend their whole
/// lives competing with each other for cores — and taking the best of several
/// runs, which is what makes a ratio robust, does not help when NO run is
/// uncontended. Measured back to back it reads 4.0x every time; measured
/// alongside its siblings it read 8.1x often enough to fail two runs in five.
///
/// A timing guard that fails when the machine is busy is one people learn to
/// re-run until it passes, which is worse than not having it.
#[test]
fn the_bake_and_its_queries_stay_linear() {
    let yard = harness_can_tell_linear_from_quadratic();
    baking_grows_with_the_area_it_covers(&yard);
    pathing_grows_with_the_level_it_crosses(&yard);
}

/// The harness has to be able to fail, or the guards below are decoration —
/// and measuring that is also how the guards get their yardstick.
fn harness_can_tell_linear_from_quadratic() -> Yardstick {
    // Two things this workspace's optimised dev profile forces. The size, so
    // the work clears the noise floor at all — and the `black_box` INSIDE the
    // loop, because summing 0..n has a closed form and LLVM will happily
    // replace the whole loop with it, leaving a "linear" measurement that takes
    // the same time at every size.
    //
    // Eight million rather than two: at two the small side ran in under a
    // millisecond and one scheduler hiccup on a busy machine measured a plain
    // loop at 8.1x. A guard that fails when the machine is busy is a guard
    // people learn to re-run, which is worse than no guard.
    let linear = growth(8_000_000, |n| {
        let mut acc = 0u64;
        for i in 0..n {
            acc = acc.wrapping_add(std::hint::black_box(i as u64));
        }
        std::hint::black_box(acc);
    });
    let quadratic = growth(400, |n| {
        let mut acc = 0u64;
        for i in 0..n {
            for j in 0..n {
                acc = acc.wrapping_add(std::hint::black_box((i ^ j) as u64));
            }
        }
        std::hint::black_box(acc);
    });
    let linear = linear.expect("the linear self-check must be long enough to time");
    let quadratic = quadratic.expect("the quadratic self-check must be long enough to time");
    // Not "linear is under 8" — that is the assumption that broke. What has to
    // hold is that the two are TELLABLE APART, whatever the machine is doing.
    assert!(
        quadratic > linear * 1.8,
        "linear work measured {linear:.1}x and quadratic work {quadratic:.1}x — the \
         harness cannot tell them apart, so nothing it says below means anything."
    );
    Yardstick { linear, quadratic }
}

fn quad(x0: f32, z0: f32, w: f32, d: f32, y: f32, out: &mut Vec<Tri>) {
    out.push(Tri::new([x0, y, z0], [x0 + w, y, z0], [x0, y, z0 + d]));
    out.push(Tri::new([x0 + w, y, z0], [x0 + w, y, z0 + d], [x0, y, z0 + d]));
}

/// A floor of `area` square metres with pillars through it, so the bake has
/// edges to erode and holes to trace rather than one clean rectangle.
fn level(area: usize) -> Vec<Tri> {
    let size = (area as f32).sqrt();
    let mut tris = Vec::new();
    quad(0.0, 0.0, size, size, 0.0, &mut tris);
    let n = (size / 8.0) as i32;
    for i in 0..n {
        for j in 0..n {
            quad(i as f32 * 8.0 + 3.0, j as f32 * 8.0 + 3.0, 2.0, 2.0, 1.0, &mut tris);
        }
    }
    tris
}

/// Four times the floor must cost about four times the bake.
///
/// Every stage walks cells, so any one of them gaining a scan per cell shows up
/// here — which is what the two the crate shipped with would have done.
fn baking_grows_with_the_area_it_covers(yard: &Yardstick) {
    let settings = NavSettings { cell_size: 0.5, ..Default::default() };
    let ratio = growth(1600, |area| {
        let tris = level(area);
        let mesh = bake(&tris, &settings).expect("this level has floor in it");
        std::hint::black_box(mesh.polys.len());
    });
    yard.assert_linearish("baking a navmesh", ratio);
}

/// And a path across four times the level must not cost sixteen times as much.
///
/// The search is over polygons rather than cells, but "nearest polygon to this
/// point" is a scan today, and two of those run per query — so a level that
/// grows makes every query slower even before the search does.
fn pathing_grows_with_the_level_it_crosses(yard: &Yardstick) {
    let settings = NavSettings { cell_size: 0.5, ..Default::default() };
    let mut meshes: Vec<(usize, floptle_nav::NavMesh)> = Vec::new();
    for area in [1600usize, 6400] {
        meshes.push((area, bake(&level(area), &settings).unwrap()));
    }
    let ratio = growth(1600, |area| {
        let mesh = &meshes.iter().find(|(a, _)| *a == area).unwrap().1;
        let size = (area as f32).sqrt();
        // The same number of queries at both sizes, so the ratio is the cost of
        // ONE query growing — and enough of them to be worth timing at all.
        for _ in 0..200 {
            // Corner to corner: the longest question the level can be asked.
            let p = mesh.path([1.0, 0.0, 1.0], [size - 1.0, 0.0, size - 1.0]);
            std::hint::black_box(p.map(|p| p.points.len()));
        }
    });
    yard.assert_linearish("pathing across a navmesh", ratio);
}
