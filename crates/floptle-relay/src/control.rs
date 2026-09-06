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
        serde_json::from_str(&body).map_err(|e| ControlError::Malformed(e.to_string()))
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
