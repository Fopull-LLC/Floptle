//! In-editor multiplayer: drives `floptle-net` sessions from the play loop and
//! provides the **"Host & Join locally"** test harness (`docs/netcode-design.md`
//! §12/§13.3) — the play world hosts, a hidden ghost world joins over an
//! in-process [`floptle_net::MemoryHub`] with latency/loss sliders, and cyan
//! ghost gizmos show exactly what a remote client would see (interp delay,
//! loss stutter, the works) without a second editor instance.

use std::collections::HashMap;

use floptle_core::transform::Transform;
use floptle_core::{Entity, ReplicationMode, World};
use floptle_net::{NetEvent, NetSession, PredictedState, RpcTarget};
use floptle_physics::BodySnapshot;
use floptle_script::{NetCmd, NetRoleState, NetState};

use crate::Editor;

/// The hidden authoritative SERVER behind "Test as remote player": a full
/// second simulation (world + physics + its own Lua host) consuming the play
/// world's replayed inputs, exactly like a dedicated server would
/// (`docs/netcode-design.md` §6/§12 2c).
pub(crate) struct HiddenServer {
    pub session: NetSession,
    pub world: World,
    pub sim: floptle_physics::Sim,
    pub host: floptle_script::ScriptHost,
    /// The play-world client's peer id on this server.
    pub peer: floptle_net::PeerId,
    /// The next server tick to simulate. The server chases a target of
    /// `client_tick − (latency + 1)` computed LIVE from the slider: raising
    /// latency makes it pause while the input pipeline refills; lowering makes
    /// it catch up — so inputs labeled T always arrive before the server
    /// simulates tick T, and mid-session slider drags don't cause repeat-last
    /// mispredictions (= jitter) on the owner. 0 = not yet started.
    pub next_tick: u64,
}

