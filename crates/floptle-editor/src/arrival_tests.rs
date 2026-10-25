//! The frame a generated world lands in, while the game runs: what each step
//! of it costs, named.

use floptle_core::math::DVec3;
use floptle_core::transform::Transform;
use floptle_core::{CelestialBody, Matter, Name, ScriptInst, Scripts};

/// A running game on a GPU, a camera 60 m above where a generated world of
/// `radius` will stand, and a driver whose `update` makes the spherecast a
/// crew member's ground check does. The world starts cold: Play streams it
/// in from its recipe.
fn arrival_scene(tag: &str, radius: f64) -> Option<(crate::Editor, floptle_core::Entity, std::path::PathBuf)> {
    let mut ed = crate::offscreen::test_editor_with_gpu()?;
    let dir = std::env::temp_dir().join(format!("floptle_arrival_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    let up = radius + 60.0;
    floptle_vfs::write(
        dir.join("scripts/driver.lua"),
        format!("function update(node, dt)\n  spherecast(vec3(0, {up}, 0), vec3(0, -1, 0), 0.4, 100)\nend\n"),
    )
    .unwrap();
    ed.project_root = dir.clone();
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform::from_translation(DVec3::new(0.0, up, 0.0)));
    ed.world.insert(cam, Name("Camera".into()));
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
        },
    );
    let driver = ed.world.spawn();
    ed.world.insert(driver, Transform::IDENTITY);
    ed.world.insert(driver, Name("Driver".into()));
    ed.world.insert(driver, Matter::Empty);
    ed.world.insert(
        driver,
        Scripts(vec![ScriptInst {
            kind: "driver".into(),
            enabled: true,
            params: Vec::new(),
            refs: Vec::new(),
            strs: Vec::new(),
            pinned_bools: None,
        }]),
    );
    let fill = floptle_field::procgen::PlanetFill {
        seed: 1911,
        radius: radius as f32,
        voxel: 1.5,
        relief: 10.0,
        cave_depth: 30.0,
        mountains: Some(floptle_field::procgen::Mountains { height: 20.0, ..Default::default() }),
        defer_caves: true,
        ..Default::default()
    };
    let w = ed.world.spawn();
    ed.world.insert(w, Transform::IDENTITY);
    ed.world.insert(w, Name("World".into()));
    ed.world.insert(w, Matter::Terrain { id: 9, collision: Default::default() });
    ed.world.insert(w, CelestialBody { body_radius: radius, ..Default::default() });
    ed.world.insert(w, floptle_core::TerrainGen(ron::to_string(&fill).unwrap()));
    Some((ed, w, dir))
}

/// One frame of the game as a build runs it, with the draw into an
/// offscreen target (a headless device has no swapchain to draw into), and
/// the frame folded into the profiler.
struct Frames {
    color: wgpu::Texture,
    depth: wgpu::Texture,
}

impl Frames {
    fn new(ed: &crate::Editor) -> Self {
        let gpu = ed.gpu.as_ref().unwrap();
        let (color, depth) = crate::viewports::offscreen_textures(gpu, 320, 180, "arrival", wgpu::TextureUsages::COPY_SRC);
        Self { color, depth }
    }

    /// The frame's wall-clock milliseconds.
    fn frame(&self, ed: &mut crate::Editor) -> f64 {
        let t = std::time::Instant::now();
        let _ = ed.player_frame(false);
        let (cv, dv) = (self.color.create_view(&Default::default()), self.depth.create_view(&Default::default()));
        let depth = self.depth.clone();
        ed.timed_into(floptle_core::profile::Bucket::Render, |ed| {
            ed.render_game_into(cv, dv, Some(depth), [0.0, 0.0], [320.0, 180.0], 0.0, true, false);
        });
        ed.drain_script_logs();
        ed.end_profile_frame();
        t.elapsed().as_secs_f64() * 1e3
    }
}

fn describe(r: &floptle_core::profile::FrameRecord) -> String {
    let mut buckets = r.buckets.clone();
    buckets.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut parts = r.parts.clone();
    parts.sort_by(|a, b| b.1.total_cmp(&a.1));
    let b: Vec<String> = buckets.iter().map(|(b, ms)| format!("{}={ms:.1}", b.name())).collect();
    let p: Vec<String> = parts.iter().filter(|(_, ms)| *ms >= 0.5).map(|(p, ms)| format!("{p}={ms:.1}")).collect();
    let c: Vec<String> = r.calls.iter().map(|c| format!("{}:{}x{}={:.2}", c.script, c.call, c.n, c.ms)).collect();
    format!("{:.1} ms | {} | parts {} | calls {}", r.ms, b.join(" "), p.join(" "), c.join(" "))
}

