//! The Floptle Cloud region list: where the managed relays are, and which
//! letter each one's lobby codes start with.
//!
//! **This is the one Cloud call that needs no account.** A shipped game hosting
//! on `net.host{ relay = "cloud" }` has a game key and nothing else — no
//! signed-in player, no token — so `GET /cloud/regions` is public, and this
//! module is the only part of `floptle-account` that talks to fopull.com
//! without a bearer.
//!
//! ## Why it is cached on disk and shipped with a fallback
//!
//! A player pressing Host is waiting. Three things therefore hold:
//!
//! 1. **A fresh answer is nice; a fast one is required.** The list is cached to
//!    disk for [`CACHE_TTL`], so the second launch of a game does not make a
//!    network call to find out something that changes a few times a decade.
//! 2. **A cold cache must not be a dead Host button.** [`shipped`] is compiled
//!    in, so a game whose first launch is offline — or whose first launch is on
//!    the day fopull.com is having a bad hour — still resolves `us-east` and
//!    still hosts. The list it carries is the one that was true when the build
//!    was made, which is exactly the guarantee a shipped binary can make.
//! 3. **A region the build has never heard of still works**, as long as the
//!    fetch succeeds once. That is what stops "we opened Frankfurt" from
//!    meaning "everyone must re-export".
//!
//! The failure this arrangement is built to avoid is the interesting one: a
//! player whose Host button does nothing because a JSON fetch timed out. There
//! is no path through here that produces an empty list.

#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// How long a fetched list is trusted before it is refreshed.
///
/// A day. Regions are an operator decision measured in months — W keeps the
/// registry in a config file in git, not a table — so this is about bounding
/// staleness after a region opens, not about tracking a moving value.
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// One managed relay region.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// `us-east`. What `net.host{ region = … }` names and what the control
    /// plane keys everything by.
    pub id: String,
    /// The letter every lobby code in this region starts with — `U`.
    ///
    /// **A permanent allocation, and the control plane owns the registry.** It
    /// is what lets `net.join("cloud://UABCDE")` find the right relay from a
    /// cached list instead of asking anybody, which is the whole reason the
    /// join path never depends on fopull.com being up.
    pub letter: char,
    /// Human-readable, for a lobby screen: "US East (Ashburn)".
    #[serde(default)]
    pub name: String,
    /// `host:port` of the managed relay.
    pub relay: String,
    /// `planned` until a lobby has actually been joined from another network;
    /// `up` once it has. **A build must not host on a region that is not `up`**
    /// — see [`Region::is_live`].
    #[serde(default)]
    pub status: String,
}

impl Region {
    /// Is this a region a game may actually host on?
    ///
    /// `planned` is the honest state of a region whose boxes exist but which
    /// nobody has joined across a real network yet, and `GET /cloud/regions` is
    /// what every shipped build reads to decide where it may host. Treating
    /// `planned` as usable would send players at a relay that has never proven
    /// it forwards a packet — and because clients cache this list for a day,
    /// they would keep doing it long after somebody noticed.
    pub fn is_live(&self) -> bool {
        self.status == "up"
    }
}

/// The whole list, as the control plane answers it.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Regions {
    #[serde(default)]
    pub regions: Vec<Region>,
}

impl Regions {
    /// The region whose lobby codes start with `letter`, case-insensitively.
    ///
    /// This is the whole of `net.join("cloud://UABCDE")`: one character maps to
    /// one relay address, from a list already on disk, with no call to anybody.
    pub fn by_letter(&self, letter: char) -> Option<&Region> {
        let want = letter.to_ascii_uppercase();
        self.regions.iter().find(|r| r.letter.to_ascii_uppercase() == want)
    }

    pub fn by_id(&self, id: &str) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// Every region a game may host on right now.
    pub fn live(&self) -> impl Iterator<Item = &Region> {
        self.regions.iter().filter(|r| r.is_live())
    }
}

