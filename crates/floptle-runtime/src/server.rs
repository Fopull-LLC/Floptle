//! The **dedicated server** (`docs/multiplayer.md` §12, 2e) — a project's
//! authoritative simulation with no window, no GPU and nobody sitting at it.
//!
//! Until now every session was hosted by an editor or a player's game, which is
//! fine for friends-and-a-lobby-code and wrong for anything that has to stay up:
//! the world ends when the host closes the laptop, and the host is also a player
//! with an unfair zero-latency view of it. This runs the same simulation with
//! neither problem — the same `World`, the same `Sim`, the same `ScriptHost`,
//! the same `NetSession`, minus the rendering.
//!
//! ```text
//! floptle-runtime --server <project> [--scene scenes/x.ron]
//!                 [--port 7777 | --relay host:port] [--tick 60]
//!                 [--interest 150] [--budget 16384]
//! ```
//!
//! ## What it is not
//!
//! It hosts **`Authority` and `Predicted`** sessions — the MMO direction, which
//! is what a dedicated server is actually for. It does not host `Rollback`
//! matches, and that is a design position rather than a gap: a rollback session
//! has every peer simulating every tick, so its "host" is a referee and a relay,
//! and for a fighting game that is one of the players (or a rented host running
//! the game, not this). If a scene's nodes are `Rollback` this says so and
//! refuses, instead of running a session none of its clients can use.
//!
//! There is no interpolation, no audio, no VFX and no input here: nobody is
//! watching, and a server that spent time on any of it would be spending it on
//! nothing.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use floptle_core::transform::Transform;
use floptle_core::{Replicated, World};
use floptle_net::{NetSession, Transport};

/// Parsed `--server` arguments.
#[derive(Debug)]
pub struct ServerArgs {
    pub project: PathBuf,
    pub scene: Option<String>,
    pub port: Option<u16>,
    pub relay: Option<String>,
    pub tick_hz: f32,
    pub interest: Option<f64>,
    pub budget: Option<u32>,
}

impl ServerArgs {
    /// Parse `--server <project> [flags]`. Unknown flags are reported rather
    /// than ignored: a server started with a misspelt `--port` would come up
    /// listening somewhere nobody is looking.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let i = args.iter().position(|a| a == "--server").ok_or("no --server")?;
        let project = args
            .get(i + 1)
            .filter(|p| !p.starts_with("--"))
            .map(PathBuf::from)
            .ok_or("usage: floptle-runtime --server <project-dir> [--scene …] [--port N]")?;
        let mut out = Self {
            project,
            scene: None,
            port: None,
            relay: None,
            tick_hz: 60.0,
            interest: None,
            budget: None,
        };
        let rest = &args[i + 2..];
        let mut k = 0;
        while k < rest.len() {
            let val = rest.get(k + 1).cloned();
            let need = |v: Option<String>, what: &str| {
                v.ok_or_else(|| format!("{what} needs a value"))
            };
            match rest[k].as_str() {
                "--scene" => out.scene = Some(need(val, "--scene")?),
                "--port" => {
                    out.port = Some(
                        need(val, "--port")?.parse().map_err(|_| "--port must be a number")?,
                    )
                }
                "--relay" => out.relay = Some(need(val, "--relay")?),
                "--tick" => {
                    out.tick_hz = need(val, "--tick")?
                        .parse()
                        .map_err(|_| "--tick must be a number (Hz)")?
                }
                "--interest" => {
                    out.interest = Some(
                        need(val, "--interest")?
                            .parse()
                            .map_err(|_| "--interest must be a radius in metres")?,
                    )
                }
                "--budget" => {
                    out.budget = Some(
                        need(val, "--budget")?
                            .parse()
                            .map_err(|_| "--budget must be bytes per second")?,
                    )
                }
                other => return Err(format!("unknown flag {other}")),
            }
            k += 2;
        }
        if out.tick_hz <= 0.0 || out.tick_hz > 1000.0 {
            return Err("--tick must be between 1 and 1000 Hz".into());
        }
        Ok(out)
    }
}

