//! `NetValue` — the Lua-compatible value tree that crosses the wire (RPC args
//! and `synced` script vars), with the §13.2 guardrails from
//! `docs/netcode-design.md` enforced at construction: scalars + nested tables
//! only, **depth ≤ 4**, **≤ 1 KB encoded per value**. Functions/userdata never
//! convert — the scripting layer rejects them with a Console error before a
//! `NetValue` exists.

use serde::{Deserialize, Serialize};

/// Maximum nesting depth for table values (a bare scalar is depth 0).
pub const MAX_VALUE_DEPTH: usize = 4;
/// Maximum encoded size of one value, bytes.
pub const MAX_VALUE_BYTES: usize = 1024;

/// A replicable Lua value. Tables are ordered key→value pairs (arrays use
/// 1-based integer keys, Lua-style); order is preserved so encoding is
/// deterministic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum NetValue {
    Nil,
    Bool(bool),
    Num(f64),
    Str(String),
    Table(Vec<(NetValue, NetValue)>),
}

/// Why a value can't replicate (surfaced to the Console by the script layer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueError {
    /// Nested deeper than [`MAX_VALUE_DEPTH`].
    TooDeep,
    /// Encodes larger than [`MAX_VALUE_BYTES`].
    TooBig(usize),
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueError::TooDeep => {
                write!(f, "replicated value nests deeper than {MAX_VALUE_DEPTH} levels")
            }
            ValueError::TooBig(n) => write!(
                f,
                "replicated value encodes to {n} bytes (limit {MAX_VALUE_BYTES}) — writes this large are dropped, not truncated"
            ),
        }
    }
}

impl NetValue {
    /// Validate the guardrails: depth and encoded size. Call after building a
    /// value from Lua and BEFORE queuing it — an invalid value is dropped whole
    /// (never silently truncated).
    pub fn validate(&self) -> Result<(), ValueError> {
        if self.depth() > MAX_VALUE_DEPTH {
            return Err(ValueError::TooDeep);
        }
        let n = postcard::to_allocvec(self).map(|v| v.len()).unwrap_or(usize::MAX);
        if n > MAX_VALUE_BYTES {
            return Err(ValueError::TooBig(n));
        }
        Ok(())
    }

    /// Nesting depth: scalars are 0, a table is 1 + its deepest child.
    pub fn depth(&self) -> usize {
        match self {
            NetValue::Table(pairs) => {
                1 + pairs.iter().map(|(k, v)| k.depth().max(v.depth())).max().unwrap_or(0)
            }
            _ => 0,
        }
    }

    /// An order-independent fingerprint — the rollback desync checksum
    /// (`docs/rollback-netcode-design.md` §6), FNV-1a like
    /// [`floptle_input::InputMap::hash`].
    ///
    /// **The canonicalization trap, which is why this exists at all.** A
    /// `NetValue` built from Lua is built by iterating a table, and Lua's
    /// `pairs()` order is not deterministic — not across machines, and not even
    /// across two rebuilds of the same table on one machine. Restore doesn't
    /// care about that; a checksum does. Hashing the encoding as-is would have
    /// two peers in *perfect agreement* report a desync, which is worse than no
    /// checksum at all: it would train everyone to ignore the alarm.
    ///
    /// So a table's pairs are sorted by their key's canonical form before
    /// hashing, at every level. `f64` is hashed by bits, with the two zeros
    /// folded together (`-0.0 == 0.0` in the simulation, so they must not
    /// disagree here) and every NaN folded to one pattern.
    pub fn canonical_hash(&self) -> u64 {
        let mut h = Fnv::new();
        self.hash_into(&mut h);
        h.0
    }

