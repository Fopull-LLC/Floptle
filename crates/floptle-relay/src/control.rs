//! The control plane, as this relay needs it: a **key snapshot it pulls**, a
//! cold-path lookup for a key too new to be in one, and a usage report.
//!
//! Everything here is a trait first and HTTP second. That is not ceremony —
//! the rules that matter (what an absent key means, what an outage means, what
//! a slow answer means) are the ones a network makes hardest to test, so they
//! are written against [`ControlPlane`] and the guards drive a fake.

use std::collections::HashMap;
use std::time::Duration;

use serde::Deserialize;

use crate::boxstats::RelayBox;

/// The free tier's concurrent-player limit — the floor a relay allows at when
/// it cannot reach the control plane and has never heard of the key.
///
/// Generous to a developer who shipped a build in the last thirty seconds,
/// worthless to anybody trying to get free hosting out of an outage.
pub const FREE_TIER_CCU: u32 = 20;

/// How a key stands with Floptle Cloud.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyState {
    Active,
    /// Rotated, still authorizing until its grace window ends — a build in the
    /// wild is on borrowed time and the relay says so once per lobby.
    Deprecated,
    Revoked,
    /// The control plane has looked and does not have it.
    Unknown,
}

/// What the control plane knows about one game key.
#[derive(Clone, Debug, Deserialize)]
pub struct KeyRow {
    pub key: String,
    #[serde(default)]
    pub game: String,
    #[serde(default = "active")]
    pub state: KeyState,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub ccu_limit: u32,
    #[serde(default)]
    pub regions: Vec<String>,
    /// The account is over its pooled cap across every region. Derived from
    /// usage this relay itself reported, so it is stale by design and nothing
    /// is billed on it.
    #[serde(default)]
    pub account_over_limit: bool,
    /// How many lobbies this key may hold open on this relay at once, when the
    /// control plane sets one (`floptle/0228`). Absent means the player cap is
    /// the only ceiling. A leaked key can fill a plan's players with empty
    /// lobbies from a handful of addresses; this is the number that stops it.
    #[serde(default)]
    pub max_lobbies: Option<u32>,
    /// Build ids the control plane has marked as not allowed to host on this
    /// key — one shipped build whose copy of the key is being abused, revoked
    /// without rotating the key for every other build.
    #[serde(default)]
    pub blocked_builds: Vec<String>,
}

fn active() -> KeyState {
    KeyState::Active
}

impl KeyRow {
    /// May this key host at all — as opposed to "is it at its limit", which is
    /// a different question with a different sentence.
    pub fn may_host(&self) -> bool {
        matches!(self.state, KeyState::Active | KeyState::Deprecated)
    }
}

/// The **cold path's** answer, which is a different shape to a snapshot row and
/// must not be confused with one.
///
/// `POST /cloud/relay/authorize` answers a question about one key the caller
/// already named, so it does not echo the key back; and it spells two fields
/// differently to the snapshot — `status` where a row says `state`, and
/// `over_limit` where a row says `account_over_limit`. Deserialising it
/// straight into a [`KeyRow`] therefore fails on the missing `key` alone, and
/// would silently read the other two as their defaults even if it did not.
///
/// **That failure has no symptom of its own**, which is why this type exists
/// rather than a `#[serde(alias)]` or two: a malformed cold answer floors the
/// key at the free tier and logs, so every key not yet in the snapshot would
/// have been quietly capped at 20 players on a paid plan, forever, with the
/// relay reporting nothing worse than "allowing at the free limit".
#[derive(Clone, Debug, Deserialize)]
pub struct AuthorizeReply {
    #[serde(default)]
    pub game: String,
    #[serde(default = "active")]
    pub status: KeyState,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub ccu_limit: u32,
    #[serde(default)]
    pub regions: Vec<String>,
    #[serde(default)]
    pub over_limit: bool,
    #[serde(default)]
    pub max_lobbies: Option<u32>,
    #[serde(default)]
    pub blocked_builds: Vec<String>,
}

