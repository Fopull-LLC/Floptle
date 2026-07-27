//! `cargo run -p floptle-input --example pad_probe`
//!
//! Prints what the gamepad backend actually sees, once per 100 ms, resolved
//! through the starter action map. Use it to confirm a controller is detected
//! and mapped correctly (bumpers vs triggers especially) without launching the
//! editor. Exits after ~15 s, or on Ctrl-C.

fn main() {
    env_logger::init();

    let mut pads = floptle_input::Pads::new();
    if !pads.available() {
        eprintln!("gamepad backend did not start — nothing to probe");
        return;
    }

    let map = floptle_input::InputMap::starter();
    let mut rt = floptle_input::ActionRuntime::new();
    let mut raw = floptle_input::RawInput::default();

    println!("probing for 15s — press buttons and move the sticks…\n");
    let step = std::time::Duration::from_millis(100);
    let mut last_line = String::new();

    for _ in 0..150 {
        raw.pressed.clear();
        raw.released.clear();
        pads.pump(&mut raw);

        let names = pads.slot_names();
        let connected: Vec<String> = names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| n.as_ref().map(|n| format!("P{} = {n}", i + 1)))
            .collect();

        let state = rt.resolve(&map, &raw, 0, 0.1, floptle_input::AllowMask::ALL);
        let held: Vec<&str> = map
            .actions
            .iter()
            .enumerate()
            .filter(|(i, _)| state.is_held(*i))
            .map(|(_, a)| a.name.as_str())
            .collect();
        let mv = state.axis2(map.axis2_index("Move").unwrap_or(0));

        // Only reprint when something changed, so the output stays readable.
        let line = format!(
            "pads: [{}]  held: [{}]  Move: ({:+.2}, {:+.2})",
            connected.join(", "),
            held.join(", "),
            mv.0,
            mv.1
        );
        if line != last_line {
            println!("{line}");
            last_line = line;
        }
        std::thread::sleep(step);
    }
}
