//! Editor plumbing for a rollback session: engaging the driver, moving inputs
//! and checksums across the wire, and driving one tick of the play loop.
//!
//! The driver ([`crate::rollback::RollbackDriver`]) is deliberately ignorant of
//! sessions, transports and the editor's frame; this is the layer that knows
//! about all three. It is the rollback equivalent of `net.rs`'s client/server
//! tick halves, and it sits beside them — nothing here touches the
//! `Authority`/`Predicted` paths.
//!
//! ## Offline is unchanged, on purpose
//!
//! A `Rollback` node with no session behind it never sees a driver: local versus
//! works exactly as it did, both fighters running under the global script pass
//! against the local pads. The driver engages when a session starts and the
//! scene actually has rollback nodes, and hands everything back when it ends.

use floptle_net::{NetInput, PeerId, SERVER};

/// How many ticks the referee may catch up in one frame. It is allowed to be
/// late; it is not allowed to be why the game stutters.
const REFEREE_CATCHUP_TICKS: u64 = 8;

use crate::rollback::{Ctx, RollbackDriver};
use crate::Editor;

impl Editor {
    /// The live driver's node ids — the set that belongs in BOTH script
    /// filters for as long as it is running those nodes' hooks itself.
    ///
    /// Empty when no driver is running, which is what makes it safe to union
    /// into every other filter computation unconditionally. The session's
    /// filter setters are whole-set assignments, so without a single agreed
    /// source for this half, `net_client_side_setup` (which re-runs on every
    /// replicated spawn) and the rollback start would take turns erasing each
    /// other — a mid-match spawn would have put the fighters back into the
    /// global passes, running them twice per tick.
    pub(crate) fn rollback_filter_eids(&self) -> std::collections::HashSet<u32> {
        self.net_rollback.as_ref().map(|d| d.eids()).unwrap_or_default()
    }

    /// Does the running scene simulate anything by rollback?
    pub(crate) fn scene_has_rollback(&self) -> bool {
        self.world
            .query::<floptle_core::Replicated>()
            .any(|(_, r)| r.mode.is_rollback())
    }