impl AuthorizeReply {
    /// The row this answer describes. The key comes from the caller, which is
    /// the only side that ever knew it.
    pub fn into_row(self, key: &str) -> KeyRow {
        KeyRow {
            key: key.to_string(),
            game: self.game,
            state: self.status,
            tier: self.tier,
            ccu_limit: self.ccu_limit,
            regions: self.regions,
            account_over_limit: self.over_limit,
            max_lobbies: self.max_lobbies,
            blocked_builds: self.blocked_builds,
        }
    }
}

/// A delta (or whole) page of the region's keys.
#[derive(Clone, Debug, Deserialize)]
pub struct KeySnapshot {
    /// Hand this back on the next pull to get only what changed.
    #[serde(default)]
    pub cursor: Option<String>,
    /// **The whole set, not a delta — replace, do not merge.** The server may
    /// send this at any time; a cursor it no longer recognises is the common
    /// reason, and a relay that merged one would keep keys the control plane
    /// has forgotten.
    #[serde(default)]
    pub full: bool,
    #[serde(default)]
    pub keys: Vec<KeyRow>,
    /// Keys that are gone. **This — and only this — is a revocation.** A key
    /// simply missing from `keys` has not been revoked; it was not in this
    /// page.
    #[serde(default)]
    pub removed: Vec<String>,
    /// **Lobby codes the control plane has promised to somebody**
    /// (`floptle/0217`).
    ///
    /// ⚠ **Always complete, never a delta** — the same rule as
    /// `over_limit_accounts` and for a sharper reason: a relay that came back
    /// empty and merged would not know which codes are spoken for, and would
    /// hand a memorised one to a stranger. W sends only reservations it can
    /// attribute to a key; one it cannot is a string this relay could do
    /// nothing with anyway.
    #[serde(default)]
    pub reserved: Vec<Reservation>,
}

/// One lobby code the control plane owns, and what its deployment is doing.
#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct Reservation {
    pub code: String,
    /// The game key entitled to reclaim it. The relay already validates this at
    /// registration, so a reservation introduces no new secret.
    pub key: String,
    #[serde(default)]
    pub state: ReservedState,
}

/// What the deployment behind a reserved code is doing.
///
/// ⚠ **`Sleeping` and `Stopped` are opposite facts and the relay must act on
/// the difference.** Sleeping is the platform saving money on a server the
/// developer still expects to work, so a join wakes it. Stopped is a decision
/// somebody made, and a stranger's join must never restart a server its owner
/// turned off — that is the one case where "no lobby" is the honest answer.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ReservedState {
    /// Up, or on its way up. Nothing to do but let the code be reclaimed.
    #[default]
    Live,
    /// Idle and shut down to free its box. A join wakes it.
    Sleeping,
    /// The developer stopped it. A join is refused as it always was.
    Stopped,
    /// ⚠ **A state this relay has never heard of.** Treated as `Live`: the code
    /// stays reserved so nobody else is given it, and a join is neither woken
    /// nor specially refused. A control plane that adds a state must not be
    /// able to make an older relay hand out somebody's code.
    #[serde(other)]
    Unknown,
}

/// What `POST /cloud/relay/wake` said.
///
/// ⚠ **Every case is a 200 with a body, not a 4xx**, which is W's design and
/// the right one: the relay retries, and there is nothing it could usefully do
/// differently on a status code. The distinction it *does* act on lives in the
/// body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeOutcome {
    /// It was asleep and is now coming up.
    Woken,
    /// Running, starting, or already on its way. Wait, do not refuse.
    AlreadyAwake,
    /// ⚠ **The developer stopped it.** Refuse the join, as before — a
    /// stranger's join must not restart a server its owner turned off.
    Stopped,
    /// No such reservation, or a malformed code. Refuse.
    UnknownCode,
}

impl WakeOutcome {
    /// Read the body's `reason`. An unrecognised one is treated as
    /// [`WakeOutcome::AlreadyAwake`] — hold the joiner rather than refuse,
    /// because a control plane that learns a new reason must not turn a waking
    /// server into a missing one on an older relay.
    pub fn from_body(woken: bool, reason: Option<&str>) -> Self {
        if woken {
            return WakeOutcome::Woken;
        }
        match reason {
            Some("stopped") => WakeOutcome::Stopped,
            Some("unknown_code") => WakeOutcome::UnknownCode,
            _ => WakeOutcome::AlreadyAwake,
        }
    }

