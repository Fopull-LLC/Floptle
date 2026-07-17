//! The Lua scheduler — `after` / `every` / `tween` (roadmap A4).
//!
//! Timers are TICK-driven and deterministic: the clock advances only when the
//! host's global `run_fixed` pass runs, by the constant tick delta. It must
//! NEVER advance in the targeted replay paths (`run_fixed_for`, prediction
//! replays) — those re-run one entity's tick after a net correction, and a
//! scheduler that advanced there would double-fire every pending timer.
//!
//! * `after(seconds, fn) → handle` — fire once. `handle:cancel()` aborts.
//! * `every(seconds, fn) → handle` — fire repeatedly, first after `seconds`.
//!   The cadence is anchored (`fire_at += interval`), so long sessions don't
//!   drift; a stall fires at most once per tick and re-anchors instead of
//!   bursting to catch up.
//! * `tween(seconds, fn[, ease]) → handle` — call `fn(alpha)` every tick with
//!   eased alpha in 0..1, final call guaranteed exactly at 1.0. `ease` is
//!   `"linear"` (default), `"smooth"`, `"in"`, or `"out"`.
//!
//! Callbacks run with no `node` argument — capture what you need as locals.
//! Errors inside a callback log to the Console and kill only that timer.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};

use crate::{LogLevel, ScriptLog};

/// Runaway guard: a script looping `every(0, ...)` should hit a wall, not OOM.
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    After,
    Every,
    Tween,
}

#[derive(Clone, Copy, PartialEq)]
enum Ease {
    Linear,
    Smooth,
    In,
    Out,
}

impl Ease {
    fn apply(self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Ease::Linear => t,
            Ease::Smooth => t * t * (3.0 - 2.0 * t),
            Ease::In => t * t,
            Ease::Out => 1.0 - (1.0 - t) * (1.0 - t),
        }
    }
}

struct Entry {
    id: u64,
    kind: Kind,
    /// `After`/`Every`: next fire time. `Tween`: start time.
    fire_at: f64,
    /// `Every`: period. `Tween`: duration.
    interval: f64,
    func: Function,
    ease: Ease,
    done: bool,
}

#[derive(Default)]
pub(crate) struct SchedState {
    entries: Vec<Entry>,
    next_id: u64,
    /// The scheduler clock, in seconds of GAME TICKS — not wall time.
    now: f64,
}

impl SchedState {
    /// Scene switch / Stop: every pending timer belonged to the old session.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.now = 0.0;
    }
}

fn log_err(logs: &Rc<RefCell<Vec<ScriptLog>>>, what: &str, e: &mlua::Error) {
    logs.borrow_mut().push(ScriptLog {
        level: LogLevel::Error,
        msg: format!("{what}: {e}"),
        source: None,
    });
}

/// Advance the clock one tick and fire what came due. Called from the host's
/// global `run_fixed` ONLY (see module docs), after `sync_scene` — callbacks
/// use node handles — and before the script pass, so a timer's effects are
/// visible to the same tick's `fixedUpdate`s.
pub(crate) fn tick(
    state: &Rc<RefCell<SchedState>>,
    logs: &Rc<RefCell<Vec<ScriptLog>>>,
    dt: f64,
) {
    // Collect due work with the borrow held, run callbacks with it RELEASED —
    // a callback scheduling new timers re-borrows the state.
    enum DueCall {
        Plain(Function),
        Alpha(Function, f64),
    }
    let due: Vec<DueCall> = {
        let mut s = state.borrow_mut();
        s.now += dt;
        let now = s.now;
        let mut due = Vec::new();
        for e in s.entries.iter_mut() {
            match e.kind {
                Kind::After => {
                    if now >= e.fire_at {
                        e.done = true;
                        due.push(DueCall::Plain(e.func.clone()));
                    }
                }
                Kind::Every => {
                    if now >= e.fire_at {
                        due.push(DueCall::Plain(e.func.clone()));
                        e.fire_at += e.interval.max(dt);
                        if e.fire_at <= now {
                            e.fire_at = now + e.interval.max(dt); // stalled: re-anchor
                        }
                    }
                }
                Kind::Tween => {
                    let alpha = if e.interval <= 0.0 {
                        1.0
                    } else {
                        ((now - e.fire_at) / e.interval).clamp(0.0, 1.0)
                    };
                    due.push(DueCall::Alpha(e.func.clone(), e.ease.apply(alpha)));
                    if alpha >= 1.0 {
                        e.done = true;
                    }
                }
            }
        }
        s.entries.retain(|e| !e.done);
        due
    };
    for call in due {
        let r = match call {
            DueCall::Plain(f) => f.call::<()>(()),
            DueCall::Alpha(f, a) => f.call::<()>(a),
        };
        if let Err(e) = r {
            log_err(logs, "scheduler callback", &e);
        }
    }
}

/// Build the `handle` table a scheduling call returns: `handle:cancel()`.
fn make_handle(lua: &Lua, state: &Rc<RefCell<SchedState>>, id: u64) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let st = state.clone();
    t.set(
        "cancel",
        lua.create_function(move |_, _this: Value| {
            let mut s = st.borrow_mut();
            if let Some(e) = s.entries.iter_mut().find(|e| e.id == id) {
                e.done = true;
            }
            Ok(())
        })?,
    )?;
    Ok(t)
}

fn schedule(
    lua: &Lua,
    state: &Rc<RefCell<SchedState>>,
    kind: Kind,
    seconds: f64,
    func: Function,
    ease: Ease,
) -> mlua::Result<Table> {
    let mut s = state.borrow_mut();
    if s.entries.len() >= MAX_ENTRIES {
        return Err(mlua::Error::runtime(format!(
            "scheduler is full ({MAX_ENTRIES} pending timers) — a loop is scheduling without cancelling"
        )));
    }
    s.next_id += 1;
    let id = s.next_id;
    let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
    let now = s.now;
    s.entries.push(Entry {
        id,
        kind,
        fire_at: if kind == Kind::Tween { now } else { now + seconds },
        interval: seconds,
        func,
        ease,
        done: false,
    });
    drop(s);
    make_handle(lua, state, id)
}

pub(crate) fn install_sched_api(lua: &Lua, state: Rc<RefCell<SchedState>>) {
    let g = lua.globals();
    let st = state.clone();
    let after = lua.create_function(move |lua, (seconds, func): (f64, Function)| {
        schedule(lua, &st, Kind::After, seconds, func, Ease::Linear)
    });
    let st = state.clone();
    let every = lua.create_function(move |lua, (seconds, func): (f64, Function)| {
        schedule(lua, &st, Kind::Every, seconds, func, Ease::Linear)
    });
    let st = state.clone();
    let tween = lua.create_function(
        move |lua, (seconds, func, ease): (f64, Function, Option<String>)| {
            let ease = match ease.as_deref() {
                None | Some("linear") => Ease::Linear,
                Some("smooth") => Ease::Smooth,
                Some("in") => Ease::In,
                Some("out") => Ease::Out,
                Some(other) => {
                    return Err(mlua::Error::runtime(format!(
                        "tween: unknown ease \"{other}\" (linear | smooth | in | out)"
                    )))
                }
            };
            schedule(lua, &st, Kind::Tween, seconds, func, ease)
        },
    );
    match (after, every, tween) {
        (Ok(a), Ok(e), Ok(t)) => {
            let _ = g.set("after", a);
            let _ = g.set("every", e);
            let _ = g.set("tween", t);
        }
        _ => eprintln!("[lua] failed to install the scheduler API"),
    }
}