    /// Start (or restart) the rollback driver for a roster.
    ///
    /// Called on the host when it announces a match and on a client when
    /// `RollbackStart` arrives — the same moment on both, which is what gives
    /// the session its shared tick origin.
    pub(crate) fn net_rollback_start(
        &mut self,
        local: PeerId,
        peers: Vec<PeerId>,
        delay: u8,
        seed: u64,
    ) {
        let mut d = match self.net_rollback.take() {
            Some(mut d) => {
                d.restart(local, peers.clone(), delay, seed);
                d
            }
            None => RollbackDriver::new(local, peers.clone(), delay, seed),
        };
        let Some(sim) = self.sim.as_mut() else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "rollback: no physics sim yet — enter Play before starting the match".into(),
                None,
            );
            return;
        };
        d.rebind(&self.world, sim, &self.script_host);
        if d.nodes().is_empty() {
            return; // nothing to drive; the session is an ordinary one
        }
        // The driver runs these nodes' hooks itself, every tick, in its own
        // order — so they leave the global passes entirely. `run_*_for` bypasses
        // both filters, which is the same arrangement the host already uses for
        // remote-owned Predicted nodes. ADDED to the session's filters, never
        // assigned over them: on a client the session is already skipping every
        // authority-driven node, and replacing the set would set all of those
        // running again.
        self.script_host.extend_filters(d.eids());
        // The warm-up ticks nobody sampled: seeded locally AND shipped, or the
        // confirmed frontier could never leave zero and every peer would stall
        // a few ticks into the match with nothing to wait for.
        for (applied, input) in d.net.prime_warmup() {
            self.net_rollback_send(applied, input);
        }
        let n = d.nodes().len();
        for f in d.faults.drain(..) {
            self.console.push(floptle_script::LogLevel::Warn, f, None);
        }
        self.net_rollback = Some(d);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "🥊 rollback match started — {n} fighter(s), {delay}-tick input delay, \
                 peers {peers:?} in slot order"
            ),
            None,
        );
    }

    /// Host: engage rollback for this session if the scene calls for it.
    ///
    /// Called right after a session comes up. A scene with no `Rollback` nodes
    /// leaves the session exactly as it was — one netcode, three modes, and a
    /// project that doesn't use this one never pays for it.
    pub(crate) fn net_rollback_host_setup(&mut self) {
        if !self.scene_has_rollback() {
            return;
        }
        let delay = floptle_net::DEFAULT_INPUT_DELAY;
        // The seed is drawn ONCE, here, from the wall clock — the only place in
        // the whole feature where a clock is allowed near the simulation. From
        // this moment it is replicated state like any other, and every draw
        // comes from (seed, tick, index).
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        let Some(s) = self.net_server.as_mut() else { return };
        s.set_rollback(true, delay, seed);
        let peers = s.rollback_slots().to_vec();
        self.net_rollback_start(SERVER, peers, delay, seed);
        self.net_referee_start();
    }

    /// Host: the roster changed (someone joined or left). The session has
    /// already re-announced it to every client; restart the local driver on the
    /// same roster so all peers share a tick origin again.
    pub(crate) fn net_rollback_resync(&mut self) {
        let Some(s) = self.net_server.as_ref() else { return };
        if !s.is_rollback() {
            return;
        }
        let (peers, delay, seed) =
            (s.rollback_slots().to_vec(), s.input_delay(), s.rollback_seed());
        self.net_rollback_start(SERVER, peers, delay, seed);
    }

    /// Host: start the referee and the match recording, if the project wants
    /// them (`docs/rollback-netcode-design.md` §5).
    ///
    /// Both ride the same input log, and both are the host's job: it is the one
    /// peer that sees every input, because it is the one fanning them out.
    fn net_referee_start(&mut self) {
        let Some(doc) = self.net_scene_doc.clone() else { return };
        if !crate::shadow::scene_has_rollback(&doc) {
            return;
        }
        let scene = self.scene_name.clone();
        let Some(s) = self.net_server.as_mut() else { return };
        s.start_recording(&scene);
        let Some(log) = s.recording().cloned() else { return };
        let (dir, map, step) = (
            self.scripts_dir(),
            self.script_host.input_system().borrow().map().clone(),
            self.game_tick.step,
        );
        self.net_referee = Some(crate::shadow::ShadowSim::build(
            &doc,
            &dir,
            map,
            log,
            floptle_net::SERVER,
            step,
        ));
        self.console.push(
            floptle_script::LogLevel::Debug,
            "⚖ referee running — a second simulation of this match, at the confirmed \
             frontier only. It never guesses, so its result is the authoritative one, and \
             every peer's checksum is judged against it rather than against a quorum."
                .into(),
            None,
        );
    }

    /// Host, once per tick: feed the referee whatever inputs arrived, let it
    /// catch up, and publish its verdict for any confirmed tick it reaches.
    fn net_referee_tick(&mut self) {
        let Some(mut r) = self.net_referee.take() else { return };
        if let Some(s) = self.net_server.as_mut() {
            for e in s.take_log_entries() {
                r.log.record(e.peer, e.tick, &e.input);
            }
        }
        // Capped: a referee catching up after a hitch must not stall the frame
        // it is catching up in. It is allowed to be late; it is not allowed to
        // be the reason the game stutters.
        r.advance(crate::shadow::Horizon::Confirmed, REFEREE_CATCHUP_TICKS);
        let due = r.tick() - r.tick() % floptle_net::CHECKSUM_EVERY;
        if due > self.net_referee_reported
            && due > 0
            && let Some(hash) = r.state_hash(due)
        {
            self.net_referee_reported = due;
            if let Some(s) = self.net_server.as_mut() {
                s.set_referee_hash(due, hash);
            }
        }
        self.net_referee = Some(r);
        let faults = self
            .net_server
            .as_mut()
            .map(|s| s.take_referee_faults())
            .unwrap_or_default();
        for (tick, peer) in faults {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "⚖ REFEREE: peer {peer}'s state at tick {tick} disagrees with the \
                     authoritative simulation. Either that machine desynced, or it is not \
                     running the game everyone else is. The referee's result is the real one."
                ),
                None,
            );
        }
    }

    /// Write the match's input log out as a replay. Inputs and the seed ARE the
    /// match, so this is kilobytes for a full set and playback is not playback —
    /// it is running the match again.
    fn net_save_replay(&mut self) {
        let Some(log) = self.net_server.as_mut().and_then(|s| s.take_recording()) else {
            return;
        };
        if log.entries.is_empty() {
            return;
        }
        let dir = crate::shadow::replay_dir(&self.project_root);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("replay not saved: {e}"),
                None,
            );
            return;
        }
        // Named by seed: it is already unique per match, it is in the file, and
        // it does not need a clock — which is the one thing this feature is not
        // allowed to depend on.
        let path = dir.join(format!("match-{:016x}.floptlereplay", log.seed));
        match std::fs::write(&path, log.to_ron()) {
            Ok(()) => self.console.push(
                floptle_script::LogLevel::Debug,
                format!(
                    "🎞 replay saved — {} ({} inputs over {} ticks). Inputs and the seed are \
                     the match, so playing it back re-simulates rather than re-enacts.",
                    path.display(),
                    log.entries.len(),
                    log.last_tick(),
                ),
                None,
            ),
            Err(e) => self.console.push(
                floptle_script::LogLevel::Warn,
                format!("replay not saved to {}: {e}", path.display()),
                None,
            ),
        }
    }

    /// Play a recorded match back in a headless second world, and report
    /// whether it reproduced. Used by the ⚖ panel button and by tests.
    pub(crate) fn net_play_replay(&mut self, path: &std::path::Path) {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("replay {}: {e}", path.display()),
                    None,
                );
                return;
            }
        };
        let log = match floptle_net::InputLog::from_ron(&text, self.input_map_hash()) {
            Ok(l) => l,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("replay {}: {e}", path.display()),
                    None,
                );
                return;
            }
        };
        let Some(doc) = self.net_scene_doc.clone().or_else(|| {
            self.playing.then(|| floptle_scene::to_doc("replay", &self.world))
        }) else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "replay: enter Play on the match's scene first — a replay is re-simulated, \
                 so it needs the world it was played in"
                    .into(),
                None,
            );
            return;
        };
        let (dir, map, step) = (
            self.scripts_dir(),
            self.script_host.input_system().borrow().map().clone(),
            self.game_tick.step,
        );
        let ticks = log.last_tick();
        let mut sh = crate::shadow::ShadowSim::build(
            &doc,
            &dir,
            map,
            log,
            floptle_net::SERVER,
            step,
        );
        sh.advance(crate::shadow::Horizon::WholeLog, u64::MAX);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "🎞 replay complete — {} of {ticks} tick(s) re-simulated{}",
                sh.tick(),
                match sh.state_hash(sh.tick().saturating_sub(1)) {
                    Some(h) => format!(", final checksum {h:016x}"),
                    None => String::new(),
                }
            ),
            None,
        );
        self.net_replay = Some(sh);
    }

    /// Hand the bodies back and forget the session (Stop, `net.leave()`, the
    /// host going away).
    pub(crate) fn net_rollback_stop(&mut self) {
        self.net_save_replay();
        self.net_referee = None;
        self.net_referee_reported = 0;
        let Some(mut d) = self.net_rollback.take() else { return };
        // Take back exactly the half of the filters the driver added. `net_stop`
        // clears both wholesale afterwards and would not have needed this, but a
        // scene switch does NOT — and an entity index left behind here is one
        // the allocator hands to an unrelated node in the next scene, whose
        // scripts would then quietly never run.
        self.script_host.shrink_filters(d.eids());
        if let Some(sim) = self.sim.as_mut() {
            d.release(sim);
        }
    }

    /// Ship one applied-tick input to the rest of the session.
    fn net_rollback_send(&mut self, applied: u64, input: NetInput) {
        if let Some(s) = self.net_server.as_mut() {
            s.push_rollback_input(applied, input);
        } else if let Some(c) = self.net_play_client.as_mut() {
            c.send_rollback_input(applied, input);
        }
    }

    /// One gameplay tick of a rollback session, in place of the tick domain's
    /// device resolve and the rollback nodes' share of the global passes.
    ///
    /// A stall (§2.3) is not an error and does not abort the rest of the tick:
    /// the fighters wait for input while the camera, the UI and everything else
    /// in the scene keep running. `RollbackDriver::stalled` is what the panel's
    /// indicator reads.
    pub(crate) fn net_rollback_tick(&mut self, game_focused: bool) {
        if self.net_rollback.is_none() {
            return;
        }
        // 0. Hosting: pull whatever arrived since the last tick BEFORE draining,
        //    the same tick-start pump the remote-Predicted path does. Without
        //    it an input that landed during the frame waits a whole tick, and
        //    every tick it waits is one more tick to re-simulate when it turns
        //    out to contradict the guess.
        if let Some(s) = self.net_server.as_mut() {
            s.pump_server(&self.world, self.game_tick_no);
        }
        // 1. Remote inputs first: a correction resolved before this tick costs
        //    one replay, the same one resolved after costs two.
        let incoming = match (self.net_server.as_mut(), self.net_play_client.as_mut()) {
            (Some(s), _) => s.take_rollback_inputs(),
            (_, Some(c)) => c.take_rollback_inputs(),
            _ => Vec::new(),
        };
        let local = self.net_rollback.as_ref().map(|d| d.net.local()).unwrap_or(SERVER);
        if let Some(d) = self.net_rollback.as_mut() {
            for (peer, applied, input) in incoming {
                if peer == local {
                    continue;
                }
                d.add_remote(peer, applied, input);
            }
        }
        // 2. Sample our own pad for the tick about to run, off the sampling
        //    runtimes — the tick domain belongs to the driver, and resolving
        //    through it here would advance input history twice per tick.
        //
        //    `sample_tick` returns None while stalled — a tick may only ever be
        //    sampled once (see its docs). A stalled frame therefore banks and
        //    sends nothing, and leaves the pad's edges UNDRAINED on purpose:
        //    press something during a stall and it lands on the first tick that
        //    actually runs, instead of being eaten by a frame that went nowhere.
        if let Some(sampled) = self.net_rollback.as_ref().and_then(|d| d.sample_tick()) {
            let slot = self
                .net_rollback
                .as_ref()
                .and_then(|d| d.slot_of(local))
                .unwrap_or(0);
            let ni = self.sample_local_net_input(slot, game_focused);
            let Some(applied) = self.net_rollback.as_mut().map(|d| d.add_local(sampled, ni.clone()))
            else {
                return;
            };
            self.net_rollback_send(applied, ni);
        }

        // 3. Advance — resolving any banked correction first.
        let step = self.game_tick.step;
        let mut d = self.net_rollback.take().expect("checked above");
        {
            let Some(sim) = self.sim.as_mut() else {
                self.net_rollback = Some(d);
                return;
            };
            let mut ctx = Ctx { world: &mut self.world, sim, host: &mut self.script_host, step };
            d.advance(&mut ctx);
        }
        // The tick loop's own passes read the frame's raw snapshot; the driver
        // left it holding the last fighter's aim.
        self.script_host.set_input(self.last_tick_input.clone());

        // 4. Checksums (§6) and the faults that must not be swallowed.
        if let Some((tick, hash)) = d.due_checksum() {
            if let Some(s) = self.net_server.as_mut() {
                s.send_state_hash(tick, hash);
            } else if let Some(c) = self.net_play_client.as_mut() {
                c.send_state_hash(tick, hash);
            }
        }
        for f in d.faults.drain(..) {
            self.console.push(floptle_script::LogLevel::Error, f, None);
        }
        self.net_rollback = Some(d);
        self.net_referee_tick();
        self.net_rollback_report_desyncs();
    }

    /// This tick's local actions in wire form, sampled without disturbing the
    /// tick domain.
    ///
    /// An unfocused game view samples neutral for the same reason the ordinary
    /// resolve does — you are editing, not playing — but the banked edges are
    /// still drained, so a key pressed while editing doesn't fire the moment
    /// play regains focus. In a rollback session that matters more than usual:
    /// a stray input is not a stray input, it is one the opponent's machine
    /// also simulates.
    fn sample_local_net_input(&mut self, slot: u8, game_focused: bool) -> NetInput {
        let mut raw = if game_focused {
            self.raw_input.clone()
        } else {
            floptle_input::RawInput::default()
        };
        raw.pressed = std::mem::take(&mut self.tick_input_edges.0);
        raw.released = std::mem::take(&mut self.tick_input_edges.1);
        if !game_focused {
            raw.pressed.clear();
            raw.released.clear();
        }
        let sys = self.script_host.input_system().clone();
        let step = self.game_tick.step;
        let state = sys.borrow_mut().sample_tick(&raw, slot, step);
        floptle_script::input_to_net(&state, self.last_tick_input.aim)
    }

    /// Surface a desync the way §6 insists on: loudly, naming the tick, on
    /// every peer, and reaching the game through `net.on("desync")` so a match
    /// can end honestly rather than play out as two different fights.
    fn net_rollback_report_desyncs(&mut self) {
        let ticks = match (self.net_server.as_mut(), self.net_play_client.as_mut()) {
            (Some(s), _) => s.take_desyncs(),
            (_, Some(c)) => c.take_desyncs(),
            _ => Vec::new(),
        };
        for tick in ticks {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "🥊 DESYNC at tick {tick} — the peers' simulations no longer agree. From \
                     here the two machines are playing different matches. Usual causes: a \
                     gameplay value outside snapshot()/restore(), an unseeded rng(), or a \
                     read of node.x inside fixedUpdate (that is the interpolated render pose \
                     — use node.tickPos)."
                ),
                None,
            );
            if let Some(d) = self.net_rollback.as_mut() {
                d.desynced = true;
            }
            self.script_host.fire_net_event(&mut self.world, "desync", None, None);
        }
    }
}

