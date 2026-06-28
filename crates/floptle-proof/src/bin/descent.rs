//! Floptle — Beat 2 "Stand in the Dream" proof slice.
//!
//! A standalone, hardcoded-WGSL binary (sibling to Beat 1's `main.rs`, which it
//! leaves untouched). It proves the SDF-first physics thesis (ADR-0012 / -0014):
//! a kinematic capsule walks on a *morphing* fractal planetoid, colliding against
//! the renderer's own signed-distance field, with SDF-surface gravity defining
//! "down" so you can run up the shifting walls — and an anti-trapping rule so a
//! heaving surface lifts you instead of swallowing you.
//!
//! The design was vetted by an adversarial panel: the visible crust is a fractal,
//! but the COLLISION field is an explicitly-designed smooth, solid planetoid
//! (core sphere + blended hills), which is genuinely walkable and never empty.
//!
//! Controls: WASD move (camera-relative, on the surface), Space jump, Shift
//! sprint, mouse look (click to capture, Esc release), F cycle third/first
//! person, R respawn above the planet, Esc (uncaptured) quits.

use std::sync::Arc;
use std::time::Instant;

use glam::Vec3;
use wgpu::*;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

const HDR: TextureFormat = TextureFormat::Rgba16Float;
const RENDER_DIV: u32 = 1;

// ---- the map is an actual morphing MANDELBULB fractal (LOCK-STEP with descent.wgsl).
// Measured walkable: ~10deg surface-normal rotation per 0.18u step, |grad|~0.8,
// 11% solid interior (collidable), and its normal is ~radial so mass-gravity
// toward the core gives a stable "up" you can walk on.
const MBS: f32 = 45.0; // scale the bulb up to a COLOSSAL ~54-radius fractal planet
const WMORPH: f32 = 0.05; // slow morph rate of the bulb power
// a moon you spawn on, with the fractal planet on the horizon to jump down to
const MOON_DIST: f32 = 150.0;
const R_MOON: f32 = 12.0;
const MOON_CAPTURE: f32 = 20.0; // within this of the moon surface, moon gravity wins
const MOON_G: f32 = 0.32; // moon gravity is weak so you can jump off it easily
const NOCLIP_SPEED: f32 = 45.0;
const DESCEND_RATE: f32 = 0.55; // infinite-descent octaves per second while holding C

fn moon_center() -> Vec3 {
    Vec3::new(0.0, MOON_DIST, 0.0)
}

// ---- character / physics tuning ----
const CAP_R: f32 = 0.18; // capsule radius
const CAP_HH: f32 = 0.22; // capsule half-height (segment)
const G_GROUND: f32 = 34.0; // gravity while grounded
const G_RISE: f32 = 28.0; // gravity while rising + jump held (floaty up)
const G_CUT: f32 = 46.0; // gravity while rising + jump released (variable height)
const G_FALL: f32 = 46.0; // gravity while falling (snappy arc)
const ACCEL: f32 = 50.0; // tangential input accel (high => reach max fast on ground)
const MAX_SPEED: f32 = 5.0; // ground walk speed (sprint x1.8)
const FRIC: f32 = 10.0; // tangential friction (grounded, no input)
const JUMP_V: f32 = 9.0;
const JUMP_LOCK: f32 = 0.12; // disable ground-stick briefly after a jump
const SNAP_RANGE: f32 = 0.28; // ground-stick: pull foot back to the surface within this
const SLOPE_COS: f32 = 0.5; // cos(60deg): movement/stick grounded gate
const JUMP_GROUND_EPS: f32 = 0.18; // generous jump ground detection (3x GROUND_EPS)
const JUMP_SLOPE: f32 = 0.35; // jump allowed on steeper surfaces than walking
const V_SHOVE_MAX: f32 = 9.0; // clamp on depenetration + surface-carry (> morph speed)
const V_DEAD: f32 = 0.05; // surface-carry deadband (kills standing jitter)
const SURF_VMAX: f32 = 8.0; // morph surface-speed bound for substep sizing
const K_UP: f32 = 9.0; // up-vector temporal smoothing rate
const MAX_UP_RATE: f32 = 2.6; // rad/s: clamp on up-TARGET change (anti camera-flip)
const SQUASH_K: f32 = 18.0; // squash/stretch relax rate
const EPS_N: f32 = 0.03; // sharp eps for contact/depenetration normals
const EPS_G: f32 = 0.12; // coarse eps for the gravity up-vector low-pass
const G_MIN: f32 = 0.3; // |grad| below this => normal is untrustworthy
const SKIN: f32 = 0.01;
const GROUND_EPS: f32 = 0.06;
const BLEND_NEAR: f32 = 0.2; // gravity follows terrain within this of surface
const BLEND_FAR: f32 = 1.0; // gravity is radial beyond this
const COYOTE: f32 = 0.12;
const FP_DIST: f32 = 1.0; // zoom in past this boom length => first person
const EYE: f32 = 0.15;
// grapple swing
const GRAPPLE_MAX: f32 = 75.0;
const REEL_SPEED: f32 = 14.0; // rope shorten rate while holding (reel in)
const GRAPPLE_HIT: f32 = CAP_R * 1.0;
// jetpack / air control
const JETPACK_UP: f32 = 24.0; // upward thrust (Space held in air)
const JETPACK_ACCEL: f32 = 26.0; // horizontal air thrust (WASD in air)
const AIR_MAX: f32 = 10.0; // max horizontal air speed from the jetpack

// ----------------------------- the field -----------------------------------

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// The INFINITE-DESCENT level is held thread-locally so every f_c call sees it
// without threading a parameter through the whole controller. step()/render()
// set it before doing any field queries.
thread_local!(static DIVE: std::cell::Cell<f32> = const { std::cell::Cell::new(0.0) });
fn set_dive(v: f32) {
    DIVE.with(|d| d.set(v));
}
fn cur_dive() -> f32 {
    DIVE.with(|d| d.get())
}
const DIVE_ITER_CAP: i32 = 12; // max bulb iterations (perf); past this the dive is self-similar

