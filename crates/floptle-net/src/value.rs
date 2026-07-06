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
