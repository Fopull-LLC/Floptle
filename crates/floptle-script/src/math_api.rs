//! Lua vector math: the `vec3` / `vec2` value types and the `distance` global.
//!
//! Vectors are small userdata values with real operators — `a + b`, `a - b`,
//! `v * 2`, `-v`, `a == b` — plus the methods games actually reach for
//! (`length`, `normalized`, `dot`, `cross`, `lerp`, `distance`). Everything
//! that ACCEPTS a vector also accepts a plain `{x=, y=, z=}` table or a node
//! handle (anything with numeric x/y/z fields), so `distance(node, target)`
//! just works. LuaJIT-friendly: components are plain doubles, ops allocate
//! one small userdata — fine at gameplay call rates.

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
        lua.create_function(|_, args: mlua::MultiValue| {
            let a: Vec<Value> = args.into_iter().collect();
            match a.len() {
                0 => Ok(LuaVec3(glam::DVec3::ZERO)),
                1 => {
                    if let Some(n) = num_of(&a[0]) {
                        Ok(LuaVec3(glam::DVec3::splat(n)))
                    } else if let Some(v) = vec3_of(&a[0]) {
                        Ok(LuaVec3(v))
                    } else {
                        Err(mlua::Error::RuntimeError(
                            "vec3(number | vector | {x=,y=,z=})".into(),
                        ))
                    }
                }
                3 => match (num_of(&a[0]), num_of(&a[1]), num_of(&a[2])) {
                    (Some(x), Some(y), Some(z)) => Ok(LuaVec3(glam::DVec3::new(x, y, z))),
                    _ => Err(mlua::Error::RuntimeError("vec3(x, y, z) takes numbers".into())),
                },
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

    // rng(seed) — a deterministic random stream: same seed, same sequence, every
    // machine. r:next() [0,1), r:range(a,b), r:int(a,b) inclusive, r:pick(list).
    // (`math.random` stays for throwaway randomness; THIS is for gameplay that
    // must reproduce — loot, procgen scatter, anything a server might replay.)
    lua.globals().set(
        "rng",
        lua.create_function(|lua, seed: f64| {
            let state = std::cell::RefCell::new(floptle_core::noise::Rng::new(seed as u32));
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
            Ok(t)
        })?,
    )?;
    Ok(())
}