/// Run until interrupted. Returns an exit code.
pub fn run(args: ServerArgs) -> i32 {
    let root = &args.project;
    let scene_path = match resolve_scene(root, args.scene.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  {e}");
            return 2;
        }
    };
    let doc = match floptle_scene::load(&scene_path) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("  cannot load {}: {e}", scene_path.display());
            return 2;
        }
    };
    // Refuse a rollback scene rather than serve a session nobody can join.
    if doc.nodes.iter().any(|n| n.net.as_ref().is_some_and(|r| r.rollback)) {
        eprintln!(
            "  {} has Rollback nodes. A rollback match is simulated by every peer, so it is \
             hosted by one of the players (or a host running the game), not by a dedicated \
             server. Nothing here could drive it.",
            scene_path.display()
        );
        return 2;
    }
    if !doc.nodes.iter().any(|n| n.net.is_some()) {
        eprintln!(
            "  {} has no Networked nodes — a session would replicate nothing. Add the \
             Networked component to what should be shared.",
            scene_path.display()
        );
        return 2;
    }

    let mut world = World::default();
    floptle_scene::spawn_into(&doc, &mut world);
    let map = floptle_input::load_map(root).ok().flatten().unwrap_or_default();
    let map_hash = map.hash();

    let transport: Box<dyn Transport> = match (&args.relay, args.port) {
        (Some(addr), _) => match floptle_net::RelayHost::host(addr) {
            Ok((t, code)) => {
                println!("  hosting via relay {addr} — LOBBY CODE {code}");
                Box::new(t)
            }
            Err(e) => {
                eprintln!("  relay {addr}: {e}");
                return 3;
            }
        },
        (None, Some(port)) => match floptle_net::QuicServer::bind(port) {
            Ok(t) => {
                println!("  listening on UDP {} (quic://<this-host>:{})", t.local_port(), t.local_port());
                Box::new(t)
            }
            Err(e) => {
                eprintln!("  cannot bind {port}: {e}");
                return 3;
            }
        },
        (None, None) => {
            eprintln!("  a dedicated server needs somewhere to listen: --port <n> or --relay <addr>");
            return 2;
        }
    };

    let mut session = NetSession::server(transport, map_hash);
    session.register_scene(&world);
    session.set_scene(&rel_scene(root, &scene_path));
    let step = 1.0 / args.tick_hz;
    session.set_tick_dt(step);
    if let Some(radius) = args.interest {
        let d = floptle_net::InterestConfig::default();
        session.set_interest(floptle_net::InterestConfig {
            enabled: true,
            radius,
            budget_bytes_per_sec: args.budget.unwrap_or(d.budget_bytes_per_sec),
            ..d
        });
        println!(
            "  interest management on — {radius:.0} m, {} KB/s per client",
            args.budget.unwrap_or(d.budget_bytes_per_sec) / 1024
        );
    }

    let mut sim = floptle_physics::Sim::build(
        &world,
        &[],
        build_gravity(&world),
        floptle_core::math::DVec3::ZERO,
    );
    let mut host = floptle_script::ScriptHost::new();
    // The same `vec3` backing the clients run. This is not merely tidiness: the
    // two modes carry different precision (f64 against f32), and a server
    // simulating in one while its clients simulate in the other is a rollback
    // divergence that presents as rubber-banding rather than as a settings
    // mismatch. See ADR-0028 Phase 3.
    {
        let cfg = floptle_scene::load_project(&root.join("project.ron"));
        let mode = match cfg.script_vec3_resolved() {
            floptle_scene::ScriptVec3Doc::Exact => floptle_script::Vec3Mode::Exact,
            floptle_scene::ScriptVec3Doc::Fast => floptle_script::Vec3Mode::Fast,
        };
        if let Err(e) = host.set_vec3_mode(mode) {
            eprintln!("  {e}");
        }
    }
    // A dedicated server IS a running session, so `http.*` is live here — and
    // this is the one place it's unambiguously the right tool: the AUTHORITY
    // talking to your web API is exactly how a client stops needing to.
    host.set_playing(true);
    host.set_input_map(map);
    host.set_layers(sim.layers().clone());
    let scripts = root.join("scripts");
    // One pass to build instances and fire `start`, as Play does.
    host.run(&mut world, &scripts, step, 0.0);
    drain_logs(&mut host);

    println!(
        "  serving {} — {} node(s), {} networked, {:.0} Hz tick. Ctrl-C to stop.",
        scene_path.display(),
        world.query::<Transform>().count(),
        world.query::<Replicated>().count(),
        args.tick_hz,
    );

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    install_stop_watcher(stop.clone());

    let period = Duration::from_secs_f32(step);
    let mut tick = 0u64;
    let mut next = Instant::now() + period;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        tick += 1;
        let time = tick as f32 * step;
        // Scripts, then physics, then replication — the editor's tick order.
        host.run_fixed(&mut world, step, time);
        sim.step_tick(step, None);
        // Alpha 1.0: the exact tick pose, not a render blend. Nobody is
        // watching this world, and replication must carry the state the
        // simulation is actually in.
        sim.writeback_interpolated(&mut world, 1.0);
        feed_session(&mut session, &mut host, &sim, &world);
        session.tick_server(&world, tick);
        drain_session(&mut session, &mut world, &mut host);
        drain_logs(&mut host);

        // A heartbeat, because a headless server that is silent and a headless
        // server that is wedged look identical from the outside.
        if tick.is_multiple_of(args.tick_hz.max(1.0) as u64 * 30) {
            println!(
                "  tick {tick} — {} peer(s) connected",
                session.peers().len()
            );
        }

        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
            next += period;
        } else {
            // Behind schedule: give up the lost time rather than sprint to
            // catch up, which would run the world faster than real time and
            // make every client's prediction wrong at once.
            next = now + period;
        }
    }
    println!("  server stopped after {tick} tick(s)");
    0
}

