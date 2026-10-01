//! Join tokens: how a server checks who a player is.
//!
//! A signed-in player asks fopull.com for a token naming the one server they
//! are joining, and hands that token over in the join handshake. It proves
//! who they are to that server, for five minutes, and is good for nothing
//! anywhere else. The account's own access token never leaves the player's
//! machine, because a server holding it could spend the player's Fobucks.
//!
//! The server checks the signature against fopull.com's published keys, which
//! it fetches once and keeps. There is no call to fopull.com per join, so a
//! burst of joins cannot be turned against the provider, and a server on a
//! slow link does not fail every join because fopull.com is slow.
//!
//! The rules are `contracts/identity-auth.md` §8, and each one has a test
//! below: the token's type, its issuer, the server it names, a lifetime of at
//! most ten minutes, two minutes of clock allowance either way, and no second
//! use of one token. A token that fails any of them leaves the player exactly
//! where they would be without one, an unverified claim, and says why.

#[cfg(not(target_arch = "wasm32"))]
pub use check::{JoinTokenCheck, Verified};

/// The `typ` a join token carries. An access token verifies against the same
/// keys, so this is the only thing that stops one being accepted as the other.
pub const JOIN_TOKEN_TYP: &str = "join+jwt";

/// The longest lifetime (`exp - iat`) a server will honour, in seconds. The
/// provider mints five minutes; this bound is for a provider bug that mints
/// longer, which a server trusting `exp` alone would honour.
pub const MAX_LIFETIME_S: u64 = 600;

/// Clock allowance on `iat` and `exp`, in seconds. Servers are rented boxes
/// with whatever time sync they came with.
pub const CLOCK_SKEW_S: u64 = 120;

/// The default port a `relay://` target names when it names none.
const RELAY_DEFAULT_PORT: u16 = 7788;

/// A join target in the one spelling both sides compare: scheme and host
/// lower-case, the lobby code upper-case, no trailing slash, and the relay's
/// default port written out. `None` for anything that is not a target a token
/// can name (`local://`, `wss://`, or something malformed).
///
/// The player and the server spell the same server differently all the time
/// (`Relay.Example.com/abcde` and `relay.example.com:7788/ABCDE`). Without one
/// spelling, a valid token would be refused for its capitalisation.
pub fn normalise_audience(target: &str) -> Option<String> {
    let (scheme, rest) = target.trim().split_once("://")?;
    let rest = rest.trim_end_matches('/');
    match scheme.to_ascii_lowercase().as_str() {
        "cloud" => {
            let code = lobby_code(rest)?;
            Some(format!("cloud://{code}"))
        }
        "relay" => {
            let (addr, code) = rest.rsplit_once('/')?;
            let code = lobby_code(code)?;
            let (host, port) = match split_port(addr) {
                Some((h, p)) => (h, p),
                None => (addr, RELAY_DEFAULT_PORT),
            };
            Some(format!("relay://{}:{port}/{code}", host_part(host)?))
        }
        "quic" => {
            let (host, port) = split_port(rest)?;
            Some(format!("quic://{}:{port}", host_part(host)?))
        }
        _ => None,
    }
}

fn lobby_code(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())).then(|| s.to_ascii_uppercase())
}

fn host_part(h: &str) -> Option<String> {
    (!h.is_empty() && !h.contains('/') && !h.contains('@')).then(|| h.to_ascii_lowercase())
}

/// `host:port` → both, when the part after the last colon is a port. An IPv6
/// literal is bracketed (`[::1]:7788`), so its own colons never reach here as
/// the last one.
fn split_port(s: &str) -> Option<(&str, u16)> {
    let (h, p) = s.rsplit_once(':')?;
    if h.contains(':') && !h.starts_with('[') {
        return None; // a bare IPv6 address: there is no telling which colon is the port
    }
    Some((h, p.parse().ok()?))
}

