//! Managed mode: the [`RelayPolicy`] Floptle Cloud runs.
//!
//! **Hosting never waits on a network call.** The relay pulls the region's key
//! snapshot every [`PULL_INTERVAL`] and answers `Host` from memory, so the
//! player-facing path has no round trip in it and a control-plane outage
//! degrades to *last known limits* rather than to an unbounded allow. That is
//! the same rule every other box in this system follows — pull, never wait —
//! and `authorize` was the one exception, sitting on the one path a player
//! waits behind.
//!
//! Five rules make it safe, and the first is the one that bites if it is
//! forgotten:
//!
//! 1. **Absent from the snapshot is NOT revoked.** A key minted ten seconds ago
//!    is not in it yet. Only an explicit removal or a non-active state refuses;
//!    anything unknown goes to the cold path. Read absence as refusal and every
//!    developer's first host of a new game fails intermittently for thirty
//!    seconds with a correct-looking key.
//! 2. **A relay with no snapshot at all knows nothing** and must not answer as
//!    though it does — rule 1 again, at boot.
//! 3. **A `full` page replaces rather than merges** ([`KeyTable::apply`]).
//! 4. **The cold path is bounded**: it runs on a worker so it cannot stop the
//!    relay, and on timeout the host is allowed at [`FREE_TIER_CCU`].
//! 5. **Snapshot age is observable.** "Revocation inside 30 s" is only true
//!    while the pull is succeeding; a relay enforcing six-hour-old limits is
//!    behaving correctly but somebody has to be able to see it.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use floptle_net::{HostAdmission, JoinAdmission, RelayPolicy};

use crate::control::{ControlError, ControlPlane, KeyRow, KeyState, KeyTable, UsageSample,
                     FREE_TIER_CCU};

/// How often the key snapshot refreshes. Revocation propagates within this,
/// comfortably inside the contract's 60 s.
pub const PULL_INTERVAL: Duration = Duration::from_secs(30);

/// Usage is coalesced to at most one POST per this interval, per region.
///
/// The control plane's database also runs the live business, so a busy game
/// opening and closing lobbies must not become a write per second. The relay
/// keeps a running sample and flushes on this timer: **send the batch, not the
/// event.**
pub const USAGE_INTERVAL: Duration = Duration::from_secs(10);

/// A cold-path lookup that has not come back yet.
struct InFlight {
    started: Instant,
}

/// What an operator can see from outside the relay's loop.
///
/// **Rule 5 made visible.** "Revocation propagates within thirty seconds" holds
/// only while the pull is succeeding, so a relay that has been cut off for six
/// hours is enforcing six-hour-old limits — correct, and invisible unless it
/// says so. Somebody asking "why is a revoked key still working" needs an
/// answer that is not guesswork.
#[derive(Default)]
pub struct Status {
    /// Keys held locally right now.
    pub keys: usize,
    /// Seconds since the last successful pull, or `None` if there has never
    /// been one — which is emphatically not the same as "fresh".
    pub snapshot_age_s: Option<u64>,
    /// Lines for the journal, drained by the binary.
    pub log: Vec<String>,
}

/// A [`Status`] shared with whoever is running the relay.
pub type StatusHandle = Arc<Mutex<Status>>;

/// What a worker thread finished doing.
enum Done {
    Pulled(Result<crate::control::KeySnapshot, ControlError>),
    Authorized(String, Result<KeyRow, ControlError>),
}

/// Floptle Cloud's admission policy.
pub struct CloudPolicy {
    control: Arc<dyn ControlPlane>,
    region: String,
    /// The region's letter, prefixed onto every lobby code this relay hands out.
    letter: char,
    keys: KeyTable,
    cursor: Option<String>,
    last_pull_ok: Option<Instant>,
    pull_in_flight: bool,
    /// key → the cold-path lookup running for it.
    cold: HashMap<String, InFlight>,
    /// Keys the cold path could not settle in time; allowed at the free floor
    /// so a bounded worst case stays bounded.
    floored: HashSet<String>,
    /// code → key, and code → live peers. The relay does not know what a key
    /// means, so the meter lives here.
    of_lobby: HashMap<String, String>,
    live: HashMap<String, u32>,
    last_usage: Instant,
    /// Deprecated keys already warned about, so a popular game on a rotated key
    /// does not write one journal line per player.
    warned: HashSet<String>,
    tx: Sender<Done>,
    rx: Receiver<Done>,
    /// What the operator sees, updated every tick.
    status: StatusHandle,
}

