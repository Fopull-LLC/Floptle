//! The bench's command line.
//!
//! ⚠ **Rate and payload are flags, not constants** (`floptle/0218`). A relay
//! multiplies traffic, so the shape of it decides the ceiling far more than the
//! volume: big infrequent packets and small frequent ones find completely
//! different limits, and games are the second kind.

use floptle_net::Channel;

pub const HELP: &str = "\
floptle-relay-bench — what a relay's link actually carries

USAGE
  floptle-relay-bench --relay <host:port> [options]

OPTIONS
  --relay <host:port>   the relay to drive. Required.
  --ccu <n>             simulated concurrent players (default 50)
  --lobby-size <n>      players per lobby, host included (default 8)
  --payload <bytes>     payload per packet (default 128; at least 8 — the
                        first bytes carry the sender and a ping number)
  --interval <ms>       per-client send interval (default 50, i.e. 20/s)
  --seconds <n>         how long to drive it (default 30)
  --link <mbps>         the relay's link, for the implied ceiling (default 480)
  --key <game-key>      present a game key; required by a MANAGED relay
  --unreliable          send on the unreliable channel (default: reliable)
  --echo-only           the host answers only the sender. By default it also
                        sends each ping to every OTHER member of its lobby,
                        the shape a game host produces — so --lobby-size
                        changes what the relay sends on, not just how many
                        lobbies there are
  --verify              drive nothing: report which certificate the relay
                        presents (its SHA-256 fingerprint, openssl's spelling)
                        and whether it verifies for the NAME in --relay against
                        the public roots. Exit 0 verified, 1 not. The check a
                        managed region's certificate is ticked by — openssl
                        s_client speaks TCP and never reaches a QUIC relay.
  -h, --help            this

WHAT TO READ
  per CCU          Mbps of relay egress one player costs. A box's ceiling is
                   0.8 x link / this — printed only on a CLEAN step.
  unreturned       packets that never came back. Anything above zero means you
                   are past the limit, whatever the arithmetic says.
  sent (of N)      pings sent against the number the rate asked for. A sender
                   waits for its echo, so a stalled relay shows here as a
                   shortfall before it shows as loss.
  fanned out       what the other members received of what the host sent them.
  round trip p95   the independent check on the relay's own step_p95_ms.

  A step with anything unreturned, or a send rate short of the ask, prints
  no ceiling: its numbers went DOWN because the relay failed, and a ceiling
  read off them goes up as it fails. Take the relay's own reading for the
  same window and compare. rx_drops rising is the honest ceiling: those are
  packets already thrown away.

BEFORE A BIG RUN
  Every simulated player is a socket on THIS machine. Past ~400 CCU the
  default limit of 1024 open files fails a join with `Too many open files`,
  which reads like a relay fault and is not one:
    ulimit -n 65536

EXAMPLE
  Walk the curve, recording the relay's box stats at each step:
    for n in 50 100 200 400; do
      floptle-relay-bench --relay us-east.relay.fopull.com:7777 --ccu $n
    done
";

pub struct Args {
    pub relay: String,
    pub ccu: usize,
    pub lobby_size: usize,
    pub payload: usize,
    pub interval_ms: u64,
    pub seconds: u64,
    pub link_mbps: f64,
    pub key: Option<String>,
    pub channel: Channel,
    pub verify: bool,
    /// The host answers the sender only, rather than every member.
    pub echo_only: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            relay: String::new(),
            ccu: 50,
            lobby_size: 8,
            payload: 128,
            // 20 packets a second: a common fixed-tick send rate, and the shape
            // a game actually produces.
            interval_ms: 50,
            seconds: 30,
            // VM.Standard.E2.1.Micro, the box this exists to size.
            link_mbps: 480.0,
            key: None,
            channel: Channel::Reliable,
            verify: false,
            echo_only: false,
        }
    }
}

impl Args {
    pub fn parse(argv: &[String]) -> Result<Self, String> {
        let mut a = Args::default();
        let mut i = 0;
        while i < argv.len() {
            let flag = argv[i].as_str();
            let val = || -> Result<String, String> {
                argv.get(i + 1)
                    .cloned()
                    .ok_or_else(|| format!("{flag} needs a value"))
            };
            match flag {
                "--unreliable" => {
                    a.channel = Channel::Unreliable;
                    i += 1;
                    continue;
                }
                "--verify" => {
                    a.verify = true;
                    i += 1;
                    continue;
                }
                "--echo-only" => {
                    a.echo_only = true;
                    i += 1;
                    continue;
                }
                "--relay" => a.relay = val()?,
                "--ccu" => a.ccu = num(&val()?, flag)?,
                "--lobby-size" => a.lobby_size = num(&val()?, flag)?,
                "--payload" => a.payload = num(&val()?, flag)?,
                "--interval" => a.interval_ms = num(&val()?, flag)? as u64,
                "--seconds" => a.seconds = num(&val()?, flag)? as u64,
                "--link" => {
                    a.link_mbps =
                        val()?.parse().map_err(|_| format!("{flag} takes a number of Mbps"))?
                }
                "--key" => a.key = Some(val()?),
                other => return Err(format!("unknown flag {other}")),
            }
            i += 2;
        }
        if a.relay.is_empty() {
            return Err("--relay <host:port> is required".into());
        }
        // ⚠ A lobby of one has no clients in it, so nothing would ever be sent
        // and the run would report a perfect zero-loss result having measured
        // nothing at all.
        if a.lobby_size < 2 {
            return Err("--lobby-size must be at least 2 (a host and a player)".into());
        }
        if a.ccu == 0 {
            return Err("--ccu must be at least 1".into());
        }
        // The tag that lets a client tell its own echo from the fan-out.
        if a.payload < 8 {
            return Err("--payload must be at least 8 bytes".into());
        }
        if a.interval_ms == 0 {
            return Err("--interval must be at least 1 ms".into());
        }
        if a.seconds == 0 {
            return Err("--seconds must be at least 1".into());
        }
        Ok(a)
    }
}

