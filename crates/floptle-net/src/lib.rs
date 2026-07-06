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

pub mod session;
pub mod transport;
pub mod value;
pub mod wire;

pub use session::{NetEvent, NetRole, NetSession, ReceivedRpc, RpcTarget, SyncedVars};
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