#[cfg(not(target_arch = "wasm32"))]
mod check {
    use super::*;
    use crate::identity::{AssertedOnly, Identity, Verifier};
    use crate::wire::IdentityClaim;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Who a token says the player is, once it has passed every rule.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Verified {
        pub sub: String,
        pub name: String,
        pub tier: String,
    }

    /// The server's half: the provider's keys, the targets this server
    /// answers to, and the tokens it has already accepted.
    ///
    /// Shared (`Arc`) because two owners need it. The session holds it as its
    /// [`Verifier`]. The engine holds it to hand in the keys when the fetch
    /// lands and the lobby code when the relay assigns one.
    pub struct JoinTokenCheck {
        issuer: String,
        state: Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        /// kid → RSA modulus and exponent, big-endian.
        keys: HashMap<String, (Vec<u8>, Vec<u8>)>,
        /// Normalised join targets this server answers to.
        audiences: Vec<String>,
        /// jti → exp, for every token accepted and not yet expired.
        seen: HashMap<String, u64>,
        /// Set when there are no keys, or a token named a key not among them.
        wants_keys: bool,
    }

    impl JoinTokenCheck {
        /// `issuer` is the provider's base URL, `https://fopull.com` in
        /// production, compared exactly against each token's `iss`.
        pub fn new(issuer: &str) -> Arc<Self> {
            Arc::new(Self {
                issuer: issuer.trim_end_matches('/').to_string(),
                state: Mutex::new(State { wants_keys: true, ..Default::default() }),
            })
        }

        /// Load the provider's key set (`/.well-known/jwks.json`). Returns how
        /// many RS256 signing keys it held. Replaces what was there, so a key
        /// the provider retired stops being honoured.
        pub fn set_keys(&self, jwks_json: &str) -> Result<usize, String> {
            let doc: serde_json::Value =
                serde_json::from_str(jwks_json).map_err(|e| format!("the key set is not JSON: {e}"))?;
            let mut keys = HashMap::new();
            for k in doc.get("keys").and_then(|v| v.as_array()).ok_or("the key set has no \"keys\"")? {
                let field = |name: &str| k.get(name).and_then(|v| v.as_str());
                if field("kty") != Some("RSA")
                    || field("alg").is_some_and(|a| a != "RS256")
                    || field("use").is_some_and(|u| u != "sig")
                {
                    continue;
                }
                let (Some(kid), Some(n), Some(e)) = (field("kid"), field("n"), field("e")) else {
                    continue;
                };
                let (Ok(n), Ok(e)) = (URL_SAFE_NO_PAD.decode(n), URL_SAFE_NO_PAD.decode(e)) else {
                    continue;
                };
                keys.insert(kid.to_string(), (n, e));
            }
            let count = keys.len();
            let mut st = self.lock();
            st.keys = keys;
            st.wants_keys = count == 0;
            Ok(count)
        }

        /// The join targets this server answers to. Each is normalised, and
        /// one that cannot be is dropped. Replaces the previous list, because
        /// a server that re-hosts under a new code stops being the old one.
        pub fn set_audiences<S: AsRef<str>>(&self, targets: &[S]) {
            self.lock().audiences =
                targets.iter().filter_map(|t| normalise_audience(t.as_ref())).collect();
        }

        pub fn audiences(&self) -> Vec<String> {
            self.lock().audiences.clone()
        }

        /// Does the engine need to fetch the key set? True before the first
        /// fetch, and again after a token names a key this server does not
        /// have, which is what a key rotation looks like from here.
        pub fn wants_keys(&self) -> bool {
            self.lock().wants_keys
        }

