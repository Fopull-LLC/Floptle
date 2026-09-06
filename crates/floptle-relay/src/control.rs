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
}

/// One region's usage for one key, at one moment.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageSample {
    pub key: String,
    pub ccu: u32,
    pub lobbies: u32,
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
    /// Report usage. Fire and forget from the caller's point of view.
    fn report_usage(&self, samples: &[UsageSample]) -> Result<(), ControlError>;
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

    fn report_usage(&self, samples: &[UsageSample]) -> Result<(), ControlError> {
        let rows: Vec<_> = samples
            .iter()
            .map(|s| ureq::json!({ "key": s.key, "ccu": s.ccu, "lobbies": s.lobbies }))
            .collect();
        Self::body(
            self.agent()
                .post(&self.url("/cloud/relay/usage"))
                .set("Authorization", &format!("Bearer {}", self.token))
                .send_json(ureq::json!({ "region": self.region, "samples": rows })),
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
