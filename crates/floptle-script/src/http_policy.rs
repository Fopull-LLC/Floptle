//! Where a script's `http.*` request may go.
//!
//! A game's Lua runs in three places that are not the developer's own machine:
//! a player's computer, a browser tab, and a dedicated server on a box the
//! developer does not own. On every one of them "any URL" means the machine's
//! own loopback services, its LAN, and — on a cloud box — the instance metadata
//! service that hands out the box's identity. None of that is a game's
//! business, so this module decides, **per resolved address**, whether a
//! request may connect.
//!
//! **The decision is made after DNS, not on the URL string.** A string check
//! is bypassed by a hostname that resolves to a private address and by a
//! redirect to one; a resolver that drops every refused address and fails the
//! request when none remain closes both, in one place, for every request the
//! agent makes. The names in [`refuse_host`] are refused up front as well, so
//! the error names the hostname the developer wrote rather than the address
//! it became.
//!
//! **One knob, set by the driver.** [`HttpPolicy::allow_local`] is `true` only
//! in the editor's Play — hitting `http://localhost:3000` while developing is
//! the ordinary case and must keep working — and `false` for an exported game,
//! a dedicated server and the browser. Link-local addresses (`169.254/16`,
//! `fe80::/10`) are refused **everywhere, the editor included**: there is no
//! development reason to reach a metadata service.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The one knob. Built by the driver, never by a script.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HttpPolicy {
    /// May a request reach loopback, private (`10/8`, `172.16/12`,
    /// `192.168/16`), carrier-grade NAT (`100.64/10`) and unique-local
    /// (`fc00::/7`) addresses? The editor's Play says yes; nothing else does.
    pub allow_local: bool,
}

impl Default for HttpPolicy {
    /// **Refusing.** The safe default is the one a driver that forgot to set
    /// the knob gets — an exported game that could reach a player's router
    /// because a constructor was never told otherwise is the bug this exists
    /// to prevent.
    fn default() -> Self {
        Self { allow_local: false }
    }
}

/// The reason an address is refused under a **refusing** policy, or `None` if
/// it is an ordinary public address.
///
/// IPv4-mapped IPv6 (`::ffff:a.b.c.d`) is unwrapped before classifying, or a
/// request to `::ffff:127.0.0.1` would be judged by the v6 rules — none of
/// which know what `127/8` is — and pass.
pub fn refuse(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => refuse_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => refuse_v4(v4),
            None => refuse_v6(v6),
        },
    }
}

/// [`refuse`] under a given policy. With `allow_local` the loopback, private,
/// CGNAT and unique-local classes pass; link-local, multicast, broadcast and
/// the unspecified address are refused whatever the policy says.
pub fn refuse_under(ip: IpAddr, policy: HttpPolicy) -> Option<&'static str> {
    let reason = refuse(ip)?;
    if policy.allow_local && is_local_class(reason) {
        return None;
    }
    Some(reason)
}

/// The classes `allow_local` opens. Kept as a match on the reason strings so
/// that a class added to [`refuse_v4`]/[`refuse_v6`] is refused everywhere
/// until somebody decides, here, that development needs it.
fn is_local_class(reason: &str) -> bool {
    matches!(reason, LOOPBACK | PRIVATE | CGNAT | UNIQUE_LOCAL)
}

const LOOPBACK: &str = "a loopback address";
const PRIVATE: &str = "a private-network address";
const CGNAT: &str = "a carrier-grade NAT address";
const UNIQUE_LOCAL: &str = "a unique-local address";
const LINK_LOCAL: &str = "a link-local address";
const MULTICAST: &str = "a multicast address";
const BROADCAST: &str = "the broadcast address";
const UNSPECIFIED: &str = "the unspecified address";

fn refuse_v4(ip: Ipv4Addr) -> Option<&'static str> {
    let [a, b, _, _] = ip.octets();
    if ip.is_unspecified() {
        Some(UNSPECIFIED)
    } else if ip.is_loopback() {
        Some(LOOPBACK)
    } else if ip.is_link_local() {
        Some(LINK_LOCAL)
    } else if ip.is_private() {
        Some(PRIVATE)
    } else if a == 100 && (64..128).contains(&b) {
        Some(CGNAT)
    } else if ip.is_multicast() {
        Some(MULTICAST)
    } else if ip.is_broadcast() {
        Some(BROADCAST)
    } else {
        None
    }
}

fn refuse_v6(ip: Ipv6Addr) -> Option<&'static str> {
    let seg = ip.segments()[0];
    if ip.is_unspecified() {
        Some(UNSPECIFIED)
    } else if ip.is_loopback() {
        Some(LOOPBACK)
    } else if seg & 0xffc0 == 0xfe80 {
        Some(LINK_LOCAL)
    } else if seg & 0xfe00 == 0xfc00 {
        Some(UNIQUE_LOCAL)
    } else if ip.is_multicast() {
        Some(MULTICAST)
    } else {
        None
    }
}

