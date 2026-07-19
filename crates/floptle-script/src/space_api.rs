//! The Lua `space.*` API — the solar demo's orbital readouts (S2, backed by
//! `floptle_core::frames`).
//!
//! The editor assembles the scene's `CelestialBody` nodes into an on-rails
//! system each tick and feeds a snapshot here; reads are instant against that
//! snapshot. `space.warp(m)` QUEUES a request the editor applies (and may
//! clamp/reject — e.g. under thrust once S4's rules land).

use std::cell::RefCell;
use std::rc::Rc;

use floptle_core::frames::Kepler;
use glam::DVec3;
use mlua::Lua;

/// One body in this tick's snapshot, WORLD coordinates.
#[derive(Clone, Debug, Default)]
pub struct SpaceBodyInfo {
    pub name: String,
    pub pos: [f64; 3],
    pub vel: [f64; 3],
    pub mu: f64,
    pub radius: f64,
    /// `f64::INFINITY` for the root.
    pub soi: f64,
}

/// The per-tick snapshot the editor feeds (empty when no celestial bodies).
#[derive(Clone, Debug, Default)]
pub struct SpaceInfo {
    pub time: f64,
    pub warp: f64,
    pub bodies: Vec<SpaceBodyInfo>,
}

/// Deepest body whose SOI contains `p` (patched conics), else None.
fn dominant(info: &SpaceInfo, p: DVec3) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, b) in info.bodies.iter().enumerate() {
        if (p - DVec3::from(b.pos)).length() <= b.soi
            && best.is_none_or(|(_, s)| b.soi < s)
        {
            best = Some((i, b.soi));
        }
    }
    best.map(|(i, _)| i)
}

fn body_table(lua: &Lua, b: &SpaceBodyInfo) -> mlua::Result<mlua::Table> {
    let t = lua.create_table()?;
    t.set("name", b.name.clone())?;
    t.set("x", b.pos[0])?;
    t.set("y", b.pos[1])?;
    t.set("z", b.pos[2])?;
    t.set("vx", b.vel[0])?;
    t.set("vy", b.vel[1])?;
    t.set("vz", b.vel[2])?;
    t.set("mu", b.mu)?;
    t.set("radius", b.radius)?;
    t.set("soi", if b.soi.is_finite() { b.soi } else { -1.0 })?;
    Ok(t)
}

