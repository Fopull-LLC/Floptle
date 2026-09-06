//! The **dedicated server** (`docs/multiplayer.md` §12, 2e) — a project's
//! authoritative simulation with no window, no GPU and nobody sitting at it.
//!
//! Until now every session was hosted by an editor or a player's game, which is
//! fine for friends-and-a-lobby-code and wrong for anything that has to stay up:
//! the world ends when the host closes the laptop, and the host is also a player
//! with an unfair zero-latency view of it. This runs the same simulation with
//! neither problem.
//!
//! ```text
//! floptle serve <project> [--scene scenes/x.ron]
//!                [--port 7777 | --relay host:port] [--tick 60]
//!                [--interest 150] [--budget 16384]
//! ```
//!
//! ## Why this is a few hundred lines and not a few thousand
//!
//! **There is one authoritative tick in this engine and it is the editor's.**
//! `Editor::play_step` is the gameplay tick — scripts, animation, physics,
//! terrain edits, collision events — and `Editor::net_tick` is the host half of
//! it: every one of the ten [`floptle_script::NetCmd`] variants a server script
//! can issue, the lag-compensation history `net.rewind` re-poses combat
//! against, interest management and its line-of-sight occluder, the join
//! policy, `net.kick`, `net.setRelevant`, voice forwarding and scene switching.
//! None of that is display code, and none of it needs a GPU: `floptle run`
//! already drives exactly this loop headlessly.
//!
//! So a dedicated server is not a second implementation of a server. It is the
//! editor's engine half with **no window and no local player**, hosting.
//!
//! The version this replaced re-derived a subset once and never caught up. It
//! drained **no** `NetCmd` at all — so `net.spawn`, `net.despawn`,
//! `net.setOwner`, `net.kick`, `net.setRelevant` and a server-originated
//! `net.send` were silent no-ops on `floptle serve` — had no rewind history,
//! passed `&[]` terrain volumes, hard-coded uniform gravity, never loaded a
//! project's packages, and never stepped animation or nav. Every one of those
//! is a thing the tick above has done for releases; the subset simply did not
//! call it. A subset cannot be kept in step by discipline, which is why the fix
//! is to delete it rather than to extend it.
//!
//! ## The one thing a dedicated server does that a host does not
//!
//! **Nobody is sitting at it, so slot #1 is not spoken for.** In an editor- or
//! player-hosted session the convention is "Predicted node #1 = the host, #2+ =
//! joiners", because slot #1's driver is at the keyboard. Here there is no
//! keyboard: leaving slot #1 reserved would put an avatar in the world that
//! nobody controls and no client predicts, and the first player to join would
//! spectate their own body. So [`Editor::dedicated`] leaves every authored slot
//! **unowned**, hands them out from #1 in node order as peers arrive
//! ([`claim_free_slot`]), and takes them back when a peer drops
//! ([`release_slots`]) — while a slot nobody owns stays out of the script
//! passes entirely, because no player is driving it.
//!
//! ## What it is not
//!
//! It hosts **`Authority` and `Predicted`** sessions — the MMO direction, which
//! is what a dedicated server is actually for. It does not host `Rollback`
//! matches, and that is a design position rather than a gap: a rollback session
//! has every peer simulating every tick, so its "host" is a referee and a relay,
//! and for a fighting game that is one of the players. If a scene's nodes are
//! `Rollback` this says so and refuses, instead of running a session none of its
//! clients can use.
//!
//! There is no rendering, no audio and no input here: nobody is watching, and a
//! server that spent time on any of it would be spending it on nothing.

use std::path::{Path, PathBuf};

use floptle_core::time::Instant;
use floptle_core::transform::Transform;
use floptle_core::{Replicated, World};
use floptle_net::NetSession;

use crate::Editor;

/// Parsed dedicated-server arguments.
///
/// **One parser, three entry points.** `floptle serve`, `floptle-runtime
/// --server` and (from workstream B) the `floptle-server` binary all reach the
/// server through here, so a flag cannot mean one thing in the editor's CLI and
/// another in the binary an operator actually deploys.
#[derive(Debug)]
pub struct ServerArgs {
    pub project: PathBuf,
    pub scene: Option<String>,
    pub port: Option<u16>,
    pub relay: Option<String>,
    pub tick_hz: f32,
    pub interest: Option<f64>,
    pub budget: Option<u32>,
    /// Refuse a join past this many concurrent peers. `None` = no ceiling of
    /// the server's own; a managed relay still applies the plan's.
    pub max_players: Option<u32>,
    /// Write a small JSON status document here every few seconds, for whatever
    /// is watching the box.
    pub status_file: Option<PathBuf>,
    /// The Floptle Cloud game key this server belongs to. **Recorded and
    /// reported, not checked** — a dedicated server is reached directly, so
    /// there is nothing here for a key to authorize. It is in the status file
    /// so an operator can tell which game a process belongs to.
    pub game_key: Option<String>,
}