impl CloudPolicy {
    pub fn new(control: Arc<dyn ControlPlane>, region: &str, letter: char) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            control,
            region: region.to_string(),
            letter,
            keys: KeyTable::default(),
            cursor: None,
            last_pull_ok: None,
            pull_in_flight: false,
            cold: HashMap::new(),
            floored: HashSet::new(),
            of_lobby: HashMap::new(),
            live: HashMap::new(),
            last_usage: Instant::now(),
            warned: HashSet::new(),
            tx,
            rx,
            status: Arc::new(Mutex::new(Status::default())),
        }
    }

    /// A handle onto what this policy knows, for the loop that owns it. Take it
    /// before handing the policy to the relay.
    pub fn status(&self) -> StatusHandle {
        self.status.clone()
    }

    fn say(&mut self, line: String) {
        if let Ok(mut s) = self.status.lock() {
            s.log.push(line);
        }
    }

    /// How stale the key snapshot is, in seconds, or `None` if there has never
    /// been one. Reported with every usage batch and logged, because
    /// "revocation within 30 s" is a promise that holds only while the pull is
    /// succeeding — and "why is a revoked key still working" must have an
    /// answer that is not guesswork.
    pub fn snapshot_age_s(&self) -> Option<u64> {
        self.last_pull_ok.map(|t| t.elapsed().as_secs())
    }

    fn spawn_pull(&mut self) {
        if self.pull_in_flight {
            return;
        }
        self.pull_in_flight = true;
        let (c, tx, cursor) = (self.control.clone(), self.tx.clone(), self.cursor.clone());
        std::thread::spawn(move || {
            let _ = tx.send(Done::Pulled(c.pull_keys(cursor.as_deref())));
        });
    }

    fn spawn_authorize(&mut self, key: &str) {
        if self.cold.contains_key(key) {
            return;
        }
        self.cold.insert(key.to_string(), InFlight { started: Instant::now() });
        let (c, tx, k) = (self.control.clone(), self.tx.clone(), key.to_string());
        std::thread::spawn(move || {
            let r = c.authorize(&k);
            let _ = tx.send(Done::Authorized(k, r));
        });
    }

    fn drain_workers(&mut self) {
        while let Ok(done) = self.rx.try_recv() {
            match done {
                Done::Pulled(Ok(snap)) => {
                    self.pull_in_flight = false;
                    if snap.full {
                        let n = snap.keys.len();
                        self.say(format!("key snapshot: full resync, {n} key(s)"));
                    }
                    // **The cursor only advances on success.** A 503 must not
                    // move it, or a deploy window silently costs us the deltas
                    // that happened during it.
                    if snap.cursor.is_some() {
                        self.cursor = snap.cursor.clone();
                    }
                    self.keys.apply(&snap);
                    self.last_pull_ok = Some(Instant::now());
                    // A key we had to floor is now properly known.
                    self.floored.retain(|k| self.keys.get(k).is_none());
                }
                Done::Pulled(Err(e)) => {
                    self.pull_in_flight = false;
                    let age =
                        self.snapshot_age_s().map(|s| s.to_string()).unwrap_or("never".into());
                    self.say(format!("key snapshot: {e:?} — serving the last one ({age}s old)"));
                }
                Done::Authorized(key, Ok(row)) => {
                    self.cold.remove(&key);
                    self.keys.adopt(row);
                }
                Done::Authorized(key, Err(e)) => {
                    self.cold.remove(&key);
                    // Could not ask. Not the developer's fault and not a
                    // verdict — floor them and let them play.
                    let k = short(&key);
                    self.say(format!("authorize({k}): {e:?} — allowing at the free limit"));
                    self.floored.insert(key);
                }
            }
        }
    }

    /// The limit in force for a key, and whether it may host at all.
    fn verdict(&mut self, key: &str) -> HostAdmission {
        if let Some(row) = self.keys.get(key) {
            if !row.may_host() {
                return HostAdmission::Refuse {
                    reason: "That game key is not one Floptle Cloud recognises any more. \
                             Check project.ron, or connect the project at fopull.com/cloud."
                        .into(),
                };
            }
            if !row.regions.is_empty() && !row.regions.iter().any(|r| r == &self.region) {
                return HostAdmission::Refuse {
                    reason: format!(
                        "Floptle Cloud: this game's plan does not include the {} region. \
                         Upgrade at fopull.com/cloud, or host in a region it covers.",
                        self.region
                    ),
                };
            }
            if row.account_over_limit {
                return HostAdmission::Refuse {
                    reason: format!(
                        "Floptle Cloud: this account is at its {}-player limit on the {} \
                         plan. Upgrade at fopull.com/cloud.",
                        row.ccu_limit, row.tier
                    ),
                };
            }
            let deprecated = row.state == KeyState::Deprecated;
            if deprecated && self.warned.insert(key.to_string()) {
                let k = short(key);
                self.say(format!(
                    "key {k} is deprecated — builds in the wild are on borrowed time"
                ));
            }
            return HostAdmission::Allow { prefix: Some(self.letter) };
        }
        // The cold path already gave up on this one: allow, bounded.
        if self.floored.contains(key) {
            return HostAdmission::Allow { prefix: Some(self.letter) };
        }
        HostAdmission::Pending
    }
}

