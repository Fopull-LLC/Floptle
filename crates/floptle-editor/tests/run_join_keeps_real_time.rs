//! **`floptle run --join` is a client, so it keeps real time.**
//!
//! A regression run steps as fast as the CPU allows. A run that has joined a
//! `floptle serve` is a peer of something ticking on the wall clock, and a
//! peer that steps ten simulated seconds per wall second sends an input clock
//! the server watches race ahead until it drops the peer — while the client
//! goes on believing it is joined. So a joined run holds each step to its
//! tick period. Guarded here across two real processes: a server writing its
//! status file, a client run for five seconds, the wall time it took, and the
//! server's roster — written at its fifth second — still holding the peer.

// The `floptle` binary needs the authoring half; see the note at the top of
// `the_json_verbs_emit_only_json.rs`.
#![cfg(feature = "editor-ui")]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_floptle")
}

fn temp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fljoin-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// A scene the server will serve: one Networked node, one camera.
const SCENE: &str = r#"(
    name: "mp",
    nodes: [
        (
            name: "Camera",
            transform: (translation: (0.0, 2.0, 6.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Camera(fov_y: 1.0, active: true),
            scripts: [],
        ),
        (
            name: "Ball",
            transform: (translation: (0.0, 1.0, 0.0), rotation: (0.0, 0.0, 0.0, 1.0), scale: (1.0, 1.0, 1.0)),
            matter: Primitive(shape: Sphere, color: (1.0, 0.5, 0.2)),
            scripts: [],
            net: Some(()),
        ),
    ],
)
"#;

#[test]
fn a_joined_run_takes_its_seconds_of_wall_time_and_the_server_still_has_the_peer() {
    let d = temp("client");
    let out = Command::new(bin())
        .args(["new", &d.to_string_lossy(), "--template", "platformer"])
        .output()
        .expect("run floptle new");
    assert!(out.status.success(), "scaffold failed: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::write(d.join("scenes/mp.ron"), SCENE).expect("write the scene");
    let p = d.to_string_lossy().to_string();
    // A port of this process's own, so two test runs on one machine do not
    // collide on it.
    let port = 40000 + (std::process::id() % 20000) as u16;
    let port_s = port.to_string();

    let status = d.join("status.json");
    let status_s = status.to_string_lossy().to_string();
    let mut server = Command::new(bin())
        .args(["serve", &p, "--scene", "scenes/mp.ron", "--port", &port_s, "--status-file", &status_s])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the server");
    // Give it a moment to bind before a client knocks.
    std::thread::sleep(Duration::from_millis(1500));

    let began = Instant::now();
    let out = Command::new(bin())
        .args(["run", &p, "--scene", "mp", "--join", &format!("127.0.0.1:{port}"), "--seconds", "5", "--json"])
        .output()
        .expect("run the client");
    let took = began.elapsed();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The server's roster, as it wrote it at its fifth second — while a paced
    // client was still in its run, and after an unpaced one had long finished.
    let roster = std::fs::read_to_string(&status).unwrap_or_default();

    // The roster is read; the server has nothing more to say.
    let _ = server.kill();
    let _ = server.wait();
    let mut server_said = String::new();
    if let Some(mut e) = server.stderr.take() {
        let _ = e.read_to_string(&mut server_said);
    }
    let _ = std::fs::remove_dir_all(&d);

    let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("the client run put no JSON on stdout ({e}):\n{stdout}\n--- stderr ---\n{stderr}\n--- server ---\n{server_said}")
    });
    assert_eq!(doc["ok"], true, "the client run raised: {doc}\n--- server ---\n{server_said}");
    assert_eq!(doc["steps"], 300, "the run did not step its five seconds: {doc}");
    // Five simulated seconds took five wall seconds: the client kept the
    // server's time rather than racing it.
    assert!(
        took >= Duration::from_millis(4500),
        "300 steps of a joined run took {took:?} — the client ran ahead of real time"
    );
    // …and the server had the peer on its roster at its fifth second, three
    // and a half seconds into the client's run.
    let peers: serde_json::Value = serde_json::from_str(&roster).unwrap_or_else(|e| {
        panic!("the server wrote no status ({e}):\n{roster}\n--- server ---\n{server_said}")
    });
    assert_eq!(
        peers["peers"], 1,
        "the server's roster did not hold the client: {roster}\n--- server ---\n{server_said}"
    );
}