/// Mirror this tick's simulation state into the session for replication.
fn feed_session(
    session: &mut NetSession,
    host: &mut floptle_script::ScriptHost,
    sim: &floptle_physics::Sim,
    world: &World,
) {
    let bodies: floptle_net::BodyStates = sim
        .body_states()
        .map(|r| (r.entity, r.vel.to_array(), r.grounded))
        .collect();
    session.update_body_states(bodies);
    // Scripts report by entity INDEX; the session replicates by entity. One
    // lookup table per tick beats a linear scan per variable.
    let by_index: std::collections::HashMap<u32, floptle_core::Entity> =
        world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
    let synced: floptle_net::SyncedVars = host
        .collect_synced()
        .into_iter()
        .filter_map(|(eid, kind, vars)| by_index.get(&eid).map(|e| (*e, kind, vars)))
        .collect();
    session.update_synced(synced);
}

/// Move whatever the scripts said this tick onto the terminal.
///
/// A dedicated server has no Console panel, so this is the only place its
/// scripts can be heard — `print(...)`, `log(...)`, and every warning the
/// engine raises on their behalf. It is also what keeps the host's log buffer
/// from growing for the whole uptime of a server nobody ever asks.
///
/// On **stderr**, matching the editor: stdout carries the server's own
/// heartbeat, which an operator may well be parsing.
fn drain_logs(host: &mut floptle_script::ScriptHost) {
    for l in host.drain_logs() {
        eprintln!("[lua] {}", l.msg);
    }
}