/// **Probe**: every frame from Play to a while after the world lands, with
/// its buckets, parts and engine calls. `FLOPTLE_ARRIVAL_RADIUS` sets the
/// world's radius (300 m by default).
#[test]
#[ignore = "probe: prints the arrival frame's steps"]
fn probe_arrival_frame() {
    let radius = std::env::var("FLOPTLE_ARRIVAL_RADIUS").ok().and_then(|s| s.parse().ok()).unwrap_or(300.0);
    let Some((mut ed, w, dir)) = arrival_scene("probe", radius) else { return };
    ed.toggle_play();
    assert!(ed.playing);
    ed.script_host.profile().borrow_mut().enable(true);
    let frames = Frames::new(&ed);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
    let mut landed_at = None;
    let mut n = 0u64;
    while std::time::Instant::now() < until {
        let ms = frames.frame(&mut ed);
        n += 1;
        let here = ed.terrains.contains_key(&w);
        if here && landed_at.is_none() {
            landed_at = Some(n);
        }
        let p = ed.script_host.profile().borrow();
        let r = p.recent_frames(1).next().unwrap();
        if landed_at.is_some_and(|l| n <= l + 40) || ms > 30.0 {
            floptle_say::say_err!("frame {n}{} wall {ms:.1}: {}", if landed_at == Some(n) { " LANDED" } else { "" }, describe(r));
        }
        drop(p);
        if landed_at.is_some_and(|l| n > l + 40) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(landed_at.is_some(), "the world never landed");
}

/// **Probe**: a real project played headless on a GPU, frame by frame as a
/// build plays it, printing every frame over 40 ms with its parts.
/// `FLOPTLE_PROBE_PROJECT` names the project, `FLOPTLE_PROBE_SECS` how long.
#[test]
#[ignore = "probe: plays a project and prints its slow frames"]
fn probe_project_frames() {
    let Ok(project) = std::env::var("FLOPTLE_PROBE_PROJECT") else { return };
    let secs: u64 = std::env::var("FLOPTLE_PROBE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120);
    let Some(mut ed) = crate::offscreen::test_editor_with_gpu() else { return };
    ed.open_project(std::path::PathBuf::from(project));
    let now = floptle_core::time::Instant::now();
    ed.started = Some(now);
    ed.last = Some(now);
    ed.toggle_play();
    let frames = Frames::new(&ed);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut n = 0u64;
    while std::time::Instant::now() < until && ed.playing {
        let ms = frames.frame(&mut ed);
        n += 1;
        for e in ed.console.entries.drain(..) {
            if e.msg.starts_with("PB ") || e.msg.contains("Δ") {
                floptle_say::say_err!("  [{n}] {}", e.msg);
            }
        }
        let p = ed.script_host.profile().borrow();
        if let Some(r) = p.recent_frames(1).next()
            && ms > 40.0
        {
            floptle_say::say_err!("frame {n} wall {ms:.1}: {}", describe(r));
        }
    }
}

/// **A generated world lands with its ground drawn and its collision made,
/// and nothing else in the game is remade.** The frame Solar's home world
/// arrived in meshed every full-detail chunk round the player (280 ms,
/// debug), rebuilt the whole physics sim, and the first spherecast at the
/// ground then meshed its collision on the frame (20 ms). Counted, not
/// timed:
/// - the full-detail chunks drawn on the landing frame come from the meshes
///   made with the world;
/// - a cast every frame at the ground under the player meshes no collision
///   chunk itself;
/// - a terrain already in the sim keeps the collision meshes it had;
/// - the world made again with the camera since gone to its far side, where
///   nothing was made round it, lands meshing no more than
///   `NEAR_MESHES_A_FRAME` chunks a frame itself, the rest on the workers.
#[test]
fn a_generated_world_lands_with_its_ground_and_collision_made() {
    let Some((mut ed, w, dir)) = arrival_scene("lands", 300.0) else { return };
    // Made whole by `terrain.generatePlanet`, as Solar's home world is,
    // rather than streamed in from its recipe round the player.
    let fill: floptle_field::procgen::PlanetFill = ron::from_str(&ed.world.get::<floptle_core::TerrainGen>(w).unwrap().0).unwrap();
    ed.world.remove::<floptle_core::TerrainGen>(w);
    // A flat terrain far off, already in the game, whose collider has meshed
    // the ground something stood on.
    let slab = ed.world.spawn();
    ed.world.insert(slab, Transform::from_translation(DVec3::new(20_000.0, 0.0, 0.0)));
    ed.world.insert(slab, Matter::Terrain { id: 3, collision: Default::default() });
    let mut field = floptle_field::ChunkField::new(1.5);
    field.fill_slab(floptle_core::math::Vec3::new(-40.0, -6.0, -40.0), floptle_core::math::Vec3::new(40.0, 0.0, 40.0), 0.0, [0.4, 0.5, 0.3]);
    ed.terrains.insert(slab, crate::terrain_edit::EditorTerrain::new(field));
    ed.toggle_play();
    assert!(ed.playing);
    assert!(!ed.terrains.contains_key(&w), "the world should start cold for this test to mean anything");
    let slab_meshed = {
        use floptle_physics::CollisionShape;
        let c = ed.sim.as_mut().unwrap().terrain_collider_mut(slab.index()).unwrap();
        let _ = c.distance(floptle_core::math::Vec3::new(0.0, 0.5, 0.0));
        c.meshed_chunks()
    };
    assert!(slab_meshed > 0, "the slab's collider meshed nothing: the fixture tests nothing");
    let frames = Frames::new(&ed);
    let near = || crate::terrain_edit::NEAR_MESHES.with(|n| n.get());
    // Frames until the generation asked for lands, bounded by time (it is
    // made on workers), and what the landing frames meshed: `[most meshed
    // on one frame, drawn from the landing's meshes, sent to the workers]`.
    let land = |ed: &mut crate::Editor| -> [usize; 3] {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
        let mut seen = [0usize; 3];
        while ed.planet_gen_pending.contains(&9) {
            assert!(std::time::Instant::now() < until, "the world never landed");
            let before = near();
            let _ = frames.frame(ed);
            let after = near();
            seen[0] = seen[0].max(after[0] - before[0]);
            seen[1] += after[1] - before[1];
            seen[2] += after[2] - before[2];
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // …and the frames after it.
        for _ in 0..30 {
            let before = near();
            let _ = frames.frame(ed);
            let after = near();
            seen[0] = seen[0].max(after[0] - before[0]);
            seen[1] += after[1] - before[1];
            seen[2] += after[2] - before[2];
        }
        seen
    };
    ed.start_terrain_generates(vec![(9, fill.clone())]);
    let first = land(&mut ed);
    let sim = ed.sim.as_mut().unwrap();
    let world_meshed_by_casts = sim.terrain_collider_mut(w.index()).unwrap().meshed_by_queries();
    let slab_after = sim.terrain_collider_mut(slab.index()).unwrap().meshed_chunks();
    // Made again, and the camera gone to the far side before it lands.
    ed.start_terrain_generates(vec![(9, fill)]);
    let cam = ed.world.query::<Name>().find_map(|(e, n)| (n.0 == "Camera").then_some(e)).unwrap();
    ed.world.insert(cam, Transform::from_translation(DVec3::new(0.0, -360.0, 0.0)));
    let again = land(&mut ed);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    assert!(first[1] > 0, "the ground round the player was not drawn from the meshes made with the world");
    assert_eq!(slab_after, slab_meshed, "the other terrain's collision meshes were thrown away when the world landed");
    assert_eq!(world_meshed_by_casts, 0, "the casts at the ground meshed its collision on the frame");
    assert!(
        again[0] + again[2] > crate::terrain_edit::NEAR_MESHES_A_FRAME,
        "the far side asked for too little meshing to test the budget: {again:?}"
    );
    for (what, seen) in [("landed", first), ("landed again", again)] {
        assert!(
            seen[0] <= crate::terrain_edit::NEAR_MESHES_A_FRAME,
            "the world {what}: a frame meshed {} full-detail chunks itself (at most {})",
            seen[0],
            crate::terrain_edit::NEAR_MESHES_A_FRAME
        );
    }
}

/// A playing editor with a cold planet whose field is a file in its project,
/// 5 km from the origin, and a camera 80 m above it. What a scene loaded
/// mid-game beside an authored planet looks like.
fn planet_from_file(tag: &str) -> (crate::Editor, floptle_core::Entity, floptle_field::ChunkField, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(&dir).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let field = floptle_field::procgen::generate_planet(&floptle_field::procgen::PlanetFill {
        radius: 60.0,
        voxel: 1.5,
        relief: 4.0,
        cave_depth: 8.0,
        seed: 77,
        ..Default::default()
    });
    let path = ed.terrain_field_path_id(7);
    floptle_vfs::create_dir_all(path.parent().unwrap()).unwrap();
    floptle_vfs::write(&path, field.to_bytes()).unwrap();
    // The field as it reads back, which is what lands.
    let field = floptle_field::ChunkField::from_bytes(&field.to_bytes()).unwrap();
    let at = DVec3::new(5000.0, 0.0, 0.0);
    let rock = ed.world.spawn();
    ed.world.insert(rock, Transform::from_translation(at));
    ed.world.insert(rock, Name("Rock".into()));
    ed.world.insert(rock, Matter::Terrain { id: 7, collision: Default::default() });
    ed.world.insert(rock, CelestialBody { body_radius: 60.0, ..Default::default() });
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform::from_translation(at + DVec3::new(0.0, 80.0, 0.0)));
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
        },
    );
    (ed, rock, field, dir)
}