    /// Is this the answer that refuses the join?
    pub fn refuses(self) -> bool {
        matches!(self, WakeOutcome::Stopped | WakeOutcome::UnknownCode)
    }
}

impl ReservedState {
    /// Should a join for this code wake the deployment behind it?
    pub fn wakes_on_join(self) -> bool {
        matches!(self, ReservedState::Sleeping)
    }

    /// Is a join for this code refused outright, as it was before reservations?
    ///
    /// Kept for readers and for the guards, which assert the two halves agree;
    /// the policy branches on [`ReservedState::wakes_on_join`] so there is
    /// exactly one decision in the code.
    #[cfg(test)]
    ///
    /// ⚠ **Every state but `Sleeping`**, which is the same set
    /// `!wakes_on_join()` describes — kept as its own name because the reasons
    /// differ even though the answer does not. `Stopped` is a decision somebody
    /// made; `Live` is a server between connections; `Unknown` is a control
    /// plane newer than this relay. The policy branches on `wakes_on_join` so
    /// there is exactly one decision in the code.
    pub fn refuses_join(self) -> bool {
        !self.wakes_on_join()
    }
}
/// One key's traffic and occupancy over a reporting interval.
///
/// **Additive by design** (`floptle/0195`): the three fields below arrived after
/// the control plane already accepted the first two. They are extra keys on a
/// JSON object a control plane that ignores them keeps parsing, which is the §8
/// rule and why this is not a schema bump.
/// One address a key's lobbies are hosted from, and how many (`floptle/0228`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostAddress {
    pub address: std::net::IpAddr,
    pub lobbies: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSample {
    pub key: String,
    pub ccu: u32,
    pub lobbies: u32,
    /// Payload bytes the relay **received** for this key's lobbies during the
    /// interval, forwarded or not.
    pub bytes_in: u64,
    /// Payload bytes the relay **sent** on for this key's lobbies. This is the
    /// half that costs money — egress is what a region is billed for — and it
    /// is deliberately separate from `bytes_in` rather than assumed equal: a
    /// datagram for a peer that has just left is received and never forwarded,
    /// so the two diverge exactly when something is going wrong.
    pub bytes_out: u64,
    /// Joins refused during the interval **because the account was at its
    /// ceiling** — not refusals for a bad code, a ban or a revoked key, which
    /// are different facts about different people.
    ///
    /// ⚠ **Zero and absent are different and must stay so.** Absent means a
    /// relay too old to count them; zero means a relay that counted and found
    /// none. A page that reads the two the same way tells a developer nobody
    /// was turned away when the truth is that nobody knows.
    pub refused_joins: u32,
    /// **Payload that arrived for a lobby that did not exist** (`floptle/0222`).
    ///
    /// ⚠ These bytes used to be visible only as `bytes_in` exceeding
    /// `bytes_out` — and that is literally how a host being torn down three
    /// times inside one real match was found, by differencing two counters that
    /// had matched to the byte in every other bucket ever recorded. Anything
    /// above zero means somebody is sending into a lobby that is gone.
    pub orphan_bytes: u64,
    /// **Where this key's live lobbies are hosted from** (`floptle/0228`):
    /// one entry per address, `lobbies` summing to the sample's `lobbies`.
    /// The developer's question is "is my key being used by someone who is
    /// not me", and the control plane's first move on it is to count and
    /// show, not to refuse — a CI runner, a LAN party and a whole ISP behind
    /// CGNAT all look like "many lobbies from one address" and are honest.
    /// Empty on a relay whose transport cannot name addresses; absent on a
    /// relay too old to send it.
    pub hosts: Vec<HostAddress>,
}

/// One key's row of the usage POST.
fn usage_row(s: &UsageSample) -> serde_json::Value {
    serde_json::json!({
        "key": s.key,
        "ccu": s.ccu,
        "lobbies": s.lobbies,
        "bytes_in": s.bytes_in,
        "bytes_out": s.bytes_out,
        "refused_joins": s.refused_joins,
        "orphan_bytes": s.orphan_bytes,
        // Addresses as strings — v4 and v6 both — and always present, so an
        // empty list means "hosted from nowhere the relay could name" and a
        // missing key means a relay too old to say (`floptle/0228`).
        "hosts": s.hosts.iter().map(|h| serde_json::json!({
            "address": h.address.to_string(),
            "lobbies": h.lobbies,
        })).collect::<Vec<_>>(),
    })
}

