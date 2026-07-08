//! # floptle-net — open, transport-agnostic netcode (ADR-0022)
//!
//! Phase 2b of `docs/netcode-design.md`: server-authoritative replication over
//! a swappable [`Transport`]. The pieces:
//!
//! - [`transport`] — the `Transport` trait + [`MemoryHub`] (in-process loopback
//!   with simulated tick-based latency/loss — tests + the editor's
//!   "Host & Join locally" harness).
//! - [`wire`] — the postcard-encoded message vocabulary.
//! - [`value`] — [`NetValue`], the guarded Lua value tree (depth ≤ 4, ≤ 1 KB).
//! - [`session`] — [`NetSession`]: hello/welcome, deterministic scene ids,
//!   spawn/despawn, changed-only snapshots + keyframes, `synced` vars, RPC,
//!   client-side interpolation.
//!
//! Prediction (2c), lag compensation (2d), and the QUIC transport + relay (2e)
//! build on these seams without changing the game-facing API.

pub mod lagcomp;
pub mod predict;
pub mod quic;
pub mod relay;
pub mod session;
pub mod transport;
pub mod value;
pub mod wire;

pub use lagcomp::{HistEntry, LagHistory, MAX_REWIND_TICKS};
pub use quic::{QuicClient, QuicServer};
pub use relay::{RelayClient, RelayHost, RelayServer};
pub use predict::{PredictedState, Predictor, DEFAULT_EPSILON};
pub use session::{BodyStates, NetEvent, NetRole, NetSession, ReceivedRpc, RpcTarget, SyncedVars};
pub use wire::{InputCmd, NetInput};
pub use transport::{
    Channel, Incoming, LinkStats, MemoryHub, MemoryTransport, PeerId, Transport, SERVER,
};
pub use value::{NetValue, ValueError, MAX_VALUE_BYTES, MAX_VALUE_DEPTH};

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;
    use floptle_core::transform::Transform;
    use floptle_core::{Replicated, World};

    /// A world with `n` replicated nodes at x = 0, 10, 20, …
    fn world_with(n: usize) -> (World, Vec<floptle_core::Entity>) {
        let mut w = World::default();
        let mut ents = Vec::new();
        for i in 0..n {
            let e = w.spawn();
            w.insert(e, Transform::from_translation(DVec3::new(10.0 * i as f64, 0.0, 0.0)));
            w.insert(e, Replicated::default());
            ents.push(e);
        }
        (w, ents)
    }

    /// Drive both sessions `ticks` times (server world is authoritative;
    /// `step` mutates it before each server tick). Returns the next tick.
    #[allow(clippy::too_many_arguments)]
    fn run(
        hub: &MemoryHub,
        server: &mut NetSession,
        s_world: &mut World,
        client: &mut NetSession,
        c_world: &mut World,
        from: u64,
        ticks: u64,
        mut step: impl FnMut(&mut World, u64),
    ) -> u64 {
        for t in from..from + ticks {
            hub.set_now(t);
            step(s_world, t);
            server.tick_server(s_world, t);
            client.tick_client(c_world);
        }
        from + ticks
    }

    fn connect_pair(hub: &MemoryHub) -> (NetSession, NetSession) {
        let server = NetSession::server(Box::new(hub.server_endpoint()));
        let client = NetSession::client(Box::new(hub.connect()));
        (server, client)
    }

    #[test]
    fn transform_replicates_and_interpolates() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with(2);
        let (mut cw, ce) = world_with(2);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Move node 0 on the server steadily; run long enough for interp delay.
        let end = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 60, |w, t| {
            if let Some(tr) = w.get_mut::<Transform>(se[0]) {
                tr.translation.x = t as f64 * 0.1;
            }
        });
        assert!(client.is_connected());
        let cx = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        let sx = sw.get::<Transform>(se[0]).unwrap().translation.x;
        assert!(cx > 0.5, "client must have received motion, x={cx}");
        assert!(
            cx <= sx,
            "client renders BEHIND the server (interp delay), client {cx} vs server {sx}"
        );
        // Node 1 never moved after the first keyframe: stays put.
        let c1 = cw.get::<Transform>(ce[1]).unwrap().translation.x;
        assert!((c1 - 10.0).abs() < 1e-9, "static node stays at its scene position, got {c1}");
        let _ = end;
    }

    #[test]
    fn survives_heavy_snapshot_loss() {
        let hub = MemoryHub::new();
        hub.set_conditions(0, 0.5); // drop half of all unreliable traffic
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with(1);
        let (mut cw, ce) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Move, then STOP — the final resting position must still arrive even
        // if the snapshot that carried it was dropped (keyframes heal it).
        let mid = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 120, |w, t| {
            if let Some(tr) = w.get_mut::<Transform>(se[0]) {
                tr.translation.x = (t.min(60)) as f64 * 0.1; // stops at 6.0
            }
        });
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid, 120, |_, _| {});
        let cx = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        assert!(
            (cx - 6.0).abs() < 1e-6,
            "resting state must converge through 50% loss (keyframes), got {cx}"
        );
    }

    #[test]
    fn rpc_both_ways_with_stamped_sender() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(0);
        let (mut cw, _) = world_with(0);

        client
            .send_rpc(
                "buy_item",
                NetValue::Table(vec![(NetValue::Str("id".into()), NetValue::Num(7.0))]),
                RpcTarget::Server,
            )
            .unwrap();
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 3, |_, _| {});
        let got = server.take_rpcs();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "buy_item");
        assert_eq!(got[0].sender, 1, "sender must be the transport identity, not payload");

        server.send_rpc("explode", NetValue::Num(3.0), RpcTarget::All).unwrap();
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 4, 3, |_, _| {});
        let got = client.take_rpcs();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "explode");
        assert_eq!(got[0].sender, SERVER);

        // Guardrails: an oversized arg is rejected at queue time.
        let big = NetValue::Str("x".repeat(MAX_VALUE_BYTES + 1));
        assert!(server.send_rpc("too_big", big, RpcTarget::All).is_err());
    }

    #[test]
    fn synced_vars_reach_the_client_changed_only() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with(1);
        let (mut cw, ce) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        server.update_synced(vec![(
            se[0],
            "combat".into(),
            vec![("hp".into(), NetValue::Num(100.0)), ("parrying".into(), NetValue::Bool(false))],
        )]);
        let mid = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});
        // The join baseline + the first keyframe may BOTH deliver the initial
        // values (idempotent last-write-wins) — assert content, not count.
        let got = client.take_synced();
        assert!(!got.is_empty());
        for (e, script, vars) in &got {
            assert_eq!(*e, ce[0]);
            assert_eq!(script, "combat");
            assert_eq!(vars.len(), 2);
        }

        // Unchanged values are NOT resent (until a keyframe).
        let mid2 = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid, 4, |_, _| {});
        assert!(client.take_synced().is_empty(), "unchanged vars must not resend");

        // A change flows through.
        server.update_synced(vec![(
            se[0],
            "combat".into(),
            vec![("hp".into(), NetValue::Num(55.0)), ("parrying".into(), NetValue::Bool(false))],
        )]);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid2, 4, |_, _| {});
        let got = client.take_synced();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].2, vec![("hp".to_string(), NetValue::Num(55.0))]);
    }

    #[test]
    fn runtime_spawn_despawn_and_late_join() {
        let hub = MemoryHub::new();
        let mut server = NetSession::server(Box::new(hub.server_endpoint()));
        let (mut sw, _) = world_with(1);
        server.register_scene(&sw);

        // Spawn a runtime node and move it, BEFORE any client exists.
        let node = floptle_scene::NodeDoc {
            name: "arrow".into(),
            transform: floptle_scene::TransformDoc {
                translation: [5.0, 1.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
                scale: [1.0, 1.0, 1.0],
            },
            matter: floptle_scene::MatterDoc::Primitive {
                shape: floptle_scene::ShapeDoc::Sphere,
                color: [1.0, 0.2, 0.2],
            },
            scripts: Vec::new(),
            material: None,
            rigidbody: None,
            mesh_collider: false,
            collidable: false,
            visible: true,
            cast_shadow: true,
            anim_controller: None,
            particles: None,
            parent: None,
            attachment: None,
            net: None,
            ui_layer: None,
            ui: None,
        };
        let arrow = server.spawn_doc(&mut sw, &node, Some(1));
        // Tick the empty-peers server a few times.
        for t in 1..5u64 {
            hub.set_now(t);
            server.tick_server(&sw, t);
        }

        // NOW a client joins late: it must receive the spawn + a baseline.
        let mut client = NetSession::client(Box::new(hub.connect()));
        let (mut cw, _) = world_with(1);
        client.register_scene(&cw);
        let before = cw.query::<Replicated>().count();
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 5, 6, |_, _| {});
        assert!(client.is_connected());
        let after = cw.query::<Replicated>().count();
        assert_eq!(after, before + 1, "late joiner must materialize the runtime spawn");
        // The spawned node carries its owner.
        let spawned = cw
            .query::<Replicated>()
            .find(|(_, r)| r.owner == Some(1))
            .map(|(e, _)| e)
            .expect("owner must replicate");
        let pos = cw.get::<Transform>(spawned).unwrap().translation;
        assert!((pos.x - 5.0).abs() < 1e-9);

        // Despawn reaches the client too.
        server.despawn(&mut sw, arrow);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 11, 3, |_, _| {});
        assert_eq!(cw.query::<Replicated>().count(), before, "despawn must replicate");
    }

    #[test]
    fn input_commands_flow_and_predicted_states_route_to_reconcile() {
        use floptle_core::ReplicationMode;
        // The 2c plumbing end-to-end over a LOSSY link: client inputs reach the
        // server (redundant window healing 30% loss), physics-synced snapshot
        // entries carry vel/grounded, and the client's OWN predicted node's
        // authoritative states go to the reconcile queue — never interpolation.
        let hub = MemoryHub::new();
        hub.set_conditions(0, 0.3);
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with(1);
        let (mut cw, ce) = world_with(1);
        let rep = Replicated {
            mode: ReplicationMode::Predicted,
            owner: Some(1),
            physics: true,
            ..Default::default()
        };
        sw.insert(se[0], rep);
        cw.insert(ce[0], rep);
        server.register_scene(&sw);
        client.register_scene(&cw);

        let mut exact_hits = 0u32;
        for t in 1..=60u64 {
            hub.set_now(t);
            client.send_input(
                t,
                NetInput { keys_down: vec![format!("k{t}")], ..Default::default() },
            );
            client.tick_client(&mut cw); // ships the input window
            server.pump_server(&sw, t); // tick START: consume inputs
            let inp = server.input_for(1, t);
            if inp.keys_down == vec![format!("k{t}")] {
                exact_hits += 1;
            }
            // server "simulates": the node moves, body state refreshed
            sw.get_mut::<Transform>(se[0]).unwrap().translation.x = t as f64;
            server.update_body_states(vec![(se[0], [1.0, 0.0, 0.0], true)]);
            server.tick_server(&sw, t);
        }
        hub.set_now(61);
        client.tick_client(&mut cw);

        // Same-tick consumption at 30% loss ⇒ exact rate ≈ 1 − loss (the
        // redundant window pays off when consumption lags sends — the driver's
        // clock-skew margin, 2c-ii). Misses fall back to repeat-last, so the
        // character never freezes. Deterministic rng ⇒ a stable count.
        assert!(exact_hits >= 40, "exact inputs must survive loss, got {exact_hits}/60");

        let upd = client.take_predicted_updates();
        assert!(!upd.is_empty(), "authoritative states must reach the reconcile queue");
        assert!(upd.iter().all(|(e, _, _)| *e == ce[0]));
        let (_, _, last) = upd.last().unwrap();
        assert_eq!(last.vel, [1.0, 0.0, 0.0], "physics-synced entries carry velocity");
        assert!(last.grounded, "…and grounded");
        // The predicted node was NOT interpolated on its owner.
        assert_eq!(
            cw.get::<Transform>(ce[0]).unwrap().translation.x,
            0.0,
            "own predicted node must not be server-interpolated"
        );
    }

    #[test]
    fn with_input_rpcs_carry_the_perceived_tick() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with(1);
        let (mut cw, _) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Run until snapshots have flowed, so the client HAS a perceived tick.
        let mid = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 10, |w, t| {
            w.get_mut::<Transform>(se[0]).unwrap().translation.x = t as f64;
        });
        server.take_rpcs();
        client
            .send_rpc_stamped("swing", NetValue::Num(1.0), RpcTarget::Server, true)
            .unwrap();
        client
            .send_rpc_stamped("chat", NetValue::Num(2.0), RpcTarget::Server, false)
            .unwrap();
        let end = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid, 3, |_, _| {});
        let got = server.take_rpcs();
        assert_eq!(got.len(), 2);
        let swing = got.iter().find(|r| r.name == "swing").unwrap();
        let stamp = swing.tick.expect("withInput must stamp the perceived tick");
        assert!(stamp < end && stamp >= mid.saturating_sub(4), "a recent server tick: {stamp}");
        assert_eq!(got.iter().find(|r| r.name == "chat").unwrap().tick, None);

        // Server → client RPCs never stamp.
        server.send_rpc_stamped("boom", NetValue::Nil, RpcTarget::All, true).unwrap();
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, end, 3, |_, _| {});
        assert_eq!(client.take_rpcs()[0].tick, None);
    }

    #[test]
    fn input_stamp_offset_translates_clock_domains() {
        // A real link runs two independent tick clocks: the client stamps its
        // inputs into the SERVER's domain via the offset (harness leaves it 0).
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(0);
        let (mut cw, _) = world_with(0);
        let mid = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 3, |_, _| {});
        assert!(client.welcome_tick().is_some(), "Welcome carries the server tick");

        client.set_input_stamp_offset(100);
        client.send_input(5, NetInput { keys_down: vec!["w".into()], ..Default::default() });
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid, 3, |_, _| {});
        server.pump_server(&sw, 105);
        let inp = server.input_for(1, 105);
        assert_eq!(inp.keys_down, vec!["w".to_string()], "local tick 5 lands at server tick 105");
        assert_eq!(server.late_inputs(), 0, "the stamped tick was an exact hit");
    }

    #[test]
    fn auto_lead_heals_a_late_input_clock() {
        // A client whose lead is too small (Welcome-time RTT guess, a frame
        // hitch, clock drift) stamps inputs that arrive AFTER the server
        // simulated their tick — repeat-last forever, misprediction storms.
        // The server's InputAck margins must steer the offset back into band.
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(0);
        let (mut cw, _) = world_with(0);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 3, |_, _| {});
        let peer = server.peers()[0];

        client.set_input_stamp_offset(-5); // inputs land 5 ticks in the past
        client.set_auto_input_lead(true);
        let mut drive = |server: &mut NetSession, client: &mut NetSession, from: u64, n: u64| {
            for t in from..from + n {
                hub.set_now(t);
                client.send_input(t, NetInput::default());
                client.tick_client(&mut cw); // ships window + applies acks
                server.pump_server(&sw, t);
                let _ = server.input_for(peer, t); // consume + measure margin
                server.tick_server(&sw, t); // acks ride the tick
            }
            from + n
        };
        let mid = drive(&mut server, &mut client, 4, 300);
        assert!(
            client.input_stamp_offset() >= 1,
            "auto-lead must have raised the offset out of the hole, got {}",
            client.input_stamp_offset()
        );
        // Once retuned, inputs hit their tick exactly: no NEW late inputs.
        let late_before = server.late_inputs();
        let _ = drive(&mut server, &mut client, mid, 120);
        assert_eq!(server.late_inputs(), late_before, "retuned clock must stop running late");
        let (margin, _) = client.input_ack().expect("acks received");
        assert!(margin >= 1, "server-side margin back in band, got {margin}");
        // And reconcile's stamp→local map survives the nudges: the newest
        // stamp maps back to the local tick that sent it.
        let last_local = mid + 119;
        let stamp = (last_local as i64 + client.input_stamp_offset()) as u64;
        assert_eq!(client.local_tick_for_stamp(stamp), Some(last_local));
    }

    #[test]
    fn join_leave_events_fire() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(0);
        let (mut cw, _) = world_with(0);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 3, |_, _| {});
        assert!(server.take_events().contains(&NetEvent::PeerJoined(1)));
        assert!(client.take_events().contains(&NetEvent::Connected));
        assert_eq!(server.peers(), &[1]);

        hub.disconnect(1);
        hub.set_now(10);
        server.tick_server(&sw, 10);
        assert!(server.take_events().contains(&NetEvent::PeerLeft(1)));
        assert!(server.peers().is_empty());
    }
}