/// Refuse a hostname **by name**, before resolving it, so the message can say
/// what the developer wrote. `localhost` and `*.localhost` are loopback by
/// definition; `*.local` is mDNS (a printer, a NAS, the next desk over);
/// `*.internal` is the convention cloud providers use for their own services.
/// A literal IP address is classified directly.
pub fn refuse_host(host: &str, policy: HttpPolicy) -> Option<String> {
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return refuse_under(ip, policy).map(|why| format!("{host} is {why}"));
    }
    let lower = host.to_ascii_lowercase();
    let lower = lower.trim_end_matches('.');
    let local_name = lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal");
    if local_name && !policy.allow_local {
        return Some(format!("{host} names a local service"));
    }
    None
}

/// Why a whole request was refused, for the script's `res.error` and the
/// Console: the rule, in one sentence a developer can act on.
pub fn explain(why: &str, policy: HttpPolicy) -> String {
    if policy.allow_local {
        format!("http: refused — {why}. Link-local addresses are never reachable from a script.")
    } else {
        format!(
            "http: refused — {why}. A game reaches public addresses only; local and private \
             ones are allowed in the editor's Play and nowhere else."
        )
    }
}

/// The resolver a policy-bound agent uses: resolve, drop what the policy
/// refuses, fail if nothing is left. Every connection the agent makes —
/// including one a redirect would open — goes through here.
#[cfg(not(target_arch = "wasm32"))]
pub struct PolicyResolver(pub HttpPolicy);

#[cfg(not(target_arch = "wasm32"))]
impl ureq::Resolver for PolicyResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
        use std::net::ToSocketAddrs;
        let host = host_of(netloc);
        if let Some(why) = refuse_host(host, self.0) {
            return Err(refusal(&why, self.0));
        }
        keep_allowed(host, netloc.to_socket_addrs()?, self.0)
    }
}

/// `host:port` → `host`, where a v6 literal is `[..]:port`.
#[cfg(not(target_arch = "wasm32"))]
fn host_of(netloc: &str) -> &str {
    netloc.rsplit_once(':').map_or(netloc, |(h, _)| h)
}

#[cfg(not(target_arch = "wasm32"))]
fn refusal(why: &str, policy: HttpPolicy) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, explain(why, policy))
}

/// The half of the resolver that runs AFTER DNS: drop every address the policy
/// refuses, and fail — naming the first refused class — if none is left. Its
/// own function so a test can hand it a resolution without needing a name
/// that resolves that way on the test machine.
#[cfg(not(target_arch = "wasm32"))]
fn keep_allowed(
    host: &str,
    resolved: impl IntoIterator<Item = std::net::SocketAddr>,
    policy: HttpPolicy,
) -> std::io::Result<Vec<std::net::SocketAddr>> {
    let mut refused = None;
    let kept: Vec<_> = resolved
        .into_iter()
        .filter(|a| match refuse_under(a.ip(), policy) {
            Some(why) => {
                refused.get_or_insert_with(|| format!("{host} resolves to {why}"));
                false
            }
            None => true,
        })
        .collect();
    match (kept.is_empty(), refused) {
        (true, Some(why)) => Err(refusal(&why, policy)),
        _ => Ok(kept),
    }
}

/// An agent builder with the policy's resolver attached. Callers add their
/// own timeouts; a game's `http.*` also sets `redirects(0)`.
#[cfg(not(target_arch = "wasm32"))]
pub fn agent_builder(policy: HttpPolicy) -> ureq::AgentBuilder {
    ureq::AgentBuilder::new().resolver(PolicyResolver(policy))
}

/// Request headers a script may not set. ureq owns the framing ones and the
/// transport owns the connection; a `Host` override is how a request to an
/// allowed address is made to ask a virtual host for something else.
pub const REFUSED_HEADERS: &[&str] = &["host", "content-length", "transfer-encoding", "connection"];

