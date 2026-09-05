//! floptle/0183 — a session peer has an identity, and a server can remove one.
//!
//! Two gaps that are really one: a server did not know **who** a peer was, and
//! could not do anything about it if it did. The Lua net surface had no `kick`,
//! and `Transport::disconnect` was a method on one concrete transport that
//! nothing exposed.
//!
//! The honesty criterion is as important as the mechanism. A server operator
//! BANS people on the strength of `verified`; a claim reported as verified when
//! nothing checked it would make every ban a guess with a confident label on it.

use floptle_core::World;
use floptle_net::{
    Identity, IdentityClaim, JoinPolicy, MemoryHub, NetEvent, NetSession, Verifier,
};

/// A claim carrying a `proof` nobody can check — which is the shape that
/// matters. A claim with no proof at all is obviously unverified; the trap is a
/// credential that LOOKS like one and was never validated against anything.
fn claim(id: &str) -> IdentityClaim {
    IdentityClaim {
        id: id.into(),
        name: "Ty".into(),
        tier: "indie".into(),
        proof: Some("looks official, checked by nobody".into()),
    }
}

/// The provider check that does not exist yet, stood in for so the seam is
/// exercised rather than merely declared.
struct TrustsOne(&'static str);

impl Verifier for TrustsOne {
    fn verify(&self, c: Option<&IdentityClaim>) -> Identity {
        match c {
            Some(c) if c.id == self.0 => Identity {
                id: Some(c.id.clone()),
                name: c.name.clone(),
                tier: c.tier.clone(),
                verified: true,
            },
            Some(c) => Identity {
                id: Some(c.id.clone()),
                name: c.name.clone(),
                tier: c.tier.clone(),
                verified: false,
            },
            None => Identity::anonymous(),
        }
    }
}

struct Sess {
    server: NetSession,
    world: World,
}

fn server() -> Sess {
    Sess { server: NetSession::server(Box::new(MemoryHub::new().server_endpoint()), 0), world: World::default() }
}

/// A server plus one client that presented `who`, past the handshake.
fn joined(policy: JoinPolicy, who: Option<IdentityClaim>) -> (NetSession, World, NetSession, World) {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    server.set_join_policy(policy);
    let mut client = NetSession::client_as(Box::new(hub.connect()), 0, who);
    let (sworld, mut cworld) = (World::default(), World::default());
    for t in 1..3 {
        server.tick_server(&sworld, t);
        client.tick_client(&mut cworld);
    }
    (server, sworld, client, cworld)
}

#[test]
fn a_signed_in_peer_arrives_with_an_account_id() {
    let (server, _sw, _c, _cw) = joined(JoinPolicy::default(), Some(claim("user_123")));
    let peer = server.peers()[0];
    let who = server.identity(peer).expect("the server knows who joined");
    assert_eq!(who.id.as_deref(), Some("user_123"));
    assert_eq!(who.name, "Ty");
    assert_eq!(who.tier, "indie");
}

/// The one that matters most. Nothing checked that claim, so nothing may say it
/// did — see the module docs.
#[test]
fn an_unchecked_claim_is_reported_unverified() {
    let (server, _sw, _c, _cw) = joined(JoinPolicy::default(), Some(claim("user_123")));
    let who = server.identity(server.peers()[0]).unwrap();
    assert!(!who.verified, "the engine has no way to check this yet, and says so");
}

#[test]
fn a_verifier_that_can_check_reports_verified() {
    // The seam works: when a provider check exists, plugging it in is all it
    // takes for `verified` to start meaning something.
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    server.set_verifier(Box::new(TrustsOne("real_user")));
    let mut good = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim("real_user")));
    let mut bad = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim("impostor")));
    let (sw, mut gw, mut bw) = (World::default(), World::default(), World::default());
    for t in 1..3 {
        server.tick_server(&sw, t);
        good.tick_client(&mut gw);
        bad.tick_client(&mut bw);
    }
    let seen: Vec<(Option<String>, bool)> = server
        .peers()
        .iter()
        .filter_map(|&p| server.identity(p))
        .map(|i| (i.id.clone(), i.verified))
        .collect();
    assert!(seen.contains(&(Some("real_user".into()), true)));
    assert!(seen.contains(&(Some("impostor".into()), false)));
}