impl Editor {
    /// Once per gameplay tick, after physics (`docs/netcode-design.md` §9):
    /// drain Lua session commands, advance the hub clock, run the server and
    /// ghost-client sessions, and dispatch received RPCs/events into scripts.
    pub(crate) fn net_tick(&mut self, tick: u64) {
        for cmd in self.script_host.take_net_commands() {
            match cmd {
                NetCmd::Host { .. } => self.net_start_hosting(),
                NetCmd::Join { addr } if addr.starts_with("local") => self.net_join_local(),
                NetCmd::Join { addr } => self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "net.join(\"{addr}\"): only local:// works in-editor today — \
                         network transports (QUIC + relay) land in phase 2e"
                    ),
                    None,
                ),
                NetCmd::Leave => self.net_stop("left the session"),
                NetCmd::Rpc { name, args, to } => {
                    if let Some(s) = self.net_server.as_mut() {
                        let target = to.map(RpcTarget::Peer).unwrap_or(RpcTarget::All);
                        if let Err(e) = s.send_rpc(&name, args, target) {
                            self.console.push(floptle_script::LogLevel::Warn, e, None);
                        }
                    } else if let Some(c) = self.net_play_client.as_mut() {
                        // On a client, rpcs are intents for the server.
                        if let Err(e) = c.send_rpc(&name, args, RpcTarget::Server) {
                            self.console.push(floptle_script::LogLevel::Warn, e, None);
                        }
                    }
                }
                NetCmd::Spawn { path, pos, owner } => self.net_spawn_path(&path, pos, owner),
                NetCmd::Despawn { eid } => {
                    if self.net_server.is_some() {
                        let ent = self
                            .world
                            .query::<Transform>()
                            .map(|(e, _)| e)
                            .find(|e| e.index() == eid);
                        if let (Some(s), Some(e)) = (self.net_server.as_mut(), ent) {
                            s.despawn(&mut self.world, e);
                        }
                    }
                }
            }
        }
        if let Some(hub) = &self.net_hub {
            hub.set_conditions(self.net_latency_ticks, self.net_loss);
            hub.set_now(tick);
        }
        // --- "Test as remote player" (2c): client prediction + hidden server ---
        if self.net_play_client.is_some() {
            self.net_client_tick(tick);
            self.net_hidden_tick(tick);
            return; // this mode owns the state mirror; 2b paths don't apply
        }
        // --- server: synced collection → tick → dispatch received RPC/events ---
        let hosting = self.net_server.is_some();
        let (rpcs, events) = if hosting {
            let raw = self.script_host.collect_synced();
            let ents: HashMap<u32, Entity> =
                self.world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
            let synced = raw
                .into_iter()
                .filter_map(|(eid, kind, vars)| ents.get(&eid).map(|e| (*e, kind, vars)))
                .collect();
            if let Some(s) = self.net_server.as_mut() {
                s.update_synced(synced);
                s.tick_server(&self.world, tick);
                (s.take_rpcs(), s.take_events())
            } else {
                (Vec::new(), Vec::new())
            }
        } else {
            (Vec::new(), Vec::new())
        };
        {
            for r in rpcs {
                self.script_host.dispatch_rpc(&mut self.world, &r.name, &r.args, r.sender);
            }
            for ev in events {
                match ev {
                    NetEvent::PeerJoined(p) => {
                        self.script_host.fire_net_event(&mut self.world, "playerJoined", Some(p), None)
                    }
                    NetEvent::PeerLeft(p) => {
                        self.script_host.fire_net_event(&mut self.world, "playerLeft", Some(p), None)
                    }
                    _ => {}
                }
            }
        }
        // --- ghost client: apply snapshots into its hidden world ---
        if let Some((c, cw)) = self.net_client.as_mut() {
            c.tick_client(cw);
            // The ghost world runs no scripts (it's a replication viewer) —
            // drain its inboxes so they don't grow.
            let _ = c.take_rpcs();
            let _ = c.take_events();
            let _ = c.take_synced();
        }
        // --- mirror session state into Lua (net.role()/peers()/ping()) ---
        let state = if let Some(s) = &self.net_server {
            NetState {
                role: NetRoleState::Server,
                peers: s.peers().to_vec(),
                rtt_ms: s.peers().first().map(|&p| s.stats(p).rtt_ms).unwrap_or(0.0),
            }
        } else {
            NetState::default()
        };
        self.script_host.set_net_state(state);
    }

    /// Become the authoritative host of an in-editor session (Lua `net.host{}`
    /// or the harness panel). Captures the scene doc at this moment — it's the
    /// baseline a ghost client loads, exactly like a remote client loading the
    /// scene from disk.
    pub(crate) fn net_start_hosting(&mut self) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "net.host: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() {
            return;
        }
        let hub = floptle_net::MemoryHub::new();
        let mut s = NetSession::server(Box::new(hub.server_endpoint()));
        s.register_scene(&self.world);
        self.net_scene_doc = Some(floptle_scene::to_doc("net-baseline", &self.world));
        self.net_hub = Some(hub);
        self.net_server = Some(s);
        self.console.push(
            floptle_script::LogLevel::Debug,
            "🌐 hosting an in-editor session (join a local ghost client from the 🌐 panel)".into(),
            None,
        );
    }

    /// Join a local ghost client to the in-editor session (hosting first if
    /// needed): a hidden world that receives real snapshots over the simulated
    /// link and shows up as cyan ghosts in the viewport.
    pub(crate) fn net_join_local(&mut self) {
        if self.net_server.is_none() {
            self.net_start_hosting();
        }
        if self.net_client.is_some() || self.net_hub.is_none() {
            return;
        }
        let Some(doc) = self.net_scene_doc.clone() else { return };
        let mut cw = World::default();
        floptle_scene::spawn_into(&doc, &mut cw);
        let hub = self.net_hub.as_ref().unwrap();
        let mut c = NetSession::client(Box::new(hub.connect()));
        c.register_scene(&cw);
        self.net_client = Some((c, cw));
        self.net_ghosts = true;
        self.console.push(
            floptle_script::LogLevel::Debug,
            "🌐 local ghost client joined — cyan ghosts = what a remote player sees".into(),
            None,
        );
    }

    /// "Test as remote player" (2c): the PLAY world becomes a predicted CLIENT
    /// and a hidden authoritative server (full second sim + Lua host) runs
    /// behind the simulated link. Your character predicts locally, the server
    /// re-runs your inputs, divergences rewind-replay — the real netcode feel,
    /// alone, in one editor.
    pub(crate) fn net_play_as_client(&mut self) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "test as remote player: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() || self.net_play_client.is_some() {
            return;
        }
        // The hidden server world: the scene as it stands right now.
        let doc = floptle_scene::to_doc("net-server", &self.world);
        let mut sworld = World::default();
        floptle_scene::spawn_into(&doc, &mut sworld);
        // Harness convenience: every Predicted node belongs to the one joining
        // client (peer 1) — on BOTH sides, so registration + routing agree.
        let assign_owner = |w: &mut World| {
            let preds: Vec<Entity> = w
                .query::<floptle_core::Replicated>()
                .filter(|(_, r)| r.mode == ReplicationMode::Predicted)
                .map(|(e, _)| e)
                .collect();
            for e in preds {
                if let Some(r) = w.get_mut::<floptle_core::Replicated>(e) {
                    r.owner = Some(1);
                }
            }
        };
        assign_owner(&mut sworld);
        assign_owner(&mut self.world);
        // The hidden server's physics: same terrain anchors (identical scene),
        // its own bodies/static colliders from ITS world. SAME sim origin as
        // the client's — different origins mean different f32 quantization and
        // the two sims drift apart at the bit level.
        let origin = self.sim.as_ref().map(|s| s.world.origin).unwrap_or_else(|| self.sim_origin_hint());
        let gravity = Self::build_gravity_field(&sworld, origin);
        let terrain_vols = self.terrain_volumes();
        let mut ssim = floptle_physics::Sim::build(&sworld, &terrain_vols, gravity, origin);
        drop(terrain_vols);
        self.add_static_colliders_for_world(&sworld, &mut ssim);
        // Seed the server bodies with the client's LIVE dynamic state (the doc
        // only carries transforms): the session starts mid-play, and a
        // vel-zero server character instantly disagrees with a moving client.
        if let Some(csim) = self.sim.as_ref() {
            let mine: Vec<Entity> = self
                .world
                .query::<Transform>()
                .map(|(e, _)| e)
                .filter(|e| self.world.get::<floptle_core::RigidBody>(*e).is_some())
                .collect();
            let theirs: Vec<Entity> = sworld
                .query::<Transform>()
                .map(|(e, _)| e)
                .filter(|e| sworld.get::<floptle_core::RigidBody>(*e).is_some())
                .collect();
            for (me, them) in mine.iter().zip(theirs.iter()) {
                if let Some(bs) = csim.body_snapshot(me.index()) {
                    ssim.restore_body(them.index(), &bs);
                }
            }
            ssim.writeback_interpolated(&mut sworld, 1.0);
        }
        // Sessions over the simulated link; skew frozen from the CURRENT slider.
        let hub = floptle_net::MemoryHub::new();
        hub.set_conditions(self.net_latency_ticks, self.net_loss);
        let mut server = NetSession::server(Box::new(hub.server_endpoint()));
        server.register_scene(&sworld);
        let mut client = NetSession::client(Box::new(hub.connect()));
        client.register_scene(&self.world);
        // The client predicts ONLY its own node; every other replicated node is
        // snapshot-driven: skip its scripts, deactivate its body.
        let mut skip = std::collections::HashSet::new();
        let mut predicted: Option<Entity> = None;
        let reps: Vec<(Entity, floptle_core::Replicated)> = self
            .world
            .query::<floptle_core::Replicated>()
            .map(|(e, r)| (e, *r))
            .collect();
        for (e, r) in reps {
            let mine = r.mode == ReplicationMode::Predicted && r.owner == Some(1);
            if mine && predicted.is_none() {
                predicted = Some(e);
                continue;
            }
            skip.insert(e.index());
            if let Some(sim) = self.sim.as_mut() {
                sim.set_body_active(e.index(), false);
            }
        }
        self.script_host.set_script_filter(skip);
        if let Some(pe) = predicted {
            // Prediction NEEDS velocity+grounded on the wire — force-enable
            // physics sync on the predicted node (both worlds; reconciliation
            // restoring a zero velocity every snapshot reads as violent jitter).
            let enable = |w: &mut World, e: Entity| {
                if let Some(r) = w.get_mut::<floptle_core::Replicated>(e)
                    && !r.physics
                {
                    r.physics = true;
                }
            };
            enable(&mut self.world, pe);
            // Same node in the server world = same position in node order; find
            // it by matching net id via name-independent index: both worlds were
            // registered in identical node order, so match by Replicated order.
            let spred: Option<Entity> = {
                let mine: Vec<Entity> = self
                    .world
                    .query::<Transform>()
                    .map(|(e, _)| e)
                    .filter(|e| self.world.get::<floptle_core::Replicated>(*e).is_some())
                    .collect();
                let theirs: Vec<Entity> = sworld
                    .query::<Transform>()
                    .map(|(e, _)| e)
                    .filter(|e| sworld.get::<floptle_core::Replicated>(*e).is_some())
                    .collect();
                mine.iter().position(|e| *e == pe).and_then(|i| theirs.get(i).copied())
            };
            if let Some(se) = spred {
                enable(&mut sworld, se);
            }
            // The predicted node's `update` moves to the TICK clock (the server
            // integrates it per tick — the client must match or they fight).
            let mut fskip = std::collections::HashSet::new();
            fskip.insert(pe.index());
            self.script_host.set_frame_filter(fskip);
        }
        self.net_predictor = predicted.map(|e| (e, floptle_net::Predictor::new()));
        if predicted.is_none() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "no Predicted node found — you're a spectator. Give your character a Networked component with mode 'Predicted (owner)'".into(),
                None,
            );
        }
        let host = floptle_script::ScriptHost::new();
        host.set_project_root(self.project_root.clone());
        self.net_hidden = Some(HiddenServer {
            session: server,
            world: sworld,
            sim: ssim,
            host,
            peer: 1,
            next_tick: 0,
        });
        self.net_play_client = Some(client);
        self.net_hub = Some(hub);
        self.net_ghosts = true;
        self.console.push(
            floptle_script::LogLevel::Debug,
            "🎮 you are now a REMOTE PLAYER: predicting locally against a hidden server \
             (clock skew tracks the latency slider live). Orange ghosts = the server's truth."
                .into(),
            None,
        );
    }

    /// Run the hidden server up to its target tick (`client_tick − latency − 1`,
    /// tracked LIVE from the slider): consume the client's replayed input, run
    /// the full authoritative sim (scripts + physics), snapshot back. Raising
    /// the latency slider pauses the server briefly (pipeline refill); lowering
    /// it catches up (≤ 4 ticks per editor tick).
    fn net_hidden_tick(&mut self, client_tick: u64) {
        let target = client_tick.saturating_sub(self.net_latency_ticks + 1);
        for _ in 0..4 {
            let Some(hs) = self.net_hidden.as_mut() else { return };
            if hs.next_tick == 0 {
                hs.next_tick = target.max(1); // first run: start at the skewed clock
            }
            if hs.next_tick > target {
                return; // caught up (or pausing while the pipeline refills)
            }
            let st = hs.next_tick;
            hs.next_tick += 1;
            self.net_hidden_run_one(st);
        }
    }

    /// One authoritative server tick at server-tick `st`.
    fn net_hidden_run_one(&mut self, st: u64) {
        let Some(hs) = self.net_hidden.as_mut() else { return };
        let step = self.game_tick.step;
        hs.session.pump_server(&hs.world, st);
        // The one-script model: the server's `input.*` IS the client's
        // replayed input for this tick (single-client harness).
        let inp = hs.session.input_for(hs.peer, st);
        hs.host.set_input(floptle_script::net_to_input(&inp));
        hs.host.set_net_state(NetState {
            role: NetRoleState::Server,
            peers: hs.session.peers().to_vec(),
            rtt_ms: hs.session.stats(hs.peer).rtt_ms,
        });
        // Feed body state + lend colliders, run scripts (server frame = tick).
        let mut states = HashMap::new();
        for (e, vel, up, grounded, height) in hs.sim.body_states() {
            states.insert(
                e.index(),
                floptle_script::BodyState {
                    vel: [vel.x, vel.y, vel.z],
                    up: [up.x, up.y, up.z],
                    grounded,
                    height,
                },
            );
        }
        hs.host.set_bodies(states);
        hs.host.set_project_root(self.project_root.clone());
        hs.host
            .set_colliders(std::mem::take(&mut hs.sim.world.colliders), hs.sim.world.origin);
        let dir = self.project_root.join("scripts");
        let t = st as f32 * step;
        hs.host.run(&mut hs.world, &dir, step, t);
        hs.host.run_fixed(&mut hs.world, step, t);
        hs.sim.world.colliders = hs.host.take_colliders();
        // Apply writes, step physics one tick, publish transforms.
        hs.sim.world.gravity = Self::build_gravity_field(&hs.world, hs.sim.world.origin);
        hs.sim.sync_dynamic_params(&hs.world);
        for (eid, v) in hs.host.take_body_changes() {
            hs.sim.set_body_velocity(eid, floptle_core::math::Vec3::new(v[0], v[1], v[2]));
        }
        for (eid, h) in hs.host.take_body_height_changes() {
            hs.sim.set_body_height(eid, h);
        }
        hs.sim.step_tick(step, None);
        hs.sim.writeback_interpolated(&mut hs.world, 1.0);
        // Server-side session commands from ITS scripts (rpc/spawn/despawn).
        for cmd in hs.host.take_net_commands() {
            match cmd {
                NetCmd::Rpc { name, args, to } => {
                    let target = to.map(RpcTarget::Peer).unwrap_or(RpcTarget::All);
                    let _ = hs.session.send_rpc(&name, args, target);
                }
                NetCmd::Despawn { eid } => {
                    let ent =
                        hs.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == eid);
                    if let Some(e) = ent {
                        hs.session.despawn(&mut hs.world, e);
                    }
                }
                _ => {}
            }
        }
        // Synced vars + body states → snapshots out.
        let raw = hs.host.collect_synced();
        let ents: HashMap<u32, Entity> =
            hs.world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
        let synced = raw
            .into_iter()
            .filter_map(|(eid, kind, vars)| ents.get(&eid).map(|e| (*e, kind, vars)))
            .collect();
        hs.session.update_synced(synced);
        let bstates: floptle_net::BodyStates = hs
            .sim
            .body_states()
            .map(|(e, vel, _, grounded, _)| (e, [vel.x, vel.y, vel.z], grounded))
            .collect();
        hs.session.update_body_states(bstates);
        hs.session.tick_server(&hs.world, st);
        // Received RPCs/events dispatch into the SERVER's scripts.
        let rpcs = hs.session.take_rpcs();
        let events = hs.session.take_events();
        for r in rpcs {
            hs.host.dispatch_rpc(&mut hs.world, &r.name, &r.args, r.sender);
        }
        for ev in events {
            match ev {
                NetEvent::PeerJoined(p) => {
                    hs.host.fire_net_event(&mut hs.world, "playerJoined", Some(p), None)
                }
                NetEvent::PeerLeft(p) => {
                    hs.host.fire_net_event(&mut hs.world, "playerLeft", Some(p), None)
                }
                _ => {}
            }
        }
        // The hidden server renders nothing: drain its cosmetic queues so
        // they don't grow unboundedly (anim:play per tick, gizmos, effects).
        let _ = hs.host.take_anim_commands();
        let _ = hs.host.take_vfx_commands();
        let _ = hs.host.take_gizmos();
        let _ = hs.host.take_spawn_effects();
        let _ = hs.host.take_model_changes();
        let _ = hs.host.take_mouse_lock();
        // Surface server-side script output in the Console, tagged.
        for log in hs.host.drain_logs() {
            self.console.push(log.level, format!("[server] {}", log.msg), log.source);
        }
    }

    /// The play world's CLIENT tick: ship input, record the prediction, apply
    /// snapshots (others interpolate; our node reconciles + rewind-replays).
    fn net_client_tick(&mut self, tick: u64) {
        let step = self.game_tick.step;
        let Some(cs) = self.net_play_client.as_mut() else { return };
        let ni = floptle_script::input_to_net(&self.last_tick_input);
        cs.send_input(tick, ni.clone());
        // Record this tick's prediction (post-physics state of our node).
        if let (Some((pe, pred)), Some(sim)) = (self.net_predictor.as_mut(), self.sim.as_ref())
            && let Some(bs) = sim.body_snapshot(pe.index()) {
                let rot = self
                    .world
                    .get::<Transform>(*pe)
                    .map(|t| t.rotation.to_array())
                    .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                pred.record(
                    tick,
                    ni,
                    PredictedState {
                        pos: bs.pos.to_array(),
                        rot,
                        vel: bs.vel.to_array(),
                        grounded: bs.grounded,
                    },
                );
            }
        // Poll: snapshots interpolate others + ship the input window.
        cs.tick_client(&mut self.world);
        // Synced vars from the server land in our scripts.
        for (e, kind, vars) in cs.take_synced() {
            self.script_host.apply_synced(e.index(), &kind, &vars);
        }
        // Reconcile our own node against authoritative states.
        let updates = cs.take_predicted_updates();
        let rpcs = cs.take_rpcs();
        let events = cs.take_events();
        for (e, stick, state) in updates {
            let Some((pe, pred)) = self.net_predictor.as_mut() else { continue };
            if e != *pe {
                continue;
            }
            let eid = pe.index();
            // A touch looser than the library default: absorbs camera-yaw
            // integration noise (per-frame vs per-tick smoothing) so only real
            // divergence triggers a correction. 5 mm is invisible.
            let Some(replay) = pred.reconcile(stick, &state, 5e-3) else {
                continue; // prediction confirmed
            };
            // Rewind to the server's word…
            if let Some(sim) = self.sim.as_mut() {
                sim.restore_body(
                    eid,
                    &BodySnapshot {
                        pos: floptle_core::math::DVec3::from_array(state.pos),
                        vel: floptle_core::math::Vec3::from_array(state.vel),
                        grounded: state.grounded,
                    },
                );
            }
            if let Some(tr) = self.world.get_mut::<Transform>(*pe) {
                tr.translation = floptle_core::math::DVec3::from_array(state.pos);
                tr.rotation = floptle_core::math::Quat::from_array(state.rot);
            }
            // …and replay the unacknowledged inputs through the SAME script —
            // BOTH hooks, exactly as the tick originally ran (update rides the
            // tick clock for a predicted node), including component writes
            // (e.g. a controller's rig.friction toggle) reaching the body.
            for (rtick, rinput) in replay {
                let rt = rtick as f32 * step;
                self.script_host.set_input(floptle_script::net_to_input(&rinput));
                // Body state for the replayed tick: the body's CURRENT
                // (being-replayed) state, so node.grounded/vx reads are right.
                if let Some(sim) = self.sim.as_ref()
                    && let Some(bs) = sim.body_snapshot(eid)
                {
                    let mut states = HashMap::new();
                    // up/height: reuse the live values (not part of rollback).
                    if let Some((_, _, up, _, height)) =
                        sim.body_states().find(|(e, ..)| e.index() == eid)
                    {
                        states.insert(
                            eid,
                            floptle_script::BodyState {
                                vel: bs.vel.to_array(),
                                up: [up.x, up.y, up.z],
                                grounded: bs.grounded,
                                height,
                            },
                        );
                    }
                    self.script_host.set_bodies(states);
                }
                // Lend colliders so the replayed hooks can raycast (ground
                // probes!) — a nil raycast mid-replay corrupts grounded /
                // friction logic and the correction never converges. Reclaimed
                // before the body steps.
                if let Some(sim) = self.sim.as_mut() {
                    self.script_host.set_colliders(
                        std::mem::take(&mut sim.world.colliders),
                        sim.world.origin,
                    );
                }
                self.script_host.run_frame_for(&mut self.world, eid, step, rt);
                self.script_host.run_fixed_for(&mut self.world, eid, step, rt);
                if let Some(sim) = self.sim.as_mut() {
                    sim.world.colliders = self.script_host.take_colliders();
                }
                for (id, v) in self.script_host.take_body_changes() {
                    if id == eid && let Some(sim) = self.sim.as_mut() {
                        sim.set_body_velocity(id, floptle_core::math::Vec3::new(v[0], v[1], v[2]));
                    }
                }
                for (id, h) in self.script_host.take_body_height_changes() {
                    if id == eid && let Some(sim) = self.sim.as_mut() {
                        sim.set_body_height(id, h);
                    }
                }
                if let Some(sim) = self.sim.as_mut() {
                    // Component writes (friction etc.) land on the body too.
                    sim.sync_dynamic_params(&self.world);
                    sim.step_body_tick(eid, step);
                    if let Some(bs) = sim.body_snapshot(eid) {
                        let rot = self
                            .world
                            .get::<Transform>(*pe)
                            .map(|t| t.rotation.to_array())
                            .unwrap_or([0.0, 0.0, 0.0, 1.0]);
                        pred.rerecord(
                            rtick,
                            PredictedState {
                                pos: bs.pos.to_array(),
                                rot,
                                vel: bs.vel.to_array(),
                                grounded: bs.grounded,
                            },
                        );
                    }
                }
            }
        }
        // Client-side RPC/event dispatch (onRpc handlers, net.on).
        for r in rpcs {
            self.script_host.dispatch_rpc(&mut self.world, &r.name, &r.args, r.sender);
        }
        for ev in events {
            match ev {
                NetEvent::Connected => {
                    self.script_host.fire_net_event(&mut self.world, "connected", None, None)
                }
                NetEvent::Disconnected(reason) => self.script_host.fire_net_event(
                    &mut self.world,
                    "disconnected",
                    None,
                    Some(&reason),
                ),
                NetEvent::PeerJoined(p) => {
                    self.script_host.fire_net_event(&mut self.world, "playerJoined", Some(p), None)
                }
                NetEvent::PeerLeft(p) => {
                    self.script_host.fire_net_event(&mut self.world, "playerLeft", Some(p), None)
                }
            }
        }
        // Smooth the visual correction.
        if let Some((_, pred)) = self.net_predictor.as_mut() {
            pred.decay_error();
        }
        // Mirror client state into Lua.
        let rtt = self
            .net_play_client
            .as_ref()
            .map(|c| c.stats(floptle_net::SERVER).rtt_ms)
            .unwrap_or(0.0);
        self.script_host.set_net_state(NetState {
            role: NetRoleState::Client,
            peers: Vec::new(),
            rtt_ms: rtt,
        });
    }

    /// Static colliders for an arbitrary world (the hidden server's) — same
    /// logic as the play world's builder.
    fn add_static_colliders_for_world(&self, world: &World, sim: &mut floptle_physics::Sim) {
        use floptle_core::math::{Mat4, Vec3};
        use floptle_core::Matter;
        let mut ents: Vec<Entity> = world
            .query::<floptle_core::Collidable>()
            .map(|(e, _)| e)
            .filter(|e| world.get::<floptle_core::RigidBody>(*e).is_none())
            .collect();
        for (e, _) in world.query::<floptle_core::MeshCollider>() {
            if !ents.contains(&e) && world.get::<floptle_core::RigidBody>(e).is_none() {
                ents.push(e);
            }
        }
        for e in ents {
            let wt = floptle_core::world_transform(world, e);
            let anchor = wt.translation;
            let s = wt.scale;
            match world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => {
                    let path = asset_path.clone();
                    let Ok(model) =
                        floptle_assets::gltf_import::import(std::path::Path::new(&path))
                    else {
                        continue;
                    };
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, Vec3::ZERO);
                    let mut verts: Vec<Vec3> = Vec::new();
                    let mut indices: Vec<u32> = Vec::new();
                    for part in &model.parts {
                        let base = verts.len() as u32;
                        verts.extend(
                            part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos))),
                        );
                        indices.extend(part.mesh.indices.iter().map(|i| i + base));
                    }
                    sim.add_static_mesh(anchor, &verts, &indices);
                }
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(
                            anchor,
                            Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z),
                            wt.rotation,
                        );
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element());
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z));
                    }
                },
                _ => {}
            }
        }
    }

    /// Tear the in-editor session down (Stop, `net.leave()`, or the panel).
    pub(crate) fn net_stop(&mut self, why: &str) {
        if self.net_server.is_none() && self.net_client.is_none() && self.net_play_client.is_none()
        {
            return;
        }
        self.net_server = None;
        self.net_client = None;
        self.net_play_client = None;
        self.net_hidden = None;
        self.net_predictor = None;
        self.net_hub = None;
        self.net_scene_doc = None;
        self.script_host.set_script_filter(std::collections::HashSet::new());
        self.script_host.set_frame_filter(std::collections::HashSet::new());
        self.script_host.clear_net_state();
        self.script_host.set_net_state(NetState::default());
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("🌐 session ended ({why})"),
            None,
        );
    }

    /// `net.spawn(path, {...})`: load a scene asset, spawn its FIRST node as a
    /// replicated runtime object (position/owner overrides applied).
    fn net_spawn_path(&mut self, path: &str, pos: Option<[f64; 3]>, owner: Option<u64>) {
        if self.net_server.is_none() {
            return;
        }
        let full = self.project_root.join(path);
        let doc = match std::fs::read_to_string(&full).map_err(|e| e.to_string()).and_then(|s| {
            floptle_scene::from_ron(&s).map_err(|e| e.to_string())
        }) {
            Ok(d) => d,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("net.spawn(\"{path}\"): {e}"),
                    None,
                );
                return;
            }
        };
        let Some(mut node) = doc.nodes.first().cloned() else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("net.spawn(\"{path}\"): the scene has no nodes"),
                None,
            );
            return;
        };
        if let Some(p) = pos {
            node.transform.translation = p;
        }
        let s = self.net_server.as_mut().unwrap();
        let e = s.spawn_doc(&mut self.world, &node, owner);
        // A spawned mesh needs its GPU import like any script-swapped model.
        if let floptle_scene::MatterDoc::Mesh { .. } = node.matter {
            self.load_script_swapped_models();
        }
        let _ = e;
    }

    /// Ghost gizmos: CYAN = where a ghost client believes every replicated
    /// node is (2b hosting); ORANGE = the hidden server's authoritative truth
    /// (2c play-as-client — the gap to your character is your prediction).
    pub(crate) fn net_ghost_gizmos(&mut self) {
        if !self.net_ghosts {
            return;
        }
        let ghost = |w: &World, color: [f32; 3], out: &mut Vec<floptle_script::GizmoCmd>| {
            for (e, _) in w.query::<floptle_core::Replicated>() {
                if let Some(tr) = w.get::<Transform>(e) {
                    out.push(floptle_script::GizmoCmd::Sphere {
                        center: [
                            tr.translation.x as f32,
                            tr.translation.y as f32,
                            tr.translation.z as f32,
                        ],
                        radius: 0.45,
                        color,
                    });
                }
            }
        };
        let mut out = Vec::new();
        if let Some((_, cw)) = &self.net_client {
            ghost(cw, [0.25, 0.9, 1.0], &mut out);
        }
        if let Some(hs) = &self.net_hidden {
            ghost(&hs.world, [1.0, 0.6, 0.15], &mut out);
        }
        self.script_gizmos.extend(out);
    }
}