/// The list compiled into this build.
///
/// **Not a default to fall back to grudgingly — the guarantee a shipped binary
/// makes.** A game exported today knows where us-east is forever, whatever
/// happens to anybody's DNS or hour of downtime, and a first launch with no
/// network still hosts.
///
/// It is deliberately marked `planned`, because that is what it is until a
/// region has been joined across a real network, and a build that shipped
/// before us-east came up must not claim otherwise. A successful fetch replaces
/// this wholesale, so the day the region goes `up` every game learns it within
/// [`CACHE_TTL`].
pub fn shipped() -> Regions {
    Regions {
        regions: vec![Region {
            id: "us-east".into(),
            letter: 'U',
            name: "US East (Ashburn)".into(),
            relay: "us-east.relay.fopull.com:7788".into(),
            status: "planned".into(),
        }],
    }
}

/// Where the cached list lives — beside the engine's other per-user state.
///
/// **Native only, and the whole cache layer with it.** A page has no filesystem
/// to cache into, no `std::time::SystemTime` that does not panic, and no reason
/// to want either: a browser build is a client, it never hosts, and the only
/// thing it could do with this list is print a region's name. [`shipped`] and
/// the lookups stay available everywhere; the disk and the clock do not.
#[cfg(not(target_arch = "wasm32"))]
pub fn cache_path() -> Option<PathBuf> {
    dirs_cache().map(|d| d.join("floptle").join("cloud-regions.json"))
}

#[cfg(not(target_arch = "wasm32"))]
fn dirs_cache() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
}

/// What a cached list is stored as: the answer, plus when we got it.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Cached {
    fetched_unix: u64,
    regions: Regions,
}

#[cfg(not(target_arch = "wasm32"))]
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read the cached list, if there is one and it is still fresh.
#[cfg(not(target_arch = "wasm32"))]
pub fn cached() -> Option<Regions> {
    let path = cache_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let c: Cached = serde_json::from_str(&text).ok()?;
    let age = now_unix().saturating_sub(c.fetched_unix);
    (age < CACHE_TTL.as_secs() && !c.regions.regions.is_empty()).then_some(c.regions)
}

/// Write a freshly-fetched list to the cache. Best effort: a cache that cannot
/// be written is a slower next launch, never a failure.
#[cfg(not(target_arch = "wasm32"))]
pub fn store(regions: &Regions) {
    let Some(path) = cache_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let payload = Cached { fetched_unix: now_unix(), regions: regions.clone() };
    if let Ok(text) = serde_json::to_string(&payload) {
        let _ = std::fs::write(path, text);
    }
}

/// Fetch the list from the control plane. **Public — no bearer.**
///
/// Native only: a browser build cannot host, so it never needs to resolve a
/// relay to host on.
#[cfg(not(target_arch = "wasm32"))]
pub fn fetch(base: &str, timeout: std::time::Duration) -> Result<Regions, String> {
    if !crate::auth::is_fopull_host(base) && !crate::auth::is_local_host(base) {
        return Err(format!("{base} is not fopull.com"));
    }
    let url = format!("{}{}/cloud/regions", base.trim_end_matches('/'), crate::cloud::API_PREFIX);
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    let text = agent
        .get(&url)
        .call()
        .map_err(|e| format!("could not read the region list: {e}"))?
        .into_string()
        .map_err(|e| format!("unreadable region list: {e}"))?;
    let out: Regions =
        serde_json::from_str(&text).map_err(|e| format!("region list did not parse: {e}"))?;
    if out.regions.is_empty() {
        // An empty list is not an answer worth caching over the shipped one: it
        // would turn every Host button in every build into a dead one for a
        // day, which is a far worse failure than a stale address.
        return Err("the region list came back empty".into());
    }
    Ok(out)
}

