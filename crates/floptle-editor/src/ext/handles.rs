//! `handles.*` — drawing in the world, from an editor extension.
//!
//! Queued during the `onSceneDraw` hook and painted over the Scene view. Every
//! call is immediate mode: the list is emptied at the top of each frame, so an
//! extension that stops drawing stops appearing, with nothing to retain and
//! nothing to leak.
//!
//! ```lua
//! ed.onSceneDraw(function()
//!     handles.color(1, 0.6, 0.2)
//!     handles.wireCube(centre, size)
//!     handles.label(centre, "spawn")
//! end)
//! ```
//!
//! **These paint over the scene rather than into it.** A handle is an authoring
//! aid — a region, a path, a measurement — and one hidden behind the wall it is
//! measuring is no use. That is the same choice `gizmo.*` makes for scripts, and
//! it is why this projects to the screen rather than going through the 3D line
//! layer.
//!
//! Everything takes world positions as `vec3(x, y, z)` or any `{x=, y=, z=}`
//! table, and colours are 0–1 floats set once and inherited by every call after
//! them — an author draws a group of things in one colour far more often than
//! they colour each one.

use std::cell::RefCell;
use std::rc::Rc;

use floptle_core::math::{DVec3, Mat4, Vec2};
use mlua::{Lua, Table, Value};

use super::Shared;

/// One queued world-space drawing command.
#[derive(Clone, Debug)]
pub(crate) enum HandleCmd {
    Line { a: [f64; 3], b: [f64; 3], color: [f32; 4], width: f32 },
    /// A filled convex polygon — heatmap cells, region floors, coverage patches.
    Poly { pts: Vec<[f64; 3]>, color: [f32; 4] },
    Label { at: [f64; 3], text: String, size: f32, color: [f32; 4] },
    Dot { at: [f64; 3], px: f32, color: [f32; 4] },
}

/// A handle command projected into a viewport, ready for an `egui::Painter`.
#[derive(Clone, Debug)]
pub(crate) enum Painted {
    Line { a: Vec2, b: Vec2, color: [f32; 4], width: f32 },
    Poly { pts: Vec<Vec2>, color: [f32; 4] },
    Label { at: Vec2, text: String, size: f32, color: [f32; 4] },
    Dot { at: Vec2, px: f32, color: [f32; 4] },
}

