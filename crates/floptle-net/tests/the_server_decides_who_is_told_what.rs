//! floptle/0182 — relevance is the game's decision and the geometry's, not the
//! radius's alone.
//!
//! An interest radius is a **bandwidth** boundary. A hidden-role or competitive
//! game needs a **security** one, and the difference is not academic: a client
//! that has been told where everyone within 25 m is standing knows where they
//! are, whatever it chooses to draw. Anything the fix does on the client is a
//! setting a modified client turns back off.
//!
//! So these check the only thing that matters — what actually leaves the
//! server for a given peer.

use floptle_core::math::DVec3;
use floptle_core::transform::Transform;
use floptle_core::{Name, Replicated, World};
use floptle_net::{InterestConfig, MemoryHub, NetSession, Occluder};

/// A wall at x = 0, running along z: it blocks any sight line that crosses it.
struct WallAtOrigin;

impl Occluder for WallAtOrigin {
    fn blocked(&self, from: [f64; 3], to: [f64; 3]) -> bool {
        (from[0] < 0.0) != (to[0] < 0.0)
    }
}

struct NothingBlocks;

impl Occluder for NothingBlocks {
    fn blocked(&self, _: [f64; 3], _: [f64; 3]) -> bool {
        false
    }
}

/// A server with two clients and three replicated bodies: each client's own
/// avatar, plus a third player standing on the far side of the wall.
struct Table {
    server: NetSession,
    world: World,
    clients: Vec<(NetSession, World)>,
    /// The node peer 1 must not be told about, once a filter or a wall says so.
    hidden: floptle_core::Entity,
}

fn spawn(world: &mut World, name: &str, at: DVec3, owner: Option<u64>) -> floptle_core::Entity {
    let e = world.spawn();
    let mut tr = Transform::IDENTITY;
    tr.translation = at;
    world.insert(e, tr);
    world.insert(e, Name(name.into()));
    world.insert(
        e,
        Replicated { owner, transform: true, ..Default::default() },
    );
    e
}

fn table() -> Table {
    let hub = MemoryHub::new();
    let mut server = NetSession::server(Box::new(hub.server_endpoint()), 0);
    let mut world = World::default();
    let mut clients: Vec<(NetSession, World)> = (0..2)
        .map(|_| (NetSession::client(Box::new(hub.connect()), 0), World::default()))
        .collect();

    // Everyone in the house, well inside any sensible radius. `Killer` is
    // behind the wall from peer 1's point of view.
    spawn(&mut world, "P1", DVec3::new(5.0, 0.0, 0.0), Some(1));
    spawn(&mut world, "P2", DVec3::new(7.0, 0.0, 0.0), Some(2));
    let hidden = spawn(&mut world, "Killer", DVec3::new(-5.0, 0.0, 0.0), None);
    for (_, cw) in clients.iter_mut() {
        spawn(cw, "P1", DVec3::new(5.0, 0.0, 0.0), Some(1));
        spawn(cw, "P2", DVec3::new(7.0, 0.0, 0.0), Some(2));
        spawn(cw, "Killer", DVec3::new(-5.0, 0.0, 0.0), None);
    }
    server.register_scene(&world);
    for (c, cw) in clients.iter_mut() {
        c.register_scene(cw);
    }
    server.set_interest(InterestConfig {
        enabled: true,
        radius: 150.0,
        budget_bytes_per_sec: 1024 * 1024,
        ..Default::default()
    });
    // Handshake.
    for t in 1..4 {
        server.tick_server(&world, t);
        for (c, cw) in clients.iter_mut() {
            c.tick_client(cw);
        }
    }
    Table { server, world, clients, hidden }
}

/// How many nodes peer `p` was told about in the last snapshot it received.
fn relevant_to(t: &Table, peer: u64) -> usize {
    t.server
        .interest_stats()
        .into_iter()
        .find(|(p, _)| *p == peer)
        .map(|(_, s)| s.relevant)
        .unwrap_or(0)
}

fn stat(t: &Table, peer: u64) -> floptle_net::InterestStat {
    t.server
        .interest_stats()
        .into_iter()
        .find(|(p, _)| *p == peer)
        .map(|(_, s)| s)
        .unwrap_or_default()
}

fn run(t: &mut Table, ticks: u64, occl: &dyn Occluder) {
    for i in 0..ticks {
        t.server.tick_server_seen(&t.world, 10 + i, occl);
        for (c, cw) in t.clients.iter_mut() {
            c.tick_client(cw);
        }
    }
}

#[test]
fn a_radius_alone_tells_every_client_about_everyone() {
    // The state of the world before the fix, asserted deliberately: without it
    // the tests below prove nothing about what they removed.
    let mut t = table();
    run(&mut t, 8, &NothingBlocks);
    assert_eq!(relevant_to(&t, 1), 3, "everyone in the radius, which is the leak");
    assert_eq!(stat(&t, 1).withheld(), 0);
}