/// **A world read from its file on the frame, because something stands on
/// it, makes its shadow proxy off the frame.** Sampling the whole field into
/// its 192³ proxy on that frame was half a second of the frame Solar's
/// system scene arrived in (debug). The collider is there on that frame; the
/// proxy lands later, made from the same field, and is that field's proxy.
#[test]
fn a_world_read_on_the_frame_makes_its_shadow_off_it() {
    let (mut ed, rock, field, dir) = planet_from_file("sync");
    ed.toggle_play();
    // A body arrives beside it while the game runs (a teleport, a scene
    // loaded with one there): no Play-start hold covers it.
    let ball = ed.world.spawn();
    ed.world.insert(ball, Transform::from_translation(DVec3::new(5000.0, 80.0, 0.0)));
    ed.world.insert(ball, Matter::Empty);
    ed.world.insert(ball, floptle_core::RigidBody { gravity: false, radius: 0.5, ..Default::default() });
    let made_here = crate::terrain_edit::made_here("proxy");
    let cam = ed.lod_center(DVec3::new(5000.0, 80.0, 0.0));
    ed.update_terrain_residency(cam);
    assert!(ed.terrains.contains_key(&rock), "a body beside the planet: its field is read on this frame");
    assert!(ed.sim.as_mut().unwrap().terrain_collider_mut(rock.index()).is_some(), "the collider is there on the frame");
    assert_eq!(crate::terrain_edit::made_here("proxy") - made_here, 0, "the shadow proxy was made on the frame");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !ed.terrain_proxy_jobs.is_empty() {
        assert!(std::time::Instant::now() < until, "the proxy never landed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        ed.land_terrain_proxies();
    }
    let want = field.to_dense(crate::terrain_edit::TERRAIN_SHADOW_MAX_DIM).unwrap();
    let got = &ed.terrains[&rock].shadow;
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!((got.dims, got.center, got.half_extent), (want.dims, want.center, want.half_extent), "a different proxy landed");
    assert!(got.distance.iter().zip(&want.distance).all(|(a, b)| a.to_bits() == b.to_bits()) && got.color == want.color, "a different proxy landed");
    assert!(ed.terrain_atlas_dirty, "the atlas was not asked to take it");
}

/// **A camera alone beside a cold world streams it in.** Only something that
/// could fall through it has its ground read on the frame; a view that only
/// has to wait to draw it waits.
#[test]
fn a_camera_alone_beside_a_cold_world_streams_it_in() {
    let (mut ed, rock, _, dir) = planet_from_file("cam");
    ed.toggle_play();
    let cam = ed.lod_center(DVec3::new(5000.0, 80.0, 0.0));
    ed.update_terrain_residency(cam);
    assert!(!ed.terrains.contains_key(&rock), "the field was read on the frame with only a camera beside it");
    assert!(ed.terrain_load_jobs.iter().any(|j| j.e == rock), "and it is not on its way either");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while !ed.terrains.contains_key(&rock) {
        assert!(std::time::Instant::now() < until, "it never streamed in");
        std::thread::sleep(std::time::Duration::from_millis(2));
        ed.update_terrain_residency(cam);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// **The terrain palette is read off the frame, and the one drawn stays
/// until it is in.** Each slot's image read and resampled on the frame was
/// 66 to 320 ms of Solar's arrival and launch-load frames, under the
/// shadow upload's name.
#[test]
fn the_terrain_palette_is_read_off_the_frame() {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_palette_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("textures")).unwrap();
    for (name, c) in [("a", [200u8, 40, 40, 255]), ("b", [40, 40, 200, 255])] {
        let tex = floptle_render::TextureData { pixels: c.repeat(64 * 64), width: 64, height: 64 };
        std::fs::write(dir.join(format!("textures/{name}.png")), floptle_assets::encode_png(&tex).unwrap()).unwrap();
    }
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.set_terrain_slot_means(vec![[0.5; 3]]);
    ed.terrain_textures = vec!["textures/a.png".into(), "textures/b.png".into()];
    ed.terrain_textures_dirty = true;
    let read_here = crate::terrain_edit::made_here("palette");
    ed.sync_terrain_palette(false);
    assert_eq!(crate::terrain_edit::made_here("palette") - read_here, 0, "the palette was read on the frame");
    let until = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while ed.terrain_palette_job.is_some() {
        assert_eq!(ed.terrain_slot_means, vec![[0.5; 3]], "the palette changed before the new one was in");
        assert!(std::time::Instant::now() < until, "the palette never landed");
        std::thread::sleep(std::time::Duration::from_millis(2));
        ed.sync_terrain_palette(false);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(ed.terrain_slot_means.len(), 2, "the new palette's means: {:?}", ed.terrain_slot_means);
    assert!(ed.terrain_slot_means[0][0] > ed.terrain_slot_means[0][2] && ed.terrain_slot_means[1][2] > ed.terrain_slot_means[1][0], "{:?}", ed.terrain_slot_means);
}

/// **Probe**: what reading one field file costs, read and decode apart.
/// `FLOPTLE_PROBE_FIELD` names the file.
#[test]
#[ignore = "probe: times a field file's read"]
fn probe_field_read() {
    let Ok(path) = std::env::var("FLOPTLE_PROBE_FIELD") else { return };
    let t = std::time::Instant::now();
    let bytes = floptle_vfs::read(std::path::Path::new(&path)).unwrap();
    let read = t.elapsed().as_secs_f64() * 1e3;
    let t = std::time::Instant::now();
    let f = floptle_field::ChunkField::from_bytes(&bytes).unwrap();
    let parse = t.elapsed().as_secs_f64() * 1e3;
    let t = std::time::Instant::now();
    drop(f);
    let free = t.elapsed().as_secs_f64() * 1e3;
    let f = floptle_field::ChunkField::from_bytes(&bytes).unwrap();
    floptle_say::say_err!("FIELD {} MB: read {read:.1} ms, decode {parse:.1} ms, drop {free:.1} ms, {} data chunks, {} stored", bytes.len() / 1_000_000, f.data_chunks(), f.chunk_coords().len());
}

/// **A world landing mid-game brings its collision with it, and every other
/// terrain keeps its own** — the half of the landing guard above that needs
/// no GPU, so it runs wherever the suite does. A cast every step at the
/// ground under the player meshes no collision chunk itself, and a terrain
/// already in the sim keeps the collision meshes it had (the sim was rebuilt
/// whole on landing, and threw them away).
#[test]
fn a_world_landing_mid_game_brings_its_collision_and_keeps_the_others() {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_nogpu_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(dir.join("scripts/driver.lua"), "function update(node, dt)\n  spherecast(vec3(0, 210, 0), vec3(0, -1, 0), 0.4, 100)\nend\n").unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let driver = ed.world.spawn();
    ed.world.insert(driver, Transform::IDENTITY);
    ed.world.insert(driver, Matter::Empty);
    ed.world.insert(
        driver,
        Scripts(vec![ScriptInst { kind: "driver".into(), enabled: true, params: Vec::new(), refs: Vec::new(), strs: Vec::new(), pinned_bools: None }]),
    );
    let cam = ed.world.spawn();
    ed.world.insert(cam, Transform::from_translation(DVec3::new(0.0, 210.0, 0.0)));
    ed.world.insert(
        cam,
        Matter::Camera {
            fov_y: 1.0,
            active: true,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 0,
            target_h: 0,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
            stack: Vec::new(),
        },
    );
    let w = ed.world.spawn();
    ed.world.insert(w, Transform::IDENTITY);
    ed.world.insert(w, Matter::Terrain { id: 9, collision: Default::default() });
    ed.world.insert(w, CelestialBody { body_radius: 150.0, ..Default::default() });
    let slab = ed.world.spawn();
    ed.world.insert(slab, Transform::from_translation(DVec3::new(20_000.0, 0.0, 0.0)));
    ed.world.insert(slab, Matter::Terrain { id: 3, collision: Default::default() });
    let mut field = floptle_field::ChunkField::new(1.5);
    field.fill_slab(floptle_core::math::Vec3::new(-40.0, -6.0, -40.0), floptle_core::math::Vec3::new(40.0, 0.0, 40.0), 0.0, [0.4, 0.5, 0.3]);
    ed.terrains.insert(slab, crate::terrain_edit::EditorTerrain::new(field));
    ed.toggle_play();
    assert!(ed.playing);
    let slab_meshed = {
        use floptle_physics::CollisionShape;
        let c = ed.sim.as_mut().unwrap().terrain_collider_mut(slab.index()).unwrap();
        let _ = c.distance(floptle_core::math::Vec3::new(0.0, 0.5, 0.0));
        c.meshed_chunks()
    };
    assert!(slab_meshed > 0, "the slab's collider meshed nothing: the fixture tests nothing");
    ed.start_terrain_generates(vec![(
        9,
        floptle_field::procgen::PlanetFill { seed: 21, radius: 150.0, voxel: 1.5, relief: 8.0, cave_depth: 20.0, ..Default::default() },
    )]);
    // Bounded by time: the world is made on a worker.
    let until = std::time::Instant::now() + std::time::Duration::from_secs(300);
    while ed.planet_gen_pending.contains(&9) {
        assert!(std::time::Instant::now() < until, "the world never landed");
        ed.pump_world_streaming();
        ed.play_step(1.0 / 60.0, true);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let adopted = ed.sim.as_mut().unwrap().terrain_collider_mut(w.index()).unwrap().meshed_chunks();
    for _ in 0..30 {
        ed.pump_world_streaming();
        ed.play_step(1.0 / 60.0, true);
    }
    assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
    let sim = ed.sim.as_mut().unwrap();
    let world = sim.terrain_collider_mut(w.index()).unwrap();
    let (by_casts, meshed) = (world.meshed_by_queries(), world.meshed_chunks());
    let slab_after = sim.terrain_collider_mut(slab.index()).unwrap().meshed_chunks();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(adopted > 0, "the world landed with no collision meshes made");
    assert_eq!(slab_after, slab_meshed, "the other terrain's collision meshes were thrown away when the world landed");
    assert_eq!(by_casts, 0, "the casts at the ground meshed its collision on the frame ({meshed} chunks meshed in all)");
}

/// **A scene swap names its steps.** One `sceneLoad` of 120 to 460 ms for
/// the same scene said nothing of which step varied. Each step is a part,
/// and together they are no more than the swap took (a step's time includes
/// what its own buckets counted, the models one's `models` among them).
#[test]
fn a_scene_swap_names_its_steps() {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_swap_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scenes")).unwrap();
    let mut other = floptle_core::World::default();
    for i in 0..20 {
        let e = other.spawn();
        other.insert(e, Transform::from_translation(DVec3::new(i as f64, 0.0, 0.0)));
        other.insert(e, Name(format!("Box{i}")));
        other.insert(e, Matter::Empty);
    }
    let doc = floptle_scene::to_doc("other", &other);
    floptle_scene::save(&doc, &dir.join("scenes/other.ron")).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.toggle_play();
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    ed.end_profile_frame();
    profile.borrow_mut().begin_frame();
    let req = floptle_script::SceneRequest::Load { name: "other".into(), fade: None };
    let t = std::time::Instant::now();
    ed.timed_wait("sceneLoad", |ed| ed.perform_scene_request(&req));
    let took = t.elapsed().as_secs_f32() * 1e3;
    ed.end_profile_frame();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(ed.scene_name, "other", "the fixture's scene did not load");
    let p = profile.borrow();
    let r = p.recent_frames(1).next().unwrap();
    const STEPS: [&str; 11] = [
        "sceneSave",
        "sceneParse",
        "sceneTeardown",
        "sceneSpawn",
        "sceneTerrain",
        "sceneMaps",
        "sceneModels",
        "scenePhysics",
        "sceneAudio",
        "sceneSession",
        "sceneScripts",
    ];
    let missing: Vec<&str> = STEPS.iter().copied().filter(|s| !r.parts.iter().any(|(p, _)| p == s)).collect();
    assert!(missing.is_empty(), "the swap did not name {missing:?}: {:?}", r.parts);
    let steps: f32 = r.parts.iter().filter(|(p, _)| STEPS.contains(p)).map(|(_, ms)| ms).sum();
    assert!(steps <= took * 1.01 + 0.05, "the steps ({steps:.2} ms) add up to more than the swap ({took:.2} ms)");
}

/// **An additive layer names its steps, and leaves the world playing.**
/// Entering Solar's builder, a layer loaded over the running world, was one
/// `sceneLoad` of 68 to 274 ms with none of it named: the layer path named
/// no step. Each is a part now, as a swap's are. And it started the whole
/// world's sounds and effects again, not only its own: every play-on-start
/// effect in the running scene was made afresh from its first frame, and
/// every one-shot in the air was put out. Counted: a fire burning in the
/// running scene and a one-shot in the air; a layer of 200 nodes with a
/// fire of its own loads over them. The steps are named and no more than
/// the load took; the running fire's playhead goes on from where it was,
/// the one-shot is still in the air, and the layer's fire is lit.
#[test]
fn an_additive_layer_names_its_steps_and_leaves_the_world_playing() {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_layer_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scenes")).unwrap();
    floptle_vfs::create_dir_all(dir.join("vfx")).unwrap();
    floptle_scene::save_vfx_effect(&crate::vfx::starter_effect_doc("Puff"), &dir.join("vfx/Puff.vfx.ron")).unwrap();
    let mut layer = floptle_core::World::default();
    for i in 0..200 {
        let e = layer.spawn();
        layer.insert(e, Transform::from_translation(DVec3::new(i as f64, 0.0, 0.0)));
        layer.insert(e, Name(format!("Part{i}")));
        layer.insert(e, Matter::Empty);
    }
    let lit = layer.spawn();
    layer.insert(lit, Transform::IDENTITY);
    layer.insert(lit, Name("LayerFire".into()));
    layer.insert(lit, Matter::Empty);
    layer.insert(lit, floptle_core::ParticleSystem { asset: "vfx/Puff".into(), play_on_start: true });
    floptle_scene::save(&floptle_scene::to_doc("layer", &layer), &dir.join("scenes/layer.ron")).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    ed.vfx.rescan(&dir);
    let fire = ed.world.spawn();
    ed.world.insert(fire, Transform::IDENTITY);
    ed.world.insert(fire, Matter::Empty);
    ed.world.insert(fire, floptle_core::ParticleSystem { asset: "vfx/Puff".into(), play_on_start: true });
    ed.toggle_play();
    for _ in 0..30 {
        ed.play_step(1.0 / 60.0, true);
    }
    ed.vfx.spawn_detached_with(
        "vfx/Puff",
        crate::vfx::DetachedSpawn { emitter: Transform::IDENTITY, vel: Default::default(), frame: None, intensity: None, tint: None, up: None },
    );
    let playhead = ed.vfx.instances.get(&fire).map(|(_, i)| i.t).expect("the running scene's fire is not burning");
    assert!(playhead > 0.2, "the fire has played {playhead} s: the fixture tests nothing");
    assert_eq!(ed.vfx.detached.len(), 1, "the one-shot did not start");
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    ed.end_profile_frame();
    profile.borrow_mut().begin_frame();
    let req = floptle_script::SceneRequest::Additive { name: "layer".into(), environment: false, offset: None, replicate: false, parent: None };
    let t = std::time::Instant::now();
    ed.timed_wait("sceneLoad", |ed| ed.perform_scene_request(&req));
    let took = t.elapsed().as_secs_f32() * 1e3;
    ed.end_profile_frame();
    let _ = std::fs::remove_dir_all(&dir);
    let layer_fire = ed.world.query::<Name>().find(|(_, n)| n.0 == "LayerFire").map(|(e, _)| e).expect("the layer did not load");
    let p = profile.borrow();
    let r = p.recent_frames(1).next().unwrap();
    const STEPS: [&str; 7] = ["sceneParse", "sceneSpawn", "sceneMaps", "sceneModels", "scenePhysics", "sceneAudio", "sceneScripts"];
    let missing: Vec<&str> = STEPS.iter().copied().filter(|s| !r.parts.iter().any(|(p, _)| p == s)).collect();
    assert!(missing.is_empty(), "the layer did not name {missing:?}: {:?}", r.parts);
    let steps: f32 = r.parts.iter().filter(|(p, _)| STEPS.contains(p)).map(|(_, ms)| ms).sum();
    assert!(steps <= took * 1.01 + 0.05, "the steps ({steps:.2} ms) add up to more than the load ({took:.2} ms)");
    let after = ed.vfx.instances.get(&fire).map(|(_, i)| i.t).expect("the running scene's fire went out");
    assert!(after >= playhead, "the running scene's fire started again: its playhead went {playhead} -> {after}");
    assert_eq!(ed.vfx.detached.len(), 1, "the one-shot in the air was put out");
    assert!(ed.vfx.instances.contains_key(&layer_fire), "the layer's own fire was not lit");
}

/// **A save left for the menu can be deleted from the menu.** A game plays a
/// world with its terrain saved under one slot, goes back to a main menu
/// with no terrain, and deletes that save. `terrain.saveDir` still names the
/// slot (a game may set it in the menu before loading its world, so a scene
/// change cannot clear it), but nothing reads or writes it any more, so the
/// delete goes through, the folder goes with it, and the slot is released.
/// In the world itself the same call is refused, and says how to release it.
#[test]
fn a_save_left_for_a_menu_can_be_deleted_from_the_menu() {
    let dir = std::env::temp_dir().join(format!("floptle_arrival_savedel_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    floptle_vfs::create_dir_all(dir.join("scenes")).unwrap();
    floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
    floptle_vfs::write(
        dir.join("scripts/game.lua"),
        "function start(node)\n\
         \x20 terrain.saveDir('saves/slot1/terrain')\n\
         \x20 local ok, why = pcall(terrain.deleteSaveDir, 'saves/slot1/terrain')\n\
         \x20 print('world ok=' .. tostring(ok) .. ' why=' .. tostring(why))\n\
         end\n",
    )
    .unwrap();
    floptle_vfs::write(
        dir.join("scripts/menu.lua"),
        "function start(node)\n\
         \x20 local ok, n = pcall(terrain.deleteSaveDir, 'saves/slot1/terrain')\n\
         \x20 print('menu ok=' .. tostring(ok) .. ' n=' .. tostring(n) .. ' dir=' .. tostring(terrain.saveDir()))\n\
         end\n",
    )
    .unwrap();
    let scripted = |w: &mut floptle_core::World, kind: &str| {
        let e = w.spawn();
        w.insert(e, Transform::IDENTITY);
        w.insert(e, Name(kind.into()));
        w.insert(e, Matter::Empty);
        w.insert(
            e,
            Scripts(vec![ScriptInst {
                kind: kind.into(),
                enabled: true,
                params: vec![],
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
    };
    let mut menu = floptle_core::World::default();
    scripted(&mut menu, "menu");
    floptle_scene::save(&floptle_scene::to_doc("menu", &menu), &dir.join("scenes/menu.ron")).unwrap();
    let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
    let ground = ed.world.spawn();
    ed.world.insert(ground, Transform::IDENTITY);
    ed.world.insert(ground, Name("Ground".into()));
    ed.world.insert(ground, Matter::Terrain { id: 7, collision: Default::default() });
    scripted(&mut ed.world, "game");
    ed.toggle_play();
    ed.play_step(1.0 / 60.0, true);
    let slot = ed.script_host.terrain_save_path().expect("the world's script set no save slot");
    // What a played slot holds: a field the player dug.
    floptle_vfs::create_dir_all(&slot).unwrap();
    floptle_vfs::write(slot.join("world.7.cfield"), b"dug").unwrap();
    let said: Vec<String> = ed.script_host.drain_logs().into_iter().map(|l| l.msg).collect();
    let world = said.iter().find(|m| m.starts_with("world ok=")).cloned().unwrap_or_default();
    assert!(world.starts_with("world ok=false"), "the slot was deleted under the terrain using it: {said:?}");
    assert!(world.contains("terrain.saveDir(\"\")"), "the refusal does not say how to release it: {world}");

    let req = floptle_script::SceneRequest::Load { name: "menu".into(), fade: None };
    ed.perform_scene_request(&req);
    assert_eq!(ed.scene_name, "menu", "the menu did not load");
    ed.play_step(1.0 / 60.0, true);
    let said: Vec<String> = ed.script_host.drain_logs().into_iter().map(|l| l.msg).collect();
    let gone = !slot.exists();
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        said.iter().any(|m| m == "menu ok=true n=1 dir=nil"),
        "the menu could not delete the save it left: {said:?}"
    );
    assert!(gone, "the slot's folder is still there");
}

/// **A frame the GPU ran out of memory in names its untimed time.** The
/// failing allocation is slow and timed by nobody, and the error arrives
/// after it. The frame that sees the out-of-memory count move names what no
/// wait named as `gpuOutOfMemory`; the frame after, with the count still,
/// does not.
#[test]
fn a_frame_the_gpu_ran_out_of_memory_in_names_its_untimed_time() {
    let mut ed = crate::Editor::default();
    let profile = ed.script_host.profile().clone();
    profile.borrow_mut().enable(true);
    ed.end_profile_frame();
    let seen = floptle_render::out_of_memory_events();
    std::thread::sleep(std::time::Duration::from_millis(20));
    ed.name_gpu_out_of_memory(seen + 1);
    profile.borrow_mut().end_frame();
    let named = |p: &floptle_core::profile::FrameProfile| p.recent_frames(1).next().unwrap().part("gpuOutOfMemory");
    let first = named(&profile.borrow());
    assert!(first >= 10.0, "a 20 ms frame with an out-of-memory error named {first:.2} ms of it");
    ed.name_gpu_out_of_memory(seen + 1);
    profile.borrow_mut().end_frame();
    assert_eq!(named(&profile.borrow()), 0.0, "a frame with no new error was named too");
}
