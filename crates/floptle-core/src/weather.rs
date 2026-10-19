//! Wind: one field that particles, props, bodies and clouds can all be moved by.
//!
//! A wind is declared by a game, in the frame of the body it blows on, and is a
//! pure function of position and time: there is no simulation state, so every
//! machine and every system that asks "how hard is it blowing here" gets the
//! same answer, and a wind costs nothing until something asks.
//!
//! A wind with no radius blows everywhere (on its body, or across the world if
//! it rides nothing). A wind with a radius is a local cell, fading to nothing
//! at its edge, which is how a storm, a valley's down-draught or a dust devil
//! sits on top of a planet's prevailing wind. The field at a point is the sum
//! of every wind there.

use glam::{DVec3, Vec3};

use crate::noise::Noise;
use crate::scatter::Frame;

/// One wind, as a game declared it.
#[derive(Clone, Debug)]
pub struct Wind {
    pub id: u32,
    /// The node this wind blows in the frame of, by name. `None` pins it to
    /// the world.
    pub anchor: Option<String>,
    /// Where the anchor is this frame. Refreshed by the host; identity without
    /// an anchor.
    pub frame: Frame,
    /// The steady wind, in metres a second, in the anchor's frame.
    pub base: Vec3,
    /// How much gusts swing the speed, as a fraction of it: 0.4 blows between
    /// 0.6 and 1.4 times `base`.
    pub gust: f32,
    /// Seconds between gusts, roughly.
    pub gust_period: f32,
    /// Eddies on top of the steady wind, as a fraction of its speed.
    pub turbulence: f32,
    /// The size of an eddy, in metres.
    pub scale: f32,
    /// A local cell: its centre in the anchor's frame and its radius. `None`
    /// blows everywhere.
    pub cell: Option<(DVec3, f64)>,
    pub seed: u32,
}

impl Default for Wind {
    fn default() -> Self {
        Self {
            id: 0,
            anchor: None,
            frame: Frame::IDENTITY,
            base: Vec3::ZERO,
            gust: 0.0,
            gust_period: 6.0,
            turbulence: 0.0,
            scale: 40.0,
            cell: None,
            seed: 1,
        }
    }
}

/// Noise lattice coordinates are taken modulo this many cells before they are
/// narrowed to f32, so a wind far from the origin keeps its detail. The field
/// repeats at that period, which at the default eddy size is 40 km.
const WRAP: f64 = 1024.0;

fn narrow(v: f64) -> f32 {
    v.rem_euclid(WRAP) as f32
}

impl Wind {
    /// The wind's velocity at world point `p` and game time `t` seconds, in
    /// world space.
    pub fn velocity_at(&self, p: DVec3, t: f64) -> Vec3 {
        let local = self.frame.to_local(p);
        let weight = match self.cell {
            None => 1.0,
            Some((centre, radius)) => {
                let r = radius.max(1e-6);
                let d = (local - centre).length() / r;
                if d >= 1.0 {
                    return Vec3::ZERO;
                }
                // Smooth to zero at the edge, so walking out of a storm is a
                // lull rather than a step.
                let k = 1.0 - d * d;
                (k * k) as f32
            }
        };
        let v = self.local_velocity(local, t);
        self.frame.dir_to_world(v) * weight
    }

    /// [`Self::velocity_at`] before the anchor's rotation and the cell's fade.
    fn local_velocity(&self, local: DVec3, t: f64) -> Vec3 {
        let speed = self.base.length();
        let noise = Noise::new(self.seed);
        let mut v = self.base;
        if self.gust != 0.0 && speed > 0.0 {
            // A gust front is carried by the wind: the gust that is here now
            // reaches a point downwind `along / speed` seconds later.
            let dir = (self.base / speed).as_dvec3();
            let phase = t - local.dot(dir) / speed as f64;
            let period = self.gust_period.max(0.05) as f64;
            let g = noise.value(Vec3::new(narrow(phase / period), 17.5, 3.25));
            v *= 1.0 + self.gust * g;
        }
        if self.turbulence != 0.0 {
            // Eddies drift with the wind too, rather than standing still while
            // it blows through them.
            let q = (local - self.base.as_dvec3() * t) / self.scale.max(0.01) as f64;
            let at = |o: f64| Vec3::new(narrow(q.x + o), narrow(q.y + o * 0.5), narrow(q.z - o));
            let eddy = Vec3::new(noise.value(at(0.0)), noise.value(at(31.7)), noise.value(at(-57.3)));
            v += eddy * (self.turbulence * speed.max(1.0));
        }
        v
    }
}

/// Every wind a game has declared: what any system asks for the wind at a point.
#[derive(Clone, Debug, Default)]
pub struct WindField {
    pub winds: Vec<Wind>,
}

impl WindField {
    /// The total wind at world point `p` and game time `t`.
    pub fn at(&self, p: DVec3, t: f64) -> Vec3 {
        self.winds.iter().map(|w| w.velocity_at(p, t)).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.winds.is_empty()
    }
}

