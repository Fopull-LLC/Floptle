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

/// How many ticks the replay audit re-simulates (floptle/0050). Four, matching
/// the offline harness that found the Fofighter instance: deep enough that a
/// value cached across hooks has been read again, shallow enough that the extra
/// simulation is a rounding error on the frame.
const ROLLBACK_AUDIT_DEPTH: u64 = 4;
/// …and how often. Twice a second: the fault it hunts is structural, so a
/// script that reads an un-restored value does it in the first exchange.
const ROLLBACK_AUDIT_EVERY: u64 = 30;

use crate::rollback::{Ctx, RollbackDriver};
use crate::Editor;

/// Which `Rollback` nodes are being run by NOTHING — not the driver, and not
/// the global script passes.
///
/// `reps` is `(entity, is_a_rollback_node)` for every `Replicated` entity,
/// `driven` is the live driver's node set, and `filtered` answers "is this
/// entity excluded from the global passes". A node in neither is a node whose
/// scripts never tick.
///
/// A free function, like [`crate::net::plan_client_side`] and for the same
/// reason: the sequence that produces this state spans a scene switch, a
/// client-side setup and a rollback start, and it cannot be driven through an
/// `Editor` in a test. floptle/0040 is what that costs.
pub(crate) fn orphaned_rollback_nodes(
    reps: &[(floptle_core::Entity, bool)],
    driven: &std::collections::HashSet<u32>,
    filtered: impl Fn(u32) -> bool,
) -> Vec<floptle_core::Entity> {
    reps.iter()
        .filter(|(_, is_rollback)| *is_rollback)
        .map(|(e, _)| *e)
        .filter(|e| !driven.contains(&e.index()) && filtered(e.index()))
        .collect()
}

/// The sibling fault the orphan check cannot see: a node the driver DOES own,
/// but which also sits in the snapshot filter that gates every pass.
///
/// The orphan check asks "does somebody run your ticks". This asks "does
/// somebody run your *passes*" — and the answer differs, because the driver
/// replays `fixedUpdate` and `update` and nothing replays `lateUpdate`. A node
/// in the all-passes filter therefore loses its late pass with no error and no
/// log line, offline behaviour perfect, net play silently wrong. floptle/0042.
pub(crate) fn late_starved_rollback_nodes(
    reps: &[(floptle_core::Entity, bool)],
    driven: &std::collections::HashSet<u32>,
    snapshot_filtered: impl Fn(u32) -> bool,
) -> Vec<floptle_core::Entity> {
    reps.iter()
        .filter(|(_, is_rollback)| *is_rollback)
        .map(|(e, _)| *e)
        .filter(|e| driven.contains(&e.index()) && snapshot_filtered(e.index()))
        .collect()
}

