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

pub mod impair;
pub mod interest;
pub mod lagcomp;
pub mod predict;
pub mod quic;
pub mod relay;
pub mod replay;
pub mod rollback;
pub mod session;
pub mod transport;
pub mod value;
pub mod wire;

pub use impair::{ImpairHandle, Impaired, Impairment, IMPAIR_ENV};
pub use interest::{Candidate, InterestConfig, InterestSets, InterestStat, PeerInterest};
pub use lagcomp::{HistEntry, LagHistory, MAX_REWIND_TICKS};
pub use quic::{QuicClient, QuicServer};
pub use relay::{RelayClient, RelayHost, RelayServer};
pub use predict::{PredictedState, Predictor, DEFAULT_EPSILON};
pub use replay::{InputLog, LogEntry, LogError};
pub use rollback::{
    Correction, ResolvedInput, Rollback, DEFAULT_INPUT_DELAY, DEFAULT_MAX_DEPTH, MAX_DELAY,
};
pub use session::{
    AnimSrcLayer, AnimStates, BodyStates, JoinState, NetEvent, NetRole, NetSession, ReceivedRpc, RpcTarget,
    SyncedVars,
};
pub use wire::{AnimEntry, AnimLayerWire, InputCmd, NetInput, CHECKSUM_EVERY, PROTO_VERSION};
pub use transport::{
    Channel, Incoming, LinkStats, MemoryHub, MemoryTransport, PeerId, Transport, SERVER,
};
pub use value::{Fnv, NetValue, ValueError, MAX_VALUE_BYTES, MAX_VALUE_DEPTH};

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::DVec3;
    use floptle_core::transform::Transform;
    use floptle_core::{Replicated, World};
    use crate::interest::InterestConfig;

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
        let server = NetSession::server(Box::new(hub.server_endpoint()), 0);
        let client = NetSession::client(Box::new(hub.connect()), 0);
        (server, client)
    }

    #[test]
    fn a_mismatched_input_map_is_refused_at_hello() {
        // Input commands index actions by their POSITION in input.ron. Two
        // builds with differently-ordered maps would decode each other's
        // commands as the wrong actions and just play wrong, with no error
        // anywhere — so the handshake has to catch it.
        let hub = MemoryHub::new();
        let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0xABCD);
        let mut client = NetSession::client(Box::new(hub.connect()), 0x1234);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);

        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});

        let refused = client
            .take_events()
            .into_iter()
            .any(|e| matches!(e, NetEvent::Disconnected(r) if r.contains("input.ron")));
        assert!(refused, "a differing action map must refuse the connection");
    }

    #[test]
    fn a_matching_input_map_connects() {
        let hub = MemoryHub::new();
        let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0xABCD);
        let mut client = NetSession::client(Box::new(hub.connect()), 0xABCD);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);

        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});

        let connected =
            client.take_events().into_iter().any(|e| matches!(e, NetEvent::Connected));
        assert!(connected, "identical maps must connect");
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
        let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
        let (mut sw, _) = world_with(1);
        server.register_scene(&sw);

        // Spawn a runtime node and move it, BEFORE any client exists.
        let node = floptle_scene::NodeDoc {
            id: None,
            parent_id: None,
            terrain_gen: None,
            disabled: false,
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
            object_materials: Default::default(),
            rigidbody: None,
            celestial: None,
            mesh_collider: false,
            paint: None,
            tex_paint: None,
            collidable: false,
            trigger: false,
            visible: true,
            cast_shadow: true,
            anim_controller: None,
            particles: None,
            parent: None,
            attachment: None,
            net: None,
            ui_layer: None,
            ui: None,
            audio: None,
            layer: None,
            tags: Vec::new(),
            sorting: None,
        };
        let arrow = server.spawn_doc(&mut sw, &node, Some(1));
        // Tick the empty-peers server a few times.
        for t in 1..5u64 {
            hub.set_now(t);
            server.tick_server(&sw, t);
        }

        // NOW a client joins late: it must receive the spawn + a baseline.
        let mut client = NetSession::client(Box::new(hub.connect()), 0);
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
                NetInput { actions: t, ..Default::default() },
            );
            client.tick_client(&mut cw); // ships the input window
            server.pump_server(&sw, t); // tick START: consume inputs
            let inp = server.input_for(1, t);
            if inp.actions == t {
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
        client.send_input(5, NetInput { actions: 0b101, ..Default::default() });
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, mid, 3, |_, _| {});
        server.pump_server(&sw, 105);
        let inp = server.input_for(1, 105);
        assert_eq!(inp.actions, 0b101, "local tick 5 lands at server tick 105");
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

    #[test]
    fn animator_replicates_transitions_not_ticking_clocks() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (sw, se) = world_with(1);
        let (mut cw, ce) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // A looping 2 s Idle whose clock just advances, with ONE transition to
        // state 1 at tick 100. The send-side predictor must cover the steady
        // clock (zero non-keyframe sends), and the transition must arrive on
        // the interp-delayed timeline.
        let dt = 1.0 / 60.0;
        let mut updates: Vec<(u64, AnimEntry)> = Vec::new();
        let (mut t_anim, mut state) = (0.0f32, 0u16);
        for t in 1..=200u64 {
            hub.set_now(t);
            if t == 100 {
                state = 1;
                t_anim = 0.0;
            }
            t_anim = (t_anim + dt).rem_euclid(2.0);
            server.update_anim_states(vec![(
                se[0],
                0,
                1.0,
                vec![AnimSrcLayer {
                    state: Some(state),
                    t: t_anim,
                    weight: 1.0,
                    dur: 2.0,
                    looped: true,
                    rate: 1.0,
                }],
            )]);
            server.tick_server(&sw, t);
            client.tick_client(&mut cw);
            for (e, en) in client.take_anim_updates() {
                assert_eq!(e, ce[0], "updates resolve to the right entity");
                updates.push((t, en));
            }
        }
        // The baseline applies promptly (a joiner doesn't idle out the delay)…
        assert!(updates.first().is_some_and(|(t, _)| *t <= 10), "baseline arrived late");
        // …the steady loop then sends NOTHING between keyframes (the join
        // baseline + the tick-2 cadence keyframe both land before ~10)…
        let quiet = updates.iter().filter(|(t, _)| (12..59).contains(t)).count();
        assert_eq!(quiet, 0, "an undisturbed loop must cost zero non-keyframe sends");
        assert!(updates.len() < 10, "got {} updates for one transition", updates.len());
        // …and the transition lands, delayed like the transforms around it.
        let hit = updates
            .iter()
            .find(|(_, en)| en.layers.first().is_some_and(|l| l.state_opt() == Some(1)))
            .expect("the transition must replicate");
        assert!(
            (100..=120).contains(&hit.0),
            "transition applies on the interp-delayed timeline, got tick {}",
            hit.0
        );
    }

    /// The scene-switch handshake end to end: the Welcome names the session's
    /// scene, a mid-session switch is announced, old-epoch state in flight is
    /// DROPPED (never applied to the new scene's same-numbered ids), and after
    /// the client rebinds, replication resumes against the new scene.
    #[test]
    fn scene_switch_rebinds_and_drops_stale_epochs() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        server.set_scene("scenes/first.ron");
        let (mut sw, se) = world_with(2);
        let (mut cw, ce) = world_with(2);
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Join: the Welcome tells the client which scene the session runs.
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 10, |_, _| {});
        assert!(client.is_connected());
        assert_eq!(client.take_scene_switch().as_deref(), Some("scenes/first.ron"));
        // The driver is already in that scene — it rebinds and traffic flows.
        client.rebind_scene(&cw);
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 40, |w, tick| {
            if let Some(tr) = w.get_mut::<Transform>(se[0]) {
                tr.translation.x = tick as f64 * 0.1;
            }
        });
        let moved = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        assert!(moved > 0.5, "pre-switch replication works, got {moved}");

        // SWITCH: the server flips to a different scene (different shape too).
        let (mut sw2, se2) = world_with(1);
        server.switch_scene("scenes/arena.ron");
        server.rebind_scene(&sw2);
        // Server ticks keep flowing while the client hasn't rebound yet: NONE
        // of the new scene's state may land on the OLD world's entities.
        let frozen = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        let t = run(&hub, &mut server, &mut sw2, &mut client, &mut cw, t, 20, |w, tick| {
            if let Some(tr) = w.get_mut::<Transform>(se2[0]) {
                tr.translation.x = 500.0 + tick as f64;
            }
        });
        assert_eq!(client.take_scene_switch().as_deref(), Some("scenes/arena.ron"));
        let still = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        assert!(
            (still - frozen).abs() < 1e-9,
            "old-scene entities must freeze once the switch is announced, {frozen} → {still}"
        );

        // The client loads the new scene locally and rebinds: replication
        // resumes against the NEW ids (keyframes heal anything dropped).
        let (mut cw2, ce2) = world_with(1);
        client.rebind_scene(&cw2);
        let _ = run(&hub, &mut server, &mut sw2, &mut client, &mut cw2, t, 80, |w, tick| {
            if let Some(tr) = w.get_mut::<Transform>(se2[0]) {
                tr.translation.x = 500.0 + tick as f64;
            }
        });
        let nx = cw2.get::<Transform>(ce2[0]).unwrap().translation.x;
        assert!(nx > 400.0, "post-switch replication resumes in the new scene, got {nx}");
    }

    // -----------------------------------------------------------------------
    // Rollback wire (docs/rollback-netcode-design.md §5, §6)
    // -----------------------------------------------------------------------

    /// An arbitrary but fixed match seed — the point is that both peers get
    /// the same one, not what it is.
    const MATCH_SEED: u64 = 0x0BAD_F00D_1234_5678;

    fn held(actions: u64) -> NetInput {
        NetInput { actions, ..Default::default() }
    }

    /// The fan-out: peers send the host their own applied-tick inputs and the
    /// host echoes everyone's to everyone, so every peer can simulate every
    /// fighter. Nothing about a hit ever crosses the wire — only inputs do.
    /// A world whose node 0 is a `Rollback` fighter and node 1 an ordinary
    /// authority prop — the mixed scene the guard has to tell apart.
    fn world_with_a_fighter() -> (World, Vec<floptle_core::Entity>) {
        let (mut w, ents) = world_with(2);
        w.insert(
            ents[0],
            Replicated {
                mode: floptle_core::ReplicationMode::Rollback,
                ..Default::default()
            },
        );
        (w, ents)
    }

    /// Before `RollbackStart` there is no driver anywhere, so a rollback node
    /// is snapshot-driven like anything else. This is what puts a joining
    /// client's fighters where the host has them before the match begins, and
    /// it must keep working — the guard is about the driver being live, not
    /// about the mode.
    #[test]
    fn before_the_match_starts_a_fighter_still_follows_the_host() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with_a_fighter();
        let (mut cw, ce) = world_with_a_fighter();
        server.register_scene(&sw);
        client.register_scene(&cw);

        run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 60, |w, t| {
            if let Some(tr) = w.get_mut::<Transform>(se[0]) {
                tr.translation.x = t as f64 * 0.1;
            }
        });
        let cx = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        assert!(cx > 0.5, "with no match running the fighter is ordinary synced state, x={cx}");
    }

    /// Once the match is on, every peer simulates the fighter from the shared
    /// input stream — so the host's snapshot of it is not authority, it is a
    /// second opinion a round trip late. Applying it drags the node between the
    /// driver's tick pose and an interpolated one from the past, every frame,
    /// while the checksums (which hash body state, not transforms) stay green:
    /// a match that LOOKS broken and REPORTS healthy.
    #[test]
    fn once_the_match_starts_the_host_stops_moving_the_fighter() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = world_with_a_fighter();
        let (mut cw, ce) = world_with_a_fighter();
        server.register_scene(&sw);
        client.register_scene(&cw);

        // Connect, then start the match.
        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 8, |_, _| {});
        server.set_rollback(true, 2, 0xFEED_FACE);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 8, |_, _| {});
        assert!(client.take_rollback_start().is_some(), "the match must have been announced");

        // The client's own copies, frozen at whatever the pre-match snapshots
        // left them at. From here the driver owns the fighter and nothing on
        // the wire may move it.
        let fighter_before = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        let prop_before = cw.get::<Transform>(ce[1]).unwrap().translation.x;

        run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 90, |w, t| {
            for e in [se[0], se[1]] {
                if let Some(tr) = w.get_mut::<Transform>(e) {
                    tr.translation.x = 100.0 + t as f64;
                }
            }
        });

        let fighter_after = cw.get::<Transform>(ce[0]).unwrap().translation.x;
        let prop_after = cw.get::<Transform>(ce[1]).unwrap().translation.x;
        assert!(
            (fighter_after - fighter_before).abs() < 1e-9,
            "a live driver owns the fighter — the host's snapshot must not touch it \
             (was {fighter_before}, now {fighter_after})"
        );
        assert!(
            prop_after > prop_before + 50.0,
            "the ordinary prop in the same scene still replicates normally \
             (was {prop_before}, now {prop_after})"
        );
    }

    /// A scene switch ends the match on every peer, not just the host's.
    ///
    /// The state ring is indexed by node position and the slot order comes from
    /// scene order, so nothing about a match survives the scene it was played
    /// in. The host restarts its own driver; without this the CLIENT kept
    /// `rollback` set, went on refusing the new scene's snapshots for nodes its
    /// dead driver still thought it owned, and waited for a `RollbackStart`
    /// that had already been and gone.
    #[test]
    fn a_scene_switch_ends_the_match_on_the_client_too() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with_a_fighter();
        let (mut cw, _) = world_with_a_fighter();
        server.register_scene(&sw);
        client.register_scene(&cw);

        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 8, |_, _| {});
        server.set_rollback(true, 2, 0xC0FF_EE00);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 8, |_, _| {});
        assert!(client.take_rollback_start().is_some(), "the match started");
        assert!(client.is_rollback(), "the client is in a match");

        server.switch_scene("scenes/other.ron");
        run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 8, |_, _| {});
        assert!(
            !client.is_rollback(),
            "a scene switch ends the match — the next one arrives with its own RollbackStart"
        );
    }

    /// Peer 1's avatar at the origin, a neighbour 20 m away, and something
    /// 500 m away that no radius worth having would include.
    fn interest_world() -> (World, Vec<floptle_core::Entity>) {
        let mut w = World::default();
        let mut ents = Vec::new();
        for (i, x) in [0.0_f64, 20.0, 500.0].into_iter().enumerate() {
            let e = w.spawn();
            w.insert(e, Transform::from_translation(DVec3::new(x, 0.0, 0.0)));
            w.insert(
                e,
                Replicated {
                    // Node 0 is the joiner's avatar: that is the eye every
                    // distance here is measured from.
                    owner: (i == 0).then_some(1),
                    ..Default::default()
                },
            );
            ents.push(e);
        }
        (w, ents)
    }

    fn interested() -> InterestConfig {
        InterestConfig { enabled: true, radius: 150.0, ..Default::default() }
    }

    /// The whole point, stated once: a client pays for its neighbourhood, not
    /// for the world. What it can't see doesn't reach it.
    #[test]
    fn a_client_hears_about_its_neighbourhood_and_not_the_far_side_of_the_map() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        server.set_interest(interested());
        let (mut sw, se) = interest_world();
        let (mut cw, ce) = interest_world();
        server.register_scene(&sw);
        client.register_scene(&cw);

        run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 90, |w, t| {
            // Both the neighbour and the distant node move, identically.
            for e in [se[1], se[2]] {
                if let Some(tr) = w.get_mut::<Transform>(e) {
                    tr.translation.z = t as f64 * 0.1;
                }
            }
        });

        let near_z = cw.get::<Transform>(ce[1]).unwrap().translation.z;
        let far_z = cw.get::<Transform>(ce[2]).unwrap().translation.z;
        assert!(near_z > 0.5, "the neighbour must replicate normally, z={near_z}");
        assert!(
            far_z.abs() < 1e-9,
            "500 m away is outside a 150 m radius — it must not have cost this client a \
             single byte, but it moved to z={far_z}"
        );
    }

    /// The counters the 🌐 panel reads have to describe the same world the
    /// snapshots do, or they are worse than no readout at all: a developer
    /// tuning a radius against a lying number tunes it the wrong way.
    #[test]
    fn the_panel_readout_matches_what_actually_went_out() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        server.set_interest(interested());
        let (mut sw, se) = interest_world();
        let (mut cw, _) = interest_world();
        server.register_scene(&sw);
        client.register_scene(&cw);

        run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 90, |w, t| {
            for e in [se[1], se[2]] {
                if let Some(tr) = w.get_mut::<Transform>(e) {
                    tr.translation.z = t as f64 * 0.1;
                }
            }
        });

        let stats = server.interest_stats();
        assert_eq!(stats.len(), 1, "one client connected, one row expected");
        let (peer, st) = stats[0];
        assert_eq!(peer, 1);
        assert_eq!(
            st.relevant, 2,
            "the avatar and its 20 m neighbour are relevant; the node 500 m away is not — \
             the readout must agree with the culling that actually happened"
        );
        assert!(st.sent > 0 && st.sent <= st.relevant, "sent={} relevant={}", st.sent, st.relevant);
        assert!(st.bytes > 0, "entries were sent, so they cost something");
        assert_eq!(
            st.deferred, 0,
            "two entities cannot exhaust a 16 KB/s budget — a non-zero deferred count here \
             would mean the number is measuring something other than the budget"
        );

        // Off is the honest empty answer rather than a stale one.
        server.set_interest(InterestConfig::default());
        assert!(
            server.interest_stats().is_empty(),
            "with interest off every client hears everything, so there is no set to report"
        );
    }

    /// Interest is opt-in, and off it must behave exactly as it always has —
    /// the same test above, with the feature off, expects the opposite.
    #[test]
    fn with_interest_off_everything_still_reaches_everyone() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, se) = interest_world();
        let (mut cw, ce) = interest_world();
        server.register_scene(&sw);
        client.register_scene(&cw);

        run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 90, |w, t| {
            if let Some(tr) = w.get_mut::<Transform>(se[2]) {
                tr.translation.z = t as f64 * 0.1;
            }
        });
        assert!(
            cw.get::<Transform>(ce[2]).unwrap().translation.z > 0.5,
            "broadcasting is the default and must be untouched by any of this"
        );
    }

    /// Walking towards something is how it becomes yours to know about. The
    /// entity is not despawned and respawned — it is scene-authored, the client
    /// has had it all along, and it simply starts being updated again.
    #[test]
    fn walking_into_range_starts_the_updates() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        server.set_interest(interested());
        let (mut sw, se) = interest_world();
        let (mut cw, ce) = interest_world();
        server.register_scene(&sw);
        client.register_scene(&cw);

        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 60, |w, tk| {
            if let Some(tr) = w.get_mut::<Transform>(se[2]) {
                tr.translation.z = tk as f64 * 0.1;
            }
        });
        assert!(
            cw.get::<Transform>(ce[2]).unwrap().translation.z.abs() < 1e-9,
            "still out of range"
        );

        // Now walk the avatar most of the way there.
        if let Some(tr) = sw.get_mut::<Transform>(se[0]) {
            tr.translation.x = 420.0;
        }
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 90, |w, tk| {
            if let Some(tr) = w.get_mut::<Transform>(se[2]) {
                tr.translation.z = tk as f64 * 0.1;
            }
        });
        let _ = t;
        assert!(
            cw.get::<Transform>(ce[2]).unwrap().translation.z > 0.5,
            "once it is within the radius the client must start hearing about it"
        );
    }

    /// Two clients standing in different places are told different things —
    /// which is the mechanism that makes cost-per-client flat as the world's
    /// population grows.
    #[test]
    fn two_clients_in_different_places_get_different_snapshots() {
        let hub = MemoryHub::new();
        let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
        server.set_interest(interested());
        let mut a = NetSession::client(Box::new(hub.connect()), 0);
        let mut b = NetSession::client(Box::new(hub.connect()), 0);

        let mut sw = World::default();
        let mut ents = Vec::new();
        // 0 = A's avatar at the origin, 1 = B's avatar 1000 m east,
        // 2 = scenery beside A, 3 = scenery beside B.
        for (i, x) in [0.0_f64, 1000.0, 10.0, 1010.0].into_iter().enumerate() {
            let e = sw.spawn();
            sw.insert(e, Transform::from_translation(DVec3::new(x, 0.0, 0.0)));
            sw.insert(
                e,
                Replicated {
                    owner: match i {
                        0 => Some(1),
                        1 => Some(2),
                        _ => None,
                    },
                    ..Default::default()
                },
            );
            ents.push(e);
        }
        let build_client = |w: &mut World| {
            for x in [0.0_f64, 1000.0, 10.0, 1010.0] {
                let e = w.spawn();
                w.insert(e, Transform::from_translation(DVec3::new(x, 0.0, 0.0)));
                w.insert(e, Replicated::default());
            }
        };
        let (mut aw, mut bw) = (World::default(), World::default());
        build_client(&mut aw);
        build_client(&mut bw);
        server.register_scene(&sw);
        a.register_scene(&aw);
        b.register_scene(&bw);

        for t in 1..120u64 {
            hub.set_now(t);
            for e in [ents[2], ents[3]] {
                if let Some(tr) = sw.get_mut::<Transform>(e) {
                    tr.translation.z = t as f64 * 0.1;
                }
            }
            server.tick_server(&sw, t);
            a.tick_client(&mut aw);
            b.tick_client(&mut bw);
        }
        let a_ents: Vec<floptle_core::Entity> = aw.query::<Transform>().map(|(e, _)| e).collect();
        let b_ents: Vec<floptle_core::Entity> = bw.query::<Transform>().map(|(e, _)| e).collect();
        let az = |i: usize| aw.get::<Transform>(a_ents[i]).unwrap().translation.z;
        let bz = |i: usize| bw.get::<Transform>(b_ents[i]).unwrap().translation.z;
        assert!(az(2) > 0.5, "A hears about the scenery next to A");
        assert!(az(3).abs() < 1e-9, "A does not hear about scenery a kilometre away");
        assert!(bz(3) > 0.5, "B hears about the scenery next to B");
        assert!(bz(2).abs() < 1e-9, "B does not hear about A's neighbourhood either");
    }

    /// A budget too small for the crowd must DEFER, never drop: run long
    /// enough and every relevant node has had its turn. A design that starves
    /// the unlucky ones is one you cannot safely turn on.
    #[test]
    fn a_tight_budget_defers_everything_and_starves_nothing() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        server.set_interest(InterestConfig {
            enabled: true,
            radius: 150.0,
            // ~2 entries per snapshot for a crowd of 24.
            budget_bytes_per_sec: 3000,
            ..Default::default()
        });
        let build = |w: &mut World| {
            for i in 0..25 {
                let e = w.spawn();
                w.insert(e, Transform::from_translation(DVec3::new(i as f64, 0.0, 0.0)));
                w.insert(
                    e,
                    Replicated { owner: (i == 0).then_some(1), ..Default::default() },
                );
            }
        };
        let (mut sw, mut cw) = (World::default(), World::default());
        build(&mut sw);
        build(&mut cw);
        server.register_scene(&sw);
        client.register_scene(&cw);
        let se: Vec<floptle_core::Entity> = sw.query::<Transform>().map(|(e, _)| e).collect();
        let ce: Vec<floptle_core::Entity> = cw.query::<Transform>().map(|(e, _)| e).collect();

        for t in 1..900u64 {
            hub.set_now(t);
            for e in &se {
                if let Some(tr) = sw.get_mut::<Transform>(*e) {
                    tr.translation.z = t as f64 * 0.01;
                }
            }
            server.tick_server(&sw, t);
            client.tick_client(&mut cw);
        }
        let unheard: Vec<usize> = ce
            .iter()
            .enumerate()
            .filter(|(_, e)| cw.get::<Transform>(**e).unwrap().translation.z.abs() < 1e-9)
            .map(|(i, _)| i)
            .collect();
        assert!(
            unheard.is_empty(),
            "every relevant node must eventually get a turn — these never did: {unheard:?}"
        );
    }

    /// Per-player round trip, measured at the application level.
    ///
    /// The transport can only report the link it owns, which through a relay is
    /// host↔relay — it would call that the player's ping and be wrong by a
    /// whole hop. This probes host↔player and so reads the same over every
    /// transport there will ever be.
    #[test]
    fn the_host_measures_a_round_trip_to_each_player() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);

        assert_eq!(server.peer_rtt_ms(1), None, "nothing measured before anything is probed");
        run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 60, |_, _| {});

        let rtt = server.peer_rtt_ms(1).expect("the host must have probed its player");
        assert!((0.0..1000.0).contains(&rtt), "implausible round trip {rtt} ms");
        assert_eq!(server.peer_rtts().len(), 1);
        // The client measures its own, so its ping display is honest too.
        assert!(client.peer_rtt_ms(SERVER).is_some(), "a client measures the host as well");
    }

    /// A peer that leaves takes its measurements with it — otherwise a long
    /// session accrues a stale ping for everyone who ever connected.
    #[test]
    fn a_departed_peer_stops_being_measured() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        server.register_scene(&sw);
        client.register_scene(&cw);
        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 60, |_, _| {});
        assert!(server.peer_rtt_ms(1).is_some());

        hub.disconnect(1);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 4, |_, _| {});
        let _ = t;
        assert_eq!(server.peer_rtt_ms(1), None, "a peer that left leaves no ping behind");
    }

    #[test]
    fn rollback_inputs_fan_out_from_the_host_to_every_peer() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});
        server.set_rollback(true, 2, MATCH_SEED);
        // `RollbackStart` is the match clock's origin, so a client queues
        // nothing until it lands — anything sampled before it belongs to a tick
        // numbering that no longer exists.
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 2, |_, _| {});
        let start = client.take_rollback_start();

        // The host's own input goes through exactly the same path as a peer's.
        for tick in 1..=6u64 {
            server.push_rollback_input(tick, held(tick));
            client.send_rollback_input(tick, held(100 + tick));
        }
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 6, |_, _| {});

        // The client learned the roster + the delay from RollbackStart…
        let (peers, delay, seed) = start.expect("the host must announce the match");
        assert_eq!(seed, MATCH_SEED, "the match seed must reach every peer");
        assert_eq!(peers, vec![SERVER, 1], "host is slot 0, the joiner slot 1");
        assert_eq!(delay, 2);
        assert_eq!(client.input_delay(), 2);

        // …the host received the client's inputs…
        let at_host = server.take_rollback_inputs();
        for tick in 1..=6u64 {
            assert!(
                at_host.iter().any(|(p, t, i)| *p == 1 && *t == tick && i.actions == 100 + tick),
                "host missing the client's tick {tick}: {at_host:?}"
            );
        }
        // …and the client received the HOST's, without its own echoed back.
        let at_client = client.take_rollback_inputs();
        for tick in 1..=6u64 {
            assert!(
                at_client.iter().any(|(p, t, i)| *p == SERVER && *t == tick && i.actions == tick),
                "client missing the host's tick {tick}: {at_client:?}"
            );
        }
        assert!(
            !at_client.iter().any(|(p, ..)| *p == 1),
            "a peer is the authority on its own input; the echo must be dropped"
        );
    }

    /// The redundant window is the loss strategy: an input only goes missing if
    /// every packet carrying it drops. This is where replay-input stability
    /// earns its keep, so it is tested at 50% loss rather than on a clean link.
    #[test]
    fn rollback_inputs_survive_heavy_loss_through_the_redundant_window() {
        let hub = MemoryHub::new();
        hub.set_conditions(2, 0.5);
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 8, |_, _| {});
        server.set_rollback(true, 2, MATCH_SEED);
        // The announcement is reliable, so it survives the loss — but it still
        // has to travel, and a client queues nothing until it lands.
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 8, |_, _| {});
        assert!(client.take_rollback_start().is_some());

        let mut seen_at_client = std::collections::HashSet::new();
        let mut seen_at_host = std::collections::HashSet::new();
        // A driver's confirmed frontier: the newest tick BOTH peers' real
        // inputs are known for. Our own are 1..=N by construction, so it is the
        // longest unbroken prefix of what has arrived. Reported every tick,
        // exactly as `net_rollback_tick` does — the host retains against it, so
        // a test that never reported would model a session that cannot exist.
        let frontier = |seen: &std::collections::HashSet<u64>| {
            (1..).take_while(|t| seen.contains(t)).last().unwrap_or(0)
        };
        for tick in 1..=40u64 {
            server.push_rollback_input(tick, held(tick));
            client.send_rollback_input(tick, held(1000 + tick));
            server.set_rollback_confirmed(frontier(&seen_at_host));
            client.set_rollback_confirmed(frontier(&seen_at_client));
            t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 1, |_, _| {});
            seen_at_client.extend(client.take_rollback_inputs().into_iter().map(|(_, t, _)| t));
            seen_at_host.extend(server.take_rollback_inputs().into_iter().map(|(_, t, _)| t));
        }
        // Let the tail drain: the last few ticks are still riding the window.
        for _ in 0..20 {
            server.set_rollback_confirmed(frontier(&seen_at_host));
            client.set_rollback_confirmed(frontier(&seen_at_client));
            t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 1, |_, _| {});
            seen_at_client.extend(client.take_rollback_inputs().into_iter().map(|(_, t, _)| t));
            seen_at_host.extend(server.take_rollback_inputs().into_iter().map(|(_, t, _)| t));
        }
        let _ = t;
        for tick in 1..=40u64 {
            assert!(seen_at_client.contains(&tick), "client lost tick {tick} at 50% loss");
            assert!(seen_at_host.contains(&tick), "host lost tick {tick} at 50% loss");
        }
    }

    /// FIELD REGRESSION (floptle/0039 Symptom A): a live relay match froze on
    /// round one, the joiner stalled at warmup+depth having never received a
    /// host input, and every layer test passed.
    ///
    /// The window was doing two jobs out of one FIFO: **dedup memory** and
    /// **fan-out payload**, capped at `INPUT_WINDOW × slots` across ALL peers.
    /// So it carried "the last N admissions", not "everything still
    /// unconfirmed" — and the host advancing evicted its OWN oldest ticks,
    /// which are exactly the ticks a starved peer is waiting for. One dropped
    /// packet early in a match and that tick was gone for good: the client
    /// could never confirm, so it stopped sending, so the host's frontier froze
    /// too. A permanent deadlock, from one lost datagram, on a design whose
    /// entire loss strategy is "say it again next tick".
    ///
    /// The invariant is: **an unconfirmed tick keeps riding every packet until
    /// every peer has it.** That is what makes the redundancy redundant.
    #[test]
    fn the_window_keeps_carrying_the_tick_a_starved_peer_is_waiting_for() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});
        server.set_rollback(true, 2, MATCH_SEED);
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 2, |_, _| {});
        client.take_rollback_start().expect("the host announces the match");

        // The opening of the match is lost. A burst at the start of a session is
        // the least surprising loss there is — the link has not settled, the
        // joiner is still loading the match scene, and the host waits for
        // neither. Both sides bank their first ten applied ticks into it.
        hub.set_conditions(0, 1.0);
        for tick in 1..=10u64 {
            server.push_rollback_input(tick, held(tick));
            client.send_rollback_input(tick, held(100 + tick));
        }
        let t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 10, |_, _| {});
        hub.set_conditions(0, 0.0);
        assert!(
            client.take_rollback_inputs().is_empty(),
            "the burst was supposed to swallow the opening — the test proves nothing otherwise"
        );

        // The link is clean again. Nobody has confirmed anything, and the
        // client says so on every packet. The host keeps playing while it
        // waits: ten more of its own ticks, well inside the depth cap.
        for tick in 11..=20u64 {
            server.push_rollback_input(tick, held(tick));
        }
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 10, |_, _| {});

        let at_client = client.take_rollback_inputs();
        assert!(
            at_client.iter().any(|(p, tk, _)| *p == SERVER && *tk == 1),
            "the host's tick 1 is unconfirmed and the client is stalled on it, but the \
             fan-out stopped carrying it — the client can never confirm, so it stops \
             sending, so the host freezes too. Ticks the client did get: {:?}",
            {
                let mut v: Vec<u64> =
                    at_client.iter().filter(|(p, ..)| *p == SERVER).map(|(_, t, _)| *t).collect();
                v.sort_unstable();
                v
            }
        );
    }

    /// FIELD REGRESSION (floptle/0041): a referee that disagrees with EVERYONE
    /// is the one that is wrong, and must not take the match down with it.
    ///
    /// The referee is the sole judge when one is running — deliberately, because
    /// a quorum of players could all be running the same modified build. But a
    /// cheat changes ONE machine, while an engine or content fault in the
    /// referee changes only the referee. So "every peer disagrees with the
    /// referee and they all agree with each other" is overwhelmingly the second
    /// case, and answering it by desyncing the whole match is the worst
    /// available response. It shipped that way: v0.10.4's referee ran the match
    /// in freefall with no floor, and every online match died at its first
    /// checksum with both players told they had desynced.
    #[test]
    fn a_referee_that_disagrees_with_everyone_is_the_one_thats_wrong() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});
        server.set_rollback(true, 2, MATCH_SEED);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 2, |_, _| {});
        assert!(client.take_rollback_start().is_some());

        // Both players agree with each other. The referee does not agree with
        // either — because the referee is running different physics.
        server.set_referee_hash(30, 0xBAD_BAD);
        server.send_state_hash(30, 0xAAAA);
        client.send_state_hash(30, 0xAAAA);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 6, |_, _| {});

        assert_eq!(
            server.take_referee_outliers(),
            vec![30],
            "the host must name the referee as the outlier"
        );
        assert!(
            server.take_desyncs().is_empty() && client.take_desyncs().is_empty(),
            "and must NOT end a match in which both players agree with each other"
        );
        assert!(
            server.take_referee_faults().is_empty(),
            "neither player is at fault, so neither is accused"
        );

        // The anti-cheat property is unchanged: ONE peer out of step is still
        // judged against the referee, not against the other player.
        server.set_referee_hash(60, 0xAAAA);
        server.send_state_hash(60, 0xAAAA);
        client.send_state_hash(60, 0xBBBB);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 6, |_, _| {});
        assert_eq!(
            server.take_referee_faults(),
            vec![(60, 1)],
            "the peer that disagrees with the referee is named, and only that peer"
        );
        assert_eq!(server.take_desyncs(), vec![60], "and that IS a desync");
        assert!(server.take_referee_outliers().is_empty());
    }

    /// Desync detection is mandatory (§6). Agreement is silent; disagreement is
    /// loud on every peer — the alternative is two machines playing a subtly
    /// different match, each convinced it is right.
    #[test]
    fn disagreeing_state_hashes_are_reported_to_every_peer() {
        let hub = MemoryHub::new();
        let (mut server, mut client) = connect_pair(&hub);
        let (mut sw, _) = world_with(1);
        let (mut cw, _) = world_with(1);
        let mut t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, 1, 4, |_, _| {});
        server.set_rollback(true, 2, MATCH_SEED);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 2, |_, _| {});
        assert!(client.take_rollback_start().is_some());

        // Agreement: nobody hears anything.
        server.send_state_hash(30, 0xAAAA);
        client.send_state_hash(30, 0xAAAA);
        t = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 4, |_, _| {});
        assert!(server.take_desyncs().is_empty(), "matching checksums must stay quiet");
        assert!(client.take_desyncs().is_empty());

        // Disagreement: both sides are told, and told WHICH tick.
        server.send_state_hash(60, 0xAAAA);
        client.send_state_hash(60, 0xBBBB);
        let _ = run(&hub, &mut server, &mut sw, &mut client, &mut cw, t, 4, |_, _| {});
        assert_eq!(server.take_desyncs(), vec![60], "the host detects it");
        assert_eq!(client.take_desyncs(), vec![60], "and says so out loud");
    }
}
