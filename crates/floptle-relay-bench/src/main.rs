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
//! The host in the loop does as little as it can. Anything it spends is noise
//! in a relay measurement.

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
    clients: Vec<RelayClient>,
}

fn run(args: &Args) -> Result<(), String> {
    let lobbies_wanted = args.ccu.div_ceil(args.lobby_size);
    println!(
        "relay {} — {} CCU in {} lobb{} of {}, {} B every {} ms per client",
        args.relay,
        args.ccu,
        lobbies_wanted,
        if lobbies_wanted == 1 { "y" } else { "ies" },
        args.lobby_size,
        args.payload,
        args.interval_ms,
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
                Err(e) => return Err(format!("lobby {i} client {c}: {e}")),
            }
        }
        opened += 1 + clients.len();
        lobbies.push(Lobby { host, clients });
    }
    println!("  {opened} connection(s) open; settling");

    // Let every join land before anything is timed. A join still in flight
    // would otherwise be counted as a slow round trip.
    let settle = Instant::now();
    while settle.elapsed() < Duration::from_secs(2) {
        for l in &mut lobbies {
            let _ = l.host.poll();
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
    let payload = vec![0u8; args.payload];
    let mut rtts: Vec<f32> = Vec::new();
    let mut sent: u64 = 0;
    let mut echoed: u64 = 0;
    // (lobby, client) → the moment its outstanding ping left.
    let mut inflight: HashMap<(usize, usize), Instant> = HashMap::new();

    let started = Instant::now();
    let mut next_send = Instant::now();
    let interval = Duration::from_millis(args.interval_ms);

    while started.elapsed() < Duration::from_secs(args.seconds) {
        let now = Instant::now();
        if now >= next_send {
            next_send = now + interval;
            for (li, l) in lobbies.iter_mut().enumerate() {
                for (ci, c) in l.clients.iter_mut().enumerate() {
                    // One outstanding ping per client: a second would measure
                    // queueing against ourselves rather than the relay.
                    if inflight.contains_key(&(li, ci)) {
                        continue;
                    }
                    c.send(SERVER, args.channel, &payload);
                    inflight.insert((li, ci), now);
                    sent += 1;
                }
            }
        }

        for (li, l) in lobbies.iter_mut().enumerate() {
            // **The host does the least it can**: it echoes, and nothing else.
            // Anything spent here is noise in a relay measurement.
            let incoming: Vec<(u64, Vec<u8>)> = l
                .host
                .poll()
                .into_iter()
                .filter_map(|i| match i {
                    Incoming::Message(peer, _, bytes) => Some((peer, bytes)),
                    _ => None,
                })
                .collect();
            for (peer, bytes) in incoming {
                l.host.send(peer, args.channel, &bytes);
            }
            for (ci, c) in l.clients.iter_mut().enumerate() {
                for inc in c.poll() {
                    if let Incoming::Message(..) = inc
                        && let Some(t) = inflight.remove(&(li, ci))
                    {
                        rtts.push(t.elapsed().as_secs_f32() * 1000.0);
                        echoed += 1;
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    report(args, started.elapsed(), opened, sent, echoed, &mut rtts);
    Ok(())
}

fn report(
    args: &Args,
    elapsed: Duration,
    conns: usize,
    sent: u64,
    echoed: u64,
    rtts: &mut [f32],
) {
    let secs = elapsed.as_secs_f64().max(0.001);
    // Every payload crosses the relay twice — in to the host, back to the
    // client — and the relay both receives and sends each time.
    let payload_bits = (args.payload as f64) * 8.0;
    let client_mbps = (echoed as f64 * payload_bits * 2.0) / secs / 1_000_000.0;
    let per_ccu = if conns > 0 { client_mbps / conns as f64 } else { 0.0 };

    println!("\n--- {conns} CCU, {:.1}s ---", secs);
    println!("  sent            {sent}");
    println!("  echoed          {echoed}");
    // ⚠ **Loss is the honest ceiling**, whatever the arithmetic says: it is
    // players losing packets. A run that reports any is past the limit.
    let lost = sent.saturating_sub(echoed);
    let loss_pct = if sent > 0 { lost as f64 * 100.0 / sent as f64 } else { 0.0 };
    println!("  unreturned      {lost}  ({loss_pct:.2}%)");
    println!("  payload rate    {client_mbps:.2} Mbps across the relay");
    println!("  per CCU         {per_ccu:.4} Mbps");
    if per_ccu > 0.0 {
        // 80% of the link, because a link at 100% is already dropping.
        let ceiling = 0.8 * args.link_mbps / per_ccu;
        println!("  implied ceiling {ceiling:.0} CCU at 80% of {:.0} Mbps", args.link_mbps);
    }
    match percentile(rtts, 0.50) {
        Some(p50) => {
            println!("  round trip p50  {p50:.2} ms");
            println!("  round trip p95  {:.2} ms", percentile(rtts, 0.95).unwrap_or(p50));
            println!("  round trip max  {:.2} ms", rtts.last().copied().unwrap_or(p50));
        }
        // ⚠ Not "0 ms". Nothing came back, which is a total failure and must
        // not print as the fastest relay ever measured.
        // Width rather than literal spaces: a long run of them inside a string
        // is what a dropped line-continuation backslash looks like, and the
        // repo has a guard that cannot tell the two apart — correctly, since
        // the one time it guessed it exempted a real hole.
        None => println!("  {:<15} NOTHING RETURNED", "round trip"),
    }
    println!(
        "\nCheck this against the relay's own reading for the same window \
         (load1/cores, egress_bps, rx_drops, step_p95_ms).\n\
         ⚠ rx_drops rising is the real ceiling — it is packets already thrown away."
    );
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
}
