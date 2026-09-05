//! floptle/0181 — a replicated spawn is a SUBTREE, and ownership is reassignable.
//!
//! Every avatar a real game has is a hierarchy: a capsule with a camera child,
//! an arms mesh, a bone-attached item socket. `net.spawn` used to send the
//! first root and drop everything under it, which is why projects authored
//! fixed player slots into the map scene and capped their lobby at build time.
//!
//! These stand up a real server and a real client over the memory hub and check
//! what actually lands on the far side — the client's world is the only place
//! the answer is honest.

use floptle_core::transform::Transform;
use floptle_core::{Name, Parent, Replicated, ReplicationMode, World};
use floptle_net::{MemoryHub, NetSession};
use floptle_scene::NodeDoc;

/// A player rig: a capsule root, a camera child, and an arms mesh under the
/// camera — three deep, so a one-level fix would not pass.
fn rig() -> Vec<NodeDoc> {
    let named = |name: &str, parent: Option<usize>| {
        let mut d: NodeDoc = ron::from_str("()").expect("an empty NodeDoc");
        d.name = name.into();
        d.parent = parent;
        d
    };
    vec![named("Player", None), named("Camera", Some(0)), named("Arms", Some(1))]
}

/// Server and client, connected and past the handshake, each with its own world.
fn linked() -> (NetSession, World, NetSession, World) {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    let mut client = NetSession::client(Box::new(hub.connect()), 0);
    let (mut sworld, mut cworld) = (World::default(), World::default());
    // Two rounds: the client's Hello reaches the server, the Welcome comes back.
    for _ in 0..2 {
        server.tick_server(&sworld, 1);
        client.tick_client(&mut cworld);
    }
    assert!(client.my_peer().is_some(), "the client never got its Welcome");
    let _ = (&mut sworld, &mut cworld);
    (server, sworld, client, cworld)
}

fn names_under(world: &World, root: floptle_core::Entity) -> Vec<String> {
    let mut out = Vec::new();
    let mut frontier = vec![root];
    while let Some(e) = frontier.pop() {
        for (k, p) in world.query::<Parent>() {
            if p.0 == e {
                frontier.push(k);
            }
        }
        if e != root {
            out.push(world.get::<Name>(e).map(|n| n.0.clone()).unwrap_or_default());
        }
    }
    out.sort();
    out
}

fn find(world: &World, name: &str) -> Option<floptle_core::Entity> {
    world.query::<Name>().find(|(_, n)| n.0 == name).map(|(e, _)| e)
}

#[test]
fn a_spawned_rig_arrives_on_the_client_with_its_children() {
    let (mut server, mut sworld, mut client, mut cworld) = linked();
    let nodes = rig();
    let ents = server.spawn_subtree(&mut sworld, &nodes, Some(1));
    assert_eq!(ents.len(), 3, "the server spawns the whole rig locally");

    client.tick_client(&mut cworld);

    let player = find(&cworld, "Player").expect("the root never arrived");
    assert_eq!(
        names_under(&cworld, player),
        vec!["Arms".to_string(), "Camera".to_string()],
        "the children came with it, three levels deep"
    );
    // The link is the real one, not two roots that happen to share a name.
    let camera = find(&cworld, "Camera").unwrap();
    let arms = find(&cworld, "Arms").unwrap();
    assert_eq!(cworld.get::<Parent>(camera).map(|p| p.0), Some(player));
    assert_eq!(cworld.get::<Parent>(arms).map(|p| p.0), Some(camera));
}

#[test]
fn only_the_root_replicates_unless_a_child_asks() {
    let (mut server, mut sworld, mut client, mut cworld) = linked();
    let mut nodes = rig();
    // The creature case: a model child that syncs its own animator.
    nodes[1].net = Some(floptle_scene::ReplicatedDoc::from_component(&Replicated::default()));
    let ents = server.spawn_subtree(&mut sworld, &nodes, Some(2));
    client.tick_client(&mut cworld);

    assert!(server.net_id_of(ents[0]).is_some(), "the root always replicates");
    assert!(
        server.net_id_of(ents[1]).is_some(),
        "a child carrying Networked replicates in its own right"
    );
    assert!(
        server.net_id_of(ents[2]).is_none(),
        "a plain child is a local node that follows — not a replicated one"
    );
    // And the ids mean the same node on both ends.
    let cam_s = server.net_id_of(ents[1]).unwrap();
    let cam_c = find(&cworld, "Camera").unwrap();
    assert_eq!(client.net_id_of(cam_c), Some(cam_s), "derived ids agree across the wire");
    // A replicated child keeps its parent link on the client.
    let player_c = find(&cworld, "Player").unwrap();
    assert_eq!(cworld.get::<Parent>(cam_c).map(|p| p.0), Some(player_c));
}