/// Project every queued handle for one viewport.
///
/// A command with **any** vertex behind the camera is dropped whole rather than
/// clipped: a partly-projected line runs off to a point that is not on the
/// screen and reads as a stray ray across the viewport, which is worse than a
/// missing edge. `crate::viz::project` returns `None` for exactly that case, so
/// this is one `?` per vertex.
pub(crate) fn project(
    cmds: &[HandleCmd],
    cam_world: DVec3,
    vp: Mat4,
    w: f32,
    h: f32,
    out: &mut Vec<Painted>,
) {
    let p = |v: [f64; 3]| crate::viz::project(DVec3::from(v), cam_world, vp, w, h);
    for c in cmds {
        match c {
            HandleCmd::Line { a, b, color, width } => {
                if let (Some(a), Some(b)) = (p(*a), p(*b)) {
                    out.push(Painted::Line { a, b, color: *color, width: *width });
                }
            }
            HandleCmd::Poly { pts, color } => {
                let mut screen = Vec::with_capacity(pts.len());
                let mut ok = true;
                for v in pts {
                    match p(*v) {
                        Some(s) => screen.push(s),
                        None => {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok && screen.len() >= 3 {
                    out.push(Painted::Poly { pts: screen, color: *color });
                }
            }
            HandleCmd::Label { at, text, size, color } => {
                if let Some(at) = p(*at) {
                    out.push(Painted::Label {
                        at,
                        text: text.clone(),
                        size: *size,
                        color: *color,
                    });
                }
            }
            HandleCmd::Dot { at, px, color } => {
                if let Some(at) = p(*at) {
                    out.push(Painted::Dot { at, px: *px, color: *color });
                }
            }
        }
    }
}

/// Paint projected handles into a viewport rect. `ppp` converts the physical
/// pixels projection works in to egui's logical points.
pub(crate) fn paint(painter: &egui::Painter, items: &[Painted], ppp: f32) {
    let pt = |v: Vec2| egui::pos2(v.x / ppp, v.y / ppp);
    let col = |c: [f32; 4]| {
        let f = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
        egui::Color32::from_rgba_unmultiplied(f(c[0]), f(c[1]), f(c[2]), f(c[3]))
    };
    for item in items {
        match item {
            Painted::Line { a, b, color, width } => {
                painter.line_segment([pt(*a), pt(*b)], egui::Stroke::new(*width, col(*color)));
            }
            Painted::Poly { pts, color } => {
                painter.add(egui::Shape::convex_polygon(
                    pts.iter().map(|v| pt(*v)).collect(),
                    col(*color),
                    egui::Stroke::NONE,
                ));
            }
            Painted::Label { at, text, size, color } => {
                painter.text(
                    pt(*at),
                    egui::Align2::CENTER_CENTER,
                    text,
                    egui::FontId::proportional(*size),
                    col(*color),
                );
            }
            Painted::Dot { at, px, color } => {
                painter.circle_filled(pt(*at), *px, col(*color));
            }
        }
    }
}

/// Pull a world position out of a Lua value: `vec3(...)`, `{x=,y=,z=}` or
/// `{1,2,3}`. Three spellings because all three are what somebody writes.
pub(crate) fn vec3_of(v: &Value) -> mlua::Result<[f64; 3]> {
    // `nav.*` answers in the scripting runtime's own vector, which is userdata
    // rather than a table. Without this arm, `handles.dot(nav.nearest(p))` —
    // the first thing anybody does with a navmesh — raises, and the message
    // blames the caller for passing something that is in fact a position.
    if let Value::UserData(ud) = v
        && let Ok(p) = ud.borrow::<floptle_script::LuaVec3>()
    {
        return Ok([p.0.x, p.0.y, p.0.z]);
    }
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime("expected a position — vec3(x, y, z) or {x=, y=, z=}"));
    };
    let num = |k: &str, i: i64| -> mlua::Result<f64> {
        if let Ok(n) = t.get::<f64>(k) {
            return Ok(n);
        }
        t.get::<f64>(i)
    };
    Ok([num("x", 1)?, num("y", 2)?, num("z", 3)?])
}

/// Per-package drawing state: the colour and width every call inherits.
#[derive(Clone, Copy)]
struct Pen {
    color: [f32; 4],
    width: f32,
}

impl Default for Pen {
    fn default() -> Self {
        // The same green `gizmo.*` starts in, so a handle and a script gizmo
        // side by side do not look like two different systems.
        Self { color: [0.35, 1.0, 0.45, 1.0], width: 1.5 }
    }
}

/// Build the `handles` table. Unlike `gui`, this is not scoped: it queues, and a
/// queue is safe to hold.
pub(crate) fn bind(lua: &Lua, shared: &Rc<Shared>) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let pen = Rc::new(RefCell::new(Pen::default()));

    let push = {
        let shared = shared.clone();
        move |c: HandleCmd| shared.handles.borrow_mut().push(c)
    };
    let push = Rc::new(push);

    {
        let pen = pen.clone();
        t.set(
            "color",
            lua.create_function(move |_, (r, g, b, a): (f32, f32, f32, Option<f32>)| {
                pen.borrow_mut().color = [r, g, b, a.unwrap_or(1.0)];
                Ok(())
            })?,
        )?;
    }
    {
        let pen = pen.clone();
        t.set(
            "width",
            lua.create_function(move |_, px: f32| {
                pen.borrow_mut().width = px.max(0.1);
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "line",
            lua.create_function(move |_, (a, b): (Value, Value)| {
                let p = *pen.borrow();
                push(HandleCmd::Line {
                    a: vec3_of(&a)?,
                    b: vec3_of(&b)?,
                    color: p.color,
                    width: p.width,
                });
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "polyline",
            lua.create_function(move |_, (pts, closed): (Vec<Value>, Option<bool>)| {
                let p = *pen.borrow();
                let vs: mlua::Result<Vec<[f64; 3]>> = pts.iter().map(vec3_of).collect();
                let vs = vs?;
                for w in vs.windows(2) {
                    push(HandleCmd::Line { a: w[0], b: w[1], color: p.color, width: p.width });
                }
                if closed.unwrap_or(false) && vs.len() > 2 {
                    push(HandleCmd::Line {
                        a: vs[vs.len() - 1],
                        b: vs[0],
                        color: p.color,
                        width: p.width,
                    });
                }
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "poly",
            lua.create_function(move |_, pts: Vec<Value>| {
                let p = *pen.borrow();
                let vs: mlua::Result<Vec<[f64; 3]>> = pts.iter().map(vec3_of).collect();
                push(HandleCmd::Poly { pts: vs?, color: p.color });
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "wireCube",
            lua.create_function(move |_, (centre, size): (Value, Value)| {
                let p = *pen.borrow();
                let c = vec3_of(&centre)?;
                let s = vec3_of(&size)?;
                for (a, b) in box_edges(c, s) {
                    push(HandleCmd::Line { a, b, color: p.color, width: p.width });
                }
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "wireSphere",
            lua.create_function(move |_, (centre, radius): (Value, f64)| {
                let p = *pen.borrow();
                let c = vec3_of(&centre)?;
                // Three great circles — the shape reads as a sphere from any
                // angle, which one circle does not.
                for axis in 0..3 {
                    let mut prev = None;
                    for i in 0..=SEGMENTS {
                        let a = i as f64 / SEGMENTS as f64 * std::f64::consts::TAU;
                        let (s, co) = (a.sin() * radius, a.cos() * radius);
                        let v = match axis {
                            0 => [c[0] + co, c[1] + s, c[2]],
                            1 => [c[0] + co, c[1], c[2] + s],
                            _ => [c[0], c[1] + co, c[2] + s],
                        };
                        if let Some(q) = prev {
                            push(HandleCmd::Line { a: q, b: v, color: p.color, width: p.width });
                        }
                        prev = Some(v);
                    }
                }
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "wireDisc",
            lua.create_function(move |_, (centre, normal, radius): (Value, Value, f64)| {
                let p = *pen.borrow();
                let c = vec3_of(&centre)?;
                let n = DVec3::from(vec3_of(&normal)?).normalize_or_zero();
                let n = if n.length_squared() < 0.5 { DVec3::Y } else { n };
                // Any perpendicular will do; picking the one furthest from `n`
                // keeps the cross product well-conditioned for an axis-aligned
                // normal, which is the common case.
                let helper = if n.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
                let u = n.cross(helper).normalize() * radius;
                let v = n.cross(u).normalize() * radius;
                let mut prev = None;
                for i in 0..=SEGMENTS {
                    let a = i as f64 / SEGMENTS as f64 * std::f64::consts::TAU;
                    let q = DVec3::from(c) + u * a.cos() + v * a.sin();
                    let q = [q.x, q.y, q.z];
                    if let Some(r) = prev {
                        push(HandleCmd::Line { a: r, b: q, color: p.color, width: p.width });
                    }
                    prev = Some(q);
                }
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "arrow",
            lua.create_function(move |_, (from, to): (Value, Value)| {
                let p = *pen.borrow();
                let a = DVec3::from(vec3_of(&from)?);
                let b = DVec3::from(vec3_of(&to)?);
                push(HandleCmd::Line {
                    a: [a.x, a.y, a.z],
                    b: [b.x, b.y, b.z],
                    color: p.color,
                    width: p.width,
                });
                let dir = (b - a).normalize_or_zero();
                if dir.length_squared() > 0.5 {
                    let len = (b - a).length() * 0.15;
                    let helper = if dir.y.abs() < 0.9 { DVec3::Y } else { DVec3::X };
                    let side = dir.cross(helper).normalize() * len * 0.5;
                    for s in [side, -side] {
                        let tip = b - dir * len + s;
                        push(HandleCmd::Line {
                            a: [b.x, b.y, b.z],
                            b: [tip.x, tip.y, tip.z],
                            color: p.color,
                            width: p.width,
                        });
                    }
                }
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "dot",
            lua.create_function(move |_, (at, px): (Value, Option<f32>)| {
                let p = *pen.borrow();
                push(HandleCmd::Dot { at: vec3_of(&at)?, px: px.unwrap_or(3.0), color: p.color });
                Ok(())
            })?,
        )?;
    }
    {
        let (pen, push) = (pen.clone(), push.clone());
        t.set(
            "label",
            lua.create_function(move |_, (at, text, size): (Value, String, Option<f32>)| {
                let p = *pen.borrow();
                push(HandleCmd::Label {
                    at: vec3_of(&at)?,
                    text,
                    size: size.unwrap_or(12.0),
                    color: p.color,
                });
                Ok(())
            })?,
        )?;
    }
    Ok(t)
}

/// How many segments a circle is drawn with. Enough that a disc reads as round
/// at the sizes an authoring aid is looked at, and few enough that a hundred of
/// them is still a few thousand lines.
const SEGMENTS: usize = 32;

/// The twelve edges of an axis-aligned box, as world-space pairs. `size` is the
/// FULL extent, not the half-extent — a package author writing `wireCube(p,
/// vec3(1,1,1))` means a one-metre cube.
fn box_edges(c: [f64; 3], size: [f64; 3]) -> Vec<([f64; 3], [f64; 3])> {
    let h = [size[0] * 0.5, size[1] * 0.5, size[2] * 0.5];
    let corner = |i: usize| {
        [
            c[0] + if i & 1 == 0 { -h[0] } else { h[0] },
            c[1] + if i & 2 == 0 { -h[1] } else { h[1] },
            c[2] + if i & 4 == 0 { -h[2] } else { h[2] },
        ]
    };
    let mut out = Vec::with_capacity(12);
    for i in 0..8usize {
        for bit in [1usize, 2, 4] {
            // Each edge once: only walk from the corner where the bit is clear.
            if i & bit == 0 {
                out.push((corner(i), corner(i | bit)));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cube_has_twelve_edges_and_the_size_is_the_full_extent() {
        let e = box_edges([0.0, 0.0, 0.0], [2.0, 2.0, 2.0]);
        assert_eq!(e.len(), 12);
        // Every corner is at ±1, i.e. the box spans 2 units as asked.
        for (a, b) in &e {
            for v in [a, b] {
                for c in v {
                    assert!((c.abs() - 1.0).abs() < 1e-9, "{c}");
                }
            }
        }
        // Each edge changes exactly one axis.
        for (a, b) in &e {
            let diff = (0..3).filter(|i| (a[*i] - b[*i]).abs() > 1e-9).count();
            assert_eq!(diff, 1);
        }
    }

    #[test]
    fn a_cube_is_centred_where_it_is_asked_for() {
        let e = box_edges([5.0, -2.0, 1.0], [2.0, 2.0, 2.0]);
        let xs: Vec<f64> = e.iter().flat_map(|(a, b)| [a[0], b[0]]).collect();
        assert!(xs.iter().all(|x| (*x - 5.0).abs() <= 1.0 + 1e-9));
        assert!(xs.iter().any(|x| (*x - 4.0).abs() < 1e-9));
        assert!(xs.iter().any(|x| (*x - 6.0).abs() < 1e-9));
    }
}