/// Morphing Mandelbulb distance estimator — the planet is an actual 3D fractal.
/// `iters` grows as you descend, unfolding finer detail.
fn bulb_de(p0: Vec3, t: f32, iters: i32) -> f32 {
    let power = 8.0 + 1.5 * (t * WMORPH).sin(); // slowly morph the fractal power
    let mut z = p0;
    let mut dr = 1.0_f32;
    let mut r = 0.0_f32;
    for _ in 0..iters {
        r = z.length();
        if r > 2.0 {
            break;
        }
        let theta = (z.z / r).clamp(-1.0, 1.0).acos();
        let phi = z.y.atan2(z.x);
        dr = r.powf(power - 1.0) * power * dr + 1.0;
        let zr = r.powf(power);
        let th = theta * power;
        let ph = phi * power;
        z = Vec3::new(th.sin() * ph.cos(), th.sin() * ph.sin(), th.cos()) * zr + p0;
    }
    0.5 * r.max(1e-6).ln() * r / dr
}

/// The fractal planet (morphing Mandelbulb, INFINITE-DESCENT-zoomed) unioned with
/// the moon. The dive shrinks the planet scale W and bumps the iteration count, so
/// you continuously zoom into finer detail; the rebase keeps pos bounded.
fn f_c(p: Vec3, t: f32) -> f32 {
    let dv = cur_dive();
    let frac = dv - dv.floor();
    let w = MBS * (-frac * std::f32::consts::LN_2).exp(); // MBS * 2^(-frac)
    let iters = (8 + dv.floor() as i32).min(DIVE_ITER_CAP);
    let planet = w * bulb_de(p / w, t, iters);
    let moon = (p - moon_center()).length() - R_MOON;
    planet.min(moon)
}

/// Surface velocity along the normal via a TIME-only central difference of f_c —
/// the morph is slow, so this is smooth and carries you cleanly as it shifts.
fn df_dt(p: Vec3, t: f32) -> f32 {
    let h = 0.01;
    (f_c(p, t + h) - f_c(p, t - h)) / (2.0 * h)
}

/// Central-difference gradient, normalized by 2*eps so |grad| ~ 1 on a metric
/// field. `eps` is decoupled: sharp (EPS_N) for contact, coarse (EPS_G) for up.
fn grad(p: Vec3, t: f32, eps: f32) -> Vec3 {
    let dx = f_c(p + Vec3::new(eps, 0.0, 0.0), t) - f_c(p - Vec3::new(eps, 0.0, 0.0), t);
    let dy = f_c(p + Vec3::new(0.0, eps, 0.0), t) - f_c(p - Vec3::new(0.0, eps, 0.0), t);
    let dz = f_c(p + Vec3::new(0.0, 0.0, eps), t) - f_c(p - Vec3::new(0.0, 0.0, eps), t);
    Vec3::new(dx, dy, dz) / (2.0 * eps)
}

/// Gravity "down" = -(blend of radial backbone and terrain normal). Radial when
/// far or where the gradient is weak (never degenerate); follows the wall near
/// the surface (so you can run up it — ADR-0014).
fn gravity_down(p: Vec3, t: f32) -> Vec3 {
    let f = f_c(p, t);
    let g = grad(p, t, EPS_G);
    let gm = g.length();
    let radial = p.try_normalize().unwrap_or(Vec3::Y);
    // backbone: away from whichever body you're near. Near the moon -> moon's
    // surface; otherwise -> away from the planet center, so "down" falls toward
    // the planet (and INSIDE the planet, down points to the core => descent).
    let to_moon = p - moon_center();
    let backbone = if to_moon.length() - R_MOON < MOON_CAPTURE {
        to_moon.try_normalize().unwrap_or(radial)
    } else {
        radial
    };
    let n_surf = if gm > 1e-5 { g / gm } else { backbone };
    // Lean the stable backbone PARTWAY toward the local surface normal so you can
    // cling to steep lobes, but keep it mostly radial so the chaotic fractal
    // normal can never tumble you.
    let w = 0.5 * smoothstep(BLEND_FAR, BLEND_NEAR, f) * smoothstep(G_MIN, 0.5, gm);
    let up = backbone.lerp(n_surf, w).try_normalize().unwrap_or(backbone);
    -up
}

// --------------------------- the controller ---------------------------------

/// Great-circle interpolation between two unit directions, antipodal-guarded so
/// it can never pass through a zero-length (sign-ambiguous) vector.
/// Rodrigues rotation of `v` about unit `axis` by `angle`.
fn rotate_around(v: Vec3, axis: Vec3, angle: f32) -> Vec3 {
    let (s, c) = angle.sin_cos();
    v * c + axis.cross(v) * s + axis * (axis.dot(v)) * (1.0 - c)
}

fn slerp_dir(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    let d = a.dot(b).clamp(-1.0, 1.0);
    if d > 0.9995 {
        return b;
    }
    if d < -0.9995 {
        let perp = a
            .cross(Vec3::X)
            .try_normalize()
            .unwrap_or_else(|| a.cross(Vec3::Y).normalize());
        return (a * (1.0 - t) + perp * t).normalize();
    }
    let omega = d.acos();
    let so = omega.sin();
    (a * (((1.0 - t) * omega).sin() / so) + b * ((t * omega).sin() / so)).normalize()
}

/// Clamp the angular step from `prev` to `target` to `max_angle` (anti camera-flip).
fn rate_limit_dir(prev: Vec3, target: Vec3, max_angle: f32) -> Vec3 {
    let ang = prev.dot(target).clamp(-1.0, 1.0).acos();
    if ang <= max_angle || ang < 1e-5 {
        target
    } else {
        slerp_dir(prev, target, max_angle / ang)
    }
}

enum Grapple {
    Idle,
    Firing { tip: Vec3, dir: Vec3, len: f32 },
    Attached { anchor: Vec3, rest_len: f32 },
}

