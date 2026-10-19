//! `weather.*` — the wind a game declares, and asks about.
//!
//! The engine owns the field (`floptle_core::weather`): one function of place
//! and time that particles, props and bodies are all moved by, so a gust that
//! bends the grass is the same gust that carries the smoke. The game decides
//! what blows where: a planet's prevailing wind, a storm cell over a valley,
//! a lull. It reads the answer back with `weather.windAt`, which is the same
//! number every engine system reads.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use floptle_core::weather::{Wind, WindField};
use mlua::{Lua, Table, Value};

use crate::opts::check_keys;

/// The winds scripts have declared, and the clock they blow by.
#[derive(Clone, Default)]
pub(crate) struct WeatherShared {
    pub field: Rc<RefCell<WindField>>,
    /// Game time in seconds, set by the host before scripts run.
    pub time: Rc<Cell<f64>>,
    pub next_id: Rc<Cell<u32>>,
}

/// Every key `weather.addWind` and `weather.setWind` read.
pub(crate) const WIND_KEYS: &[&str] =
    &["velocity", "gust", "gustPeriod", "turbulence", "scale", "parent", "center", "radius", "seed"];

fn num(t: &Table, key: &str) -> Option<f64> {
    t.get::<Option<f64>>(key).ok().flatten()
}

/// Apply the keys an options table names to `w`, leaving the rest as they are.
fn apply(w: &mut Wind, opts: &Table, call: &str) -> mlua::Result<()> {
    check_keys(opts, WIND_KEYS, call)?;
    if let Ok(v) = opts.get::<Value>("velocity")
        && !v.is_nil()
    {
        let Some(v) = crate::math_api::vec3_of(&v) else {
            return Err(mlua::Error::RuntimeError(format!("{call}: `velocity` is a vec3, in metres a second")));
        };
        w.base = v.as_vec3();
    }
    if let Some(v) = num(opts, "gust") {
        w.gust = v.max(0.0) as f32;
    }
    if let Some(v) = num(opts, "gustPeriod") {
        w.gust_period = v.max(0.05) as f32;
    }
    if let Some(v) = num(opts, "turbulence") {
        w.turbulence = v.max(0.0) as f32;
    }
    if let Some(v) = num(opts, "scale") {
        w.scale = v.max(0.01) as f32;
    }
    if let Some(v) = num(opts, "seed") {
        w.seed = v as u32;
    }
    if let Ok(Some(p)) = opts.get::<Option<String>>("parent") {
        w.anchor = (!p.is_empty()).then_some(p);
    }
    let center = opts.get::<Value>("center").ok().as_ref().and_then(crate::math_api::vec3_of);
    match (num(opts, "radius"), center) {
        (Some(r), c) if r > 0.0 => {
            let c = c.or(w.cell.map(|(c, _)| c)).unwrap_or(glam::DVec3::ZERO);
            w.cell = Some((c, r));
        }
        // A radius of 0 makes a cell a wind that blows everywhere again.
        (Some(_), _) => w.cell = None,
        (None, Some(c)) => {
            if let Some((_, r)) = w.cell {
                w.cell = Some((c, r));
            }
        }
        (None, None) => {}
    }
    Ok(())
}