impl Editor {
    /// The live driver's node ids — the set that belongs in the DRIVER filter
    /// for as long as it is running those nodes' ticks itself. Not the snapshot
    /// filter: that one gates `lateUpdate` too, which no driver replays
    /// (floptle/0042).
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
        // Give back the filters the OUTGOING driver held, before anything can
        // abandon it. Every `return` below drops `d`, and a dropped driver whose
        // eids are still in the script filters is a node nothing runs: not the
        // driver (gone) and not the global passes (skipping it). Its scripts
        // then sit un-ticked for the life of the match, silently, which is
        // exactly what a joiner and a local bot match both did (floptle/0039).
        // Re-added below only once the driver is actually installed.
        self.script_host.shrink_filters(d.eids());
        let Some(sim) = self.sim.as_mut() else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "rollback: no physics sim yet — enter Play before starting the match".into(),
                None,
            );
            return;
        };
        // Hand the bodies back too. `rebind` re-takes whatever it binds, but on
        // the abandon paths nothing does, and a body left driven by a driver
        // that no longer exists never steps again.
        sim.set_driven_bodies(&std::collections::HashSet::new());
        d.rebind(&self.world, sim, &self.script_host);
        if d.nodes().is_empty() {
            // Nothing to drive; the session is an ordinary one. Not silent —
            // the host announced a match, so a scene with no Rollback nodes on
            // THIS machine means the two projects disagree about the scene.
            for f in d.faults.drain(..) {
                self.console.push(floptle_script::LogLevel::Warn, f, None);
            }
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "⚔ a rollback match was announced (peers {peers:?}) but scene \
                     \"{}\" has no Rollback nodes here — this machine will simulate \
                     nothing while the others fight. Check the scene loaded, and that its \
                     fighters carry Replicated with mode Rollback.",
                    self.scene_name,
                ),
                None,
            );
            return;
        }
        // The driver runs these nodes' TICKS itself, in its own order — so they
        // leave the global `fixedUpdate` and `update` passes. Their `lateUpdate`
        // stays on the global pass: no driver replays it, and a rollback frame
        // runs many ticks, so replaying it would fire it N times (floptle/0042).
        // `run_*_for` bypasses every filter, which is the same arrangement the
        // host already uses for remote-owned Predicted nodes. ADDED to the
        // driver filter, never assigned over the session's own: on a client the
        // session is already skipping every authority-driven node.
        //
        // …but FIRST take them out of the session's sets. On a client,
        // `net_client_side_setup` ran at join time and again at Welcome, and
        // both of those are structurally BEFORE this moment — so
        // `rollback_filter_eids()` was empty for them and every fighter landed
        // in `script_skip`. That set gates every pass INCLUDING `lateUpdate`,
        // which no driver replays, and nothing else ever removes them. The
        // fight then runs normally (the driver bypasses filters) while the
        // cosmetic pass is silently dead on the client only — which is why
        // floptle/0042 looked fixed from the host and was reported three times.
        self.script_host.shrink_filters(d.eids());
        self.script_host.extend_filters(d.eids());
        // And tell the SESSION which nodes the driver owns, so its snapshot
        // guards stop depending on a flag a scene message can clear
        // (floptle/0048). Refreshed every frame as well; done here too so the
        // opening frame of a match is already right.
        self.net_publish_driven(&d.eids());
        // A new match: both once-per-session diagnostics arm again.
        self.net_flow_reported = 0;
        self.net_rollback_orphans_checked = false;
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
                "⚔ rollback match started — {n} fighter(s), {delay}-tick input delay, \
                 peers {peers:?} in slot order"
            ),
            None,
        );
    }

    /// Should the replay audit run this tick? (floptle/0050)
    ///
    /// **On in the editor, off in a shipped build**, and forced either way by
    /// `FLOPTLE_ROLLBACK_AUDIT=1` / `=0`. It costs an extra
    /// [`ROLLBACK_AUDIT_DEPTH`] ticks of simulation each time it fires, which
    /// is a fine trade while you are building the game and not one to make a
    /// player pay for.
    fn net_rollback_audit_due(&self) -> bool {
        let on = match std::env::var("FLOPTLE_ROLLBACK_AUDIT").ok().as_deref() {
            Some("1" | "true" | "on") => true,
            Some(_) => false,
            None => !self.player_mode,
        };
        on && self.game_tick_no.is_multiple_of(ROLLBACK_AUDIT_EVERY)
    }

    /// Report an audit's findings — LOCAL, and deliberately not a desync.
    ///
    /// Nothing has gone wrong between the peers yet: this machine is about to
    /// be wrong on its own. Ending the match to say so would be the desync
    /// detector's mistake repeated — it fires on the wrong machine, at the
    /// wrong time, about a value that has already been overwritten.
    fn net_rollback_report_replay_divergence(&mut self, diverged: Vec<(u64, String, u64, u64)>) {
        if diverged.is_empty() {
            return;
        }
        for (tick, label, before, after) in diverged.iter().take(6) {
            // Once per VALUE per session. The same script reads the same
            // un-restored thing every correction, and sixty identical lines a
            // second is a diagnostic nobody reads.
            if !self.net_replay_audit_reported.insert(label.clone()) {
                continue;
            }
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "⚖ replay divergence at tick {tick} — {label}: {before:016x} → {after:016x}. \
                     Re-simulating that tick from the state ring, with the SAME inputs, produced \
                     a different value. No network condition explains that: something the \
                     simulation reads is not in this node's snapshot() — a Lua local cached \
                     across hooks, a value on another script, a global. This peer will desync \
                     eventually and the other machine will be the one to notice."
                ),
                None,
            );
        }
        // The game hears about it too, with the value named — a lobby can put
        // "this build is broken" on screen rather than voiding a match later.
        if let Some((tick, label, ..)) = diverged.first() {
            let reason = format!("{label} (tick {tick})");
            self.script_host.fire_net_event(
                &mut self.world,
                "replayDiverged",
                None,
                Some(&reason),
            );
        }
    }

    /// The input delay this match will run at (floptle/0049).
    ///
    /// The game's choice if it made one, otherwise derived from the link. Two
    /// ticks — 33 ms — was the constant, and it is right only for peers in the
    /// same building: past that the driver mispredicts on essentially every
    /// tick and re-simulates its way through the frame budget, correctly and
    /// unplayably. A host that has been up long enough to have pinged its peers
    /// already knows the number.
    ///
    /// Still FIXED for the session and never auto-adjusted mid-match. Adaptive
    /// delay hides a bad connection by changing how the game feels while you
    /// are playing it, which a fighting game cannot tolerate. This chooses the
    /// starting value informed; it does not keep choosing.
    pub(crate) fn net_choose_input_delay(&self) -> u8 {
        if let Some(n) = self.net_input_delay {
            return n.min(floptle_net::MAX_DELAY);
        }
        let worst = self
            .net_server
            .as_ref()
            .map(|s| s.peer_rtts().into_iter().map(|(_, ms)| ms).fold(0.0f32, f32::max))
            .unwrap_or(0.0);
        Self::delay_for_rtt(worst)
    }

    /// `ceil(worst one-way / tick) + 1`, clamped.
    ///
    /// The `+ 1` is the tick the input still has to wait for after it arrives.
    /// A LAN (sub-16 ms RTT) comes out at 2, the constant that was there; the
    /// 112 ms link in the field report comes out at 5.
    pub(crate) fn delay_for_rtt(rtt_ms: f32) -> u8 {
        const TICK_MS: f32 = 1000.0 / 60.0;
        let one_way = (rtt_ms.max(0.0)) * 0.5;
        let ticks = (one_way / TICK_MS).ceil() as u32 + 1;
        ticks.clamp(
            u32::from(floptle_net::DEFAULT_INPUT_DELAY),
            u32::from(floptle_net::MAX_DELAY),
        ) as u8
    }

    /// Tell every play-world session which nodes the local driver simulates.
    ///
    /// The session already asks the question a second way (its own `rollback`
    /// flag plus the node's `Replicated.mode`), and the two answers can come
    /// apart: a `Msg::Scene` clears the flag, `RollbackStart` sets it, and the
    /// driver is installed a frame later from a queued signal. Any ordering
    /// that leaves the flag off while the driver is on turns off every guard
    /// that stops a snapshot pose landing on a locally-simulated fighter — and
    /// the symptom is a fighter facing the wrong way with a green checksum,
    /// because rotation is deliberately not hashed (floptle/0048).
    ///
    /// This set comes from the driver, so it cannot disagree with the driver.
    pub(crate) fn net_publish_driven(&mut self, eids: &std::collections::HashSet<u32>) {
        for s in [self.net_play_client.as_mut(), self.net_server.as_mut()].into_iter().flatten() {
            s.set_locally_driven(eids.clone());
        }
    }

    /// Drain the "a snapshot was refused because the driver owns this node"
    /// counter into the Console, once per node per session.
    ///
    /// The refusal is correct. Reaching this code at all is not: it means the
    /// ingest guard let a sample through, which means the two answers to "is
    /// this node locally driven" disagreed at the moment it arrived. This is
    /// the detector floptle/0048 asked for, and it fires on the machine that
    /// has the problem, while it has it.
    pub(crate) fn net_report_driven_drops(&mut self) {
        let mut drops: Vec<(u32, u64)> = Vec::new();
        for s in [self.net_play_client.as_mut(), self.net_server.as_mut()].into_iter().flatten() {
            drops.extend(s.take_driven_snapshot_drops());
        }
        for (eid, n) in drops {
            if !self.net_driven_drop_reported.insert(eid) {
                continue;
            }
            let name = self
                .world
                .query::<floptle_core::Name>()
                .find(|(e, _)| e.index() == eid)
                .map(|(_, n)| n.0.clone())
                .unwrap_or_else(|| format!("#{eid}"));
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "⚔ \"{name}\" is simulated by the rollback driver here, but {n} snapshot \
                     sample(s) of it got past the ingest guard and would have been written over \
                     its transform. They were dropped. The session's rollback flag and the \
                     driver disagreed — the fight is still correct, but this node's rotation \
                     would have been wrong on this machine only, and the checksum does not \
                     hash rotation."
                ),
                None,
            );
        }
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
        let delay = self.net_choose_input_delay();
        // The seed is drawn ONCE, here, from the wall clock — the only place in
        // the whole feature where a clock is allowed near the simulation. From
        // this moment it is replicated state like any other, and every draw
        // comes from (seed, tick, index).
        let seed = floptle_core::time::SystemTime::now()
            .duration_since(floptle_core::time::UNIX_EPOCH)
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
    /// them (`docs/multiplayer.md` §5).
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
        self.net_referee = Some(crate::shadow::ShadowSim::build_with(
            &doc,
            &dir,
            map,
            log,
            floptle_net::SERVER,
            step,
            |w| self.build_sim_for_world(w),
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
        // The other verdict, and the one that used to end matches: the referee
        // against the field. A cheat changes one machine; a referee fault
        // changes only the referee, so everybody disagreeing with it and
        // nobody disagreeing with each other means IT is wrong. The match keeps
        // going and this says why (floptle/0041).
        let outliers =
            self.net_server.as_mut().map(|s| s.take_referee_outliers()).unwrap_or_default();
        for tick in outliers {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "⚖ REFEREE disagrees with EVERY peer at tick {tick}, while the peers all \
                     agree with each other — so the referee is the one that is wrong, not the \
                     match. Play continues and no desync is raised. This is an engine or \
                     content fault in the second simulation (its physics, its scripts, or its \
                     scene) and it is worth reporting; the players are fine."
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
        if let Err(e) = floptle_vfs::create_dir_all(&dir) {
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
        match floptle_vfs::write(&path, log.to_ron()) {
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
        let text = match floptle_vfs::read_to_string(path) {
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
        let mut sh = crate::shadow::ShadowSim::build_with(
            &doc,
            &dir,
            map,
            log,
            floptle_net::SERVER,
            step,
            |w| self.build_sim_for_world(w),
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
        self.net_flow_reported = 0;
        self.net_rollback_orphans_checked = false;
        self.net_driven_drop_reported.clear();
        // Hand the session's guard set back too, for the same reason the
        // filters go back: these are entity INDICES, and the allocator reuses
        // them. A stale one here would make the next scene's unrelated node
        // silently refuse its snapshots.
        self.net_publish_driven(&std::collections::HashSet::new());
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
        // Re-publish who the driver owns. Once per tick rather than once per
        // match, because the driver rebinds when nodes spawn or despawn and
        // because a session-side flag can be cleared underneath us at any
        // moment (floptle/0048). It is a set compare in the common case.
        let driven = self.rollback_filter_eids();
        self.net_publish_driven(&driven);
        self.net_report_driven_drops();
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
            // The DEVICE slot, not the roster slot — see
            // `RollbackDriver::local_device_slot`. The input is applied to the
            // roster slot on every machine (the driver does that from `local`),
            // but it is READ from this machine's own player-one hardware and
            // bindings. Sampling by roster slot handed a joiner the couch's
            // player-two layout, and a joiner on a gamepad nothing whatsoever.
            let slot = self
                .net_rollback
                .as_ref()
                .map(|d| d.local_device_slot())
                .unwrap_or(0);
            let ni = self.sample_local_net_input(slot, game_focused);
            let Some(applied) = self.net_rollback.as_mut().map(|d| d.add_local(sampled, ni.clone()))
            else {
                return;
            };
            self.net_rollback_send(applied, ni);
        }

        // 3. Advance — resolving any banked correction first.
        //
        // ⚠ From this `take` to the restore at the bottom there is NO early
        // return, deliberately. An exit that skips the restore DROPS the driver
        // — and a dropped driver leaves its fighters in the script filters with
        // nothing running them, for the rest of the match, with no error. That
        // is floptle/0040: the fighters ticked exactly once, then the driver
        // fell out of the editor at the end of its own first tick. If you need
        // to bail below, set a flag and bail after the restore.
        let step = self.game_tick.step;
        let mut d = self.net_rollback.take().expect("checked above");
        if let Some(sim) = self.sim.as_mut() {
            let mut ctx = Ctx { world: &mut self.world, sim, host: &mut self.script_host, step };
            d.advance(&mut ctx);
        }
        // The tick loop's own passes read the frame's raw snapshot; the driver
        // left it holding the last fighter's aim.
        self.script_host.set_input(self.last_tick_input.clone());

        // Publish our confirmed frontier. This is what tells the host it may
        // stop re-sending a tick — and a session where nobody reports one keeps
        // every unconfirmed tick forever, because the host has no way to know.
        // It is also half the answer to "which side is starved", which used to
        // cost a replay-file autopsy (floptle/0039).
        let confirmed = d.net.confirmed();
        if let Some(s) = self.net_server.as_mut() {
            s.set_rollback_confirmed(confirmed);
        } else if let Some(c) = self.net_play_client.as_mut() {
            c.set_rollback_confirmed(confirmed);
        }

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
        // The script-level audit, deferred out of `rebind` until the bound
        // nodes' environments exist (`RollbackDriver::audit`). A no-op once it
        // has run. `d` is still the owned driver from step 3 — it is put back
        // below, on the one path that reaches here.
        d.audit(&self.world, &self.script_host);
        for f in d.faults.drain(..) {
            self.console.push(floptle_script::LogLevel::Warn, f, None);
        }
        // 5. The replay audit (floptle/0050): re-simulate the last few ticks
        //    from the ring with provably identical inputs and check the world
        //    comes out the same. Anything that doesn't is a value the
        //    simulation reads and `snapshot()` does not carry.
        //
        //    On a slow cadence — it is a second simulation of those ticks, and
        //    the fault it hunts is structural, not intermittent: a script that
        //    caches an un-restored value does it every match, so twice a second
        //    finds it inside the first exchange.
        let audit = self.net_rollback_audit_due();
        if audit && let Some(sim) = self.sim.as_mut() {
            let names: std::collections::HashMap<u32, String> = self
                .world
                .query::<floptle_core::Name>()
                .map(|(e, n)| (e.index(), n.0.clone()))
                .collect();
            let mut ctx = Ctx { world: &mut self.world, sim, host: &mut self.script_host, step };
            let diverged = d.audit_replay(&mut ctx, ROLLBACK_AUDIT_DEPTH, &names);
            self.net_rollback_report_replay_divergence(diverged);
        }
        self.net_rollback = Some(d);
        self.net_referee_tick();
        self.net_rollback_report_desyncs();
        self.net_rollback_report_flow();
        self.net_rollback_check_orphans();
    }

    /// Every `Rollback` node must be run by SOMETHING each tick: the driver, or
    /// the global script passes. Say so loudly when neither can.
    ///
    /// The failure this guards is invisible from Lua and invisible on screen:
    /// a node whose eid is in the script filters with no driver holding it
    /// simply never ticks. Its scripts' state stays at whatever the loader left
    /// it, cross-script calls into it read `nil` forever, and nothing anywhere
    /// says why. That is a match that looks frozen with a clean console — the
    /// state floptle/0039 was reported in.
    ///
    /// Checked once per session rather than per tick: the condition is
    /// structural, so repeating it sixty times a second would only bury it.
    fn net_rollback_check_orphans(&mut self) {
        if self.net_rollback_orphans_checked {
            return;
        }
        self.net_rollback_orphans_checked = true;
        let driven = self.rollback_filter_eids();
        let reps: Vec<(floptle_core::Entity, bool)> = self
            .world
            .query::<floptle_core::Replicated>()
            .map(|(e, r)| (e, r.mode.is_rollback()))
            .collect();
        let orphans: Vec<String> = orphaned_rollback_nodes(&reps, &driven, |eid| {
            self.script_host.is_filtered(eid)
        })
        .into_iter()
        .map(|e| {
            self.world
                .get::<floptle_core::Name>(e)
                .map(|n| n.0.clone())
                .unwrap_or_else(|| format!("#{}", e.index()))
        })
        .collect();
        let name = |e: floptle_core::Entity| {
            self.world
                .get::<floptle_core::Name>(e)
                .map(|n| n.0.clone())
                .unwrap_or_else(|| format!("#{}", e.index()))
        };
        // The pass-level sibling: owned by the driver, but ALSO in the filter
        // that gates every pass — so its `lateUpdate` runs nowhere.
        let starved: Vec<String> =
            late_starved_rollback_nodes(&reps, &driven, |eid| {
                self.script_host.is_snapshot_filtered(eid)
            })
            .into_iter()
            .map(name)
            .collect();
        if !starved.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!(
                    "⚔ {} Rollback node(s) are in the snapshot filter as well as the \
                     driver's: {}. The driver replays fixedUpdate and update but nothing \
                     replays lateUpdate, so their late pass runs NOWHERE — cosmetic work \
                     written there (model yaw, rim lights, camera offsets) silently stops \
                     in net play while offline stays perfect. This is an engine fault, not \
                     a project one; please report it with the console text.",
                    starved.len(),
                    starved.join(", "),
                ),
                None,
            );
        }
        if orphans.is_empty() {
            return;
        }
        self.console.push(
            floptle_script::LogLevel::Error,
            format!(
                "⚔ {} Rollback node(s) are being run by NOTHING this match — not the \
                 rollback driver and not the global script passes: {}. Their scripts will \
                 never tick, so anything asking them a question gets nil for the whole \
                 match. This is an engine fault, not a project one; please report it with \
                 the console text.",
                orphans.len(),
                orphans.join(", "),
            ),
            None,
        );
    }

    /// Name the value the peers disagreed about, once their breakdowns arrive.
    ///
    /// The breakdown crosses the wire AFTER the desync, so this runs on later
    /// frames until the reports are in. What it prints is the thing the ticket
    /// asked for and the checksum always knew:
    ///
    /// ```text
    /// ⚖ tick 4127 — Player2/fighterController/visYaw differs (peer 0: a3f1…, peer 1: a3f2…)
    /// ```
    fn net_rollback_name_the_divergence(&mut self) {
        let pending = std::mem::take(&mut self.net_desync_pending);
        let mut still = Vec::new();
        for tick in pending {
            let detail = match (self.net_server.as_mut(), self.net_play_client.as_mut()) {
                (Some(s), _) => s.take_desync_detail(tick),
                (_, Some(c)) => c.take_desync_detail(tick),
                _ => Vec::new(),
            };
            if detail.is_empty() {
                still.push(tick); // reports not in yet
                continue;
            }
            for (label, per_peer) in detail.iter().take(6) {
                let who: Vec<String> =
                    per_peer.iter().map(|(p, h)| format!("peer {p}: {:016x}", h)).collect();
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("⚖ tick {tick} — {label} differs ({})", who.join(", ")),
                    None,
                );
            }
            if detail.len() > 6 {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("⚖ …and {} more value(s) at tick {tick}", detail.len() - 6),
                    None,
                );
            }
        }
        // Give a straggling report a few frames before giving up on it.
        self.net_desync_pending = still;
        if self.net_desync_pending.len() > 8 {
            self.net_desync_pending.remove(0);
        }
    }

    /// Once a second while a match is stalled, say who we are waiting for.
    ///
    /// Silent while the match is healthy — a line per second in a working
    /// session is noise, and noise is what gets a diagnostic ignored. But a
    /// frozen match must never again be silent on both screens at once
    /// (floptle/0039): the whole failure was two machines showing the same
    /// frozen frame with nothing anywhere saying which one had stopped
    /// receiving.
    fn net_rollback_report_flow(&mut self) {
        let Some(d) = self.net_rollback.as_ref() else { return };
        if !d.stalled {
            self.net_flow_reported = 0;
            return;
        }
        // A stall of a few ticks is ordinary jitter absorption, not an event.
        // ~1 s of continuous waiting is not.
        let hz = (1.0 / self.game_tick.step).round().max(1.0) as u64;
        let tick = self.game_tick_no;
        if self.net_flow_reported != 0 && tick.saturating_sub(self.net_flow_reported) < hz {
            return;
        }
        let first = self.net_flow_reported == 0;
        self.net_flow_reported = tick;
        if first {
            // The first second of a stall is normal on a rough link. Start the
            // clock, say nothing yet.
            return;
        }
        let (confirmed, current) = (d.net.confirmed(), d.net.current());
        let session = self.net_server.as_ref().or(self.net_play_client.as_ref());
        let who = match session.map(|s| s.rollback_frontiers()) {
            Some(f) if !f.is_empty() => f
                .iter()
                .map(|(p, t)| {
                    let name =
                        if *p == SERVER { "host".to_string() } else { format!("peer {p}") };
                    format!("{name} at {t}")
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "no peer frontiers reported".to_string(),
        };
        let role = if self.net_server.is_some() { "host" } else { "client" };
        self.console.push(
            floptle_script::LogLevel::Warn,
            format!(
                "⚔ rollback STALLED — this machine ({role}) has simulated to tick {current} \
                 but only tick {confirmed} is confirmed, so it is waiting rather than \
                 guessing further. Frontiers: {who}. The peer whose frontier has stopped \
                 moving is the one not receiving."
            ),
            None,
        );
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
                    "⚔ DESYNC at tick {tick} — the peers' simulations no longer agree. From \
                     here the two machines are playing different matches. Usual causes: a \
                     gameplay value outside snapshot()/restore(), an unseeded rng(), a \
                     read of node.x inside fixedUpdate (that is the interpolated render pose \
                     — use node.tickPos), or a libm call (exp/log/sin/cos/pow) whose last \
                     bit differs between platforms."
                ),
                None,
            );
            // Post-mortem: publish OUR breakdown of the offending tick so the
            // host can name the value that actually diverged. Costs nothing in
            // a healthy session — it only ever runs once the match is lost.
            let names: std::collections::HashMap<u32, String> = self
                .world
                .query::<floptle_core::Name>()
                .map(|(e, n)| (e.index(), n.0.clone()))
                .collect();
            if let Some(d) = self.net_rollback.as_ref() {
                let detail = d.state_breakdown(tick, &names);
                if !detail.is_empty() {
                    match (self.net_server.as_mut(), self.net_play_client.as_mut()) {
                        (Some(s), _) => s.send_state_detail(tick, detail),
                        (_, Some(c)) => c.send_state_detail(tick, detail),
                        _ => {}
                    }
                }
            }
            if let Some(d) = self.net_rollback.as_mut() {
                d.desynced = true;
            }
            self.net_desync_pending.push(tick);
            self.script_host.fire_desync(&mut self.world, tick, None);
        }
        self.net_rollback_name_the_divergence();
    }
}