/// What the 🌐 panel and `net.rollback*` report about a live rollback session.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RollbackStats {
    pub corrections: u64,
    pub last_depth: u32,
    pub max_depth_seen: u32,
    pub average_depth: f32,
    pub mispredict_rate: f32,
    pub input_delay: u8,
    pub ring_ticks: usize,
    pub ring_bytes: usize,
    pub stalled: bool,
    pub fighters: usize,
    /// The newest confirmed tick whose checksum this peer has published, and
    /// whether a desync has ever been reported. "Never checked" and "checked,
    /// agreeing" are very different states to be in, so the panel says which.
    pub checksum_tick: u64,
    pub desynced: bool,
}

impl RollbackStats {
    /// Read a live driver's counters. A free constructor rather than an
    /// `Editor` method because the panel builds it mid-render, with half the
    /// editor already mutably borrowed for the GPU.
    pub(crate) fn of(d: &RollbackDriver) -> Self {
        Self {
            corrections: d.net.corrections,
            last_depth: d.net.last_depth,
            max_depth_seen: d.net.max_depth_seen,
            // What a player actually felt: total ticks re-simulated per
            // correction. `max_depth_seen` is the worst moment; this is the
            // texture of the connection.
            average_depth: if d.net.corrections == 0 {
                0.0
            } else {
                d.resimulated_ticks as f32 / d.net.corrections as f32
            },
            mispredict_rate: d.net.mispredict_rate(),
            input_delay: d.net.delay(),
            ring_ticks: d.ring_depth(),
            ring_bytes: d.ring_bytes(),
            stalled: d.stalled,
            fighters: d.nodes().len(),
            checksum_tick: d.last_checksum(),
            desynced: d.desynced,
        }
    }
}

