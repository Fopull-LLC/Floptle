//! **What a relay's link actually carries** (`floptle/0218`).
//!
//! Two decisions are waiting on one number nobody has: whether the free tier can
//! go to 100 CCU, and whether the relay stays on a `VM.Standard.E2.1.Micro`
//! (1 OCPU, 954 MB, **0.48 Gbps**). `floptle/0215` gave the relay instruments;
//! this points something at them.
//!
//! ⚠ **This drives the REAL client and host legs against a REAL relay.** The
//! thing being measured is the forwarding loop and the link, and a mock
//! transport measures neither.
//!
//! # What it measures, and why those two numbers
//!
//! - **Mbps per CCU**, from which a box's ceiling is `0.8 × link / (Mbps per
//!   CCU)`.
//! - **The CCU at which drops first appear**, which is the honest ceiling
//!   whatever the arithmetic says, because that is players losing packets.
//!
//! ⚠ **Traffic SHAPE matters more than volume**, so rate and payload are flags
//! rather than constants. A relay multiplies: one datagram into an eight-player
//! lobby leaves seven times. A bench sending big infrequent packets and one
//! sending small frequent ones find completely different ceilings — and games
//! are the second kind.
//!
//! The host in the loop does as little as it can: it echoes each ping to its
//! sender and — because a game host tells every player what the others did —
//! sends the same bytes to each other member of the lobby (`floptle/0234`;
//! `--echo-only` for the older shape). Anything more spent here is noise in a
//! relay measurement.
//!
//! ⚠ **A ceiling is read off a CLEAN step only.** The first run of this tool
//! printed its highest ceiling on the step where the relay was failing: the
//! senders were stalled on echoes that never came, so the rate fell, so the
//! per-CCU cost fell, so `0.8 × link / cost` went up. Any step with a packet
//! unreturned or a send rate short of the one asked for prints no ceiling.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use floptle_net::{Incoming, QuicClient, RelayClient, RelayHost, SERVER, Transport};

mod args;
use args::Args;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", args::HELP);
        return;
    }
    let args = match Args::parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("floptle-relay-bench: {e}");
            eprintln!("\n{}", args::HELP);
            std::process::exit(2);
        }
    };
    let outcome = if args.verify { verify(&args.relay) } else { run(&args) };
    if let Err(e) = outcome {
        eprintln!("floptle-relay-bench: {e}");
        std::process::exit(1);
    }
}

