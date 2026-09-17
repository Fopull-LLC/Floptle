use super::*;

/// `terrain.busy()` has to be true the moment work is
/// queued, not on the frame after.
///
/// The consumer is a game that builds its world as the player travels: it
/// queues one system, then asks whether it may queue the next. Answering
/// with last frame's state means the answer to "did what I just asked for
/// start?" is "no" — so the game queues it again, and the second request is
/// the one that lands behind the ground somebody is standing on. The flag is
/// therefore raised by the queueing call itself; the editor's per-frame
/// publish then owns it from the real job state and is what lowers it again.
#[test]
fn terrain_busy_is_true_the_moment_a_fill_is_queued() {
    let dir = std::env::temp_dir().join(format!("floptle-busy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    write_script(
        &dir,
        "galaxy",
        concat!(
            "function update(node, dt)\n",
            "  if not asked then\n",
            "    log('before=' .. tostring(terrain.busy()))\n",
            "    terrain.generatePlanet(1, { radius = 20 })\n",
            "    log('after=' .. tostring(terrain.busy()))\n",
            "    asked = true\n",
            "  else\n",
            "    log('later=' .. tostring(terrain.busy()))\n",
            "  end\n",
            "end\n",
        ),
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "galaxy".into(),
            enabled: true,
            params: vec![],
            refs: vec![],
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(logs.iter().any(|l| l == "before=false"), "idle to start with: {logs:?}");
    assert!(
        logs.iter().any(|l| l == "after=true"),
        "the queueing call itself must raise it — a game that queues and then asks in the \
         same breath is the whole consumer: {logs:?}"
    );

    // What the editor does with the queue, and then its per-frame publish
    // finding nothing left to do. The flag is the worker's state, so the
    // host is what lowers it — and the script sees that on the next tick.
    let queued = host.take_terrain_generates();
    assert_eq!(queued.len(), 1, "the fill was queued for the editor to drain");
    host.set_terrain_busy(false);
    host.run(&mut world, &dir, 1.0 / 60.0, 1.0 / 60.0);
    let logs: Vec<String> = host.drain_logs().into_iter().map(|l| l.msg).collect();
    assert!(logs.iter().any(|l| l == "later=false"), "quiet again once nothing is running: {logs:?}");
}

#[test]
fn script_can_raycast() {
    let dir = std::env::temp_dir().join("floptle_script_test_raycast");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "caster",
        "function update(node, dt)\n  local h = raycast(0, 5, 0, 0, -1, 0, 20)\n  if h then node.y = h.y end\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "caster".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.set_colliders(
        vec![floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)))],
        glam::DVec3::ZERO,
    );
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let _ = host.take_colliders();
    let y = world.get::<Transform>(e).unwrap().translation.y;
    assert!(y.abs() < 0.1, "raycast should have set y to the ground (≈0), got {y}");
}