/// Apply whatever arrived: RPCs into scripts, and peer lifecycle into events.
fn drain_session(
    session: &mut NetSession,
    world: &mut World,
    host: &mut floptle_script::ScriptHost,
) {
    // Moderation decisions, in words. A dedicated server has nobody watching a
    // Console, so this is the only place a refused join or a kick can be seen
    // at all — and an action nobody can audit is not a moderation tool.
    for line in session.take_join_log() {
        println!("  {line}");
    }
    // Voice is FORWARDED here, not heard: this process has no output device and
    // nobody sitting at it. The session already relayed each frame to whoever
    // the game said may hear it (floptle/0180); draining is what stops the
    // host's own copy accumulating on a machine that never listens.
    let _ = session.take_voice();
    for ev in session.take_events() {
        match ev {
            floptle_net::NetEvent::PeerJoined(p) => {
                println!("  peer {p} joined");
                if let Some(name) = claim_free_slot(session, world, p) {
                    println!("    peer {p} drives authored player slot \"{name}\"");
                }
                host.fire_net_event(world, "playerJoined", Some(p), None);
            }
            floptle_net::NetEvent::PeerLeft(p, why) => {
                match &why {
                    Some(reason) => println!("  peer {p} removed: {reason}"),
                    None => println!("  peer {p} left"),
                }
                release_slots(session, world, p);
                host.fire_net_event(world, "playerLeft", Some(p), why.as_deref());
            }
            _ => {}
        }
    }
    for rpc in session.take_rpcs() {
        host.dispatch_rpc(world, &rpc.name, &rpc.args, rpc.sender);
    }
}

/// Hand a joining peer the first authored `Predicted` slot nobody owns.
///
/// **Why the server does this and a host does not.** In an editor- or
/// player-hosted session the convention is "Predicted node #1 = host, #2+ =
/// joiners" (`floptle-editor/src/net.rs`), because slot #1's driver is sitting
/// at the keyboard. A dedicated server has no local player: leaving slot #1
/// with no owner leaves an avatar in the world that nobody controls and no
/// client predicts — the first joiner spectates their own body. So here the
/// slots are handed out from #1, in node order, as peers arrive.
///
/// It only ever touches a slot that is **unowned**, so a game that assigns its
/// own (`net.setOwner`, or `net.spawn{ owner = peer }`) keeps every decision it
/// makes; and a peer that already owns something is left alone, so a returning
/// player given their old slot back does not also collect a second one.
fn claim_free_slot(
    session: &mut NetSession,
    world: &mut World,
    peer: floptle_net::PeerId,
) -> Option<String> {
    let slots: Vec<floptle_core::Entity> = world
        .query::<Transform>()
        .map(|(e, _)| e)
        .filter(|e| {
            world
                .get::<Replicated>(*e)
                .is_some_and(|r| r.mode == floptle_core::ReplicationMode::Predicted)
        })
        .collect();
    if slots.iter().any(|e| world.get::<Replicated>(*e).and_then(|r| r.owner) == Some(peer)) {
        return None;
    }
    let free = slots
        .iter()
        .find(|e| world.get::<Replicated>(**e).is_some_and(|r| r.owner.is_none()))
        .copied()?;
    let name = world.get::<floptle_core::Name>(free).map(|n| n.0.clone()).unwrap_or_default();
    session.set_owner(world, free, Some(peer)).then_some(name)
}

/// Clean up after a departed peer: its runtime spawns go, its authored slots
/// come back.
///
/// The two halves are different on purpose. A rig the game spawned FOR a player
/// belongs to that player and leaves with them, subtree and all — the same rule
/// a hosted session has always followed. An authored slot belongs to the scene:
/// it stays in the world and becomes free, so the next joiner can have it
/// instead of the lobby shrinking by one every time somebody's wifi drops.
fn release_slots(session: &mut NetSession, world: &mut World, peer: floptle_net::PeerId) {
    // Runtime spawns first, so the sweep below doesn't try to release an
    // entity that is about to stop existing.
    for e in session.owned_runtime_spawns(peer) {
        session.despawn(world, e);
    }
    let mine: Vec<floptle_core::Entity> = world
        .query::<Replicated>()
        .filter(|(_, r)| r.owner == Some(peer))
        .map(|(e, _)| e)
        .collect();
    for e in mine {
        session.set_owner(world, e, None);
    }
}

