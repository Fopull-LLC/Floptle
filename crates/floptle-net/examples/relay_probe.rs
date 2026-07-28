//! Prove a relay is actually reachable and serving lobbies.
//!
//!     cargo run -p floptle-net --example relay_probe -- relay.example.com:7788
//!
//! A port scan can only tell you something is bound; UDP can't even tell you
//! that reliably. This does the thing a player's game does — completes a QUIC
//! handshake, registers a lobby, and reads back the code — so a pass means the
//! whole path works: DNS, the cloud firewall, the host firewall, the service.
//! Anything short of that has a failure mode where the port looks open and no
//! one can play.

fn main() {
    let addr = match std::env::args().nth(1) {
        Some(a) => a,
        None => {
            eprintln!("usage: relay_probe <host:port>");
            std::process::exit(2);
        }
    };
    println!("probing {addr} …");
    let t = std::time::Instant::now();
    match floptle_net::RelayHost::host(&addr) {
        Ok((_c, code)) => {
            println!("OK — lobby code {code}  ({} ms)", t.elapsed().as_millis());
            println!("a friend would join with: net.join(\"relay://{addr}/{code}\")");
        }
        Err(e) => {
            eprintln!("FAILED after {} ms: {e}", t.elapsed().as_millis());
            eprintln!(
                "check, in order: the cloud ingress rule (UDP), the host firewall, \
                 and `systemctl status floptle-relay` on the box"
            );
            std::process::exit(1);
        }
    }
}
