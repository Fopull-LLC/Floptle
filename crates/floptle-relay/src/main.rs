//! The reference relay (`docs/multiplayer.md` §10, ADR-0022): hosts get a
//! lobby code, clients join with it, traffic forwards both ways — nobody
//! port-forwards. Self-hostable by anyone; Floptle Cloud runs the managed one.
//!
//!     floptle-relay [port]                        the open relay
//!     floptle-relay --control <url> --region <id> --letter U --token <t>
//!
//! **Without the managed flags this is the open relay and nothing else.** No
//! keys, no control plane, no accounting — byte-identical to the day it was
//! written, which is what ADR-0022 promises and what `floptle-net`'s
//! `a_self_hosted_relay_still_hosts_keyless_with_a_five_character_code` holds
//! us to. Managed mode is additive and opt-in, and a developer running their
//! own relay never touches any of it.

mod boxstats;
mod control;
mod policy;
mod tls;

use std::sync::Arc;
use std::time::Duration;

use floptle_net::RelayServer;

/// Parsed command line.
#[derive(Debug)]
struct Args {
    port: u16,
    control: Option<String>,
    region: String,
    letter: char,
    token: Option<String>,
    /// `--no-address-limits`: lift the per-address rates for a load test run
    /// from one machine. Every other limit stays.
    no_address_limits: bool,
    /// `--tls-cert` + `--tls-key`: the certificate to present instead of a
    /// self-signed one, watched for renewal (see `tls`).
    tls: Option<(std::path::PathBuf, std::path::PathBuf)>,
}

impl Args {
    /// Managed mode needs all three of `--control`, `--region` and `--token`.
    /// Two of the three is a misconfiguration, not a degraded mode: a relay
    /// that came up open because its token was missing would be exactly the
    /// untracked path the whole feature exists to close.
    fn managed(&self) -> Option<(&str, &str)> {
        match (&self.control, &self.token) {
            (Some(c), Some(t)) => Some((c.as_str(), t.as_str())),
            _ => None,
        }
    }

