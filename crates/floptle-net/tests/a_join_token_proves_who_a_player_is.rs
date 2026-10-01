//! A join token proves who a player is, through a real session handshake.
//!
//! The unit tests in `src/join_token.rs` check each rule of the token. These
//! check that the session uses the result: the identity a game reads is the
//! token's, a token for another lobby admits nobody under `requireVerified`,
//! and a held Hello joins nobody until it is sent.
#![cfg(not(target_arch = "wasm32"))]

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use floptle_core::World;
use floptle_net::{IdentityClaim, JoinPolicy, JoinState, JoinTokenCheck, MemoryHub, NetSession};

const KEY: &[u8] = include_bytes!("fixtures/join_token_TEST_ONLY_key.der");
const ISS: &str = "https://fopull.com";

fn keypair() -> ring::signature::RsaKeyPair {
    ring::signature::RsaKeyPair::from_der(KEY).expect("test key")
}

fn jwks() -> String {
    let ring::rsa::PublicKeyComponents { n, e } =
        ring::rsa::PublicKeyComponents::<Vec<u8>>::from(keypair().public());
    serde_json::json!({"keys": [{"kty": "RSA", "alg": "RS256", "use": "sig", "kid": "k1",
        "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e)}]})
    .to_string()
}

/// A token the provider would mint for `aud`, now.
fn token(aud: &str, jti: &str) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let h = URL_SAFE_NO_PAD.encode(serde_json::json!({"alg": "RS256", "typ": "join+jwt", "kid": "k1"}).to_string());
    let c = URL_SAFE_NO_PAD.encode(
        serde_json::json!({"iss": ISS, "sub": "user_42", "aud": aud, "name": "Ty", "tier": "studio",
            "iat": now - 1, "exp": now + 299, "jti": jti})
        .to_string(),
    );
    let input = format!("{h}.{c}");
    let kp = keypair();
    let mut sig = vec![0; kp.public().modulus_len()];
    kp.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig)
        .unwrap();
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
}

fn claim(proof: Option<String>) -> IdentityClaim {
    IdentityClaim { id: "user_42".into(), name: "typed by the client".into(), tier: "free".into(), proof }
}

/// A server answering to `cloud://UABCDE`, with the keys loaded.
fn server(hub: &MemoryHub, policy: JoinPolicy) -> NetSession {
    let check = JoinTokenCheck::new(ISS);
    check.set_keys(&jwks()).unwrap();
    check.set_audiences(&["cloud://UABCDE"]);
    let mut s = NetSession::server(Box::new(hub.server_endpoint()), 0);
    s.set_verifier(Box::new(check));
    s.set_join_policy(policy);
    s
}

fn run(server: &mut NetSession, clients: &mut [&mut NetSession]) {
    let (sw, mut cw) = (World::default(), World::default());
    for t in 1..4 {
        server.tick_server(&sw, t);
        for c in clients.iter_mut() {
            c.tick_client(&mut cw);
        }
    }
}

#[test]
fn a_token_for_this_lobby_arrives_verified_with_the_providers_name() {
    let hub = MemoryHub::new();
    let mut s = server(&hub, JoinPolicy::default());
    let mut c = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim(Some(token("cloud://uabcde", "a")))));
    run(&mut s, &mut [&mut c]);
    let who = s.identity(s.peers()[0]).expect("joined");
    assert!(who.verified);
    assert_eq!((who.id.as_deref(), who.name.as_str(), who.tier.as_str()), (Some("user_42"), "Ty", "studio"));
}

#[test]
fn require_verified_turns_away_a_token_for_another_lobby_and_says_why() {
    let hub = MemoryHub::new();
    let mut s = server(&hub, JoinPolicy { require_verified: true, ..Default::default() });
    let mut wrong = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim(Some(token("cloud://UZZZZZ", "b")))));
    let mut none = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim(None)));
    let mut right = NetSession::client_as(Box::new(hub.connect()), 0, Some(claim(Some(token("cloud://UABCDE", "c")))));
    run(&mut s, &mut [&mut wrong, &mut none, &mut right]);
    assert_eq!(s.peers().len(), 1, "only the proven player is on the roster");
    assert!(matches!(wrong.join_state(), JoinState::Refused(r) if r.contains("confirm")), "{:?}", wrong.join_state());
    assert!(matches!(none.join_state(), JoinState::Refused(_)));
    assert!(matches!(right.join_state(), JoinState::Joined));
    let log = s.take_join_log().join("\n");
    assert!(log.contains("UZZZZZ"), "the operator's log names the mismatch: {log}");
}

/// The Hello is held while a token is fetched. Nothing joins until it goes,
/// and when it goes it carries the token.
#[test]
fn a_held_hello_joins_only_when_presented() {
    let hub = MemoryHub::new();
    let mut s = server(&hub, JoinPolicy::default());
    let mut c = NetSession::client_awaiting_identity(Box::new(hub.connect()), 0);
    run(&mut s, &mut [&mut c]);
    assert!(s.peers().is_empty(), "nobody joined on a Hello that was never sent");
    assert!(c.awaiting_identity());
    assert!(matches!(c.join_state(), JoinState::Connecting));

    c.present_identity(Some(claim(Some(token("cloud://UABCDE", "d")))));
    c.present_identity(Some(claim(None))); // a second call is ignored
    run(&mut s, &mut [&mut c]);
    assert_eq!(s.peers().len(), 1);
    assert!(s.identity(s.peers()[0]).unwrap().verified);
}
