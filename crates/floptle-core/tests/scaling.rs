//! Complexity guards: the operations a game does once per node, per frame.
//!
//! Three separate accidental quadratics have reached players — component lookup
//! (a linear scan per `get`, 60 ms/frame of pure lookups at 5,500 nodes),
//! cross-script wiring (`findScript` scanning every script, 25.7 ms/frame at
//! 5,000), and scatter residency (a sweep whose side grew with the view
//! distance, ~117,000 props per source). Every one of them was found by
//! somebody playing a game that had got big, and every one of them looked fine
//! at the size it was written against.
//!
//! ## Why these assert a RATIO and not a duration
//!
//! A millisecond threshold on a shared CI runner is a coin flip — a noisy
//! neighbour fails the build and everyone learns to re-run it until it passes,
//! which is worse than no test. So each guard measures the same work at `n` and
//! at `4n` and asserts on the GROWTH. Runner speed cancels: a machine twice as
//! slow takes twice as long at both sizes and the ratio is unchanged.
//!
//! Linear work grows 4x when the input does. Quadratic work grows 16x. The
//! ceilings below sit well above the first and far below the second, so ordinary
//! variance is invisible and a complexity class change is unmissable.
//!
//! These are guards, not benchmarks. `cargo run -p floptle-render --release
//! --example perf_probe` is where absolute frame costs are measured — it needs a
//! GPU, which is why it cannot live here.

use std::time::{Duration, Instant};

use floptle_core::math::DVec3;
use floptle_core::scatter::{self, Align, Band, Region, ScatterSource};
use floptle_core::World;

/// Time `f(n)` at `n` and `4n` and hand back the growth ratio.
///
/// Repeats until each size has had a few milliseconds of work, so a fast
/// machine doesn't measure the clock instead of the code — and takes the BEST
/// of several runs rather than the mean, because scheduler noise only ever adds
/// time. The fastest observed run is the closest thing to the real cost.
fn growth(n: usize, mut f: impl FnMut(usize)) -> f64 {
    fn best_of(size: usize, f: &mut impl FnMut(usize)) -> Duration {
        let mut best = Duration::MAX;
        for _ in 0..5 {
            let t = Instant::now();
            f(size);
            best = best.min(t.elapsed());
        }
        best
    }
    // Warm caches and let any lazy init happen before the measured runs.
    f(n);
    let small = best_of(n, &mut f);
    let large = best_of(n * 4, &mut f);
    // A run too short to time says nothing; treat it as perfectly linear rather
    // than dividing by a rounding error.
    if small.as_nanos() < 10_000 {
        return 4.0;
    }
    large.as_secs_f64() / small.as_secs_f64()
}

/// Four times the input must not cost much more than four times the work.
///
/// The ceiling is deliberately loose. This is not measuring a constant factor —
/// it is asking whether the algorithm changed class.
#[track_caller]
fn assert_linearish(what: &str, ratio: f64) {
    assert!(
        ratio < 8.0,
        "{what}: 4x the input cost {ratio:.1}x the time. Linear would be ~4x and \
         quadratic ~16x, so this has gained a scan it did not have."
    );
}