    fn hash_into(&self, h: &mut Fnv) {
        match self {
            NetValue::Nil => h.eat(b"n"),
            NetValue::Bool(b) => {
                h.eat(b"b");
                h.eat(&[*b as u8]);
            }
            NetValue::Num(n) => {
                h.eat(b"f");
                // −0.0 and 0.0 are the same number to every comparison the
                // simulation makes; a bit-for-bit hash would call two agreeing
                // peers desynced over a sign nobody can observe. NaN likewise
                // has 2^52 spellings and one meaning.
                let bits = if *n == 0.0 {
                    0
                } else if n.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    n.to_bits()
                };
                h.eat(&bits.to_le_bytes());
            }
            NetValue::Str(s) => {
                h.eat(b"s");
                h.eat(&(s.len() as u64).to_le_bytes());
                h.eat(s.as_bytes());
            }
            NetValue::Table(pairs) => {
                h.eat(b"t");
                let mut sorted: Vec<(u64, &NetValue, &NetValue)> =
                    pairs.iter().map(|(k, v)| (k.canonical_hash(), k, v)).collect();
                // By hashed key, with the key itself as the tie-break so two
                // colliding keys still order the same way on both peers.
                sorted.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| cmp_value(a.1, b.1)));
                h.eat(&(sorted.len() as u64).to_le_bytes());
                for (_, k, v) in sorted {
                    k.hash_into(h);
                    v.hash_into(h);
                }
            }
        }
    }
}

/// A total order over values, for breaking checksum ties deterministically.
fn cmp_value(a: &NetValue, b: &NetValue) -> std::cmp::Ordering {
    fn tag(v: &NetValue) -> u8 {
        match v {
            NetValue::Nil => 0,
            NetValue::Bool(_) => 1,
            NetValue::Num(_) => 2,
            NetValue::Str(_) => 3,
            NetValue::Table(_) => 4,
        }
    }
    tag(a).cmp(&tag(b)).then_with(|| match (a, b) {
        (NetValue::Bool(x), NetValue::Bool(y)) => x.cmp(y),
        (NetValue::Num(x), NetValue::Num(y)) => x.total_cmp(y),
        (NetValue::Str(x), NetValue::Str(y)) => x.cmp(y),
        (NetValue::Table(x), NetValue::Table(y)) => x.len().cmp(&y.len()),
        _ => std::cmp::Ordering::Equal,
    })
}

/// FNV-1a, spelled out so no dependency (and no hasher-version drift) can
/// change a checksum between builds — the same reasoning as
/// `floptle_input::InputMap::hash`. Public because the rollback driver hashes
/// physics snapshots into the same digest as the script state (§6), and those
/// types live outside this crate.
pub struct Fnv(pub u64);

impl Default for Fnv {
    fn default() -> Self {
        Self::new()
    }
}

