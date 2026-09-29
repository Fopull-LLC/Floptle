//! **An additive layer the server loads, every client loads, late joiners
//! included, with the same ids.**
//!
//! A layer used to be local: the server's level went up, nobody else's did,
//! and its Networked nodes never got ids, so nothing in it replicated. The
//! server now numbers them and sends one message; each client loads the same
//! scene and numbers its copy the same way.

use floptle_core::{Name, Replicated, World};
use floptle_net::{LayerEvent, MemoryHub, NetSession};
use floptle_scene::NodeDoc;

fn nodes() -> Vec<NodeDoc> {
    let named = |name: &str| {
        let mut d: NodeDoc = ron::from_str("()").expect("an empty NodeDoc");
        d.name = name.into();
        d
    };
    vec![named("Door"), named("Rock"), named("Lift")]
}

/// Spawn the layer's nodes the way the driver does, with Door and Lift
/// networked and Rock not.
fn spawn_layer(world: &mut World) -> Vec<floptle_core::Entity> {
    let ents = floptle_scene::spawn_nodes(&nodes(), world);
    for &e in &ents {
        if world.get::<Name>(e).is_some_and(|n| n.0 != "Rock") {
            world.insert(e, Replicated::default());
        }
    }
    ents
}

fn name(world: &World, e: Option<floptle_core::Entity>) -> Option<String> {
    e.and_then(|e| world.get::<Name>(e).map(|n| n.0.clone()))
}

#[test]
fn a_layer_reaches_every_client_with_the_same_ids() {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    let mut client = NetSession::client(Box::new(hub.connect()), 0);
    let (mut sworld, mut cworld) = (World::default(), World::default());
    for _ in 0..2 {
        server.tick_server(&sworld, 1);
        client.tick_client(&mut cworld);
    }
    assert!(client.my_peer().is_some(), "the client never got its Welcome");

    // An ordinary spawn first, so the layer's numbers do not start at 1 and a
    // client that guessed them would be wrong.
    server.spawn_subtree(&mut sworld, &nodes()[..1], None);
    let sents = spawn_layer(&mut sworld);
    server.load_layer(&sworld, "level2", "scenes/level2.ron", [500.0, 0.0, 0.0], true, &sents);
    client.tick_client(&mut cworld);

    let events = client.take_layer_events();
    let [LayerEvent::Load { tag, scene, offset, environment, id_base }] = events.as_slice() else {
        panic!("expected one layer load, got {events:?}");
    };
    assert_eq!((tag.as_str(), scene.as_str(), *offset, *environment), ("level2", "scenes/level2.ron", [500.0, 0.0, 0.0], true));
    assert!(*id_base > 1, "the layer's ids follow the spawn's: {id_base}");
    let cents = spawn_layer(&mut cworld);
    client.bind_layer(&cworld, tag, scene, *offset, *id_base, &cents);
    for k in 0..2 {
        let (s, c) = (name(&sworld, server.entity_of(id_base + k)), name(&cworld, client.entity_of(id_base + k)));
        assert!(s.is_some() && s == c, "id {} is {s:?} on the server and {c:?} on the client", id_base + k);
    }
    assert_eq!(name(&sworld, server.entity_of(*id_base)), Some("Door".into()));
    assert_eq!(name(&sworld, server.entity_of(id_base + 1)), Some("Lift".into()), "Rock is not networked and takes no id");

    // A player who joins now is told to load it too, with the same numbers.
    let mut late = NetSession::client(Box::new(hub.connect()), 0);
    let mut lworld = World::default();
    for _ in 0..3 {
        server.tick_server(&sworld, 1);
        late.tick_client(&mut lworld);
    }
    let late_events = late.take_layer_events();
    assert!(
        matches!(late_events.as_slice(), [LayerEvent::Load { tag, id_base: b, .. }] if tag == "level2" && b == id_base),
        "the late joiner was not told about the layer: {late_events:?}"
    );

    // And when the server takes it away, the client hears, and its ids let go.
    assert!(server.unload_layer("level2"));
    assert!(server.entity_of(*id_base).is_none(), "the server kept a gone layer's id");
    client.tick_client(&mut cworld);
    assert_eq!(client.take_layer_events(), vec![LayerEvent::Unload { tag: "level2".into() }]);
    client.drop_layer("level2");
    assert!(client.entity_of(*id_base).is_none(), "the client kept a gone layer's id");
    assert!(!server.unload_layer("level2"), "a second unload of the same layer");
}