/// The `box` object, built by hand so the omission rule is visible in one place.
///
/// ⚠ **A measurement that was not taken is left OUT of the object**, never sent
/// as `0`. This is the fleet agent's rule and its shape (`floptle/0215`), so one
/// code path on the control plane reads both — but the reason is sharper here:
/// `rx_drops: 0` from a relay that is keeping up is the best news it has, and
/// `rx_drops: 0` from a relay that could not read `/proc/net/udp` is a relay
/// dropping packets while reporting health.
fn box_json(b: &RelayBox) -> serde_json::Value {
    let mut o = serde_json::Map::new();
    o.insert("host".into(), b.host.clone().into());
    // **This binary's version**, compiled in (`floptle/0232`): the control
    // plane could not see a relay's version at all, so nothing — the lobby
    // code reclaim, the certificate fallback — could be gated on what the
    // relay on a box actually is. Never read from a file or a unit.
    o.insert("version".into(), env!("CARGO_PKG_VERSION").into());
    // Occupancy is always known — the relay is holding the lobbies.
    o.insert("lobbies".into(), b.lobbies.into());
    o.insert("peers".into(), b.peers.into());
    let mut num = |k: &str, v: Option<serde_json::Value>| {
        if let Some(v) = v {
            o.insert(k.into(), v);
        }
    };
    // ⚠ An `f32` widened to JSON's `f64` prints its own imprecision: 7.4
    // becomes 7.400000095367432. Harmless arithmetically and ugly in a log a
    // person reads, so the two floats are rounded to the precision they
    // actually carry.
    let hundredths = |v: f32| -> serde_json::Value {
        ((v as f64 * 100.0).round() / 100.0).into()
    };
    num("load1", b.load1.map(hundredths));
    num("cores", b.cores.map(Into::into));
    num("mem_free_mb", b.mem_free_mb.map(Into::into));
    num("disk_free_mb", b.disk_free_mb.map(Into::into));
    num("egress_bps", b.egress_bps.map(Into::into));
    num("ingress_bps", b.ingress_bps.map(Into::into));
    num("rx_drops", b.rx_drops.map(Into::into));
    num("rx_queue_bytes", b.rx_queue_bytes.map(Into::into));
    num("step_p95_ms", b.step_p95_ms.map(hundredths));
    num("limit_drops", b.limit_drops.map(Into::into));
    serde_json::Value::Object(o)
}

#[cfg(test)]
mod box_tests {
    use super::*;

    /// ⚠ **The `box` object omits what it could not measure** (`floptle/0215`).
    ///
    /// Same rule and same field names as the fleet agent, so the control plane
    /// reads both with one code path. The reason bites harder here: `rx_drops:
    /// 0` from a relay that is keeping up is the best news it has, while the
    /// same `0` from a relay that could not read `/proc/net/udp` is a relay
    /// losing players' packets while reporting perfect health.
    /// ⚠ **The two halves of the state's meaning agree.**
    ///
    /// `refuses_join` and `wakes_on_join` describe complementary sets, and the
    /// policy branches on only one of them. If they ever drifted apart, the
    /// unused one would document behaviour the code does not have.
    #[test]
    fn only_a_sleeping_deployment_wakes_and_everything_else_refuses() {
        use crate::control::ReservedState as S;
        for st in [S::Live, S::Sleeping, S::Stopped, S::Unknown] {
            assert_eq!(st.refuses_join(), !st.wakes_on_join(), "{st:?} disagrees with itself");
        }
        assert!(S::Sleeping.wakes_on_join(), "a sleeping server must wake on a join");
        assert!(S::Stopped.refuses_join(), "a stopped server must not be restarted by a stranger");
        // ⚠ A state this relay is too old to know keeps the code reserved and
        // wakes nothing — a newer control plane must not be able to make an
        // older relay hand somebody's code away.
        assert!(!S::Unknown.wakes_on_join());
    }

