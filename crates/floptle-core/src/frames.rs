//! Celestial reference frames — the on-rails orbital core (solar demo S2,
//! `docs/solar-demo-plan.md`).
//!
//! The model is KSP's, because it is the right one for a game:
//!
//! * **Bodies ride rails.** A planet's position at time `t` comes from its
//!   Kepler elements analytically — no integration, so orbits are exact, never
//!   drift, and cost the same at 1× and 100 000× time-warp.
//! * **One dominant body at a time** (patched conics): a ship is inside exactly
//!   one body's sphere of influence and feels only that body's inverse-square
//!   gravity. SOI handoffs swap the frame.
//! * **f64 everywhere.** Orbits live at 10⁴–10⁶ unit scales; f32 arithmetic
//!   visibly wobbles a periapsis. Rendering re-centres near the camera (the
//!   floating origin), so f64 stays confined to this math.
//!
//! Angles are radians, times are seconds, µ = GM in units³/s². The reference
//! plane is XZ (engine Y-up): a zero-inclination orbit circles in XZ.

use glam::DVec3;

/// Classical Keplerian elements, the rails a body/ship coasts on.
///
/// `a` is the semi-major axis: positive = elliptic, **negative = hyperbolic**
/// (a flyby/escape — same math, hyperbolic anomaly). `e` must match (`e < 1`
/// elliptic, `e > 1` hyperbolic); parabolic (`e == 1`) is not representable —
/// nudge eccentricity, nobody flies an exact parabola.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Kepler {
    /// Semi-major axis (units). Negative for hyperbolic orbits.
    pub a: f64,
    /// Eccentricity: 0 circle, <1 ellipse, >1 hyperbola.
    pub e: f64,
    /// Inclination from the XZ reference plane (rad).
    pub i: f64,
    /// Longitude of the ascending node (rad).
    pub lan: f64,
    /// Argument of periapsis (rad).
    pub arg_pe: f64,
    /// Mean anomaly at `epoch` (rad; hyperbolic mean anomaly when `a < 0`).
    pub m0: f64,
    /// The time `m0` is stated at (s).
    pub epoch: f64,
}

impl Kepler {
    /// A circular equatorial orbit of radius `r` — the procgen starting point.
    pub fn circular(r: f64, phase: f64) -> Self {
        Self { a: r, e: 0.0, i: 0.0, lan: 0.0, arg_pe: 0.0, m0: phase, epoch: 0.0 }
    }

    /// Mean motion n (rad/s) around a primary of gravitational parameter `mu`.
    pub fn mean_motion(&self, mu: f64) -> f64 {
        (mu / self.a.abs().powi(3)).sqrt()
    }

    /// Orbital period (s) — `None` for hyperbolic (an escape has no period).
    pub fn period(&self, mu: f64) -> Option<f64> {
        (self.a > 0.0).then(|| std::f64::consts::TAU / self.mean_motion(mu))
    }

    /// Position + velocity in the primary's frame at time `t`.
    ///
    /// Solves Kepler's equation by Newton iteration (quadratic convergence; the
    /// cosh form for hyperbolic). ~1 µs — cheap enough to call per body per
    /// frame and per trajectory-line sample.
    pub fn pos_vel(&self, mu: f64, t: f64) -> (DVec3, DVec3) {
        let n = self.mean_motion(mu);
        let m = self.m0 + n * (t - self.epoch);
        let e = self.e;
        // Perifocal-plane coordinates (P toward periapsis, Q 90° ahead in-plane).
        let (xp, yp, vxp, vyp) = if self.a > 0.0 {
            // Elliptic: M = E - e sin E.
            let m = m.rem_euclid(std::f64::consts::TAU);
            let mut big_e = if e > 0.8 { std::f64::consts::PI } else { m };
            for _ in 0..32 {
                let f = big_e - e * big_e.sin() - m;
                let d = 1.0 - e * big_e.cos();
                let step = f / d;
                big_e -= step;
                if step.abs() < 1e-14 {
                    break;
                }
            }
            let (sin_e, cos_e) = big_e.sin_cos();
            let r = self.a * (1.0 - e * cos_e);
            let xp = self.a * (cos_e - e);
            let yp = self.a * (1.0 - e * e).sqrt() * sin_e;
            let k = (mu * self.a).sqrt() / r;
            (xp, yp, -k * sin_e, k * (1.0 - e * e).sqrt() * cos_e)
        } else {
            // Hyperbolic: M = e sinh H - H.
            let mut h = (m / e).asinh();
            for _ in 0..64 {
                let f = e * h.sinh() - h - m;
                let d = e * h.cosh() - 1.0;
                let step = f / d;
                h -= step;
                if step.abs() < 1e-14 {
                    break;
                }
            }
            let r = self.a * (1.0 - e * h.cosh()); // a < 0 ⇒ r > 0
            let xp = self.a * (h.cosh() - e);
            let yp = -self.a * (e * e - 1.0).sqrt() * h.sinh();
            let k = (mu * -self.a).sqrt() / r;
            (xp, yp, -k * h.sinh(), k * (e * e - 1.0).sqrt() * h.cosh())
        };
        // Perifocal → reference frame: Rz(lan) · Rx(i) · Rz(arg_pe) in the
        // classical Z-up basis, then swap into the engine's Y-up/XZ-plane
        // convention (classical x→x, y→z, z→y).
        let (so, co) = self.lan.sin_cos();
        let (si, ci) = self.i.sin_cos();
        let (sw, cw) = self.arg_pe.sin_cos();
        let rot = |px: f64, py: f64| {
            let x = (co * cw - so * sw * ci) * px + (-co * sw - so * cw * ci) * py;
            let y = (so * cw + co * sw * ci) * px + (-so * sw + co * cw * ci) * py;
            let z = (sw * si) * px + (cw * si) * py;
            DVec3::new(x, z, y)
        };
        (rot(xp, yp), rot(vxp, vyp))
    }

