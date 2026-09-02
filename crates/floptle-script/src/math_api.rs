//! Lua vector math: the `vec3` / `vec2` value types and the `distance` global.
//!
//! Vectors are small userdata values with real operators — `a + b`, `a - b`,
//! `v * 2`, `-v`, `a == b` — plus the methods games actually reach for
//! (`length` — also spelled `magnitude` — `normalized`, `dot`, `cross`, `lerp`,
//! `distance`). Everything that ACCEPTS a vector also accepts a plain
//! `{x=, y=, z=}` table or a node handle (anything with numeric x/y/z fields),
//! so `distance(node, target)` just works. LuaJIT-friendly: components are
//! plain doubles, ops allocate one small userdata — fine at gameplay call rates.

use mlua::{Lua, MetaMethod, Table, UserData, UserDataFields, UserDataMethods, Value};

/// A 3-component vector (f64 — matches the engine's world coordinates).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LuaVec3(pub glam::DVec3);

/// A 2-component vector (UI/screen math).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LuaVec2(pub glam::DVec2);

/// Read a 3-vector out of a Lua value: a `vec3`, a `vec2` (z = 0), or any
/// table with numeric `x`/`y`(/`z`) fields — which includes NODE HANDLES, so
/// vector APIs accept nodes directly.
pub(crate) fn vec3_of(v: &Value) -> Option<glam::DVec3> {
    match v {
        Value::UserData(ud) => {
            if let Ok(v3) = ud.borrow::<LuaVec3>() {
                return Some(v3.0);
            }
            if let Ok(v2) = ud.borrow::<LuaVec2>() {
                return Some(glam::DVec3::new(v2.0.x, v2.0.y, 0.0));
            }
            None
        }
        Value::Table(t) => {
            let x = t.get::<f64>("x").ok()?;
            let y = t.get::<f64>("y").ok()?;
            let z = t.get::<f64>("z").unwrap_or(0.0);
            Some(glam::DVec3::new(x, y, z))
        }
        _ => None,
    }
}

fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => Some(*n),
        Value::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

impl UserData for LuaVec3 {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("x", |_, v| Ok(v.0.x));
        fields.add_field_method_get("y", |_, v| Ok(v.0.y));
        fields.add_field_method_get("z", |_, v| Ok(v.0.z));
        fields.add_field_method_set("x", |_, v, n: f64| {
            v.0.x = n;
            Ok(())
        });
        fields.add_field_method_set("y", |_, v, n: f64| {
            v.0.y = n;
            Ok(())
        });
        fields.add_field_method_set("z", |_, v, n: f64| {
            v.0.z = n;
            Ok(())
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("length", |_, v, ()| Ok(v.0.length()));
        // `magnitude` is `length` spelled the way most engines spell it. A
        // developer arriving from one of them types the name they already know,
        // and a name that isn't there reads as a missing feature rather than a
        // different word for the same thing — so both are the same call.
        methods.add_method("magnitude", |_, v, ()| Ok(v.0.length()));
        methods.add_method("lengthSquared", |_, v, ()| Ok(v.0.length_squared()));
        methods.add_method("normalized", |_, v, ()| {
            Ok(LuaVec3(v.0.try_normalize().unwrap_or(glam::DVec3::ZERO)))
        });
        methods.add_method("dot", |_, v, o: Value| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("dot takes a vector".into()))?;
            Ok(v.0.dot(o))
        });
        methods.add_method("cross", |_, v, o: Value| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("cross takes a vector".into()))?;
            Ok(LuaVec3(v.0.cross(o)))
        });
        methods.add_method("lerp", |_, v, (o, t): (Value, f64)| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("lerp takes a vector".into()))?;
            Ok(LuaVec3(v.0.lerp(o, t)))
        });
        methods.add_method("distance", |_, v, o: Value| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("distance takes a vector".into()))?;
            Ok(v.0.distance(o))
        });
        // ---- the orientation methods ----------------------------------------
        // `v:flatten(up)` — the part of `v` that lies in the plane ⟂ `up`,
        // renormalised. THE planet-safe move: "forward along the ground" is
        // `node.forward:flatten(node.up)` whatever the local vertical is, and on
        // a flat world `flatten(vec3(0,1,0))` is the familiar "drop the Y".
        // Straight up/down (nothing left after the projection) → vec3(0,0,0),
        // never a NaN — check it with `if f:length() > 0`.
        methods.add_method("flatten", |_, v, up: Option<Value>| {
            let up = match up {
                Some(u) => vec3_of(&u)
                    .ok_or_else(|| mlua::Error::RuntimeError("flatten takes a vector".into()))?,
                None => glam::DVec3::Y,
            };
            Ok(LuaVec3(flatten(v.0, up)))
        });
        // The "same vector but one component" trio — `node.vel:withY(0)` keeps
        // your fall speed out of a horizontal speed clamp.
        methods.add_method("withX", |_, v, n: f64| Ok(LuaVec3(glam::DVec3::new(n, v.0.y, v.0.z))));
        methods.add_method("withY", |_, v, n: f64| Ok(LuaVec3(glam::DVec3::new(v.0.x, n, v.0.z))));
        methods.add_method("withZ", |_, v, n: f64| Ok(LuaVec3(glam::DVec3::new(v.0.x, v.0.y, n))));
        // `v:rotatedY(rad)` — spun about world +Y (the yaw of a flat world).
        methods.add_method("rotatedY", |_, v, rad: f64| {
            let (s, c) = rad.sin_cos();
            Ok(LuaVec3(glam::DVec3::new(
                v.0.x * c + v.0.z * s,
                v.0.y,
                -v.0.x * s + v.0.z * c,
            )))
        });
        // `v:rotatedAround(axis, rad)` — Rodrigues about ANY axis, which is what
        // a planet camera's yaw actually is (about the local up, not about Y).
        methods.add_method("rotatedAround", |_, v, (axis, rad): (Value, f64)| {
            let a = vec3_of(&axis)
                .ok_or_else(|| mlua::Error::RuntimeError("rotatedAround takes a vector".into()))?;
            let Some(a) = a.try_normalize() else { return Ok(LuaVec3(v.0)) };
            let (s, c) = rad.sin_cos();
            Ok(LuaVec3(v.0 * c + a.cross(v.0) * s + a * a.dot(v.0) * (1.0 - c)))
        });
        // `v:towards(other, maxDelta)` — step toward another point without ever
        // overshooting it (`math.approach`, for positions).
        methods.add_method("towards", |_, v, (o, max_delta): (Value, f64)| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("towards takes a vector".into()))?;
            Ok(LuaVec3(towards(v.0, o, max_delta)))
        });
        // `v:angleTo(other)` — the unsigned angle between two directions, in
        // radians. Clamped before the acos, so parallel vectors give 0, not NaN.
        methods.add_method("angleTo", |_, v, o: Value| {
            let o = vec3_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("angleTo takes a vector".into()))?;
            match (v.0.try_normalize(), o.try_normalize()) {
                (Some(a), Some(b)) => Ok(a.dot(b).clamp(-1.0, 1.0).acos()),
                _ => Ok(0.0),
            }
        });
        methods.add_meta_function(MetaMethod::Add, |_, (a, b): (Value, Value)| {
            match (vec3_of(&a), vec3_of(&b)) {
                (Some(a), Some(b)) => Ok(LuaVec3(a + b)),
                _ => Err(mlua::Error::RuntimeError("vec3 + vec3 only".into())),
            }
        });
        methods.add_meta_function(MetaMethod::Sub, |_, (a, b): (Value, Value)| {
            match (vec3_of(&a), vec3_of(&b)) {
                (Some(a), Some(b)) => Ok(LuaVec3(a - b)),
                _ => Err(mlua::Error::RuntimeError("vec3 - vec3 only".into())),
            }
        });
        // `v * 2`, `2 * v`, and component-wise `v * v`.
        methods.add_meta_function(MetaMethod::Mul, |_, (a, b): (Value, Value)| {
            match (vec3_of(&a), num_of(&a), vec3_of(&b), num_of(&b)) {
                (Some(v), _, _, Some(s)) | (_, Some(s), Some(v), _) => Ok(LuaVec3(v * s)),
                (Some(a), _, Some(b), _) => Ok(LuaVec3(a * b)),
                _ => Err(mlua::Error::RuntimeError("vec3 * number or vec3 * vec3".into())),
            }
        });
        methods.add_meta_function(MetaMethod::Div, |_, (a, b): (Value, Value)| {
            match (vec3_of(&a), num_of(&b)) {
                (Some(v), Some(s)) => Ok(LuaVec3(v / s)),
                _ => Err(mlua::Error::RuntimeError("vec3 / number only".into())),
            }
        });
        methods.add_meta_method(MetaMethod::Unm, |_, v, ()| Ok(LuaVec3(-v.0)));
        methods.add_meta_method(MetaMethod::Eq, |_, v, o: Value| {
            Ok(vec3_of(&o).map(|o| v.0 == o).unwrap_or(false))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!("vec3({}, {}, {})", v.0.x, v.0.y, v.0.z))
        });
    }
}

