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
    /// **The lobby code to reclaim** (`floptle/0217`), from `--lobby-code` or
    /// `FLOPTLE_LOBBY_CODE`. `None` means "give me a fresh one", which is every
    /// self-hosted server and every player.
    pub lobby_code: Option<String>,
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
            lobby_code: None,
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
                "--lobby-code" => self.lobby_code = Some(need(val, "--lobby-code")?.to_uppercase()),
                "--status-file" => self.status_file = Some(PathBuf::from(need(val, "--status-file")?)),
                "--game-key" => self.game_key = Some(need(val, "--game-key")?),
                other => return Err(format!("unknown flag {other}")),
            }
            k += 2;
        }
        if self.tick_hz <= 0.0 || self.tick_hz > 1000.0 {
            return Err("--tick must be between 1 and 1000 Hz".into());
        }
        // The key a supervisor could only pass safely through the environment.
        self.game_key =
            resolve_game_key(self.game_key.take(), std::env::var(GAME_KEY_ENV).ok());
        // The code a supervisor hands down, same shape and same reasoning as
        // the key above: `floptle-fleet` sets it from `/desired`, and an
        // explicit flag still wins.
        self.lobby_code =
            resolve_lobby_code(self.lobby_code.take(), std::env::var(LOBBY_CODE_ENV).ok());
        Ok(())
    }
}

/// Which game key this process reports, given the command line and the
/// environment.
///
/// **A supervisor cannot put a key on the command line.** An `ExecStart` is
/// readable by every `ps` on the box and is echoed into the journal — which is
/// then shipped to a control plane and rendered on a web page — so
/// `floptle-fleet` deliberately passes the key as `FLOPTLE_GAME_KEY` in the
/// unit's environment instead, with a test of its own asserting it never
/// appears in the command. Nothing here read that variable, so the careful path
/// went nowhere and every status report said `"game_key": null`.
///
/// An explicit `--game-key` still wins, and a blank value is no key rather than
/// an empty one — an unset variable and one set to nothing should not mean two
/// different things to a script that exports it conditionally.
pub(crate) fn resolve_game_key(explicit: Option<String>, from_env: Option<String>) -> Option<String> {
    explicit.or(from_env).map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

/// The environment variable a supervisor passes the game key in.
pub(crate) const GAME_KEY_ENV: &str = "FLOPTLE_GAME_KEY";

/// **Which lobby code this process should reclaim** (`floptle/0217`), given the
/// command line and the environment.
///
/// ⚠ **A blank value is no code, not an empty code.** The fleet agent writes
/// `FLOPTLE_LOBBY_CODE` only when `/desired` carries one, but a hand-written
/// unit or a shell wrapper can easily export it as `""` — and an empty string
/// reaching the relay would be a claim on a code that cannot exist, answered by
/// minting a fresh one after a pointless round trip.
///
/// Upper-cased because lobby codes are, and a developer typing a lower-case one
/// into a unit file should not silently fail to reclaim.
pub(crate) fn resolve_lobby_code(
    explicit: Option<String>,
    from_env: Option<String>,
) -> Option<String> {
    explicit
        .or(from_env)
        .map(|c| c.trim().to_uppercase())
        .filter(|c| !c.is_empty())
}

/// The environment variable a supervisor passes the lobby code in.
pub(crate) const LOBBY_CODE_ENV: &str = "FLOPTLE_LOBBY_CODE";

/// **Where a running server can actually be reached** (`floptle/0209`).
///
/// `--port` and `--relay` are alternatives, not a pair: with a relay the server
/// makes one outbound connection and listens on nothing, so a port given
/// alongside it is not bound and never was. That was silent, and it mattered
/// because the control plane was publishing an address built from the port it
/// allocated — `quic://host:30000` — for a process that had no socket there.
/// For two days that address resolved to a *different* game, a leftover test
/// server that happened to hold the port.
///
/// One rule, read twice: once for what the server says at startup and once for
/// what it writes into its status file. A server that told an operator one
/// thing and a control plane another would be worse than either.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reachable {
    /// The UDP port this server is listening on, if it is listening at all.
    pub port: Option<u16>,
    /// The relay it registered with, if it did.
    pub relay: Option<String>,
}