    /// Recover elements from a state vector — how a coasting ship "snaps to
    /// rails" for high warp, and what the map draws from.
    ///
    /// Degenerate cases (radial plunge, exact parabola) are nudged rather than
    /// rejected: gameplay wants *an* orbit, not a NaN.
    pub fn from_state(r: DVec3, v: DVec3, mu: f64, t: f64) -> Self {
        // Into the classical Z-up basis (engine y ↔ classical z).
        let r = DVec3::new(r.x, r.z, r.y);
        let v = DVec3::new(v.x, v.z, v.y);
        let rn = r.length().max(1e-9);
        let h = r.cross(v); // specific angular momentum
        let hn = h.length().max(1e-12);
        let energy = v.length_squared() * 0.5 - mu / rn;
        let a = if energy.abs() < 1e-12 { 1e12 } else { -mu / (2.0 * energy) };
        let evec = v.cross(h) / mu - r / rn; // eccentricity vector → periapsis
        let e = evec.length();
        let i = (h.z / hn).clamp(-1.0, 1.0).acos();
        // Node vector (toward the ascending node).
        let node = DVec3::new(-h.y, h.x, 0.0);
        let nn = node.length();
        let equatorial = nn < 1e-9;
        let circular = e < 1e-9;
        let lan = if equatorial { 0.0 } else { node.y.atan2(node.x) };
        let arg_pe = match (equatorial, circular) {
            (_, true) => 0.0,
            (true, false) => evec.y.atan2(evec.x),
            (false, false) => {
                let cos_w = (node.dot(evec) / (nn * e)).clamp(-1.0, 1.0);
                let w = cos_w.acos();
                if evec.z < 0.0 {
                    std::f64::consts::TAU - w
                } else {
                    w
                }
            }
        };
        // True anomaly ν, then anomaly + mean anomaly in the right conic family.
        let nu = if circular {
            let refv = if equatorial { DVec3::X } else { node / nn };
            let cos_nu = (refv.dot(r) / rn).clamp(-1.0, 1.0);
            let nu = cos_nu.acos();
            if r.dot(v) < 0.0 { std::f64::consts::TAU - nu } else { nu }
        } else {
            let cos_nu = (evec.dot(r) / (e * rn)).clamp(-1.0, 1.0);
            let nu = cos_nu.acos();
            if r.dot(v) < 0.0 { std::f64::consts::TAU - nu } else { nu }
        };
        let m0 = if a > 0.0 && e < 1.0 {
            // ν → eccentric anomaly E → mean anomaly M.
            let big_e = 2.0 * ((nu * 0.5).tan() * ((1.0 - e) / (1.0 + e)).sqrt()).atan();
            (big_e - e * big_e.sin()).rem_euclid(std::f64::consts::TAU)
        } else {
            // ν → hyperbolic anomaly H → hyperbolic mean anomaly.
            let tanh_half = ((e - 1.0) / (e + 1.0)).sqrt() * (nu * 0.5).tan();
            let h_an = 2.0 * tanh_half.clamp(-0.999_999_999, 0.999_999_999).atanh();
            e * h_an.sinh() - h_an
        };
        Self { a, e, i, lan, arg_pe, m0, epoch: t }
    }
}

/// Per-body atmosphere for the contextual sky + drag later (S8); `None` = vacuum.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Atmosphere {
    /// Horizon/scatter tint.
    pub color: [f32; 3],
    /// Sea-level density scalar (drag + haze strength).
    pub density: f32,
    /// Height above the surface where it fades out (units).
    pub height: f32,
}

