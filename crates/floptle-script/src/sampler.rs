//! The script sampler: which line of which script a slow pass was spending
//! its time on.
//!
//! `perf.scripts()` says a script cost 559 ms in a frame; it does not say
//! where. Wrapping a script's functions from outside to find out changes
//! what is being measured, and a script cannot sample itself (there is no
//! `debug.sethook` in Luau). So the engine samples, from the interrupt Luau
//! already calls at every function call and loop back-edge for the pass
//! budget ([`crate::budget`]).
//!
//! At every interrupt it reads the clock, and once the interval has passed it
//! notes the running function and line, charged with the time since the
//! sample before: this thread's CPU time where the platform can say, so a
//! moment the OS took the core away lands on nobody, and the wall where it
//! cannot (the browser). Every interrupt, not every few: an engine call's time lands
//! on the first interrupt after it returns, and reading the clock less often
//! put a long call's time on whatever line the loop had reached by then. It
//! makes a tight loop slower while sampling, which is why it is opt-in. An engine call that ran long (a sweep, a
//! terrain query) never interrupts, so its whole time lands on the line that
//! made it, on the first interrupt after it returns: which is where a reader
//! would look. The clock restarts with every new entry into Lua, so the time
//! between two passes is never charged to anybody.
//!
//! Off (the default) it costs one `Cell` read per interrupt.

use std::cell::Cell;
use std::rc::Rc;

use floptle_core::time::Instant;

use crate::SharedProfile;


/// The sampler's state, shared between the interrupt and `perf.sampleScripts`.
pub struct Sampler {
    profile: SharedProfile,
    /// The interval in milliseconds while sampling.
    interval: Cell<Option<f32>>,
    /// The last sample (or the start of the pass): the wall clock then, this
    /// thread's CPU time then where the platform can say, and which entry
    /// into Lua it belongs to.
    last: Cell<Option<(Instant, Option<std::time::Duration>, u64)>>,
}

impl Sampler {
    pub fn new(profile: SharedProfile) -> Rc<Self> {
        Rc::new(Self { profile, interval: Cell::new(None), last: Cell::new(None) })
    }

    /// Start (with an interval in ms) or stop sampling.
    pub fn set(&self, interval_ms: Option<f32>) {
        self.profile.borrow_mut().set_sampling(interval_ms);
        // What the profile accepted: nothing while collection is off.
        self.interval.set(self.profile.borrow().sampling());
        self.last.set(None);
    }

    /// Is the sampler on?
    pub fn on(&self) -> bool {
        self.interval.get().is_some()
    }

    /// Called from the interrupt, inside entry `entry` into Lua.
    pub fn poll(&self, lua: &mlua::Lua, entry: u64) {
        let Some(interval) = self.interval.get() else { return };
        let now = Instant::now();
        let (since, cpu0) = match self.last.get() {
            Some((at, cpu, e)) if e == entry => ((now - at).as_secs_f32() * 1000.0, cpu),
            _ => {
                self.last.set(Some((now, crate::budget::thread_cpu(), entry)));
                return;
            }
        };
        if since < interval {
            return;
        }
        // Charged in this thread's CPU time where the platform has it, so a
        // moment the OS took the core away is not charged to whichever line
        // happened to be next; the wall where it cannot say.
        let cpu = crate::budget::thread_cpu();
        let charge = match (cpu0, cpu) {
            (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f32() * 1000.0,
            _ => since,
        };
        self.last.set(Some((now, cpu, entry)));
        let Some(d) = lua.inspect_stack(0) else { return };
        let src = d.source();
        let script = src.source.as_deref().map(|c| c.trim_start_matches(['@', '='])).unwrap_or("?");
        let names = d.names();
        let func = names.name.as_deref().unwrap_or("?");
        let line = d.curr_line().max(0) as u32;
        let Ok(mut p) = self.profile.try_borrow_mut() else { return };
        if p.sampling().is_none() {
            // Collection was switched off underneath: stop asking the clock.
            drop(p);
            self.interval.set(None);
            return;
        }
        p.record_sample(script, func, line, charge);
    }
}

/// Hang the sampler and the budget together on the state's interrupt, the
/// one Luau has.
pub fn install(lua: &mlua::Lua, budget: &Rc<crate::budget::Budget>, sampler: &Rc<Sampler>) {
    let (b, s) = (budget.clone(), sampler.clone());
    lua.set_interrupt(move |lua| {
        s.poll(lua, b.entries());
        b.check(lua)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A long engine call is charged to the line that made it.** A binding
    /// that runs 5 ms never interrupts, so its time arrives with the first
    /// sample after it; that sample has to land on the calling line, not on
    /// the loop the script goes on to. Four calls from line 2, then a busy
    /// loop on line 4: line 2 carries the calls' 20 ms.
    #[test]
    fn a_long_engine_call_is_charged_to_the_line_that_made_it() {
        let lua = mlua::Lua::new();
        let profile: SharedProfile = Rc::new(std::cell::RefCell::new(Default::default()));
        profile.borrow_mut().enable(true);
        let budget = crate::budget::Budget::new();
        let sampler = Sampler::new(profile.clone());
        install(&lua, &budget, &sampler);
        sampler.set(Some(0.05));
        let spent = Rc::new(Cell::new(0.0f32));
        let s2 = spent.clone();
        let slow = lua
            .create_function(move |_, ()| {
                let t = Instant::now();
                let c0 = crate::budget::thread_cpu();
                while t.elapsed().as_secs_f32() < 0.005 {}
                // In the clock the sampler charges in.
                let ms = match (c0, crate::budget::thread_cpu()) {
                    (Some(a), Some(b)) => b.saturating_sub(a).as_secs_f32() * 1000.0,
                    _ => t.elapsed().as_secs_f32() * 1000.0,
                };
                s2.set(s2.get() + ms);
                Ok(())
            })
            .unwrap();
        lua.globals().set("slowcall", slow).unwrap();
        lua.load(
            "local function work()\n\
               for i = 1, 4 do slowcall() end\n\
               local x = 0\n\
               for i = 1, 200000 do x = x + i end\n\
               return x\n\
             end\n\
             work()\n",
        )
        .set_name("=probe")
        .exec()
        .unwrap();
        profile.borrow_mut().end_frame();
        let samples = profile.borrow_mut().take_samples();
        let line = |n: u32| samples.iter().filter(|s| s.script == "probe" && s.line == n).map(|s| s.ms).sum::<f32>();
        let calls = spent.get();
        assert!(
            line(2) >= calls * 0.9,
            "line 2 made {calls:.2} ms of calls and was charged {:.2}; line 4 got {:.2}: {samples:?}",
            line(2),
            line(4)
        );
        assert!(line(4) > 0.0, "the loop was never sampled: {samples:?}");
        // Off, nothing more is recorded.
        sampler.set(None);
        lua.load("for i = 1, 100000 do end").exec().unwrap();
        profile.borrow_mut().end_frame();
        assert!(profile.borrow_mut().take_samples().is_empty());
    }
}