fn num(s: &str, flag: &str) -> Result<usize, String> {
    s.parse().map_err(|_| format!("{flag} takes a whole number, got {s:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// The shape flags are the point of the tool, so they are asserted rather
    /// than assumed to have arrived.
    #[test]
    fn the_traffic_shape_is_settable_because_it_decides_the_answer() {
        let a = Args::parse(&argv(&[
            "--relay", "h:1", "--ccu", "400", "--lobby-size", "4",
            "--payload", "512", "--interval", "16", "--seconds", "60",
            "--link", "1000", "--unreliable",
        ]))
        .expect("parses");
        assert_eq!(a.ccu, 400);
        assert_eq!(a.lobby_size, 4);
        assert_eq!(a.payload, 512);
        assert_eq!(a.interval_ms, 16);
        assert_eq!(a.seconds, 60);
        assert_eq!(a.link_mbps, 1000.0);
        assert_eq!(a.channel, Channel::Unreliable);
    }

    /// ⚠ **A run that could not measure anything is refused up front.**
    ///
    /// A lobby of one has no clients, so nothing is ever sent — and the report
    /// would print a flawless zero-loss result having driven no traffic at all.
    /// A bench that reports success for measuring nothing is worse than one
    /// that crashes.
    #[test]
    fn a_configuration_that_would_measure_nothing_is_refused() {
        assert!(Args::parse(&argv(&["--relay", "h:1", "--lobby-size", "1"])).is_err());
        assert!(Args::parse(&argv(&["--relay", "h:1", "--ccu", "0"])).is_err());
        assert!(Args::parse(&argv(&["--relay", "h:1", "--interval", "0"])).is_err());
        assert!(Args::parse(&argv(&["--relay", "h:1", "--seconds", "0"])).is_err());
        assert!(Args::parse(&argv(&["--relay", "h:1", "--payload", "4"])).is_err());
        assert!(Args::parse(&argv(&["--ccu", "10"])).is_err(), "a relay is required");
    }

    /// Fan-out is the default because it is the shape a game produces; the
    /// echo-only host is the opt-out, and it is a flag rather than a mode.
    #[test]
    fn the_host_fans_out_unless_told_not_to() {
        assert!(!Args::parse(&argv(&["--relay", "h:1"])).unwrap().echo_only);
        assert!(Args::parse(&argv(&["--relay", "h:1", "--echo-only"])).unwrap().echo_only);
    }

    /// ⚠ **Every flag the parser takes is in the help text.**
    ///
    /// The list comes off this file's OWN match arms, so adding a flag and not
    /// documenting it fails here — W has to run this without E present, and an
    /// undocumented flag may as well not exist.
    /// `--verify` is a mode, not a run: it needs the relay and nothing else,
    /// and the shape flags do not apply to it.
    #[test]
    fn verify_is_a_mode_that_needs_only_the_relay() {
        let a = Args::parse(&argv(&["--relay", "us-east.relay.fopull.com:7788", "--verify"]))
            .expect("parses");
        assert!(a.verify);
        assert!(Args::parse(&argv(&["--verify"])).is_err(), "a relay is still required");
        assert!(!Args::parse(&argv(&["--relay", "h:1"])).unwrap().verify);
    }

    #[test]
    fn every_flag_the_parser_takes_is_in_the_help() {
        let src = include_str!("args.rs");
        let mut flags: Vec<&str> = Vec::new();
        for line in src.lines().map(str::trim) {
            let Some(rest) = line.strip_prefix('"') else { continue };
            let Some((f, tail)) = rest.split_once('"') else { continue };
            if f.starts_with("--") && tail.trim_start().starts_with("=>") {
                flags.push(f);
            }
        }
        flags.sort_unstable();
        flags.dedup();
        // Without this the guard passes by finding nothing the day the parser
        // is reformatted — measuring nothing while reporting success.
        assert!(flags.len() >= 8, "the scrape found {flags:?} and has stopped seeing the arms");
        for f in flags {
            assert!(HELP.contains(f), "{f} is accepted and undocumented");
        }
    }
}