impl UserData for LuaVec2 {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("x", |_, v| Ok(v.0.x));
        fields.add_field_method_get("y", |_, v| Ok(v.0.y));
        fields.add_field_method_set("x", |_, v, n: f64| {
            v.0.x = n;
            Ok(())
        });
        fields.add_field_method_set("y", |_, v, n: f64| {
            v.0.y = n;
            Ok(())
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        fn v2_of(v: &Value) -> Option<glam::DVec2> {
            vec3_of(v).map(|v| glam::DVec2::new(v.x, v.y))
        }
        methods.add_method("length", |_, v, ()| Ok(v.0.length()));
        // The same alias vec3 carries, for the same reason.
        methods.add_method("magnitude", |_, v, ()| Ok(v.0.length()));
        methods.add_method("lengthSquared", |_, v, ()| Ok(v.0.length_squared()));
        methods.add_method("normalized", |_, v, ()| {
            Ok(LuaVec2(v.0.try_normalize().unwrap_or(glam::DVec2::ZERO)))
        });
        methods.add_method("dot", |_, v, o: Value| {
            let o =
                v2_of(&o).ok_or_else(|| mlua::Error::RuntimeError("dot takes a vector".into()))?;
            Ok(v.0.dot(o))
        });
        methods.add_method("lerp", |_, v, (o, t): (Value, f64)| {
            let o =
                v2_of(&o).ok_or_else(|| mlua::Error::RuntimeError("lerp takes a vector".into()))?;
            Ok(LuaVec2(v.0.lerp(o, t)))
        });
        methods.add_method("distance", |_, v, o: Value| {
            let o = v2_of(&o)
                .ok_or_else(|| mlua::Error::RuntimeError("distance takes a vector".into()))?;
            Ok(v.0.distance(o))
        });
        methods.add_meta_function(MetaMethod::Add, |_, (a, b): (Value, Value)| {
            match (v2_of(&a), v2_of(&b)) {
                (Some(a), Some(b)) => Ok(LuaVec2(a + b)),
                _ => Err(mlua::Error::RuntimeError("vec2 + vec2 only".into())),
            }
        });
        methods.add_meta_function(MetaMethod::Sub, |_, (a, b): (Value, Value)| {
            match (v2_of(&a), v2_of(&b)) {
                (Some(a), Some(b)) => Ok(LuaVec2(a - b)),
                _ => Err(mlua::Error::RuntimeError("vec2 - vec2 only".into())),
            }
        });
        methods.add_meta_function(MetaMethod::Mul, |_, (a, b): (Value, Value)| {
            match (v2_of(&a), num_of(&a), v2_of(&b), num_of(&b)) {
                (Some(v), _, _, Some(s)) | (_, Some(s), Some(v), _) => Ok(LuaVec2(v * s)),
                (Some(a), _, Some(b), _) => Ok(LuaVec2(a * b)),
                _ => Err(mlua::Error::RuntimeError("vec2 * number or vec2 * vec2".into())),
            }
        });
        methods.add_meta_function(MetaMethod::Div, |_, (a, b): (Value, Value)| {
            match (v2_of(&a), num_of(&b)) {
                (Some(v), Some(s)) => Ok(LuaVec2(v / s)),
                _ => Err(mlua::Error::RuntimeError("vec2 / number only".into())),
            }
        });
        methods.add_meta_method(MetaMethod::Unm, |_, v, ()| Ok(LuaVec2(-v.0)));
        methods.add_meta_method(MetaMethod::Eq, |_, v, o: Value| {
            Ok(v2_of(&o).map(|o| v.0 == o).unwrap_or(false))
        });
        methods.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!("vec2({}, {})", v.0.x, v.0.y))
        });
    }
}

