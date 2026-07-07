//! The reference relay (`docs/netcode-design.md` §10, ADR-0022): hosts get a
//! lobby code, clients join with it, traffic forwards both ways — nobody
//! port-forwards. Self-hostable by anyone; Floptle Cloud runs the managed one.
//!
//!     floptle-relay [port]        (default 7788)

use floptle_net::RelayServer;

fn main() {
    let port = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(7788u16);
    let mut relay = match RelayServer::bind(port) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("floptle-relay: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "floptle-relay listening on UDP {} — hosts: net.host{{ relay = \"<this-machine>:{}\" }}",
        relay.port(),
        relay.port()
    );
    let mut lobbies = 0;
    loop {
        relay.step();
        let now = relay.lobby_count();
        if now != lobbies {
            println!("lobbies: {now}");
            lobbies = now;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}