/// One strand of a lightning bolt: a jagged line, and how thick to draw it
/// relative to the main channel (1).
#[derive(Clone, Debug, PartialEq)]
pub struct Strand {
    pub points: Vec<DVec3>,
    pub width: f32,
    /// The strand it forks from (an index into the bolt), or `None` for the
    /// main channel. A fork starts on one of its parent's points.
    pub parent: Option<usize>,
}

/// A lightning bolt from `from` to `to`, as strands to draw: the main channel
/// first, then `branches` forks off it, each jagged by `jag` (0 is a straight
/// line; 0.15 is a storm's). The same seed gives the same bolt, so every
/// machine sees the one strike.
///
/// Midpoint displacement: each segment is split at a point pushed sideways by
/// up to `jag` times its length, until segments are about a fiftieth of the
/// whole. A fork leaves the channel at a random point, heads off within about
/// 40 degrees of it, and is a fifth to a half as long as the rest of the way.
pub fn bolt(from: DVec3, to: DVec3, seed: u32, branches: u32, jag: f32) -> Vec<Strand> {
    let mut rng = crate::noise::Rng::new(seed.max(1));
    let jag = jag.max(0.0) as f64;
    let mut strands = vec![Strand { points: jagged(from, to, jag, &mut rng), width: 1.0, parent: None }];
    for _ in 0..branches {
        let main = &strands[0].points;
        if main.len() < 3 {
            break;
        }
        let i = 1 + (rng.next_f64() * (main.len() - 2) as f64) as usize;
        let start = main[i];
        let rest = (to - start).length();
        let ahead = (to - start).normalize_or_zero();
        // A direction off the channel's own heading, no further than ~40°.
        let side = ahead.any_orthonormal_vector();
        let side = glam::DQuat::from_axis_angle(ahead, rng.range(0.0, std::f64::consts::TAU)) * side;
        let dir = (ahead + side * rng.range(0.35, 0.85)).normalize_or_zero();
        let end = start + dir * rest * rng.range(0.2, 0.5);
        strands.push(Strand { points: jagged(start, end, jag, &mut rng), width: rng.range(0.3, 0.6) as f32, parent: Some(0) });
    }
    strands
}