/// **Which certificate a relay presents, and whether it verifies**
/// (`floptle/0227`). Two handshakes: one under the dev-trust model, to read
/// the leaf whatever it is — so an operator can compare its fingerprint with
/// the file on the box — and one verified against the public roots for the
/// name in `--relay`, with no fallback. The second is what every managed
/// client will run once the fallback is gone, so a relay that fails it here
/// is one that will refuse every player then.
fn verify(relay: &str) -> Result<(), String> {
    let name = relay.rsplit_once(':').map_or(relay, |(h, _)| h);
    let name = name.trim_start_matches('[').trim_end_matches(']');
    if name.parse::<std::net::IpAddr>().is_ok() {
        return Err(format!(
            "{relay} is an address; a certificate is issued for a NAME, so give the relay's \
             name (the one a client would be told)"
        ));
    }

    let mut any = QuicClient::connect(relay)?;
    let presented = loop_until(&mut any, Duration::from_secs(5))?;
    match &presented {
        Some(leaf) => println!("{relay} presents {}", floptle_net::quic::fingerprint_of(leaf)),
        None => return Err(format!("{relay} did not answer a handshake at all within 5 s")),
    }
    drop(any);

    let mut strict = QuicClient::connect_verified(relay, name)?;
    let mut refused: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    let verified = loop {
        for ev in strict.poll() {
            match ev {
                Incoming::Connected(SERVER) => break,
                Incoming::Disconnected(SERVER, why) => refused = Some(why.unwrap_or_default()),
                _ => {}
            }
        }
        if strict.peer_certificate().is_some() {
            break true;
        }
        if refused.is_some() || Instant::now() > deadline {
            break false;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    if verified {
        println!("VERIFIED — the chain is trusted for {name} by the public roots");
        Ok(())
    } else {
        Err(format!(
            "NOT VERIFIED for {name}: {}",
            refused.unwrap_or_else(|| "no answer within 5 s".into())
        ))
    }
}

/// Wait for a dev-trust handshake and return the leaf it presented.
fn loop_until(client: &mut QuicClient, within: Duration) -> Result<Option<Vec<u8>>, String> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        for ev in client.poll() {
            if let Incoming::Disconnected(SERVER, why) = ev {
                return Err(format!("dropped before the handshake: {}", why.unwrap_or_default()));
            }
        }
        if let Some(leaf) = client.peer_certificate() {
            return Ok(Some(leaf));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(None)
}

/// One lobby: a host and the clients attached to it.
struct Lobby {
    host: RelayHost,
    /// The host's view of who is in the lobby — the peers it fans out to.
    /// Filled from the joins the relay reports, not assumed from
    /// `--lobby-size`: a join that never landed must not be sent to.
    peers: Vec<u64>,
    clients: Vec<RelayClient>,
}

/// Bytes at the front of every payload: the sending client's index in its
/// lobby and a ping counter. A client sees every packet the host fans out —
/// its own echo among the other members' — and only the echo is a round
/// trip, so it has to be able to tell them apart.
const TAG: usize = 6;

fn tag(buf: &mut [u8], client: u16, ping: u32) {
    debug_assert!(buf.len() >= TAG, "--payload is floored at 8 in args.rs");
    buf[..2].copy_from_slice(&client.to_le_bytes());
    buf[2..6].copy_from_slice(&ping.to_le_bytes());
}

fn tagged_client(bytes: &[u8]) -> Option<u16> {
    bytes.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}

/// Everything the drive loop counted. Kept apart from the printing so the
/// honesty rules in [`report`] can be tested against numbers rather than
/// against a run.
#[derive(Debug, Default, Clone, PartialEq)]
struct Counts {
    /// Connections opened (the CCU actually driven).
    conns: usize,
    /// Client pings sent.
    sent: u64,
    /// Client pings that came back to their sender.
    echoed: u64,
    /// Messages the host sent — one echo plus one per OTHER member for every
    /// ping it received, unless `--echo-only`.
    host_sent: u64,
    /// Pings the host received.
    host_received: u64,
    /// Fan-out messages clients received that were somebody else's ping.
    fanned_in: u64,
    /// Pings the loop would have sent at the requested rate had every client
    /// been free to send on every tick. A sender waits for its outstanding
    /// ping, so a stalled relay is a shortfall here before it is loss.
    wanted: u64,
}

fn run(args: &Args) -> Result<(), String> {
    let lobbies_wanted = args.ccu.div_ceil(args.lobby_size);
    println!(
        "relay {} — {} CCU in {} lobb{} of {}, {} B every {} ms per client, host {}",
        args.relay,
        args.ccu,
        lobbies_wanted,
        if lobbies_wanted == 1 { "y" } else { "ies" },
        args.lobby_size,
        args.payload,
        args.interval_ms,
        if args.echo_only { "echoes only" } else { "fans out to the lobby" },
    );

    // --- open the lobbies ---------------------------------------------------
    let mut lobbies: Vec<Lobby> = Vec::new();
    let mut opened = 0;
    for i in 0..lobbies_wanted {
        let hosted = match &args.key {
            Some(k) => RelayHost::host_keyed(&args.relay, k, None),
            None => RelayHost::host(&args.relay),
        };
        let (host, code) = hosted.map_err(|e| format!("lobby {i}: {e}"))?;
        // The host seat counts as a player, so a lobby of N wants N-1 clients.
        let mut clients = Vec::new();
        for c in 0..args.lobby_size.saturating_sub(1) {
            match RelayClient::join(&args.relay, &code) {
                Ok(cl) => clients.push(cl),
                // ⚠ At 400 CCU the first failure here was `Too many open
                // files` on the DRIVING box (ulimit 1024), which reads like
                // a relay fault and is not one. Say which side it is.
                Err(e) => {
                    return Err(format!(
                        "lobby {i} client {c}: {e} (if this says too many open files, it is \
                         this machine's ulimit -n, not the relay — see --help)"
                    ))
                }
            }
        }
        opened += 1 + clients.len();
        lobbies.push(Lobby { host, peers: Vec::new(), clients });
    }
    println!("  {opened} connection(s) open; settling");

    // Let every join land before anything is timed. A join still in flight
    // would otherwise be counted as a slow round trip. The host learns its
    // roster here too.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(2) {
        for l in &mut lobbies {
            for ev in l.host.poll() {
                roster(&mut l.peers, &ev);
            }
            for c in &mut l.clients {
                let _ = c.poll();
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // --- drive it -----------------------------------------------------------
    //
    // ⚠ **Round trips are measured on the CLIENT leg**, client → relay → host →
    // relay → client. That is the path a player's input actually takes, and it
    // is the number the relay's own `step_p95_ms` has to be checked against —
    // an independent measurement, from outside the box.
    let mut payload = vec![0u8; args.payload];
    let mut rtts: Vec<f32> = Vec::new();
    let mut n = Counts { conns: opened, ..Default::default() };
    // (lobby, client) → the moment its outstanding ping left.
    let mut inflight: HashMap<(usize, usize), Instant> = HashMap::new();
    let mut ping: u32 = 0;

    let started = Instant::now();
    let mut next_send = Instant::now();
    let interval = Duration::from_millis(args.interval_ms);

    while started.elapsed() < Duration::from_secs(args.seconds) {
        let now = Instant::now();
        if now >= next_send {
            next_send = now + interval;
            ping = ping.wrapping_add(1);
            for (li, l) in lobbies.iter_mut().enumerate() {
                for (ci, c) in l.clients.iter_mut().enumerate() {
                    n.wanted += 1;
                    // One outstanding ping per client: a second would measure
                    // queueing against ourselves rather than the relay.
                    if inflight.contains_key(&(li, ci)) {
                        continue;
                    }
                    tag(&mut payload, ci as u16, ping);
                    c.send(SERVER, args.channel, &payload);
                    inflight.insert((li, ci), now);
                    n.sent += 1;
                }
            }
        }

        for (li, l) in lobbies.iter_mut().enumerate() {
            // **The host does the least it can**: it echoes, and — because a
            // game host tells every player what the others did — sends the
            // same bytes to each other member. Anything more spent here is
            // noise in a relay measurement.
            let mut incoming: Vec<(u64, Vec<u8>)> = Vec::new();
            for ev in l.host.poll() {
                roster(&mut l.peers, &ev);
                if let Incoming::Message(peer, _, bytes) = ev {
                    incoming.push((peer, bytes));
                }
            }
            for (peer, bytes) in incoming {
                n.host_received += 1;
                l.host.send(peer, args.channel, &bytes);
                n.host_sent += 1;
                if !args.echo_only {
                    for &other in l.peers.iter().filter(|&&p| p != peer) {
                        l.host.send(other, args.channel, &bytes);
                        n.host_sent += 1;
                    }
                }
            }
            for (ci, c) in l.clients.iter_mut().enumerate() {
                for inc in c.poll() {
                    let Incoming::Message(_, _, bytes) = inc else { continue };
                    if tagged_client(&bytes) == Some(ci as u16) {
                        if let Some(t) = inflight.remove(&(li, ci)) {
                            rtts.push(t.elapsed().as_secs_f32() * 1000.0);
                            n.echoed += 1;
                        }
                    } else {
                        n.fanned_in += 1;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    print!("{}", report(args, started.elapsed(), &n, &mut rtts));
    Ok(())
}

/// Keep a host's roster from the relay's own join/leave notices.
fn roster(peers: &mut Vec<u64>, ev: &Incoming) {
    match ev {
        Incoming::Connected(p) => {
            if !peers.contains(p) {
                peers.push(*p);
            }
        }
        Incoming::Disconnected(p, _) => peers.retain(|q| q != p),
        _ => {}
    }
}

/// What a step's numbers say about whether its cost figures can be believed.
/// `None` is a clean step; `Some(why)` is one that is past the limit, whose
/// `per CCU` is not a cost and whose ceiling must not be printed.
///
/// ⚠ **The arithmetic went UP as the relay failed.** At 400 CCU the drive
/// loop sent 45,798 of the 240,000 pings the flags asked for — every sender
/// was waiting on an echo the relay had dropped — so the payload rate fell,
/// `per CCU` fell with it, and `0.8 × link / per CCU` printed `implied
/// ceiling 49446 CCU` on the very step where players would have been losing
/// packets. A ceiling is only a ceiling on a step that returned everything
/// it sent at the rate it was asked to send.
fn past_the_limit(n: &Counts) -> Option<String> {
    let lost = n.sent.saturating_sub(n.echoed);
    if lost > 0 {
        return Some(format!("{lost} unreturned"));
    }
    // The loop's 1 ms sleep costs a tick or two over a run; anything past a
    // twentieth is senders stalled on the relay, not the clock.
    if n.wanted > 0 && n.sent * 20 < n.wanted * 19 {
        return Some(format!(
            "senders stalled: sent {} of the {} pings the rate asked for",
            n.sent, n.wanted
        ));
    }
    None
}

fn report(args: &Args, elapsed: Duration, n: &Counts, rtts: &mut [f32]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let secs = elapsed.as_secs_f64().max(0.001);
    let payload_bits = (args.payload as f64) * 8.0;
    // What the relay sent on, counted where it ARRIVED: the host's copy of
    // every ping, and every echo and fan-out a client received. Egress is the
    // half that is metered and the half a relay multiplies — with fan-out, a
    // lobby of eight turns one ping into eight packets out.
    let egress_pkts = n.host_received + n.echoed + n.fanned_in;
    let egress_mbps = egress_pkts as f64 * payload_bits / secs / 1_000_000.0;
    // What it took in: every ping the clients sent and everything the host
    // sent back. Counted at the senders, so a packet the kernel dropped
    // before the relay read it is in here — that is the point of comparing
    // it against the relay's own rx_drops.
    let ingress_mbps = (n.sent + n.host_sent) as f64 * payload_bits / secs / 1_000_000.0;
    let per_ccu = if n.conns > 0 { egress_mbps / n.conns as f64 } else { 0.0 };
    let verdict = past_the_limit(n);

    let _ = writeln!(out, "\n--- {} CCU, {:.1}s ---", n.conns, secs);
    let _ = writeln!(out, "  sent            {}  (of {} the rate asked for)", n.sent, n.wanted);
    let _ = writeln!(out, "  echoed          {}", n.echoed);
    // ⚠ **Loss is the honest ceiling**, whatever the arithmetic says: it is
    // players losing packets. A run that reports any is past the limit.
    let lost = n.sent.saturating_sub(n.echoed);
    let loss_pct = if n.sent > 0 { lost as f64 * 100.0 / n.sent as f64 } else { 0.0 };
    let _ = writeln!(out, "  unreturned      {lost}  ({loss_pct:.2}%)");
    if !args.echo_only {
        // Each ping the host received went to every OTHER member.
        let others = args.lobby_size.saturating_sub(2) as u64;
        let fan_expected = n.host_received * others;
        let fan_lost = fan_expected.saturating_sub(n.fanned_in);
        let _ = writeln!(
            out,
            "  fanned out      {} received of {} sent to the other members ({} short)",
            n.fanned_in, fan_expected, fan_lost
        );
    }
    let _ = writeln!(out, "  relay egress    {egress_mbps:.2} Mbps  (payload; what it sent on)");
    let _ = writeln!(out, "  relay ingress   {ingress_mbps:.2} Mbps  (payload; what was sent to it)");
    match &verdict {
        None => {
            let _ = writeln!(out, "  per CCU         {per_ccu:.4} Mbps");
            if per_ccu > 0.0 {
                // 80% of the link, because a link at 100% is already dropping.
                let ceiling = 0.8 * args.link_mbps / per_ccu;
                let _ = writeln!(
                    out,
                    "  implied ceiling {ceiling:.0} CCU at 80% of {:.0} Mbps",
                    args.link_mbps
                );
            }
        }
        Some(why) => {
            let _ = writeln!(
                out,
                "  per CCU         {per_ccu:.4} Mbps  ⚠ NOT a cost: this step is past the limit ({why})"
            );
            let _ = writeln!(
                out,
                "  implied ceiling withheld — a ceiling is read off a clean step, and this one \
                 is above it"
            );
        }
    }
    match percentile(rtts, 0.50) {
        Some(p50) => {
            let _ = writeln!(out, "  round trip p50  {p50:.2} ms");
            let _ = writeln!(out, "  round trip p95  {:.2} ms", percentile(rtts, 0.95).unwrap_or(p50));
            let _ = writeln!(out, "  round trip max  {:.2} ms", rtts.last().copied().unwrap_or(p50));
        }
        // ⚠ Not "0 ms". Nothing came back, which is a total failure and must
        // not print as the fastest relay ever measured.
        // Width rather than literal spaces: a long run of them inside a string
        // is what a dropped line-continuation backslash looks like, and the
        // repo has a guard that cannot tell the two apart — correctly, since
        // the one time it guessed it exempted a real hole.
        None => {
            let _ = writeln!(out, "  {:<15} NOTHING RETURNED", "round trip");
        }
    }
    let _ = writeln!(
        out,
        "\nCheck this against the relay's own reading for the same window \
         (load1/cores, egress_bytes_per_sec, rx_drops, step_p95_ms).\n\
         ⚠ rx_drops rising is the real ceiling — it is packets already thrown away."
    );
    out
}

/// Nearest-rank percentile. `None` for an empty sample — **never `0.0`**, which
/// would report a relay that returned nothing as the fastest one ever measured.
fn percentile(v: &mut [f32], q: f64) -> Option<f32> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f32::total_cmp);
    let i = (((v.len() as f64) * q).ceil() as usize).saturating_sub(1).min(v.len() - 1);
    Some(v[i])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠ **An empty sample is `None`, not a very fast relay.**
    ///
    /// A run where nothing came back is a total failure. Printing `0.00 ms` for
    /// it would report the worst possible result as the best one — and this
    /// number exists to be compared against the relay's own `step_p95_ms`.
    #[test]
    fn a_run_that_returned_nothing_reports_nothing_rather_than_zero() {
        assert_eq!(percentile(&mut [], 0.95), None);
        assert_eq!(percentile(&mut [4.0], 0.95), Some(4.0));
        let mut v: Vec<f32> = (1..=20).map(|i| i as f32).collect();
        assert_eq!(percentile(&mut v, 0.95), Some(19.0));
        assert_eq!(percentile(&mut [2.0, 1.0], 0.95), Some(2.0));
    }

    fn bench_args() -> Args {
        Args::parse(&["--relay".to_string(), "h:1".to_string()]).unwrap()
    }

    /// The numbers W's 400 CCU step produced, to the packet.
    fn w_400() -> Counts {
        Counts {
            conns: 400,
            sent: 45_798,
            echoed: 44_000,
            host_sent: 44_000 * 7,
            host_received: 44_000,
            fanned_in: 44_000 * 6,
            wanted: 240_000,
        }
    }

    /// ⚠ **A step past the limit prints no ceiling** (`floptle/0234`).
    ///
    /// At 400 CCU the relay was dropping, every sender was stalled on an echo
    /// that never came, the payload rate fell, and the arithmetic printed
    /// `implied ceiling 49446 CCU` — up from the clean steps, on the one step
    /// where players lose packets. Two independent conditions withhold it:
    /// anything unreturned, and a send rate short of the one asked for. Each
    /// is asserted alone, because a relay can stall senders without dropping
    /// (the reliable channel's backpressure) and drop without stalling them.
    #[test]
    fn a_ceiling_is_not_printed_on_a_step_that_lost_packets_or_stalled_its_senders() {
        let a = bench_args();
        let clean = Counts {
            conns: 56,
            sent: 10_000,
            echoed: 10_000,
            host_sent: 70_000,
            host_received: 10_000,
            fanned_in: 60_000,
            wanted: 10_000,
        };
        assert_eq!(past_the_limit(&clean), None);
        let r = report(&a, Duration::from_secs(30), &clean, &mut [5.0, 6.0]);
        assert!(r.contains("implied ceiling") && r.contains("CCU at 80%"), "{r}");
        assert!(!r.contains("NOT a cost"), "{r}");

        let lossy = Counts { echoed: 9_999, ..clean.clone() };
        assert_eq!(past_the_limit(&lossy).as_deref(), Some("1 unreturned"));
        let r = report(&a, Duration::from_secs(30), &lossy, &mut [5.0]);
        assert!(r.contains("implied ceiling withheld"), "{r}");
        assert!(!r.contains("CCU at 80%"), "{r}");
        assert!(r.contains("NOT a cost"), "{r}");

        // Every ping returned; the senders simply could not send them.
        let stalled = Counts { sent: 9_000, echoed: 9_000, ..clean.clone() };
        let why = past_the_limit(&stalled).expect("a shortfall is past the limit");
        assert!(why.contains("stalled") && why.contains("9000 of the 10000"), "{why}");
        let r = report(&a, Duration::from_secs(30), &stalled, &mut [5.0]);
        assert!(r.contains("implied ceiling withheld"), "{r}");

        // A tick or two lost to the loop's own sleep is not a stall.
        let jitter = Counts { sent: 9_960, echoed: 9_960, ..clean.clone() };
        assert_eq!(past_the_limit(&jitter), None);

        // And W's actual step, which printed a ceiling of 49,446.
        let r = report(&a, Duration::from_secs(30), &w_400(), &mut [700.0, 774.0]);
        assert!(!r.contains("49446") && r.contains("withheld"), "{r}");
    }

    /// ⚠ **Egress is what the relay multiplies, and the report must show it
    /// growing with the lobby.** The first cut's host answered only the sender,
    /// so `ingress == egress` to the byte in every sample and `--lobby-size`
    /// changed the lobby count and nothing else. With fan-out, a lobby of
    /// eight turns each ping into eight packets out — and the report's egress
    /// is counted where packets ARRIVED, so a fan-out the relay dropped is
    /// not in it.
    #[test]
    fn egress_is_counted_at_the_receivers_and_grows_with_the_lobby() {
        let a = bench_args();
        let n = w_400();
        let r = report(&a, Duration::from_secs(30), &n, &mut [1.0]);
        // 128 B × 8 bits × (44000 + 44000 + 264000) / 30 s = 12.0 Mbps.
        assert!(r.contains("relay egress    12.01 Mbps"), "{r}");
        // Ingress at the senders: 45798 + 308000 pings' worth.
        assert!(r.contains("relay ingress   12.08 Mbps"), "{r}");
        assert!(r.contains("fanned out      264000 received of 264000"), "{r}");

        // The same run with an echo-only host carries an eighth of it.
        let echo = Args { echo_only: true, ..bench_args() };
        let n = Counts { host_sent: 44_000, fanned_in: 0, ..w_400() };
        let r = report(&echo, Duration::from_secs(30), &n, &mut [1.0]);
        assert!(r.contains("relay egress    3.00 Mbps"), "{r}");
        assert!(!r.contains("fanned out"), "{r}");
    }

    /// A client tells its own echo from the other members' pings by the tag,
    /// and the tag survives the payload floor.
    #[test]
    fn a_client_recognises_its_own_ping_among_the_fan_out() {
        let mut buf = vec![0u8; 8];
        tag(&mut buf, 5, 0xDEAD_BEEF);
        assert_eq!(tagged_client(&buf), Some(5));
        assert_eq!(tagged_client(&buf[..1]), None);
        assert_eq!(TAG, 6);
    }
}