pub(crate) fn install_space_api(
    lua: &Lua,
    info: Rc<RefCell<SpaceInfo>>,
    warp_request: Rc<RefCell<Option<f64>>>,
) {
    let Ok(t) = lua.create_table() else { return };

    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(move |_, ()| Ok(info.borrow().time)) {
            let _ = t.set("time", f);
        }
    }
    {
        let info = info.clone();
        let req = warp_request.clone();
        if let Ok(f) = lua.create_function(move |_, m: Option<f64>| match m {
            None => Ok(info.borrow().warp),
            Some(m) => {
                if !(1.0..=100_000.0).contains(&m) {
                    return Err(mlua::Error::runtime(format!(
                        "space.warp: multiplier {m} out of range (1 .. 100000)"
                    )));
                }
                *req.borrow_mut() = Some(m);
                Ok(m)
            }
        }) {
            let _ = t.set("warp", f);
        }
    }
    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(move |lua, ()| {
            let info = info.borrow();
            let arr = lua.create_table()?;
            for (i, b) in info.bodies.iter().enumerate() {
                arr.set(i + 1, body_table(lua, b)?)?;
            }
            Ok(arr)
        }) {
            let _ = t.set("bodies", f);
        }
    }
    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(move |lua, name: String| {
            let info = info.borrow();
            match info.bodies.iter().find(|b| b.name == name) {
                Some(b) => Ok(Some(body_table(lua, b)?)),
                None => Ok(None),
            }
        }) {
            let _ = t.set("body", f);
        }
    }
    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let info = info.borrow();
            Ok(dominant(&info, DVec3::new(x, y, z)).map(|i| info.bodies[i].name.clone()))
        }) {
            let _ = t.set("dominant", f);
        }
    }
    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(move |_, (x, y, z): (f64, f64, f64)| {
            let info = info.borrow();
            let p = DVec3::new(x, y, z);
            match dominant(&info, p) {
                Some(i) => {
                    let b = &info.bodies[i];
                    let to = DVec3::from(b.pos) - p;
                    let r2 = to.length_squared().max(1e-9);
                    let g = to / r2.sqrt() * (b.mu / r2);
                    Ok((g.x, g.y, g.z))
                }
                None => Ok((0.0, 0.0, 0.0)),
            }
        }) {
            let _ = t.set("gravity", f);
        }
    }
    // space.elements(x,y,z, vx,vy,vz) → the conic you're ON around the dominant
    // body: { body, a, e, period, apoapsis, periapsis } (period/apoapsis nil on
    // an escape). Feed it your ship's position + velocity; the map/HUD draw it.
    // The velocity is taken AS the dominant-frame velocity — which is exactly
    // what `node.vx/vy/vz` already are (the carry moves positions only), so
    // scripts pass node state straight through. Subtracting the body's world
    // velocity here again bent every readout once planets started moving.
    {
        let info = info.clone();
        if let Ok(f) = lua.create_function(
            move |lua, (x, y, z, vx, vy, vz): (f64, f64, f64, f64, f64, f64)| {
                let info = info.borrow();
                let p = DVec3::new(x, y, z);
                let Some(i) = dominant(&info, p) else { return Ok(None) };
                let b = &info.bodies[i];
                let r = p - DVec3::from(b.pos);
                let v = DVec3::new(vx, vy, vz);
                let k = Kepler::from_state(r, v, b.mu, info.time);
                let t = lua.create_table()?;
                t.set("body", b.name.clone())?;
                t.set("a", k.a)?;
                t.set("e", k.e)?;
                if let Some(period) = k.period(b.mu) {
                    t.set("period", period)?;
                    t.set("apoapsis", k.a * (1.0 + k.e))?;
                }
                t.set("periapsis", (k.a * (1.0 - k.e)).abs())?;
                Ok(Some(t))
            },
        ) {
            let _ = t.set("elements", f);
        }
    }

    // space.propagate(px,py,pz, vx,vy,vz, mu, dt) → the state vector
    // (px,py,pz, vx,vy,vz) advanced `dt` seconds on the two-body conic about a
    // point mass `mu`. THE generic celestial-mechanics primitive the map's
    // maneuver nodes + patched-conic encounter walk are built from (game-side,
    // in Lua): convert a state to its orbit and evaluate it at any future time,
    // exactly and stably — elliptic AND hyperbolic (the tested `frames::Kepler`,
    // no per-step integration drift). The state is in whatever frame you pass;
    // the caller composes parent frames (add the attractor's own motion).
    // Degenerate inputs (µ ≤ 0, non-finite state/result) pass through unchanged.
    {
        if let Ok(f) = lua.create_function(
            |_, (px, py, pz, vx, vy, vz, mu, dt): (f64, f64, f64, f64, f64, f64, f64, f64)| {
                let r = DVec3::new(px, py, pz);
                let v = DVec3::new(vx, vy, vz);
                if !mu.is_finite() || mu <= 0.0 || !r.is_finite() || !v.is_finite() {
                    return Ok((px, py, pz, vx, vy, vz));
                }
                let k = Kepler::from_state(r, v, mu, 0.0);
                let (r2, v2) = k.pos_vel(mu, dt);
                if !r2.is_finite() || !v2.is_finite() {
                    return Ok((px, py, pz, vx, vy, vz));
                }
                Ok((r2.x, r2.y, r2.z, v2.x, v2.y, v2.z))
            },
        ) {
            let _ = t.set("propagate", f);
        }
    }

    let _ = lua.globals().set("space", t);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lua() -> Lua {
        let lua = Lua::new();
        let info = Rc::new(RefCell::new(SpaceInfo::default()));
        let warp = Rc::new(RefCell::new(None));
        install_space_api(&lua, info, warp);
        lua
    }

    #[test]
    fn propagate_is_identity_at_zero_dt() {
        let lua = test_lua();
        let (x, y, z, vx, vy, vz): (f64, f64, f64, f64, f64, f64) = lua
            .load("return space.propagate(100, 0, 0, 0, 0, 31.6, 1e5, 0)")
            .eval()
            .unwrap();
        assert!((x - 100.0).abs() < 1e-9 && y.abs() < 1e-9 && z.abs() < 1e-9);
        assert!(vx.abs() < 1e-9 && vy.abs() < 1e-9 && (vz - 31.6).abs() < 1e-9);
    }

    #[test]
    fn propagate_a_full_period_returns_to_start() {
        // Circular orbit in the XZ plane (engine is Y-up): r=100, mu=1e5.
        // v_circ = sqrt(mu/r) = sqrt(1000); T = 2π·sqrt(r³/mu).
        let r = 100.0_f64;
        let mu = 1e5_f64;
        let v = (mu / r).sqrt();
        let period = std::f64::consts::TAU * (r * r * r / mu).sqrt();
        let lua = test_lua();
        let src = format!("return space.propagate(100, 0, 0, 0, 0, {v}, {mu}, {period})");
        let (x, y, z, _vx, _vy, vz): (f64, f64, f64, f64, f64, f64) =
            lua.load(&src).eval().unwrap();
        // Back to the start after exactly one period (loose tol: transcendental).
        assert!((x - 100.0).abs() < 1e-3, "x={x}");
        assert!(y.abs() < 1e-3, "y={y}");
        assert!(z.abs() < 1e-3, "z={z}");
        assert!((vz - v).abs() < 1e-3, "vz={vz}");
    }

    #[test]
    fn propagate_half_period_reaches_the_antipode() {
        let r = 100.0_f64;
        let mu = 1e5_f64;
        let v = (mu / r).sqrt();
        let half = std::f64::consts::PI * (r * r * r / mu).sqrt();
        let lua = test_lua();
        let src = format!("return space.propagate(100, 0, 0, 0, 0, {v}, {mu}, {half})");
        let (x, _y, z, _vx, _vy, vz): (f64, f64, f64, f64, f64, f64) =
            lua.load(&src).eval().unwrap();
        assert!((x + 100.0).abs() < 1e-2, "x={x}"); // opposite side
        assert!(z.abs() < 1e-2, "z={z}");
        assert!((vz + v).abs() < 1e-2, "vz={vz}"); // reversed velocity
    }
}