#[cfg(test)]
mod tests {
    /// Player-facing sentences must not carry a flattened line continuation.
    ///
    /// A multi-line string in Rust keeps the newline AND the source indentation
    /// unless the line ends in `\`. Drop the backslash — or re-wrap a string
    /// that had one — and the message still compiles, still reads correctly in
    /// the source, and reaches the Console with a twenty-space hole punched
    /// through the middle of a sentence. Nothing in the toolchain says a word:
    /// not the compiler, not clippy, not a test that only checks behaviour.
    ///
    /// Four of these shipped in the referee and replay messages, which are the
    /// first thing a developer sees when they use either feature. This is the
    /// check that would have caught them.
    #[test]
    fn console_messages_have_no_flattened_continuations() {
        let files = [
            ("rollback_session.rs", include_str!("rollback_session.rs")),
            ("rollback.rs", include_str!("rollback.rs")),
            ("shadow.rs", include_str!("shadow.rs")),
            ("net.rs", include_str!("net.rs")),
        ];
        let mut bad = Vec::new();
        for (name, src) in files {
            for (n, line) in src.lines().enumerate() {
                if !line.contains('"') {
                    continue;
                }
                // A run of spaces sitting mid-sentence: text, a gap wider than
                // any real one, then more text. Leading indentation is skipped
                // (it starts the line), so only interior gaps can match.
                let b = line.as_bytes();
                for i in 0..b.len() {
                    let run = b[i..].iter().take_while(|c| **c == b' ').count();
                    if run < 4 || i == 0 {
                        continue;
                    }
                    let before = b[i - 1];
                    let after = b.get(i + run).copied().unwrap_or(b' ');
                    if (before.is_ascii_alphanumeric() || b",.;".contains(&before))
                        && after.is_ascii_alphabetic()
                    {
                        bad.push(format!("{name}:{}", n + 1));
                        break;
                    }
                }
            }
        }
        assert!(
            bad.is_empty(),
            "these lines put a run of spaces inside a sentence — a string continuation lost \
             its trailing `\\`, and the gap reaches the player's Console: {bad:?}"
        );
    }
}