/// A first-person game asks "what am I standing on" to pick a footstep. It
/// got two wrong answers. `raycast` returned no `hit.node` at all for static
/// geometry — so the level, which is all static geometry, was invisible to
/// the one query the docs point people at. And the best available answer,
/// the node's own material, is one material per node: a mansion that is one
/// map mesh with nine slots reported stone for its grass, its boards and its
/// wallpaper alike.
///
/// Both are read here the way a footstep script reads them.
#[test]
fn a_script_can_ask_what_surface_it_is_standing_on() {
    let dir = std::env::temp_dir().join("floptle_script_test_surface");
    let _ = std::fs::create_dir_all(&dir);
    // Cast down from above each half of the floor and record what came back.
    write_script(
        &dir,
        "footsteps",
        "function update(node, dt)\n            \x20 local l = raycast(-2, 5, 0, 0, -1, 0, 20)\n            \x20 local r = raycast(2, 5, 0, 0, -1, 0, 20)\n            \x20 local s = spherecast(vec3(2, 5, 0), vec3(0, -1, 0), 0.2, 20)\n            \x20 node.strs = {\n            \x20   left = l and l.material or \"nil\",\n            \x20   right = r and r.material or \"nil\",\n            \x20   sphere = s and s.material or \"nil\",\n            \x20   node = (l and l.node) and \"yes\" or \"no\",\n            \x20 }\n            \x20 print(node.strs.left .. \"|\" .. node.strs.right .. \"|\" .. node.strs.sphere .. \"|\" .. node.strs.node)\n            end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "footsteps".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.set_colliders(vec![labelled_floor(7)], glam::DVec3::ZERO);
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let _ = host.take_colliders();
    let said =
        host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
    assert!(
        said.contains("Grass|Boards|Boards|yes"),
        "a script asked what it was standing on and got {said:?} — it should read the \
         floor's own material on each side, and name the node it hit"
    );
}

/// The other half of the same promise: something with no per-face material
/// answers `nil`, not a plausible wrong name. A name that is right for map
/// meshes and quietly wrong for terrain is worse than no name at all.
#[test]
fn a_surface_with_no_per_face_material_answers_nothing() {
    let dir = std::env::temp_dir().join("floptle_script_test_surface_none");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "asker",
        "function update(node, dt)\n            \x20 local h = raycast(0, 5, 0, 0, -1, 0, 20)\n            \x20 print(h and (h.material or \"nil\") or \"miss\")\n            end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "asker".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    let mut plane =
        floptle_physics::AnchoredCollider::world(Box::new(floptle_physics::Plane::ground(0.0)));
    plane.eid = Some(3);
    host.set_colliders(vec![plane], glam::DVec3::ZERO);
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let _ = host.take_colliders();
    let said =
        host.drain_logs().into_iter().map(|l| l.msg).collect::<Vec<_>>().join("\n");
    assert!(said.contains("nil"), "an analytic plane invented a material: {said:?}");
}

/// **A line-of-sight ray must not pay for a footstep's question.**
///
/// The per-face lookup costs a closest-point search of its own, and the
/// ordinary ray — cast far more often than a ground check and never
/// interested in the answer — must not run it. Which is why `hit.material`
/// is resolved lazily rather than filled in when the hit is built: a script
/// that never reads the field never triggers the search.
///
/// Counted rather than timed. A timing ratio cannot see one extra
/// closest-point search against a march of dozens of steps, so it would pass
/// while the promise was broken.
#[test]
fn a_query_that_never_asks_what_it_hit_pays_nothing_for_the_answer() {
    use std::sync::atomic::Ordering;
    let dir = std::env::temp_dir().join("floptle_script_test_surface_cost");
    let _ = std::fs::create_dir_all(&dir);
    // Three queries, and not one of them reads `material`.
    write_script(
        &dir,
        "looker",
        "function update(node, dt)\n\
        \x20 local a = raycast(-2, 5, 0, 0, -1, 0, 20)\n\
        \x20 local b = spherecast(vec3(2, 5, 0), vec3(0, -1, 0), 0.2, 20)\n\
        \x20 local c = overlapSphere(vec3(0, 0, 0), 2)\n\
        \x20 node.y = (a and a.distance or 0) + (b and b.nx or 0) + #c\n\
        end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "looker".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let verts = [
        glam::Vec3::new(-4.0, 0.0, -4.0),
        glam::Vec3::new(0.0, 0.0, -4.0),
        glam::Vec3::new(0.0, 0.0, 4.0),
        glam::Vec3::new(-4.0, 0.0, 4.0),
        glam::Vec3::new(4.0, 0.0, -4.0),
        glam::Vec3::new(4.0, 0.0, 4.0),
    ];
    let shape = std::sync::Arc::new(CountsLabelAsks {
        inner: floptle_physics::TriMeshCollider::labelled(
            &verts,
            &[0, 1, 2, 0, 2, 3, 1, 4, 5, 1, 5, 2],
            &[0, 0, 1, 1],
            vec!["Grass".into(), "Boards".into()],
        ),
        asks: std::sync::atomic::AtomicU32::new(0),
    });
    struct Shared(std::sync::Arc<CountsLabelAsks>);
    impl floptle_physics::CollisionShape for Shared {
        fn distance(&self, p: glam::Vec3) -> f32 {
            self.0.distance(p)
        }
        fn normal(&self, p: glam::Vec3) -> glam::Vec3 {
            self.0.normal(p)
        }
        fn face_label(&self, p: glam::Vec3) -> Option<&str> {
            self.0.face_label(p)
        }
    }
    let mut c = floptle_physics::AnchoredCollider::world(Box::new(Shared(shape.clone())));
    c.eid = Some(7);

    let mut host = ScriptHost::new();
    host.set_colliders(vec![c], glam::DVec3::ZERO);
    host.run(&mut world, &dir, 0.1, 0.1);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let _ = host.take_colliders();
    assert_eq!(
        shape.asks.load(Ordering::Relaxed),
        0,
        "three queries ran and none of them read hit.material, yet the per-face lookup was \
         paid anyway — that cost belongs to the caller that wants the answer"
    );

    // …and the counter is not stuck at zero: asking does reach the shape.
    assert_eq!(
        floptle_physics::CollisionShape::face_label(&*shape, glam::Vec3::new(2.0, 0.1, 0.0)),
        Some("Boards")
    );
    assert_eq!(shape.asks.load(Ordering::Relaxed), 1);
}

#[test]
fn script_reads_grounded_and_writes_velocity() {
    // The physics API: a script reads node.grounded + sets node.vx; the engine
    // reads that velocity back via take_body_changes.
    let dir = std::env::temp_dir().join("floptle_script_test_physapi");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "drive",
        "function update(node, dt)\n  if node.grounded then node.vx = 5.0 end\nend\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, Scripts(vec![floptle_core::ScriptInst {
        kind: "drive".into(),
        enabled: true,
        params: Vec::new(), refs: Vec::new(),
        strs: Vec::new(),
    }]));
    let mut host = ScriptHost::new();
    let mut bodies = HashMap::new();
    bodies.insert(
        e.index(),
        BodyState { grounded: true, ..Default::default() },
    );
    host.set_bodies(bodies);
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let changes = host.take_body_changes();
    assert_eq!(changes.get(&e.index()).copied().unwrap()[0], 5.0);
}

