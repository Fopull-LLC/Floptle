//! How long a script may run before the engine takes the frame back.
//!
//! Without one, `while true do end` in an `update` freezes the editor, the
//! player and a dedicated server alike, with nothing to say why. Luau calls an
//! interrupt at every loop back-edge and function call; this module hangs a
//! deadline on it. The driver arms the deadline when it enters Lua for a
//! pass — every hook, timer and callback in that pass shares it — and the
//! interrupt raises a Lua error once the clock is past it. The error names the
//! budget and the pass is abandoned; the script runs again next frame. Only a
//! script that overruns [`STRIKES`] passes in a row joins
//! [`ScriptHost`](crate::ScriptHost)'s stopped set, and is not called again
//! until its file is edited.
//!
//! **Each script is timed on its own clock.** The driver starts the clock
//! again ([`Budget::restart`]) as it hands the pass to each script's hook,
//! and again after an overrun, so a script is judged by the time it spent
//! itself. One clock shared by the whole pass blamed every script that ran
//! after an overrun: the deadline had already gone by, so each of them
//! raised the budget's error at its first check, and fifteen scripts that
//! had run for microseconds were reported, and struck, for one that looped.
//!
//! **One slow pass does not end a script.** In Play or a shipped game there
//! is no edit to bring a stopped script back, so stopping on the first
//! overrun lost it for the session: a builder holding 500 parts overran once
//! and its game was gone. A pass that overran once and then returns quickly
//! keeps running; a loop without an exit trips every pass and is stopped
//! after [`STRIKES`], at the cost of that many budgets of frozen frames.
//!
//! The clock is read every [`CHECK_EVERY`] interrupts, not every one: a tight
//! loop interrupts tens of millions of times a second, and `Instant::now()`
//! at each would be the slowest thing in it.
//!
//! **The clock is this thread's CPU time, not the wall.** On a machine loaded
//! on every core a pass that does milliseconds of work can take seconds of
//! wall time, waiting its turn for a core, and a wall-clock budget stopped
//! honest scripts for good at whatever line they were on — a game lost the
//! base it was building. CPU time only grows while the script's thread runs,
//! so a loop that never ends still uses it up at full speed, and a busy
//! machine does not. Where a platform cannot say (the browser), the wall is
//! the clock, as before.
//!
//! **Wall time never abandons a pass where CPU time can be read.** A machine
//! that stalls (a disk so busy the engine's pages wait minutes to load, a
//! laptop lid closed mid-frame, a debugger paused on a breakpoint) runs up
//! wall time while the script does nothing, and a wall-clock ceiling abandoned
//! whatever script happened to be running, then stopped it for the session
//! after three. A wall limit could not catch the case it was meant for
//! anyway: the interrupt only runs while Lua runs, so a binding that never
//! returns never reaches it.
//!
//! **The collector's work inside a pass is timed here too.** Luau collects
//! in small steps it takes on its own whenever a script allocates past its
//! debt, and it calls the same interrupt, with a non-negative state, at the
//! start and the end of every step. mlua drops those calls, so the
//! interrupt is wrapped ([`time_gc_steps`]): the wrapper times each step on
//! this thread and hands every other call on to mlua. What an armed pass
//! spent in those steps reaches the frame profile as `gc_ms`. The steps the
//! engine itself paces between frames (`ScriptHost::step_gc`) run outside
//! any pass and are not counted.
//!

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use floptle_core::time::Instant;

/// The editor's and the player's budget per pass.
pub const DEFAULT_BUDGET: Duration = Duration::from_secs(2);
/// A dedicated server's: a tick that takes half a second is already a server
/// nobody can play on.
pub const SERVER_BUDGET: Duration = Duration::from_millis(500);
/// Passes in a row a script may overrun before it is stopped for good. A
/// pass that finishes in time starts the count again.
pub const STRIKES: u32 = 3;
/// Interrupts between clock reads.
const CHECK_EVERY: u32 = 1024;
/// The marker every budget error carries, so the host can tell one from an
/// ordinary runtime error without parsing the sentence around it.
const MARKER: &str = "ran for more than";