impl ServerArgs {
    /// Parse `--server <project> [flags]`. Unknown flags are reported rather
    /// than ignored: a server started with a misspelt `--port` would come up
    /// listening somewhere nobody is looking.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let i = args.iter().position(|a| a == "--server").ok_or("no --server")?;
        Self::parse_argv(&args[i + 1..])
    }

    /// Parse the **`floptle-server` binary's own** argv: the project is a bare
    /// positional (or `--build <dir>`, which is the same thing said about an
    /// exported folder), and there is no `--server` marker because the binary
    /// is the marker.
    ///
    /// One parser, three entry points — `floptle serve`, `floptle-runtime
    /// --server` and this — so a flag cannot mean one thing in the editor's
    /// command line and another in the binary an operator actually deploys.
    pub fn parse_argv(argv: &[String]) -> Result<Self, String> {
        let mut out = Self {
            project: PathBuf::new(),
            scene: None,
            port: None,
            relay: None,
            tick_hz: 60.0,
            interest: None,
            budget: None,
            max_players: None,
            status_file: None,
            game_key: None,
        };
        // A leading positional is the project directory. Everything after is
        // flags, and `--build` can name it instead.
        let rest = match argv.first() {
            Some(first) if !first.starts_with("--") => {
                out.project = PathBuf::from(first);
                &argv[1..]
            }
            _ => argv,
        };
        out.parse_flags(rest)?;
        if out.project.as_os_str().is_empty() {
            return Err(
                "a dedicated server needs a project: floptle-server <project-dir> \
                 (or --build <exported-server-folder>)"
                    .into(),
            );
        }
        Ok(out)
    }

    /// The flag half, shared with any caller that already knows the project
    /// directory (the `serve` verb takes it as a positional argument).
    pub fn parse_flags(&mut self, rest: &[String]) -> Result<(), String> {
        let mut k = 0;
        while k < rest.len() {
            let val = rest.get(k + 1).cloned();
            let need = |v: Option<String>, what: &str| v.ok_or_else(|| format!("{what} needs a value"));
            match rest[k].as_str() {
                "--scene" => self.scene = Some(need(val, "--scene")?),
                "--port" => {
                    self.port =
                        Some(need(val, "--port")?.parse().map_err(|_| "--port must be a number")?)
                }
                "--relay" => self.relay = Some(need(val, "--relay")?),
                "--tick" => {
                    self.tick_hz = need(val, "--tick")?
                        .parse()
                        .map_err(|_| "--tick must be a number (Hz)")?
                }
                "--interest" => {
                    self.interest = Some(
                        need(val, "--interest")?
                            .parse()
                            .map_err(|_| "--interest must be a radius in metres")?,
                    )
                }
                "--budget" => {
                    self.budget = Some(
                        need(val, "--budget")?
                            .parse()
                            .map_err(|_| "--budget must be bytes per second")?,
                    )
                }
                "--build" => self.project = PathBuf::from(need(val, "--build")?),
                "--max-players" => {
                    self.max_players = Some(
                        need(val, "--max-players")?
                            .parse()
                            .map_err(|_| "--max-players must be a number")?,
                    )
                }
                "--status-file" => self.status_file = Some(PathBuf::from(need(val, "--status-file")?)),
                "--game-key" => self.game_key = Some(need(val, "--game-key")?),
                other => return Err(format!("unknown flag {other}")),
            }
            k += 2;
        }
        if self.tick_hz <= 0.0 || self.tick_hz > 1000.0 {
            return Err("--tick must be between 1 and 1000 Hz".into());
        }
        Ok(())
    }
}

