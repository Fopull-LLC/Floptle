//! `script.*`: putting a script instance to sleep, and running its tick slower.
//!
//! A level with dozens of NPCs pays for every one of them every tick, including
//! the ones nowhere near the player: the engine calls three hooks per instance
//! per frame, and each hook's own "far away, do nothing" check still costs the
//! call, the node stamp and the early return. Freeflier's 64 enemies cost 1.1 ms
//! a frame doing exactly that while every one of them was dormant.
//!
//! * `script.sleep(opts?)` — stop calling the calling instance's `update`,
//!   `fixedUpdate` and `lateUpdate`. The engine skips it entirely until it is
//!   woken: by `script.wake`, by a collision or trigger event on its node (the
//!   event is delivered, and wakes it), after `opts.seconds`, or once the
//!   active camera is within `opts.wakeWithin` metres.
//! * `script.wake(node, kind?)` — wake one script on a node, or all of them.
//! * `script.asleep(node, kind)` — is it asleep?
//! * `script.setRate(hz, node?, kind?)` — run `fixedUpdate` at `hz` instead of
//!   every tick, with `dt` the time since its last call. `nil` or `0` restores
//!   every tick. Instances are staggered so a crowd set to 10 Hz doesn't all
//!   think on the same tick.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// One sleeping instance: which script on the node, and what wakes it besides
/// `script.wake` and an event.
pub(crate) struct Sleeper {
    pub(crate) kind: String,
    /// Wake at this script time (`time`, seconds).
    pub(crate) until: Option<f64>,
    /// Wake once the active camera is this close, in metres.
    pub(crate) wake_within: Option<f32>,
}

/// One instance whose `fixedUpdate` runs at a lower rate.
pub(crate) struct RateLod {
    pub(crate) kind: String,
    /// Seconds between calls.
    pub(crate) period: f32,
    /// Time banked since the last call.
    pub(crate) acc: f32,
}

/// Every sleeping or rate-limited instance, by node. By node rather than by
/// `(node, kind)` so the per-pass check needs no `String` to look one up.
#[derive(Default)]
pub(crate) struct Sleepers {
    pub(crate) asleep: HashMap<u32, Vec<Sleeper>>,
    pub(crate) rates: HashMap<u32, Vec<RateLod>>,
}

impl Sleepers {
    pub(crate) fn is_asleep(&self, eid: u32, kind: &str) -> bool {
        self.asleep.get(&eid).is_some_and(|v| v.iter().any(|s| s.kind == kind))
    }

    /// Wake `kind` on `eid`, or every script on it when `kind` is `None`.
    pub(crate) fn wake(&mut self, eid: u32, kind: Option<&str>) {
        let Some(v) = self.asleep.get_mut(&eid) else { return };
        match kind {
            Some(k) => v.retain(|s| s.kind != k),
            None => v.clear(),
        }
        if v.is_empty() {
            self.asleep.remove(&eid);
        }
    }

    fn sleep(&mut self, eid: u32, s: Sleeper) {
        let v = self.asleep.entry(eid).or_default();
        v.retain(|o| o.kind != s.kind);
        v.push(s);
    }

    /// Wake whoever's timer ran out or whose camera came close. `camera` is the
    /// active camera's world position, if there is one; `pos` answers a node's.
    pub(crate) fn wake_due(&mut self, time: f64, camera: Option<glam::DVec3>, pos: impl Fn(u32) -> Option<glam::DVec3>) {
        if self.asleep.is_empty() {
            return;
        }
        self.asleep.retain(|&eid, v| {
            v.retain(|s| {
                if s.until.is_some_and(|t| time >= t) {
                    return false;
                }
                if let (Some(r), Some(cam)) = (s.wake_within, camera)
                    && pos(eid).is_some_and(|p| p.distance_squared(cam) <= (r as f64) * (r as f64))
                {
                    return false;
                }
                true
            });
            !v.is_empty()
        });
    }