    /// **A key's row says where its lobbies are hosted from** (`floptle/0228`):
    /// the developer's question is "is somebody who is not me using my key",
    /// and the control plane cannot count what the relay does not send.
    #[test]
    fn a_usage_row_names_the_addresses_a_key_is_hosting_from() {
        let row = usage_row(&UsageSample {
            key: "fk_live_X".into(),
            lobbies: 3,
            hosts: vec![
                HostAddress { address: "203.0.113.5".parse().unwrap(), lobbies: 2 },
                HostAddress { address: "2001:db8::7".parse().unwrap(), lobbies: 1 },
            ],
            ..Default::default()
        });
        assert_eq!(row["hosts"][0]["address"], "203.0.113.5");
        assert_eq!(row["hosts"][0]["lobbies"], 2);
        assert_eq!(row["hosts"][1]["address"], "2001:db8::7");
        let sum: u64 = row["hosts"].as_array().unwrap().iter().map(|h| h["lobbies"].as_u64().unwrap()).sum();
        assert_eq!(sum, row["lobbies"].as_u64().unwrap(), "hosts partitions lobbies");
        // Present and empty is a fact; absent would be an old relay.
        let bare = usage_row(&UsageSample { key: "fk_live_Y".into(), ..Default::default() });
        assert_eq!(bare["hosts"], serde_json::json!([]));
    }

    #[test]
    fn a_relay_that_could_not_measure_omits_rather_than_reporting_zero() {
        let v = box_json(&RelayBox { host: "relay-1".into(), ..Default::default() });
        let o = v.as_object().expect("an object");
        assert_eq!(o["host"], "relay-1");
        assert!(o.contains_key("version"), "a version is always known: {v}");
        // Occupancy is always known — the relay holds the lobbies itself.
        assert_eq!(o["lobbies"], 0);
        assert_eq!(o["peers"], 0);
        for f in [
            "load1", "cores", "mem_free_mb", "disk_free_mb", "egress_bps",
            "ingress_bps", "rx_drops", "rx_queue_bytes", "step_p95_ms", "limit_drops",
        ] {
            assert!(!o.contains_key(f), "{f} was sent as zero rather than omitted: {v}");
        }
    }

    /// A fully-measured relay sends every field, under the names W consumes.
    #[test]
    fn a_measured_relay_sends_the_shape_the_control_plane_reads() {
        let v = box_json(&RelayBox {
            host: "us-east-relay-1".into(),
            load1: Some(0.91),
            cores: Some(1),
            mem_free_mb: Some(412),
            disk_free_mb: Some(21000),
            egress_bps: Some(41_000_000),
            ingress_bps: Some(6_000_000),
            lobbies: 12,
            peers: 74,
            rx_drops: Some(318),
            rx_queue_bytes: Some(65_536),
            step_p95_ms: Some(7.4),
            limit_drops: Some(9),
        });
        assert_eq!(v["host"], "us-east-relay-1");
        // The relay's own version rides every report, and it is the crate's —
        // not a constant somebody has to remember to bump (`floptle/0232`).
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert!(
            v["version"].as_str().unwrap().split('.').count() == 3,
            "a version W can compare: {}",
            v["version"]
        );
        assert_eq!(v["limit_drops"], 9);
        assert_eq!(v["cores"], 1, "one OCPU is what makes load1 0.91 alarming");
        assert_eq!(v["egress_bps"], 41_000_000u64);
        assert_eq!(v["rx_drops"], 318);
        assert_eq!(v["peers"], 74);
        assert_eq!(v["step_p95_ms"], 7.4);
        println!("{}", serde_json::to_string_pretty(&v).unwrap());
    }
}

/// Why a control-plane call did not answer.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlError {
    /// Unreachable, timed out, or `503 cloud_hosting_unavailable` — the control
    /// plane is **broken, not authoritative**. Never a verdict about a key.
    Unavailable(String),
    /// `401`/`403`: **this relay's own credential was refused.** Not an outage
    /// and not a verdict about anybody's key — a configuration error on this
    /// box, and the only one here that a human has to go and fix.
    ///
    /// It is kept apart from `Unavailable` because the right behaviour differs.
    /// An outage is temporary and the snapshot rides it out; a refused box
    /// token means this relay can never authorize or report anything, so a
    /// relay that has not managed a single successful pull is not a degraded
    /// managed relay — it is an untracked open one wearing the flags, which is
    /// exactly what Ty's rule exists to prevent.
    Denied(String),
    /// It answered, and the answer was not something this relay can use.
    Malformed(String),
}