        /// Check a token at `now` (unix seconds). `Err` says which rule it broke,
        /// in a sentence for the server's log.
        pub fn check_at(&self, token: &str, now: u64) -> Result<Verified, String> {
            let mut parts = token.split('.');
            let (Some(h), Some(p), Some(sig), None) = (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err("the join token is not a JWT".into());
            };
            let header = decode_json(h).ok_or("the join token's header is unreadable")?;
            let str_of = |v: &serde_json::Value, k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
            if str_of(&header, "typ").as_deref() != Some(JOIN_TOKEN_TYP) {
                return Err(format!(
                    "the token is not a join token (typ {:?}); an access token is never accepted here",
                    str_of(&header, "typ").unwrap_or_default()
                ));
            }
            if str_of(&header, "alg").as_deref() != Some("RS256") {
                return Err("the join token is not signed RS256".into());
            }
            let kid = str_of(&header, "kid").unwrap_or_default();
            let mut st = self.lock();
            let Some((n, e)) = st.keys.get(&kid).cloned() else {
                st.wants_keys = true;
                return Err(if st.keys.is_empty() {
                    "this server has not fetched fopull.com's signing keys yet".into()
                } else {
                    format!("signed with a key this server does not have ({kid}); fetching the key set again")
                });
            };
            let sig = URL_SAFE_NO_PAD.decode(sig).map_err(|_| "the join token's signature is unreadable")?;
            let signed = &token[..h.len() + 1 + p.len()];
            ring::signature::RsaPublicKeyComponents { n: &n, e: &e }
                .verify(&ring::signature::RSA_PKCS1_2048_8192_SHA256, signed.as_bytes(), &sig)
                .map_err(|_| "the join token's signature does not verify")?;

            // Signed by the provider. Now whether it is for this server, now.
            let claims = decode_json(p).ok_or("the join token's claims are unreadable")?;
            if str_of(&claims, "iss").as_deref() != Some(self.issuer.as_str()) {
                return Err(format!(
                    "the join token was issued by {:?}, not {}",
                    str_of(&claims, "iss").unwrap_or_default(),
                    self.issuer
                ));
            }
            let aud: Vec<String> = match claims.get("aud") {
                Some(serde_json::Value::String(s)) => vec![s.clone()],
                Some(serde_json::Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
                _ => Vec::new(),
            };
            if st.audiences.is_empty() {
                return Err("this server does not know its own join address yet, so it cannot tell \
                            whether the token names it"
                    .into());
            }
            let names_us = aud
                .iter()
                .filter_map(|a| normalise_audience(a))
                .any(|a| st.audiences.contains(&a));
            if !names_us {
                return Err(format!(
                    "the join token is for {}, and this server is {}",
                    aud.join(", "),
                    st.audiences.join(" / ")
                ));
            }
            let num = |k: &str| claims.get(k).and_then(|v| v.as_u64());
            let (Some(iat), Some(exp)) = (num("iat"), num("exp")) else {
                return Err("the join token has no iat/exp".into());
            };
            if exp <= iat || exp - iat > MAX_LIFETIME_S {
                return Err(format!(
                    "the join token claims a lifetime of {} s; the most a server honours is {MAX_LIFETIME_S} s",
                    exp.saturating_sub(iat)
                ));
            }
            if now > exp + CLOCK_SKEW_S {
                return Err(format!("the join token expired {} s ago", now - exp));
            }
            if iat > now + CLOCK_SKEW_S {
                return Err(format!(
                    "the join token was issued {} s in this server's future; check this machine's clock",
                    iat - now
                ));
            }
            let sub = str_of(&claims, "sub").filter(|s| !s.is_empty()).ok_or("the join token names no account")?;
            st.seen.retain(|_, e| *e + CLOCK_SKEW_S >= now);
            if let Some(jti) = str_of(&claims, "jti") {
                if st.seen.contains_key(&jti) {
                    return Err("this join token was already used to join".into());
                }
                st.seen.insert(jti, exp);
            }
            Ok(Verified {
                sub,
                name: str_of(&claims, "name").unwrap_or_default(),
                tier: str_of(&claims, "tier").unwrap_or_default(),
            })
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(|p| p.into_inner())
        }
    }