/// Install `vec3(...)`, `vec2(...)` and `distance(...)` into the globals.
pub(crate) fn install(lua: &Lua) -> mlua::Result<()> {
    // vec3() = zero; vec3(s) = splat; vec3(x, y, z); vec3(other) = copy.
    lua.globals().set(
        "vec3",
        // **The three-number form does not collect its arguments.** This is the
        // most-called function in the whole script API — a game builds thousands
        // of vectors a frame — and `MultiValue::into_iter().collect::<Vec<_>>()`
        // is a heap allocation on every one of them, on top of the Lua-side
        // object. It does not show up in `collectgarbage("count")`, which is why
        // it survived being profiled: it is the host's malloc, not the script's
        // garbage.
        lua.create_function(|_, args: mlua::MultiValue| {
            let n = args.len();
            let mut it = args.into_iter();
            match n {
                0 => Ok(LuaVec3(glam::DVec3::ZERO)),
                1 => {
                    let a = it.next().unwrap_or(Value::Nil);
                    if let Some(n) = num_of(&a) {
                        Ok(LuaVec3(glam::DVec3::splat(n)))
                    } else if let Some(v) = vec3_of(&a) {
                        Ok(LuaVec3(v))
                    } else {
                        Err(mlua::Error::RuntimeError(
                            "vec3(number | vector | {x=,y=,z=})".into(),
                        ))
                    }
                }
                3 => {
                    let (x, y, z) = (
                        it.next().as_ref().and_then(num_of),
                        it.next().as_ref().and_then(num_of),
                        it.next().as_ref().and_then(num_of),
                    );
                    match (x, y, z) {
                        (Some(x), Some(y), Some(z)) => Ok(LuaVec3(glam::DVec3::new(x, y, z))),
                        _ => Err(mlua::Error::RuntimeError(
                            "vec3(x, y, z) takes numbers".into(),
                        )),
                    }
                }
                _ => Err(mlua::Error::RuntimeError(
                    "vec3 takes 0, 1 (splat/copy) or 3 (x, y, z) arguments".into(),
                )),
            }
        })?,
    )?;
    lua.globals().set(
        "vec2",
        lua.create_function(|_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            match a.len() {
                0 => Ok(LuaVec2(glam::DVec2::ZERO)),
                1 => {
                    if let Some(n) = num_of(&a[0]) {
                        Ok(LuaVec2(glam::DVec2::splat(n)))
                    } else if let Some(v) = vec3_of(&a[0]) {
                        Ok(LuaVec2(glam::DVec2::new(v.x, v.y)))
                    } else {
                        Err(mlua::Error::RuntimeError("vec2(number | vector | {x=,y=})".into()))
                    }
                }
                2 => match (num_of(&a[0]), num_of(&a[1])) {
                    (Some(x), Some(y)) => Ok(LuaVec2(glam::DVec2::new(x, y))),
                    _ => Err(mlua::Error::RuntimeError("vec2(x, y) takes numbers".into())),
                },
                _ => Err(mlua::Error::RuntimeError(
                    "vec2 takes 0, 1 (splat/copy) or 2 (x, y) arguments".into(),
                )),
            }
        })?,
    )?;
    // distance(a, b) — vectors, plain {x=,y=,z=} tables, or NODE HANDLES (so
    // `distance(node, target)` reads both nodes' positions directly). Also
    // distance(x1,y1,z1, x2,y2,z2) for raw numbers.
    lua.globals().set(
        "distance",
        lua.create_function(|_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            match a.len() {
                2 => match (vec3_of(&a[0]), vec3_of(&a[1])) {
                    (Some(a), Some(b)) => Ok(a.distance(b)),
                    _ => Err(mlua::Error::RuntimeError(
                        "distance(a, b) takes vectors or nodes (things with x/y/z)".into(),
                    )),
                },
                6 => {
                    let n: Option<Vec<f64>> = a.iter().map(num_of).collect();
                    match n {
                        Some(n) => Ok(glam::DVec3::new(n[0], n[1], n[2])
                            .distance(glam::DVec3::new(n[3], n[4], n[5]))),
                        None => Err(mlua::Error::RuntimeError(
                            "distance(x1,y1,z1, x2,y2,z2) takes numbers".into(),
                        )),
                    }
                }
                _ => Err(mlua::Error::RuntimeError(
                    "distance takes (a, b) or (x1,y1,z1, x2,y2,z2)".into(),
                )),
            }
        })?,
    )?;

    install_math_helpers(lua)?;
    install_table_helpers(lua)?;
    install_direction_helpers(lua)?;

    // ---- color -----------------------------------------------------------
    // A plain `{r, g, b, a}` table (also indexable [1]..[4]) rather than a
    // userdata: it prints, it serialises into a save, it compares, and any
    // `{1, 0, 0}` a project already had lying around is now a colour. Channels
    // are 0..1 to match every other colour in the engine.
    {
        let ctor = lua.create_function(|lua, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            let c: [f32; 4] = match a.len() {
                // color(gray) / color(gray, alpha) — the two-argument form is
                // how you dim something without naming its hue.
                1 | 2 if num_of(&a[0]).is_some() => {
                    let g = num_of(&a[0]).unwrap_or(0.0) as f32;
                    [g, g, g, a.get(1).and_then(num_of).unwrap_or(1.0) as f32]
                }
                // color(other) / color(other, alpha) — copy, optionally with a
                // new alpha. `color(theme.accent, 0.5)` is the common one.
                1 | 2 => {
                    let Value::Table(t) = &a[0] else {
                        return Err(mlua::Error::RuntimeError(
                            "color(gray | color | {r=,g=,b=,a=} [, alpha])".into(),
                        ));
                    };
                    let mut c = crate::api::read_color(t)?;
                    if let Some(al) = a.get(1).and_then(num_of) {
                        c[3] = al as f32;
                    }
                    c
                }
                3 | 4 => {
                    let n: Option<Vec<f64>> = a.iter().map(num_of).collect();
                    let Some(n) = n else {
                        return Err(mlua::Error::RuntimeError(
                            "color(r, g, b [, a]) takes numbers".into(),
                        ));
                    };
                    [
                        n[0] as f32,
                        n[1] as f32,
                        n[2] as f32,
                        n.get(3).copied().unwrap_or(1.0) as f32,
                    ]
                }
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "color(r, g, b [, a]) | color(gray [, a]) | color(other [, a])".into(),
                    ));
                }
            };
            crate::api::new_color(lua, c)
        })?;
        // `color.hex("#ff8800")` / `color.hex("ff8800aa")` — the form a
        // designer pastes out of anywhere else.
        let helpers = lua.create_table()?;
        helpers.set(
            "hex",
            lua.create_function(|lua, s: String| {
                let h = s.trim().trim_start_matches('#');
                let byte = |i: usize| -> Option<f32> {
                    u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|v| v as f32 / 255.0)
                };
                // 6 or 8 digits. A 3-digit shorthand is deliberately refused:
                // silently reading "#f80" as something else would be worse
                // than saying so.
                let c = match h.len() {
                    6 | 8 => [
                        byte(0).unwrap_or(0.0),
                        byte(2).unwrap_or(0.0),
                        byte(4).unwrap_or(0.0),
                        if h.len() == 8 { byte(6).unwrap_or(1.0) } else { 1.0 },
                    ],
                    _ => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "color.hex expects 6 or 8 hex digits, got '{s}'"
                        )));
                    }
                };
                crate::api::new_color(lua, c)
            })?,
        )?;
        // `color.lerp(a, b, t)` — a fade between two colours, per channel.
        helpers.set(
            "lerp",
            lua.create_function(|lua, (a, b, t): (Table, Table, f64)| {
                let (a, b) = (crate::api::read_color(&a)?, crate::api::read_color(&b)?);
                let t = t.clamp(0.0, 1.0) as f32;
                crate::api::new_color(
                    lua,
                    [
                        a[0] + (b[0] - a[0]) * t,
                        a[1] + (b[1] - a[1]) * t,
                        a[2] + (b[2] - a[2]) * t,
                        a[3] + (b[3] - a[3]) * t,
                    ],
                )
            })?,
        )?;
        // Callable table: `color(1, 0, 0)` and `color.hex("#f80")` both work.
        let mt = lua.create_table()?;
        mt.set(
            "__call",
            lua.create_function(move |_, args: mlua::MultiValue| {
                let mut a: Vec<Value> = args.into_iter().collect();
                // Drop the `color` table itself, which `__call` passes as self.
                if !a.is_empty() {
                    a.remove(0);
                }
                ctor.call::<Table>(mlua::MultiValue::from_iter(a))
            })?,
        )?;
        helpers.set_metatable(Some(mt));
        lua.globals().set("color", helpers)?;
    }

    // ---- deterministic noise + RNG (floptle-core::noise — the SAME numbers the
    // Rust generators produce, on every machine; the substrate for replicated
    // procgen and netcode-safe gameplay randomness) ------------------------------

    let math: Table = lua.globals().get("math")?;
    // math.noise(x, y, z [, seed]) — one octave of seeded value noise, ≈ [-1, 1].
    math.set(
        "noise",
        lua.create_function(|_, (x, y, z, seed): (f64, f64, f64, Option<f64>)| {
            let n = floptle_core::noise::Noise::new(seed.unwrap_or(0.0) as u32);
            Ok(n.value(glam::Vec3::new(x as f32, y as f32, z as f32)) as f64)
        })?,
    )?;
    // math.fbm(x, y, z [, octaves [, seed]]) — fractal noise, rotated octaves.
    math.set(
        "fbm",
        lua.create_function(
            |_, (x, y, z, octaves, seed): (f64, f64, f64, Option<f64>, Option<f64>)| {
                let n = floptle_core::noise::Noise::new(seed.unwrap_or(0.0) as u32);
                Ok(n.fbm(
                    glam::Vec3::new(x as f32, y as f32, z as f32),
                    octaves.unwrap_or(4.0).clamp(1.0, 10.0) as u32,
                ) as f64)
            },
        )?,
    )?;

    // rng([seed]) — a deterministic random stream: same seed, same sequence,
    // every machine. r:next() [0,1), r:range(a,b), r:int(a,b) inclusive,
    // r:pick(list). NO seed = seeded from the clock — a fresh stream every
    // call (procgen "surprise me" rolls); print/store r.seed to reproduce.
    // (`math.random` stays for throwaway randomness; THIS is for gameplay
    // that must reproduce — loot, procgen, anything a server might replay.)
    lua.globals().set(
        "rng",
        lua.create_function(|lua, seed: Option<f64>| {
            let seed = seed.map(|s| s as u32).unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
                    .unwrap_or(1)
            });
            let state = std::cell::RefCell::new(floptle_core::noise::Rng::new(seed));
            let t = lua.create_table()?;
            {
                let s = std::rc::Rc::new(state);
                let sc = s.clone();
                t.set(
                    "next",
                    lua.create_function(move |_, _: Value| Ok(sc.borrow_mut().next_f64()))?,
                )?;
                let sc = s.clone();
                t.set(
                    "range",
                    lua.create_function(move |_, (_, a, b): (Value, f64, f64)| {
                        Ok(sc.borrow_mut().range(a, b))
                    })?,
                )?;
                let sc = s.clone();
                t.set(
                    "int",
                    lua.create_function(move |_, (_, a, b): (Value, f64, f64)| {
                        Ok(sc.borrow_mut().int(a as i64, b as i64))
                    })?,
                )?;
                let sc = s.clone();
                t.set(
                    "pick",
                    lua.create_function(move |_, (_, list): (Value, Table)| {
                        let n = list.raw_len();
                        if n == 0 {
                            return Ok(Value::Nil);
                        }
                        let i = sc.borrow_mut().int(1, n as i64);
                        list.raw_get::<Value>(i)
                    })?,
                )?;
            }
            // The actual seed in play — store/print it to reproduce a roll
            // made with the no-seed form.
            t.set("seed", seed)?;
            Ok(t)
        })?,
    )?;
    Ok(())
}