/// Run until interrupted. Returns an exit code.
///
/// **Off wasm32**: a browser tab can open a connection but never accept one, so
/// there is nothing here for it to listen with — and a web export is a client
/// anyway. Same gate the transport itself carries in `floptle-net`.
#[cfg(not(target_arch = "wasm32"))]
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
    if let Err(e) = check_servable(&doc, &scene_path) {
        eprintln!("  {e}");
        return 2;
    }
    if args.relay.is_none() && args.port.is_none() {
        eprintln!("  a dedicated server needs somewhere to listen: --port <n> or --relay <addr>");
        return 2;
    }

    let mut ed = open(root, &scene_path, args.tick_hz);
    // Everything the open said — a scene with bad wiring, a package that failed
    // to load — belongs on the terminal before the first tick, and Play is
    // about to clear the Console.
    ed.adopt_script_logs(false);
    drain_console(&mut ed);
    ed.toggle_play();
    if !ed.playing {
        eprintln!("  the project did not enter play mode");
        return 2;
    }

    match (&args.relay, args.port) {
        (Some(addr), _) => ed.net_host_relay(addr),
        (None, Some(port)) => ed.net_host_quic(port),
        (None, None) => unreachable!("checked above"),
    }
    ed.adopt_script_logs(false);
    drain_console(&mut ed);
    if ed.net_server.is_none() {
        // The Console already carries the reason (the bind or the relay said
        // so); this is the exit code an operator's unit file reads.
        return 3;
    }
    if let Some(code) = &ed.net_lobby_code {
        println!("  LOBBY CODE {code}");
    }
    apply_server_opts(&mut ed, &args);
    if let Some(max) = args.max_players {
        println!("  at most {max} player(s); the next arrival is refused, nobody is dropped");
    }
    if let Some(key) = &args.game_key {
        // Recorded and reported, not checked: a dedicated server is reached
        // directly, so there is nothing here for a key to authorize. It says
        // which game this process belongs to.
        println!("  game key {} (recorded, not checked — see docs/multiplayer.md §6c)", redact(key));
    }
    if let Some(radius) = args.interest {
        println!(
            "  interest management on — {radius:.0} m, {} KB/s per client",
            args.budget.unwrap_or(floptle_net::InterestConfig::default().budget_bytes_per_sec)
                / 1024
        );
    }

    let step = 1.0 / args.tick_hz;
    println!(
        "  serving {} — {} node(s), {} networked, {:.0} Hz tick. Ctrl-C to stop.",
        scene_path.display(),
        ed.world.query::<Transform>().count(),
        ed.world.query::<Replicated>().count(),
        args.tick_hz,
    );

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    install_stop_watcher(stop.clone());

    let period = std::time::Duration::from_secs_f32(step);
    let mut ticks = 0u64;
    let started = Instant::now();
    let mut last_status = Instant::now() - STATUS_EVERY;
    let mut next = Instant::now() + period;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
        ticks += 1;
        // `game_focused: false` — there is no keyboard here, so the only input
        // reaching a script is a client's replayed input, per owner.
        ed.play_step(step, false);
        // **The tick does not print anything.** Every host drains the script
        // host itself — the windowed frame, `floptle run`, and this. A server
        // that skipped it would run a game whose scripts were raising every
        // tick and report a clean, silent uptime.
        ed.adopt_script_logs(false);
        drain_console(&mut ed);

        // A heartbeat, because a headless server that is silent and a headless
        // server that is wedged look identical from the outside.
        if ticks.is_multiple_of(args.tick_hz.max(1.0) as u64 * 30) {
            let peers = ed.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
            println!("  tick {ticks} — {peers} peer(s) connected");
        }

        if let Some(path) = &args.status_file
            && last_status.elapsed() >= STATUS_EVERY
        {
            last_status = Instant::now();
            write_status(path, &args, &ed, ticks, started);
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
    println!("  server stopped after {ticks} tick(s)");
    0
}

/// How often `--status-file` is rewritten.
#[cfg(not(target_arch = "wasm32"))]
const STATUS_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// A game key with its middle taken out, for a log line.
///
/// The key is public by design — it ships inside every build — so this is not
/// secrecy, it is not filling an operator's terminal with 40 characters they
/// cannot read anyway. The prefix is the useful part: it says which game.
fn redact(key: &str) -> String {
    match key.len() {
        0..=12 => key.to_string(),
        n => format!("{}…{}", &key[..12], &key[n - 4..]),
    }
}

/// Write the status document `--status-file` asks for.
///
/// **Written to a temp file and renamed**, so whatever is watching it never
/// reads half a document. A monitor that occasionally parses a truncated JSON
/// file reports an outage that is not happening, which is worse than no
/// monitoring at all.
///
/// Best effort throughout: a status file that cannot be written is a server
/// that is harder to watch, never a server that stops.
#[cfg(not(target_arch = "wasm32"))]
fn write_status(path: &Path, args: &ServerArgs, ed: &Editor, ticks: u64, started: Instant) {
    let peers = ed.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
    let doc = format!(
        "{{\n  \"peers\": {peers},\n  \"max_players\": {},\n  \"uptime_s\": {},\n  \
         \"ticks\": {ticks},\n  \"tick_hz\": {},\n  \"scene\": {:?},\n  \
         \"project\": {:?},\n  \"game_key\": {},\n  \"lobby_code\": {}\n}}\n",
        args.max_players.map(|m| m.to_string()).unwrap_or_else(|| "null".into()),
        started.elapsed().as_secs(),
        args.tick_hz,
        ed.scene_rel_or_default(),
        args.project.to_string_lossy(),
        args.game_key.as_deref().map(|k| format!("{k:?}")).unwrap_or_else(|| "null".into()),
        ed.net_lobby_code.as_deref().map(|c| format!("{c:?}")).unwrap_or_else(|| "null".into()),
    );
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, doc).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The headless engine this server is: a project, a scene, its packages, its
/// terrain and no window.
///
/// `dedicated` is set **before** the project opens, because it is read at host
/// time (who owns an authored slot) and by the script filters, and a server
/// that adopted it late would have already handed slot #1 to a player who does
/// not exist.
pub(crate) fn open(root: &Path, scene_path: &Path, tick_hz: f32) -> Editor {
    let mut ed = Editor {
        // Nothing draws, so gizmos and overlays would only cost work.
        show_gizmos: false,
        // The Console is the log, and this drains it per tick — mirroring as
        // well would print every warning twice and drop every `print`.
        console: crate::console::ConsoleState { mirror_to_stderr: false, ..Default::default() },
        dedicated: true,
        ..Default::default()
    };
    ed.game_tick.step = 1.0 / tick_hz;
    ed.open_project(root.to_path_buf());
    ed.open_scene_file(&scene_path.to_string_lossy());
    ed
}