/// What this relay needs from fopull.com.
pub trait ControlPlane: Send + Sync {
    /// Pull what changed since `cursor` (or everything, with no cursor).
    fn pull_keys(&self, cursor: Option<&str>) -> Result<KeySnapshot, ControlError>;
    /// The cold path: ask about one key the snapshot has never carried.
    fn authorize(&self, key: &str) -> Result<KeyRow, ControlError>;
    /// **Ask the control plane to wake the deployment behind `code`**
    /// (`floptle/0217`). Idempotent and fire-and-forget.
    ///
    /// The answer matters even though the relay cannot act on most of it: only
    /// `Stopped` changes what the joiner is told, and it is the one case where
    /// refusing is honest.
    fn wake(&self, _code: &str) -> Result<WakeOutcome, ControlError> {
        Err(ControlError::Unavailable("this control plane cannot wake".into()))
    }

    /// Report usage, and what this box looks like while carrying it.
    ///
    /// Fire and forget from the caller's point of view.
    fn report_usage(&self, box_: &RelayBox, samples: &[UsageSample])
    -> Result<(), ControlError>;
}

/// The real one: fopull.com over HTTPS, with a per-box token.
pub struct HttpControl {
    base: String,
    region: String,
    token: String,
    timeout: Duration,
}

impl HttpControl {
    pub fn new(base: &str, region: &str, token: &str, timeout: Duration) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            region: region.to_string(),
            token: token.to_string(),
            timeout,
        }
    }

    fn agent(&self) -> ureq::Agent {
        ureq::AgentBuilder::new().timeout(self.timeout).build()
    }

    fn url(&self, tail: &str) -> String {
        format!("{}/api/floptle/v1{tail}", self.base)
    }

    /// The cold path's body → the row the policy stores.
    ///
    /// A named function rather than two lines inside [`HttpControl::authorize`]
    /// because those two lines are the whole of this seam, and inside the
    /// method they are only reachable through a live HTTPS call — which is to
    /// say, not reachable from a test at all. Split out, the mapping is the
    /// thing a guard can hold.
    fn parse_authorize(body: &str, key: &str) -> Result<KeyRow, ControlError> {
        let reply: AuthorizeReply =
            serde_json::from_str(body).map_err(|e| ControlError::Malformed(e.to_string()))?;
        Ok(reply.into_row(key))
    }

    /// Turn a ureq outcome into our two-way split.
    ///
    /// **`503 cloud_hosting_unavailable` is `Unavailable`, not a bad
    /// credential.** A control plane whose tables are missing is broken, and a
    /// relay that read it as "your key is bad" would refuse every host in the
    /// region on the strength of an answer nobody actually gave.
    fn body(res: Result<ureq::Response, ureq::Error>) -> Result<String, ControlError> {
        match res {
            Ok(r) => r.into_string().map_err(|e| ControlError::Malformed(e.to_string())),
            Err(ureq::Error::Status(503, _)) => {
                Err(ControlError::Unavailable("cloud_hosting_unavailable".into()))
            }
            Err(ureq::Error::Status(s @ (401 | 403), _)) => Err(ControlError::Denied(format!(
                "HTTP {s} — this relay's box token was refused"
            ))),
            Err(ureq::Error::Status(s, _)) => Err(ControlError::Malformed(format!("HTTP {s}"))),
            Err(e) => Err(ControlError::Unavailable(e.to_string())),
        }
    }
}

impl ControlPlane for HttpControl {
    fn pull_keys(&self, cursor: Option<&str>) -> Result<KeySnapshot, ControlError> {
        let mut req = self
            .agent()
            .get(&self.url(&format!("/cloud/relay/{}/keys", self.region)))
            .set("Authorization", &format!("Bearer {}", self.token))
            .set("Accept", "application/json");
        if let Some(c) = cursor {
            req = req.query("cursor", c);
        }
        let body = Self::body(req.call())?;
        serde_json::from_str(&body).map_err(|e| ControlError::Malformed(e.to_string()))
    }

