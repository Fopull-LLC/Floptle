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
//! the clock, as before. A pass that runs [`WALL_CEILING`] times its budget
//! in wall time is stopped too, so a script waiting forever on the engine
//! inside a binding still comes back.
//!

use std::cell::Cell;
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
/// How many budgets of wall time a pass may take, whatever its CPU time.
const WALL_CEILING: u32 = 30;
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
}

/// CPU time this thread has used so far, where the platform can say.
#[cfg(unix)]
pub fn thread_cpu() -> Option<Duration> {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, writable timespec for the duration of the call.
    let ok = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) } == 0;
    ok.then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
}

/// CPU time this thread has used so far, where the platform can say.
#[cfg(windows)]
pub fn thread_cpu() -> Option<Duration> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut c, mut e, mut k, mut u) = (z(), z(), z(), z());
    // SAFETY: the pseudo-handle is always valid for the calling thread, and
    // every out-pointer is a live FILETIME.
    let ok = unsafe { GetThreadTimes(GetCurrentThread(), &mut c, &mut e, &mut k, &mut u) } != 0;
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    // 100 ns units, kernel and user both: a script's own work in a binding
    // counts as its own.
    ok.then(|| Duration::from_nanos((t(k) + t(u)) * 100))
}

/// No per-thread CPU clock here (the browser): the wall is the budget.
#[cfg(not(any(unix, windows)))]
pub fn thread_cpu() -> Option<Duration> {
    None
}

impl Budget {
    pub fn new() -> Rc<Self> {
        Rc::new(Self {
            limit: Cell::new(DEFAULT_BUDGET),
            deadline: Cell::new(None),
            ticks: Cell::new(0),
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
            self.deadline.set(Some((Instant::now(), thread_cpu())));
        }
        Armed { budget: self.clone(), prev }
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
            if let Some((wall0, cpu0)) = self.deadline.get() {
                let limit = self.limit.get();
                let wall = wall0.elapsed();
                let cpu = cpu0.zip(thread_cpu()).map(|(a, b)| b.saturating_sub(a));
                let over = match cpu {
                    Some(cpu) => cpu > limit || wall > limit * WALL_CEILING,
                    None => wall > limit,
                };
                if over {
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

    /// Hang [`check`](Self::check) on the state's interrupt.
    pub fn install(self: &Rc<Self>, lua: &mlua::Lua) {
        let b = self.clone();
        lua.set_interrupt(move |lua| b.check(lua));
    }
}

/// The armed deadline; dropping it restores what was there before.
pub struct Armed {
    budget: Rc<Budget>,
    prev: Option<(Instant, Option<Duration>)>,
}

impl Drop for Armed {
    fn drop(&mut self) {
        self.budget.deadline.set(self.prev);
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
}
