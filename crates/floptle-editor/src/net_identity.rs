//! Verified player identity, both ends (`contracts/identity-auth.md` §8).
//!
//! A signed-in player joining a server asks fopull.com for a join token that
//! names that server, and the Hello waits for it. A short wait, never a
//! blocked join: fopull.com being slow or down means the player joins
//! unverified, exactly as they did before tokens existed.
//!
//! A server fetches fopull.com's signing keys once, in the background, and
//! checks each token itself. It answers to the addresses players use to reach
//! it: its lobby code on Floptle Cloud, its relay address and code, or the
//! address a direct host was told it has (`net.host{ address = … }`).

#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
use std::sync::mpsc::{Receiver, TryRecvError};

use floptle_core::time::Instant;

use crate::Editor;

/// How long a join waits for its token before going ahead without one.
const JOIN_TOKEN_WAIT: std::time::Duration = std::time::Duration::from_secs(4);

/// How often a server that wants the key set asks for it again.
#[cfg(not(target_arch = "wasm32"))]
const KEY_RETRY: std::time::Duration = std::time::Duration::from_secs(60);

/// A client's Hello, held while its join token is minted.
pub(crate) struct PendingJoinToken {
    rx: Receiver<Result<String, String>>,
    claim: floptle_net::IdentityClaim,
    since: Instant,
}

/// A server's side: the checker its session uses, and what feeds it.
#[derive(Default)]
pub(crate) struct NetIdentity {
    /// `net.host{ address = "host:port" }`: where players reach a direct
    /// host. A direct host cannot know its own public address, and without it
    /// a token for it cannot be checked.
    pub(crate) address: Option<String>,
    /// (lobby code, relay) → does `cloud://CODE` route to this relay? Asked
    /// once per code, because the answer reads the regions list.
    #[cfg(not(target_arch = "wasm32"))]
    cloud_names_us: Option<(String, bool)>,
    /// The join policy asks for verified players, so a key fetch that fails
    /// is a warning, not a footnote.
    pub(crate) required: bool,
    #[cfg(not(target_arch = "wasm32"))]
    check: Option<Arc<floptle_net::JoinTokenCheck>>,
    #[cfg(not(target_arch = "wasm32"))]
    keys_rx: Option<Receiver<Result<String, String>>>,
    #[cfg(not(target_arch = "wasm32"))]
    keys_asked: Option<Instant>,
    /// A failed key fetch has been reported. An offline LAN host retries
    /// every minute, and says so once.
    #[cfg(not(target_arch = "wasm32"))]
    keys_failure_said: bool,
    /// The client's held Hello, when a join is waiting on its token.
    pub(crate) pending: Option<PendingJoinToken>,
    /// The audience of the join `net_join_addr` is starting, for the
    /// `net_join_with` that follows it.
    pub(crate) next_audience: Option<String>,
}

/// The provider whose tokens are honoured: the account's, or production.
fn account_base(ed: &Editor) -> String {
    ed.account.as_ref().map(|a| a.base().to_string()).unwrap_or_else(|| {
        std::env::var("FLOPTLE_ACCOUNT_BASE").unwrap_or_else(|_| floptle_account::DEFAULT_BASE.to_string())
    })
}

impl Editor {
    /// Client: build the joining session. With a signed-in account and an
    /// address a token can name, the Hello is held for the token; otherwise it
    /// goes at once, as it always has.
    pub(crate) fn net_identity_client(
        &mut self,
        transport: Box<dyn floptle_net::Transport>,
    ) -> floptle_net::NetSession {
        let claim = self.net_identity_claim();
        let audience = self.net_ident.next_audience.take();
        let hash = self.input_map_hash();
        match (claim, audience, self.account.as_ref()) {
            (Some(claim), Some(audience), Some(account)) => {
                let (tx, rx) = std::sync::mpsc::channel();
                account.join_token(&audience, JOIN_TOKEN_WAIT, tx);
                self.net_ident.pending = Some(PendingJoinToken { rx, claim, since: Instant::now() });
                floptle_net::NetSession::client_awaiting_identity(transport, hash)
            }
            (claim, _, _) => floptle_net::NetSession::client_as(transport, hash, claim),
        }
    }