/// The gameplay arithmetic every controller script was writing out by hand,
/// added to the stock `math` table (camelCase, like the rest of the API).
///
/// These exist because the alternative is what the solar scripts actually
/// contained: `math.max(0, math.min(1, x))` for a clamp, a five-line local
/// `norm(x, y, z)`, and a hand-rolled "move towards" that overshoots at low
/// frame rates. One name each, once, correct.
fn install_math_helpers(lua: &Lua) -> mlua::Result<()> {
    let m: Table = lua.globals().get("math")?;

    // clamp / saturate — the two most-written lines in any game script.
    m.set(
        "clamp",
        lua.create_function(|_, (x, lo, hi): (f64, f64, f64)| Ok(x.clamp(lo.min(hi), hi.max(lo))))?,
    )?;
    m.set("saturate", lua.create_function(|_, x: f64| Ok(x.clamp(0.0, 1.0)))?)?;
    m.set("sign", lua.create_function(|_, x: f64| Ok(if x == 0.0 { 0.0 } else { x.signum() }))?)?;
    // round(x [, step]) — `round(x, 0.25)` snaps to quarters (grid placement).
    m.set(
        "round",
        lua.create_function(|_, (x, step): (f64, Option<f64>)| {
            Ok(match step.filter(|s| *s != 0.0) {
                Some(s) => (x / s).round() * s,
                None => x.round(),
            })
        })?,
    )?;
    // lerp is UNCLAMPED (extrapolation is useful); mix() is the clamped twin.
    m.set("lerp", lua.create_function(|_, (a, b, t): (f64, f64, f64)| Ok(a + (b - a) * t))?)?;
    m.set(
        "mix",
        lua.create_function(|_, (a, b, t): (f64, f64, f64)| {
            Ok(a + (b - a) * t.clamp(0.0, 1.0))
        })?,
    )?;
    // Where does `x` sit between a and b (the inverse of lerp)? 0 when a == b,
    // rather than a NaN that quietly poisons everything downstream.
    m.set(
        "inverseLerp",
        lua.create_function(|_, (a, b, x): (f64, f64, f64)| {
            Ok(if (b - a).abs() < f64::EPSILON { 0.0 } else { ((x - a) / (b - a)).clamp(0.0, 1.0) })
        })?,
    )?;
    m.set(
        "remap",
        lua.create_function(|_, (x, a, b, c, d): (f64, f64, f64, f64, f64)| {
            let t = if (b - a).abs() < f64::EPSILON { 0.0 } else { (x - a) / (b - a) };
            Ok(c + (d - c) * t)
        })?,
    )?;
    m.set(
        "smoothstep",
        lua.create_function(|_, (a, b, x): (f64, f64, f64)| {
            let t = if (b - a).abs() < f64::EPSILON { 0.0 } else { ((x - a) / (b - a)).clamp(0.0, 1.0) };
            Ok(t * t * (3.0 - 2.0 * t))
        })?,
    )?;
    // approach(current, target, maxDelta) — frame-rate-correct "move towards"
    // that never overshoots. Pass `rate * dt` as maxDelta.
    m.set(
        "approach",
        lua.create_function(|_, (cur, target, max_delta): (f64, f64, f64)| {
            let d = target - cur;
            let step = max_delta.abs();
            Ok(if d.abs() <= step { target } else { cur + d.signum() * step })
        })?,
    )?;
    // Angles: wrap into (−π, π], and the SHORTEST signed way from a to b — the
    // thing every turret, heading readout and camera yaw needs and every script
    // got subtly wrong across the ±π seam.
    m.set("wrapAngle", lua.create_function(|_, a: f64| Ok(wrap_pi(a)))?)?;
    m.set(
        "deltaAngle",
        lua.create_function(|_, (a, b): (f64, f64)| Ok(wrap_pi(b - a)))?,
    )?;
    // approachAngle — approach() across the seam, for "turn to face" logic.
    m.set(
        "approachAngle",
        lua.create_function(|_, (cur, target, max_delta): (f64, f64, f64)| {
            let d = wrap_pi(target - cur);
            let step = max_delta.abs();
            Ok(if d.abs() <= step { wrap_pi(target) } else { wrap_pi(cur + d.signum() * step) })
        })?,
    )?;
    // pingPong(t, len) — 0 → len → 0 forever (patrols, bobbing, breathing).
    m.set(
        "pingPong",
        lua.create_function(|_, (t, len): (f64, f64)| {
            if len <= 0.0 {
                return Ok(0.0);
            }
            let c = t.rem_euclid(len * 2.0);
            Ok(if c > len { len * 2.0 - c } else { c })
        })?,
    )?;
    Ok(())
}

// ---- the direction primitives, in Rust, once ----------------------------------
//
// Every one of these was written out longhand in the engine's own example
// scripts, and the longhand is where the sign errors live. They are `pub(crate)`
// because the node methods (`node:lookAt`, `node:turnTowards`, `node:moveTowards`
// in `api.rs`) are the same arithmetic applied to a transform.

/// The part of `v` in the plane ⟂ `up`, renormalised. Zero when `v` is parallel
/// to `up` — a direction with nothing left in the plane, not a NaN.
pub(crate) fn flatten(v: glam::DVec3, up: glam::DVec3) -> glam::DVec3 {
    let Some(up) = up.try_normalize() else { return v.try_normalize().unwrap_or(glam::DVec3::ZERO) };
    (v - up * v.dot(up)).try_normalize().unwrap_or(glam::DVec3::ZERO)
}