pub(crate) fn install_weather_api(lua: &Lua, shared: WeatherShared) {
    let Ok(t) = lua.create_table() else { return };

    // weather.addWind{ velocity = vec3(4, 0, 0), gust = 0.4, ... } → an id.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, opts: Table| {
            let id = s.next_id.get().wrapping_add(1).max(1);
            let mut w = Wind { id, ..Wind::default() };
            apply(&mut w, &opts, "weather.addWind")?;
            s.next_id.set(id);
            s.field.borrow_mut().winds.push(w);
            Ok(id)
        }) {
            let _ = t.set("addWind", f);
        }
    }

    // weather.setWind(id, { ... }) — change what it names, keep the rest.
    // Returns false when there is no such wind.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, (id, opts): (u32, Table)| {
            let mut field = s.field.borrow_mut();
            let Some(w) = field.winds.iter_mut().find(|w| w.id == id) else { return Ok(false) };
            apply(w, &opts, "weather.setWind")?;
            Ok(true)
        }) {
            let _ = t.set("setWind", f);
        }
    }

    // weather.removeWind(id) → true if there was one.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, id: u32| {
            let mut field = s.field.borrow_mut();
            let before = field.winds.len();
            field.winds.retain(|w| w.id != id);
            Ok(field.winds.len() != before)
        }) {
            let _ = t.set("removeWind", f);
        }
    }

    // weather.windAt(point) → vec3, the whole field there, in world space.
    {
        let s = shared.clone();
        if let Ok(f) = lua.create_function(move |_, p: Value| {
            let Some(p) = crate::math_api::vec3_of(&p) else {
                return Err(mlua::Error::RuntimeError("weather.windAt(point): point is a vec3".into()));
            };
            let v = s.field.borrow().at(p, s.time.get());
            Ok(crate::math_api::LuaVec3(v.as_dvec3()))
        }) {
            let _ = t.set("windAt", f);
        }
    }

    // weather.bolt(from, to [, { seed, branches, jag }]) → the strands of a
    // lightning bolt to draw: { {points={x1,y1,z1, ...}, width=, parent=}, ... },
    // the main channel first. The same seed is the same bolt everywhere.
    if let Ok(f) = lua.create_function(|lua, (from, to, opts): (Value, Value, Option<Table>)| {
        let (Some(from), Some(to)) = (crate::math_api::vec3_of(&from), crate::math_api::vec3_of(&to)) else {
            return Err(mlua::Error::RuntimeError("weather.bolt(from, to [, opts]): from and to are vec3s".into()));
        };
        if let Some(o) = &opts {
            check_keys(o, &["seed", "branches", "jag"], "weather.bolt")?;
        }
        let get = |k: &str| opts.as_ref().and_then(|o| num(o, k));
        let strands = floptle_core::weather::bolt(
            from,
            to,
            get("seed").unwrap_or(1.0) as u32,
            get("branches").unwrap_or(3.0).clamp(0.0, 64.0) as u32,
            get("jag").unwrap_or(0.15) as f32,
        );
        let out = lua.create_table()?;
        for (i, s) in strands.iter().enumerate() {
            let row = lua.create_table()?;
            let pts = lua.create_table_with_capacity(s.points.len() * 3, 0)?;
            for (k, p) in s.points.iter().enumerate() {
                pts.raw_set(k * 3 + 1, p.x)?;
                pts.raw_set(k * 3 + 2, p.y)?;
                pts.raw_set(k * 3 + 3, p.z)?;
            }
            row.set("points", pts)?;
            row.set("width", s.width)?;
            if let Some(p) = s.parent {
                row.set("parent", p + 1)?;
            }
            out.raw_set(i + 1, row)?;
        }
        Ok(out)
    }) {
        let _ = t.set("bolt", f);
    }

    let _ = lua.globals().set("weather", t);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> (Lua, WeatherShared) {
        let lua = Lua::new();
        crate::math_api::install(&lua).unwrap();
        let shared = WeatherShared::default();
        install_weather_api(&lua, shared.clone());
        (lua, shared)
    }

    /// A script's wind is the one the engine sees, and `windAt` reads the
    /// whole field: a storm cell adds to the prevailing wind inside it and is
    /// nothing outside.
    #[test]
    fn a_scripts_wind_is_the_field_the_engine_reads() {
        let (lua, shared) = host();
        let (inside, outside): (Vec<f64>, Vec<f64>) = lua
            .load(
                r#"local prevailing = weather.addWind{ velocity = vec3(3, 0, 0) }
                   local storm = weather.addWind{ velocity = vec3(0, 0, 8), center = vec3(100, 0, 0), radius = 40 }
                   local a = weather.windAt(vec3(100, 0, 0))
                   local b = weather.windAt(vec3(0, 0, 0))
                   return { a.x, a.y, a.z }, { b.x, b.y, b.z }"#,
            )
            .eval()
            .unwrap();
        assert_eq!(inside, vec![3.0, 0.0, 8.0]);
        assert_eq!(outside, vec![3.0, 0.0, 0.0]);
        assert_eq!(shared.field.borrow().winds.len(), 2);
        let engine = shared.field.borrow().at(glam::DVec3::new(100.0, 0.0, 0.0), 0.0);
        assert_eq!(engine, glam::Vec3::new(3.0, 0.0, 8.0), "the engine reads a different field");
    }

    /// `setWind` changes what it names and keeps the rest; `removeWind` takes
    /// the wind away.
    #[test]
    fn a_wind_can_be_changed_and_removed() {
        let (lua, shared) = host();
        let gone: bool = lua
            .load(
                r#"local id = weather.addWind{ velocity = vec3(1, 0, 0), gust = 0.3 }
                   assert(weather.setWind(id, { velocity = vec3(0, 2, 0) }))
                   local v = weather.windAt(vec3(0, 0, 0))
                   assert(v.x == 0 and v.z == 0 and v.y > 1 and v.y < 3, "setWind did not take")
                   assert(not weather.setWind(999, { gust = 1 }))
                   return weather.removeWind(id) and not weather.removeWind(id)"#,
            )
            .eval()
            .unwrap();
        assert!(gone);
        assert!(shared.field.borrow().winds.is_empty());
    }

    /// A bolt reaches Lua as strands to draw: the channel from `from` to `to`
    /// first, its forks naming it as their parent.
    #[test]
    fn a_bolt_reaches_lua_as_strands_to_draw() {
        let (lua, _) = host();
        let (n, ends, parent): (i64, bool, i64) = lua
            .load(
                r#"local b = weather.bolt(vec3(0, 100, 0), vec3(5, 0, 2), { seed = 3, branches = 2 })
                   local p = b[1].points
                   local ends = p[1] == 0 and p[2] == 100 and p[#p - 2] == 5 and p[#p - 1] == 0 and p[#p] == 2
                   return #b, ends, b[2].parent"#,
            )
            .eval()
            .unwrap();
        assert_eq!((n, ends, parent), (3, true, 1));
    }

    /// An option the wind does not read is an error, not a default forever.
    #[test]
    fn a_misspelt_option_is_refused() {
        let (lua, _) = host();
        let err = lua.load(r#"weather.addWind{ velocity = vec3(1, 0, 0), gusts = 0.4 }"#).exec().unwrap_err();
        assert!(err.to_string().contains("gusts"), "{err}");
    }
}