/// Three lines of Lua, and the node walks across the level.
///
/// This is the shape the whole agent layer exists to make possible, so it is
/// worth pinning end to end rather than only in the crate that does the
/// walking: a script that says `moveTo` and never touches a position, and a
/// node that arrives anyway.
#[test]
fn an_agent_ordered_from_a_script_walks_the_node_there() {
    let dir = std::env::temp_dir().join("floptle_script_test_navagent");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "unit",
        "function start(node)\n\
         \x20 agent = nav.agent(node, { speed = 6, arrive = 0.4 })\n\
         \x20 agent:moveTo(vec3(9, 0, 9))\n\
         end\n\
         function update(node, dt)\n\
         \x20 if agent.arrived then arrived = true end\n\
         end\n",
    );

    // A plain 12x12 floor, baked for a small character.
    let floor = [
        floptle_nav::Tri::new([0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 0.0, 12.0]),
        floptle_nav::Tri::new([12.0, 0.0, 0.0], [12.0, 0.0, 12.0], [0.0, 0.0, 12.0]),
    ];
    let mesh = floptle_nav::bake(
        &floor,
        &floptle_nav::NavSettings { agent_radius: 0.3, cell_size: 0.15, ..Default::default() },
    )
    .expect("this floor bakes");

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(1.5, 0.0, 1.5)));
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "unit".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    host.set_nav_mesh(Some(mesh));
    for _ in 0..400 {
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());

    let at = world.get::<Transform>(e).unwrap().translation;
    assert!(
        (at.x - 9.0).abs() < 0.6 && (at.z - 9.0).abs() < 0.6,
        "the node should have walked to (9, 9): {at:?}"
    );
}