/// Enough of a key to identify it in a log line, and not enough to be a leak.
fn short(key: &str) -> String {
    key.chars().take(16).collect::<String>() + "…"
}

impl RelayPolicy for CloudPolicy {
    fn admit_host(&mut self, key: Option<&str>, _build: Option<&str>) -> HostAdmission {
        let Some(key) = key.filter(|k| !k.is_empty()) else {
            return HostAdmission::Refuse {
                reason: "This relay is Floptle Cloud. Connect your project to a game at \
                         fopull.com/cloud, or self-host floptle-relay."
                    .into(),
            };
        };
        // **Rule 2.** No snapshot has ever landed, so this relay knows nothing
        // — it must ask rather than assume, whatever the key is.
        if !self.keys.primed {
            self.spawn_authorize(key);
            return self.verdict(key);
        }
        // **Rule 1.** A key the snapshot does not carry has NOT been revoked;
        // it may simply have been minted since the last pull. Ask.
        if self.keys.get(key).is_none() && !self.floored.contains(key) {
            self.spawn_authorize(key);
        }
        self.verdict(key)
    }

    fn admit_join(&mut self, code: &str) -> JoinAdmission {
        let Some(key) = self.of_lobby.get(code) else { return JoinAdmission::Allow };
        let limit = self
            .keys
            .get(key)
            .map(|r| r.ccu_limit)
            .filter(|l| *l > 0)
            .unwrap_or(FREE_TIER_CCU);
        // The host counts against the cap alongside its clients.
        let here = self.live.get(code).copied().unwrap_or(0) + 1;
        if here >= limit {
            let (game, tier) = self
                .keys
                .get(key)
                .map(|r| (r.game.clone(), r.tier.clone()))
                .unwrap_or_else(|| (String::new(), "free".into()));
            let who = if game.is_empty() { "this game".into() } else { game };
            return JoinAdmission::Refuse {
                reason: format!(
                    "Floptle Cloud: {who} is at its {limit}-player limit on the {tier} plan. \
                     Upgrade at fopull.com/cloud."
                ),
            };
        }
        JoinAdmission::Allow
    }

    fn lobby_opened(&mut self, code: &str, key: Option<&str>) {
        self.live.insert(code.to_string(), 0);
        if let Some(k) = key {
            self.of_lobby.insert(code.to_string(), k.to_string());
        }
    }

    fn lobby_closed(&mut self, code: &str) {
        self.live.remove(code);
        self.of_lobby.remove(code);
    }

    fn peer_joined(&mut self, code: &str) {
        *self.live.entry(code.to_string()).or_insert(0) += 1;
    }

    fn peer_left(&mut self, code: &str) {
        if let Some(n) = self.live.get_mut(code) {
            *n = n.saturating_sub(1);
        }
    }

    fn tick(&mut self) {
        self.drain_workers();
        if let Ok(mut s) = self.status.lock() {
            s.keys = self.keys.len();
            s.snapshot_age_s = self.last_pull_ok.map(|t| t.elapsed().as_secs());
        }
        // Rule 4: a cold lookup that has outlived the host waiting on it stops
        // being worth waiting for.
        let stale: Vec<String> = self
            .cold
            .iter()
            .filter(|(_, f)| f.started.elapsed() >= floptle_net::HOST_DECISION_DEADLINE)
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.cold.remove(&k);
            self.floored.insert(k);
        }
        let due = self.last_pull_ok.map(|t| t.elapsed() >= PULL_INTERVAL).unwrap_or(true);
        if due {
            self.spawn_pull();
        }
        if self.last_usage.elapsed() >= USAGE_INTERVAL {
            self.last_usage = Instant::now();
            // One POST for the whole region, built from the running counts —
            // never one per lobby event.
            let mut by_key: HashMap<String, (u32, u32)> = HashMap::new();
            for (code, key) in &self.of_lobby {
                let e = by_key.entry(key.clone()).or_insert((0, 0));
                e.0 += self.live.get(code).copied().unwrap_or(0) + 1; // + the host
                e.1 += 1;
            }
            if !by_key.is_empty() {
                let samples: Vec<UsageSample> = by_key
                    .into_iter()
                    .map(|(key, (ccu, lobbies))| UsageSample { key, ccu, lobbies })
                    .collect();
                let c = self.control.clone();
                std::thread::spawn(move || {
                    let _ = c.report_usage(&samples);
                });
            }
        }
    }
}

