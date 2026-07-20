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

use crate::{anim, Editor};

/// The rewound world for a stamped combat intent (`docs/netcode-design.md`
/// §7): every networked node's pose (+ its scripts' `synced` vars) at the
/// tick the SENDER perceived — their stamp minus each node's interp delay,
/// clamped to the rewind window. Poses only exist for transform-synced nodes;
/// synced vars rewind for every networked node.
fn build_rewind_scope(
    world: &World,
    session: &NetSession,
    history: &floptle_net::LagHistory,
    now: u64,
    sender: u64,
    stamp: u64,
) -> floptle_script::RewindScope {
    let mut poses = Vec::new();
    let mut synced = Vec::new();
    for (nid, e) in session.net_entities() {
        let Some(rep) = world.get::<floptle_core::Replicated>(e) else { continue };
        let target =
            floptle_net::LagHistory::clamp_rewind(now, stamp.saturating_sub(rep.interp_delay as u64));
        let Some(h) = history.state_at(nid, target) else { continue };
        if rep.transform {
            poses.push((e.index(), h.pos));
        }
        for (kind, vars) in &h.synced {
            synced.push((e.index(), kind.clone(), vars.clone()));
        }
    }
    floptle_script::RewindScope { peer: sender, poses, synced }
}

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
    /// Lag-compensation history (`docs/netcode-design.md` §7): the last ~600 ms
    /// of authoritative poses + synced vars, per networked node — what
    /// `net.rewind` re-poses combat queries to.
    pub history: floptle_net::LagHistory,
    /// The server's OWN animator runtimes: server scripts' `anim:play(...)`
    /// drives real controllers here (state machines + scene-binding transform
    /// writes + gameplay events — hit windows are server-authoritative), and
    /// their (state, time) per layer is what replicates to clients.
    pub anim: crate::anim::AnimSystem,
}