/// One celestial body on rails.
#[derive(Clone, Debug)]
pub struct Body {
    pub name: String,
    /// Index into [`System::bodies`] of the primary this body orbits;
    /// `None` = the system root (the sun) sitting at the origin.
    pub parent: Option<usize>,
    /// Gravitational parameter µ = GM (units³/s²).
    pub mu: f64,
    /// Physical (terrain sphere) radius, for altitude and impostor scale.
    pub radius: f64,
    /// Sphere-of-influence radius. Inside it, this body is the dominant
    /// attractor (children shadow their parent). The root's is effectively ∞.
    pub soi: f64,
    /// Rails. Ignored for the root.
    pub elements: Kepler,
    pub atmosphere: Option<Atmosphere>,
}

/// A solar system on rails: bodies + the queries the demo hangs off.
#[derive(Clone, Debug, Default)]
pub struct System {
    pub bodies: Vec<Body>,
}

impl System {
    /// System-frame position of body `idx` at time `t` (root at the origin) —
    /// the parent chain walked recursively.
    pub fn body_pos(&self, idx: usize, t: f64) -> DVec3 {
        self.body_pos_vel(idx, t).0
    }

    /// System-frame position AND velocity of body `idx` at `t` (velocity sums
    /// down the parent chain — a moon moves with its planet).
    pub fn body_pos_vel(&self, idx: usize, t: f64) -> (DVec3, DVec3) {
        let b = &self.bodies[idx];
        match b.parent {
            None => (DVec3::ZERO, DVec3::ZERO),
            Some(p) => {
                let (pp, pv) = self.body_pos_vel(p, t);
                let (lp, lv) = b.elements.pos_vel(self.bodies[p].mu, t);
                (pp + lp, pv + lv)
            }
        }
    }

    /// The dominant attractor at a system-frame position — the DEEPEST body
    /// whose SOI contains it (a moon's SOI shadows its planet's, which shadows
    /// the sun's). Falls back to the root. Returns the body index.
    pub fn dominant(&self, pos: DVec3, t: f64) -> usize {
        let mut best = self.root();
        let mut advanced = true;
        // Walk down: from the current dominant body, find a CHILD whose SOI
        // contains the point; repeat until none does.
        while advanced {
            advanced = false;
            for (i, b) in self.bodies.iter().enumerate() {
                if b.parent == Some(best)
                    && (pos - self.body_pos(i, t)).length() < b.soi
                {
                    best = i;
                    advanced = true;
                    break;
                }
            }
        }
        best
    }

    /// Inverse-square acceleration a ship at `pos` feels from its dominant
    /// body — THE gravity of the space game (patched conics: one attractor).
    pub fn gravity(&self, pos: DVec3, t: f64) -> DVec3 {
        let dom = self.dominant(pos, t);
        let d = self.body_pos(dom, t) - pos;
        let r2 = d.length_squared().max(1e-6);
        d / r2.sqrt() * (self.bodies[dom].mu / r2)
    }

    /// The root (parentless) body's index — the sun.
    pub fn root(&self) -> usize {
        self.bodies.iter().position(|b| b.parent.is_none()).unwrap_or(0)
    }