/// What the 🌐 panel and `net.rollback*` report about a live rollback session.
#[derive(Clone, Debug)]
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
    /// This peer's own confirmed frontier, and how far the local simulation has
    /// run past it. `current − confirmed` IS the stall: at the depth cap the
    /// driver stops rather than guess further.
    pub confirmed: u64,
    pub current: u64,
    /// Per peer: `(peer, their reported frontier, applied ticks we are still
    /// holding for them)`. Empty on a client, which only knows about itself.
    ///
    /// This is the readout floptle/0039 cost a replay-file autopsy for want of.
    /// A peer whose frontier has stopped moving while its backlog grows is the
    /// starved one, and it says so on the host's screen the moment it happens.
    pub peers: Vec<(floptle_net::PeerId, u64, usize)>,
}

impl RollbackStats {
    /// Read a live driver's counters, plus the session's per-peer view of input
    /// flow when there is a session to ask. A free constructor rather than an
    /// `Editor` method because the panel builds it mid-render, with half the
    /// editor already mutably borrowed for the GPU.
    pub(crate) fn with_session(
        d: &RollbackDriver,
        session: Option<&floptle_net::NetSession>,
    ) -> Self {
        let peers = session
            .map(|s| {
                let backlog: std::collections::HashMap<_, _> =
                    s.rollback_backlog().into_iter().collect();
                s.rollback_frontiers()
                    .into_iter()
                    .map(|(p, f)| (p, f, backlog.get(&p).copied().unwrap_or(0)))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            confirmed: d.net.confirmed(),
            current: d.net.current(),
            peers,
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
    use super::{late_starved_rollback_nodes, orphaned_rollback_nodes};
    use std::collections::HashSet;

    /// FIELD REGRESSION (floptle/0042): the pass-level sibling of the orphan
    /// check. A node the driver owns is NOT an orphan — somebody runs its ticks
    /// — so the orphan check is blind to it. But if it is also in the snapshot
    /// filter, its `lateUpdate` runs nowhere: the driver replays `fixedUpdate`
    /// and `update`, and nothing replays the late pass.
    ///
    /// In the field this drew both fighters at the same yaw for a whole match,
    /// with no error and no log line, while offline was perfect.
    #[test]
    fn a_driven_node_in_the_snapshot_filter_is_reported_as_late_starved() {
        use floptle_core::transform::Transform;
        use floptle_core::{Replicated, ReplicationMode, World};

        let mut world = World::default();
        let mut fighters = Vec::new();
        for _ in 0..2 {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, Replicated { mode: ReplicationMode::Rollback, ..Default::default() });
            fighters.push(e);
        }
        let reps: Vec<(floptle_core::Entity, bool)> =
            world.query::<Replicated>().map(|(e, r)| (e, r.mode.is_rollback())).collect();
        let driven: HashSet<u32> = fighters.iter().map(|e| e.index()).collect();

        // The fix: driven, and NOT in the all-passes filter.
        assert!(
            late_starved_rollback_nodes(&reps, &driven, |_| false).is_empty(),
            "a driver-owned node outside the snapshot filter keeps its late pass"
        );
        // And the orphan check agrees they are not orphans — which is exactly
        // why this second check has to exist.
        assert!(
            orphaned_rollback_nodes(&reps, &driven, |_| true).is_empty(),
            "the orphan check cannot see this class: somebody DOES run their ticks"
        );

        // The bug: driven, but also snapshot-filtered.
        let starved = late_starved_rollback_nodes(&reps, &driven, |_| true);
        assert_eq!(starved.len(), 2, "both fighters lose their late pass");

        // A node nobody drives is the ORPHAN case, not this one — the two
        // reports must not double-count the same fault.
        assert!(
            late_starved_rollback_nodes(&reps, &HashSet::new(), |_| true).is_empty(),
            "an undriven node is an orphan, and is reported by the other check"
        );
    }

    /// FIELD REGRESSION (floptle/0049): a delay derived from the link, not a
    /// constant. The constant was 2 — right for a LAN, and wrong for anyone
    /// playing across a country, which is most matches.
    #[test]
    fn the_default_delay_comes_from_the_measured_link() {
        use crate::Editor;
        // No session yet, or a LAN: the historic constant, unchanged.
        assert_eq!(Editor::delay_for_rtt(0.0), floptle_net::DEFAULT_INPUT_DELAY);
        assert_eq!(Editor::delay_for_rtt(8.0), 2, "same room");
        assert_eq!(Editor::delay_for_rtt(30.0), 2, "same city, still inside one tick each way");
        // The link two players in different houses actually measured.
        assert_eq!(Editor::delay_for_rtt(112.0), 5, "the field report's 112 ms");
        // And it never asks for more than the state ring can roll back to.
        assert_eq!(Editor::delay_for_rtt(2000.0), floptle_net::MAX_DELAY);
        const { assert!(floptle_net::MAX_DELAY >= floptle_net::DEFAULT_INPUT_DELAY) };
    }

    /// FIELD REGRESSION (floptle/0040): the client's join sequence must never
    /// leave a `Rollback` node filtered with no driver holding it.
    ///
    /// The sequence spans three steps that each own part of the answer, and the
    /// middle one runs while the driver does not exist:
    ///
    /// 1. scene switch → `net_rollback_stop()` — the old driver goes
    /// 2. `net_client_side_setup` → `plan_client_side` with an EMPTY rollback
    ///    set, which classifies the fighters as ordinary synced nodes and
    ///    filters them
    /// 3. `net_rollback_start` → `rebind` + `extend_filters` — the new driver
    ///    claims them back
    ///
    /// Step 2 is a genuine "filtered with no driver" window. It is fine because
    /// step 3 closes it in the same frame — but only if step 3 actually runs
    /// AND the driver it installs survives. It stopped surviving, and this is
    /// the shape that catches it.
    #[test]
    fn the_client_join_sequence_leaves_no_rollback_node_unrun() {
        use floptle_core::{Replicated, ReplicationMode, World};
        use floptle_core::transform::Transform;

        let mut world = World::default();
        let mut fighters = Vec::new();
        for _ in 0..2 {
            let e = world.spawn();
            world.insert(e, Transform::IDENTITY);
            world.insert(e, Replicated { mode: ReplicationMode::Rollback, ..Default::default() });
            fighters.push(e);
        }
        let reps: Vec<(floptle_core::Entity, bool)> = world
            .query::<Replicated>()
            .map(|(e, r)| (e, r.mode.is_rollback()))
            .collect();
        let all: Vec<(floptle_core::Entity, Replicated)> =
            world.query::<Replicated>().map(|(e, r)| (e, *r)).collect();

        // Step 2, with no driver: the fighters ARE filtered and nothing drives
        // them. The window is real — assert it, so the test is honest about
        // what it is checking rather than accidentally passing.
        let plan = crate::net::plan_client_side(&all, None, &HashSet::new());
        let filters = plan.skip.clone();
        assert!(
            !orphaned_rollback_nodes(&reps, &HashSet::new(), |e| filters.contains(&e)).is_empty(),
            "step 2 is supposed to be the un-driven window; if it isn't, this test proves nothing"
        );

        // Step 3: the driver binds them and re-adds its half of the filters.
        let driven: HashSet<u32> = fighters.iter().map(|e| e.index()).collect();
        let mut filters = filters;
        filters.extend(driven.iter().copied());
        assert!(
            orphaned_rollback_nodes(&reps, &driven, |e| filters.contains(&e)).is_empty(),
            "after the rollback start every fighter must be driven or unfiltered"
        );

        // And the failure the field actually hit: the driver was installed and
        // then dropped at the end of its own first tick, so `driven` went empty
        // while the filters stayed. Both fighters, both machines, silent.
        let lost = orphaned_rollback_nodes(&reps, &HashSet::new(), |e| filters.contains(&e));
        assert_eq!(
            lost.len(),
            2,
            "a driver that disappears must be detectable — that is what the check is for"
        );
    }

    /// `net_rollback_tick` takes the driver out of the `Editor` and must put it
    /// back on every path. A `return` between the two drops it.
    ///
    /// This is a source check because the thing it guards cannot be reached
    /// without a live `Editor`, and the cost of missing it is not a crash but
    /// silence: the fighters keep their place in the script filters, nothing
    /// runs them, and the match freezes with a clean console. That is exactly
    /// how floptle/0040 shipped — a `return` was not added, a re-`take()` was,
    /// and the single restore that used to follow it went away in the edit.
    #[test]
    fn the_rollback_tick_always_puts_its_driver_back() {
        let src = include_str!("rollback_session.rs");
        let body = src
            .split_once("pub(crate) fn net_rollback_tick(")
            .expect("net_rollback_tick exists")
            .1;
        let body = body.split_once("\n    /// ").map_or(body, |(b, _)| b);
        let take = body.find("self.net_rollback.take()").expect("it takes the driver");
        let after = &body[take..];
        assert_eq!(
            after.matches("self.net_rollback.take()").count(),
            1,
            "one take, or the second one finds a None the first one left behind"
        );
        assert_eq!(
            after.matches("self.net_rollback = Some(").count(),
            1,
            "exactly one restore — several means several paths, and paths are what get missed"
        );
        // A `return` after the take that is not preceded by a restore drops it.
        let restore = after.find("self.net_rollback = Some(").expect("it restores the driver");
        assert!(
            !after[..restore].contains("return"),
            "there is a `return` between taking the driver and putting it back — that path \
             drops it, and a dropped driver leaves its fighters filtered with nothing running \
             them for the rest of the match, silently (floptle/0040)"
        );
    }

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