/// The deadline the interrupt watches. One per Lua state, shared with the
/// interrupt closure.
pub struct Budget {
    limit: Cell<Duration>,
    /// When the outermost entry armed: the wall then, and this thread's CPU
    /// time then where the platform can say.
    deadline: Cell<Option<(Instant, Option<Duration>)>>,
    ticks: Cell<u32>,
    /// The clock is to start again at the next check: a new script has the
    /// pass, or the last one overran. Read at the next clock read rather than
    /// taken now, so handing over costs a flag, not two clock reads.
    fresh: Cell<bool>,
    /// Outermost entries into Lua so far, so a reader of the interrupt (the
    /// script sampler) can tell one pass from the next.
    entries: Cell<u64>,
    /// This thread's collector time ([`gc_spent`]) when the outermost entry
    /// armed.
    gc_at_arm: Cell<Duration>,
    /// Where a pass's collector time is reported, and what of it is still
    /// waiting because the profile was borrowed when the pass ended.
    gc_report: RefCell<Option<crate::SharedProfile>>,
    gc_pending: Cell<f32>,
}

/// CPU time this thread has used so far, where the platform can say. It
/// lives in `floptle_core::time` because the frame profile reads it too.
pub use floptle_core::time::thread_cpu;

impl Budget {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            limit: Cell::new(DEFAULT_BUDGET),
            deadline: Cell::new(None),
            ticks: Cell::new(0),
            fresh: Cell::new(false),
            entries: Cell::new(0),
            gc_at_arm: Cell::new(Duration::ZERO),
            gc_report: RefCell::new(None),
            gc_pending: Cell::new(0.0),
        })
    }

    pub fn limit(&self) -> Duration {
        self.limit.get()
    }

    pub fn set_limit(&self, limit: Duration) {
        self.limit.set(limit);
    }

    /// Start the clock for one entry into Lua. Nested entries — a hook that
    /// calls into Rust that calls back into Lua — keep the outer deadline: the
    /// budget is per pass, and re-arming from inside would let a loop that
    /// bounces through a Rust binding run forever.
    pub fn arm(self: &Rc<Self>) -> Armed {
        let prev = self.deadline.get();
        if prev.is_none() {
            self.fresh.set(false);
            self.deadline.set(Some((Instant::now(), thread_cpu())));
            self.entries.set(self.entries.get().wrapping_add(1));
            self.gc_at_arm.set(gc_spent());
        }
        Armed { budget: self.clone(), prev }
    }

    /// Start the clock again for the script about to run, within the armed
    /// entry: nothing it had spent before belongs to the next one. Cheap — a
    /// flag the next check reads. Nested calls never come through here; it
    /// is the driver's, at each script it hands the pass to.
    ///
    /// It counts as a new entry for the script sampler too, whose clock
    /// starts again at the script's first line: the engine's own work since
    /// the last sample (stamps, the scene mirror, timers' bookkeeping) is
    /// nobody's line, and it was charged to the first line the next script
    /// ran, the first line of a `fixedUpdate` most visibly.
    pub fn restart(&self) {
        self.fresh.set(true);
        self.entries.set(self.entries.get().wrapping_add(1));
    }

    /// The interrupt body: cheap on every call; every [`CHECK_EVERY`] a clock
    /// read against the deadline and a heap read against [`MEMORY_LIMIT`].
    ///
    /// The heap is checked here rather than through mlua's allocator limit.
    /// That limit works, but the moment one is set mlua stops trusting any
    /// allocation and wraps every push into Lua in a protected call — which
    /// costs about ten kilobytes of Lua heap per scripted node per pass, and
    /// undoes v0.84's allocation work at a stroke. A counter read every
    /// thousand interrupts bounds a script's heap to the limit plus one
    /// allocation, which is the bound that matters.
    pub fn check(&self, lua: &mlua::Lua) -> mlua::Result<mlua::VmState> {
        let t = self.ticks.get().wrapping_add(1);
        self.ticks.set(t);
        if t.is_multiple_of(CHECK_EVERY) {
            if self.fresh.replace(false) && self.deadline.get().is_some() {
                self.deadline.set(Some((Instant::now(), thread_cpu())));
            } else if let Some((wall0, cpu0)) = self.deadline.get() {
                let limit = self.limit.get();
                let wall = wall0.elapsed();
                let cpu = cpu0.zip(thread_cpu()).map(|(a, b)| b.saturating_sub(a));
                let over = match cpu {
                    Some(cpu) => cpu > limit,
                    None => wall > limit,
                };
                if over {
                    // The overrun is this script's alone: whatever runs next
                    // starts with the whole budget.
                    self.fresh.set(true);
                    return Err(mlua::Error::RuntimeError(self.message(wall, cpu)));
                }
            }
            if lua.used_memory() > MEMORY_LIMIT {
                return Err(mlua::Error::MemoryError(memory_message()));
            }
        }
        Ok(mlua::VmState::Continue)
    }

    fn message(&self, wall: Duration, cpu: Option<Duration>) -> String {
        let shown = |d: Duration| {
            if d.as_millis() >= 1000 {
                format!("{:.1} s", d.as_secs_f64())
            } else {
                format!("{} ms", d.as_millis())
            }
        };
        let limit = self.limit.get();
        let (what, spent) = match cpu {
            Some(cpu) => ("of its own CPU time", format!("{} of CPU, {} of wall time", shown(cpu), shown(wall))),
            None => ("", format!("{} of wall time", shown(wall))),
        };
        let what = if what.is_empty() { String::new() } else { format!(" {what}") };
        format!(
            "{MARKER} {}{what} without returning ({spent}) — a loop without an exit?",
            shown(limit)
        )
    }

    /// Is this error the budget's? Matched on the marker, not the whole
    /// sentence, because the host wraps it in the hook's name first.
    pub fn is_timeout(msg: &str) -> bool {
        msg.contains(MARKER)
    }

    /// Entries into Lua so far: outermost ones, and each script the driver
    /// hands a pass to ([`Self::restart`]).
    pub fn entries(&self) -> u64 {
        self.entries.get()
    }

    /// Hang [`check`](Self::check) on the state's interrupt.
    pub fn install(self: &Rc<Self>, lua: &mlua::Lua) {
        let b = self.clone();
        lua.set_interrupt(move |lua| b.check(lua));
        time_gc_steps(lua);
    }

    /// Report the collector's time inside each pass to `profile`
    /// ([`FrameProfile::record_gc`](floptle_core::profile::FrameProfile::record_gc)).
    pub fn report_gc_to(&self, profile: &crate::SharedProfile) {
        *self.gc_report.borrow_mut() = Some(profile.clone());
    }

    /// The outermost pass ended: hand its collector time to the profile.
    fn pass_ended(&self) {
        let spent = gc_spent().saturating_sub(self.gc_at_arm.get());
        let ms = self.gc_pending.get() + spent.as_secs_f32() * 1000.0;
        if ms <= 0.0 {
            return;
        }
        let report = self.gc_report.borrow();
        let Some(profile) = report.as_ref() else { return };
        // A pass that ends while the profile is borrowed keeps its time for
        // the next one rather than losing it.
        match profile.try_borrow_mut() {
            Ok(mut p) => {
                p.record_gc(ms);
                self.gc_pending.set(0.0);
            }
            Err(_) => self.gc_pending.set(ms),
        }
    }
}