    /// For a `fixedUpdate` about to run: `Some(dt)` to call it with, or `None`
    /// to skip it this tick. Instances with no rate get `Some(tick_dt)`.
    pub(crate) fn fixed_dt(&mut self, eid: u32, kind: &str, tick_dt: f32) -> Option<f32> {
        let Some(r) = self.rates.get_mut(&eid).and_then(|v| v.iter_mut().find(|r| r.kind == kind)) else {
            return Some(tick_dt);
        };
        r.acc += tick_dt;
        // A hair of slack so a 20 Hz rate on a 60 Hz tick is every third tick,
        // not every fourth when three sixtieths round to just under a twentieth.
        if r.acc + 1e-5 < r.period {
            return None;
        }
        Some(std::mem::take(&mut r.acc))
    }

    fn set_rate(&mut self, eid: u32, kind: &str, hz: f32) {
        let v = self.rates.entry(eid).or_default();
        v.retain(|r| r.kind != kind);
        if hz > 0.0 {
            let period = 1.0 / hz;
            // Staggered by node, deterministically: a crowd set to the same rate
            // spreads its calls over the period instead of spiking one tick.
            let phase = (eid.wrapping_mul(2_654_435_761) >> 8) as f32 / (1u32 << 24) as f32;
            v.push(RateLod { kind: kind.to_string(), period, acc: phase * period });
        }
        if v.is_empty() {
            self.rates.remove(&eid);
        }
    }

    /// Forget one instance that no longer exists.
    pub(crate) fn forget(&mut self, eid: u32, kind: &str) {
        self.wake(eid, Some(kind));
        if let Some(v) = self.rates.get_mut(&eid) {
            v.retain(|r| r.kind != kind);
            if v.is_empty() {
                self.rates.remove(&eid);
            }
        }
    }

    /// Forget the instances on nodes that are gone, or everything.
    pub(crate) fn retain_nodes(&mut self, keep: impl Fn(u32) -> bool) {
        self.asleep.retain(|e, _| keep(*e));
        self.rates.retain(|e, _| keep(*e));
    }
}

/// The keys `script.sleep{…}` reads.
pub(crate) const SLEEP_KEYS: &[&str] = &["seconds", "wakeWithin"];

fn node_id(v: &Value) -> Option<u32> {
    match v {
        Value::Table(t) => t.raw_get::<Option<u32>>("__id").ok().flatten(),
        _ => None,
    }
}

