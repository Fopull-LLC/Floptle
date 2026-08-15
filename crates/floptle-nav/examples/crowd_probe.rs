//! Sixty units through one doorway — the case a crowd layer either handles or
//! deadlocks on.
//!
//! Two rooms joined by a two-metre gap, everybody ordered through it at once.
//! It reports how long the crowd took in game time, how long it cost the CPU,
//! and — the number that matters — how many actually arrived. A layer that
//! jams reports a handful arrived and the rest blocked, which is exactly what
//! it looks like in a game and is very hard to argue with.
//!
//! `cargo run --release -p floptle-nav --example crowd_probe`

use floptle_nav::{bake, AgentParams, AgentState, Crowd, NavSettings, Tri};

fn slab(x0: f32, z0: f32, w: f32, d: f32) -> Vec<Tri> {
    vec![
        Tri::new([x0, 0.0, z0], [x0 + w, 0.0, z0], [x0, 0.0, z0 + d]),
        Tri::new([x0 + w, 0.0, z0], [x0 + w, 0.0, z0 + d], [x0, 0.0, z0 + d]),
    ]
}

fn main() {
    let mut tris = slab(0.0, 0.0, 14.0, 14.0);
    tris.extend(slab(14.0, 6.0, 3.0, 2.0)); // the doorway
    tris.extend(slab(17.0, 0.0, 14.0, 14.0));

    let settings = NavSettings { agent_radius: 0.4, cell_size: 0.15, ..Default::default() };
    let t = std::time::Instant::now();
    let mesh = bake(&tris, &settings).expect("this level bakes");
    println!("bake: {} polygons in {:?}", mesh.polys.len(), t.elapsed());

    let mut crowd = Crowd::default();
    let mut units = Vec::new();
    for i in 0..60 {
        let (x, z) = (1.5 + (i % 10) as f32 * 1.1, 1.5 + (i / 10) as f32 * 1.1);
        let id = crowd.add(AgentParams { radius: 0.45, ..Default::default() }, [x, 0.0, z]);
        crowd.agent_mut(id).unwrap().move_to([25.0, 0.0, 7.0]);
        units.push(id);
    }

    let t = std::time::Instant::now();
    let mut steps = 0u32;
    for _ in 0..3600 {
        crowd.step(Some(&mesh), 1.0 / 60.0);
        steps += 1;
        if units.iter().all(|id| crowd.agent(*id).unwrap().arrived()) {
            break;
        }
    }
    let cpu = t.elapsed();

    let arrived = units.iter().filter(|id| crowd.agent(**id).unwrap().arrived()).count();
    let blocked = units
        .iter()
        .filter(|id| crowd.agent(**id).unwrap().state() == AgentState::Blocked)
        .count();
    println!(
        "{steps} steps ({:.1}s of game time), {cpu:?} of CPU — {:?} per step for 60 agents",
        steps as f32 / 60.0,
        cpu / steps.max(1)
    );
    println!("arrived {arrived}/60, blocked {blocked}");
}