thread_local! {
    /// When the collector step in progress on this thread began.
    static GC_STEP_BEGAN: Cell<Option<Instant>> = const { Cell::new(None) };
    /// The wall time this thread has spent in collector steps so far.
    static GC_SPENT: Cell<Duration> = const { Cell::new(Duration::ZERO) };
}

/// The wall time this thread has spent in Luau's collector steps so far,
/// counted by the interrupt [`time_gc_steps`] installs.
pub fn gc_spent() -> Duration {
    GC_SPENT.with(Cell::get)
}

type Interrupt = unsafe extern "C-unwind" fn(*mut mlua::ffi::lua_State, std::ffi::c_int);

/// mlua's own interrupt, which [`gc_interrupt`] hands the script's
/// safepoints on to. One function for every state.
static NEXT_INTERRUPT: std::sync::OnceLock<Interrupt> = std::sync::OnceLock::new();

/// Luau calls the interrupt with `gc >= 0` twice a collector step, at its
/// start and at its end (`luaC_step`), and with `gc < 0` at a script's
/// safepoints. The first are timed here; the rest go to mlua. No script runs
/// inside a step, so a safepoint seen while a step is open means the step
/// never reached its end (an error raised inside it), and the open step is
/// dropped rather than paired with the next one.
unsafe extern "C-unwind" fn gc_interrupt(state: *mut mlua::ffi::lua_State, gc: std::ffi::c_int) {
    if gc >= 0 {
        GC_STEP_BEGAN.with(|began| match began.take() {
            None => began.set(Some(Instant::now())),
            Some(t0) => GC_SPENT.with(|s| s.set(s.get() + t0.elapsed())),
        });
        return;
    }
    GC_STEP_BEGAN.with(|began| {
        if began.get().is_some() {
            began.set(None);
        }
    });
    if let Some(next) = NEXT_INTERRUPT.get() {
        // SAFETY: the same call Luau would have made to mlua's interrupt had
        // this one not been put in front of it.
        unsafe { next(state, gc) }
    }
}

