//! How long a script may run before the engine takes the frame back.
//!
//! `while true do end` in an `update` used to freeze the editor, the player
//! and a dedicated server alike, forever, with nothing to say why. Luau calls
//! an *interrupt* at every loop back-edge and function call; this module hangs
//! a deadline on it. The driver arms the deadline when it enters Lua for a
//! pass — every hook, timer and callback in that pass shares it — and the
//! interrupt raises a Lua error once the clock is past it. The error names the
//! budget, the script joins [`ScriptHost`](crate::ScriptHost)'s stopped set,
//! and it is not called again until its file is edited.
//!
//! The clock is read every [`CHECK_EVERY`] interrupts, not every one: a tight
//! loop interrupts tens of millions of times a second, and `Instant::now()`
//! at each would be the slowest thing in it.
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
/// Interrupts between clock reads.
const CHECK_EVERY: u32 = 1024;
/// The marker every budget error carries, so the host can tell one from an
/// ordinary runtime error without parsing the sentence around it.
const MARKER: &str = "ran for more than";

/// The deadline the interrupt watches. One per Lua state, shared with the
/// interrupt closure.
pub struct Budget {
    limit: Cell<Duration>,
    deadline: Cell<Option<Instant>>,
    ticks: Cell<u32>,
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
            self.deadline.set(Some(Instant::now() + self.limit.get()));
        }
        Armed { budget: self.clone(), prev }
    }

    /// The interrupt body: cheap on every call; every [`CHECK_EVERY`] a clock
    /// read against the deadline and a heap read against [`MEMORY_LIMIT`].
    ///
    /// The heap is checked HERE rather than through mlua's allocator limit.
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
            if let Some(d) = self.deadline.get()
                && Instant::now() > d
            {
                return Err(mlua::Error::RuntimeError(self.message()));
            }
            if lua.used_memory() > MEMORY_LIMIT {
                return Err(mlua::Error::MemoryError(memory_message()));
            }
        }
        Ok(mlua::VmState::Continue)
    }

    fn message(&self) -> String {
        let limit = self.limit.get();
        let shown = if limit.as_millis() >= 1000 {
            format!("{} s", limit.as_secs_f64())
        } else {
            format!("{} ms", limit.as_millis())
        };
        format!(
            "{MARKER} {shown} without returning — a loop without an exit? The script is \
             stopped until it is edited."
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
    prev: Option<Instant>,
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
            let d = b.deadline.get().expect("armed");
            {
                let _inner = b.arm();
                assert_eq!(b.deadline.get(), Some(d), "an inner arm moved the deadline");
            }
            assert_eq!(b.deadline.get(), Some(d), "dropping the inner arm disarmed the outer");
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
        std::thread::sleep(Duration::from_millis(2));
        let mut tripped = None;
        for _ in 0..(CHECK_EVERY * 2) {
            if let Err(e) = b.check(&lua) {
                tripped = Some(e.to_string());
                break;
            }
        }
        let e = tripped.expect("an expired deadline never tripped");
        assert!(Budget::is_timeout(&e), "{e}");
        assert!(e.contains("0 ms") && e.contains("stopped until it is edited"), "{e}");
        assert!(!Budget::is_timeout("attempt to index nil"));
    }
}