pub(crate) fn reachable(args: &ServerArgs) -> Reachable {
    match &args.relay {
        // The relay wins, and the port is not bound — see `run`'s dispatch.
        Some(addr) => Reachable { port: None, relay: Some(addr.clone()) },
        None => Reachable { port: args.port, relay: None },
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

    // **Say that the port is not being listened on** (`floptle/0209`). A
    // caller that passed both had no way to learn one of them did nothing, and
    // an address built from it reaches nothing. Said rather than refused,
    // deliberately: a fleet box passes both today, and refusing would take a
    // region down to make a point about a flag.
    if args.relay.is_some()
        && let Some(port) = args.port
    {
        println!(
            "  --port {port} is not being listened on: this server is reachable through the \
             relay, by lobby code, and has no socket of its own"
        );
    }
    // Before hosting, not after: the code has to ride out with the host
    // request itself.
    ed.net_reclaim_code = args.lobby_code.clone();
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
    install_signal_stop();

    let period = std::time::Duration::from_secs_f32(step);
    let mut ticks = 0u64;
    let started = Instant::now();
    let mut last_status = Instant::now() - STATUS_EVERY;
    let mut ticks_ms = TickWindow::default();
    let mut next = Instant::now() + period;
    while !stop.load(std::sync::atomic::Ordering::Relaxed) && !signalled() {
        ticks += 1;
        // `game_focused: false` — there is no keyboard here, so the only input
        // reaching a script is a client's replayed input, per owner.
        let tick_began = Instant::now();
        ed.play_step(step, false);
        ticks_ms.push(tick_began.elapsed().as_secs_f32() * 1000.0);
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
            write_status(path, &args, &ed, ticks, started, &ticks_ms);
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
    say_goodbye(&mut ed, step);
    println!("  server stopped after {ticks} tick(s)");
    0
}

/// What a player sees instead of "connection lost".
const GOODBYE: &str = "the server is shutting down";

/// Tell every player the world is going away, and give the message time to
/// leave.
///
/// **A restart that looks like a crash is the difference between "they are
/// updating" and "it broke again".** Clients are told through the kick path
/// rather than a new message — it is the one route that already carries a
/// reason all the way to `net.on("kicked")` and the F1 menu, and it is tested.
/// The cost is that a game which hardcodes "you were removed by a moderator"
/// instead of showing the reason will say the wrong thing; the docs have always
/// said to show the reason, and inventing a second nearly-identical message
/// would leave both halves half-handled.
///
/// The flush matters as much as the message. A reliable send is retransmitted
/// until it is acknowledged, and retransmission happens *in ticks* — so exiting
/// straight after queueing it would close the socket on a message that had not
/// left yet, which is precisely the silence this exists to replace.
#[cfg(not(target_arch = "wasm32"))]
fn say_goodbye(ed: &mut Editor, step: f32) {
    let peers: Vec<floptle_net::PeerId> =
        ed.net_server.as_ref().map(|s| s.peers().to_vec()).unwrap_or_default();
    if peers.is_empty() {
        return;
    }
    println!("  telling {} player(s) the server is stopping", peers.len());
    if let Some(s) = ed.net_server.as_mut() {
        for p in &peers {
            s.kick(*p, GOODBYE);
        }
    }
    let deadline = Instant::now() + std::time::Duration::from_millis(500);
    let period = std::time::Duration::from_secs_f32(step);
    while Instant::now() < deadline {
        ed.play_step(step, false);
        ed.adopt_script_logs(false);
        drain_console(ed);
        std::thread::sleep(period);
    }
}

/// Set by the signal handler, read by the loop. **The only thing a handler may
/// safely do**: no allocation, no locks, no printing — just a flag the ordinary
/// code notices a tick later.
#[cfg(all(unix, not(target_arch = "wasm32")))]
static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(all(unix, not(target_arch = "wasm32")))]
extern "C" fn on_stop_signal(_sig: libc::c_int) {
    SIGNALLED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Stop cleanly on SIGTERM and SIGINT.
///
/// **This is how a service is actually stopped.** `systemctl stop`, a container
/// runtime, and Ctrl-C at a terminal all send one of these, and Rust's default
/// disposition ends the process where it stands — no goodbye, and every client
/// reporting a connection lost. The Enter watcher below only ever covered an
/// operator sitting at a TTY, which is the one case that matters least.
#[cfg(all(unix, not(target_arch = "wasm32")))]
fn install_signal_stop() {
    // SAFETY: `on_stop_signal` only stores into an atomic, which is
    // async-signal-safe. Registering a handler is the documented use of this
    // call.
    unsafe {
        libc::signal(libc::SIGTERM, on_stop_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_stop_signal as *const () as libc::sighandler_t);
    }
}

#[cfg(all(not(unix), not(target_arch = "wasm32")))]
fn install_signal_stop() {}

#[cfg(all(unix, not(target_arch = "wasm32")))]
fn signalled() -> bool {
    SIGNALLED.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(all(not(unix), not(target_arch = "wasm32")))]
fn signalled() -> bool {
    false
}

/// A rolling window of tick durations, so the status file can carry a p95.
///
/// **The p95 rather than the mean is the point.** A server whose average tick
/// is 4 ms and whose worst one in twenty is 40 ms is a server players describe
/// as stuttering, and a mean hides that completely. The fleet agent ships this
/// number to the control plane (`floptle/0199` §3) and it is what a developer
/// looks at when a match "felt bad" — so it has to be the statistic that can
/// actually say so.
///
/// One window of the last `CAP` ticks: at 60 Hz that is the last ten seconds,
/// which matches how often the status file is rewritten.
#[derive(Default)]
pub(crate) struct TickWindow {
    ms: Vec<f32>,
    at: usize,
}

impl TickWindow {
    const CAP: usize = 600;

    pub(crate) fn push(&mut self, ms: f32) {
        if self.ms.len() < Self::CAP {
            self.ms.push(ms);
        } else {
            self.ms[self.at] = ms;
            self.at = (self.at + 1) % Self::CAP;
        }
    }

    /// The 95th percentile of the window, or `None` before there is one.
    ///
    /// `None` rather than 0.0 while the window is empty: a zero would be
    /// reported as a perfect tick time, which is the wrong thing to say about a
    /// server that has not run yet.
    pub(crate) fn p95(&self) -> Option<f32> {
        if self.ms.is_empty() {
            return None;
        }
        let mut v = self.ms.clone();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        // The index of the 95th percentile, clamped so a one-sample window
        // answers with its one sample rather than reading off the end.
        let i = ((v.len() as f32 * 0.95).ceil() as usize).saturating_sub(1).min(v.len() - 1);
        Some(v[i])
    }
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
fn write_status(
    path: &Path,
    args: &ServerArgs,
    ed: &Editor,
    ticks: u64,
    started: Instant,
    ticks_ms: &TickWindow,
) {
    let doc = status_document(args, ed, ticks, started.elapsed().as_secs(), ticks_ms);
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, doc).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The status document itself, as text — split from the write so a test can
/// read what a keyed server would put in the file without running one.
///
/// **The game key is not in it.** The file sits in a runtime directory on a
/// box that runs other developers' servers too, and a key is a credential; what
/// the portal wants from it is *which* key is live, and the first twelve
/// characters (`game_key_prefix`) say that. The agent that wrote the unit
/// already holds the whole key.
#[cfg(not(target_arch = "wasm32"))]
fn status_document(
    args: &ServerArgs,
    ed: &Editor,
    ticks: u64,
    uptime_s: u64,
    ticks_ms: &TickWindow,
) -> String {
    let peers = ed.net_server.as_ref().map(|s| s.peers().len()).unwrap_or(0);
    let where_reachable = reachable(args);
    format!(
        "{{\n  \"peers\": {peers},\n  \"max_players\": {},\n  \"uptime_s\": {uptime_s},\n  \
         \"ticks\": {ticks},\n  \"tick_hz\": {},\n  \"scene\": {:?},\n  \
         \"project\": {:?},\n  \"game_key_prefix\": {},\n  \"lobby_code\": {},\n  \
         \"port\": {},\n  \"relay\": {},\n  \
         \"tick_p95_ms\": {}\n}}\n",
        args.max_players.map(|m| m.to_string()).unwrap_or_else(|| "null".into()),
        args.tick_hz,
        ed.scene_rel_or_default(),
        args.project.to_string_lossy(),
        args.game_key
            .as_deref()
            .map(|k| format!("{:?}", key_prefix(k)))
            .unwrap_or_else(|| "null".into()),
        ed.net_lobby_code.as_deref().map(|c| format!("{c:?}")).unwrap_or_else(|| "null".into()),
        // **Where this server is reachable, measured rather than derived**
        // (`floptle/0209`). A control plane that builds an address out of the
        // port it allocated publishes one that reaches nothing whenever the
        // server is relay-hosted, and nothing anywhere contradicts it.
        where_reachable.port.map(|p| p.to_string()).unwrap_or_else(|| "null".into()),
        where_reachable.relay.as_deref().map(|r| format!("{r:?}")).unwrap_or_else(|| "null".into()),
        // `null` rather than 0 before the window has a sample: a zero would be
        // read as a perfect tick time on a server that has not run one yet.
        ticks_ms.p95().map(|v| format!("{v:.3}")).unwrap_or_else(|| "null".into()),
    )
}

/// The part of a game key that says which key it is and nothing else: the
/// `fk_live_` tag and the first four characters after it.
fn key_prefix(key: &str) -> &str {
    let end = key.char_indices().nth(12).map_or(key.len(), |(i, _)| i);
    &key[..end]
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
pub(crate) fn check_servable(doc: &floptle_scene::SceneDoc, scene_path: &Path) -> Result<(), String> {
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

    /// A `ServerArgs` with nothing set, for a test about one field.
    pub(super) fn blank_args() -> ServerArgs {
        ServerArgs {
            project: PathBuf::new(),
            scene: None,
            port: None,
            relay: None,
            lobby_code: None,
            tick_hz: 60.0,
            interest: None,
            budget: None,
            max_players: None,
            status_file: None,
            game_key: None,
        }
    }

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

    /// ⚠ **Every flag `serve` accepts is in the published CLI docs**, and the
    /// list comes off this file's OWN match arms rather than a second
    /// hand-written one.
    ///
    /// `cli.json` is generated from the verb table and published to the website
    /// as the developer-facing command reference (`floptle/0201`, where that
    /// page turned out to have been hand-carried and wrong). A flag added to
    /// the parser and not to the table is invisible to every developer who does
    /// not read the source — which is all of them. `--status-file` and
    /// `--max-players` had both been in the parser and out of the docs, and the
    /// fleet agent depends on the first one.
    ///
    /// ⚠ Gated on `editor-ui` because the verb table is: `dedicated` builds in
    /// the player and server configurations and `cli` does not, so an ungated
    /// reference here fails a build that workspace clippy never attempts.
    #[cfg(feature = "editor-ui")]
    #[test]
    fn every_serve_flag_the_parser_takes_is_in_the_published_cli_docs() {
        let src = include_str!("dedicated.rs");
        // The parser's arms as written: a line whose first token is a quoted
        // flag, followed by `=>`.
        let mut parsed: Vec<&str> = Vec::new();
        for line in src.lines().map(str::trim) {
            let Some(rest) = line.strip_prefix('"') else { continue };
            let Some((flag, tail)) = rest.split_once('"') else { continue };
            if flag.starts_with("--") && tail.trim_start().starts_with("=>") {
                parsed.push(flag);
            }
        }
        parsed.sort_unstable();
        parsed.dedup();
        // ⚠ Without this the guard passes by finding NOTHING the day the parser
        // is reformatted — measuring nothing while reporting success.
        assert!(
            parsed.len() >= 9,
            "the scrape found {parsed:?} — it has stopped seeing the match arms, so this \
             guard is measuring nothing"
        );

        let serve = crate::cli::VERBS
            .iter()
            .find(|v| v.name == "serve")
            .expect("the serve verb");
        let documented: Vec<&str> =
            serve.args.iter().map(|a| a.name).filter(|n| n.starts_with("--")).collect();

        // Two are deliberately out of the published table, and neither is an
        // oversight:
        //
        // `--game-key` is a CREDENTIAL. `ps` shows a command line, the journal
        // echoes it, and the fleet agent ships the last 200 journal lines to the
        // control plane where they are rendered on a web page — which is exactly
        // why the agent passes the key in `Environment=` instead. Publishing a
        // flag that puts it on the command line would be advice against the
        // engine's own design.
        //
        // `--build` is an internal alias for the project path used by the export
        // path, not a second way for a person to say PROJECT.
        const UNPUBLISHED: &[&str] = &["--game-key", "--build"];

        for flag in &parsed {
            if UNPUBLISHED.contains(flag) {
                continue;
            }
            assert!(
                documented.contains(flag),
                "`serve` accepts {flag} and the published CLI docs never mention it — \
                 documented: {documented:?}"
            );
        }
        // And the other direction: a documented flag the parser would reject is
        // worse than an undocumented one, because somebody will type it.
        for flag in &documented {
            assert!(
                parsed.contains(flag),
                "the CLI docs advertise {flag} and the parser refuses it"
            );
        }
    }

    /// **A relay-hosted server is not listening on a port** (`floptle/0209`).
    ///
    /// `--port` and `--relay` are alternatives. With a relay the server makes
    /// one outbound connection and binds nothing, so a port passed alongside is
    /// not listened on — which was silent, while the control plane published
    /// `quic://<host>:<that port>` as the deployment's address. For two days
    /// that address resolved to a **different game**: a leftover test server
    /// was holding the port. Once it was killed the address simply reached
    /// nothing, which is how it was noticed at all.
    ///
    /// The rule is asserted rather than the flags, because the same answer is
    /// read twice — the startup line an operator sees and the status file a
    /// control plane reads — and those two disagreeing is the actual failure.
    #[test]
    fn a_relay_hosted_server_reports_no_local_port() {
        let with_relay = ServerArgs {
            port: Some(30000),
            relay: Some("us-east.relay.fopull.com:7788".into()),
            ..blank_args()
        };
        assert_eq!(
            reachable(&with_relay),
            Reachable { port: None, relay: Some("us-east.relay.fopull.com:7788".into()) },
            "a port that is not bound must not be published as an address"
        );

        // Direct hosting is the case where the port IS the handle, and a region
        // with no relay has nothing else to publish.
        let direct = ServerArgs { port: Some(30000), relay: None, ..blank_args() };
        assert_eq!(
            reachable(&direct),
            Reachable { port: Some(30000), relay: None },
            "a directly hosted server is reachable at its port and nowhere else"
        );
    }

    /// **A key a supervisor could only pass through the environment is read.**
    ///
    /// `floptle-fleet` puts the game key in its unit's `Environment=` rather
    /// than on the `ExecStart`, because a command line is readable by every
    /// `ps` on the box and is echoed into the journal the agent then ships to
    /// the control plane and which is rendered on a web page. The agent has a
    /// test of its own asserting the key never reaches the command line — and
    /// nothing on this side read the variable, so that careful path went
    /// nowhere and every live status report said `"game_key": null`.
    #[test]
    fn a_game_key_can_arrive_through_the_environment() {
        assert_eq!(
            resolve_game_key(Some("fk_flag".into()), Some("fk_env".into())).as_deref(),
            Some("fk_flag"),
            "an explicit --game-key wins over the environment"
        );
        assert_eq!(
            resolve_game_key(None, Some("fk_env".into())).as_deref(),
            Some("fk_env"),
            "the supervisor's variable is the only way to pass a key safely"
        );
        assert_eq!(resolve_game_key(None, None), None, "no key is no key");
        assert_eq!(
            resolve_game_key(None, Some("   ".into())),
            None,
            "a blank variable is no key rather than an empty one — a script that \
             exports it conditionally must not produce a second meaning"
        );
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
    /// **The p95 is the statistic that can say "it stutters".**
    ///
    /// A mean cannot: a server whose ticks are 4 ms with one in ten at 40 ms is
    /// one players describe as stuttering, and its mean is a healthy-looking
    /// 7.6 ms. The fleet agent ships this number to the control plane
    /// (`floptle/0199` §3) and it is what a developer looks at when a match
    /// "felt bad", so it has to be the statistic that can actually say so.
    ///
    /// Note what p95 does NOT promise, because the first version of this test
    /// asserted it and was wrong: at exactly one bad tick in twenty, five per
    /// cent of ticks are worse than the answer, so the 95th percentile is the
    /// last GOOD one. That is p95 behaving correctly. To show, a stutter has to
    /// be more than five per cent of ticks — which is also the threshold at
    /// which a player notices it.
    #[test]
    fn the_tick_p95_reports_the_bad_ticks_rather_than_averaging_them_off() {
        let mut w = TickWindow::default();
        assert_eq!(w.p95(), None, "a server that has not ticked has no tick time, not zero");

        // One in ten bad: comfortably above the percentile's own threshold.
        for _ in 0..18 {
            w.push(4.0);
        }
        w.push(40.0);
        w.push(40.0);
        let p95 = w.p95().expect("a sample");
        assert_eq!(p95, 40.0, "two ticks in twenty at 40 ms must show");
        let mean: f32 = 4.0f32.mul_add(18.0, 80.0) / 20.0;
        assert!(p95 > mean, "the mean is {mean:.1} ms and reads as healthy");

        // A single sample answers with itself rather than reading off the end.
        let mut one = TickWindow::default();
        one.push(7.5);
        assert_eq!(one.p95(), Some(7.5));

        // A server that is genuinely fine reports that it is fine.
        let mut good = TickWindow::default();
        for _ in 0..100 {
            good.push(3.0);
        }
        assert_eq!(good.p95(), Some(3.0));
    }

    /// The window is bounded, so a server up for a week does not grow a vector
    /// of six hundred thousand floats.
    #[test]
    fn the_tick_window_stays_bounded_and_keeps_the_recent_ticks() {
        let mut w = TickWindow::default();
        for _ in 0..(TickWindow::CAP * 3) {
            w.push(100.0);
        }
        assert_eq!(w.ms.len(), TickWindow::CAP);
        // Now the recent past is fast: the number has to follow it down, or a
        // server that recovered would look broken forever.
        for _ in 0..TickWindow::CAP {
            w.push(2.0);
        }
        assert_eq!(w.p95(), Some(2.0), "the window forgot the old ticks");
    }

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

    /// **A dedicated server says so, through the binding a game actually
    /// calls.**
    ///
    /// This is the one session fact a game cannot work out for itself, and the
    /// bug it exists for was expensive: Forgery's `hello()` ran on any machine
    /// answering `net.isServer()`, so a deployed box entered its own roster as
    /// "Player", was dealt a character, and counted toward `min_players` — and
    /// because `canBegin` requires every entry to be ready, **a match on a
    /// dedicated server could never start**, with nothing saying why.
    ///
    /// The whole wiring is `dedicated: true` on the Editor `open` builds and
    /// `self.dedicated` in one `NetState`. Either line could be dropped by a
    /// refactor and turn the bug back on with the same silent symptom, so the
    /// assertion runs a real script through the real binding rather than
    /// reading the field. `net.isServer()` is asserted alongside it because the
    /// two must not be confused: a dedicated server is emphatically still the
    /// server.
    #[test]
    fn a_dedicated_server_says_so_through_the_binding_a_game_calls() {
        let root = temp("isdedicated");
        write(
            &root,
            "scripts/rules.lua",
            "local said = false\n\
             function update(node)\n\
             \x20 if not said then\n\
             \x20   said = true\n\
             \x20   print(\"server=\" .. tostring(net.isServer())\n\
             \x20     .. \" dedicated=\" .. tostring(net.isDedicated()))\n\
             \x20 end\n\
             end\n",
        );
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        s.pump(SETTLE, &mut []);
        assert!(
            s.ed.dedicated,
            "the Editor `dedicated::open` built does not know it is a dedicated server"
        );
        let said = s.console();
        assert!(
            said.contains("server=true dedicated=true"),
            "a dedicated server did not report itself as one. Server said:\n{said}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **The status file does not carry the game key.** It sits in a runtime
    /// directory on a box that runs other developers' servers, and the agent
    /// that reads it already holds the key. The prefix says which key is live;
    /// the whole key is asserted ABSENT — a test that only checked the prefix
    /// was present would pass with the key beside it.
    #[test]
    fn the_status_file_names_the_keys_prefix_and_never_the_key() {
        let root = temp("statuskey");
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "scripts/rules.lua", "function update(node) end\n");
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");
        let s = serve(&root, "scenes/arena.ron");
        let mut args = super::tests::blank_args();
        args.game_key = Some("fk_live_SECRETSECRETSECRET".into());
        let doc = super::status_document(&args, &s.ed, 0, 0, &TickWindow::default());
        assert!(!doc.contains("SECRET"), "the key is in the status file:\n{doc}");
        assert!(!doc.contains("\"game_key\""), "the key field is still written:\n{doc}");
        assert!(doc.contains("\"game_key_prefix\": \"fk_live_SECR\""), "{doc}");
        // A keyless server says so, and a short key is its own prefix.
        args.game_key = None;
        let doc = super::status_document(&args, &s.ed, 0, 0, &TickWindow::default());
        assert!(doc.contains("\"game_key_prefix\": null"), "{doc}");
        assert_eq!(key_prefix("fk_live_ab"), "fk_live_ab");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// **A dedicated server's scripts cannot reach the box they run on.** The
    /// address policy is the driver's to set (`floptle_script::http_policy`),
    /// and this driver never opens local addresses: a game's `http.get` at a
    /// loopback port is refused with the rule named, and the port sees no
    /// connection. The listener's count is the guard — an error in the
    /// script and a socket opened anyway would read the same in the Console.
    #[test]
    fn a_dedicated_servers_scripts_are_refused_the_boxs_own_ports() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = std::sync::Arc::new(AtomicUsize::new(0));
        {
            let n = accepted.clone();
            std::thread::spawn(move || {
                for _ in listener.incoming().flatten() {
                    n.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        let root = temp("httppolicy");
        write(
            &root,
            "scripts/rules.lua",
            &format!(
                "local asked = false\n\
                 function update(node)\n\
                 \x20 if asked then return end\n\
                 \x20 asked = true\n\
                 \x20 local ok, why = pcall(http.get, 'http://127.0.0.1:{port}/', \
                 function(r) print('reply=' .. tostring(r.error)) end)\n\
                 \x20 print('called=' .. tostring(ok) .. ' why=' .. tostring(why))\n\
                 end\n"
            ),
        );
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        s.pump(SETTLE, &mut []);
        let said = s.console();
        assert!(said.contains("called=false"), "the call was accepted. Server said:\n{said}");
        assert!(said.contains("http: refused"), "the rule is not named. Server said:\n{said}");
        assert!(said.contains("loopback"), "the class is not named. Server said:\n{said}");
        assert_eq!(accepted.load(Ordering::SeqCst), 0, "the server's script opened a socket");
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

    /// **A stopping server says goodbye, and the player is told rather than
    /// dropped.**
    ///
    /// A restart that looks like a crash is the difference between "they are
    /// updating" and "it broke again".
    ///
    /// **What this can and cannot hold.** The memory hub delivers in-process,
    /// so removing the flush entirely leaves this green — over a real UDP link
    /// it would not, because a reliable send is retransmitted until it is
    /// acknowledged and retransmission happens in TICKS. Rather than claim
    /// coverage it does not have, the guard asserts the flush *ran*: the
    /// server's tick advanced while saying goodbye, which is the thing a queued-
    /// and-immediately-exited version would not do. The delivery itself is
    /// covered; the wire timing waits for the field test.
    #[test]
    fn a_stopping_server_says_goodbye_before_it_goes() {
        let root = temp("goodbye");
        write(&root, "scripts/rules.lua", "-- nothing to do\n");
        write(&root, "scenes/arena.ron", &scene_with("rules"));
        write(&root, "project.ron", "(entry_scene: Some(\"scenes/arena.ron\"))");

        let mut s = serve(&root, "scenes/arena.ron");
        let mut c = s.join();
        s.pump(SETTLE, &mut [&mut c]);
        assert_eq!(
            s.ed.net_server.as_ref().map(|n| n.peers().len()),
            Some(1),
            "the player has to be in before they can be said goodbye to"
        );
        let _ = c.session.take_events();

        let before = s.ed.game_tick_no;
        super::say_goodbye(&mut s.ed, STEP);
        assert!(
            s.ed.game_tick_no > before,
            "the goodbye was queued and the server left without ticking, so a real link \
             would have closed on a message that never went out"
        );
        // The client has to tick to hear it — the goodbye is on the wire, not
        // in the server's memory.
        let mut heard = Vec::new();
        for _ in 0..30 {
            c.session.tick_client(&mut c.world);
            heard.extend(c.session.take_events().into_iter().filter_map(|e| match e {
                floptle_net::NetEvent::Kicked(why) => Some(why),
                _ => None,
            }));
        }
        assert!(
            heard.iter().any(|w| w.contains("shutting down")),
            "the player was dropped without being told why: {heard:?}"
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