/// Put [`gc_interrupt`] in front of the interrupt mlua installed, so the
/// collector's steps are timed. Call it after every `set_interrupt`, which
/// puts mlua's own back. Does nothing when no interrupt is set, or when the
/// one set is not mlua's.
pub fn time_gc_steps(lua: &mlua::Lua) {
    // SAFETY: `lua_callbacks` returns the state's callback table, valid for
    // the state's life; only its `interrupt` is read and written, from the
    // thread that owns the state.
    let _ = unsafe {
        lua.exec_raw::<()>((), |state| {
            let cb = mlua::ffi::lua_callbacks(state);
            let Some(current) = (*cb).interrupt else { return };
            if std::ptr::fn_addr_eq(current, gc_interrupt as Interrupt) {
                return;
            }
            let next = *NEXT_INTERRUPT.get_or_init(|| current);
            if std::ptr::fn_addr_eq(next, current) {
                (*cb).interrupt = Some(gc_interrupt);
            }
        })
    };
}

/// The armed deadline; dropping it restores what was there before.
pub struct Armed {
    budget: Rc<Budget>,
    prev: Option<(Instant, Option<Duration>)>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.budget.deadline.set(self.prev);
        if self.prev.is_none() {
            self.budget.pass_ended();
        }
    }
}

/// How much memory the scripts may hold between them. A script that keeps
/// every string it ever made hits "not enough memory" in its own call rather
/// than taking the machine down with it.
pub const MEMORY_LIMIT: usize = 512 * 1024 * 1024;

/// The sentence a script sees when it trips [`MEMORY_LIMIT`].
pub fn memory_message() -> String {
    format!(
        "not enough memory — the scripts hold more than {} MB between them. Something is kept \
         forever: a table that only grows, a string built every frame and never let go.",
        MEMORY_LIMIT / (1024 * 1024)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outermost_arm_owns_the_deadline_and_drop_restores_it() {
        let b = Budget::new();
        assert!(b.deadline.get().is_none());
        {
            let _outer = b.arm();
            let d = b.deadline.get().expect("armed").0;
            {
                let _inner = b.arm();
                assert_eq!(b.deadline.get().map(|d| d.0), Some(d), "an inner arm moved the deadline");
            }
            assert_eq!(b.deadline.get().map(|d| d.0), Some(d), "dropping the inner arm disarmed the outer");
        }
        assert!(b.deadline.get().is_none(), "the outer arm did not disarm");
    }

    #[test]
    fn an_unarmed_budget_never_trips_and_an_expired_one_does() {
        let lua = mlua::Lua::new();
        let b = Budget::new();
        for _ in 0..(CHECK_EVERY * 4) {
            assert!(b.check(&lua).is_ok());
        }
        b.set_limit(Duration::ZERO);
        let _armed = b.arm();
        // Spent, not slept: the budget is CPU time where the platform has it.
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(2) {}
        let mut tripped = None;
        for _ in 0..(CHECK_EVERY * 2) {
            if let Err(e) = b.check(&lua) {
                tripped = Some(e.to_string());
                break;
            }
        }
        let e = tripped.expect("an expired deadline never tripped");
        assert!(Budget::is_timeout(&e), "{e}");
        assert!(e.contains("0 ms") && e.contains("without an exit"), "{e}");
        assert!(!Budget::is_timeout("attempt to index nil"));
    }

    /// A pass armed long ago in wall time, with almost no CPU spent since, is
    /// a machine that stalled (a starved disk, a suspended process), not a
    /// runaway script. Abandoning it stopped honest scripts for the session.
    #[test]
    fn a_stalled_machine_does_not_use_up_a_pass() {
        if thread_cpu().is_none() {
            return; // the wall is the only clock here, by design
        }
        let lua = mlua::Lua::new();
        let b = Budget::new();
        b.set_limit(Duration::from_millis(50));
        let _armed = b.arm();
        // Ten minutes of wall time ago (12 000 budgets), as if the process sat stalled.
        let (_, cpu0) = b.deadline.get().expect("armed");
        let long_ago = Instant::now().checked_sub(Duration::from_secs(600)).expect("uptime");
        b.deadline.set(Some((long_ago, cpu0)));
        for _ in 0..(CHECK_EVERY * 4) {
            if let Err(e) = b.check(&lua) {
                panic!("a stall abandoned the pass: {e}");
            }
        }
    }
}
