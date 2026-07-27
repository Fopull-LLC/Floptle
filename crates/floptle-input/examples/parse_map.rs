//! Parse an `input.ron` and print what it holds — a fast check that a real project's
//! map still loads after a schema change, and that a hand edit is well-formed.
//!
//!     cargo run -p floptle-input --example parse_map -- <project>/input.ron …

fn main() {
    let mut bad = 0;
    for path in std::env::args().skip(1) {
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                println!("{path}: unreadable — {e}");
                bad += 1;
                continue;
            }
        };
        match floptle_input::InputMap::parse(&text) {
            Ok(map) => {
                let scoped = map
                    .actions
                    .iter()
                    .flat_map(|a| &a.bindings)
                    .filter(|b| b.player.is_some())
                    .count();
                println!(
                    "{path}: OK — {} actions, {} axes1, {} axes2, {} motions, {} player(s), \
                     hash {:#018x}{}",
                    map.actions.len(),
                    map.axes1.len(),
                    map.axes2.len(),
                    map.motions.len(),
                    map.players,
                    map.hash(),
                    if scoped > 0 {
                        format!(", {scoped} player-scoped binding(s)")
                    } else {
                        String::new()
                    },
                );
                // Round-trip: what we would write back must read the same.
                match floptle_input::InputMap::parse(&map.to_ron()) {
                    Ok(back) if back == map => {}
                    Ok(_) => {
                        println!("  ⚠ re-serialising changed the map");
                        bad += 1;
                    }
                    Err(e) => {
                        println!("  ⚠ our own output does not re-parse: {e}");
                        bad += 1;
                    }
                }
            }
            Err(e) => {
                println!("{path}: PARSE ERROR — {e}");
                bad += 1;
            }
        }
    }
    if bad > 0 {
        std::process::exit(1);
    }
}
