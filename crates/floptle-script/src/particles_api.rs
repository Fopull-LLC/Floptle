//! A budget on live particles, across every effect.
//!
//! ```lua
//! particles.setMax(2000)   -- a settings screen's particle limit
//! particles.setMax(nil)    -- none (the default)
//! print(particles.max())   -- 2000, or nil
//! ```
//!
//! Past the budget every effect is thinned alike: each is born at the share
//! of its rate the budget leaves, a burst keeps at least one, and what the
//! scene still holds over the budget gives up the particles nearest the end
//! of their lives. Dust, rain and sparks each get fewer, and none goes out.
//! The engine reads the budget each frame; it lasts until Stop.

use std::cell::Cell;
use std::rc::Rc;

use mlua::{Lua, Table};

/// The most live particles `particles.setMax` takes.
pub(crate) const MOST: f64 = 10_000_000.0;

pub(crate) fn install(lua: &Lua, budget: Rc<Cell<Option<u32>>>) -> mlua::Result<()> {
    let t: Table = lua.create_table()?;
    let b = budget.clone();
    t.set(
        "setMax",
        lua.create_function(move |_, n: Option<f64>| {
            match n {
                None => b.set(None),
                Some(n) if n.is_finite() && n >= 1.0 => b.set(Some(n.min(MOST) as u32)),
                Some(_) => {
                    return Err(mlua::Error::RuntimeError(
                        "particles.setMax: a number of particles, at least 1, or nil for no budget".into(),
                    ));
                }
            }
            Ok(())
        })?,
    )?;
    t.set("max", lua.create_function(move |_, ()| Ok(budget.get()))?)?;
    lua.globals().set("particles", t)
}