/// Per-frame control inputs.
struct Ctrl {
    wish: Vec3,
    sprint: bool,
    jump_edge: bool,
    jump_held: bool,
    aim: Vec3,
    grapple_edge: bool,
    grapple_held: bool,
    free_orient: bool,
    descend: f32, // +1 = descend (C), -1 = ascend (X)
}

struct Character {
    pos: Vec3,
    vel: Vec3,
    up_smooth: Vec3,
    prev_up_target: Vec3,
    grounded: bool,
    ground_count: i32,
    coyote: f32,
    jump_lock: f32,
    jump_buffer: f32,
    dive: f32, // continuous infinite-descent level (hold C to descend)
    dive_level: i32,
    world_phase: f32,
    squash: f32,
    grapple: Grapple,
    noclip: bool,
    // telemetry for the HUD + render
    contact: Vec3,
    f_player: f32,
    v_surface: f32,
}

impl Character {
    fn spawn() -> Self {
        // spawn on the side of the moon, with the planet on the horizon below
        let pos = moon_center() + Vec3::new(R_MOON + CAP_R + 0.3, 0.0, 0.0);
        let up = Vec3::X;
        Character {
            pos,
            vel: Vec3::ZERO,
            up_smooth: up,
            prev_up_target: up,
            grounded: false,
            ground_count: 0,
            coyote: 0.0,
            jump_lock: 0.0,
            jump_buffer: 0.0,
            dive: 0.0,
            dive_level: 0,
            world_phase: 0.0,
            squash: 1.0,
            grapple: Grapple::Idle,
            noclip: false,
            contact: pos,
            f_player: 0.0,
            v_surface: 0.0,
        }
    }

    /// Noclip free-fly (V): no gravity/collision, camera-relative — fly anywhere
    /// to inspect the fractal.
    fn fly(&mut self, dt: f32, time: f32, dir: Vec3) {
        set_dive(self.dive);
        if dir.length_squared() > 1e-6 {
            self.pos += dir.normalize() * NOCLIP_SPEED * dt;
        }
        self.vel = Vec3::ZERO;
        self.grounded = false;
        let up = (-gravity_down(self.pos, time)).try_normalize().unwrap_or(self.up_smooth);
        self.up_smooth = slerp_dir(self.up_smooth, up, 1.0 - (-6.0 * dt).exp());
        self.prev_up_target = self.up_smooth;
        self.f_player = f_c(self.pos, time);
    }

    /// Generous, ceiling-safe ground test for jumping: only the LOWER half of the
    /// capsule counts, and the contact must face up (so you can't jump off a
    /// ceiling strut when walking UNDER a bridge).
    fn can_jump(&self, time: f32) -> bool {
        let up = self.up_smooth;
        for o in [-1.0_f32, -0.5] {
            let cap = self.pos + up * (CAP_HH * o);
            if f_c(cap, time) <= CAP_R + JUMP_GROUND_EPS {
                let n = grad(cap, time, EPS_N).try_normalize().unwrap_or(up);
                if n.dot(up) > JUMP_SLOPE {
                    return true;
                }
            }
        }
        false
    }

