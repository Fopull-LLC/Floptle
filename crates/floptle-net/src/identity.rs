//! Who a peer is, and whether this server will have them (`floptle/0183`).
//!
//! Two gaps that are really one: a server did not know **who** a peer was, and
//! could not do anything about it if it did. A peer was a transport id plus
//! whatever display name the game's own handshake carried, so a dedicated
//! server could not ban, allow-list, keep per-account statistics, or recognise
//! a returning player after a disconnect except by trusting a string the client
//! typed. And the Lua net surface had no `kick`.
//!
//! ## Asserted is not verified, and the difference is said out loud
//!
//! A signed-in client presents its account's public claim — subject id, display
//! name, tier. Anyone can send those bytes. Turning a claim into an identity
//! needs a credential the SERVER can check with the provider, scoped so that
//! presenting it to a game server does not hand that server the account: a
//! full-scope access token would let any server you join spend your Fobucks and
//! read your mail.
//!
//! `contracts/identity-auth.md` has no such credential today — every route in
//! it is the account holder talking to fopull.com about itself. So this crate
//! ships the whole shape (the claim travels, the server records it, the policy
//! consults it, `net.identity` reports it) with `verified: false` on every
//! claim, and a [`Verifier`] seam for the moment the provider can answer. What
//! it does **not** do is quietly report `verified: true` for a string somebody
//! typed — a moderation tool that lies about its own confidence is worse than
//! no moderation tool, because a server operator acts on it.

use std::collections::HashSet;

use crate::wire::IdentityClaim;

/// What the server concluded about a peer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The account's stable subject id — `None` for an anonymous peer.
    pub id: Option<String>,
    /// The account's display name. Empty for an anonymous peer; a game that
    /// wants one anyway asks the client for it in its own handshake, and knows
    /// it is doing so.
    pub name: String,
    /// free | indie | studio. Empty when unknown.
    pub tier: String,
    /// Has the claim been checked with the provider? See the module docs: this
    /// is `false` for every claim until an audience-scoped credential exists,
    /// and a caller must treat `false` as "this peer says so".
    pub verified: bool,
}

impl Identity {
    /// Anonymous: a LAN or friends game with nobody signed in. A normal state,
    /// not an error.
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// A short human label for a log line — the id if there is one, else that
    /// there isn't.
    pub fn label(&self) -> String {
        match (&self.id, self.verified) {
            (Some(id), true) => format!("{id} (verified)"),
            (Some(id), false) => format!("{id} (unverified — asserted by the client)"),
            (None, _) => "an anonymous peer".to_string(),
        }
    }
}

/// Turns a claim into an [`Identity`], by asking the provider.
///
/// A seam rather than a function so the check can be swapped: a test needs a
/// deterministic one, a dedicated server needs a real one, and an editor
/// hosting a friends game needs none at all.
pub trait Verifier: Send {
    /// `None` = the client presented nothing.
    fn verify(&self, claim: Option<&IdentityClaim>) -> Identity;
}

/// The default: record what the client said, and mark it unverified.
///
/// This is not a stub that will one day be filled in with a lie. It is the
/// honest answer while no verification route exists — the claim is carried and
/// reported, and every consumer is told it has not been checked.
pub struct AssertedOnly;

impl Verifier for AssertedOnly {
    fn verify(&self, claim: Option<&IdentityClaim>) -> Identity {
        match claim {
            None => Identity::anonymous(),
            Some(c) => Identity {
                id: Some(c.id.clone()),
                name: c.name.clone(),
                tier: c.tier.clone(),
                // Deliberately ignores `c.proof`: there is nothing to check it
                // against yet, and accepting a proof nobody validated would be
                // strictly worse than refusing to claim verification at all.
                verified: false,
            },
        }
    }
}

/// Who this server will admit, consulted BEFORE a join is accepted.
///
/// Before-not-after is the whole point of the allow/deny half: kicking somebody
/// each time they reconnect is not a ban, it is a chore.
#[derive(Clone, Debug, Default)]
pub struct JoinPolicy {
    /// Refuse anyone who presented no account claim at all.
    pub require_identity: bool,
    /// If non-empty, ONLY these account ids may join.
    pub allow: HashSet<String>,
    /// These account ids may never join.
    pub deny: HashSet<String>,
}

impl JoinPolicy {
    /// `Some(reason)` = refuse this peer, and the reason reaches the client so
    /// its UI can say why rather than showing a generic drop.
    pub fn refuse(&self, who: &Identity) -> Option<String> {
        let Some(id) = who.id.as_deref() else {
            return self.require_identity.then(|| {
                "this server requires a signed-in account — sign in and try again".to_string()
            });
        };
        if self.deny.contains(id) {
            return Some("this account is not allowed on this server".to_string());
        }
        if !self.allow.is_empty() && !self.allow.contains(id) {
            return Some("this server is invite-only".to_string());
        }
        None
    }

    /// Is anything about this policy actually being enforced?
    pub fn is_active(&self) -> bool {
        self.require_identity || !self.allow.is_empty() || !self.deny.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim(id: &str) -> IdentityClaim {
        IdentityClaim {
            id: id.into(),
            name: "Ty".into(),
            tier: "indie".into(),
            proof: Some("a token nobody can check".into()),
        }
    }

    /// The property the module exists to keep honest. A server operator BANS
    /// people on the strength of this flag; reporting `true` for a string the
    /// client typed would make every ban a guess.
    #[test]
    fn a_claim_nobody_could_check_is_never_reported_as_verified() {
        let who = AssertedOnly.verify(Some(&claim("user_123")));
        assert_eq!(who.id.as_deref(), Some("user_123"), "the claim is still carried");
        assert!(!who.verified, "…and never dressed up as proof");
        assert!(who.label().contains("unverified"), "the log line says so too");
    }

    #[test]
    fn anonymous_play_is_a_normal_state() {
        let who = AssertedOnly.verify(None);
        assert_eq!(who, Identity::anonymous());
        assert!(JoinPolicy::default().refuse(&who).is_none(), "a LAN game still works");
    }

    #[test]
    fn requiring_an_account_refuses_an_anonymous_peer_with_a_reason() {
        let p = JoinPolicy { require_identity: true, ..Default::default() };
        let reason = p.refuse(&Identity::anonymous()).expect("refused");
        assert!(reason.contains("sign in"), "the client can show this: {reason}");
        // …and still admits somebody who presented one.
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("user_1")))).is_none());
    }

    #[test]
    fn a_denied_account_is_refused_at_the_door_not_kicked_afterwards() {
        let mut p = JoinPolicy::default();
        p.deny.insert("griefer".into());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("griefer")))).is_some());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("someone_else")))).is_none());
    }

    #[test]
    fn an_allow_list_is_only_a_list_when_it_has_something_in_it() {
        let mut p = JoinPolicy::default();
        // An EMPTY allow list means "no allow list", not "nobody" — the other
        // reading turns a mistyped config into a server nobody can join, with
        // no message that says so.
        assert!(!p.is_active());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("anyone")))).is_none());
        p.allow.insert("friend".into());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("friend")))).is_none());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("stranger")))).is_some());
    }

    /// Deny beats allow. Otherwise "on the invite list, then banned" resolves
    /// by whichever check ran first.
    #[test]
    fn deny_wins_over_allow() {
        let mut p = JoinPolicy::default();
        p.allow.insert("both".into());
        p.deny.insert("both".into());
        assert!(p.refuse(&AssertedOnly.verify(Some(&claim("both")))).is_some());
    }
}