    fn authorize(&self, key: &str) -> Result<KeyRow, ControlError> {
        let body = Self::body(
            self.agent()
                .post(&self.url("/cloud/relay/authorize"))
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", "application/json")
                .send_json(ureq::json!({ "key": key, "region": self.region })),
        )?;
        Self::parse_authorize(&body, key)
    }

    fn wake(&self, code: &str) -> Result<WakeOutcome, ControlError> {
        let body = Self::body(
            self.agent()
                .post(&self.url("/cloud/relay/wake"))
                .set("Authorization", &format!("Bearer {}", self.token))
                .set("Accept", "application/json")
                .send_json(ureq::json!({ "code": code })),
        )?;
        // A body that does not parse is not a refusal. The server answered, so
        // the deployment is somebody's — holding the joiner is the safe read.
        let v: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        Ok(WakeOutcome::from_body(
            v.get("woken").and_then(|w| w.as_bool()).unwrap_or(false),
            v.get("reason").and_then(|r| r.as_str()),
        ))
    }

    fn report_usage(
        &self,
        box_: &RelayBox,
        samples: &[UsageSample],
    ) -> Result<(), ControlError> {
        let rows: Vec<_> = samples.iter().map(usage_row).collect();
        Self::body(
            self.agent()
                .post(&self.url("/cloud/relay/usage"))
                .set("Authorization", &format!("Bearer {}", self.token))
                .send_json(ureq::json!({
                    "region": self.region,
                    "box": box_json(box_),
                    "samples": rows,
                })),
        )
        .map(|_| ())
    }
}

/// A key map plus the moment it was last refreshed.
#[derive(Default)]
pub struct KeyTable {
    rows: HashMap<String, KeyRow>,
    /// **False until the first successful pull.** A relay that has never
    /// reached the control plane knows nothing, and must not answer as though
    /// it knows a key is bad.
    pub primed: bool,
}

impl KeyTable {
    pub fn get(&self, key: &str) -> Option<&KeyRow> {
        self.rows.get(key)
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Fold a page in. A `full` page **replaces**; a delta merges and applies
    /// its removals.
    pub fn apply(&mut self, snap: &KeySnapshot) {
        if snap.full {
            self.rows.clear();
        }
        for r in &snap.keys {
            self.rows.insert(r.key.clone(), r.clone());
        }
        for gone in &snap.removed {
            self.rows.remove(gone);
        }
        self.primed = true;
    }

    /// Record a cold-path answer so the next host does not pay for it again.
    pub fn adopt(&mut self, row: KeyRow) {
        self.rows.insert(row.key.clone(), row);
    }
}

/// **The control plane's shapes, pinned against what it actually answers.**
///
/// This is the one seam where a mistake has no symptom on this side. Everything
/// else in the relay fails loudly; a JSON field this file spells differently to
/// fopull.com parses to a default, and a default here is a policy decision —
/// `state` missing reads as `Active`, `ccu_limit` missing reads as 0 which
/// floors to the free tier. So the bodies below are copied from
/// `contracts/cloud-hosting.md` and from W's deployed responses rather than
/// written to match this file, and they are the test.
#[cfg(test)]
mod shape_tests {
    use super::*;

    /// The cold path, verbatim from the contract's §3.
    ///
    /// It carries **no `key`**, says `status` rather than `state`, and says
    /// `over_limit` rather than `account_over_limit` — three ways for a
    /// `KeyRow` to be the wrong type for it, one of which is fatal and two of
    /// which are silent. Watched failing: parsed as a `KeyRow` this is a
    /// missing-field error, and every cold lookup on the real box would have
    /// floored to 20 players with nothing said but "allowing at the free
    /// limit".
    #[test]
    fn the_authorize_reply_parses_and_is_not_a_snapshot_row() {
        // **Every value here differs from its own serde default**, or the
        // assertion below it cannot tell a correct field name from a missing
        // one. The first version of this test used `"over_limit":false` and
        // `"status":"active"` — both the defaults — and it passed cheerfully
        // with the field renamed underneath it. Watched failing to fail, which
        // is how that was found.
        let body = r#"{"game":"forgery","account":"u_15","tier":"indie","ccu_limit":100,
          "regions":["us-east"],"over_limit":true,"status":"deprecated"}"#;
        // Through the same call the HTTPS path uses, not around it.
        let row = HttpControl::parse_authorize(body, "fk_live_ABC").expect("the §3 shape");