/// The header a script tried to set that it may not, if any.
pub fn refused_header<'a>(headers: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    headers.into_iter().find(|h| REFUSED_HEADERS.contains(&h.to_ascii_lowercase().as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    const REFUSING: HttpPolicy = HttpPolicy { allow_local: false };
    const EDITOR: HttpPolicy = HttpPolicy { allow_local: true };

    /// Every class the policy names, and the mapped-v6 spellings of the v4 ones —
    /// the spelling that bypasses a check written only for v4.
    #[test]
    fn every_reserved_range_is_refused_and_public_addresses_are_not() {
        let refused = [
            ("127.0.0.1", LOOPBACK),
            ("127.255.255.254", LOOPBACK),
            ("::1", LOOPBACK),
            ("::ffff:127.0.0.1", LOOPBACK),
            ("10.0.0.1", PRIVATE),
            ("172.16.0.1", PRIVATE),
            ("172.31.255.255", PRIVATE),
            ("192.168.1.1", PRIVATE),
            ("::ffff:192.168.1.1", PRIVATE),
            ("169.254.169.254", LINK_LOCAL),
            ("::ffff:169.254.169.254", LINK_LOCAL),
            ("fe80::1", LINK_LOCAL),
            ("febf::1", LINK_LOCAL),
            ("fc00::1", UNIQUE_LOCAL),
            ("fd12:3456::1", UNIQUE_LOCAL),
            ("100.64.0.1", CGNAT),
            ("100.127.255.255", CGNAT),
            ("0.0.0.0", UNSPECIFIED),
            ("::", UNSPECIFIED),
            ("224.0.0.1", MULTICAST),
            ("ff02::1", MULTICAST),
            ("255.255.255.255", BROADCAST),
        ];
        for (a, why) in refused {
            assert_eq!(refuse(ip(a)), Some(why), "{a}");
        }
        for a in ["8.8.8.8", "172.32.0.1", "100.128.0.1", "2606:4700::1111", "::ffff:8.8.8.8"] {
            assert_eq!(refuse(ip(a)), None, "{a}");
        }
    }

    /// The editor opens the local classes and nothing else.
    #[test]
    fn allow_local_opens_loopback_and_private_but_never_link_local() {
        for a in ["127.0.0.1", "::1", "10.1.2.3", "192.168.0.10", "100.64.1.1", "fd00::1"] {
            assert_eq!(refuse_under(ip(a), EDITOR), None, "{a}");
            assert!(refuse_under(ip(a), REFUSING).is_some(), "{a}");
        }
        for a in ["169.254.169.254", "fe80::1", "::ffff:169.254.169.254", "0.0.0.0", "224.0.0.1"] {
            assert!(refuse_under(ip(a), EDITOR).is_some(), "{a}");
        }
    }

    #[test]
    fn local_names_are_refused_before_resolving_and_say_which_name() {
        for h in ["localhost", "LOCALHOST", "api.localhost", "nas.local", "metadata.internal", "localhost."] {
            let why = refuse_host(h, REFUSING).expect(h);
            assert!(why.contains(h), "{why}");
            assert_eq!(refuse_host(h, EDITOR), None, "{h} is fine in the editor");
        }
        assert_eq!(refuse_host("fopull.com", REFUSING), None);
        assert_eq!(refuse_host("example.local.example.com", REFUSING), None);
        // A literal is classified as an address, in either spelling.
        assert!(refuse_host("169.254.169.254", EDITOR).unwrap().contains(LINK_LOCAL));
        assert!(refuse_host("[::ffff:127.0.0.1]", REFUSING).unwrap().contains(LOOPBACK));
    }

    #[test]
    fn the_default_policy_refuses() {
        assert_eq!(HttpPolicy::default(), REFUSING);
    }

    #[test]
    fn framing_and_connection_headers_are_refused_case_insensitively() {
        assert_eq!(refused_header(["Accept", "HOST"]), Some("HOST"));
        assert_eq!(refused_header(["Content-Type", "content-length"]), Some("content-length"));
        assert_eq!(refused_header(["Authorization", "X-Api-Key"]), None);
    }

    /// The resolver is what makes the policy hold for a NAME: what DNS hands
    /// back is filtered, and a name whose every address is refused fails with
    /// the class named. A name with one public address among private ones
    /// keeps only the public one.
    #[test]
    fn what_dns_returns_is_filtered_and_a_name_with_nothing_left_is_refused() {
        let sa = |s: &str| -> std::net::SocketAddr { s.parse().unwrap() };
        let metadata = [sa("[::ffff:169.254.169.254]:80"), sa("169.254.169.254:80")];
        let e = keep_allowed("metadata.evil.example", metadata, EDITOR).unwrap_err();
        assert!(e.to_string().contains("metadata.evil.example resolves to"), "{e}");
        assert!(e.to_string().contains(LINK_LOCAL), "{e}");
        let nas = [sa("192.168.1.20:80")];
        assert!(keep_allowed("nas.example", nas, REFUSING).is_err());
        assert_eq!(keep_allowed("nas.example", nas, EDITOR).unwrap().len(), 1);
        let mixed = [sa("10.0.0.5:443"), sa("93.184.216.34:443")];
        let kept = keep_allowed("mixed.example", mixed, REFUSING).unwrap();
        assert_eq!(kept, vec![sa("93.184.216.34:443")]);
    }

    /// Literals and local names never reach DNS at all.
    #[test]
    fn the_resolver_refuses_literals_and_local_names_before_dns() {
        use ureq::Resolver as _;
        let r = PolicyResolver(REFUSING);
        let e = r.resolve("127.0.0.1:80").unwrap_err();
        assert!(e.to_string().contains(LOOPBACK), "{e}");
        let e = r.resolve("[::ffff:169.254.169.254]:80").unwrap_err();
        assert!(e.to_string().contains(LINK_LOCAL), "{e}");
        let e = r.resolve("nas.local:80").unwrap_err();
        assert!(e.to_string().contains("nas.local"), "{e}");
        assert_eq!(PolicyResolver(EDITOR).resolve("127.0.0.1:80").unwrap().len(), 1);
        // Still refused in the editor: link-local has no development use.
        assert!(PolicyResolver(EDITOR).resolve("169.254.169.254:80").is_err());
        assert_eq!(host_of("[::1]:80"), "[::1]");
        assert_eq!(host_of("fopull.com:443"), "fopull.com");
    }
}