#[test]
fn despawning_the_root_takes_the_subtree_everywhere() {
    let (mut server, mut sworld, mut client, mut cworld) = linked();
    let nodes = rig();
    let ents = server.spawn_subtree(&mut sworld, &nodes, Some(1));
    client.tick_client(&mut cworld);
    assert!(find(&cworld, "Arms").is_some());

    server.despawn(&mut sworld, ents[0]);
    client.tick_client(&mut cworld);

    for name in ["Player", "Camera", "Arms"] {
        assert!(find(&cworld, name).is_none(), "{name} survived the despawn on the client");
        assert!(find(&sworld, name).is_none(), "{name} survived the despawn on the server");
    }
}

#[test]
fn an_owner_can_be_assigned_after_the_node_exists() {
    let (mut server, mut sworld, mut client, mut cworld) = linked();
    // An AUTHORED slot, the shape a dedicated server's scene has: it exists
    // before anyone joins and belongs to nobody.
    let slot = sworld.spawn();
    sworld.insert(slot, Transform::IDENTITY);
    sworld.insert(slot, Name("Survivor1".into()));
    sworld.insert(
        slot,
        Replicated { mode: ReplicationMode::Predicted, ..Default::default() },
    );
    let cslot = cworld.spawn();
    cworld.insert(cslot, Transform::IDENTITY);
    cworld.insert(cslot, Name("Survivor1".into()));
    cworld.insert(
        cslot,
        Replicated { mode: ReplicationMode::Predicted, ..Default::default() },
    );
    server.register_scene(&sworld);
    client.register_scene(&cworld);
    assert_eq!(sworld.get::<Replicated>(slot).unwrap().owner, None);

    assert!(server.set_owner(&mut sworld, slot, Some(1)), "a registered node is assignable");
    client.tick_client(&mut cworld);

    assert_eq!(sworld.get::<Replicated>(slot).unwrap().owner, Some(1));
    assert_eq!(
        cworld.get::<Replicated>(cslot).unwrap().owner,
        Some(1),
        "the reassignment reached the client — otherwise the joiner spectates its own body"
    );
    assert_eq!(
        client.take_owner_changed().into_iter().map(|(_, e, o)| (e, o)).collect::<Vec<_>>(),
        vec![(cslot, Some(1))],
        "and the driver is told, so it starts predicting"
    );

    // Releasing it is the disconnect path, and it is not a special case.
    server.set_owner(&mut sworld, slot, None);
    client.tick_client(&mut cworld);
    assert_eq!(cworld.get::<Replicated>(cslot).unwrap().owner, None);
}

#[test]
fn a_late_joiner_is_sent_whole_subtrees() {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    let mut sworld = World::default();
    let mut early = NetSession::client(Box::new(hub.connect()), 0);
    let mut eworld = World::default();
    for _ in 0..2 {
        server.tick_server(&sworld, 1);
        early.tick_client(&mut eworld);
    }
    server.spawn_subtree(&mut sworld, &rig(), Some(1));

    // Somebody joins after the rig already exists.
    let mut late = NetSession::client(Box::new(hub.connect()), 0);
    let mut lworld = World::default();
    for t in 2..5 {
        server.tick_server(&sworld, t);
        late.tick_client(&mut lworld);
    }

    let player = find(&lworld, "Player").expect("the late joiner never got the rig");
    assert_eq!(
        names_under(&lworld, player),
        vec!["Arms".to_string(), "Camera".to_string()],
        "the catch-up carries the subtree, not just its root"
    );
}