pub(crate) fn install_sleep_api(
    lua: &Lua,
    sleepers: Rc<RefCell<Sleepers>>,
    current: Rc<RefCell<Option<(u32, String)>>>,
    clock: Rc<std::cell::Cell<f64>>,
) -> mlua::Result<()> {
    let t = lua.create_table()?;

    let (s, cur, clk) = (sleepers.clone(), current.clone(), clock.clone());
    t.set(
        "sleep",
        lua.create_function(move |_, opts: Option<Table>| {
            if let Some(o) = &opts {
                crate::opts::check_keys(o, SLEEP_KEYS, "script.sleep")?;
            }
            let Some((eid, kind)) = cur.borrow().clone() else {
                return Err(mlua::Error::runtime(
                    "script.sleep: only a running script can put itself to sleep — call it from a \
                     hook (update, fixedUpdate, start…). To sleep another script, it calls this itself.",
                ));
            };
            let mut until = None;
            let mut wake_within = None;
            if let Some(o) = opts {
                if let Some(secs) = o.get::<Option<f64>>("seconds")? {
                    until = Some(clk.get() + secs.max(0.0));
                }
                if let Some(m) = o.get::<Option<f64>>("wakeWithin")? {
                    wake_within = Some(m.max(0.0) as f32);
                }
            }
            s.borrow_mut().sleep(eid, Sleeper { kind, until, wake_within });
            Ok(())
        })?,
    )?;

    let s = sleepers.clone();
    t.set(
        "wake",
        lua.create_function(move |_, (node, kind): (Value, Option<String>)| {
            let Some(eid) = node_id(&node) else {
                return Err(mlua::Error::runtime("script.wake(node, kind?): the first argument must be a node"));
            };
            s.borrow_mut().wake(eid, kind.as_deref());
            Ok(())
        })?,
    )?;

    let s = sleepers.clone();
    t.set(
        "asleep",
        lua.create_function(move |_, (node, kind): (Value, String)| {
            let Some(eid) = node_id(&node) else {
                return Err(mlua::Error::runtime("script.asleep(node, kind): the first argument must be a node"));
            };
            Ok(s.borrow().is_asleep(eid, &kind))
        })?,
    )?;

    let (s, cur) = (sleepers, current);
    t.set(
        "setRate",
        lua.create_function(move |_, (hz, node, kind): (Option<f64>, Value, Option<String>)| {
            let target = match (node_id(&node), kind) {
                (Some(eid), Some(k)) => (eid, k),
                (None, None) if matches!(node, Value::Nil) => match cur.borrow().clone() {
                    Some(c) => c,
                    None => {
                        return Err(mlua::Error::runtime(
                            "script.setRate(hz): with no node, only a running script can set its own rate",
                        ));
                    }
                },
                _ => {
                    return Err(mlua::Error::runtime(
                        "script.setRate(hz, node, kind): name both the node and the script, or neither",
                    ));
                }
            };
            let hz = hz.unwrap_or(0.0);
            if !hz.is_finite() || hz < 0.0 {
                return Err(mlua::Error::runtime(format!("script.setRate: {hz} is not a rate in Hz")));
            }
            s.borrow_mut().set_rate(target.0, &target.1, hz as f32);
            Ok(())
        })?,
    )?;

    lua.globals().set("script", t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_calls_every_nth_tick_with_the_time_banked_and_crowds_are_staggered() {
        let mut s = Sleepers::default();
        let dt = 1.0 / 60.0;
        s.set_rate(7, "enemy", 20.0);
        // Run long enough to be past the stagger, then count.
        let calls: Vec<f32> = (0..120).filter_map(|_| s.fixed_dt(7, "enemy", dt)).collect();
        assert!((39..=41).contains(&calls.len()), "{} calls in 2 s at 20 Hz", calls.len());
        assert!(calls[1..].iter().all(|d| (d - 3.0 * dt).abs() < 1e-4), "{calls:?}");
        assert_eq!(s.fixed_dt(7, "other", dt), Some(dt), "a script with no rate runs every tick");

        // Many nodes at one rate don't all land on the same tick.
        for eid in 0..30 {
            s.set_rate(eid, "enemy", 10.0);
        }
        let first: Vec<usize> = (0..30u32)
            .map(|eid| (0..10).position(|_| s.fixed_dt(eid, "enemy", dt).is_some()).unwrap())
            .collect();
        let spread = first.iter().collect::<std::collections::HashSet<_>>().len();
        assert!(spread >= 4, "30 nodes at 10 Hz all start on {spread} tick(s): {first:?}");

        s.set_rate(7, "enemy", 0.0);
        assert_eq!(s.fixed_dt(7, "enemy", dt), Some(dt), "rate 0 is every tick again");
    }

    #[test]
    fn a_sleeper_wakes_on_its_timer_or_when_the_camera_comes_close() {
        let mut s = Sleepers::default();
        s.sleep(1, Sleeper { kind: "a".into(), until: Some(5.0), wake_within: None });
        s.sleep(2, Sleeper { kind: "a".into(), until: None, wake_within: Some(10.0) });
        s.sleep(2, Sleeper { kind: "b".into(), until: None, wake_within: None });
        let at = |e: u32| Some(glam::DVec3::new(e as f64 * 100.0, 0.0, 0.0));
        s.wake_due(4.0, Some(glam::DVec3::ZERO), at);
        assert!(s.is_asleep(1, "a") && s.is_asleep(2, "a"));
        s.wake_due(5.0, Some(glam::DVec3::new(195.0, 0.0, 0.0)), at);
        assert!(!s.is_asleep(1, "a"), "the timer ran out");
        assert!(!s.is_asleep(2, "a"), "the camera came within 10 m");
        assert!(s.is_asleep(2, "b"), "only the sleeper that asked for it woke");
        s.wake(2, None);
        assert!(s.asleep.is_empty());
    }
}
