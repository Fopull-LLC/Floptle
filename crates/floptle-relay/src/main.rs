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

mod control;
mod policy;

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

    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut out = Self {
            port: 7788,
            control: None,
            region: "us-east".into(),
            letter: 'U',
            token: None,
        };
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
    let mut relay = match RelayServer::bind(args.port) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("floptle-relay: {e}");
            std::process::exit(1);
        }
    };

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
}