impl Editor {
    /// Once per gameplay tick, after physics (`docs/netcode-design.md` §9):
    /// drain Lua session commands, advance the hub clock, run the server and
    /// ghost-client sessions, and dispatch received RPCs/events into scripts.
    pub(crate) fn net_tick(&mut self, tick: u64) {
        for cmd in self.script_host.take_net_commands() {
            match cmd {
                NetCmd::Host { relay: Some(addr), .. } => {
                    let a = addr.clone();
                    self.net_host_relay(&a);
                }
                NetCmd::Host { port: Some(p), .. } => self.net_host_quic(p),
                NetCmd::Host { .. } => self.net_start_hosting(),
                NetCmd::Join { addr } if addr.starts_with("local") => self.net_join_local(),
                NetCmd::Join { addr } if addr.starts_with("relay://") => {
                    let rest = addr.trim_start_matches("relay://").to_string();
                    match rest.rsplit_once('/') {
                        Some((raddr, code)) => self.net_join_relay(raddr, code),
                        None => self.console.push(
                            floptle_script::LogLevel::Warn,
                            format!("net.join(\"{addr}\"): expected relay://host:port/CODE"),
                            None,
                        ),
                    }
                }
                NetCmd::Join { addr } if addr.starts_with("quic://") => {
                    let a = addr.trim_start_matches("quic://").to_string();
                    self.net_join_quic(&a);
                }
                NetCmd::Join { addr } => self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "net.join(\"{addr}\"): use relay://relayaddr/CODE (a lobby code), \
                         quic://host:port (a server directly), or local:// (the in-editor \
                         harness)"
                    ),
                    None,
                ),
                NetCmd::Leave => self.net_stop("left the session"),
                NetCmd::Rpc { name, args, to, with_input } => {
                    if let Some(s) = self.net_server.as_mut() {
                        let target = to.map(RpcTarget::Peer).unwrap_or(RpcTarget::All);
                        if let Err(e) = s.send_rpc(&name, args, target) {
                            self.console.push(floptle_script::LogLevel::Warn, e, None);
                        }
                    } else if let Some(c) = self.net_play_client.as_mut() {
                        // On a client, rpcs are intents for the server;
                        // `{withInput = true}` stamps the perceived tick for
                        // lag compensation (§7).
                        if let Err(e) = c.send_rpc_stamped(&name, args, RpcTarget::Server, with_input)
                        {
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
                            if let Some(sim) = self.sim.as_mut() {
                                sim.remove_body(eid);
                            }
                            let n = self.net_remote_predicted.len();
                            self.net_remote_predicted.retain(|(re, _)| *re != e);
                            if self.net_remote_predicted.len() != n {
                                self.net_apply_host_filters();
                            }
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
            // EXACT post-tick poses first: the frame-end writeback renders
            // partway into a tick (alpha < 1), so the Transforms still hold
            // LAST frame's render pose — 1–2 ticks stale. Snapshots and the
            // lag-comp history read Transforms; shipping the stale pose makes
            // every moving owner mispredict by ~a tick of motion and scoot at
            // the snapshot cadence. The hidden-server harness always did this
            // (writeback at alpha 1.0) — the real host must too. The frame's
            // later interpolated writeback re-smooths what's rendered here.
            if let Some(sim) = self.sim.as_ref() {
                sim.writeback_interpolated(&mut self.world, 1.0);
            }
            let raw = self.script_host.collect_synced();
            let ents: HashMap<u32, Entity> =
                self.world.query::<Transform>().map(|(e, _)| (e.index(), e)).collect();
            // Record this tick into the lag-comp history (post-physics poses +
            // the synced values just collected) — what net.rewind rewinds to.
            if let Some(s) = self.net_server.as_ref() {
                let mut hist: Vec<(u64, floptle_net::HistEntry)> = Vec::new();
                for (nid, e) in s.net_entities() {
                    let Some(tr) = self.world.get::<Transform>(e) else { continue };
                    let synced = raw
                        .iter()
                        .filter(|(eid, _, _)| *eid == e.index())
                        .map(|(_, kind, vars)| (kind.clone(), vars.clone()))
                        .collect();
                    hist.push((
                        nid,
                        floptle_net::HistEntry {
                            pos: [tr.translation.x, tr.translation.y, tr.translation.z],
                            synced,
                        },
                    ));
                }
                self.net_history.record(tick, hist);
            }
            let synced = raw
                .into_iter()
                .filter_map(|(eid, kind, vars)| ents.get(&eid).map(|e| (*e, kind, vars)))
                .collect();
            // Live body states ride the snapshots (velocity + grounded) — a
            // predicted node's owner reconciles against them; without this,
            // every correction restores ZERO velocity + airborne (dead jumps,
            // ground-sticking stutter).
            let bstates: floptle_net::BodyStates = self
                .sim
                .as_ref()
                .map(|sim| {
                    sim.body_states()
                        .map(|(e, vel, _, grounded, _)| (e, [vel.x, vel.y, vel.z], grounded))
                        .collect()
                })
                .unwrap_or_default();
            // Networked animators: the host's play loop already advanced every
            // controller — ship each one's (state, time, weight) per layer for
            // snapshot diffing (transitions cost bytes; steady loops cost none).
            let anims = anim::collect_net_states(&self.world, &self.mesh_registry, &self.anim);
            if let Some(s) = self.net_server.as_mut() {
                s.update_synced(synced);
                s.update_body_states(bstates);
                s.update_anim_states(anims);
                s.tick_server(&self.world, tick);
                (s.take_rpcs(), s.take_events())
            } else {
                (Vec::new(), Vec::new())
            }
        } else {
            (Vec::new(), Vec::new())
        };
        if !rpcs.is_empty() || !events.is_empty() {
            // Lend colliders + hulls so onRpc / net.on handlers can raycast
            // (physics already stepped this tick; reclaimed right after).
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
            for r in rpcs {
                // A stamped intent gets the lag-comp rewind scope (§7).
                let scope = match (r.tick, self.net_server.as_ref()) {
                    (Some(stamp), Some(s)) => Some(build_rewind_scope(
                        &self.world,
                        s,
                        &self.net_history,
                        tick,
                        r.sender,
                        stamp,
                    )),
                    _ => None,
                };
                self.script_host.set_rewind(scope);
                self.script_host.dispatch_rpc(&mut self.world, &r.name, &r.args, r.sender);
                self.script_host.set_rewind(None);
            }
            for ev in events {
                match ev {
                    NetEvent::PeerJoined(p) => {
                        self.script_host.fire_net_event(&mut self.world, "playerJoined", Some(p), None)
                    }
                    NetEvent::PeerLeft(p) => {
                        // Their avatar leaves with them: every runtime spawn
                        // that peer owned despawns everywhere, automatically.
                        let owned = self
                            .net_server
                            .as_ref()
                            .map(|s| s.owned_runtime_spawns(p))
                            .unwrap_or_default();
                        let cleaned = !owned.is_empty();
                        for e in owned {
                            if let Some(s) = self.net_server.as_mut() {
                                s.despawn(&mut self.world, e);
                            }
                            if let Some(sim) = self.sim.as_mut() {
                                sim.remove_body(e.index());
                            }
                            self.net_remote_predicted.retain(|(re, _)| *re != e);
                        }
                        if cleaned {
                            self.net_apply_host_filters();
                        }
                        self.script_host.fire_net_event(&mut self.world, "playerLeft", Some(p), None)
                    }
                    _ => {}
                }
            }
            if let Some(sim) = self.sim.as_mut() {
                sim.world.colliders = self.script_host.take_colliders();
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
            let _ = c.take_anim_updates();
        }
        // The ghost follows scene switches exactly like a remote client:
        // reload the scene from DISK into its hidden world, rebind NetIds.
        let ghost_switch = self.net_client.as_mut().and_then(|(c, _)| c.take_scene_switch());
        if let Some(scene) = ghost_switch {
            let loaded = self
                .resolve_scene_request(&scene)
                .and_then(|p| floptle_scene::load(&p).ok());
            match (loaded, self.net_client.as_mut()) {
                (Some(doc), Some((c, cw))) => {
                    *cw = World::default();
                    floptle_scene::spawn_into(&doc, cw);
                    c.rebind_scene(cw);
                }
                _ => {
                    self.net_client = None;
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("ghost client left — it couldn't load \"{scene}\""),
                        None,
                    );
                }
            }
        }
        // --- mirror session state into Lua (net.role()/peers()/ping()/isMine) ---
        let state = if let Some(s) = &self.net_server {
            NetState {
                role: NetRoleState::Server,
                peers: s.peers().to_vec(),
                rtt_ms: s.peers().first().map(|&p| s.stats(p).rtt_ms).unwrap_or(0.0),
                my_peer: None,
            }
        } else {
            NetState::default()
        };
        let owners = if self.net_server.is_some() {
            Self::collect_net_owners(&self.world)
        } else {
            HashMap::new()
        };
        self.script_host.set_net_owners(owners);
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
        s.set_tick_dt(self.game_tick.step); // the animator time predictor's clock
        s.set_scene(&self.scene_rel_or_default());
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

    /// Client-side session setup shared by the 2c harness and a real
    /// `quic://` join: every TRANSFORM/PHYSICS-synced authority node becomes
    /// snapshot-driven (scripts skipped, body deactivated — snapshots own it);
    /// var-only Networked nodes keep running everywhere (the door pattern).
    /// The Predicted node owned by `my_owner` (if any) becomes the local
    /// player: body re-activated, physics sync forced on (prediction needs
    /// vel+grounded on the wire), its `update` moved to the tick clock, the
    /// Predictor armed. A real join calls this twice: with `None` at join
    /// time (my peer id is unknown — everything snapshot-driven), then with
    /// `Some(my_peer)` when the Welcome lands.
    fn net_client_side_setup(&mut self, my_owner: Option<u64>, warn_if_none: bool) -> Option<Entity> {
        let mut skip = std::collections::HashSet::new();
        let mut predicted: Option<Entity> = None;
        let reps: Vec<(Entity, floptle_core::Replicated)> = self
            .world
            .query::<floptle_core::Replicated>()
            .map(|(e, r)| (e, *r))
            .collect();
        for (e, r) in reps {
            let mine = r.mode == ReplicationMode::Predicted
                && r.owner.is_some()
                && r.owner == my_owner;
            if mine && predicted.is_none() {
                predicted = Some(e);
                continue;
            }
            if !(r.transform || r.physics) {
                continue; // var-only: scripts run on the client too
            }
            skip.insert(e.index());
            if let Some(sim) = self.sim.as_mut() {
                sim.set_body_active(e.index(), false);
            }
        }
        self.script_host.set_script_filter(skip);
        if let Some(pe) = predicted {
            if let Some(r) = self.world.get_mut::<floptle_core::Replicated>(pe)
                && !r.physics
            {
                r.physics = true;
            }
            // Deferred bind: the node was snapshot-driven until the Welcome
            // told us it's ours — its body simulates locally again.
            if let Some(sim) = self.sim.as_mut() {
                sim.set_body_active(pe.index(), true);
            }
            // The predicted node's `update` moves to the TICK clock (the server
            // integrates it per tick — the client must match or they fight).
            let mut fskip = std::collections::HashSet::new();
            fskip.insert(pe.index());
            self.script_host.set_frame_filter(fskip);
        }
        // Keep an existing predictor when it's still the same node — this
        // setup re-runs whenever a replicated spawn/despawn arrives, and that
        // must not reset prediction state mid-flight.
        let keep = matches!(
            (&self.net_predictor, predicted),
            (Some((e, _)), Some(pe)) if *e == pe
        );
        if !keep {
            self.net_predictor = predicted.map(|e| (e, floptle_net::Predictor::new()));
        }
        if predicted.is_none() && warn_if_none {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "no Predicted node is yours (yet) — spectating. Either the scene needs a \
                 Predicted slot per player (node #1 = host, #2+ = joiners), or the game spawns \
                 avatars on join (net.spawn in a playerJoined handler) and yours is on its way"
                    .into(),
                None,
            );
        }
        predicted
    }

    /// The session ownership convention, applied identically on every machine
    /// at session start: scene-authored Predicted nodes, in NODE order, belong
    /// to — #1 the HOST (owner None: it runs under the host's live input),
    /// #2 the first joiner (peer 1), #3 the second (peer 2), and so on. No
    /// negotiation needed; registration, snapshot routing, and input routing
    /// all agree. Runtime avatars carry explicit owners via `net.spawn`.
    fn net_assign_scene_owners(world: &mut World) {
        let preds: Vec<Entity> = world
            .query::<Transform>()
            .map(|(e, _)| e)
            .filter(|e| {
                world
                    .get::<floptle_core::Replicated>(*e)
                    .map(|r| r.mode == ReplicationMode::Predicted)
                    .unwrap_or(false)
            })
            .collect();
        for (i, e) in preds.iter().enumerate() {
            if let Some(r) = world.get_mut::<floptle_core::Replicated>(*e) {
                r.owner = if i == 0 { None } else { Some(i as u64) };
                // Prediction needs vel+grounded in snapshots on BOTH ends.
                if !r.physics {
                    r.physics = true;
                }
            }
        }
    }

    /// Networked nodes' owners, mirrored into Lua for `net.isMine(node)`.
    fn collect_net_owners(world: &World) -> HashMap<u32, Option<u64>> {
        world.query::<floptle_core::Replicated>().map(|(e, r)| (e.index(), r.owner)).collect()
    }

    /// (Re)apply the HOST's script filters from the remote-owned Predicted
    /// set: those nodes leave the global passes (they run per tick with their
    /// owner's replayed input) — everything else runs under the host normally.
    pub(crate) fn net_apply_host_filters(&mut self) {
        let mut skip = std::collections::HashSet::new();
        let mut fskip = std::collections::HashSet::new();
        for (e, _) in &self.net_remote_predicted {
            skip.insert(e.index());
            fskip.insert(e.index());
        }
        self.script_host.set_script_filter(skip);
        self.script_host.set_frame_filter(fskip);
    }

    /// Offline play: only player slot #1 (the first Predicted node) takes
    /// input — the other slots exist for joiners, and running their identical
    /// controllers against the same keyboard would move every copy at once.
    /// Applied at Play start and again when a session ends.
    pub(crate) fn net_apply_offline_slots(&mut self) {
        let preds: Vec<Entity> = self
            .world
            .query::<Transform>()
            .map(|(e, _)| e)
            .filter(|e| {
                self.world
                    .get::<floptle_core::Replicated>(*e)
                    .map(|r| r.mode == ReplicationMode::Predicted)
                    .unwrap_or(false)
            })
            .collect();
        let skip: std::collections::HashSet<u32> =
            preds.iter().skip(1).map(|e| e.index()).collect();
        if !skip.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!(
                    "{} extra player slot(s) idle offline — they come alive when peers join",
                    skip.len()
                ),
                None,
            );
        }
        self.script_host.set_script_filter(skip);
    }