/// Step from `a` toward `b` by at most `max_delta`, landing exactly on `b`
/// rather than overshooting it (the bug in every hand-rolled version).
pub(crate) fn towards(a: glam::DVec3, b: glam::DVec3, max_delta: f64) -> glam::DVec3 {
    let d = b - a;
    let len = d.length();
    let step = max_delta.abs();
    if len <= step || len < 1e-12 { b } else { a + d / len * step }
}

/// The yaw that points along `dir`. Engine forward is −Z, so this is
/// `atan2(-x, -z)` — the pair of minus signs that four shipped scripts each
/// had to get right on their own.
pub(crate) fn yaw_of(dir: glam::DVec3) -> f64 {
    if dir.x.abs() < 1e-12 && dir.z.abs() < 1e-12 { 0.0 } else { (-dir.x).atan2(-dir.z) }
}

/// The pitch that points along `dir` (positive = looking up).
pub(crate) fn pitch_of(dir: glam::DVec3) -> f64 {
    match dir.try_normalize() {
        Some(d) => d.y.clamp(-1.0, 1.0).asin(),
        None => 0.0,
    }
}

/// The unit direction a node with this yaw and pitch faces.
pub(crate) fn dir_from_yaw(yaw: f64, pitch: f64) -> glam::DVec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    glam::DVec3::new(-sy * cp, sp, -cy * cp)
}

/// The (yaw, pitch, roll) that faces `dir` with `up` overhead — the engine's
/// YXZ Euler order, which is what a node's `yaw`/`pitch`/`roll` fields are.
///
/// Without an `up` the roll is 0. With one, the roll is whatever it takes to put
/// that up over the camera's head — the twenty lines of undo-yaw-then-pitch that
/// keeps a planet camera's horizon level.
pub(crate) fn look_rotation(dir: glam::DVec3, up: Option<glam::DVec3>) -> (f64, f64, f64) {
    let Some(f) = dir.try_normalize() else { return (0.0, 0.0, 0.0) };
    let (yaw, pitch) = (yaw_of(f), pitch_of(f));
    let Some(up) = up else { return (yaw, pitch, 0.0) };
    // The wanted camera-up: `up` made perpendicular to the view direction.
    let w = match (up - f * up.dot(f)).try_normalize() {
        Some(w) => w,
        None => return (yaw, pitch, 0.0),
    };
    // Undo the yaw (about Y), then the pitch (about X); the roll is what is left.
    let (sy, cy) = (-yaw).sin_cos();
    let a = glam::DVec3::new(w.x * cy + w.z * sy, w.y, -w.x * sy + w.z * cy);
    let (sp, cp) = (-pitch).sin_cos();
    let by = a.y * cp - a.z * sp;
    (yaw, pitch, (-a.x).atan2(by))
}

/// Frame-rate-independent exponential ease: `a` moves a `rate`-dependent
/// FRACTION of the remaining distance each second, so 30 fps and 240 fps feel
/// identical. `rate <= 0` snaps.
pub(crate) fn ease_scalar(a: f64, b: f64, rate: f64, dt: f64) -> f64 {
    if rate <= 0.0 { b } else { a + (b - a) * (1.0 - (-rate * dt).exp()) }
}

/// Install `dirTo`, `yawOf`, `pitchOf`, `dirFromYaw`, `lookRotation`, `ease` and
/// `smoothDamp` — the vocabulary of "which way is that, and how do I get there".
fn install_direction_helpers(lua: &Lua) -> mlua::Result<()> {
    let g = lua.globals();

    // dirTo(from, to) — the unit direction from one thing to another. Both
    // arguments may be a vec3, a {x=,y=,z=} table, or a NODE HANDLE, so
    // `dirTo(node, target)` is the whole sentence. Same point twice →
    // vec3(0,0,0), never a NaN that quietly poisons a transform.
    g.set(
        "dirTo",
        lua.create_function(|_, (from, to): (Value, Value)| {
            match (vec3_of(&from), vec3_of(&to)) {
                (Some(a), Some(b)) => {
                    Ok(LuaVec3((b - a).try_normalize().unwrap_or(glam::DVec3::ZERO)))
                }
                _ => Err(mlua::Error::RuntimeError(
                    "dirTo(from, to) takes vectors or nodes (things with x/y/z)".into(),
                )),
            }
        })?,
    )?;
    g.set(
        "yawOf",
        lua.create_function(|_, v: Value| {
            let v = vec3_of(&v)
                .ok_or_else(|| mlua::Error::RuntimeError("yawOf takes a direction".into()))?;
            Ok(yaw_of(v))
        })?,
    )?;
    g.set(
        "pitchOf",
        lua.create_function(|_, v: Value| {
            let v = vec3_of(&v)
                .ok_or_else(|| mlua::Error::RuntimeError("pitchOf takes a direction".into()))?;
            Ok(pitch_of(v))
        })?,
    )?;
    // dirFromYaw(yaw [, pitch]) — the inverse of yawOf/pitchOf. The pitch
    // argument is what a camera needs; without it you get the ground direction.
    g.set(
        "dirFromYaw",
        lua.create_function(|_, (yaw, pitch): (f64, Option<f64>)| {
            Ok(LuaVec3(dir_from_yaw(yaw, pitch.unwrap_or(0.0))))
        })?,
    )?;
    // lookRotation(dir [, up]) -> yaw, pitch, roll — the angles WITHOUT applying
    // them (node:lookAt applies them). Three returns, so
    // `node.yaw, node.pitch, node.roll = lookRotation(f, up)` is one line.
    g.set(
        "lookRotation",
        lua.create_function(|_, (dir, up): (Value, Option<Value>)| {
            let dir = vec3_of(&dir).ok_or_else(|| {
                mlua::Error::RuntimeError("lookRotation takes a direction".into())
            })?;
            let up = match up {
                Some(u) => Some(vec3_of(&u).ok_or_else(|| {
                    mlua::Error::RuntimeError("lookRotation's up is a vector".into())
                })?),
                None => None,
            };
            Ok(look_rotation(dir, up))
        })?,
    )?;
    // ease(a, b, rate, dt) — the frame-rate-independent exponential ease three
    // camera scripts each defined privately. Works on numbers AND vectors, so a
    // follow position and a follow distance ease the same way.
    g.set(
        "ease",
        lua.create_function(|lua, (a, b, rate, dt): (Value, Value, f64, f64)| {
            if let (Some(a), Some(b)) = (num_of(&a), num_of(&b)) {
                return Ok(Value::Number(ease_scalar(a, b, rate, dt)));
            }
            match (vec3_of(&a), vec3_of(&b)) {
                (Some(a), Some(b)) => {
                    let t = if rate <= 0.0 { 1.0 } else { 1.0 - (-rate * dt).exp() };
                    Ok(Value::UserData(lua.create_userdata(LuaVec3(a + (b - a) * t))?))
                }
                _ => Err(mlua::Error::RuntimeError(
                    "ease(a, b, rate, dt) takes two numbers or two vectors".into(),
                )),
            }
        })?,
    )?;
    // smoothDamp(current, target, vel, smoothTime, dt) -> value, vel — the
    // critically-damped spring a camera follow wants: it has MOMENTUM, so it
    // does not stop dead the instant the target does. Lua has no reference
    // parameters, so the velocity comes back as the second return:
    //   camX, camVX = smoothDamp(camX, wantX, camVX, 0.25, dt)
    // Numbers or vectors, like `ease`.
    g.set(
        "smoothDamp",
        lua.create_function(
            |lua, (cur, target, vel, smooth_time, dt): (Value, Value, Value, f64, f64)| {
                let st = smooth_time.max(1e-4);
                // Game Programming Gems 4 §1.10, the stable discrete form.
                let omega = 2.0 / st;
                let x = omega * dt;
                let exp = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
                let step = |cur: f64, target: f64, vel: f64| -> (f64, f64) {
                    let change = cur - target;
                    let temp = (vel + omega * change) * dt;
                    let new_vel = (vel - omega * temp) * exp;
                    let out = target + (change + temp) * exp;
                    // Never overshoot the target from the near side.
                    if (target - cur > 0.0) == (out > target) {
                        (target, (target - cur) / dt.max(1e-9))
                    } else {
                        (out, new_vel)
                    }
                };
                if let (Some(c), Some(t), Some(v)) = (num_of(&cur), num_of(&target), num_of(&vel)) {
                    let (o, nv) = step(c, t, v);
                    return Ok((Value::Number(o), Value::Number(nv)));
                }
                match (vec3_of(&cur), vec3_of(&target), vec3_of(&vel)) {
                    (Some(c), Some(t), Some(v)) => {
                        let (x, vx) = step(c.x, t.x, v.x);
                        let (y, vy) = step(c.y, t.y, v.y);
                        let (z, vz) = step(c.z, t.z, v.z);
                        Ok((
                            Value::UserData(
                                lua.create_userdata(LuaVec3(glam::DVec3::new(x, y, z)))?,
                            ),
                            Value::UserData(
                                lua.create_userdata(LuaVec3(glam::DVec3::new(vx, vy, vz)))?,
                            ),
                        ))
                    }
                    _ => Err(mlua::Error::RuntimeError(
                        "smoothDamp(cur, target, vel, smoothTime, dt) takes numbers or vectors"
                            .into(),
                    )),
                }
            },
        )?,
    )?;
    Ok(())
}

