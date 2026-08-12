//! Semantic versions and the ranges that ask for them.
//!
//! A package says what it *is* (`version: "1.4.2"`) and what it *needs*
//! (`engine: ">=0.55.0"`, `dependencies: [(id: "…", version: "^1.2")]`). Those
//! are two different types and conflating them is how a resolver ends up
//! comparing `"^1.2"` to `"1.2.0"` as strings and getting it wrong.
//!
//! **Why not the `semver` crate.** This is ~200 lines with exactly the operators
//! the manifest documents, and it lets the *error messages* be ours — a package
//! author who typed `1.2.x` deserves to be told that, in the editor, not to see
//! a parse error from a dependency they never named.
//!
//! **A bare requirement is caret, not exact.** `"1.2.3"` means "1.2.3 or any
//! later release that did not break compatibility" — the same rule Cargo uses.
//! Exactness is available and has to be asked for: `"=1.2.3"`.

use std::fmt;
use std::str::FromStr;

/// A `major.minor.patch` version, with an optional pre-release tag
/// (`1.0.0-beta.2`). Build metadata (`+sha`) is accepted and ignored, as the
/// spec requires — two versions differing only in build metadata are equal.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Dot-separated pre-release identifiers, empty for a normal release.
    pub pre: Vec<String>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self { major, minor, patch, pre: Vec::new() }
    }

    /// True for `1.0.0-rc.1`, false for `1.0.0`. A pre-release only satisfies a
    /// range whose own bound is a pre-release of the same version — otherwise
    /// `>=1.0.0` would quietly accept `2.0.0-alpha`, which is the one release
    /// nobody meant by it.
    pub fn is_pre(&self) -> bool {
        !self.pre.is_empty()
    }

    /// The exclusive upper bound of this version's compatibility range — what
    /// `^` means. `1.2.3` → `2.0.0`; `0.2.3` → `0.3.0`; `0.0.3` → `0.0.4`.
    /// Below 1.0 every release may break, so the caret narrows as the leading
    /// zeros pile up.
    pub fn next_breaking(&self) -> Version {
        if self.major > 0 {
            Version::new(self.major + 1, 0, 0)
        } else if self.minor > 0 {
            Version::new(0, self.minor + 1, 0)
        } else {
            Version::new(0, 0, self.patch + 1)
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        let core = (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch));
        if core != Ordering::Equal {
            return core;
        }
        // A pre-release sorts BEFORE the release it leads up to (1.0.0-rc < 1.0.0).
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => cmp_pre(&self.pre, &other.pre),
        }
    }
}

/// Compare pre-release identifier lists: numeric ones compare numerically and
/// rank below alphanumeric ones; a shorter list of otherwise-equal identifiers
/// sorts first (`1.0.0-rc` < `1.0.0-rc.1`).
fn cmp_pre(a: &[String], b: &[String]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(nx), Ok(ny)) => nx.cmp(&ny),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => x.cmp(y),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err("a version cannot be empty".into());
        }
        // Build metadata is accepted and discarded — `1.0.0+sha` IS `1.0.0`.
        let s = s.split('+').next().unwrap_or(s);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, p),
            None => (s, ""),
        };
        let mut nums = core.split('.');
        let mut part = |what: &str| -> Result<u64, String> {
            let raw = nums.next().unwrap_or("0");
            raw.parse::<u64>()
                .map_err(|_| format!("`{s}` is not a version: the {what} part is `{raw}`, which is not a number"))
        };
        let major = part("major")?;
        let minor = part("minor")?;
        let patch = part("patch")?;
        if nums.next().is_some() {
            return Err(format!("`{s}` is not a version: it has more than three number parts"));
        }
        let pre: Vec<String> =
            if pre.is_empty() { Vec::new() } else { pre.split('.').map(|p| p.to_string()).collect() };
        if pre.iter().any(|p| p.is_empty()) {
            return Err(format!("`{s}` is not a version: it has an empty pre-release part"));
        }
        Ok(Version { major, minor, patch, pre })
    }
}

impl serde::Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// One comparison in a range.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Op {
    Exact,
    Greater,
    GreaterEq,
    Less,
    LessEq,
    /// `^1.2.3` — at least this, below the next breaking release.
    Caret,
    /// `~1.2.3` — at least this, below the next MINOR.
    Tilde,
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Comparator {
    op: Op,
    ver: Version,
}