/// The five rules that make a pulled snapshot safe to authorize from, plus the
/// outage behaviour underneath them.
///
/// Every one of these is about a case where the obvious implementation is
/// wrong in a way that only shows up in production: a key that is absent
/// because it is new, a relay that has never pulled, an outage that must not
/// become free hosting, a cold lookup that never comes back.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::KeySnapshot;
    use std::sync::Mutex;

    const KEY: &str = "fk_live_KNOWNKEYFORTHETESTS000000000";
    const NEW: &str = "fk_live_MINTEDTENSECONDSAGO000000000";

    /// A control plane that answers from fields a test sets, and counts what it
    /// was asked. No network, no timing games.
    #[derive(Default)]
    struct Fake {
        snapshot: Mutex<Option<KeySnapshot>>,
        /// What `pull_keys` should fail with, if anything.
        pull_err: Mutex<Option<ControlError>>,
        /// What `authorize` answers, by key. Absent = it never answers at all,
        /// which is the "control plane is hanging" case.
        cold: Mutex<HashMap<String, Result<KeyRow, ControlError>>>,
        cursors_seen: Mutex<Vec<Option<String>>>,
        usage_posts: Mutex<Vec<Vec<UsageSample>>>,
    }

    fn row(key: &str, limit: u32) -> KeyRow {
        KeyRow {
            key: key.into(),
            game: "forgery".into(),
            state: KeyState::Active,
            tier: "free".into(),
            ccu_limit: limit,
            regions: vec!["us-east".into()],
            account_over_limit: false,
        }
    }

    impl ControlPlane for Fake {
        fn pull_keys(&self, cursor: Option<&str>) -> Result<KeySnapshot, ControlError> {
            self.cursors_seen.lock().unwrap().push(cursor.map(str::to_string));
            if let Some(e) = self.pull_err.lock().unwrap().clone() {
                return Err(e);
            }
            match self.snapshot.lock().unwrap().clone() {
                Some(s) => Ok(s),
                None => Ok(KeySnapshot {
                    cursor: Some("c0".into()),
                    full: true,
                    keys: vec![],
                    removed: vec![],
                }),
            }
        }
        fn authorize(&self, key: &str) -> Result<KeyRow, ControlError> {
            match self.cold.lock().unwrap().get(key).cloned() {
                Some(r) => r,
                // Nothing configured: hang, the way a wedged control plane does.
                None => {
                    std::thread::sleep(Duration::from_secs(30));
                    Err(ControlError::Unavailable("never answered".into()))
                }
            }
        }
        fn report_usage(&self, samples: &[UsageSample]) -> Result<(), ControlError> {
            self.usage_posts.lock().unwrap().push(samples.to_vec());
            Ok(())
        }
    }

    /// Tick until `f` is satisfied, or give up. Real threads are doing the
    /// work, so the loop is how the answers arrive.
    fn settle(p: &mut CloudPolicy, f: impl Fn(&mut CloudPolicy) -> bool) -> bool {
        for _ in 0..200 {
            p.tick();
            if f(p) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn policy(fake: Arc<Fake>) -> CloudPolicy {
        CloudPolicy::new(fake, "us-east", 'U')
    }

    /// Make the next `tick` pull. A settled policy waits `PULL_INTERVAL`, and a
    /// test that changed what the control plane says would otherwise be
    /// asserting against a snapshot half a minute in the future.
    fn due_now(p: &mut CloudPolicy) {
        p.last_pull_ok = Some(Instant::now() - PULL_INTERVAL - Duration::from_secs(1));
    }

    /// **Rule 1, and the one that bites.** A key minted since the last pull is
    /// not in the snapshot — and a relay that read "not in my map" as "refuse"
    /// would fail every developer's first host of a new game, intermittently,
    /// for thirty seconds, with a key that is perfectly good.
    #[test]
    fn a_key_the_snapshot_has_never_carried_is_asked_about_not_refused() {
        let fake = Arc::new(Fake::default());
        fake.cold.lock().unwrap().insert(NEW.into(), Ok(row(NEW, 20)));
        let mut p = policy(fake);
        // Prime it, so this is genuinely "absent from a snapshot we have".
        assert!(settle(&mut p, |p| p.snapshot_age_s().is_some()), "never pulled");

        let first = p.admit_host(Some(NEW), None);
        assert_eq!(first, HostAdmission::Pending, "an unknown key must be asked about, not judged");
        assert!(
            settle(&mut p, |p| p.admit_host(Some(NEW), None)
                == HostAdmission::Allow { prefix: Some('U') }),
            "the cold path never resolved the new key"
        );
    }

    /// **Rule 2.** A relay that has never reached the control plane knows
    /// nothing at all, and must not answer as though it knows a key is bad.
    #[test]
    fn a_relay_with_no_snapshot_yet_asks_rather_than_refusing() {
        let fake = Arc::new(Fake::default());
        *fake.pull_err.lock().unwrap() = Some(ControlError::Unavailable("boot".into()));
        fake.cold.lock().unwrap().insert(KEY.into(), Ok(row(KEY, 20)));
        let mut p = policy(fake);
        assert_eq!(p.snapshot_age_s(), None, "nothing has landed");
        assert_eq!(
            p.admit_host(Some(KEY), None),
            HostAdmission::Pending,
            "with no snapshot, every key is a question"
        );
    }

    /// **Rule 3.** A `full` page replaces. A relay that merged one would keep
    /// serving keys the control plane has forgotten — which, since a full page
    /// is what an unrecognised cursor gets answered with, is exactly when it
    /// matters.
    #[test]
    fn a_full_snapshot_replaces_rather_than_merging() {
        let fake = Arc::new(Fake::default());
        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c1".into()),
            full: true,
            keys: vec![row(KEY, 20)],
            removed: vec![],
        });
        let mut p = policy(fake.clone());
        assert!(settle(&mut p, |p| p.keys.len() == 1), "the first page never landed");

        // A full page that no longer carries KEY.
        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c2".into()),
            full: true,
            keys: vec![row(NEW, 20)],
            removed: vec![],
        });
        due_now(&mut p);
        assert!(
            settle(&mut p, |p| p.keys.len() == 1 && p.keys.get(KEY).is_none()),
            "the replaced page still holds the old key: {} key(s)",
            p.keys.len()
        );
    }

    /// **An explicit removal IS a revocation** — the other half of rule 1, and
    /// the reason rule 1 is safe. Absence is a question; `removed` is an
    /// answer.
    #[test]
    fn an_explicitly_removed_key_stops_hosting() {
        let fake = Arc::new(Fake::default());
        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c1".into()),
            full: true,
            keys: vec![row(KEY, 20)],
            removed: vec![],
        });
        let mut p = policy(fake.clone());
        assert!(settle(&mut p, |p| p.keys.len() == 1));
        assert_eq!(p.admit_host(Some(KEY), None), HostAdmission::Allow { prefix: Some('U') });

        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c2".into()),
            full: false,
            keys: vec![],
            removed: vec![KEY.into()],
        });
        due_now(&mut p);
        // It hangs on the cold path rather than refusing outright, which is
        // rule 1 doing its job — but it does NOT host.
        assert!(
            settle(&mut p, |p| p.keys.get(KEY).is_none()),
            "the removal never applied"
        );
        assert_ne!(
            p.admit_host(Some(KEY), None),
            HostAdmission::Allow { prefix: Some('U') },
            "a removed key must not keep hosting"
        );
    }

    /// **Rule 4.** A cold lookup that never comes back must not leave a host
    /// waiting forever: it is floored to the free tier, which is generous to a
    /// real developer and worthless to anyone trying to get free hosting out of
    /// an outage.
    #[test]
    fn a_cold_lookup_that_never_answers_is_floored_rather_than_left_hanging() {
        let fake = Arc::new(Fake::default());
        // `cold` has no entry for NEW, so `authorize` hangs.
        let mut p = policy(fake);
        assert!(settle(&mut p, |p| p.snapshot_age_s().is_some()));
        assert_eq!(p.admit_host(Some(NEW), None), HostAdmission::Pending);
        // Pretend the deadline passed rather than waiting it out.
        p.cold.get_mut(NEW).unwrap().started =
            Instant::now() - floptle_net::HOST_DECISION_DEADLINE - Duration::from_millis(1);
        p.tick();
        assert_eq!(
            p.admit_host(Some(NEW), None),
            HostAdmission::Allow { prefix: Some('U') },
            "a host must not wait on a control plane that is not answering"
        );
    }

    /// **Rule 5.** How stale the snapshot is has to be visible — "revocation
    /// within 30 s" is a promise that holds only while the pull succeeds, and
    /// "why is a revoked key still working" needs an answer that is not
    /// guesswork.
    #[test]
    fn snapshot_age_is_observable_and_starts_unknown() {
        let fake = Arc::new(Fake::default());
        let mut p = policy(fake);
        assert_eq!(p.snapshot_age_s(), None, "never pulled is not the same as fresh");
        assert!(settle(&mut p, |p| p.snapshot_age_s().is_some()), "never pulled");
    }

    /// **An outage is not a bad credential, and it must not move the cursor.**
    /// A 503 while the control plane's tables are missing is a deploy window;
    /// advancing the cursor through one would silently lose the deltas that
    /// happened during it.
    #[test]
    fn an_outage_serves_the_last_snapshot_and_does_not_advance_the_cursor() {
        let fake = Arc::new(Fake::default());
        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c1".into()),
            full: true,
            keys: vec![row(KEY, 20)],
            removed: vec![],
        });
        let mut p = policy(fake.clone());
        assert!(settle(&mut p, |p| p.keys.len() == 1));

        *fake.pull_err.lock().unwrap() =
            Some(ControlError::Unavailable("cloud_hosting_unavailable".into()));
        p.last_pull_ok = Some(Instant::now() - PULL_INTERVAL - Duration::from_secs(1));
        for _ in 0..5 {
            p.tick();
            std::thread::sleep(Duration::from_millis(10));
        }
        // Still serving real limits, from the snapshot it holds.
        assert_eq!(
            p.admit_host(Some(KEY), None),
            HostAdmission::Allow { prefix: Some('U') },
            "an outage must degrade to last-known limits, not to a refusal"
        );
        let seen = fake.cursors_seen.lock().unwrap().clone();
        assert!(
            seen.iter().skip(1).all(|c| c.as_deref() == Some("c1")),
            "the cursor moved through an outage: {seen:?}"
        );
    }

    /// Ty's rule at this layer: no key, no managed relay — and the sentence
    /// names both ways out.
    #[test]
    fn a_keyless_host_is_refused_with_somewhere_to_go() {
        let mut p = policy(Arc::new(Fake::default()));
        let r = p.admit_host(None, None);
        match r {
            HostAdmission::Refuse { reason } => {
                assert!(reason.contains("fopull.com/cloud"), "{reason}");
                assert!(reason.contains("self-host floptle-relay"), "{reason}");
            }
            other => panic!("a keyless host must be refused, got {other:?}"),
        }
        // An empty string is not a key either.
        assert!(matches!(p.admit_host(Some(""), None), HostAdmission::Refuse { .. }));
    }

    /// **Usage is a batch on a timer, never an event per lobby.** The control
    /// plane's database also runs the live business, and a busy game opening
    /// and closing lobbies would otherwise become a write per second.
    #[test]
    fn usage_is_one_post_per_interval_however_busy_the_relay_is() {
        let fake = Arc::new(Fake::default());
        *fake.snapshot.lock().unwrap() = Some(KeySnapshot {
            cursor: Some("c1".into()),
            full: true,
            keys: vec![row(KEY, 20)],
            removed: vec![],
        });
        let mut p = policy(fake.clone());
        assert!(settle(&mut p, |p| p.keys.len() == 1));
        p.lobby_opened("UABCDE", Some(KEY));

        // A hundred lobby events, and the clock has not moved on.
        for _ in 0..100 {
            p.peer_joined("UABCDE");
            p.peer_left("UABCDE");
            p.tick();
        }
        assert!(
            fake.usage_posts.lock().unwrap().is_empty(),
            "lobby churn must not post; only the timer posts"
        );

        p.last_usage = Instant::now() - USAGE_INTERVAL - Duration::from_millis(1);
        p.tick();
        for _ in 0..40 {
            if !fake.usage_posts.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let posts = fake.usage_posts.lock().unwrap().clone();
        assert_eq!(posts.len(), 1, "one batch for the whole region, got {posts:?}");
        assert_eq!(posts[0].len(), 1, "one row per key");
        assert_eq!(posts[0][0].key, KEY);
        assert_eq!(posts[0][0].lobbies, 1);
    }
}