    /// Client, each tick: send the held Hello once the token is in, or once
    /// waiting for it has gone on long enough.
    pub(crate) fn net_identity_client_tick(&mut self) {
        let Some(p) = self.net_ident.pending.as_ref() else { return };
        let outcome = match p.rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) if p.since.elapsed() < JOIN_TOKEN_WAIT => return,
            Err(_) => Err("fopull.com did not answer in time".into()),
        };
        let Some(PendingJoinToken { mut claim, .. }) = self.net_ident.pending.take() else { return };
        match outcome {
            Ok(token) => claim.proof = Some(token),
            Err(why) => self.console.push(
                floptle_script::LogLevel::Debug,
                format!("🌐 joining without a join token, so the server sees your account as unverified: {why}"),
                None,
            ),
        }
        if let Some(c) = self.net_play_client.as_mut() {
            c.present_identity(Some(claim));
        }
    }

    /// Server: give a freshly made session the token check, for a host
    /// other machines can reach.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn net_identity_server_setup(&mut self, session: &mut floptle_net::NetSession) {
        let check = floptle_net::JoinTokenCheck::new(&account_base(self));
        session.set_verifier(Box::new(check.clone()));
        self.net_ident.check = Some(check);
        self.net_ident.keys_asked = None;
    }

    /// Server, each tick: keep the addresses the check answers to in step
    /// with the lobby code, and fetch the key set when the check wants it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn net_identity_server_tick(&mut self) {
        let Some(check) = self.net_ident.check.clone() else { return };
        if self.net_server.is_none() {
            self.net_ident.check = None;
            return;
        }
        let mut targets = Vec::new();
        if let (Some(relay), Some(code)) = (self.net_relay_hosting.clone(), self.net_lobby_code.clone()) {
            targets.push(format!("relay://{relay}/{code}"));
            // `cloud://CODE` names this server when a player typing it reaches
            // this relay. Decided by where the server is, not by how hosting
            // was asked for: `floptle serve` on a fleet box names the region's
            // address, never "cloud". A relay under fopull.com counts too:
            // a managed code starts with its own region's letter, so the code
            // this server holds there is that region's code, whatever alias
            // the box was given for the relay.
            let key = format!("{code}@{relay}");
            let routed_here = match &self.net_ident.cloud_names_us {
                Some((k, yes)) if *k == key => *yes,
                _ => {
                    let yes = cloud_code_names(&relay, Self::cloud_relay_for_code(&code).ok().as_deref());
                    self.net_ident.cloud_names_us = Some((key, yes));
                    yes
                }
            };
            if routed_here {
                targets.push(format!("cloud://{code}"));
            }
        }
        if let Some(addr) = &self.net_ident.address {
            targets.push(format!("quic://{addr}"));
        }
        let want: Vec<String> = targets.iter().filter_map(|t| floptle_net::normalise_audience(t)).collect();
        if want != check.audiences() {
            check.set_audiences(&want);
        }

        if let Some(rx) = &self.net_ident.keys_rx {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => self.net_ident.keys_rx = None,
                Ok(fetched) => {
                    self.net_ident.keys_rx = None;
                    let loaded = fetched.and_then(|json| check.set_keys(&json));
                    let quiet = loaded.is_err() && self.net_ident.keys_failure_said && !self.net_ident.required;
                    self.net_ident.keys_failure_said |= loaded.is_err();
                    let (level, line) = match loaded {
                        Ok(n) => (
                            floptle_script::LogLevel::Debug,
                            format!("🌐 fopull.com's signing keys loaded ({n}); signed-in players arrive verified"),
                        ),
                        Err(e) => (
                            if self.net_ident.required {
                                floptle_script::LogLevel::Warn
                            } else {
                                floptle_script::LogLevel::Debug
                            },
                            format!(
                                "🌐 {e}. Players join unverified until the keys load; retrying in {} s",
                                KEY_RETRY.as_secs()
                            ),
                        ),
                    };
                    if !quiet {
                        self.console.push(level, line, None);
                    }
                }
            }
        }
        let due = self.net_ident.keys_asked.is_none_or(|t| t.elapsed() >= KEY_RETRY);
        if check.wants_keys() && self.net_ident.keys_rx.is_none() && due {
            self.net_ident.keys_asked = Some(Instant::now());
            let (tx, rx) = std::sync::mpsc::channel();
            let base = account_base(self);
            let started = std::thread::Builder::new().name("floptle-jwks".into()).spawn(move || {
                let _ = tx.send(floptle_account::join_token::fetch_jwks(&base, std::time::Duration::from_secs(10)));
            });
            if started.is_ok() {
                self.net_ident.keys_rx = Some(rx);
            }
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn net_identity_server_tick(&mut self) {}
}

/// Does `cloud://CODE` name a server holding CODE on `relay`? Yes when the
/// code's region routes players to this relay (`region_relay`), and yes on any
/// relay under fopull.com: a managed code starts with its own region's letter,
/// so a code held there is that region's code whatever alias the box was
/// given. Never on a self-hosted relay, whose code could match a Cloud lobby's.
#[cfg(not(target_arch = "wasm32"))]
fn cloud_code_names(relay: &str, region_relay: Option<&str>) -> bool {
    floptle_account::auth::is_fopull_host(relay) || region_relay.is_some_and(|r| r.eq_ignore_ascii_case(relay))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::cloud_code_names;

    /// A fleet server is started with the region's relay address, never with
    /// "cloud", and its players type `cloud://CODE`. If this answered no, every
    /// Cloud player would arrive unverified and `requireVerified` would refuse
    /// them all.
    #[test]
    fn a_cloud_code_names_a_server_on_a_managed_relay_and_no_other() {
        let region = Some("us-east.relay.fopull.com:7788");
        assert!(cloud_code_names("us-east.relay.fopull.com:7788", region), "the fleet box");
        assert!(cloud_code_names("US-East.Relay.fopull.com:7788", region), "however it is capitalised");
        assert!(cloud_code_names("relay.fopull.com:7788", region), "an alias for the same relay");
        assert!(cloud_code_names("10.0.0.9:7788", Some("10.0.0.9:7788")), "a region listed by address");
        assert!(!cloud_code_names("relay.example.com:7788", region), "a self-hosted relay");
        assert!(!cloud_code_names("relay.example.com:7788", None));
        assert!(!cloud_code_names("fopull.com.evil.example:7788", region), "a name that only looks like ours");
    }
}