/// Apply the command line's session options to the live session.
///
/// **A named function rather than four lines inside [`run`], because inside
/// [`run`] they are reachable only by starting a real server on a real port.**
/// A flag the command line accepts and the session never receives is the
/// quietest bug this binary can have — nothing fails, the ceiling simply is not
/// there — and the only way a guard can hold that is if the wiring is something
/// a guard can call.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn apply_server_opts(ed: &mut Editor, args: &ServerArgs) {
    let Some(s) = ed.net_server.as_mut() else { return };
    // The operator's ceiling, if they set one. Refused at the door — a limit
    // that removed somebody already playing would read as a crash to whoever
    // got unlucky.
    s.set_max_peers(args.max_players);
    if let Some(radius) = args.interest {
        let d = floptle_net::InterestConfig::default();
        s.set_interest(floptle_net::InterestConfig {
            enabled: true,
            radius,
            budget_bytes_per_sec: args.budget.unwrap_or(d.budget_bytes_per_sec),
            ..d
        });
    }
}

/// Move whatever the tick said onto the terminal.
///
/// A dedicated server has no Console panel, so this is the only place its
/// scripts can be heard — `print(...)`, `log(...)`, and every warning the
/// engine raises on their behalf. It is also what keeps the buffer from growing
/// for the whole uptime of a server nobody ever asks.
///
/// On **stderr**, matching the editor: stdout carries the server's own
/// heartbeat, which an operator may well be parsing.
fn drain_console(ed: &mut Editor) {
    for e in ed.console.entries.drain(..) {
        // A Debug line with a source is a script's `print`/`log`; one without is
        // the engine talking. Tagging both "print" reads as though the engine's
        // own startup notes came out of the game's Lua, which is a confusing
        // thing for an operator to be reading a stack trace next to.
        let tag = match (e.level, e.source.is_some()) {
            (floptle_script::LogLevel::Error, _) => "error: ",
            (floptle_script::LogLevel::Warn, _) => "warning: ",
            (floptle_script::LogLevel::Debug, true) => "print: ",
            (floptle_script::LogLevel::Debug, false) => "",
        };
        let times = if e.count > 1 { format!(" (x{})", e.count) } else { String::new() };
        match &e.source {
            Some((file, line)) => eprintln!("  {tag}{file}:{line}: {}{times}", e.msg),
            None => eprintln!("  {tag}{}{times}", e.msg),
        }
    }
}

/// Refuse a scene this server could not usefully host, and say which of the two
/// reasons it is.
fn check_servable(doc: &floptle_scene::SceneDoc, scene_path: &Path) -> Result<(), String> {
    if doc.nodes.iter().any(|n| n.net.as_ref().is_some_and(|r| r.rollback)) {
        return Err(format!(
            "{} has Rollback nodes. A rollback match is simulated by every peer, so it is \
             hosted by one of the players (or a host running the game), not by a dedicated \
             server. Nothing here could drive it.",
            scene_path.display()
        ));
    }
    if !doc.nodes.iter().any(|n| n.net.is_some()) {
        return Err(format!(
            "{} has no Networked nodes — a session would replicate nothing. Add the \
             Networked component to what should be shared.",
            scene_path.display()
        ));
    }
    Ok(())
}