impl Fnv {
    pub fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    pub fn eat(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(levels: usize) -> NetValue {
        let mut v = NetValue::Num(1.0);
        for _ in 0..levels {
            v = NetValue::Table(vec![(NetValue::Str("k".into()), v)]);
        }
        v
    }

    #[test]
    fn depth_guard() {
        assert!(nest(MAX_VALUE_DEPTH).validate().is_ok());
        assert_eq!(nest(MAX_VALUE_DEPTH + 1).validate(), Err(ValueError::TooDeep));
    }

    #[test]
    fn size_guard() {
        let big = NetValue::Str("x".repeat(MAX_VALUE_BYTES + 1));
        assert!(matches!(big.validate(), Err(ValueError::TooBig(_))));
        let ok = NetValue::Table(vec![
            (NetValue::Num(1.0), NetValue::Str("sword".into())),
            (NetValue::Num(2.0), NetValue::Str("shield".into())),
        ]);
        assert!(ok.validate().is_ok());
    }

    /// The whole reason `canonical_hash` exists (§6): two peers in perfect
    /// agreement must not report a desync because Lua handed them the same
    /// table's pairs in a different order.
    #[test]
    fn two_permutations_of_one_table_hash_equal() {
        let a = NetValue::Table(vec![
            (NetValue::Str("hp".into()), NetValue::Num(87.0)),
            (NetValue::Str("meter".into()), NetValue::Num(50.0)),
            (NetValue::Str("combo".into()), NetValue::Table(vec![
                (NetValue::Num(1.0), NetValue::Str("light".into())),
                (NetValue::Num(2.0), NetValue::Str("heavy".into())),
            ])),
        ]);
        let b = NetValue::Table(vec![
            (NetValue::Str("combo".into()), NetValue::Table(vec![
                (NetValue::Num(2.0), NetValue::Str("heavy".into())),
                (NetValue::Num(1.0), NetValue::Str("light".into())),
            ])),
            (NetValue::Str("hp".into()), NetValue::Num(87.0)),
            (NetValue::Str("meter".into()), NetValue::Num(50.0)),
        ]);
        assert_ne!(a, b, "the two really are differently ordered");
        assert_eq!(a.canonical_hash(), b.canonical_hash(), "…and must still agree");
    }

    /// It has to catch a real difference, or it is decoration.
    #[test]
    fn a_changed_value_changes_the_hash() {
        let base = NetValue::Table(vec![
            (NetValue::Str("hp".into()), NetValue::Num(87.0)),
            (NetValue::Str("frame".into()), NetValue::Num(12.0)),
        ]);
        for changed in [
            NetValue::Table(vec![
                (NetValue::Str("hp".into()), NetValue::Num(86.0)),
                (NetValue::Str("frame".into()), NetValue::Num(12.0)),
            ]),
            NetValue::Table(vec![
                (NetValue::Str("hp".into()), NetValue::Num(87.0)),
                (NetValue::Str("frames".into()), NetValue::Num(12.0)),
            ]),
            NetValue::Table(vec![(NetValue::Str("hp".into()), NetValue::Num(87.0))]),
        ] {
            assert_ne!(base.canonical_hash(), changed.canonical_hash(), "{changed:?}");
        }
        // One frame of difference in one fighter's state must be visible.
        assert_ne!(NetValue::Num(12.0).canonical_hash(), NetValue::Num(13.0).canonical_hash());
        // …and types are not interchangeable.
        assert_ne!(NetValue::Num(1.0).canonical_hash(), NetValue::Bool(true).canonical_hash());
        assert_ne!(NetValue::Nil.canonical_hash(), NetValue::Str(String::new()).canonical_hash());
    }

    /// Differences the simulation cannot observe must not raise the alarm:
    /// signed zero compares equal everywhere else, and one NaN is every NaN.
    #[test]
    fn indistinguishable_numbers_hash_the_same() {
        assert_eq!(NetValue::Num(0.0).canonical_hash(), NetValue::Num(-0.0).canonical_hash());
        let nan_a = NetValue::Num(f64::NAN);
        let nan_b = NetValue::Num(f64::from_bits(f64::NAN.to_bits() | 0x7));
        assert_eq!(nan_a.canonical_hash(), nan_b.canonical_hash());
    }

    /// A string's length is hashed alongside its bytes, so concatenation can't
    /// alias — `["ab", "c"]` and `["a", "bc"]` are different states.
    #[test]
    fn adjacent_strings_do_not_alias() {
        let a = NetValue::Table(vec![
            (NetValue::Num(1.0), NetValue::Str("ab".into())),
            (NetValue::Num(2.0), NetValue::Str("c".into())),
        ]);
        let b = NetValue::Table(vec![
            (NetValue::Num(1.0), NetValue::Str("a".into())),
            (NetValue::Num(2.0), NetValue::Str("bc".into())),
        ]);
        assert_ne!(a.canonical_hash(), b.canonical_hash());
    }

    #[test]
    fn round_trips_through_postcard() {
        let v = NetValue::Table(vec![
            (NetValue::Str("hp".into()), NetValue::Num(87.5)),
            (NetValue::Str("name".into()), NetValue::Str("floppy".into())),
            (NetValue::Str("dead".into()), NetValue::Bool(false)),
            (NetValue::Str("aux".into()), NetValue::Nil),
        ]);
        let bytes = postcard::to_allocvec(&v).unwrap();
        let back: NetValue = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(v, back);
    }
}