/// `agent:teleport` puts the node there — the host used to read the scene
/// position straight back over it every frame, which turned a documented
/// teleport into a `stop()` that moved nothing.
#[test]
fn an_agent_teleported_from_a_script_moves_the_node() {
    let dir = std::env::temp_dir().join("floptle_script_test_navteleport");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "porter",
        "function start(node)\n\
         \x20 agent = nav.agent(node)\n\
         \x20 agent:teleport(vec3(10, 0, 10))\n\
         end\n",
    );

    let floor = [
        floptle_nav::Tri::new([0.0, 0.0, 0.0], [12.0, 0.0, 0.0], [0.0, 0.0, 12.0]),
        floptle_nav::Tri::new([12.0, 0.0, 0.0], [12.0, 0.0, 12.0], [0.0, 0.0, 12.0]),
    ];
    let mesh = floptle_nav::bake(
        &floor,
        &floptle_nav::NavSettings { agent_radius: 0.3, cell_size: 0.15, ..Default::default() },
    )
    .expect("this floor bakes");

    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(1.5, 0.0, 1.5)));
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "porter".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );

    let mut host = ScriptHost::new();
    host.set_nav_mesh(Some(mesh));
    for _ in 0..10 {
        host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let at = world.get::<Transform>(e).unwrap().translation;
    assert!(
        (at.x - 10.0).abs() < 0.3 && (at.z - 10.0).abs() < 0.3,
        "the node should be AT the teleport point, not still at the spawn: {at:?}"
    );
}

/// The shape queries exist as globals and answer from Lua — the Rust unit
/// tests prove the geometry, this proves a script can actually reach it.
#[test]
fn shape_queries_are_callable_from_lua() {
    let dir = std::env::temp_dir().join("floptle_script_test_shape_api");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "probe",
        "function update(node, dt)\n  \
           kinds = type(overlapSphere) .. type(spherecast) .. type(capsulecast)\n  \
           n = #overlapSphere(vec3(0, 0, 0), 5)\n  \
           miss = spherecast(vec3(0, 0, 0), vec3(1, 0, 0), 0.5, 10)\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "probe".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    // No colliders lent, so the answers are "nothing" — but they must be
    // Answers (an empty list, a nil) rather than an error about a missing
    // global, which is what a query nobody wired would give.
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
}

#[test]
fn script_tunes_every_rigidbody_field() {
    // Every Inspector tunable on a Rigidbody is scriptable: read the mirror,
    // assign new values (booleans allowed), and the ECS component reflects
    // them after the same run() — which is what the live sim re-reads.
    let dir = std::env::temp_dir().join("floptle_script_test_rigidbody");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "ice",
        "function update(node, dt)\n\
         local rb = node:getcomponent(\"RigidBody\")\n\
         rb.friction = 0.02\n\
         rb.restitution = 0.9\n\
         rb.gravity = false\n\
         rb.shape = 2\n\
         rb.radius = 1.5\n\
         rb.height = 3.0\n\
         rb.half_x = 0.25\n\
         rb.half_y = 0.5\n\
         rb.half_z = 0.75\n\
         rb.lock_z = true\n\
         rb.lock_rot_x = true\n\
         rb.lock_rot_z = 1\n\
         if rb.lock_y then rb.friction = -1 end -- reads back as a BOOLEAN, and is false\n\
        end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(e, RigidBody::default());
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst { kind: "ice".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let rb = world.get::<RigidBody>(e).unwrap();
    assert!((rb.friction - 0.02).abs() < 1e-4, "friction = {}", rb.friction);
    assert!((rb.restitution - 0.9).abs() < 1e-4);
    assert!(!rb.gravity);
    assert_eq!(rb.kind, floptle_core::BodyKind::Box);
    assert!((rb.radius - 1.5).abs() < 1e-4);
    assert!((rb.height - 3.0).abs() < 1e-4);
    assert_eq!(rb.half_extents, [0.25, 0.5, 0.75]);
    assert_eq!(rb.lock_pos, [false, false, true]);
    assert_eq!(rb.lock_rot, [true, false, true]);
}

/// Collision/trigger hooks: `call_touch` dispatches to a script's
/// `onCollisionEnter(node, other, hit)` with the other node's handle and
/// the contact info — and never mis-fires a hook the script doesn't define.
#[test]
fn touch_dispatch_reaches_the_hook_with_other_and_hit() {
    let dir = std::env::temp_dir().join("floptle_script_test_touch");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "bumper",
        "function update(node, dt) end\n\
         function onCollisionEnter(node, other, hit)\n\
           -- prove we got the right other node + contact info\n\
           if other.name == \"Wall\" and hit.ny == 1 then\n\
             node.x = hit.x + 100\n\
           end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "bumper".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let wall = world.spawn();
    world.insert(wall, Transform::IDENTITY);
    world.insert(wall, floptle_core::Name("Wall".into()));
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0); // build envs + mirror
    host.call_touch(&mut world, e.index(), "onCollisionEnter", wall.index(), [7.0, 0.0, 0.0], [
        0.0, 1.0, 0.0,
    ]);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    assert_eq!(world.get::<Transform>(e).unwrap().translation.x, 107.0);
    // An undefined hook is a clean no-op.
    host.call_touch(&mut world, e.index(), "onTriggerEnter", wall.index(), [0.0; 3], [0.0; 3]);
    assert!(host.errors().is_empty());
}