/// Let an INTERACTIVE operator stop the server with a keypress.
///
/// Only when stdin is a terminal. A server under systemd, docker or a CI job
/// has stdin on `/dev/null`, which reads EOF immediately — watching it there
/// would make the process exit the instant it started, and the symptom ("the
/// server won't stay up") looks nothing like the cause. With no terminal the
/// default signal disposition does the job: SIGINT and SIGTERM end the process,
/// which is exactly how a service is meant to be stopped.
fn install_stop_watcher(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        return;
    }
    println!("  (press enter to stop)");
    std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::stdin().lock(), &mut buf);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    });
}

fn build_gravity(_world: &World) -> floptle_physics::GravityField {
    floptle_physics::GravityField::uniform(floptle_core::math::Vec3::new(0.0, -9.81, 0.0))
}

fn rel_scene(root: &Path, scene: &Path) -> String {
    scene
        .strip_prefix(root)
        .unwrap_or(scene)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Which scene to serve: the flag, else the project's entry scene, else the
/// only scene there is.
fn resolve_scene(root: &Path, flag: Option<&str>) -> Result<PathBuf, String> {
    if let Some(s) = flag {
        let p = root.join(s);
        return p.exists().then_some(p).ok_or_else(|| format!("no scene at {s}"));
    }
    if let Ok(text) = std::fs::read_to_string(root.join("project.ron"))
        && let Some(entry) = entry_scene(&text)
    {
        let p = root.join(&entry);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("project.ron names {entry}, which isn't there"));
    }
    let scenes: Vec<PathBuf> = std::fs::read_dir(root.join("scenes"))
        .map_err(|_| "no scenes/ directory and no entry_scene in project.ron".to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "ron"))
        .collect();
    match scenes.len() {
        0 => Err("no scenes to serve".into()),
        1 => Ok(scenes[0].clone()),
        _ => Err("more than one scene — say which with --scene <path>".into()),
    }
}