    fn step(&mut self, dt: f32, time: f32, c: &Ctrl) {
        let dt = dt.min(0.033);
        self.jump_lock = (self.jump_lock - dt).max(0.0);
        self.jump_buffer = if c.jump_edge { 0.12 } else { (self.jump_buffer - dt).max(0.0) };

        // INFINITE DESCENT: hold C to descend (zoom into finer fractal detail), X
        // to ascend. The planet scale W shrinks the surface toward finer detail;
        // crossing a level rebases pos by 2 — SEAMLESS (same fractal coordinate,
        // one more iteration of detail) so pos stays bounded forever.
        let prev_level = self.dive.floor();
        // only descend while in/near the fractal (not out on the moon)
        if self.pos.length() < MBS * 2.0 {
            self.dive = (self.dive + c.descend * DESCEND_RATE * dt).max(0.0);
        }
        let lvl = self.dive.floor();
        if lvl != prev_level {
            let factor = (lvl - prev_level).exp2();
            self.pos *= factor;
            self.vel *= factor;
            self.contact *= factor;
            if let Grapple::Attached { anchor, rest_len } = &mut self.grapple {
                *anchor *= factor;
                *rest_len *= factor;
            }
            self.dive_level = lvl as i32;
        }
        set_dive(self.dive);

        // grapple: start a shot, advance an in-flight shot, release if let go
        if c.grapple_edge {
            if let Grapple::Idle = self.grapple {
                self.grapple = Grapple::Firing { tip: self.pos + self.up_smooth * 0.2, dir: c.aim, len: 0.0 };
            }
        }
        self.update_grapple(time, c);

        // jump (generous can_jump + coyote + buffer)
        let can = self.can_jump(time);
        self.coyote = if can { COYOTE } else { (self.coyote - dt).max(0.0) };
        if self.jump_buffer > 0.0 && (can || self.coyote > 0.0) {
            self.vel += self.up_smooth * JUMP_V;
            self.grounded = false;
            self.ground_count = 0;
            self.coyote = 0.0;
            self.jump_buffer = 0.0;
            self.jump_lock = JUMP_LOCK;
            self.squash = 1.35; // stretch on jump
        }

        // extract grapple swing state (mutated in the substeps, written back after)
        let mut g_attached = false;
        let mut g_anchor = Vec3::ZERO;
        let mut g_rest = 0.0_f32;
        if let Grapple::Attached { anchor, rest_len } = &self.grapple {
            g_attached = true;
            g_anchor = *anchor;
            g_rest = *rest_len;
        }
        let near_moon = (self.pos - moon_center()).length() - R_MOON < MOON_CAPTURE;
        let speed = self.vel.length().max(SURF_VMAX).max(if g_attached { 16.0 } else { 0.0 });
        let n = (((speed * dt) / (0.5 * CAP_R)).ceil().max(4.0) as u32).min(16);
        let sub = dt / n as f32;

        for _ in 0..n {
            let up = self.up_smooth;

            // (1) SURFACE-VELOCITY CARRY (time-FD df/dt of the slow morph)
            let gn = grad(self.pos, time, EPS_N);
            let gm = gn.length();
            if gm > G_MIN {
                let nrm = gn / gm;
                let mut vsurf = -df_dt(self.pos, time) / gm.clamp(0.5, 1.5);
                vsurf = vsurf.clamp(-V_SHOVE_MAX, V_SHOVE_MAX);
                self.v_surface = vsurf;
                if vsurf.abs() > V_DEAD {
                    self.pos += nrm * vsurf * sub;
                    if self.grounded {
                        let vn = self.vel.dot(nrm);
                        self.vel += nrm * (vsurf - vn);
                    }
                }
            } else {
                self.v_surface = 0.0;
            }

            // (2) GRAVITY (asymmetric arc; jetpack floats it; weak near the moon)
            let gdir = gravity_down(self.pos, time);
            let vup = self.vel.dot(up);
            let mut g_mag = if self.grounded {
                G_GROUND
            } else if c.jump_held {
                G_RISE // jetpack makes the arc floaty
            } else if vup > 0.0 {
                G_CUT
            } else {
                G_FALL
            };
            if near_moon {
                g_mag *= MOON_G; // weak moon gravity => easy to jump off
            }
            self.vel += gdir * g_mag * sub;

            // (3) MOVEMENT: walk on the ground; JETPACK in the air
            if self.grounded {
                if c.wish.length_squared() > 1e-6 {
                    if let Some(wt) = (c.wish - up * c.wish.dot(up)).try_normalize() {
                        let mss = MAX_SPEED * if c.sprint { 1.8 } else { 1.0 };
                        self.vel += wt * ACCEL * sub;
                        let vn = up * self.vel.dot(up);
                        let mut vt = self.vel - vn;
                        if vt.length() > mss {
                            vt = vt.normalize() * mss;
                        }
                        self.vel = vn + vt;
                    }
                } else {
                    let vn = up * self.vel.dot(up);
                    let vt = self.vel - vn;
                    self.vel = vn + vt * (-FRIC * sub).exp();
                }
            } else {
                // jetpack: WASD air thrust (speed-capped) + Space up-thrust
                if c.wish.length_squared() > 1e-6 {
                    if let Some(wt) = (c.wish - up * c.wish.dot(up)).try_normalize() {
                        self.vel += wt * JETPACK_ACCEL * sub;
                        let vn = up * self.vel.dot(up);
                        let mut vt = self.vel - vn;
                        if vt.length() > AIR_MAX {
                            vt = vt.normalize() * AIR_MAX;
                        }
                        self.vel = vn + vt;
                    }
                }
                if c.jump_held {
                    self.vel += up * JETPACK_UP * sub;
                }
            }

            // (4) INTEGRATE
            self.pos += self.vel * sub;

            // (4b) GRAPPLE SWING — reel the rope in, and when it's taut hold to
            // length + remove outward velocity => you SWING on it like a pendulum.
            if g_attached {
                g_rest = (g_rest - REEL_SPEED * sub).max(2.0);
                let to = g_anchor - self.pos;
                let dist = to.length();
                if dist > 1e-3 {
                    let dir = to / dist;
                    if dist > g_rest {
                        self.pos += dir * (dist - g_rest);
                        let away = self.vel.dot(dir);
                        if away < 0.0 {
                            self.vel -= dir * away;
                        }
                    }
                }
            }

            // (5) DEPENETRATION — 5 spheres, position-only, clamped
            let max_shove = V_SHOVE_MAX * sub;
            let mut correction = Vec3::ZERO;
            let mut deepest_f = f32::INFINITY;
            let mut contact_n = up;
            for o in [-1.0_f32, -0.5, 0.0, 0.5, 1.0] {
                let cap = self.pos + up * (CAP_HH * o);
                let mut cc = cap;
                for _ in 0..4 {
                    let f = f_c(cc, time);
                    if f >= CAP_R - SKIN {
                        break;
                    }
                    let g = grad(cc, time, EPS_N);
                    let gm = g.length();
                    let nrm = if gm > G_MIN { g / gm } else { up };
                    cc += nrm * (CAP_R - f).min(max_shove);
                }
                correction += cc - cap;
                let f0 = f_c(cap, time);
                if f0 < deepest_f {
                    deepest_f = f0;
                    contact_n = grad(cap, time, EPS_N).try_normalize().unwrap_or(up);
                }
            }
            self.pos += correction / 5.0;

            // (6) SLIDE
            if deepest_f < CAP_R + 0.02 {
                let into = self.vel.dot(contact_n).min(0.0);
                self.vel -= contact_n * into;
            }

            // (7) GROUNDED (strict, debounced) + ground stick
            let lo = self.pos - up * CAP_HH;
            let f_lo = f_c(lo, time);
            let n_lo = grad(lo, time, EPS_N).try_normalize().unwrap_or(up);
            let grounded_now = f_lo <= CAP_R + GROUND_EPS && n_lo.dot(up) > SLOPE_COS;
            self.ground_count = if grounded_now {
                (self.ground_count + 1).min(3)
            } else {
                (self.ground_count - 1).max(0)
            };
            self.grounded = self.ground_count >= 2;

            if self.grounded && self.jump_lock <= 0.0 {
                if f_lo > CAP_R && f_lo < CAP_R + SNAP_RANGE {
                    self.pos -= up * (f_lo - CAP_R);
                }
                let vn = self.vel.dot(up);
                if vn > 0.0 {
                    self.vel -= up * vn;
                }
            }

            // up target: stable contact normal when grounded, gravity otherwise;
            // RATE-LIMITED before the slerp so a small mass can't flip it. SKIPPED
            // in free-orient (the player controls their own up via Ctrl+mouse).
            if !c.free_orient {
                let raw = if self.grounded { n_lo } else { -gravity_down(self.pos, time) };
                let limited = rate_limit_dir(self.prev_up_target, raw, MAX_UP_RATE * sub);
                self.prev_up_target = limited;
                self.up_smooth = slerp_dir(self.up_smooth, limited, 1.0 - (-K_UP * sub).exp());
            }

            self.contact = lo - n_lo * f_lo;
            self.f_player = f_lo;
        }

        // write the reeled rope length back
        if g_attached {
            if let Grapple::Attached { rest_len, .. } = &mut self.grapple {
                *rest_len = g_rest;
            }
        }

        // squash/stretch relaxes back to neutral
        self.squash += (1.0 - self.squash) * (1.0 - (-SQUASH_K * dt).exp());
    }