#[test]
fn anonymous_play_still_works() {
    let (server, _sw, client, _cw) = joined(JoinPolicy::default(), None);
    assert_eq!(server.peers().len(), 1, "a LAN game with nobody signed in still joins");
    assert!(client.is_connected());
    let who = server.identity(server.peers()[0]).unwrap();
    assert_eq!(who.id, None);
    assert!(!who.verified, "anonymous is a normal state, not an error");
}

#[test]
fn requiring_an_account_refuses_an_anonymous_join_with_words() {
    let policy = JoinPolicy { require_identity: true, ..Default::default() };
    let (server, _sw, mut client, mut cw) = joined(policy, None);
    assert!(server.peers().is_empty(), "never admitted");
    client.tick_client(&mut cw);
    let refusal = client
        .take_events()
        .into_iter()
        .find_map(|e| match e {
            NetEvent::Disconnected(why) => Some(why),
            _ => None,
        })
        .expect("the client is told why, not merely dropped");
    assert!(refusal.contains("sign in"), "a UI can show this: {refusal}");
}

#[test]
fn a_denied_account_never_gets_on_the_roster() {
    let mut policy = JoinPolicy::default();
    policy.deny.insert("griefer".into());
    let (server, _sw, _c, _cw) = joined(policy, Some(claim("griefer")));
    assert!(server.peers().is_empty(), "a ban is a closed door, not a kick you repeat");
}

#[test]
fn kicking_removes_the_peer_and_tells_it_why() {
    let (mut server, sw, mut client, mut cw) = joined(JoinPolicy::default(), Some(claim("u")));
    let peer = server.peers()[0];
    let _ = server.take_events();

    assert!(server.kick(peer, "griefing the objective"));
    assert!(server.peers().is_empty(), "off the roster on the server's own side of the wire");

    // The server's own event carries the reason, so `playerLeft` can too.
    let left = server.take_events().into_iter().find_map(|e| match e {
        NetEvent::PeerLeft(p, why) => Some((p, why)),
        _ => None,
    });
    assert_eq!(left, Some((peer, Some("griefing the objective".into()))));

    server.tick_server(&sw, 5);
    client.tick_client(&mut cw);
    let events = client.take_events();
    assert!(
        events.iter().any(|e| matches!(e, NetEvent::Kicked(why) if why.contains("griefing"))),
        "the kicked player is owed an explanation, not a generic drop: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, NetEvent::Disconnected(_))),
        "…and every teardown path written before this still fires"
    );
    assert!(!client.is_connected());
}

#[test]
fn kicking_a_peer_that_is_not_here_says_so_rather_than_pretending() {
    let mut s = server();
    assert!(!s.server.kick(42, "who?"));
    let _ = &s.world;
}

/// A dedicated server has nobody watching a Console, so a moderation decision
/// that is not written down did not happen as far as anyone can tell.
#[test]
fn refusals_and_kicks_are_logged_in_words() {
    let mut policy = JoinPolicy::default();
    policy.deny.insert("griefer".into());
    let (mut server, _sw, _c, _cw) = joined(policy, Some(claim("griefer")));
    let log = server.take_join_log();
    assert_eq!(log.len(), 1, "{log:?}");
    assert!(log[0].contains("refused"), "{}", log[0]);
    assert!(log[0].contains("griefer"), "naming who: {}", log[0]);
    assert!(log[0].contains("unverified"), "and how much to trust it: {}", log[0]);
    assert!(server.take_join_log().is_empty(), "drained, not repeated every tick");
}