/// Wrap an angle into (−π, π].
fn wrap_pi(a: f64) -> f64 {
    let tau = std::f64::consts::TAU;
    let mut x = (a + std::f64::consts::PI).rem_euclid(tau) - std::f64::consts::PI;
    if x <= -std::f64::consts::PI {
        x += tau;
    }
    x
}

/// List helpers on the stock `table`, so working with a list of things reads as
/// one line instead of a bookkeeping loop. All of them treat the table as an
/// ARRAY (1..n) and return a new table rather than mutating, except `extend`.
fn install_table_helpers(lua: &Lua) -> mlua::Result<()> {
    let t: Table = lua.globals().get("table")?;

    t.set(
        "map",
        lua.create_function(|lua, (list, f): (Table, mlua::Function)| {
            let out = lua.create_table()?;
            for (i, v) in list.sequence_values::<Value>().enumerate() {
                out.raw_set(i + 1, f.call::<Value>((v?, i + 1))?)?;
            }
            Ok(out)
        })?,
    )?;
    t.set(
        "filter",
        lua.create_function(|lua, (list, f): (Table, mlua::Function)| {
            let out = lua.create_table()?;
            let mut n = 0;
            for (i, v) in list.sequence_values::<Value>().enumerate() {
                let v = v?;
                if f.call::<bool>((v.clone(), i + 1))? {
                    n += 1;
                    out.raw_set(n, v)?;
                }
            }
            Ok(out)
        })?,
    )?;
    // find(list, fn) → value, index (nil if none). Takes a PREDICATE, so
    // `table.find(ships, function(s) return s.docked end)` reads as a sentence.
    t.set(
        "find",
        lua.create_function(|_, (list, f): (Table, mlua::Function)| {
            for (i, v) in list.sequence_values::<Value>().enumerate() {
                let v = v?;
                if f.call::<bool>((v.clone(), i + 1))? {
                    return Ok((v, Value::Integer(i as mlua::Integer + 1)));
                }
            }
            Ok((Value::Nil, Value::Nil))
        })?,
    )?;
    // indexOf(list, value) → index or nil (plain equality, no predicate).
    t.set(
        "indexOf",
        lua.create_function(|_, (list, want): (Table, Value)| {
            for (i, v) in list.sequence_values::<Value>().enumerate() {
                if v? == want {
                    return Ok(Value::Integer(i as mlua::Integer + 1));
                }
            }
            Ok(Value::Nil)
        })?,
    )?;
    // count(list [, fn]) — the length, or how many satisfy the predicate. Also
    // counts a KEYED table's entries, which `#t` cannot.
    t.set(
        "count",
        lua.create_function(|_, (list, f): (Table, Option<mlua::Function>)| {
            match f {
                Some(f) => {
                    let mut n = 0i64;
                    for (i, v) in list.sequence_values::<Value>().enumerate() {
                        if f.call::<bool>((v?, i + 1))? {
                            n += 1;
                        }
                    }
                    Ok(n)
                }
                None => {
                    let mut n = 0i64;
                    for pair in list.pairs::<Value, Value>() {
                        pair?;
                        n += 1;
                    }
                    Ok(n)
                }
            }
        })?,
    )?;
    t.set(
        "sum",
        lua.create_function(|_, (list, f): (Table, Option<mlua::Function>)| {
            let mut acc = 0.0;
            for (i, v) in list.sequence_values::<Value>().enumerate() {
                let v = v?;
                acc += match &f {
                    Some(f) => f.call::<f64>((v, i + 1))?,
                    None => num_of(&v).unwrap_or(0.0),
                };
            }
            Ok(acc)
        })?,
    )?;
    // keys(t) — a SORTED key list, so iterating a keyed table is deterministic
    // (raw `pairs` order is hash order, which a replay can't reproduce).
    t.set(
        "keys",
        lua.create_function(|lua, src: Table| {
            let mut keys: Vec<(String, Value)> = Vec::new();
            for pair in src.pairs::<Value, Value>() {
                let (k, _) = pair?;
                let sort_key = match &k {
                    Value::String(s) => format!("s{}", s.to_string_lossy()),
                    Value::Integer(i) => format!("n{i:020}"),
                    Value::Number(n) => format!("n{n:020}"),
                    _ => continue,
                };
                keys.push((sort_key, k));
            }
            keys.sort_by(|a, b| a.0.cmp(&b.0));
            let out = lua.create_table()?;
            for (i, (_, k)) in keys.into_iter().enumerate() {
                out.raw_set(i + 1, k)?;
            }
            Ok(out)
        })?,
    )?;
    t.set(
        "copy",
        lua.create_function(|lua, src: Table| {
            let out = lua.create_table()?;
            for pair in src.pairs::<Value, Value>() {
                let (k, v) = pair?;
                out.raw_set(k, v)?;
            }
            Ok(out)
        })?,
    )?;
    // extend(dst, src) — append src's items onto dst (in place) and return dst.
    t.set(
        "extend",
        lua.create_function(|_, (dst, src): (Table, Table)| {
            let mut n = dst.raw_len();
            for v in src.sequence_values::<Value>() {
                n += 1;
                dst.raw_set(n, v?)?;
            }
            Ok(dst)
        })?,
    )?;
    t.set(
        "reverse",
        lua.create_function(|lua, src: Table| {
            let out = lua.create_table()?;
            let n = src.raw_len();
            for i in 1..=n {
                out.raw_set(n - i + 1, src.raw_get::<Value>(i)?)?;
            }
            Ok(out)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod helper_tests {
    use mlua::Lua;

    fn lua() -> Lua {
        let lua = Lua::new();
        super::install(&lua).expect("install");
        lua
    }

    /// The arithmetic helpers, including the edge cases the hand-written
    /// versions in solar/ got wrong: an overshooting approach, a zero-width
    /// remap dividing by zero, and an angle delta across the ±π seam.
    #[test]
    fn math_helpers_behave_at_the_edges() {
        let lua = lua();
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        assert_eq!(n("return math.clamp(5, 0, 1)"), 1.0);
        assert_eq!(n("return math.clamp(-5, 0, 1)"), 0.0);
        // Reversed bounds must not produce NaN or panic.
        assert_eq!(n("return math.clamp(0.5, 1, 0)"), 0.5);
        assert_eq!(n("return math.saturate(2)"), 1.0);
        assert_eq!(n("return math.sign(-3)"), -1.0);
        assert_eq!(n("return math.sign(0)"), 0.0);
        assert_eq!(n("return math.round(2.5)"), 3.0);
        assert_eq!(n("return math.round(2.34, 0.25)"), 2.25);
        assert_eq!(n("return math.lerp(0, 10, 1.5)"), 15.0, "lerp extrapolates");
        assert_eq!(n("return math.mix(0, 10, 1.5)"), 10.0, "mix clamps");
        assert_eq!(n("return math.inverseLerp(2, 4, 3)"), 0.5);
        assert_eq!(n("return math.inverseLerp(2, 2, 3)"), 0.0, "no divide by zero");
        assert_eq!(n("return math.remap(5, 0, 10, 0, 100)"), 50.0);
        assert_eq!(n("return math.remap(5, 3, 3, 0, 100)"), 0.0, "no divide by zero");
        assert_eq!(n("return math.smoothstep(0, 1, 0.5)"), 0.5);
        // approach never overshoots — the bug in every hand-rolled version.
        assert_eq!(n("return math.approach(0, 1, 10)"), 1.0);
        assert_eq!(n("return math.approach(0, 1, 0.25)"), 0.25);
        assert_eq!(n("return math.approach(1, 0, 0.25)"), 0.75);
        assert_eq!(n("return math.pingPong(3, 2)"), 1.0);
        assert_eq!(n("return math.pingPong(0, 0)"), 0.0);

        // The ±π seam: 350° → 10° is +20°, not −340°.
        let d = n("return math.deltaAngle(math.rad(350), math.rad(10))");
        assert!((d - 20f64.to_radians()).abs() < 1e-9, "deltaAngle crossed the seam wrong: {d}");
        let w = n("return math.wrapAngle(math.pi * 3)");
        assert!((w - std::f64::consts::PI).abs() < 1e-9, "wrapAngle: {w}");
        // 350° stepping 5° toward 10° lands on 355°, which wrapAngle reports as
        // −5° — the same angle, so compare modulo a turn rather than literally.
        let a = n("return math.approachAngle(math.rad(350), math.rad(10), math.rad(5))");
        let off = super::wrap_pi(a - 355f64.to_radians());
        assert!(off.abs() < 1e-9, "approachAngle went the long way: {a} (off by {off})");
    }

    /// The direction vocabulary. Every assertion here is a line that used to be
    /// written out longhand in a shipped example script — including the two
    /// minus signs in `atan2(-x, -z)` that nothing but a test can keep honest.
    #[test]
    fn directions_round_trip_and_never_produce_nan() {
        let lua = lua();
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };
        let s = |src: &str| -> String { lua.load(src).call::<String>(()).expect(src) };

        // Engine forward is −Z: yaw 0 looks down −Z, +90° looks down −X.
        assert_eq!(s("return tostring(dirFromYaw(0))"), "vec3(-0, 0, -1)");
        let y = n("return yawOf(vec3(0, 0, -1))");
        assert!(y.abs() < 1e-12, "yaw of forward should be 0, got {y}");
        let y = n("return yawOf(vec3(-1, 0, 0))");
        assert!((y - std::f64::consts::FRAC_PI_2).abs() < 1e-9, "yaw of −X: {y}");
        // yawOf ∘ dirFromYaw is the identity for every quarter turn.
        for deg in [0.0, 37.0, 90.0, 179.0, -140.0] {
            let r = deg * std::f64::consts::PI / 180.0;
            let back = n(&format!("return yawOf(dirFromYaw({r}))"));
            assert!(super::wrap_pi(back - r).abs() < 1e-9, "yaw round trip at {deg}°: {back}");
        }
        // …and with pitch, both ways.
        let p = n("return pitchOf(dirFromYaw(1.0, 0.3))");
        assert!((p - 0.3).abs() < 1e-9, "pitch round trip: {p}");
        // A zero direction answers 0, not NaN — the whole point of the guards.
        assert_eq!(n("return yawOf(vec3(0, 0, 0))"), 0.0);
        assert_eq!(n("return pitchOf(vec3(0, 0, 0))"), 0.0);
        assert_eq!(s("return tostring(dirTo(vec3(1,2,3), vec3(1,2,3)))"), "vec3(0, 0, 0)");
        // dirTo is unit length and points the right way.
        assert_eq!(s("return tostring(dirTo(vec3(0,0,0), vec3(0,0,-5)))"), "vec3(0, 0, -1)");
        // …and it takes anything with x/y/z, which is what makes `dirTo(node, target)` read.
        assert_eq!(
            s("return tostring(dirTo({x=0,y=0,z=0}, {x=3,y=0,z=0}))"),
            "vec3(1, 0, 0)"
        );
    }

    /// `flatten` is the four-line project-onto-plane that appeared in
    /// `first_person.lua` twice and `planet_camera.lua` twice.
    #[test]
    fn flatten_projects_onto_the_tangent_plane() {
        let lua = lua();
        let s = |src: &str| -> String { lua.load(src).call::<String>(()).expect(src) };
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        // Looking down at 45° on a flat world: the ground direction is due −Z.
        assert_eq!(
            s("return tostring(vec3(0, -1, -1):flatten(vec3(0, 1, 0)))"),
            "vec3(0, 0, -1)"
        );
        // The result is always unit length…
        let l = n("return vec3(0.3, -9, -0.1):flatten(vec3(0,1,0)):length()");
        assert!((l - 1.0).abs() < 1e-12, "flatten renormalises: {l}");
        // …on ANY up, which is what makes it planet-safe.
        let d = n("return vec3(1, 0, 0):flatten(vec3(1, 0, 0):normalized()):length()");
        assert_eq!(d, 0.0, "straight along up leaves nothing in the plane");
        let up = "vec3(0.6, 0.8, 0)";
        let dot = n(&format!("return vec3(1, 0, 0):flatten({up}):dot({up})"));
        assert!(dot.abs() < 1e-12, "flattened is perpendicular to up: {dot}");
        // Default up is +Y, so the flat-world spelling stays short.
        assert_eq!(s("return tostring(vec3(0, 5, -1):flatten())"), "vec3(0, 0, -1)");
    }

    /// `lookRotation` with an `up` is the twenty-line undo-yaw-then-pitch dance
    /// in `planet_camera.lua`. The test that matters: apply the angles and the
    /// node's own up must come back out.
    #[test]
    fn look_rotation_puts_the_given_up_over_the_head() {
        // Rebuild the basis the engine would from (yaw, pitch, roll) and check
        // the local +Y lands on the up we asked for.
        for (dir, up) in [
            (glam::DVec3::new(0.0, 0.0, -1.0), glam::DVec3::new(0.0, 1.0, 0.0)),
            (glam::DVec3::new(1.0, 0.0, 0.0), glam::DVec3::new(0.0, 0.0, 1.0)),
            (glam::DVec3::new(0.3, 0.5, -0.8), glam::DVec3::new(-0.2, 0.9, 0.1)),
        ] {
            let (yaw, pitch, roll) = super::look_rotation(dir, Some(up));
            let q = glam::DQuat::from_euler(glam::EulerRot::YXZ, yaw, pitch, roll);
            let f = q * glam::DVec3::NEG_Z;
            let u = q * glam::DVec3::Y;
            assert!(f.dot(dir.normalize()) > 1.0 - 1e-9, "forward: {f} vs {dir}");
            // The camera up must be the given up with the forward part removed.
            let want = (up - dir.normalize() * up.dot(dir.normalize())).normalize();
            assert!(u.dot(want) > 1.0 - 1e-9, "up: {u} vs {want}");
        }
        // Without an up, the roll is left at zero.
        let (_, _, roll) = super::look_rotation(glam::DVec3::new(1.0, 1.0, 1.0), None);
        assert_eq!(roll, 0.0);
    }

    /// `ease` and `smoothDamp` — the two smoothing shapes, and the properties
    /// that make them safe to reach for: frame-rate independence and no
    /// overshoot.
    #[test]
    fn easing_is_frame_rate_independent_and_never_overshoots() {
        let lua = lua();
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        // One 1-second step and sixty 1/60-second steps land in the same place —
        // the property the private copies in three camera scripts existed for.
        let one = n("return ease(0, 10, 3, 1)");
        let many = n("local v = 0 for _ = 1, 60 do v = ease(v, 10, 3, 1/60) end return v");
        assert!((one - many).abs() < 1e-9, "ease drifted with dt: {one} vs {many}");
        assert_eq!(n("return ease(0, 10, 0, 1)"), 10.0, "rate 0 snaps");
        // Vectors ease component-wise.
        assert_eq!(n("return ease(vec3(0,0,0), vec3(10,0,0), 0, 1).x"), 10.0);
        // smoothDamp arrives, and stays arrived.
        let v = n(
            "local x, v = 0, 0\n\
             for _ = 1, 300 do x, v = smoothDamp(x, 5, v, 0.2, 1/60) end\n\
             return x",
        );
        assert!((v - 5.0).abs() < 1e-6, "smoothDamp should settle on the target: {v}");
        // It has momentum, so it is NOT the same curve as ease — if it were,
        // there would be no reason for both.
        let sd = n("local x, v = 0, 0 x, v = smoothDamp(x, 1, v, 0.2, 1/60) return x");
        assert!(sd > 0.0 && sd < 0.1, "first smoothDamp step is gentle: {sd}");
    }

    /// The small vector methods, each of which replaced a line of arithmetic in
    /// a shipped script.
    #[test]
    fn the_small_vector_methods_do_what_they_say() {
        let lua = lua();
        let s = |src: &str| -> String { lua.load(src).call::<String>(()).expect(src) };
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        assert_eq!(s("return tostring(vec3(1,2,3):withY(0))"), "vec3(1, 0, 3)");
        assert_eq!(s("return tostring(vec3(1,2,3):withX(9))"), "vec3(9, 2, 3)");
        assert_eq!(s("return tostring(vec3(1,2,3):withZ(9))"), "vec3(1, 2, 9)");
        // rotatedY by a quarter turn: −Z becomes −X (the engine's yaw sense).
        let v = n("local v = vec3(0,0,-1):rotatedY(math.pi/2) return v.x");
        assert!((v + 1.0).abs() < 1e-9, "rotatedY quarter turn: x={v}");
        // rotatedAround an arbitrary axis keeps length and honours the angle.
        let l = n("return vec3(1,2,3):rotatedAround(vec3(0.3,0.5,0.8), 1.1):length()");
        assert!((l - (14f64).sqrt()).abs() < 1e-9, "rotation preserves length: {l}");
        // …and about +Y it agrees with rotatedY.
        let d = n(
            "local a = vec3(1,0,-2):rotatedY(0.7)\n\
             local b = vec3(1,0,-2):rotatedAround(vec3(0,1,0), 0.7)\n\
             return a:distance(b)",
        );
        assert!(d < 1e-9, "rotatedY and rotatedAround(+Y) disagree by {d}");
        // towards never overshoots, and lands exactly.
        assert_eq!(s("return tostring(vec3(0,0,0):towards(vec3(10,0,0), 99))"), "vec3(10, 0, 0)");
        assert_eq!(s("return tostring(vec3(0,0,0):towards(vec3(10,0,0), 2.5))"), "vec3(2.5, 0, 0)");
        // angleTo is clamped before the acos: parallel is 0, not NaN.
        assert_eq!(n("return vec3(1,0,0):angleTo(vec3(2,0,0))"), 0.0);
        let a = n("return vec3(1,0,0):angleTo(vec3(0,1,0))");
        assert!((a - std::f64::consts::FRAC_PI_2).abs() < 1e-12, "perpendicular: {a}");
        assert_eq!(n("return vec3(0,0,0):angleTo(vec3(0,1,0))"), 0.0);
    }

    /// `magnitude` is `length` under the name most other engines use for it.
    /// The guard is that the two are the SAME number on BOTH vector types — an
    /// alias that has drifted from the call it aliases is worse than no alias.
    #[test]
    fn magnitude_is_length_on_both_vector_types() {
        let lua = lua();
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        assert_eq!(n("return vec3(3, 4, 12):magnitude()"), 13.0);
        assert_eq!(n("return vec3(3, 4, 12):magnitude()"), n("return vec3(3, 4, 12):length()"));
        assert_eq!(n("return vec2(3, 4):magnitude()"), 5.0);
        assert_eq!(n("return vec2(3, 4):magnitude()"), n("return vec2(3, 4):length()"));
        // A zero vector is zero long, not a NaN.
        assert_eq!(n("return vec3(0, 0, 0):magnitude()"), 0.0);
        assert_eq!(n("return vec2(0, 0):magnitude()"), 0.0);
        // It reads the same off a difference, which is what most calls are.
        assert_eq!(n("return (vec3(1, 0, 0) - vec3(4, 4, 0)):magnitude()"), 5.0);
    }

    /// The list helpers, and the two behaviours worth pinning: `find` takes a
    /// predicate and returns value+index, and `keys` is SORTED (hash order isn't
    /// reproducible, which matters for replays).
    #[test]
    fn table_helpers_read_like_sentences() {
        let lua = lua();
        let s = |src: &str| -> String { lua.load(src).call::<String>(()).expect(src) };
        let n = |src: &str| -> f64 { lua.load(src).call::<f64>(()).expect(src) };

        assert_eq!(
            s("return table.concat(table.map({1,2,3}, function(v) return v * 2 end), ',')"),
            "2,4,6"
        );
        assert_eq!(
            s("return table.concat(table.filter({1,2,3,4}, function(v) return v % 2 == 0 end), ',')"),
            "2,4"
        );
        assert_eq!(
            s("local v, i = table.find({'a','b','c'}, function(x) return x == 'b' end)\n\
               return v .. i"),
            "b2"
        );
        assert_eq!(n("return table.indexOf({'a','b'}, 'b')"), 2.0);
        assert_eq!(n("return table.count({a=1, b=2, c=3})"), 3.0, "counts keyed tables too");
        assert_eq!(n("return table.count({1,2,3,4}, function(v) return v > 2 end)"), 2.0);
        assert_eq!(n("return table.sum({1,2,3})"), 6.0);
        assert_eq!(n("return table.sum({{hp=2},{hp=5}}, function(v) return v.hp end)"), 7.0);
        assert_eq!(s("return table.concat(table.keys({z=1, a=1, m=1}), ',')"), "a,m,z");
        assert_eq!(s("return table.concat(table.reverse({1,2,3}), ',')"), "3,2,1");
        assert_eq!(s("return table.concat(table.extend({1,2}, {3,4}), ',')"), "1,2,3,4");
        assert_eq!(n("local c = table.copy({a=7}) return c.a"), 7.0);
    }
}