/// Hand a joining peer the first authored `Predicted` slot nobody owns.
///
/// It only ever touches a slot that is **unowned**, so a game that assigns its
/// own (`net.setOwner`, or `net.spawn{ owner = peer }`) keeps every decision it
/// makes; and a peer that already owns something is left alone, so a returning
/// player given their old slot back does not also collect a second one.
pub(crate) fn claim_free_slot(
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

/// Clean up after a departed peer: its authored slots come back.
///
/// Its runtime spawns are **not** this function's business — the host tick
/// already despawns every runtime spawn a leaving peer owned, everywhere, and
/// doing it twice would try to release an entity that has stopped existing. An
/// authored slot is the other half and belongs to the scene: it stays in the
/// world and becomes free, so the next joiner can have it instead of the lobby
/// shrinking by one every time somebody's wifi drops.
pub(crate) fn release_slots(
    session: &mut NetSession,
    world: &mut World,
    peer: floptle_net::PeerId,
) {
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
#[cfg(not(target_arch = "wasm32"))]
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

/// Which scene to serve: the flag, else the project's entry scene, else the
/// only scene there is.
fn resolve_scene(root: &Path, flag: Option<&str>) -> Result<PathBuf, String> {
    if let Some(s) = flag {
        let p = root.join(s);
        return floptle_vfs::exists(&p).then_some(p).ok_or_else(|| format!("no scene at {s}"));
    }
    if let Ok(text) = floptle_vfs::read_to_string(root.join("project.ron"))
        && let Some(entry) = entry_scene(&text)
    {
        let p = root.join(&entry);
        if floptle_vfs::exists(&p) {
            return Ok(p);
        }
        return Err(format!("project.ron names {entry}, which isn't there"));
    }
    let scenes: Vec<PathBuf> = floptle_vfs::read_dir(root.join("scenes"))
        .map_err(|_| "no scenes/ directory and no entry_scene in project.ron".to_string())?
        .into_iter()
        .map(|e| e.path().to_path_buf())
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

    /// The binary's own argv: the project is a bare positional, and there is
    /// no `--server` marker because the binary is the marker.
    #[test]
    fn the_server_binary_takes_its_project_as_a_positional() {
        let a = ServerArgs::parse_argv(&args(["/p", "--port", "7777"].as_ref())).expect("parses");
        assert_eq!(a.project, PathBuf::from("/p"));
        assert_eq!(a.port, Some(7777));
    }

    /// `--build` names the same thing about an exported folder, so a deploy
    /// script does not have to know which word this invocation wants.
    #[test]
    fn a_build_folder_names_the_project_too() {
        let a = ServerArgs::parse_argv(&args(["--build", "/srv/game", "--port", "1"].as_ref()))
            .expect("parses");
        assert_eq!(a.project, PathBuf::from("/srv/game"));
    }

    /// **No project at all is refused, and the message names both spellings.**
    /// A server that came up on an empty path would fail later, somewhere less
    /// obvious.
    #[test]
    fn a_server_with_no_project_says_so_and_names_both_flags() {
        let e = ServerArgs::parse_argv(&args(["--port", "7777"].as_ref())).unwrap_err();
        assert!(e.contains("--build"), "{e}");
        assert!(e.contains("<project-dir>"), "{e}");
    }

    /// The operator flags parse, and — this is the point — they are carried
    /// rather than accepted and dropped. A flag the command line takes and the
    /// server ignores is worse than one it refuses.
    #[test]
    fn the_operator_flags_are_carried() {
        let a = ServerArgs::parse_argv(&args(
            [
                "/p", "--port", "1", "--max-players", "8", "--status-file", "/run/s.json",
                "--game-key", "fk_live_ABC",
            ]
            .as_ref(),
        ))
        .expect("parses");
        assert_eq!(a.max_players, Some(8));
        assert_eq!(a.status_file, Some(PathBuf::from("/run/s.json")));
        assert_eq!(a.game_key.as_deref(), Some("fk_live_ABC"));
    }

    /// Both entry points reach the same parser, so a flag cannot mean one thing
    /// in the editor's command line and another in the deployed binary.
    #[test]
    fn the_server_flag_and_the_binary_agree() {
        let via_flag =
            ServerArgs::parse(&args(["x", "--server", "/p", "--tick", "30"].as_ref())).unwrap();
        let via_argv = ServerArgs::parse_argv(&args(["/p", "--tick", "30"].as_ref())).unwrap();
        assert_eq!(via_flag.project, via_argv.project);
        assert_eq!(via_flag.tick_hz, via_argv.tick_hz);
    }

    /// A game key is public and ships in every build, so this is legibility,
    /// not secrecy — but the prefix has to survive, because it is the half that
    /// says which game.
    #[test]
    fn a_redacted_key_keeps_the_part_that_identifies_the_game() {
        let r = redact("fk_live_ABCDEFGHJKLMNPQRSTUVWX2345");
        assert!(r.starts_with("fk_live_ABCD"), "{r}");
        assert!(r.len() < 24, "it is shortened: {r}");
        assert_eq!(redact("short"), "short", "nothing to take out of a short one");
    }

    #[test]
    fn the_entry_scene_is_read_out_of_project_ron() {
        let text = "(\n  retro: false,\n  entry_scene: Some(\"scenes/planetoid.ron\"),\n)";
        assert_eq!(entry_scene(text).as_deref(), Some("scenes/planetoid.ron"));
        assert_eq!(entry_scene("(retro: false)"), None);
    }
}

/// The dedicated server, end to end, over the in-process memory hub.
///
/// **Every one of these was watched failing against the server this replaced**
/// — the one that re-derived a subset of the tick and drained no `NetCmd`. They
/// are not written against `ServerWorld`-shaped internals on purpose: what a
/// server does is only observable on the CLIENT's world, so that is where they
/// assert.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod server_tests {
    use super::*;
    use floptle_core::{Entity, Name, Parent};
    use floptle_net::MemoryHub;

    /// The gameplay tick these tests run at. Real time never enters into it —
    /// `play_step(STEP)` with `game_tick.step == STEP` advances exactly one.
    const STEP: f32 = 1.0 / 60.0;

    /// A client's world takes **~4× longer than you think** to show what the
    /// server did: the snapshot has to arrive, and then interpolation has to
    /// walk the node to it (`interp_delay` is 6 ticks by itself). Twelve ticks
    /// reads as a false pass — see the rc4 note in `.internal/docs/HANDOFF.md`.
    const SETTLE: u32 = 120;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "floptle-dedicated-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scenes")).unwrap();
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join("prefabs")).unwrap();
        dir
    }

    fn write(root: &Path, rel: &str, text: &str) {
        std::fs::write(root.join(rel), text).unwrap();
    }

    /// The server under test, plus the hub its clients arrive on.
    struct Server {
        ed: Editor,
        hub: MemoryHub,
        tick: u64,
        root: PathBuf,
    }

    /// A client of it: a real `NetSession` and its own world, exactly what a
    /// second machine would have.
    struct Client {
        session: NetSession,
        world: World,
    }

    impl Client {
        /// Follow the server's announced scene, which is what an editor or a
        /// player build does with `take_scene_switch` — load it from disk and
        /// rebind. Without this a client holds no NetIds and discards every
        /// snapshot, which makes a working server look like a broken one.
        fn follow_scene(&mut self, root: &Path) {
            let Some(scene) = self.session.take_scene_switch() else { return };
            let doc = floptle_scene::load(&root.join(&scene))
                .unwrap_or_else(|e| panic!("the client could not load {scene}: {e}"));
            self.world = World::default();
            floptle_scene::spawn_into(&doc, &mut self.world);
            self.session.rebind_scene(&self.world);
        }
    }

    /// Open the project as a dedicated server would, enter Play, and host.
    fn serve(root: &Path, scene: &str) -> Server {
        let mut ed = super::open(root, &root.join(scene), 1.0 / STEP);
        assert!(
            ed.script_host.errors().is_empty(),
            "the project did not open cleanly: {:?}",
            ed.script_host.errors()
        );
        ed.toggle_play();
        assert!(ed.playing, "the server never entered play mode");
        let hub = MemoryHub::new();
        ed.net_host_with(Box::new(hub.server_endpoint()), "the test hub");
        assert!(ed.net_server.is_some(), "the server never came up");
        Server { ed, hub, tick: 0, root: root.to_path_buf() }
    }

    impl Server {
        fn join(&self) -> Client {
            Client {
                session: NetSession::client(Box::new(self.hub.connect()), self.ed.input_map_hash()),
                world: World::default(),
            }
        }

        /// Advance the server and every client by `n` gameplay ticks.
        fn pump(&mut self, n: u32, clients: &mut [&mut Client]) {
            for _ in 0..n {
                self.tick += 1;
                self.hub.set_now(self.tick);
                self.ed.play_step(STEP, false);
                self.ed.adopt_script_logs(false);
                for c in clients.iter_mut() {
                    c.session.tick_client(&mut c.world);
                    c.follow_scene(&self.root);
                }
            }
        }

        fn console(&self) -> String {
            self.ed.console.entries.iter().map(|e| e.msg.as_str()).collect::<Vec<_>>().join("\n")
        }

        fn owner_of(&self, name: &str) -> Option<Option<u64>> {
            find(&self.ed.world, name)
                .and_then(|e| self.ed.world.get::<Replicated>(e))
                .map(|r| r.owner)
        }
    }

    fn find(world: &World, name: &str) -> Option<Entity> {
        world.query::<Name>().find(|(_, n)| n.0 == name).map(|(e, _)| e)
    }

    /// A scene with **two** authored player slots and one node carrying a
    /// script.
    ///
    /// Two, not one, and that is the fixture doing work. With a single slot the
    /// hosted convention (#1 = the host, unowned) and the dedicated one (every
    /// slot unowned) produce the same world, so a guard written against one
    /// slot passes under either and proves nothing. With two, the hosted
    /// convention pre-assigns #2 to peer 1 before anybody has joined — which is
    /// the thing a dedicated server must not do.
    fn scene_with(script: &str) -> String {
        format!(
            "(nodes: [\n\
             (name: \"Survivor1\", net: Some((predicted: true))),\n\
             (name: \"Survivor2\", net: Some((predicted: true))),\n\
             (name: \"Rules\", scripts: [(kind: \"{script}\")]),\n\
             ])"
        )
    }

    // ---------------------------------------------------------------- guards

    /// **A server script can build a player a body.** `net.spawn` is the whole
    /// of runtime replication, and on the server this replaced it was a no-op
    /// with no message: the command was pushed into the host's queue and
    /// nothing ever drained it, so a joiner arrived into an empty world and the
    /// game looked like a networking failure.
    #[test]
    fn a_server_script_can_spawn_a_rig_for_a_joiner() {
        let root = temp("spawn");
        write(
            &root,
            "prefabs/Survivor.prefab.ron",
            "[(name: \"Survivor\", net: Some((predicted: true))),\n\
              (name: \"Camera\", parent: Some(0))]",
        );
        write(
            &root,
            "scripts/rules.lua",
            "net.on(\"playerJoined\", function(peer)\n\
               net.spawn(\"Survivor\", { owner = peer })\n\
             end)\n",
        );
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);

        let peer = c.session.my_peer().expect("the client never got its Welcome");
        let rig = find(&c.world, "Survivor").unwrap_or_else(|| {
            panic!("the spawned rig never reached the client. Server said:\n{}", s.console())
        });
        // The whole subtree, not just the root (floptle/0181).
        let camera = find(&c.world, "Camera").expect("the rig arrived without its child");
        assert_eq!(c.world.get::<Parent>(camera).map(|p| p.0), Some(rig));
        // …and it belongs to the peer it was spawned for, or their client will
        // never predict it.
        assert_eq!(
            c.world.get::<Replicated>(rig).and_then(|r| r.owner),
            Some(peer),
            "the spawn's owner did not survive the trip"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A server script can remove a player, and they are told why.** `net.kick`
    /// was one of the six commands the old server never drained: the peer stayed
    /// connected and the script's decision evaporated.
    #[test]
    fn a_server_script_can_kick_and_the_client_learns_why() {
        let root = temp("kick");
        write(
            &root,
            "scripts/rules.lua",
            "net.on(\"playerJoined\", function(peer)\n\
               net.kick(peer, \"the lobby is closed\")\n\
             end)\n",
        );
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);

        assert!(
            s.ed.net_server.as_ref().unwrap().peers().is_empty(),
            "the kicked peer is still in the session. Server said:\n{}",
            s.console()
        );
        let told: Vec<String> = c
            .session
            .take_events()
            .into_iter()
            .filter_map(|e| match e {
                floptle_net::NetEvent::Kicked(why) => Some(why),
                _ => None,
            })
            .collect();
        assert_eq!(
            told,
            vec!["the lobby is closed".to_string()],
            "the client was dropped without being told why"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The slot a joiner drives is not reserved for a host who is not there.**
    /// A hosted session keeps Predicted node #1 for the player at the keyboard.
    /// On a dedicated server that leaves an avatar nobody controls and nobody
    /// predicts, and the first joiner spectates their own body.
    #[test]
    fn the_first_joiner_of_a_dedicated_server_drives_slot_one() {
        let root = temp("slots");
        write(&root, "scripts/rules.lua", "-- nothing to do\n");
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        assert_eq!(
            (s.owner_of("Survivor1"), s.owner_of("Survivor2")),
            (Some(None), Some(None)),
            "an authored slot belongs to nobody until somebody joins — a dedicated server \
             pre-assigns none of them"
        );
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);
        let peer = c.session.my_peer().expect("the client never got its Welcome");
        assert_eq!(
            s.owner_of("Survivor1"),
            Some(Some(peer)),
            "the first joiner did not get slot #1. Server said:\n{}",
            s.console()
        );
        assert_eq!(
            s.owner_of("Survivor2"),
            Some(None),
            "…and took only one, leaving the next slot for the next player"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A slot nobody owns is driven by nobody, and comes alive when it is
    /// claimed.**
    ///
    /// With no keyboard attached, running an unclaimed avatar's controller
    /// against permanently-empty input simulates a player who is not there —
    /// and then ships every client snapshots of them. Asserted on the SCRIPT,
    /// not on the filter sets: whether a controller ran is the thing that
    /// matters, and the filters are two of them with different rules.
    #[test]
    fn an_unclaimed_slot_runs_no_scripts_until_someone_claims_it() {
        let root = temp("idle");
        write(
            &root,
            "scripts/counter.lua",
            "local ran = false\n\
             function fixedUpdate(node, dt)\n\
               if not ran then ran = true log(\"the slot is being driven\") end\n\
             end\n",
        );
        write(
            &root,
            "scenes/arena.ron",
            "(nodes: [\n\
             (name: \"Survivor1\", net: Some((predicted: true)), scripts: [(kind: \"counter\")]),\n\
             (name: \"Survivor2\", net: Some((predicted: true)), scripts: [(kind: \"counter\")]),\n\
             ])",
        );
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        s.pump(30, &mut []);
        assert!(
            !s.console().contains("the slot is being driven"),
            "an unclaimed player slot is being simulated by nobody's input: {}",
            s.console()
        );
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);
        assert!(
            s.console().contains("the slot is being driven"),
            "the claimed slot never came alive. Server said:\n{}",
            s.console()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **`--max-players` reaches the session, not just the struct.**
    ///
    /// Parsing a flag and applying it are two different things, and only one of
    /// them is what an operator asked for. This runs the same
    /// `apply_server_opts` the real server does, so a flag that stopped being
    /// wired fails here rather than on a box at three in the morning. Watched
    /// failing with the wiring replaced by `None`.
    #[test]
    fn the_max_players_flag_reaches_the_live_session() {
        let root = temp("maxplayers");
        write(&root, "scripts/rules.lua", "-- nothing to do\n");
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        let args = super::ServerArgs::parse_argv(&[
            root.to_string_lossy().into_owned(),
            "--max-players".into(),
            "3".into(),
        ])
        .expect("parses");
        super::apply_server_opts(&mut s.ed, &args);
        assert_eq!(
            s.ed.net_server.as_ref().and_then(|n| n.max_peers()),
            Some(3),
            "the ceiling never reached the session"
        );

        // …and with no flag there is no ceiling, because capacity is the
        // operator's call and not the engine's.
        let bare = super::ServerArgs::parse_argv(&[root.to_string_lossy().into_owned()])
            .expect("parses");
        super::apply_server_opts(&mut s.ed, &bare);
        assert_eq!(s.ed.net_server.as_ref().and_then(|n| n.max_peers()), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The world a dedicated server simulates is the project's world.** The
    /// server this replaced hard-coded uniform −Y gravity and passed no terrain
    /// volumes, so a game built on a planet — the shape half this engine's demos
    /// are — simulated something the editor never showed anybody.
    #[test]
    fn a_gravity_volume_applies_on_a_dedicated_server() {
        let root = temp("gravity");
        write(&root, "scripts/rules.lua", "-- nothing to do\n");
        // Gravity pulls toward the volume's centre, which is +X of the body —
        // the opposite of the −Y the old server assumed, so a pass cannot be a
        // coincidence.
        write(
            &root,
            "scenes/arena.ron",
            "(nodes: [\n\
             (name: \"Planet\", transform: (translation: (60.0, 0.0, 0.0)),\n\
              matter: GravityVolume(radial: true, strength: 30.0, radius: 500.0)),\n\
             (name: \"Survivor1\", net: Some((predicted: true, physics: true)),\n\
              rigidbody: Some((mode: Dynamic))),\n\
             (name: \"Rules\", scripts: [(kind: \"rules\")]),\n\
             ])",
        );
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        s.pump(60, &mut []);
        let body = find(&s.ed.world, "Survivor1").unwrap();
        let pos = s.ed.world.get::<Transform>(body).unwrap().translation;
        assert!(
            pos.x > 0.5,
            "the body did not fall toward the planet (x = {:.3}, y = {:.3}) — the server is \
             using its own gravity, not the scene's. Server said:\n{}",
            pos.x,
            pos.y,
            s.console()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A package's scripts run on a dedicated server too.** Mirror of the rc4
    /// guard `a_projects_package_scripts_are_reachable_after_opening_it`: a
    /// package ships `scripts/*.lua` that scene nodes are attached to by bare
    /// name, and a server that never resolved the package would run those nodes
    /// silently — nothing failed, the name simply named nothing.
    #[test]
    fn a_package_script_on_a_node_runs_on_a_dedicated_server() {
        let root = temp("package");
        let pkg = root.join("packages/com.example.rules");
        std::fs::create_dir_all(pkg.join("scripts")).unwrap();
        write(
            &root,
            "packages/com.example.rules/package.ron",
            "(id: \"com.example.rules\", name: \"Rules\", version: \"1.0.0\")",
        );
        write(
            &root,
            "packages/com.example.rules/scripts/pkgRules.lua",
            "function start(node) log(\"the package script ran\") end\n",
        );
        write(
            &root,
            "packages.ron",
            "(packages: [(id: \"com.example.rules\", version: \"1.0.0\", source: Authored, \
             enabled: true)])",
        );
        write(&root, "scenes/arena.ron", &scene_with("pkgRules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        s.pump(10, &mut []);
        assert!(
            s.console().contains("the package script ran"),
            "the package's script never resolved on the server. Server said:\n{}",
            s.console()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The first peer can be on the roster before the host's first tick
    /// ends**, and `playerJoined` fires from inside that tick.
    ///
    /// The state mirror that answers `net.role()` used to run only at the
    /// bottom of a tick, so the very first handler ran while the role still
    /// said `offline` — and `net.spawn`, `net.kick`, `net.setOwner` and
    /// `net.setRelevant` all check that role and refuse. The FIRST player to
    /// join got no avatar and no moderation and the rest were fine, which reads
    /// as a flaky link rather than as a bug. Watched failing.
    #[test]
    fn the_first_peer_to_join_is_not_told_the_server_is_offline() {
        let root = temp("firsttick");
        write(
            &root,
            "scripts/rules.lua",
            "net.on(\"playerJoined\", function(peer)\n\
               log(\"role=\" .. tostring(net.role()))\n\
             end)\n",
        );
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);
        let said = s.console();
        assert!(said.contains("role=server"), "the first joiner's handler ran as: {said}");
        assert!(
            !said.contains("only the server"),
            "a server-only call was refused ON the server: {said}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