/// `entry_scene: Some("scenes/x.ron")` out of project.ron, without parsing the
/// whole document (which would drag in every component type a project can hold).
fn entry_scene(text: &str) -> Option<String> {
    let i = text.find("entry_scene:")?;
    let rest = &text[i..];
    let a = rest.find('"')?;
    let b = rest[a + 1..].find('"')?;
    Some(rest[a + 1..a + 1 + b].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Two authored player slots and a session nobody has joined yet — the
    /// shape a dedicated server's scene actually has.
    fn slots() -> (NetSession, World, Vec<floptle_core::Entity>) {
        let hub = floptle_net::MemoryHub::new();
        let mut session = NetSession::server(Box::new(hub.server_endpoint()), 0);
        let mut world = World::default();
        let ents: Vec<_> = ["Survivor1", "Survivor2"]
            .iter()
            .map(|name| {
                let e = world.spawn();
                world.insert(e, Transform::IDENTITY);
                world.insert(e, floptle_core::Name((*name).into()));
                world.insert(
                    e,
                    Replicated {
                        mode: floptle_core::ReplicationMode::Predicted,
                        ..Default::default()
                    },
                );
                e
            })
            .collect();
        session.register_scene(&world);
        (session, world, ents)
    }

    fn owner_of(world: &World, e: floptle_core::Entity) -> Option<floptle_net::PeerId> {
        world.get::<Replicated>(e).and_then(|r| r.owner)
    }

    /// floptle/0181 — **slot #1 is not reserved for a host that does not exist.**
    ///
    /// A hosted session gives slot #1 to the host, because somebody is sitting
    /// at that keyboard. A dedicated server has nobody: reserving it leaves an
    /// avatar in the world that no client predicts and no input drives, and the
    /// first player to join spectates their own body.
    #[test]
    fn the_first_joiner_gets_slot_one_on_a_dedicated_server() {
        let (mut session, mut world, ents) = slots();
        assert_eq!(claim_free_slot(&mut session, &mut world, 1).as_deref(), Some("Survivor1"));
        assert_eq!(owner_of(&world, ents[0]), Some(1));
        assert_eq!(claim_free_slot(&mut session, &mut world, 2).as_deref(), Some("Survivor2"));
        assert_eq!(owner_of(&world, ents[1]), Some(2));
        // The lobby is full: a third joiner takes nothing rather than stealing.
        assert_eq!(claim_free_slot(&mut session, &mut world, 3), None);
        assert_eq!(owner_of(&world, ents[0]), Some(1), "…and nobody is displaced");
    }

    /// A peer that already owns something is left alone, so a game that assigns
    /// its own slots (`net.setOwner` on reconnect, say) does not also collect a
    /// spare one behind its back.
    #[test]
    fn a_peer_that_already_owns_a_node_is_not_handed_another() {
        let (mut session, mut world, ents) = slots();
        session.set_owner(&mut world, ents[1], Some(7));
        assert_eq!(claim_free_slot(&mut session, &mut world, 7), None);
        assert_eq!(owner_of(&world, ents[0]), None, "slot #1 stays free for a real joiner");
    }

    /// A slot comes back when its player drops, so a lobby does not shrink by
    /// one every time somebody's wifi does.
    #[test]
    fn a_departed_peers_slot_is_freed_for_the_next_joiner() {
        let (mut session, mut world, ents) = slots();
        claim_free_slot(&mut session, &mut world, 1);
        release_slots(&mut session, &mut world, 1);
        assert_eq!(owner_of(&world, ents[0]), None);
        assert_eq!(claim_free_slot(&mut session, &mut world, 2).as_deref(), Some("Survivor1"));
    }

    #[test]
    fn the_project_directory_is_required() {
        assert!(ServerArgs::parse(&args(["floptle", "--server"].as_ref())).is_err());
        assert!(ServerArgs::parse(&args(["floptle", "--server", "--port", "1"].as_ref())).is_err());
    }

    #[test]
    fn flags_parse_and_default_sensibly() {
        let a = ServerArgs::parse(&args(
            ["x", "--server", "/p", "--port", "7777", "--interest", "120", "--tick", "30"]
                .as_ref(),
        ))
        .expect("parses");
        assert_eq!(a.project, PathBuf::from("/p"));
        assert_eq!(a.port, Some(7777));
        assert_eq!(a.interest, Some(120.0));
        assert_eq!(a.tick_hz, 30.0);
        assert_eq!(a.budget, None, "budget is only meaningful with interest, and defaults");
    }

    /// A server started with a misspelt flag would come up listening somewhere
    /// nobody is looking, which is a worse failure than not starting.
    #[test]
    fn an_unknown_flag_is_refused_rather_than_ignored() {
        let e = ServerArgs::parse(&args(["x", "--server", "/p", "--prot", "7777"].as_ref()));
        assert!(e.unwrap_err().contains("--prot"));
    }

    #[test]
    fn a_nonsense_tick_rate_is_refused() {
        assert!(ServerArgs::parse(&args(["x", "--server", "/p", "--tick", "0"].as_ref())).is_err());
        assert!(
            ServerArgs::parse(&args(["x", "--server", "/p", "--tick", "9999"].as_ref())).is_err()
        );
    }

    #[test]
    fn the_entry_scene_is_read_out_of_project_ron() {
        let text = "(\n  retro: false,\n  entry_scene: Some(\"scenes/planetoid.ron\"),\n)";
        assert_eq!(entry_scene(text).as_deref(), Some("scenes/planetoid.ron"));
        assert_eq!(entry_scene("(retro: false)"), None);
    }
}