    fn update_grapple(&mut self, time: f32, c: &Ctrl) {
        let pos = self.pos;
        match &mut self.grapple {
            Grapple::Firing { tip, dir, len } => {
                for _ in 0..64 {
                    let d = f_c(*tip, time);
                    if d < GRAPPLE_HIT {
                        self.grapple = Grapple::Attached {
                            anchor: *tip,
                            rest_len: (pos - *tip).length().max(2.0),
                        };
                        return;
                    }
                    let step = d.max(0.15);
                    *tip += *dir * step;
                    *len += step;
                    if *len > GRAPPLE_MAX {
                        self.grapple = Grapple::Idle;
                        return;
                    }
                }
            }
            Grapple::Attached { anchor, .. } => {
                if !c.grapple_held {
                    self.grapple = Grapple::Idle; // release keeps momentum (slingshot)
                } else {
                    // stick the anchor to the SHIFTING surface; detach if it
                    // morphs away.
                    let f = f_c(*anchor, time);
                    if f.abs() > 3.0 {
                        self.grapple = Grapple::Idle;
                    } else {
                        let g = grad(*anchor, time, EPS_N);
                        let gm = g.length();
                        if gm > 1e-4 {
                            *anchor -= (g / gm) * f;
                        }
                    }
                }
            }
            Grapple::Idle => {}
        }
    }
}

// ------------------------------ rendering -----------------------------------

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Globals {
    cam_pos: [f32; 4],
    cam_right: [f32; 4],
    cam_up: [f32; 4],
    cam_fwd: [f32; 4],
    resolution: [f32; 2],
    time: f32,
    dt: f32,
    frame: f32,
    feedback: f32,
    warp: f32,
    fov: f32,
    capsule_pos: [f32; 4],
    capsule_up: [f32; 4],
    contact: [f32; 4],
    capsule_fwd: [f32; 4],
    dive: [f32; 4],    // [dive_level, world_phase, squash, rho_player]
    grapple: [f32; 4], // [point.xyz, state] (0 idle, 1 firing tip, 2 attached anchor)
}

#[derive(Default)]
struct Input {
    w: bool,
    a: bool,
    s: bool,
    d: bool,
    jump_edge: bool,
    jump_held: bool,
    grapple_edge: bool,
    grapple_held: bool,
    sprint: bool,
    ctrl: bool,
    descend: bool,
    ascend: bool,
    captured: bool,
    mouse_dx: f32,
    mouse_dy: f32,
}

struct Targets {
    scene_view: TextureView,
    hist_view: [TextureView; 2],
    bg_post: [BindGroup; 2],
    bg_present: [BindGroup; 2],
}

fn build_targets(
    device: &Device,
    queue: &Queue,
    bgl_post: &BindGroupLayout,
    bgl_present: &BindGroupLayout,
    sampler: &Sampler,
    w: u32,
    h: u32,
) -> Targets {
    let mk = |label: &str| {
        device
            .create_texture(&TextureDescriptor {
                label: Some(label),
                size: Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: HDR,
                usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&TextureViewDescriptor::default())
    };

    let scene_view = mk("scene");
    let hist0 = mk("hist0");
    let hist1 = mk("hist1");

    let post = |hist: &TextureView| {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_post"),
            layout: bgl_post,
            entries: &[
                BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&scene_view) },
                BindGroupEntry { binding: 1, resource: BindingResource::Sampler(sampler) },
                BindGroupEntry { binding: 2, resource: BindingResource::TextureView(hist) },
            ],
        })
    };
    let present = |hist: &TextureView| {
        device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_present"),
            layout: bgl_present,
            entries: &[
                BindGroupEntry { binding: 0, resource: BindingResource::TextureView(hist) },
                BindGroupEntry { binding: 1, resource: BindingResource::Sampler(sampler) },
            ],
        })
    };

    let bg_post = [post(&hist0), post(&hist1)];
    let bg_present = [present(&hist0), present(&hist1)];

    let mut enc = device.create_command_encoder(&CommandEncoderDescriptor { label: Some("clear") });
    for v in [&scene_view, &hist0, &hist1] {
        enc.begin_render_pass(&RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: v,
                depth_slice: None,
                resolve_target: None,
                ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
    }
    queue.submit(std::iter::once(enc.finish()));

    Targets { scene_view, hist_view: [hist0, hist1], bg_post, bg_present }
}