impl Comparator {
    fn matches(&self, v: &Version) -> bool {
        // A pre-release only ever satisfies a bound that is itself a
        // pre-release of the SAME major.minor.patch. Without this rule
        // `>=1.0.0` accepts `2.0.0-alpha1`, and an unreleased package installs
        // itself into a project that asked for a stable one.
        if v.is_pre() {
            let same_core = (v.major, v.minor, v.patch)
                == (self.ver.major, self.ver.minor, self.ver.patch);
            if !(self.ver.is_pre() && same_core) {
                return false;
            }
        }
        match self.op {
            Op::Exact => v == &self.ver,
            Op::Greater => v > &self.ver,
            Op::GreaterEq => v >= &self.ver,
            Op::Less => v < &self.ver,
            Op::LessEq => v <= &self.ver,
            Op::Caret => v >= &self.ver && v < &self.ver.next_breaking(),
            Op::Tilde => {
                let upper = Version::new(self.ver.major, self.ver.minor + 1, 0);
                v >= &self.ver && v < &upper
            }
        }
    }
}

impl fmt::Display for Comparator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sym = match self.op {
            Op::Exact => "=",
            Op::Greater => ">",
            Op::GreaterEq => ">=",
            Op::Less => "<",
            Op::LessEq => "<=",
            Op::Caret => "^",
            Op::Tilde => "~",
        };
        write!(f, "{sym}{}", self.ver)
    }
}

/// A version requirement: `*`, `^1.2`, `>=0.55.0`, or several joined by commas
/// (`">=1.2, <2.0"` — every part must match).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct VersionReq {
    /// Empty = `*`, which matches any release (but still not a pre-release).
    parts: Vec<Comparator>,
    /// What the author actually typed, so a message can quote it back.
    raw: String,
}

impl VersionReq {
    /// Matches anything (except pre-releases, which always have to be asked for
    /// by name).
    pub fn any() -> Self {
        Self { parts: Vec::new(), raw: "*".into() }
    }

    /// Does `v` satisfy this requirement?
    pub fn matches(&self, v: &Version) -> bool {
        if self.parts.is_empty() {
            return !v.is_pre();
        }
        self.parts.iter().all(|c| c.matches(v))
    }

    /// The text the manifest carried, for error messages.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl FromStr for VersionReq {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let raw = s.trim().to_string();
        if raw.is_empty() || raw == "*" {
            return Ok(VersionReq { parts: Vec::new(), raw: if raw.is_empty() { "*".into() } else { raw } });
        }
        let mut parts = Vec::new();
        for piece in raw.split(',') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            // Longest operator first: `>=` before `>`.
            let (op, rest) = if let Some(r) = piece.strip_prefix(">=") {
                (Op::GreaterEq, r)
            } else if let Some(r) = piece.strip_prefix("<=") {
                (Op::LessEq, r)
            } else if let Some(r) = piece.strip_prefix('>') {
                (Op::Greater, r)
            } else if let Some(r) = piece.strip_prefix('<') {
                (Op::Less, r)
            } else if let Some(r) = piece.strip_prefix('^') {
                (Op::Caret, r)
            } else if let Some(r) = piece.strip_prefix('~') {
                (Op::Tilde, r)
            } else if let Some(r) = piece.strip_prefix('=') {
                (Op::Exact, r)
            } else {
                // A bare version is a CARET, the same as Cargo. Documented in
                // docs/packages.md so nobody has to guess.
                (Op::Caret, piece)
            };
            let rest = rest.trim();
            // `1.2` and `1` are legal in a requirement (they are not in a
            // version): the missing parts are zero, and the caret/tilde bound
            // widens accordingly. `^1.2` therefore means ">=1.2.0, <2.0.0".
            let ver = parse_partial(rest).map_err(|e| {
                format!("`{raw}` is not a version range: {e}")
            })?;
            parts.push(Comparator { op, ver });
        }
        if parts.is_empty() {
            return Err(format!("`{raw}` is not a version range: it names no version"));
        }
        Ok(VersionReq { parts, raw })
    }
}

/// Parse a version that is allowed to leave off its minor/patch — legal in a
/// requirement (`^1.2`), never in a package's own `version`.
fn parse_partial(s: &str) -> Result<Version, String> {
    let filled = match s.split('-').next().unwrap_or(s).matches('.').count() {
        0 => format!("{s}.0.0"),
        1 => {
            // Insert before any pre-release tag: `1.2-rc` → `1.2.0-rc`.
            match s.split_once('-') {
                Some((core, pre)) => format!("{core}.0-{pre}"),
                None => format!("{s}.0"),
            }
        }
        _ => s.to_string(),
    };
    filled.parse()
}

impl serde::Serialize for VersionReq {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.raw)
    }
}