    /// Laplace sphere-of-influence radius for a body of parameter `mu` orbiting
    /// a primary of `parent_mu` at semi-major axis `a`.
    pub fn soi_radius(a: f64, mu: f64, parent_mu: f64) -> f64 {
        a * (mu / parent_mu).powf(0.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MU: f64 = 5.0e6;

    fn assert_close(a: DVec3, b: DVec3, tol: f64, what: &str) {
        assert!((a - b).length() < tol, "{what}: {a:?} vs {b:?} (Δ {})", (a - b).length());
    }

    #[test]
    fn circular_orbit_has_the_textbook_period_and_speed() {
        let k = Kepler::circular(1000.0, 0.0);
        let (r0, v0) = k.pos_vel(MU, 0.0);
        assert!((r0.length() - 1000.0).abs() < 1e-6);
        // v = sqrt(mu/r) for a circle, and the period closes the loop exactly.
        assert!((v0.length() - (MU / 1000.0).sqrt()).abs() < 1e-9);
        let period = k.period(MU).unwrap();
        let (r1, v1) = k.pos_vel(MU, period);
        assert_close(r0, r1, 1e-6, "one full period returns home");
        assert_close(v0, v1, 1e-9, "velocity too");
        // Orbit lies in the engine's XZ plane (Y-up convention).
        assert!(r0.y.abs() < 1e-9 && k.pos_vel(MU, period * 0.3).0.y.abs() < 1e-9);
    }

    #[test]
    fn elements_round_trip_through_state_vectors() {
        // An eccentric, inclined, rotated ellipse — every element nonzero.
        let k = Kepler { a: 2000.0, e: 0.45, i: 0.6, lan: 1.1, arg_pe: 2.3, m0: 0.7, epoch: 0.0 };
        for &t in &[0.0, 13.7, 500.0, 4321.0] {
            let (r, v) = k.pos_vel(MU, t);
            let k2 = Kepler::from_state(r, v, MU, t);
            // Compare by TRAJECTORY, not raw angles (angle aliasing): the
            // recovered elements must reproduce the same future states.
            for &dt in &[0.0, 100.0, 777.0] {
                let (ra, va) = k.pos_vel(MU, t + dt);
                let (rb, vb) = k2.pos_vel(MU, t + dt);
                assert_close(ra, rb, 1e-5 * k.a, "position round-trip");
                assert_close(va, vb, 1e-7 * va.length().max(1.0), "velocity round-trip");
            }
        }
    }

    #[test]
    fn hyperbolic_flyby_round_trips_and_conserves_energy() {
        let k = Kepler { a: -1500.0, e: 1.8, i: 0.3, lan: 0.4, arg_pe: 1.0, m0: -2.0, epoch: 0.0 };
        assert!(k.period(MU).is_none(), "an escape has no period");
        let mut last_energy = None;
        for &t in &[0.0, 50.0, 400.0, 2000.0] {
            let (r, v) = k.pos_vel(MU, t);
            let energy = v.length_squared() * 0.5 - MU / r.length();
            if let Some(prev) = last_energy {
                let rel: f64 = (energy - prev) / energy;
                assert!(rel.abs() < 1e-9, "energy drifted: {prev} → {energy}");
            }
            last_energy = Some(energy);
            let k2 = Kepler::from_state(r, v, MU, t);
            let (ra, _) = k.pos_vel(MU, t + 123.0);
            let (rb, _) = k2.pos_vel(MU, t + 123.0);
            assert_close(ra, rb, 1e-4 * k.a.abs(), "hyperbolic round-trip");
        }
    }

    fn demo_system() -> System {
        let sun_mu = 1.0e9;
        let planet_mu = 2.0e6;
        let moon_mu = 4.0e4;
        let planet_a = 50_000.0;
        let moon_a = 1_200.0;
        System {
            bodies: vec![
                Body {
                    name: "Sun".into(),
                    parent: None,
                    mu: sun_mu,
                    radius: 2000.0,
                    soi: f64::INFINITY,
                    elements: Kepler::circular(0.0, 0.0),
                    atmosphere: None,
                },
                Body {
                    name: "Kest".into(),
                    parent: Some(0),
                    mu: planet_mu,
                    radius: 300.0,
                    soi: System::soi_radius(planet_a, planet_mu, sun_mu),
                    elements: Kepler::circular(planet_a, 0.0),
                    atmosphere: Some(Atmosphere { color: [0.45, 0.65, 1.0], density: 1.0, height: 60.0 }),
                },
                Body {
                    name: "Pebble".into(),
                    parent: Some(1),
                    mu: moon_mu,
                    radius: 60.0,
                    soi: System::soi_radius(moon_a, moon_mu, planet_mu),
                    elements: Kepler::circular(moon_a, 1.0),
                    atmosphere: None,
                },
            ],
        }
    }

    #[test]
    fn soi_dominance_walks_sun_planet_moon() {
        let sys = demo_system();
        let t = 3600.0;
        let planet = sys.body_pos(1, t);
        let moon = sys.body_pos(2, t);
        // Deep space → the sun; near the planet → the planet; near the moon →
        // the moon (whose SOI sits INSIDE the planet's and must shadow it).
        assert_eq!(sys.dominant(planet * 3.0, t), 0);
        assert_eq!(sys.dominant(planet + DVec3::new(500.0, 0.0, 0.0), t), 1);
        assert_eq!(sys.dominant(moon + DVec3::new(30.0, 0.0, 0.0), t), 2);
        // Gravity near the planet points at the planet, inverse-square.
        let p = planet + DVec3::new(400.0, 0.0, 0.0);
        let g = sys.gravity(p, t);
        assert!(g.normalize().dot((planet - p).normalize()) > 0.999);
        assert!((g.length() - sys.bodies[1].mu / (400.0f64 * 400.0)).abs() < 1e-9);
        // A moon's velocity includes its planet's (it travels WITH it).
        let (_, moon_v) = sys.body_pos_vel(2, t);
        let (_, planet_v) = sys.body_pos_vel(1, t);
        assert!((moon_v - planet_v).length() < moon_v.length());
    }
}