fn make_pipeline(
    device: &Device,
    layouts: &[Option<&BindGroupLayout>],
    src: &str,
    label: &str,
    fmt: TextureFormat,
) -> RenderPipeline {
    let sm = device.create_shader_module(ShaderModuleDescriptor {
        label: Some(label),
        source: ShaderSource::Wgsl(src.into()),
    });
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: layouts,
        immediate_size: 0,
    });
    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: VertexState {
            module: &sm,
            entry_point: Some("vs"),
            compilation_options: PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: PrimitiveState::default(),
        depth_stencil: None,
        multisample: MultisampleState::default(),
        fragment: Some(FragmentState {
            module: &sm,
            entry_point: Some("fs"),
            compilation_options: PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format: fmt,
                blend: None,
                write_mask: ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

struct State {
    window: Arc<Window>,
    surface: Surface<'static>,
    device: Device,
    queue: Queue,
    config: SurfaceConfiguration,
    globals_buf: Buffer,
    bg_globals: BindGroup,
    bgl_post: BindGroupLayout,
    bgl_present: BindGroupLayout,
    sampler: Sampler,
    raymarch_pl: RenderPipeline,
    post_pl: RenderPipeline,
    present_pl: RenderPipeline,
    targets: Targets,
    render_size: (u32, u32),
    cc: Character,
    cam_pitch: f32,
    cam_dist: f32, // third-person boom length; < FP_DIST => first person
    cam_fwd_t: Vec3, // persistent tangent forward (parallel-transported, no flips)
    input: Input,
    frame: u64,
    start: Instant,
    last: Instant,
    fps_t: Instant,
    fps_frames: u32,
}

impl State {
    fn new(window: Arc<Window>) -> State {
        let size = window.inner_size();
        let instance = Instance::new(InstanceDescriptor {
            backends: Backends::all(),
            flags: InstanceFlags::default(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });
        let surface = instance.create_surface(window.clone()).expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("device"),
            required_features: Features::empty(),
            required_limits: Limits::default(),
            experimental_features: ExperimentalFeatures::default(),
            memory_hints: MemoryHints::Performance,
            trace: Trace::Off,
        }))
        .expect("no device");

        let caps = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let present_mode = if caps.present_modes.contains(&PresentMode::Mailbox) {
            PresentMode::Mailbox
        } else {
            PresentMode::Fifo
        };
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let bgl_globals = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX_FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let tex_entry = |binding: u32| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Texture {
                sample_type: TextureSampleType::Float { filterable: true },
                view_dimension: TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let samp_entry = |binding: u32| BindGroupLayoutEntry {
            binding,
            visibility: ShaderStages::FRAGMENT,
            ty: BindingType::Sampler(SamplerBindingType::Filtering),
            count: None,
        };
        let bgl_post = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("post"),
            entries: &[tex_entry(0), samp_entry(1), tex_entry(2)],
        });
        let bgl_present = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("present"),
            entries: &[tex_entry(0), samp_entry(1)],
        });

        let raymarch_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals)],
            include_str!("../descent.wgsl"),
            "descent",
            HDR,
        );
        let post_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_post)],
            include_str!("../post.wgsl"),
            "post",
            HDR,
        );
        let present_pl = make_pipeline(
            &device,
            &[Some(&bgl_globals), Some(&bgl_present)],
            include_str!("../present_plain.wgsl"),
            "present_plain",
            config.format,
        );

        let globals_buf = device.create_buffer(&BufferDescriptor {
            label: Some("globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bg_globals = device.create_bind_group(&BindGroupDescriptor {
            label: Some("bg_globals"),
            layout: &bgl_globals,
            entries: &[BindGroupEntry { binding: 0, resource: globals_buf.as_entire_binding() }],
        });

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: MipmapFilterMode::Nearest,
            ..Default::default()
        });

        let render_size = (config.width.max(2) / RENDER_DIV, config.height.max(2) / RENDER_DIV);
        let targets =
            build_targets(&device, &queue, &bgl_post, &bgl_present, &sampler, render_size.0, render_size.1);

        let now = Instant::now();
        State {
            window,
            surface,
            device,
            queue,
            config,
            globals_buf,
            bg_globals,
            bgl_post,
            bgl_present,
            sampler,
            raymarch_pl,
            post_pl,
            present_pl,
            targets,
            render_size,
            cc: Character::spawn(),
            cam_pitch: -0.25,
            cam_dist: 7.0,
            cam_fwd_t: Vec3::new(0.0, -1.0, 0.0), // look toward the planet on spawn
            input: Input::default(),
            frame: 0,
            start: now,
            last: now,
            fps_t: now,
            fps_frames: 0,
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
        self.render_size = (w.max(2) / RENDER_DIV, h.max(2) / RENDER_DIV);
        self.targets = build_targets(
            &self.device,
            &self.queue,
            &self.bgl_post,
            &self.bgl_present,
            &self.sampler,
            self.render_size.0,
            self.render_size.1,
        );
    }

    /// Tangent-plane basis from a PARALLEL-TRANSPORTED persistent forward (no
    /// discrete axis switch => no ~90deg snaps). Returns (up, fwd_t, right_t).
    fn tangent_basis(&self) -> (Vec3, Vec3, Vec3) {
        let up = self.cc.up_smooth;
        let mut f = self.cam_fwd_t - up * self.cam_fwd_t.dot(up);
        if f.length_squared() < 1e-6 {
            // forward became parallel to up (rare): pick the least-aligned axis
            let ax = if up.x.abs() <= up.y.abs() && up.x.abs() <= up.z.abs() {
                Vec3::X
            } else if up.y.abs() <= up.z.abs() {
                Vec3::Y
            } else {
                Vec3::Z
            };
            f = ax - up * ax.dot(up);
        }
        let fwd_t = f.normalize();
        let right_t = fwd_t.cross(up).normalize();
        (up, fwd_t, right_t)
    }

    fn update(&mut self, dt: f32, time: f32) {
        set_dive(self.cc.dive); // so aim_dir/camera query the field at the right depth
        let sens = 0.0025;
        // CTRL + mouse while airborne = roll/pitch your whole frame (wingsuit).
        let free_orient = self.input.ctrl && !self.cc.noclip && !self.cc.grounded;
        if free_orient {
            let up = self.cc.up_smooth;
            let right = self.cam_fwd_t.cross(up).try_normalize().unwrap_or(Vec3::X);
            let pitch_amt = -self.input.mouse_dy * sens * 1.4;
            let roll_amt = self.input.mouse_dx * sens * 1.4;
            let nu = rotate_around(up, right, pitch_amt);
            self.cam_fwd_t = rotate_around(self.cam_fwd_t, right, pitch_amt).normalize();
            self.cc.up_smooth = rotate_around(nu, self.cam_fwd_t, roll_amt).normalize();
            self.cc.prev_up_target = self.cc.up_smooth;
        } else {
            let up = self.cc.up_smooth;
            // yaw: rotate the persistent forward about up (incremental => no flips)
            let yaw = -self.input.mouse_dx * sens;
            if yaw != 0.0 {
                let (s, cz) = yaw.sin_cos();
                let f = self.cam_fwd_t;
                self.cam_fwd_t = f * cz + up.cross(f) * s + up * (up.dot(f)) * (1.0 - cz);
            }
            self.cam_fwd_t = (self.cam_fwd_t - up * self.cam_fwd_t.dot(up))
                .try_normalize()
                .unwrap_or(self.cam_fwd_t);
            self.cam_pitch = (self.cam_pitch - self.input.mouse_dy * sens).clamp(-1.0, 1.2);
        }
        self.input.mouse_dx = 0.0;
        self.input.mouse_dy = 0.0;

        let (up, fwd_t, right_t) = self.tangent_basis();
        let mut wish = Vec3::ZERO;
        if self.input.w {
            wish += fwd_t;
        }
        if self.input.s {
            wish -= fwd_t;
        }
        if self.input.d {
            wish += right_t;
        }
        if self.input.a {
            wish -= right_t;
        }
        let (sp, cp) = self.cam_pitch.sin_cos();
        let fly_aim = (fwd_t * cp + up * sp).try_normalize().unwrap_or(fwd_t);
        let aim = self.aim_dir(time); // grapple aim from the crosshair raycast

        if self.cc.noclip {
            // free-fly: full-3D camera-relative (Space up, Shift down)
            let mut dir = Vec3::ZERO;
            if self.input.w {
                dir += fly_aim;
            }
            if self.input.s {
                dir -= fly_aim;
            }
            if self.input.d {
                dir += right_t;
            }
            if self.input.a {
                dir -= right_t;
            }
            if self.input.jump_held {
                dir += up;
            }
            if self.input.sprint {
                dir -= up;
            }
            self.cc.fly(dt, time, dir);
        } else {
            let ctrl = Ctrl {
                wish,
                sprint: self.input.sprint,
                jump_edge: self.input.jump_edge,
                jump_held: self.input.jump_held,
                aim,
                grapple_edge: self.input.grapple_edge,
                grapple_held: self.input.grapple_held,
                free_orient,
                descend: (self.input.descend as i32 - self.input.ascend as i32) as f32,
            };
            self.cc.step(dt, time, &ctrl);
        }
        self.input.jump_edge = false;
        self.input.grapple_edge = false;
    }

    /// Camera pose as vectors: (cam_pos, forward, up).
    fn camera_pose(&self, time: f32) -> (Vec3, Vec3, Vec3) {
        let (up, fwd_t, _right_t) = self.tangent_basis();
        let (cam_pos, fwd) = if self.cam_dist <= FP_DIST {
            let (sp, cp) = self.cam_pitch.sin_cos();
            let look = (fwd_t * cp + up * sp).try_normalize().unwrap_or(fwd_t);
            (self.cc.pos + up * EYE, look)
        } else {
            // third person: orbit ABOVE and BEHIND, looking down at the player.
            let target = self.cc.pos + up * (CAP_HH + 0.3);
            let e = (0.55 - self.cam_pitch * 0.5).clamp(0.15, 1.3);
            let (se, ce) = e.sin_cos();
            let dir_to_cam = (-fwd_t * ce + up * se).try_normalize().unwrap_or(up);
            let dist: f32;
            let mut s = 0.4_f32;
            loop {
                let d = f_c(target + dir_to_cam * s, time) - 0.2;
                if d < 0.0 {
                    dist = s.max(0.5);
                    break;
                }
                s += d.max(0.08);
                if s >= self.cam_dist {
                    dist = self.cam_dist;
                    break;
                }
            }
            let cp_pos = target + dir_to_cam * dist;
            (cp_pos, (target - cp_pos).try_normalize().unwrap_or(-fwd_t))
        };
        (cam_pos, fwd, up)
    }

    fn camera(&self, time: f32) -> ([f32; 4], [f32; 4], [f32; 4], [f32; 4]) {
        let (cam_pos, fwd, up) = self.camera_pose(time);
        let right = fwd.cross(up).try_normalize().unwrap_or(Vec3::X);
        let camup = right.cross(fwd).normalize();
        (
            [cam_pos.x, cam_pos.y, cam_pos.z, 0.0],
            [right.x, right.y, right.z, 0.0],
            [camup.x, camup.y, camup.z, 0.0],
            [fwd.x, fwd.y, fwd.z, 0.0],
        )
    }

    /// Grapple aim = raycast the screen-center crosshair (camera forward) and aim
    /// from the player toward the hit, so THIRD-PERSON aim matches the reticle.
    fn aim_dir(&self, time: f32) -> Vec3 {
        let (cam_pos, cam_fwd, _up) = self.camera_pose(time);
        let mut target = cam_pos + cam_fwd * (GRAPPLE_MAX + 20.0);
        let mut tt = 0.5_f32;
        for _ in 0..96 {
            let d = f_c(cam_pos + cam_fwd * tt, time);
            if d < 0.3 {
                target = cam_pos + cam_fwd * tt;
                break;
            }
            tt += d.max(0.3);
            if tt > 220.0 {
                break;
            }
        }
        (target - self.cc.pos).try_normalize().unwrap_or(cam_fwd)
    }

    fn render(&mut self) {
        let now = Instant::now();
        let dt = (now - self.last).as_secs_f32().min(0.1);
        self.last = now;
        let time = (now - self.start).as_secs_f32();
        self.update(dt, time);

        let (cam_pos, cam_right, cam_up, cam_fwd) = self.camera(time);
        let up = self.cc.up_smooth;
        let (_fb_up, face_fwd, _fb_r) = self.tangent_basis();
        let gp = match &self.cc.grapple {
            Grapple::Idle => [0.0, 0.0, 0.0, 0.0],
            Grapple::Firing { tip, .. } => [tip.x, tip.y, tip.z, 1.0],
            Grapple::Attached { anchor, .. } => [anchor.x, anchor.y, anchor.z, 2.0],
        };
        let g = Globals {
            cam_pos,
            cam_right,
            cam_up,
            cam_fwd,
            resolution: [self.render_size.0 as f32, self.render_size.1 as f32],
            time,
            dt,
            frame: self.frame as f32,
            feedback: 0.0, // feedback trails OFF — clean geometry view
            warp: 1.0,
            fov: 1.25,
            capsule_pos: [
                self.cc.pos.x,
                self.cc.pos.y,
                self.cc.pos.z,
                if self.cam_dist <= FP_DIST { -1.0 } else { CAP_R },
            ],
            capsule_up: [up.x, up.y, up.z, CAP_HH],
            contact: [
                self.cc.contact.x,
                self.cc.contact.y,
                self.cc.contact.z,
                if self.cc.grounded { 1.0 } else { 0.0 },
            ],
            capsule_fwd: [face_fwd.x, face_fwd.y, face_fwd.z, 0.0],
            dive: [
                self.cc.dive,
                self.cc.world_phase,
                self.cc.squash,
                0.0,
            ],
            grapple: gp,
        };
        self.queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&g));

        let surface_texture = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(t) | CurrentSurfaceTexture::Suboptimal(t) => t,
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let surf_view = surface_texture.texture.create_view(&TextureViewDescriptor::default());

        let read = (self.frame % 2) as usize;
        let write = 1 - read;

        let mut enc =
            self.device.create_command_encoder(&CommandEncoderDescriptor { label: Some("frame") });

        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("walk"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.targets.scene_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.raymarch_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.draw(0..3, 0..1);
        }
        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("post"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &self.targets.hist_view[write],
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.post_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.set_bind_group(1, &self.targets.bg_post[read], &[]);
            rp.draw(0..3, 0..1);
        }
        {
            let mut rp = enc.begin_render_pass(&RenderPassDescriptor {
                label: Some("present"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &surf_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations { load: LoadOp::Clear(Color::BLACK), store: StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&self.present_pl);
            rp.set_bind_group(0, &self.bg_globals, &[]);
            rp.set_bind_group(1, &self.targets.bg_present[write], &[]);
            rp.draw(0..3, 0..1);
        }

        self.queue.submit(std::iter::once(enc.finish()));
        surface_texture.present();
        self.frame += 1;

        self.fps_frames += 1;
        let since = now - self.fps_t;
        if since.as_secs_f32() >= 0.5 {
            let fps = self.fps_frames as f32 / since.as_secs_f32();
            let mode = if self.cc.noclip {
                "noclip"
            } else if self.cam_dist <= FP_DIST {
                "1st"
            } else {
                "3rd"
            };
            self.window.set_title(&format!(
                "Floptle — Descent into the Fractal Core (Beat 3)  |  {fps:.0} fps  [{mode}]  depth:{}  grounded:{}  f:{:+.2}",
                self.cc.dive_level, self.cc.grounded as u8, self.cc.f_player
            ));
            self.fps_frames = 0;
            self.fps_t = now;
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Floptle — Descent into the Fractal Core (Beat 3)")
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        self.state = Some(State::new(window));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(state) = self.state.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => state.resize(size.width, size.height),
            WindowEvent::RedrawRequested => state.render(),
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32 * 0.05,
                };
                state.cam_dist = (state.cam_dist - dy).clamp(0.5, 18.0);
            }
            WindowEvent::MouseInput { state: btn, button: MouseButton::Left, .. } => {
                let pressed = btn == ElementState::Pressed;
                if pressed {
                    if !state.input.captured {
                        let w = &state.window;
                        let _ = w
                            .set_cursor_grab(CursorGrabMode::Locked)
                            .or_else(|_| w.set_cursor_grab(CursorGrabMode::Confined));
                        w.set_cursor_visible(false);
                        state.input.captured = true;
                    } else {
                        // fire grapple while captured
                        state.input.grapple_edge = true;
                        state.input.grapple_held = true;
                    }
                } else {
                    state.input.grapple_held = false;
                }
            }
            WindowEvent::Focused(false) => {
                let _ = state.window.set_cursor_grab(CursorGrabMode::None);
                state.window.set_cursor_visible(true);
                state.input.captured = false;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::KeyW => state.input.w = pressed,
                        KeyCode::KeyA => state.input.a = pressed,
                        KeyCode::KeyS => state.input.s = pressed,
                        KeyCode::KeyD => state.input.d = pressed,
                        KeyCode::Space => {
                            if pressed && !state.input.jump_held {
                                state.input.jump_edge = true;
                            }
                            state.input.jump_held = pressed;
                        }
                        KeyCode::ShiftLeft | KeyCode::ShiftRight => state.input.sprint = pressed,
                        KeyCode::ControlLeft | KeyCode::ControlRight => state.input.ctrl = pressed,
                        KeyCode::KeyC => state.input.descend = pressed,
                        KeyCode::KeyX => state.input.ascend = pressed,
                        KeyCode::KeyF if pressed => state.cam_dist = 7.0,
                        KeyCode::KeyV if pressed => state.cc.noclip = !state.cc.noclip,
                        KeyCode::KeyR if pressed => state.cc = Character::spawn(),
                        KeyCode::Escape if pressed => {
                            if state.input.captured {
                                let _ = state.window.set_cursor_grab(CursorGrabMode::None);
                                state.window.set_cursor_visible(true);
                                state.input.captured = false;
                            } else {
                                event_loop.exit();
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            if let Some(state) = self.state.as_mut() {
                if state.input.captured {
                    state.input.mouse_dx += delta.0 as f32;
                    state.input.mouse_dy += delta.1 as f32;
                }
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(state) = self.state.as_ref() {
            state.window.request_redraw();
        }
    }
}

fn main() {
    env_logger::init();
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Poll);
    let mut app = App::default();
    event_loop.run_app(&mut app).expect("run app");
}