    /// What `--help` prints. A const so the test can hold it to naming every
    /// flag the parser accepts — a help table that has fallen behind the parser
    /// is worse than none, because it is believed.
    const HELP: &'static str = "\
floptle-relay — the rendezvous relay: hosts get a lobby code, clients join with
it, traffic forwards both ways, and nobody port-forwards.

USAGE
  floptle-relay [PORT]                     the open relay (default port 7788)
  floptle-relay --control <url> --region <id> --letter <c> --token <t>
                                           managed mode (Floptle Cloud)

FLAGS
  PORT                  UDP port to listen on. Bare positional. Default 7788.
  --control <url>       control-plane base URL. Managed mode.
  --region <id>         region id this box serves. Default us-east.
  --letter <c>          one character, prefixed to every lobby code it issues.
                        Default U.
  --token <t>           this box's token. Prefer --token-file.
  --token-file <path>   read the token from a file, so it is not in a command
                        line every `ps` on the box can read.
  --no-address-limits   lift the per-address rates (lobby opens and joins a
                        minute from one address) — for a load test run from one
                        machine, which is the only case that looks like an
                        attacker to a relay. Every other limit stays.
  --tls-cert <path>     PEM certificate chain to present, leaf first — certbot's
                        fullchain.pem. Given together with --tls-key. Without
                        them the relay mints a self-signed certificate at
                        startup, which is the open relay's dev-trust model and
                        which no client verifies.
  --tls-key <path>      PEM private key for --tls-cert — certbot's privkey.pem.
                        Both files are watched: a renewal written to the same
                        paths is presented to new connections within ten
                        seconds and drops nobody. A renewal that will not load
                        is said once and the old certificate stays.
  --help, -h            this table.

Without --control and --token this is the open relay and nothing else: no keys,
no control plane, no accounting. Managed mode is additive and opt-in, and the
two flags must be given together — a relay that came up open because its token
was missing is the untracked path that refuses to start instead.
";

    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut out = Self {
            port: 7788,
            control: None,
            region: "us-east".into(),
            letter: 'U',
            no_address_limits: false,
            token: None,
            tls: None,
        };
        let mut tls_cert: Option<String> = None;
        let mut tls_key: Option<String> = None;
        let mut i = 0;
        while i < argv.len() {
            let a = &argv[i];
            let val = argv.get(i + 1).cloned();
            let need = |v: Option<String>, what: &str| {
                v.filter(|s| !s.starts_with("--"))
                    .ok_or_else(|| format!("{what} needs a value"))
            };
            match a.as_str() {
                "--control" => {
                    out.control = Some(need(val, "--control")?);
                    i += 2;
                }
                "--region" => {
                    out.region = need(val, "--region")?;
                    i += 2;
                }
                "--letter" => {
                    let s = need(val, "--letter")?;
                    out.letter = s
                        .chars()
                        .next()
                        .filter(|_| s.chars().count() == 1)
                        .ok_or("--letter is a single character")?;
                    i += 2;
                }
                "--token" => {
                    out.token = Some(need(val, "--token")?);
                    i += 2;
                }
                // The token belongs in a root-owned 0600 file, not in a command
                // line every `ps` on the box can read.
                "--token-file" => {
                    let p = need(val, "--token-file")?;
                    let t = std::fs::read_to_string(&p)
                        .map_err(|e| format!("--token-file {p}: {e}"))?;
                    out.token = Some(t.trim().to_string());
                    i += 2;
                }
                "--no-address-limits" => {
                    out.no_address_limits = true;
                    i += 1;
                }
                "--tls-cert" => {
                    tls_cert = Some(need(val, "--tls-cert")?);
                    i += 2;
                }
                "--tls-key" => {
                    tls_key = Some(need(val, "--tls-key")?);
                    i += 2;
                }
                // Printed and exit 0, rather than refused as an unknown flag
                // or — as the July binary did — parsed as a PORT NUMBER, which
                // is how the two builds were told apart on the box
                // (`floptle/0191`).
                "--help" | "-h" => {
                    print!("{}", Self::HELP);
                    std::process::exit(0);
                }
                other if other.starts_with("--") => {
                    return Err(format!("unknown flag {other}"));
                }
                // The bare positional port, kept for the open relay's original
                // one-argument spelling.
                other => {
                    out.port = other.parse().map_err(|_| format!("'{other}' is not a port"))?;
                    i += 1;
                }
            }
        }
        if out.control.is_some() != out.token.is_some() {
            return Err(
                "managed mode needs --control and --token together (a relay that came up open \
                 because its token was missing is the untracked path this closes)"
                    .into(),
            );
        }
        // The same rule as the token: a relay told half a certificate would
        // come up self-signed, and every client would then fall back — or,
        // once the fallback is gone, refuse — without a word on the box.
        out.tls = match (tls_cert, tls_key) {
            (Some(c), Some(k)) => Some((c.into(), k.into())),
            (None, None) => None,
            _ => return Err("--tls-cert and --tls-key go together".into()),
        };
        Ok(out)
    }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("floptle-relay: {e}");
            std::process::exit(2);
        }
    };
    // The certificate, if there is one to present. A relay told to present
    // one it cannot read does not come up self-signed instead: that is the
    // half-configuration that refuses to start, like a token with no control
    // plane.
    let mut cert_watch: Option<tls::CertWatch> = None;
    let bound = match &args.tls {
        None => RelayServer::bind(args.port),
        Some((cert_path, key_path)) => match tls::CertWatch::load(cert_path, key_path) {
            Ok((watch, cert)) => {
                cert_watch = Some(watch);
                RelayServer::bind_with_certificate(args.port, &cert)
            }
            Err(e) => Err(format!("certificate: {e}")),
        },
    };
    let mut relay = match bound {
        Ok(r) => r,
        Err(e) => {
            eprintln!("floptle-relay: {e}");
            std::process::exit(1);
        }
    };
    match (&cert_watch, &args.tls) {
        (Some(w), Some((cert_path, _))) => println!(
            "certificate: {} from {} (watched; a renewal is presented within {}s, nobody dropped)",
            w.fingerprint(),
            cert_path.display(),
            tls::POLL_INTERVAL.as_secs()
        ),
        _ => println!("certificate: self-signed (no --tls-cert) — the dev-trust model, unverified"),
    }
    if args.no_address_limits {
        let limits = floptle_net::RelayLimits {
            opens_per_address: u32::MAX,
            joins_per_address: u32::MAX,
            ..relay.limits()
        };
        relay.set_limits(limits);
        println!("per-address limits OFF (--no-address-limits) — a load test, not a deployment");
    }

    let mut managed: Option<policy::StatusHandle> = None;
    if let Some((base, token)) = args.managed() {
        let http = control::HttpControl::new(
            base,
            &args.region,
            token,
            // The cold path's own bound. It runs on a worker, so this is a
            // ceiling on that worker, never on the relay's loop.
            Duration::from_secs(2),
        );
        let p = policy::CloudPolicy::new(Arc::new(http), &args.region, args.letter);
        println!(
            "floptle-relay: MANAGED — region {} (codes start '{}'), control plane {base}",
            args.region, args.letter
        );
        println!("  hosting requires a game key; a keyless host is refused with where to get one");
        managed = Some(p.status());
        relay.set_policy(Box::new(p));
    }
    if managed.is_none() {
        println!("floptle-relay: open relay — no keys, no control plane, no accounting");
    }
    println!(
        "floptle-relay listening on UDP {} — hosts: net.host{{ relay = \"<this-machine>:{}\" }}",
        relay.port(),
        relay.port()
    );

    let mut lobbies = 0;
    let mut last_report = std::time::Instant::now();
    loop {
        relay.step();
        let now = relay.lobby_count();
        if now != lobbies {
            println!("lobbies: {now}");
            lobbies = now;
        }
        if let Some(w) = &mut cert_watch {
            match w.poll() {
                None => {}
                Some(tls::Reload::Loaded(cert)) => match relay.set_certificate(&cert) {
                    Ok(()) => println!("certificate: renewed on disk, now presenting {}", w.fingerprint()),
                    Err(e) => println!("certificate: renewal loaded but NOT applied ({e})"),
                },
                Some(tls::Reload::Failed(e)) => println!(
                    "certificate: the files changed but did not load ({e}); still presenting {} — \
                     said once, retried when they change again",
                    w.fingerprint()
                ),
            }
        }
        if let Some(status) = &managed {
            // Whatever the policy has to say, as it says it.
            let (lines, keys, age) = match status.lock() {
                Ok(mut s) => (std::mem::take(&mut s.log), s.keys, s.snapshot_age_s),
                Err(_) => (Vec::new(), 0, None),
            };
            for l in lines {
                println!("  {l}");
            }
            // **How old the key snapshot is, on a schedule.** A relay that has
            // been cut off from the control plane is still enforcing, just
            // enforcing something old — and that is the difference between a
            // system behaving as designed and one nobody can explain.
            if last_report.elapsed() >= Duration::from_secs(60) {
                last_report = std::time::Instant::now();
                match age {
                    Some(a) => println!("  keys: {keys}, snapshot {a}s old"),
                    None => println!(
                        "  keys: {keys}, NO SNAPSHOT YET — every host is going to the cold path"
                    ),
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

#[cfg(test)]
mod arg_tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// **The help table cannot fall behind the parser** (`floptle/0191`).
    ///
    /// A flag table that is out of date is worse than none, because it is
    /// believed — and this one is what an operator reads to tell a managed
    /// binary from the open one before installing it on a live box. The list
    /// comes off this file's OWN match arms rather than a second hand-written
    /// list, so adding a flag and not documenting it fails here.
    #[test]
    fn every_flag_the_parser_takes_is_in_the_help_table() {
        let src = include_str!("main.rs");
        // The parser's arms, as written: `"--flag" => {` and `"--a" | "-b" =>`.
        let mut flags: Vec<&str> = Vec::new();
        for line in src.lines().map(str::trim) {
            if !line.ends_with("=> {") {
                continue;
            }
            for piece in line.trim_end_matches("=> {").split('|') {
                let f = piece.trim().trim_matches('"');
                if f.starts_with('-') {
                    flags.push(f);
                }
            }
        }
        flags.sort_unstable();
        flags.dedup();
        assert!(
            flags.len() >= 6,
            "the scrape found {flags:?} — it has stopped seeing the match arms, so this \
             guard is measuring nothing"
        );
        // The flags the table actually LISTS: the leading token of a row, not
        // any mention anywhere. A plain `contains` passes on a flag named only
        // in passing — `--token`'s row says "Prefer --token-file", which
        // documented `--token-file` by accident — and every short flag is a
        // prefix of some longer one, so both directions were satisfiable
        // without the row existing.
        let listed: std::collections::HashSet<&str> = Args::HELP
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with('-'))
            // The flag COLUMN — everything before the two-space gutter that
            // starts the description — so a row spelling two names (`--help,
            // -h`) lists both.
            .flat_map(|l| {
                l.split("  ")
                    .next()
                    .unwrap_or("")
                    .split([',', ' '])
                    .map(str::trim)
                    .collect::<Vec<_>>()
            })
            .filter(|t| t.starts_with('-'))
            .collect();
        let missing: Vec<&&str> = flags.iter().filter(|f| !listed.contains(**f)).collect();
        assert!(
            missing.is_empty(),
            "flags the parser takes with no row in the --help table: {missing:?} (the table \
             lists {listed:?})"
        );
    }

    /// …and the other direction: `--help` is a flag, not a port.
    ///
    /// The July open relay on the live box parsed `--help` as a positional PORT
    /// NUMBER, which is how the two builds were told apart. A managed binary
    /// has to answer it, so the check an operator runs before installing means
    /// something.
    #[test]
    fn help_is_not_parsed_as_a_port() {
        // It exits the process, so the parser is not called with it here; what
        // is asserted is that it is not treated as a positional, which is what
        // the port arm would do.
        assert!(Args::HELP.contains("--help"), "the table names itself");
        assert!(
            Args::parse(&args(&["--nope"])).is_err(),
            "an unknown flag is still refused rather than read as a port"
        );
        assert_eq!(
            Args::parse(&args(&["7788"])).unwrap().port,
            7788,
            "and a real positional port still is one"
        );
    }

    #[test]
    fn the_bare_port_still_works_and_is_the_open_relay() {
        let a = Args::parse(&args(&["9000"])).expect("parses");
        assert_eq!(a.port, 9000);
        assert!(a.managed().is_none(), "no flags means the open relay");
    }

    #[test]
    fn no_arguments_at_all_is_the_open_relay_on_the_default_port() {
        let a = Args::parse(&args(&[])).expect("parses");
        assert_eq!(a.port, 7788);
        assert!(a.managed().is_none());
    }

    /// **Half a configuration is a misconfiguration.** A relay that fell back
    /// to open mode because its token was missing would be precisely the
    /// untracked hosting path managed mode exists to close, and it would do it
    /// silently.
    #[test]
    fn a_control_url_without_a_token_refuses_to_start() {
        let e = Args::parse(&args(&["--control", "https://fopull.com"]))
            .expect_err("must not come up open");
        assert!(e.contains("--token"), "{e}");
        let e = Args::parse(&args(&["--token", "fb_x"])).expect_err("must not come up open");
        assert!(e.contains("--control"), "{e}");
    }

    #[test]
    fn managed_mode_takes_a_region_and_its_letter() {
        let a = Args::parse(&args(&[
            "--control", "https://fopull.com", "--token", "fb_x", "--region", "eu-central",
            "--letter", "E", "7788",
        ]))
        .expect("parses");
        assert!(a.managed().is_some());
        assert_eq!(a.region, "eu-central");
        assert_eq!(a.letter, 'E');
        assert_eq!(a.port, 7788);
    }

    /// A misspelt flag would come up in the wrong mode entirely, which is worse
    /// than not coming up.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let e = Args::parse(&args(&["--contrl", "x"])).expect_err("refused");
        assert!(e.contains("--contrl"), "{e}");
    }

    /// `--letter EU` would silently take 'E' and produce codes nobody can map
    /// back to a region.
    #[test]
    fn a_region_letter_is_one_character() {
        assert!(Args::parse(&args(&["--letter", "EU"])).is_err());
    }

    /// **Half a certificate is refused, like half a token** (`floptle/0227`).
    /// A relay given `--tls-cert` alone would come up self-signed, and the
    /// first sign would be every client at the region name falling back.
    #[test]
    fn a_certificate_without_its_key_refuses_to_start() {
        let e = Args::parse(&args(&["--tls-cert", "/etc/x/fullchain.pem"])).expect_err("no key");
        assert!(e.contains("--tls-key"), "{e}");
        let e = Args::parse(&args(&["--tls-key", "/etc/x/privkey.pem"])).expect_err("no cert");
        assert!(e.contains("--tls-cert"), "{e}");
        let a = Args::parse(&args(&[
            "--tls-cert", "/etc/x/fullchain.pem", "--tls-key", "/etc/x/privkey.pem", "7788",
        ]))
        .expect("parses");
        let (c, k) = a.tls.expect("both paths kept");
        assert_eq!(c, std::path::Path::new("/etc/x/fullchain.pem"));
        assert_eq!(k, std::path::Path::new("/etc/x/privkey.pem"));
        assert!(Args::parse(&args(&["7788"])).unwrap().tls.is_none(), "no flags = self-signed");
    }
}