    fn decode_json(part: &str) -> Option<serde_json::Value> {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).ok()?).ok()
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    impl Verifier for Arc<JoinTokenCheck> {
        fn verify(&self, claim: Option<&IdentityClaim>) -> Identity {
            self.verify_noting(claim).0
        }

        /// A token that passes is the identity, whatever the claim beside it
        /// says: the name and tier are the provider's, not the client's. A
        /// token that fails leaves the claim exactly as unverified as no token.
        fn verify_noting(&self, claim: Option<&IdentityClaim>) -> (Identity, Option<String>) {
            let asserted = AssertedOnly.verify(claim);
            let Some(token) = claim.and_then(|c| c.proof.as_deref()) else {
                return (asserted, None);
            };
            match self.check_at(token, unix_now()) {
                Ok(v) => {
                    let note = (asserted.id.as_deref() != Some(v.sub.as_str())).then(|| {
                        format!(
                            "the client claimed account {:?}; its join token is for {}, which is what counts",
                            asserted.id.unwrap_or_default(),
                            v.sub
                        )
                    });
                    (Identity { id: Some(v.sub), name: v.name, tier: v.tier, verified: true }, note)
                }
                Err(why) => (asserted, Some(format!("join token refused: {why}"))),
            }
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use crate::identity::Verifier;
    use crate::wire::IdentityClaim;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use std::sync::Arc;

    /// A private key generated for these tests and nothing else. Its public
    /// half is what the "provider" publishes below.
    const KEY: &[u8] = include_bytes!("../tests/fixtures/join_token_TEST_ONLY_key.der");
    const ISS: &str = "https://fopull.com";
    const NOW: u64 = 1_800_000_000;

    fn keypair() -> ring::signature::RsaKeyPair {
        ring::signature::RsaKeyPair::from_der(KEY).expect("test key")
    }

    fn jwks(kid: &str) -> String {
        let kp = keypair();
        let ring::rsa::PublicKeyComponents { n, e } =
            ring::rsa::PublicKeyComponents::<Vec<u8>>::from(kp.public());
        serde_json::json!({"keys": [{
            "kty": "RSA", "alg": "RS256", "use": "sig", "kid": kid,
            "n": URL_SAFE_NO_PAD.encode(n), "e": URL_SAFE_NO_PAD.encode(e),
        }]})
        .to_string()
    }

    fn sign(header: serde_json::Value, claims: serde_json::Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(header.to_string());
        let c = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{h}.{c}");
        let kp = keypair();
        let mut sig = vec![0; kp.public().modulus_len()];
        kp.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut sig)
            .expect("sign");
        format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig))
    }

    fn header() -> serde_json::Value {
        serde_json::json!({"alg": "RS256", "typ": "join+jwt", "kid": "k1"})
    }

    fn claims(aud: &str, jti: &str) -> serde_json::Value {
        serde_json::json!({
            "iss": ISS, "sub": "user_42", "aud": aud, "name": "Ty", "tier": "studio",
            "iat": NOW - 10, "exp": NOW + 290, "jti": jti,
        })
    }

    fn server(audiences: &[&str]) -> Arc<JoinTokenCheck> {
        let c = JoinTokenCheck::new(ISS);
        assert_eq!(c.set_keys(&jwks("k1")).unwrap(), 1);
        c.set_audiences(audiences);
        c
    }

    #[test]
    fn both_sides_spell_a_target_the_same_way() {
        let cases = [
            ("cloud://ubdqby", "cloud://UBDQBY"),
            ("CLOUD://UBDQBY/", "cloud://UBDQBY"),
            ("relay://Relay.Example.com/abcde", "relay://relay.example.com:7788/ABCDE"),
            ("relay://relay.example.com:7788/ABCDE", "relay://relay.example.com:7788/ABCDE"),
            ("relay://10.0.0.5:9000/abcde", "relay://10.0.0.5:9000/ABCDE"),
            ("relay://[::1]:7788/abcde", "relay://[::1]:7788/ABCDE"),
            ("quic://Host.Example.com:30000", "quic://host.example.com:30000"),
        ];
        for (raw, want) in cases {
            assert_eq!(normalise_audience(raw).as_deref(), Some(want), "{raw}");
        }
        // A token cannot name these, so a join to one carries no token.
        for raw in ["local://", "wss://relay.example.com:7789/ABCDE", "quic://host", "cloud://", "relay://host"] {
            assert_eq!(normalise_audience(raw), None, "{raw}");
        }
    }

    #[test]
    fn a_token_for_this_server_names_the_player() {
        let s = server(&["cloud://UABCDE"]);
        let v = s.check_at(&sign(header(), claims("cloud://UABCDE", "j1")), NOW).expect("valid");
        assert_eq!(v, Verified { sub: "user_42".into(), name: "Ty".into(), tier: "studio".into() });
    }

    /// The point of naming a server in the token: one minted for lobby A is
    /// worth nothing at lobby B.
    #[test]
    fn a_token_for_another_server_is_refused() {
        let s = server(&["cloud://UABCDE"]);
        let err = s.check_at(&sign(header(), claims("cloud://UZZZZZ", "j1")), NOW).unwrap_err();
        assert!(err.contains("UZZZZZ"), "{err}");
        // Same code, different relay: still a different server.
        let s = server(&["relay://a.example.com:7788/ABCDE"]);
        assert!(s.check_at(&sign(header(), claims("relay://b.example.com:7788/ABCDE", "j2")), NOW).is_err());
        // And the same server spelled differently is the same server.
        assert!(s.check_at(&sign(header(), claims("relay://A.example.com/abcde", "j3")), NOW).is_ok());
    }

    /// An access token verifies against the same keys. Without the `typ`
    /// check, a server handed one would accept it as an identity.
    #[test]
    fn an_access_token_is_not_a_join_token() {
        let s = server(&["cloud://UABCDE"]);
        for typ in [serde_json::json!("JWT"), serde_json::json!("at+jwt"), serde_json::Value::Null] {
            let h = serde_json::json!({"alg": "RS256", "typ": typ, "kid": "k1"});
            let err = s.check_at(&sign(h, claims("cloud://UABCDE", "j1")), NOW).unwrap_err();
            assert!(err.contains("not a join token"), "{err}");
        }
    }

    #[test]
    fn a_tampered_token_does_not_verify() {
        let s = server(&["cloud://UABCDE"]);
        let good = sign(header(), claims("cloud://UABCDE", "j1"));
        let mut parts: Vec<String> = good.split('.').map(str::to_string).collect();
        let mut c = claims("cloud://UABCDE", "j1");
        c["sub"] = "somebody_else".into();
        parts[1] = URL_SAFE_NO_PAD.encode(c.to_string());
        let err = s.check_at(&parts.join("."), NOW).unwrap_err();
        assert!(err.contains("signature"), "{err}");
    }

    #[test]
    fn a_token_from_another_issuer_is_refused() {
        let s = server(&["cloud://UABCDE"]);
        let mut c = claims("cloud://UABCDE", "j1");
        c["iss"] = "https://evil.example.com".into();
        assert!(s.check_at(&sign(header(), c), NOW).unwrap_err().contains("issued by"));
    }

    /// Both edges of the two-minute allowance, from both sides, so a sign
    /// error in either comparison turns one of these red.
    #[test]
    fn expiry_allows_two_minutes_of_clock_drift_and_no_more() {
        let s = server(&["cloud://UABCDE"]);
        let mut n = 0;
        let mut at = |iat: u64, exp: u64, now: u64| {
            n += 1;
            let mut c = claims("cloud://UABCDE", &format!("j{n}"));
            c["iat"] = iat.into();
            c["exp"] = exp.into();
            s.check_at(&sign(header(), c), now)
        };
        assert!(at(NOW - 300, NOW, NOW + CLOCK_SKEW_S).is_ok(), "expired, but inside the allowance");
        assert!(at(NOW - 300, NOW, NOW + CLOCK_SKEW_S + 1).unwrap_err().contains("expired"));
        assert!(at(NOW + CLOCK_SKEW_S, NOW + CLOCK_SKEW_S + 300, NOW).is_ok(), "a fast provider clock");
        assert!(at(NOW + CLOCK_SKEW_S + 1, NOW + CLOCK_SKEW_S + 301, NOW).unwrap_err().contains("future"));
    }

    #[test]
    fn a_token_that_lives_too_long_is_refused_even_unexpired() {
        let s = server(&["cloud://UABCDE"]);
        let mut c = claims("cloud://UABCDE", "j1");
        c["iat"] = NOW.into();
        c["exp"] = (NOW + MAX_LIFETIME_S + 1).into();
        assert!(s.check_at(&sign(header(), c.clone()), NOW).unwrap_err().contains("lifetime"));
        c["exp"] = (NOW + MAX_LIFETIME_S).into();
        assert!(s.check_at(&sign(header(), c), NOW).is_ok(), "exactly the limit is honoured");
    }

    #[test]
    fn a_token_joins_once() {
        let s = server(&["cloud://UABCDE"]);
        let t = sign(header(), claims("cloud://UABCDE", "j1"));
        assert!(s.check_at(&t, NOW).is_ok());
        assert!(s.check_at(&t, NOW).unwrap_err().contains("already used"));
    }

    /// A key rotation looks, from the server, like a token signed by a key it
    /// has never seen. That must ask for the key set again, not fail forever.
    #[test]
    fn an_unknown_key_asks_for_the_key_set_again() {
        let s = server(&["cloud://UABCDE"]);
        assert!(!s.wants_keys());
        let h = serde_json::json!({"alg": "RS256", "typ": "join+jwt", "kid": "rotated"});
        assert!(s.check_at(&sign(h.clone(), claims("cloud://UABCDE", "j1")), NOW).is_err());
        assert!(s.wants_keys());
        s.set_keys(&jwks("rotated")).unwrap();
        assert!(!s.wants_keys());
        assert!(s.check_at(&sign(h, claims("cloud://UABCDE", "j1")), NOW).is_ok());
    }

    #[test]
    fn a_server_that_does_not_know_its_address_verifies_nobody() {
        let s = server(&[]);
        let err = s.check_at(&sign(header(), claims("cloud://UABCDE", "j1")), NOW).unwrap_err();
        assert!(err.contains("own join address"), "{err}");
    }

    /// Through the session's seam: a verified token's account replaces the
    /// claim's, and a failed one leaves the claim unverified with the reason.
    #[test]
    fn the_token_not_the_claim_is_who_the_player_is() {
        let s = server(&["cloud://UABCDE"]);
        // `verify_noting` reads the real clock, so the token is minted for now.
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut c = claims("cloud://UABCDE", "j1");
        c["iat"] = (now - 1).into();
        c["exp"] = (now + 299).into();
        let claim = IdentityClaim {
            id: "impostor".into(),
            name: "Not Ty".into(),
            tier: "free".into(),
            proof: Some(sign(header(), c)),
        };
        let (who, note) = s.verify_noting(Some(&claim));
        assert!(who.verified);
        assert_eq!((who.id.as_deref(), who.name.as_str(), who.tier.as_str()), (Some("user_42"), "Ty", "studio"));
        assert!(note.is_some_and(|n| n.contains("impostor")));

        let forged = IdentityClaim { proof: Some("a.b.c".into()), ..claim };
        let (who, note) = s.verify_noting(Some(&forged));
        assert!(!who.verified);
        assert_eq!(who.id.as_deref(), Some("impostor"), "still carried, as an assertion");
        assert!(note.is_some_and(|n| n.starts_with("join token refused")));
    }
}