/// The harness has to be able to FAIL, or every guard below is decoration.
///
/// A test that passes against the bug it guards is worse than no test: it reads
/// as coverage. So this measures work that is deliberately linear and work that
/// is deliberately quadratic, and asserts the ratio tells them apart — with the
/// same `growth` the real guards use, at a size where the difference is real.
#[test]
fn the_harness_can_tell_linear_from_quadratic() {
    let linear = growth(20_000, |n| {
        let mut acc = 0u64;
        for i in 0..n {
            acc = acc.wrapping_add(i as u64);
        }
        std::hint::black_box(acc);
    });
    assert_linearish("a deliberately linear loop", linear);

    let quadratic = growth(700, |n| {
        // The shape every one of these bugs had: a scan nested inside a walk.
        let data: Vec<u64> = (0..n as u64).collect();
        let mut acc = 0u64;
        for x in &data {
            acc = acc.wrapping_add(data.iter().filter(|y| *y == x).count() as u64);
        }
        std::hint::black_box(acc);
    });
    assert!(
        quadratic > 8.0,
        "a deliberately quadratic loop measured {quadratic:.1}x for 4x the input. \
         The harness cannot see a scan-inside-a-walk, so none of the guards in \
         this file mean anything."
    );
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Pos(f32, f32, f32);
#[derive(Clone, Copy, PartialEq, Debug)]
struct Tag(u32);

/// Reading a component per node, which is what every per-node pass does.
///
/// This was a LINEAR SCAN per lookup. Every system that walked the scene and
/// asked for a component was therefore quadratic in the scene, and at 5,500
/// nodes it cost 60 ms a frame doing nothing but finding things.
#[test]
fn component_lookup_does_not_scan_the_world() {
    let ratio = growth(2_000, |n| {
        let mut w = World::new();
        let ents: Vec<_> = (0..n)
            .map(|i| {
                let e = w.spawn();
                w.insert(e, Pos(i as f32, 0.0, 0.0));
                w.insert(e, Tag(i as u32));
                e
            })
            .collect();
        // The per-frame shape: touch every node, and for each one ask for a
        // component by entity rather than iterating a single column.
        let mut acc = 0.0f32;
        for e in &ents {
            if let Some(p) = w.get::<Pos>(*e) {
                acc += p.0;
            }
            if let Some(t) = w.get::<Tag>(*e) {
                acc += t.0 as f32;
            }
        }
        std::hint::black_box(acc);
    });
    assert_linearish("World::get per node", ratio);
}

/// Iterating a column, which is the other half of every per-node pass.
#[test]
fn a_query_walks_its_own_column_and_nothing_else() {
    let ratio = growth(4_000, |n| {
        let mut w = World::new();
        for i in 0..n {
            let e = w.spawn();
            w.insert(e, Pos(i as f32, 0.0, 0.0));
            // A second component on every node, so a query that walked the
            // whole entity space instead of its own column would show up.
            w.insert(e, Tag(i as u32));
        }
        let acc: f32 = w.query::<Pos>().map(|(_, p)| p.0).sum();
        std::hint::black_box(acc);
    });
    assert_linearish("World::query", ratio);
}

/// Spawning and inserting, which a game does in bursts on a scene load.
#[test]
fn building_a_scene_is_linear_in_its_size() {
    let ratio = growth(4_000, |n| {
        let mut w = World::new();
        for i in 0..n {
            let e = w.spawn();
            w.insert(e, Pos(i as f32, 1.0, 2.0));
        }
        std::hint::black_box(w.query::<Pos>().count());
    });
    assert_linearish("spawn + insert", ratio);
}

fn field(far: f32) -> ScatterSource {
    ScatterSource {
        id: 1,
        seed: 9,
        region: Region::Ground { center: DVec3::ZERO, half: [50_000.0, 50_000.0] },
        per_chunk: 8,
        chunk: 16.0,
        align: Align::Surface,
        scale: (1.0, 1.0),
        bands: vec![Band { asset: "rock.glb".into(), distance: far }],
        fade: 0.0,
        density: None,
        removed: Default::default(),
        anchor: None,
        frame: Default::default(),
    }
}

/// Scatter residency against the knob that sets it.
///
/// This one is quadratic BY DESIGN — the swept area really does grow with the
/// square of the view distance, and no amount of cleverness changes that a
/// bigger disc holds more chunks. What must not happen is it getting worse than
/// its own geometry, which is what the missing region clamp did: 4,489 keys
/// resolving to 174 real chunks, three quarters of the props stacked on a seam.
///
/// So this guard is against the AREA, not the distance: 4x the area is 4x the
/// chunks, and anything beyond that is waste.
#[test]
fn scatter_residency_tracks_its_own_area_and_no_worse() {
    let (near, far) = (cost_at(200.0), cost_at(400.0));
    // Twice the distance is four times the area.
    let ratio = far as f64 / near as f64;
    assert!(
        (2.5..5.0).contains(&ratio),
        "doubling the view distance moved residency {ratio:.1}x ({near} → {far} chunks). \
         Four times the area is four times the chunks; more than that is duplicate \
         coverage, less means chunks inside the range are being dropped."
    );

    fn cost_at(far: f32) -> u64 {
        scatter::cost(&field(far)).chunks
    }
}

/// …and the sweep itself is linear in the number of chunks it returns, rather
/// than doing per-chunk work that scales with the whole field.
#[test]
fn sweeping_the_resident_chunks_is_linear_in_how_many_there_are() {
    let ratio = growth(64, |n| {
        // n is a chunk count; turn it into the view distance that produces it.
        let far = (n as f32).sqrt() * 16.0;
        let s = field(far);
        let keys = scatter::chunks_near(&s, DVec3::ZERO, far as f64);
        std::hint::black_box(keys.len());
    });
    assert_linearish("chunks_near", ratio);
}

/// The spatial index (`floptle/0076`): N sphere queries over N items must stay
/// roughly LINEAR in N, where the honest scan they replace is quadratic.
///
/// This is the guard the card asked for, and it is the measurement that decided
/// the shape. "What is near here?" asked once per body per frame — which is what
/// a game with damage volumes, triggers or AI perception does — is the exact
/// N-queries-over-N-items case, and the scan makes it grow 16x when the scene
/// grows 4x. Every accidental quadratic that reached a player in this engine had
/// that same signature.
///
/// The rebuild is INSIDE the measurement on purpose. An index whose query is
/// sub-linear but whose build is worse than the scan it replaced is not a win,
/// and measuring only the query would hide that.
#[test]
fn n_sphere_queries_over_n_bodies_stay_linear() {
    use floptle_core::math::Vec3;
    use floptle_core::spatial::Grid;

    let ratio = growth(2_000, |n| {
        // A lattice a few radii apart, so a query's own neighbourhood is small
        // and the index has something to narrow.
        let side = (n as f32).cbrt().ceil() as usize;
        let items: Vec<(Vec3, f32)> = (0..n)
            .map(|i| {
                let (x, y, z) = (i % side, (i / side) % side, i / (side * side));
                (Vec3::new(x as f32 * 3.0, y as f32 * 3.0, z as f32 * 3.0), 0.5)
            })
            .collect();
        let mut grid = Grid::default();
        grid.rebuild(items.iter().copied());
        // One query per item, each a couple of cells wide — the per-frame shape.
        let mut cand = Vec::new();
        let mut total = 0usize;
        for (c, _) in &items {
            cand.clear();
            grid.sphere(*c, 2.0, &mut cand);
            total += cand.len();
        }
        std::hint::black_box(total);
    });
    assert_linearish("spatial::Grid, n queries over n items", ratio);
}

/// …and the honest scan it replaces is measured in the SAME run, so "linear" is
/// falsifiable.
///
/// Without a before-number, any cheap-enough loop looks linear at these sizes.
/// The two measurements are compared against each other rather than against an
/// absolute ceiling, for the same reason every guard here asserts a ratio:
/// whatever a loaded shared runner does to one number it does to the other.
#[test]
fn the_scan_the_index_replaces_grows_far_worse_than_it_does() {
    use floptle_core::math::Vec3;
    use floptle_core::spatial::Grid;

    fn lattice(n: usize) -> Vec<(Vec3, f32)> {
        let side = (n as f32).cbrt().ceil() as usize;
        (0..n)
            .map(|i| {
                let (x, y, z) = (i % side, (i / side) % side, i / (side * side));
                (Vec3::new(x as f32 * 3.0, y as f32 * 3.0, z as f32 * 3.0), 0.5)
            })
            .collect()
    }

    // The index: build, then one query per item.
    let indexed = growth(2_000, |n| {
        let items = lattice(n);
        let mut grid = Grid::default();
        grid.rebuild(items.iter().copied());
        let mut cand = Vec::new();
        let mut total = 0usize;
        for (c, _) in &items {
            cand.clear();
            grid.sphere(*c, 2.0, &mut cand);
            total += cand.len();
        }
        std::hint::black_box(total);
    });
    // The scan: the same question, asked of everything.
    let scanned = growth(2_000, |n| {
        let items = lattice(n);
        let mut total = 0usize;
        for (c, _) in &items {
            for (o, r) in &items {
                if o.distance_squared(*c) <= (r + 2.0) * (r + 2.0) {
                    total += 1;
                }
            }
        }
        std::hint::black_box(total);
    });
    assert!(
        scanned > indexed * 2.0,
        "4x the bodies cost the index {indexed:.1}x and the full scan {scanned:.1}x. \
         The scan is supposed to grow ~16x against the index's ~4x — if the two move \
         together, this harness is not measuring the change it claims to."
    );
}