#[test]
fn the_game_can_withhold_one_node_from_one_peer() {
    let mut t = table();
    assert!(t.server.set_relevant(t.hidden, 1, Some(false)));
    run(&mut t, 8, &NothingBlocks);

    assert_eq!(relevant_to(&t, 1), 2, "peer 1 is not told the killer exists");
    assert_eq!(stat(&t, 1).withheld_filter, 1, "and the panel can say why");
    assert_eq!(relevant_to(&t, 2), 3, "peer 2 is unaffected — this is per pair");

    // Handing the decision back restores it, so a role that changes mid-match
    // is not a one-way door.
    t.server.set_relevant(t.hidden, 1, None);
    run(&mut t, 8, &NothingBlocks);
    assert_eq!(relevant_to(&t, 1), 3);
}

/// The one exemption that must hold whatever the game says: a client is always
/// told about its own avatar. It is what prediction reconciles against, so a
/// filter that could hide it would produce a player who cannot see themselves.
#[test]
fn a_filter_cannot_hide_a_client_from_itself() {
    let mut t = table();
    let own = t.world.query::<Replicated>().find(|(_, r)| r.owner == Some(1)).unwrap().0;
    t.server.set_relevant(own, 1, Some(false));
    run(&mut t, 8, &NothingBlocks);
    assert_eq!(relevant_to(&t, 1), 3, "your own node survives your own filter");
}

#[test]
fn a_node_behind_a_wall_is_not_relevant_when_sight_is_required() {
    let mut t = table();
    // Without occlusion the wall means nothing, whichever occluder is passed.
    run(&mut t, 8, &WallAtOrigin);
    assert_eq!(relevant_to(&t, 1), 3, "occlusion is OFF by default");

    let mut cfg = t.server.interest();
    cfg.occlusion = true;
    t.server.set_interest(cfg);
    // Enough snapshots to exhaust the hysteresis grace.
    run(&mut t, 20, &WallAtOrigin);
    assert_eq!(relevant_to(&t, 1), 2, "the killer is behind the wall and stays there");
    assert_eq!(stat(&t, 1).withheld_occluded, 1);
}

/// Losing sight is damped; regaining it is not. A body strobing behind a door
/// frame must not flicker, and a player stepping out of cover must be there on
/// the frame they step out — damping both directions would trade the flicker
/// for a player who appears three snapshots after they shot you.
#[test]
fn sight_comes_back_immediately_and_goes_away_slowly() {
    let mut t = table();
    let mut cfg = t.server.interest();
    cfg.occlusion = true;
    cfg.occlusion_grace = 3;
    t.server.set_interest(cfg);

    // In sight first, so it is a node the client actually holds.
    run(&mut t, 4, &NothingBlocks);
    assert_eq!(relevant_to(&t, 1), 3);

    // One blocked snapshot does not drop it.
    t.server.tick_server_seen(&t.world, 100, &WallAtOrigin);
    assert_eq!(relevant_to(&t, 1), 3, "one frame behind a door frame is not gone");

    // Past the grace, it does.
    for i in 0..6 {
        t.server.tick_server_seen(&t.world, 102 + i * 2, &WallAtOrigin);
    }
    assert_eq!(relevant_to(&t, 1), 2);

    // And stepping out brings it straight back, on the very next snapshot.
    t.server.tick_server_seen(&t.world, 200, &NothingBlocks);
    assert_eq!(relevant_to(&t, 1), 3, "no grace on the way back in");

    // The counter RESETS on sight, rather than merely pausing. Otherwise the
    // grace is spent once for the whole match: two blocked frames now and two
    // in a minute's time would add up to a drop, and a player walking past a
    // row of pillars would vanish partway down the corridor.
    for i in 0..2 {
        t.server.tick_server_seen(&t.world, 202 + i * 2, &WallAtOrigin);
    }
    t.server.tick_server_seen(&t.world, 210, &NothingBlocks);
    for i in 0..2 {
        t.server.tick_server_seen(&t.world, 212 + i * 2, &WallAtOrigin);
    }
    assert_eq!(relevant_to(&t, 1), 3, "four blocked frames either side of a clear one");
}

/// A pin outlives neither its peer nor its node: a NetId means nothing outside
/// the scene that issued it, and a stale pin would name a stranger.
#[test]
fn pins_are_dropped_with_the_node_they_name() {
    let mut t = table();
    t.server.set_relevant(t.hidden, 1, Some(false));
    assert_eq!(t.server.relevance_pins().count(), 1);
    t.server.despawn(&mut t.world, t.hidden);
    assert_eq!(t.server.relevance_pins().count(), 0);
}
