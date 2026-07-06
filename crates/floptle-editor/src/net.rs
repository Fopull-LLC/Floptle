//! In-editor multiplayer: drives `floptle-net` sessions from the play loop and
//! provides the **"Host & Join locally"** test harness (`docs/netcode-design.md`
//! §12/§13.3) — the play world hosts, a hidden ghost world joins over an
//! in-process [`floptle_net::MemoryHub`] with latency/loss sliders, and cyan
//! ghost gizmos show exactly what a remote client would see (interp delay,
//! loss stutter, the works) without a second editor instance.

use std::collections::HashMap;

use floptle_core::transform::Transform;
use floptle_core::{Entity, World};
use floptle_net::{NetEvent, NetSession, RpcTarget};
use floptle_script::{NetCmd, NetRoleState, NetState};

use crate::Editor;

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

    /// Tear the in-editor session down (Stop, `net.leave()`, or the panel).
    pub(crate) fn net_stop(&mut self, why: &str) {
        if self.net_server.is_none() && self.net_client.is_none() {
            return;
        }
        self.net_server = None;
        self.net_client = None;
        self.net_hub = None;
        self.net_scene_doc = None;
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

    /// Cyan ghost gizmos: where the ghost client's world thinks every
    /// replicated node is — drawn into the normal gizmo overlay each frame.
    pub(crate) fn net_ghost_gizmos(&mut self) {
        if !self.net_ghosts {
            return;
        }
        let Some((_, cw)) = &self.net_client else { return };
        for (e, _) in cw.query::<floptle_core::Replicated>() {
            if let Some(tr) = cw.get::<Transform>(e) {
                self.script_gizmos.push(floptle_script::GizmoCmd::Sphere {
                    center: [
                        tr.translation.x as f32,
                        tr.translation.y as f32,
                        tr.translation.z as f32,
                    ],
                    radius: 0.45,
                    color: [0.25, 0.9, 1.0],
                });
            }
        }
    }
}