/// Position writes on body nodes must queue real teleports — the physics
/// writeback stomps bare transform writes next frame, which silently ate
/// respawns ("G restores the ship… nothing moves") and the parked-in-hull
/// astronaut. Both write paths: own-node raw fields and cross-node handles.
#[test]
fn body_position_writes_queue_teleports() {
    let dir = std::env::temp_dir().join("floptle_script_test_teleport");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "teleporter",
        "function fixedUpdate(node, dt)\n\
           node.y = 50.0\n\
           local buddy = find(\"Buddy\")\n\
           if buddy then buddy.x = 7.0 end\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "teleporter".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    world.insert(e, floptle_core::Name("Pilot".into()));
    let buddy = world.spawn();
    world.insert(buddy, Transform::IDENTITY);
    world.insert(buddy, floptle_core::Name("Buddy".into()));
    let mut host = ScriptHost::new();
    // Both entities have bodies this tick (the gate for teleport queuing).
    let mut states = HashMap::new();
    for eid in [e.index(), buddy.index()] {
        states.insert(eid, BodyState::default());
    }
    host.set_bodies(states.clone());
    host.run(&mut world, &dir, 1.0 / 60.0, 0.0);
    host.set_bodies(states);
    host.run_fixed(&mut world, 1.0 / 60.0, 0.0);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tp = host.take_body_pos_changes();
    assert_eq!(
        tp.get(&e.index()).map(|p| p[1]),
        Some(50.0),
        "own-node position write must queue a body teleport (got {tp:?})"
    );
    assert_eq!(
        tp.get(&buddy.index()).map(|p| p[0]),
        Some(7.0),
        "cross-node handle position write must queue a body teleport (got {tp:?})"
    );
}

#[test]
fn raycast_hits_body_hulls_with_node_identity_and_self_exclusion() {
    let dir = std::env::temp_dir().join("floptle_script_test_hulls");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "caster",
        "function update(node, dt)\n\
           -- the explicit ignore makes the only other hull invisible too\n\
           if raycast(0, 0, 0, 1, 0, 0, 50, params.targetid) == nil then\n\
             node.scale = 3\n\
           end\n\
           local hit = raycast(node.x, node.y, node.z, 1, 0, 0, 50)\n\
           if hit then\n\
             node.y = hit.distance\n\
             if hit.node then node.z = 42 end\n\
           end\n\
           net.rpc(\"swing\", { dir = 1 }, { withInput = true })\n\
         end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "caster".into(),
            enabled: true,
            params: vec![("targetid".into(), (e.index() + 1000) as f32)], refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    // The caster's own hull sits at its position — without self-exclusion
    // the ray would hit it at distance 0.
    host.set_hulls(vec![hull(e.index(), 0.0), hull(e.index() + 1000, 5.0)]);
    host.run(&mut world, &dir, 0.01, 0.01);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let tr = world.get::<Transform>(e).unwrap();
    assert!(
        (tr.translation.y - 4.6).abs() < 0.05,
        "must hit the OTHER hull's surface (5 − 0.4), not itself: {}",
        tr.translation.y
    );
    assert_eq!(tr.translation.z, 42.0, "a body hit must carry hit.node");
    assert_eq!(tr.scale.x, 3.0, "the explicit `ignore` arg must skip that body");
    // `{withInput = true}` reaches the command queue.
    let cmds = host.take_net_commands();
    assert!(
        cmds.iter().any(|c| matches!(
            c,
            NetCmd::Rpc { name, with_input: true, .. } if name == "swing"
        )),
        "withInput must ride the rpc command: {cmds:?}"
    );
}