        assert_eq!(row.key, "fk_live_ABC", "the key comes from the caller, not the wire");
        assert_eq!(row.game, "forgery");
        assert_eq!(row.state, KeyState::Deprecated, "`status`, not `state` — and not the default");
        assert_eq!(row.ccu_limit, 100, "an Indie plan is not floored to the free tier");
        assert_eq!(row.tier, "indie");
        assert!(row.account_over_limit, "`over_limit`, not `account_over_limit`");

        // And the same body is NOT a snapshot row — if it ever becomes one,
        // this file has two names for one thing again.
        assert!(
            serde_json::from_str::<KeyRow>(body).is_err(),
            "authorize's answer must not silently parse as a snapshot row"
        );
    }

    /// The refusal form: a bad key is a 200 with a status, never a 4xx —
    /// because a 4xx would be indistinguishable from "the control plane is
    /// broken", and the relay fails OPEN on the second.
    #[test]
    fn a_refusal_is_a_status_rather_than_an_error_code() {
        for (body, want) in [
            (r#"{"status":"revoked"}"#, KeyState::Revoked),
            (r#"{"status":"unknown"}"#, KeyState::Unknown),
        ] {
            let row = HttpControl::parse_authorize(body, "fk_live_ABC").expect("the refusal shape");
            assert_eq!(row.state, want);
            assert!(!row.may_host(), "{body} must not host");
        }
    }

    /// The snapshot, from W's §3b — the main path, and the one that carries a
    /// `key` per row because it was not asked about any particular one.
    #[test]
    fn the_key_snapshot_parses_with_every_field_the_policy_reads() {
        let body = r#"{"cursor":"eyJ2IjoxfQ","full":false,"ttl":30,
          "keys":[{"key":"fk_live_ABC","game":"forgery","account":"u_15",
                   "state":"active","expires_at":null,"tier":"indie","ccu_limit":100,
                   "regions":["us-east"],"account_over_limit":false}],
          "removed":["fk_live_GONE"]}"#;
        let snap: KeySnapshot = serde_json::from_str(body).expect("the §3b shape");
        assert_eq!(snap.cursor.as_deref(), Some("eyJ2IjoxfQ"));
        assert!(!snap.full, "a delta merges; a full replaces");
        assert_eq!(snap.removed, ["fk_live_GONE"]);
        let row = &snap.keys[0];
        assert_eq!(row.key, "fk_live_ABC");
        assert_eq!(row.state, KeyState::Active);
        assert_eq!(row.ccu_limit, 100);
        assert_eq!(row.tier, "indie");
        // `ttl` and `expires_at` are fields this relay does not read, and it
        // must not refuse a body for carrying them — the control plane grows
        // fields without asking.
    }

    /// **A revocation arrives as a state, never as an absence.** W carries a
    /// dead key in the delta precisely so a relay holding it is told; parsing
    /// that has to yield a row that cannot host.
    #[test]
    fn a_revoked_key_in_a_delta_is_carried_and_cannot_host() {
        let body = r#"{"cursor":"c2","full":false,
          "keys":[{"key":"fk_live_DEAD","state":"revoked","tier":"free","ccu_limit":20}],
          "removed":[]}"#;
        let snap: KeySnapshot = serde_json::from_str(body).expect("parses");
        assert_eq!(snap.keys[0].state, KeyState::Revoked);
        assert!(!snap.keys[0].may_host(), "a revoked key in the map must stop hosting");
    }

    /// A deprecated key still hosts — a build in the wild is on borrowed time,
    /// not dead. Getting this backwards would take every shipped copy of a game
    /// offline the moment its developer rotated a key.
    #[test]
    fn a_deprecated_key_still_hosts() {
        let row: KeyRow =
            serde_json::from_str(r#"{"key":"fk_live_OLD","state":"deprecated"}"#).expect("parses");
        assert!(row.may_host(), "rotation has a grace window and this is it");
    }
}
