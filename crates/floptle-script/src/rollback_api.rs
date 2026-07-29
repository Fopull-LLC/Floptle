//! Script state capture for rollback (`docs/rollback-netcode-design.md` §2.3, §5).
//!
//! A rollback restores a confirmed tick and re-simulates every tick since. For
//! that to be correct, everything the simulation can *read* has to come back —
//! including the parts that live in Lua. A script opts in by defining two hooks:
//!
//! ```lua
//! function snapshot()      return { hp = hp, meter = meter, frame = frame } end
//! function restore(s)      hp, meter, frame = s.hp, s.meter, s.frame end
//! ```
//!
//! Everything else about the script stays exactly as it is: a script that
//! defines neither is simply not rolled back, which is right for cosmetics and
//! wrong for gameplay. That is the whole contract, and it belongs in the docs
//! in those words.
//!
//! ## The engine owns the copy
//!
//! The captured value is converted to an owned [`NetValue`] tree on the way out
//! and rebuilt as a fresh Lua table on the way in. That is a deep copy in both
//! directions, by construction — which matters more than it sounds:
//!
//! A rollback restores a tick and then **re-simulates from it, mutating whatever
//! it was handed**. A snapshot that shared its tables with the live sim would be
//! corrupted by the first replay and wrong for every replay after it, and the
//! symptom would be a desync that only appears under packet loss. Making the
//! engine own the copy means a script cannot get this wrong, rather than merely
//! being told not to.
//!
//! It also fixes what may live in rollback state: scalars, strings and nested
//! tables. A function, a coroutine or a node handle in a snapshot is refused
//! with a Console error naming the script — those cannot be meaningfully
//! restored anyway, and silently dropping them would produce a state that looks
//! restored and isn't.

use floptle_net::NetValue;

/// Depth ceiling for a captured state tree. Far beyond any sane controller's
/// state table, and low enough that a cyclic table is refused rather than
/// recursing until the stack goes.
pub const MAX_STATE_DEPTH: usize = 16;

/// One entity's rollback state: per script kind, whatever its `snapshot()`
/// returned. Owned and `Clone`, so the driver's state ring is a plain `VecDeque`
/// with no Lua registry lifetime to manage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ScriptState {
    /// `(script kind, captured value)`, in the order the instances ran.
    ///
    /// **Everything here is hashed**, so everything here is something two peers
    /// must agree about bit for bit.
    pub entries: Vec<(String, NetValue)>,
    /// The **cosmetic** half: restored on rollback, never hashed.
    ///
    /// A correction must put back everything the replay needs to reproduce, but
    /// the checksum should only fire on divergence the *simulation* can feel.
    /// Those are different sets, and `snapshot()` used to be all-or-nothing —
    /// so presentation state smuggled itself into the checksum through the only
    /// door available.
    ///
    /// That cost a cross-platform match: a model's turn-toward-the-opponent
    /// angle, smoothed with `math.exp`, which is library code and is not
    /// required to agree between glibc and Windows' UCRT. One ULP a tick, and a
    /// match that both players could see was identical voided itself every few
    /// seconds. The alarm was working perfectly and firing on something neither
    /// simulation could feel. floptle/0045.
    pub cosmetic: Vec<(String, NetValue)>,
}

impl ScriptState {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Roughly how much this occupies — the rollback health readout reports the
    /// ring's total so "why is this using memory" has an answer.
    pub fn size_hint(&self) -> usize {
        fn v(n: &NetValue) -> usize {
            match n {
                NetValue::Str(s) => s.len() + 8,
                NetValue::Table(p) => p.iter().map(|(k, val)| v(k) + v(val)).sum::<usize>() + 16,
                _ => 8,
            }
        }
        self.entries.iter().chain(self.cosmetic.iter()).map(|(k, val)| k.len() + v(val)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_hint_grows_with_the_tree() {
        let small = ScriptState { entries: vec![("a".into(), NetValue::Num(1.0))], cosmetic: Vec::new() };
        let big = ScriptState {
            cosmetic: Vec::new(),
            entries: vec![(
                "a".into(),
                NetValue::Table(vec![
                    (NetValue::Str("hp".into()), NetValue::Num(100.0)),
                    (NetValue::Str("meter".into()), NetValue::Num(50.0)),
                ]),
            )],
        };
        assert!(big.size_hint() > small.size_hint());
        assert_eq!(ScriptState::default().size_hint(), 0);
        assert!(ScriptState::default().is_empty());
    }
}