fn jagged(from: DVec3, to: DVec3, jag: f64, rng: &mut crate::noise::Rng) -> Vec<DVec3> {
    let mut pts = vec![from, to];
    let along = (to - from).normalize_or_zero();
    for _ in 0..6 {
        let mut next = Vec::with_capacity(pts.len() * 2);
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = (b - a).length();
            // Sideways only, so the bolt still gets where it is going.
            let side = DVec3::new(rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), rng.range(-1.0, 1.0));
            let side = (side - along * side.dot(along)).normalize_or_zero();
            next.push(a);
            next.push((a + b) * 0.5 + side * len * jag * rng.range(0.0, 1.0));
        }
        next.push(*pts.last().unwrap_or(&to));
        pts = next;
    }
    pts
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Quat;

    fn steady(base: Vec3) -> Wind {
        Wind { base, ..Wind::default() }
    }

    /// A steady wind is exactly what was declared, everywhere and at every time.
    #[test]
    fn a_steady_wind_blows_what_it_was_given() {
        let w = steady(Vec3::new(3.0, 0.0, -1.0));
        for (p, t) in [(DVec3::ZERO, 0.0), (DVec3::new(5e6, -2.0, 3e5), 1234.5)] {
            assert_eq!(w.velocity_at(p, t), Vec3::new(3.0, 0.0, -1.0));
        }
    }

    /// **A wind on a planet turns with the planet.** It is declared in the
    /// body's frame, so a planet a quarter of the way through its spin blows
    /// the same way across its own ground, and a different way in the world.
    #[test]
    fn a_wind_on_a_body_turns_with_the_body() {
        let mut w = steady(Vec3::X * 4.0);
        w.frame = Frame { origin: DVec3::new(1000.0, 0.0, 0.0), rot: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2) };
        let v = w.velocity_at(DVec3::new(1000.0, 5.0, 0.0), 0.0);
        assert!((v - Vec3::new(0.0, 0.0, -4.0)).length() < 1e-5, "{v}");
    }

    /// A cell blows at full strength at its centre, fades smoothly and is
    /// nothing at and past its edge.
    #[test]
    fn a_wind_cell_fades_to_nothing_at_its_edge() {
        let w = Wind { base: Vec3::Y * 10.0, cell: Some((DVec3::new(0.0, 0.0, 50.0), 20.0)), ..Wind::default() };
        let at = |z: f64| w.velocity_at(DVec3::new(0.0, 0.0, z), 0.0).y;
        assert_eq!(at(50.0), 10.0);
        assert!(at(40.0) > at(35.0) && at(35.0) > 0.0, "{} {}", at(40.0), at(35.0));
        assert_eq!(at(30.0), 0.0);
        assert_eq!(at(0.0), 0.0);
    }

    /// The field at a point is every wind there added up: a storm cell sits on
    /// top of the prevailing wind, not in place of it.
    #[test]
    fn winds_add_up() {
        let f = WindField {
            winds: vec![
                steady(Vec3::X * 2.0),
                Wind { base: Vec3::Z * 5.0, cell: Some((DVec3::ZERO, 10.0)), ..Wind::default() },
            ],
        };
        assert_eq!(f.at(DVec3::ZERO, 0.0), Vec3::new(2.0, 0.0, 5.0));
        assert_eq!(f.at(DVec3::new(50.0, 0.0, 0.0), 0.0), Vec3::new(2.0, 0.0, 0.0));
    }

    /// **A gust travels downwind at the wind's speed.** What blows at a point
    /// now blows at a point ten metres downwind a little later, so a gust is
    /// seen crossing a field of grass rather than every blade jerking at once.
    #[test]
    fn a_gust_is_carried_downwind() {
        let w = Wind { base: Vec3::X * 5.0, gust: 0.6, gust_period: 3.0, ..Wind::default() };
        let mut varied = false;
        for k in 0..40 {
            let t = k as f64 * 0.37;
            let here = w.velocity_at(DVec3::ZERO, t);
            let later = w.velocity_at(DVec3::new(10.0, 0.0, 0.0), t + 2.0);
            assert!((here - later).length() < 1e-3, "t {t}: {here} then {later}");
            varied |= (here.x - 5.0).abs() > 0.5;
        }
        assert!(varied, "the gusts never moved the speed");
    }

    /// Gusts and eddies swing the wind about its declared value rather than
    /// adding to it: averaged over a long time, the wind is what was declared.
    #[test]
    fn gusts_and_eddies_average_out_to_the_declared_wind() {
        let w = Wind { base: Vec3::new(4.0, 0.0, 0.0), gust: 0.5, gust_period: 2.0, turbulence: 0.3, scale: 10.0, ..Wind::default() };
        let n = 20_000;
        let mean: Vec3 = (0..n).map(|k| w.velocity_at(DVec3::new(3.0, 1.0, -2.0), k as f64 * 0.13)).sum::<Vec3>() / n as f32;
        assert!((mean - Vec3::new(4.0, 0.0, 0.0)).length() < 0.4, "{mean}");
        let spread = (0..200).map(|k| w.velocity_at(DVec3::ZERO, k as f64 * 0.7)).fold(0.0f32, |m, v| m.max((v - w.base).length()));
        assert!(spread > 1.0, "neither gusts nor eddies moved it: {spread}");
    }

    /// A bolt runs from where it was asked to where it was asked, jagged but
    /// never straying further sideways than its jag allows, forks off its own
    /// channel, and is the same bolt for the same seed.
    #[test]
    fn a_bolt_strikes_where_it_was_aimed() {
        let (from, to) = (DVec3::new(0.0, 500.0, 0.0), DVec3::new(40.0, 0.0, 10.0));
        let b = bolt(from, to, 7, 3, 0.15);
        assert_eq!(b.len(), 4, "a channel and three forks");
        let main = &b[0].points;
        assert_eq!((main[0], *main.last().unwrap()), (from, to));
        let axis = (to - from).normalize();
        let len = (to - from).length();
        let stray = main.iter().map(|p| ((*p - from) - axis * (*p - from).dot(axis)).length()).fold(0.0, f64::max);
        assert!(stray > 1.0, "a storm's bolt is not a straight line: {stray}");
        assert!(stray < len * 0.15, "it strayed {stray} m from a {len} m channel");
        for f in &b[1..] {
            assert_eq!(f.parent, Some(0));
            assert!(main.contains(&f.points[0]), "a fork must leave from the channel");
            assert!(f.width < 1.0);
        }
        assert_eq!(bolt(from, to, 7, 3, 0.15), b, "the same seed must give the same bolt");
        assert_ne!(bolt(from, to, 8, 3, 0.15), b);
        let straight = bolt(from, to, 7, 0, 0.0);
        assert!(straight[0].points.iter().all(|p| ((*p - from) - axis * (*p - from).dot(axis)).length() < 1e-9));
    }

    /// Far from the origin the wind keeps its detail, rather than an f32 noise
    /// coordinate rounding every eddy away.
    #[test]
    fn a_wind_far_from_the_origin_still_has_eddies() {
        let w = Wind { base: Vec3::X, turbulence: 1.0, scale: 5.0, ..Wind::default() };
        let far = DVec3::new(5.0e7, 0.0, 0.0);
        let a = w.velocity_at(far, 0.0);
        let b = w.velocity_at(far + DVec3::new(2.5, 0.0, 0.0), 0.0);
        assert!((a - b).length() > 1e-3, "two points half an eddy apart blow alike: {a} {b}");
    }
}