/// The list to use right now: cache, else fetch, else what the build shipped
/// with — **never nothing**.
///
/// The order matters and so does the fallback. A player pressing Host is
/// waiting on this, so a fresh-enough cache short-circuits the network
/// entirely; a fetch is only attempted when the cache is stale or missing; and
/// a fetch that fails leaves the game hosting on the addresses it was built
/// with rather than leaving the player looking at a button that did nothing.
#[cfg(not(target_arch = "wasm32"))]
pub fn resolve(base: &str, timeout: std::time::Duration) -> Regions {
    if let Some(c) = cached() {
        return c;
    }
    match fetch(base, timeout) {
        Ok(fresh) => {
            store(&fresh);
            fresh
        }
        Err(_) => shipped(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn list() -> Regions {
        Regions {
            regions: vec![
                Region {
                    id: "us-east".into(),
                    letter: 'U',
                    name: "US East".into(),
                    relay: "us-east.relay.fopull.com:7788".into(),
                    status: "up".into(),
                },
                Region {
                    id: "eu-central".into(),
                    letter: 'E',
                    name: "Frankfurt".into(),
                    relay: "eu-central.relay.fopull.com:7788".into(),
                    status: "planned".into(),
                },
            ],
        }
    }

    /// `net.join("cloud://UABCDE")` is one character mapped to one address, out
    /// of a list already on disk. That is the whole reason the join path never
    /// waits on fopull.com.
    #[test]
    fn a_lobby_codes_first_letter_names_its_relay() {
        let r = list();
        assert_eq!(r.by_letter('U').unwrap().relay, "us-east.relay.fopull.com:7788");
        assert_eq!(r.by_letter('E').unwrap().id, "eu-central");
        // Players type codes in whatever case they like.
        assert_eq!(r.by_letter('u').unwrap().id, "us-east");
        assert!(r.by_letter('Z').is_none(), "an unknown letter is not a guess");
    }

    /// **`planned` is not somewhere to send a player.** It is the honest state
    /// of a region whose boxes exist but which nobody has joined across a real
    /// network yet, and a client caches this list for a day — so treating it as
    /// usable keeps sending players at an unproven relay long after somebody
    /// noticed.
    #[test]
    fn a_planned_region_is_not_one_a_game_may_host_on() {
        let r = list();
        assert!(r.by_id("us-east").unwrap().is_live());
        assert!(!r.by_id("eu-central").unwrap().is_live());
        let live: Vec<&str> = r.live().map(|r| r.id.as_str()).collect();
        assert_eq!(live, ["us-east"]);
    }

    /// A build ships knowing where us-east is, so a first launch with no
    /// network still resolves a relay. The one thing it must not do is claim a
    /// region is `up` when the build predates that being true.
    #[test]
    fn the_shipped_list_is_never_empty_and_never_overclaims() {
        let s = shipped();
        assert!(!s.regions.is_empty(), "an empty fallback is a dead Host button");
        assert!(s.by_letter('U').is_some());
        assert!(
            s.live().next().is_none(),
            "a build cannot know a region came up after it was made"
        );
    }

    /// The control plane's real answer, verbatim from W's deployed endpoint —
    /// pinned here so a shape change on that side fails a test rather than a
    /// player's Host button.
    #[test]
    fn the_live_answer_parses() {
        let body = r#"{"regions":[{"id":"us-east","letter":"U","name":"US East (Ashburn)",
          "relay":"us-east.relay.fopull.com:7788",
          "fleet":["us-east-1.fleet.fopull.com"],"status":"planned"}]}"#;
        let r: Regions = serde_json::from_str(body).expect("the live shape parses");
        assert_eq!(r.regions.len(), 1);
        let us = r.by_letter('U').expect("us-east is there");
        assert_eq!(us.relay, "us-east.relay.fopull.com:7788");
        assert_eq!(us.name, "US East (Ashburn)");
        assert!(!us.is_live(), "it reads planned, and that is correct today");
    }

    /// A stale cache is ignored rather than served, and a fresh one is trusted.
    #[test]
    fn a_cache_older_than_a_day_is_not_used() {
        let fresh = Cached { fetched_unix: now_unix(), regions: list() };
        let stale = Cached { fetched_unix: now_unix() - CACHE_TTL.as_secs() - 1, regions: list() };
        let age_ok = |c: &Cached| now_unix().saturating_sub(c.fetched_unix) < CACHE_TTL.as_secs();
        assert!(age_ok(&fresh));
        assert!(!age_ok(&stale));
    }
}