    /// A queued `scene.load(...)` from a script, routed by session role:
    /// offline = plain switch; HOSTING = switch locally, announce to every
    /// client (they load + rebind), rebuild the session against the new scene;
    /// a JOINED client = refused (the server drives scenes — ask it via an
    /// RPC). Runs at the top of a frame, never mid-frame under the scripts.
    pub(crate) fn perform_scene_request(&mut self, req: &str) {
        if self.net_play_client.is_some() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!(
                    "scene.load(\"{req}\"): only the server switches scenes in a session — \
                     send it an RPC (net.send) and let ITS script call scene.load"
                ),
                None,
            );
            return;
        }
        let Some(rel) = self.switch_scene_during_play(req) else { return };
        // The new scene's input model is its own business — release any cursor
        // grab the OLD scene earned (game trap / script mouse lock), or a
        // cursor-driven scene (a main menu) arrives with the mouse frozen.
        // Its scripts re-lock via input.setMouseLocked if they want free-look.
        if self.game_trap || self.script_mouse_lock {
            self.game_trap = false;
            self.script_mouse_lock = false;
            if let Some(window) = self.window.as_ref() {
                self.cursor_lock_soft = crate::grab_cursor(window, false);
            }
        }
        if self.net_server.is_some() {
            // Re-run the host's per-scene session setup against the new world:
            // slot ownership, script filters, fresh NetIds, a clean lag-comp
            // ring — and the announcement that makes every client follow.
            Self::net_assign_scene_owners(&mut self.world);
            let remote: Vec<(Entity, u64)> = self
                .world
                .query::<floptle_core::Replicated>()
                .filter(|(_, r)| r.mode == ReplicationMode::Predicted)
                .filter_map(|(e, r)| r.owner.map(|p| (e, p)))
                .collect();
            self.net_remote_predicted = remote;
            self.net_apply_host_filters();
            self.net_history = floptle_net::LagHistory::new();
            if let Some(s) = self.net_server.as_mut() {
                s.switch_scene(&rel);
                s.rebind_scene(&self.world);
            }
            self.net_scene_doc = Some(floptle_scene::to_doc("net-baseline", &self.world));
        } else {
            self.net_apply_offline_slots();
        }
    }

    /// Host a REAL session on a UDP port (QUIC): other machines running the
    /// same project join with `net.join("quic://<ip>:port")`. The play world
    /// is the authoritative server — and scene-authored Predicted nodes belong
    /// to the FIRST joining peer, whose replayed inputs drive them in the tick
    /// loop (the one-script model, server side).
    pub(crate) fn net_host_quic(&mut self, port: u16) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "net.host{port}: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() || self.net_play_client.is_some() {
            return;
        }
        let transport = match floptle_net::QuicServer::bind(port) {
            Ok(t) => t,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("net.host{{port = {port}}}: {e}"),
                    None,
                );
                return;
            }
        };
        let bound = transport.local_port();
        self.net_host_with(
            Box::new(transport),
            &format!(
                "on UDP port {bound} — friends with this project join via the 🌐 panel or \
                 net.join(\"quic://<your-LAN-ip>:{bound}\")"
            ),
        );
    }

    /// Host a REAL session through a rendezvous RELAY: nobody port-forwards —
    /// the relay hands out a lobby code and friends join with it from
    /// anywhere that can reach the relay. Self-host `floptle-relay`, or use a
    /// managed one (Floptle Cloud).
    pub(crate) fn net_host_relay(&mut self, relay_addr: &str) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "net.host{relay}: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() || self.net_play_client.is_some() {
            return;
        }
        let (transport, code) = match floptle_net::RelayHost::host(relay_addr) {
            Ok(t) => t,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("net.host{{relay = \"{relay_addr}\"}}: {e}"),
                    None,
                );
                return;
            }
        };
        self.net_lobby_code = Some(code.clone());
        self.net_host_with(
            Box::new(transport),
            &format!(
                "via relay {relay_addr} — LOBBY CODE {code}. Friends join with \
                 net.join(\"relay://{relay_addr}/{code}\") or the 🌐 panel"
            ),
        );
    }

    /// The transport-agnostic tail of hosting a real session: the ownership
    /// convention, per-owner routing filters, the session itself.
    fn net_host_with(&mut self, transport: Box<dyn floptle_net::Transport>, how: &str) {
        Self::net_assign_scene_owners(&mut self.world);
        // Remote-owned Predicted nodes (slots #2, #3, …): skipped in the
        // global script passes, run per-tick with their owner's replayed
        // input instead. Slot #1 (owner None) is the HOST's — it stays in the
        // global passes under the host's live keyboard.
        let remote: Vec<(Entity, u64)> = self
            .world
            .query::<floptle_core::Replicated>()
            .filter(|(_, r)| r.mode == ReplicationMode::Predicted)
            .filter_map(|(e, r)| r.owner.map(|p| (e, p)))
            .collect();
        let slots = remote.len();
        self.net_remote_predicted = remote;
        self.net_apply_host_filters();
        let mut s = NetSession::server(transport);
        s.set_tick_dt(self.game_tick.step); // the animator time predictor's clock
        s.set_scene(&self.scene_rel_or_default()); // joiners land in OUR scene
        s.register_scene(&self.world);
        self.net_server = Some(s);
        self.net_scene_doc = Some(floptle_scene::to_doc("net-baseline", &self.world));
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!(
                "🌐 hosting {how}. Predicted node #1 is YOURS; {slots} joiner slot(s) follow \
                 in node order (or spawn avatars per joiner — see player_spawner.lua)."
            ),
            None,
        );
    }

    /// Join a REAL session at `host:port` (QUIC). The play world becomes a
    /// predicting client of a server on another machine — same machinery as
    /// "Test as remote player", minus the hidden server.
    pub(crate) fn net_join_quic(&mut self, addr: &str) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "net.join: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() || self.net_play_client.is_some() {
            return;
        }
        let transport = match floptle_net::QuicClient::connect(addr) {
            Ok(t) => t,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("net.join(\"quic://{addr}\"): {e}"),
                    None,
                );
                return;
            }
        };
        self.net_join_with(Box::new(transport), &format!("quic://{addr}"));
    }

    /// Join a session through a relay by LOBBY CODE.
    pub(crate) fn net_join_relay(&mut self, relay_addr: &str, code: &str) {
        if !self.playing {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "net.join: enter Play mode first".into(),
                None,
            );
            return;
        }
        if self.net_server.is_some() || self.net_play_client.is_some() {
            return;
        }
        let transport = match floptle_net::RelayClient::join(relay_addr, code) {
            Ok(t) => t,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!("net.join(\"relay://{relay_addr}/{code}\"): {e}"),
                    None,
                );
                return;
            }
        };
        self.net_join_with(Box::new(transport), &format!("relay://{relay_addr}/{code}"));
    }

    /// The transport-agnostic tail of joining a real session.
    fn net_join_with(&mut self, transport: Box<dyn floptle_net::Transport>, what: &str) {
        Self::net_assign_scene_owners(&mut self.world);
        let mut client = NetSession::client(transport);
        client.register_scene(&self.world);
        // Which slot is ours depends on the peer id the server assigns —
        // everything is snapshot-driven until the Welcome binds our avatar.
        self.net_client_side_setup(None, false);
        self.net_play_client = Some(client);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("🌐 joining {what} — your avatar binds when the server welcomes us"),
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
        let layers = self.project.build_layers();
        let terrain_vols = self.terrain_volumes(&layers);
        let mut ssim =
            floptle_physics::Sim::build_layered(&sworld, &terrain_vols, gravity, origin, layers);
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
        let predicted = self.net_client_side_setup(Some(1), true);
        if let Some(pe) = predicted {
            // The harness's hidden server needs physics sync force-enabled on
            // ITS copy of the predicted node too (the setup did the play
            // world's). Same node = same position in Replicated node order.
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
            if let Some(se) = spred
                && let Some(r) = sworld.get_mut::<floptle_core::Replicated>(se)
                && !r.physics
            {
                r.physics = true;
            }
        }
        let host = floptle_script::ScriptHost::new();
        host.set_project_root(self.project_root.clone());
        server.set_tick_dt(self.game_tick.step);
        // The hidden server runs REAL animators (state machines, no rendering):
        // same clip/controller registries as the editor, its own instances.
        let sanim = {
            let mut a = crate::anim::AnimSystem::default();
            a.rescan(&self.project_root);
            a
        };
        self.net_hidden = Some(HiddenServer {
            session: server,
            world: sworld,
            sim: ssim,
            host,
            peer: 1,
            next_tick: 0,
            history: floptle_net::LagHistory::new(),
            anim: sanim,
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
            my_peer: None,
        });
        hs.host.set_net_owners(Self::collect_net_owners(&hs.world));
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
        hs.host.set_hulls(hs.sim.body_hulls(&hs.world));
        // Dispatch the client intents that arrived by tick start (the pump
        // above), BEFORE this tick's scripts — with lag compensation: an rpc
        // stamped `{withInput = true}` gets a rewind scope holding every
        // networked node's pose + synced vars at the tick its sender PERCEIVED
        // (their stamp minus that node's interp delay, clamped to the rewind
        // window). `net.rewind(peer, fn)` applies it around the handler's
        // queries (`docs/netcode-design.md` §7).
        for r in hs.session.take_rpcs() {
            let scope = r
                .tick
                .map(|stamp| build_rewind_scope(&hs.world, &hs.session, &hs.history, st, r.sender, stamp));
            hs.host.set_rewind(scope);
            hs.host.dispatch_rpc(&mut hs.world, &r.name, &r.args, r.sender);
            hs.host.set_rewind(None);
        }
        let dir = self.project_root.join("scripts");
        let t = st as f32 * step;
        // Server scripts can query animator state (`anim:state()` etc.).
        hs.host.set_anim_info(anim::build_info(&hs.anim));
        hs.host.run(&mut hs.world, &dir, step, t);
        hs.host.run_fixed(&mut hs.world, step, t);
        // Animation runs server-side for real (scripts → anim → physics, the
        // same order as the play loop): `anim:play` transitions actual
        // controllers, scene-binding clips move actual transforms (they
        // replicate as transforms), and clip events fire into SERVER scripts —
        // hit windows are server-authoritative. Poses aren't rendered; the
        // (state, time) per layer replicates and every client samples locally.
        let anim_cmds = hs.host.take_anim_commands();
        let fired = anim::advance_animators(
            &mut hs.anim,
            &mut hs.world,
            &self.mesh_registry,
            step,
            anim_cmds,
        );
        for (eid, func) in fired {
            hs.host.call_function(&mut hs.world, eid, &func);
        }
        for msg in hs.anim.warnings.drain(..) {
            self.console.push(floptle_script::LogLevel::Warn, format!("[server] {msg}"), None);
        }
        hs.sim.world.colliders = hs.host.take_colliders();
        // Terrain edits aren't wired through the hidden-server harness yet (the ghost
        // world has no render/authority terrain of its own) — drop them loudly rather
        // than let the queue grow. Real sessions apply them on each machine normally.
        if !hs.host.take_terrain_ops().is_empty() && !self.net_terrain_warned {
            self.net_terrain_warned = true;
            self.console.push(
                floptle_script::LogLevel::Warn,
                "[server] terrain.* edits aren't supported in the local test harness yet \
                 (they apply normally offline and in real sessions)"
                    .into(),
                None,
            );
        }
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
                NetCmd::Rpc { name, args, to, .. } => {
                    let target = to.map(RpcTarget::Peer).unwrap_or(RpcTarget::All);
                    let _ = hs.session.send_rpc(&name, args, target);
                }
                NetCmd::Despawn { eid } => {
                    let ent =
                        hs.world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == eid);
                    if let Some(e) = ent {
                        hs.session.despawn(&mut hs.world, e);
                        hs.sim.remove_body(eid);
                    }
                }
                NetCmd::Spawn { path, .. } => self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "net.spawn(\"{path}\"): not supported in the local test harness yet — \
                         test avatar spawning over a real session (🌐 Host on LAN)"
                    ),
                    None,
                ),
                _ => {}
            }
        }
        // Synced vars + body states → snapshots out.
        let raw = hs.host.collect_synced();
        // Record this tick into the lag-comp history: every networked node's
        // post-tick pose plus its scripts' synced values — the world state
        // `net.rewind` can later re-judge combat against.
        {
            let mut hist: Vec<(u64, floptle_net::HistEntry)> = Vec::new();
            for (nid, e) in hs.session.net_entities() {
                let Some(tr) = hs.world.get::<Transform>(e) else { continue };
                let synced = raw
                    .iter()
                    .filter(|(eid, _, _)| *eid == e.index())
                    .map(|(_, kind, vars)| (kind.clone(), vars.clone()))
                    .collect();
                hist.push((
                    nid,
                    floptle_net::HistEntry {
                        pos: [tr.translation.x, tr.translation.y, tr.translation.z],
                        synced,
                    },
                ));
            }
            hs.history.record(st, hist);
        }
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
        hs.session
            .update_anim_states(anim::collect_net_states(&hs.world, &self.mesh_registry, &hs.anim));
        hs.session.tick_server(&hs.world, st);
        // Received events dispatch into the SERVER's scripts. (RPCs are NOT
        // dispatched here — they wait for the next tick's start, where the
        // colliders are lent and the lag-comp rewind scope can be staged.)
        let events = hs.session.take_events();
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
        // The hidden server renders nothing: drain its cosmetic queues so they
        // don't grow unboundedly. (Animator commands are REAL now — consumed
        // by the advance above, not drained.)
        let _ = hs.host.take_vfx_commands();
        let _ = hs.host.take_gizmos();
        let _ = hs.host.take_spawn_effects();
        let _ = hs.host.take_model_changes();
        let _ = hs.host.take_mouse_lock();
        if hs.host.take_scene_request().is_some() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "[server] scene.load isn't supported in the in-editor remote-player harness \
                 yet — host a real (LAN) session to test scene switching"
                    .into(),
                None,
            );
        }
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
        // Networked animators: remote proxies take the server's (state, time)
        // per layer, already delayed onto the interpolation timeline — the
        // local controllers then extrapolate by advancing normally until the
        // next update. Our own predicted node never appears here (its scripts
        // drive its animator locally).
        let anim_updates = cs.take_anim_updates();
        anim::apply_net_states(&mut self.anim, &mut self.world, &self.mesh_registry, anim_updates);
        // Reconcile our own node against authoritative states. On a real link
        // the server ticks in ITS OWN clock domain: translate each state's
        // tick back through the exact stamp→local map (correct even across
        // auto-lead nudges), falling back to offset arithmetic (harness: 0).
        let stamp_off = cs.input_stamp_offset();
        let updates: Vec<_> = cs
            .take_predicted_updates()
            .into_iter()
            .map(|(e, stick, state)| {
                let local = cs
                    .local_tick_for_stamp(stick)
                    .unwrap_or(((stick as i64) - stamp_off).max(0) as u64);
                (e, local, state)
            })
            .collect();
        let lead_events = cs.take_lead_events();
        let rpcs = cs.take_rpcs();
        let events = cs.take_events();
        let spawned = cs.take_spawned();
        let despawned = cs.take_despawned();
        let my_peer = cs.my_peer();
        let scene_switch = cs.take_scene_switch();
        for (delta, margin) in lead_events {
            self.console.push(
                floptle_script::LogLevel::Debug,
                format!("🌐 input lead retuned by {delta:+} tick(s) — server margin was {margin}"),
                None,
            );
        }
        // Replicated spawns/despawns materialize live: bodies register or go,
        // and ownership re-evaluates — a spawn owned by US becomes the
        // predicted avatar (the net.spawn player-avatar flow), everyone
        // else's becomes snapshot-driven.
        if !spawned.is_empty() || !despawned.is_empty() {
            for eid in &despawned {
                if let Some(sim) = self.sim.as_mut() {
                    sim.remove_body(*eid);
                }
            }
            let mut mesh = false;
            for (_, e, _) in &spawned {
                if let Some(sim) = self.sim.as_mut() {
                    sim.add_body_for(*e, &self.world);
                }
                mesh |= matches!(
                    self.world.get::<floptle_core::Matter>(*e),
                    Some(floptle_core::Matter::Mesh { .. })
                );
            }
            if mesh {
                self.load_script_swapped_models();
            }
            let was = self.net_predictor.as_ref().map(|(e, _)| *e);
            let owner = if self.net_hub.is_some() { Some(1) } else { my_peer };
            self.net_client_side_setup(owner, false);
            let now = self.net_predictor.as_ref().map(|(e, _)| *e);
            if now != was && let Some(pe) = now {
                let name = self
                    .world
                    .get::<floptle_core::Name>(pe)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| "?".into());
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("🎮 your avatar spawned — predicting \"{name}\""),
                    None,
                );
            }
        }
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
                // Hulls too — the live tick saw them, so the replay must
                // (other bodies at their CURRENT pose: the standard tradeoff).
                if let Some(sim) = self.sim.as_ref() {
                    self.script_host.set_hulls(sim.body_hulls(&self.world));
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
        // Client-side RPC/event dispatch (onRpc handlers, net.on) — with
        // colliders + hulls lent so handlers can raycast (cosmetic hit FX).
        let dispatching = !rpcs.is_empty() || !events.is_empty();
        if dispatching {
            if let Some(sim) = self.sim.as_mut() {
                self.script_host
                    .set_colliders(std::mem::take(&mut sim.world.colliders), sim.world.origin);
            }
            if let Some(sim) = self.sim.as_ref() {
                self.script_host.set_hulls(sim.body_hulls(&self.world));
            }
        }
        for r in rpcs {
            self.script_host.dispatch_rpc(&mut self.world, &r.name, &r.args, r.sender);
        }
        for ev in events {
            match ev {
                NetEvent::Connected => {
                    // Real link (no hub = independent tick clocks): translate
                    // our input stamps into the server's domain, leading it by
                    // the RTT plus a small margin so inputs labeled T arrive
                    // before the server simulates T. The harness's hidden
                    // server slaves to OUR clock instead — offset stays 0.
                    if self.net_hub.is_none() {
                        let mut my_peer = None;
                        if let Some(cs) = self.net_play_client.as_mut() {
                            my_peer = cs.my_peer();
                            if let Some(wt) = cs.welcome_tick() {
                                let rtt_ticks = (cs.stats(floptle_net::SERVER).rtt_ms / 1000.0
                                    * 60.0)
                                    .ceil() as i64;
                                let offset = wt as i64 + rtt_ticks + 3 - tick as i64;
                                cs.set_input_stamp_offset(offset);
                                // The Welcome-time RTT is a guess (often 0 —
                                // no ping has completed yet): let the session
                                // keep the lead tuned from server margin
                                // feedback instead of trusting it forever.
                                cs.set_auto_input_lead(true);
                                self.console.push(
                                    floptle_script::LogLevel::Debug,
                                    format!(
                                        "🌐 connected — input clock leads the server by {} \
                                         tick(s) (rtt {:.0} ms)",
                                        rtt_ticks + 3,
                                        cs.stats(floptle_net::SERVER).rtt_ms
                                    ),
                                    None,
                                );
                            }
                        }
                        // Deferred avatar bind: the Welcome told us WHO we are
                        // — claim the Predicted node in our slot (peer p owns
                        // scene Predicted node #p+1; #1 is the host's).
                        if let Some(p) = my_peer {
                            let bound = self.net_client_side_setup(Some(p), true);
                            if let Some(pe) = bound {
                                let name = self
                                    .world
                                    .get::<floptle_core::Name>(pe)
                                    .map(|n| n.0.clone())
                                    .unwrap_or_else(|| "?".into());
                                self.console.push(
                                    floptle_script::LogLevel::Debug,
                                    format!(
                                        "🎮 you are peer {p} — predicting \"{name}\" locally"
                                    ),
                                    None,
                                );
                            }
                        }
                    }
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
        if dispatching && let Some(sim) = self.sim.as_mut() {
            sim.world.colliders = self.script_host.take_colliders();
        }
        // The server put the session in a scene (a mid-session switch, or the
        // Welcome naming one we're not in): load it from OUR project, rebind
        // NetIds against it, and re-bind our avatar/prediction. Until the
        // rebind, the session drops scene-scoped traffic — nothing from the
        // new scene can land on the old world's entities.
        if let Some(scene) = scene_switch {
            if self.switch_scene_during_play(&scene).is_some() {
                Self::net_assign_scene_owners(&mut self.world);
                // Stale prediction history must not survive into the new scene.
                self.net_predictor = None;
                if let Some(cs) = self.net_play_client.as_mut() {
                    cs.rebind_scene(&self.world);
                }
                let owner = if self.net_hub.is_some() { Some(1) } else { my_peer };
                self.net_client_side_setup(owner, false);
            } else {
                // We can't load what the server is playing — leaving is the
                // only honest option (staying = a frozen desynced world).
                self.net_stop("the server switched to a scene this project doesn't have");
            }
        }
        // Smooth the visual correction.
        if let Some((_, pred)) = self.net_predictor.as_mut() {
            pred.decay_error();
        }
        // Mirror client state into Lua.
        let (rtt, my_peer) = self
            .net_play_client
            .as_ref()
            .map(|c| (c.stats(floptle_net::SERVER).rtt_ms, c.my_peer()))
            .unwrap_or((0.0, None));
        self.script_host.set_net_state(NetState {
            role: NetRoleState::Client,
            peers: Vec::new(),
            rtt_ms: rtt,
            my_peer,
        });
        self.script_host.set_net_owners(Self::collect_net_owners(&self.world));
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
            let layer = sim.tag_for(world, e);
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
                    sim.add_static_mesh(anchor, &verts, &indices, layer);
                }
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(
                            anchor,
                            Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z),
                            wt.rotation,
                            layer,
                        );
                    }
                    floptle_core::Shape::Plane => {
                        // The plane quad is flat in Z: a thin box collider.
                        sim.add_static_box(
                            anchor,
                            Vec3::new(0.7 * s.x, 0.7 * s.y, 0.02 * s.z.max(1.0)),
                            wt.rotation,
                            layer,
                        );
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element(), layer);
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z), layer);
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
        self.net_remote_predicted.clear();
        self.net_lobby_code = None;
        self.net_history = floptle_net::LagHistory::new();
        self.net_hub = None;
        self.net_scene_doc = None;
        self.script_host.set_frame_filter(std::collections::HashSet::new());
        // Back to offline rules: extra player slots idle again (this also
        // clears the session's script filter for everything else).
        if self.playing {
            self.net_apply_offline_slots();
        } else {
            self.script_host.set_script_filter(std::collections::HashSet::new());
        }
        self.script_host.clear_net_state();
        self.script_host.set_net_state(NetState::default());
        self.script_host.set_net_owners(HashMap::new());
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
        // Accepts a scene file (its first node spawns) or a PREFAB — by name
        // ("bullet") or path ("prefabs/bullet.prefab.ron"). Replication is
        // single-node, so a multi-node prefab spawns its first root only.
        let first = if path.ends_with(floptle_scene::PREFAB_EXT)
            || self.resolve_prefab_request(path).is_some()
        {
            let full = self
                .resolve_prefab_request(path)
                .unwrap_or_else(|| self.project_root.join(path));
            match crate::prefab::load_prefab_docs(&full) {
                Ok(docs) => docs.into_iter().find(|d| d.parent.is_none()),
                Err(e) => {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("net.spawn(\"{path}\"): {e}"),
                        None,
                    );
                    return;
                }
            }
        } else {
            let full = self.project_root.join(path);
            match std::fs::read_to_string(&full)
                .map_err(|e| e.to_string())
                .and_then(|s| floptle_scene::from_ron(&s).map_err(|e| e.to_string()))
            {
                Ok(d) => d.nodes.first().cloned(),
                Err(e) => {
                    self.console.push(
                        floptle_script::LogLevel::Warn,
                        format!("net.spawn(\"{path}\"): {e}"),
                        None,
                    );
                    return;
                }
            }
        };
        let Some(mut node) = first else {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("net.spawn(\"{path}\"): no nodes in it"),
                None,
            );
            return;
        };
        node.parent = None;
        if let Some(p) = pos {
            node.transform.translation = p;
        }
        let s = self.net_server.as_mut().unwrap();
        let e = s.spawn_doc(&mut self.world, &node, owner);
        // A spawned mesh needs its GPU import like any script-swapped model.
        if let floptle_scene::MatterDoc::Mesh { .. } = node.matter {
            self.load_script_swapped_models();
        }
        // Runtime spawns simulate immediately: register a live physics body.
        if let Some(sim) = self.sim.as_mut() {
            sim.add_body_for(e, &self.world);
        }
        // A remote player's avatar (`net.spawn(..., { owner = peer })` +
        // Predicted): its scripts run with the OWNER's replayed input, not
        // the host's keyboard.
        if let Some(rep) = self.world.get::<floptle_core::Replicated>(e)
            && rep.mode == ReplicationMode::Predicted
            && let Some(p) = rep.owner
        {
            self.net_remote_predicted.push((e, p));
            self.net_apply_host_filters();
        }
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