impl<'de> serde::Deserialize<'de> for VersionReq {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        s.parse().unwrap()
    }
    fn r(s: &str) -> VersionReq {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_prints_round_trip() {
        for s in ["0.0.1", "1.2.3", "10.20.30", "1.0.0-rc.1", "2.0.0-beta"] {
            assert_eq!(v(s).to_string(), s, "{s}");
        }
    }

    #[test]
    fn build_metadata_is_ignored() {
        assert_eq!(v("1.2.3+abc123"), v("1.2.3"));
    }

    #[test]
    fn a_bad_version_says_which_part() {
        let err = "1.2.x".parse::<Version>().unwrap_err();
        assert!(err.contains("patch"), "{err}");
        assert!(err.contains("`x`"), "{err}");
    }

    #[test]
    fn a_version_may_not_have_four_parts() {
        assert!("1.2.3.4".parse::<Version>().is_err());
    }

    #[test]
    fn ordering_puts_a_pre_release_before_its_release() {
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(v("1.0.0-rc.1") < v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc") < v("1.0.0-rc.1"));
        assert!(v("1.0.0-alpha") < v("1.0.0-beta"));
        // Numeric identifiers rank below alphanumeric ones.
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert!(v("0.9.9") < v("1.0.0-rc.1"));
    }

    #[test]
    fn caret_narrows_below_one_point_zero() {
        assert_eq!(v("1.2.3").next_breaking(), v("2.0.0"));
        assert_eq!(v("0.2.3").next_breaking(), v("0.3.0"));
        assert_eq!(v("0.0.3").next_breaking(), v("0.0.4"));
    }

    #[test]
    fn a_bare_requirement_is_a_caret() {
        let req = r("1.2.3");
        assert!(req.matches(&v("1.2.3")));
        assert!(req.matches(&v("1.9.0")));
        assert!(!req.matches(&v("2.0.0")));
        assert!(!req.matches(&v("1.2.2")));
    }

    #[test]
    fn exact_has_to_be_asked_for() {
        let req = r("=1.2.3");
        assert!(req.matches(&v("1.2.3")));
        assert!(!req.matches(&v("1.2.4")));
    }

    #[test]
    fn tilde_holds_the_minor() {
        let req = r("~1.2.3");
        assert!(req.matches(&v("1.2.9")));
        assert!(!req.matches(&v("1.3.0")));
    }

    #[test]
    fn a_partial_requirement_fills_in_zeros() {
        let req = r("^1.2");
        assert!(req.matches(&v("1.2.0")));
        assert!(req.matches(&v("1.9.9")));
        assert!(!req.matches(&v("1.1.9")));
        assert!(!req.matches(&v("2.0.0")));
        let major_only = r("^1");
        assert!(major_only.matches(&v("1.0.0")));
        assert!(!major_only.matches(&v("0.9.9")));
    }

    #[test]
    fn comma_joins_bounds() {
        let req = r(">=1.2, <1.5");
        assert!(req.matches(&v("1.3.0")));
        assert!(!req.matches(&v("1.5.0")));
        assert!(!req.matches(&v("1.1.0")));
    }

    #[test]
    fn star_matches_any_release() {
        assert!(r("*").matches(&v("0.0.1")));
        assert!(VersionReq::any().matches(&v("99.0.0")));
    }

    /// The rule that keeps an unreleased package out of a project that asked
    /// for a stable one — `>=1.0.0` must NOT accept `2.0.0-alpha`.
    #[test]
    fn a_pre_release_only_satisfies_a_pre_release_bound() {
        assert!(!r(">=1.0.0").matches(&v("2.0.0-alpha")));
        assert!(!r("*").matches(&v("1.0.0-alpha")));
        assert!(!r("^1.0.0").matches(&v("1.5.0-rc.1")));
        // …and one that names the same release does accept it.
        assert!(r(">=1.0.0-alpha").matches(&v("1.0.0-beta")));
        assert!(r("=1.0.0-rc.1").matches(&v("1.0.0-rc.1")));
    }

    #[test]
    fn an_unparseable_range_quotes_what_was_typed() {
        let err = "1.2.x".parse::<VersionReq>().unwrap_err();
        assert!(err.contains("1.2.x"), "{err}");
    }

    #[test]
    fn serde_round_trips_through_ron() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Doc {
            v: Version,
            r: VersionReq,
        }
        let doc = Doc { v: v("1.2.3-rc.1"), r: r(">=0.55.0") };
        let text = ron::ser::to_string(&doc).unwrap();
        let back: Doc = ron::from_str(&text).unwrap();
        assert_eq!(doc, back);
    }
}