#[test]
fn second_script_on_a_body_node_must_not_clobber_velocity_writes() {
    // A movement controller sets the velocity; a weapon script on the same
    // node never touches it. The weapon's pass must not write the stale
    // seeded velocity back over the controller's (the sliding-player bug).
    let dir = std::env::temp_dir().join("floptle_script_test_two_scripts");
    let _ = std::fs::create_dir_all(&dir);
    write_script(&dir, "mover", "function update(node, dt)\n  node.vx = 5\n  node.vy = 7\nend\n");
    write_script(&dir, "weapon", "function update(node, dt)\n  -- looks at the node, never writes velocity\n  local _ = node.vx\nend\n");
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![
            floptle_core::ScriptInst { kind: "mover".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() },
            floptle_core::ScriptInst { kind: "weapon".into(), enabled: true, params: vec![], refs: Vec::new(), strs: Vec::new() },
        ]),
    );
    let mut host = ScriptHost::new();
    // The body's pre-hook state this frame (what node.vx is seeded with).
    let mut bodies = HashMap::new();
    bodies.insert(
        e.index(),
        BodyState { vel: [0.0, -2.0, 0.0], grounded: true, ..Default::default() },
    );
    host.set_bodies(bodies);
    host.run(&mut world, &dir, 0.016, 0.016);
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let changes = host.take_body_changes();
    assert_eq!(
        changes.get(&e.index()),
        Some(&[5.0, 7.0, 0.0f32]),
        "the controller's write must survive the weapon's pass"
    );
    // And a script that touches nothing queues nothing.
    assert!(host.take_body_height_changes().is_empty(), "untouched height must not queue");
}

/// Physics moving a body between hooks must not read as a pending write through the
/// stashed handle — otherwise every tick would teleport the body back to where the
/// table happened to be left. The drain compares the table against what the engine
/// last stamped into it, not against the transform.
#[test]
fn physics_moving_a_body_is_not_mistaken_for_a_stashed_write() {
    let dir = std::env::temp_dir().join("floptle_script_test_no_phantom_teleport");
    let _ = std::fs::create_dir_all(&dir);
    write_script(
        &dir,
        "rider",
        "function start(node) me = node end\n\
         function update(node, dt) seen = me.x end\n",
    );
    let mut world = World::default();
    let e = world.spawn();
    world.insert(e, Transform::IDENTITY);
    world.insert(
        e,
        Scripts(vec![floptle_core::ScriptInst {
            kind: "rider".into(),
            enabled: true,
            params: vec![],
            refs: Vec::new(),
            strs: Vec::new(),
        }]),
    );
    let mut host = ScriptHost::new();
    host.run(&mut world, &dir, 0.016, 0.0);
    // The driver (physics) moves the body between hooks; the script wrote nothing.
    for i in 1..=3 {
        world.get_mut::<Transform>(e).unwrap().translation.x = i as f64;
        host.run(&mut world, &dir, 0.016, i as f32 * 0.016);
        assert_eq!(
            world.get::<Transform>(e).unwrap().translation.x,
            i as f64,
            "the engine must not drag the body back to the table's last value"
        );
    }
    assert!(host.errors().is_empty(), "errors: {:?}", host.errors());
    let seen = host
        .instance_env(e.index(), "rider")
        .and_then(|env| env.get::<f64>("seen").ok())
        .unwrap_or(f64::NAN);
    assert_eq!(seen, 3.0, "and the stashed handle still reads the live pose");
}
