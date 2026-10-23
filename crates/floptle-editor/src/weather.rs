//! The editor's side of `weather.*`: keeping each wind on the node it rides.

impl crate::Editor {
    /// Where each anchored wind's node has got to this frame, before scripts
    /// run, so a planet's wind turns with the planet for every system that
    /// reads it this frame. Every anchor is found in one pass over the names.
    pub(crate) fn refresh_wind_frames(&mut self) {
        self.refresh_undrawn_colliders();
        self.refresh_water_frames();
        self.step_water_sims();
        self.refresh_winds();
        // The wind particles are carried by: last frame's declarations at this
        // frame's clock, refreshed below once the anchors have moved.
        self.vfx.wind = (self.script_host.wind_field().clone(), self.script_host.weather_time());
    }

    fn refresh_winds(&mut self) {
        let anchored = self.script_host.anchored_winds();
        if anchored.is_empty() {
            return;
        }
        let mut nodes: std::collections::HashMap<&str, floptle_core::Entity> = std::collections::HashMap::new();
        for (e, n) in self.world.query::<floptle_core::Name>() {
            if anchored.iter().any(|(_, a)| *a == n.0) {
                nodes.entry(n.0.as_str()).or_insert(e);
            }
        }
        for (id, name) in &anchored {
            let frame = nodes.get(name.as_str()).map_or(floptle_core::scatter::Frame::IDENTITY, |&e| {
                let wt = floptle_core::world_transform(&self.world, e);
                floptle_core::scatter::Frame { origin: wt.translation, rot: wt.rotation.normalize() }
            });
            self.script_host.set_wind_frame(*id, frame);
        }
    }
}

/// Water the engine moves (`water.addVolume{ shape = "sim" }`): a shallow-
/// water grid over the ground under each such volume, stepped every Play
/// frame, its levels and flow handed back to the volume so floating, swimming
/// and `water.at` all read the water where it has flowed to.
#[derive(Default)]
pub(crate) struct WaterSims {
    sims: std::collections::HashMap<u32, WaterSim>,
    /// The Play clock at the last step, so each step covers the time since.
    last_t: Option<f32>,
    /// Volumes waiting for ground to be under them, and how many frames
    /// they have waited.
    waiting: std::collections::HashMap<u32, u32>,
}

struct WaterSim {
    grid: floptle_field::water::WaterGrid,
    water: floptle_field::water::ShallowWater,
    /// The level edit the ground was last read against.
    seen_edits: u64,
}

/// Steps a frame may take, at most: a long frame runs its water slow rather
/// than spending the next frame catching up.
const SIM_SUBSTEPS: usize = 8;
/// Depth below which a point is dry ground, not water.
const SIM_DRY: f32 = 1e-3;
/// Frames a simulated volume waits for ground under every point before it is
/// filled anyway.
const SEED_WAIT_FRAMES: u32 = 60;

impl crate::Editor {
    /// Step every simulated volume one frame, seeding the new ones from the
    /// ground under them and waking the water where the ground was dug.
    fn step_water_sims(&mut self) {
        use floptle_core::water::{HeightGrid, Surface};
        let Some(sim) = self.sim.as_ref() else {
            self.water_sims.sims.clear();
            self.water_sims.last_t = None;
            return;
        };
        let declared: Vec<floptle_core::water::DeclaredWater> =
            self.script_host.declared_water().iter().filter(|w| w.simulated.is_some()).cloned().collect();
        self.water_sims.sims.retain(|id, _| declared.iter().any(|w| w.id == *id));
        let edits = sim.world.level_edits;
        // The level is lent to the scripts for the frame by the time this
        // runs; it is the same level either way.
        let lent = self.script_host.lent_colliders();
        let cols: &[floptle_physics::AnchoredCollider] = if sim.world.colliders.is_empty() { &lent } else { &sim.world.colliders };
        // The time since the last step, from the Play clock: this runs once a
        // frame, and twice in a frame that ticks twice, so a clock rather than
        // a frame count is what keeps the water at the game's speed.
        let now = self.play_t;
        let dt = self.water_sims.last_t.map_or(0.0, |t| (now - t).max(0.0));
        self.water_sims.last_t = Some(now);
        for w in &declared {
            let (Some(level), Surface::Grid(g)) = (w.simulated, &w.surface) else { continue };
            // The ground under point (i, j): a ray down the parent's up from
            // well above the water, in the sim's frame.
            let ground_hit = |i: usize, j: usize| -> Option<f32> {
                let local = floptle_core::math::DVec3::new(g.x0 + i as f64 * g.cell, level as f64, g.z0 + j as f64 * g.cell);
                let up = (w.frame.to_world(floptle_core::math::DVec3::Y) - w.frame.origin).normalize_or_zero();
                let lift = (g.cell * (g.columns.max(g.rows) as f64)).max(20.0);
                let from = (w.frame.to_world(local) + up * lift - sim.world.origin).as_vec3();
                floptle_physics::raycast_ground_colliders(cols, from, -up.as_vec3(), (lift * 2.0) as f32)
                    .map(|hit| level + lift as f32 - hit.distance)
            };
            // Nothing under a point is a drop as deep as the reach of the ray.
            let floorless = level - (g.cell * (g.columns.max(g.rows) as f64)).max(20.0) as f32;
            let ground_at = |i: usize, j: usize| ground_hit(i, j).unwrap_or(floorless);
            if !self.water_sims.sims.contains_key(&w.id) {
                // Seeded once the ground is there to stand on: terrain's
                // collision lands a few frames into Play, and water filled
                // over ground that was not there yet would be a lake as deep
                // as the ray's reach. Seeded anyway after a second, for water
                // declared over a real drop.
                let tries = self.water_sims.waiting.entry(w.id).or_insert(0);
                *tries += 1;
                let ready = (0..g.rows).all(|j| (0..g.columns).all(|i| ground_hit(i, j).is_some()));
                if !ready && *tries < SEED_WAIT_FRAMES {
                    continue;
                }
                self.water_sims.waiting.remove(&w.id);
                let grid = floptle_field::water::WaterGrid::flat(g.columns, g.rows, g.cell as f32, ground_at);
                let water = floptle_field::water::ShallowWater::at_rest(&grid, &vec![level; grid.len()]);
                self.water_sims.sims.insert(w.id, WaterSim { grid, water, seen_edits: edits });
            }
            let Some(s) = self.water_sims.sims.get_mut(&w.id) else { continue };
            // Ground dug or filled since: read it again where it changed, and
            // wake the water there.
            if edits != s.seen_edits {
                let seen = std::mem::replace(&mut s.seen_edits, edits);
                let log = &sim.world.level_edit_log;
                let everywhere = log.front().is_none_or(|(first, _)| *first > seen + 1)
                    || log.iter().any(|(q, r)| *q > seen && r.is_none());
                let regions: Vec<(floptle_core::math::DVec3, f64)> =
                    log.iter().filter(|(q, _)| *q > seen).filter_map(|(_, r)| *r).collect();
                for j in 0..g.rows {
                    for i in 0..g.columns {
                        let c = j * g.columns + i;
                        let at = w.frame.to_world(floptle_core::math::DVec3::new(
                            g.x0 + i as f64 * g.cell,
                            level as f64,
                            g.z0 + j as f64 * g.cell,
                        ));
                        if everywhere || regions.iter().any(|(centre, r)| (at - *centre).length() < r + g.cell) {
                            let ground = ground_at(i, j);
                            if ground != s.grid.ground[c] {
                                s.grid.ground[c] = ground;
                                s.water.wake(c);
                                for &o in &s.grid.nbr[c] {
                                    if o != floptle_field::water::WALL {
                                        s.water.wake(o as usize);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Substeps no longer than the grid can take stably.
            if s.water.awake_count() > 0 && dt > 0.0 {
                let step = s.water.stable_dt(&s.grid).max(1e-3);
                let n = ((dt / step).ceil() as usize).clamp(1, SIM_SUBSTEPS);
                for _ in 0..n {
                    s.water.step(&s.grid, (dt / n as f32).min(step));
                }
            }
            let surface = s.water.surface(&s.grid);
            let levels = surface.iter().zip(&s.water.depth).map(|(l, d)| if *d > SIM_DRY { *l } else { f32::NAN }).collect();
            let flow = (0..s.grid.len()).map(|c| s.water.velocity(&s.grid, c)).collect();
            self.script_host.set_water_surface(
                w.id,
                Surface::Grid(std::sync::Arc::new(HeightGrid { levels, flow, ..(**g).clone() })),
            );
        }
    }
}

impl crate::Editor {
    /// Tell the decals which nodes draw nothing (Empty matter): a collider on
    /// one of those is a wall nobody sees, and a mark laid on it would hang in
    /// the air. Before the scripts, which lay marks. Only when there are marks
    /// or scripts that could lay them, which in Play is always; the scan is a
    /// pass over the nodes' matter.
    fn refresh_undrawn_colliders(&mut self) {
        let undrawn: std::collections::HashSet<u32> = self
            .world
            .query::<floptle_core::Matter>()
            .filter(|(_, m)| matches!(m, floptle_core::Matter::Empty))
            .map(|(e, _)| e.index())
            .collect();
        let store = self.script_host.decals().clone();
        let mut store = store.borrow_mut();
        if store.undrawn() != &undrawn {
            store.set_undrawn(undrawn);
        }
    }
}

impl crate::Editor {
    /// Where each anchored `water.addVolume` lake's node has got to, in the
    /// same pass the winds take, so a lake turns with its planet for the
    /// solver and for `water.at` alike.
    fn refresh_water_frames(&mut self) {
        let anchored = self.script_host.anchored_water();
        if anchored.is_empty() {
            return;
        }
        let mut nodes: std::collections::HashMap<&str, floptle_core::Entity> = std::collections::HashMap::new();
        for (e, n) in self.world.query::<floptle_core::Name>() {
            if anchored.iter().any(|(_, a)| *a == n.0) {
                nodes.entry(n.0.as_str()).or_insert(e);
            }
        }
        for (id, name) in &anchored {
            let frame = nodes.get(name.as_str()).map_or(floptle_core::scatter::Frame::IDENTITY, |&e| {
                let wt = floptle_core::world_transform(&self.world, e);
                floptle_core::scatter::Frame { origin: wt.translation, rot: wt.rotation.normalize() }
            });
            self.script_host.set_water_frame(*id, frame);
        }
    }

    /// The wind a swaying surface bends in, at the camera: one sample a frame,
    /// so every plant in view leans with the same gusts. Still air outside
    /// Play, where nothing has declared any.
    pub(crate) fn sway_wind(&self, at: floptle_core::math::DVec3) -> floptle_core::math::Vec3 {
        if !self.playing || self.vfx.wind.0.is_empty() {
            return floptle_core::math::Vec3::ZERO;
        }
        self.vfx.wind.0.at(at, self.vfx.wind.1)
    }

    /// The water scripts declared, as the solver's volumes in the sim frame
    /// about `origin`. Each is its own volume with an id no node has, so a
    /// body is matched to the water it is actually in.
    pub(crate) fn declared_water_volumes(&self, origin: floptle_core::math::DVec3) -> Vec<floptle_physics::WaterVolume> {
        self.script_host
            .declared_water()
            .iter()
            .map(|w| floptle_physics::WaterVolume {
                shape: floptle_physics::WaterShape::Declared { water: std::sync::Arc::new(w.clone()), origin },
                density: w.density,
                drag: w.drag,
                angular_drag: w.angular_drag,
                frozen: false,
                entity: u32::MAX - w.id,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use floptle_core::math::{DVec3, Quat, Vec3};
    use floptle_core::transform::Transform;

    /// **A planet's wind turns with the planet.** Declared in the planet's
    /// frame, it blows the same way across the planet's own ground however the
    /// planet has turned, so in the world it turns as the planet does.
    #[test]
    fn a_wind_on_a_planet_turns_as_the_planet_turns() {
        let dir = std::env::temp_dir().join(format!("floptle-wind-frame-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(
            dir.join("scripts/winds.lua"),
            "weather.addWind{ velocity = vec3(4, 0, 0), parent = \"Planet\" }\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let planet = ed.world.spawn();
        ed.world.insert(planet, Transform::IDENTITY);
        ed.world.insert(planet, floptle_core::Name("Planet".into()));
        ed.world.insert(
            planet,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "winds".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.script_host.set_playing(true);
        ed.script_host.run(&mut ed.world, &dir.join("scripts"), 1.0 / 60.0, 0.0);
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());

        let wind = |ed: &crate::Editor| ed.script_host.wind_field().at(DVec3::ZERO, 0.0);
        ed.refresh_wind_frames();
        assert!((wind(&ed) - floptle_core::math::Vec3::new(4.0, 0.0, 0.0)).length() < 1e-5, "{}", wind(&ed));
        // A quarter turn about +Y takes +X to -Z.
        if let Some(t) = ed.world.get_mut::<Transform>(planet) {
            t.rotation = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        }
        ed.refresh_wind_frames();
        assert!((wind(&ed) - floptle_core::math::Vec3::new(0.0, 0.0, -4.0)).length() < 1e-4, "{}", wind(&ed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A crate floats on water a script declared, and its current carries
    /// it.** The volume is in the frame of the node it lies on and reaches the
    /// solver every frame, so a light body dropped on it settles at its level
    /// and drifts downstream with the current.
    #[test]
    fn a_crate_floats_and_drifts_on_declared_water() {
        use floptle_core::{BodyKind, Matter, Name, RigidBody};
        let dir = std::env::temp_dir().join(format!("floptle-declared-water-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        // A 40 m square pond whose surface stands 2 m above the Pond node,
        // flowing along +X at 1 m/s.
        floptle_vfs::write(
            dir.join("scripts/pond.lua"),
            "local levels = {}\nfor i = 1, 21 * 21 do levels[i] = 2 end\n\
             water.addVolume{ shape = \"heightfield\", parent = \"Pond\", origin = vec3(-20, 0, -20), cell = 2,\n\
                              columns = 21, rows = 21, levels = levels, current = vec3(1, 0, 0) }\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let g = ed.world.spawn();
        ed.world.insert(g, Transform::IDENTITY);
        ed.world.insert(g, Matter::GravityVolume { mode: floptle_core::GravityMode::Down, strength: 9.81, radius: 500.0 });
        let pond = ed.world.spawn();
        ed.world.insert(pond, Transform { translation: DVec3::new(0.0, 10.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(pond, Name("Pond".into()));
        ed.world.insert(
            pond,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "pond".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        let crate_ = ed.world.spawn();
        ed.world.insert(crate_, Transform { translation: DVec3::new(0.0, 13.0, 0.0), ..Transform::IDENTITY });
        ed.world.insert(crate_, RigidBody { kind: BodyKind::Sphere, mass: 200.0, ..Default::default() });
        ed.toggle_play();
        assert!(ed.playing);
        for _ in 0..300 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let at = floptle_core::world_transform(&ed.world, crate_).translation;
        assert!(at.y > 10.5 && at.y < 13.0, "the crate sank or flew: y = {:.2} against a surface at 12", at.y);
        assert!(at.x > 2.0, "the current did not carry it: x = {:.2} after 5 s", at.x);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A particle effect that asks for the game's wind is carried by it.**
    /// The wind a script declares reaches every effect playing that Play
    /// frame, so smoke drifts downwind with no script reading the wind for it.
    #[test]
    fn a_scripts_wind_carries_the_smoke() {
        use floptle_core::{Matter, Name};
        use floptle_scene::{VfxForceDoc, VfxPropDoc, VfxSpaceDoc, VfxValueDoc};
        let dir = std::env::temp_dir().join(format!("floptle-wind-smoke-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(dir.join("scripts/winds.lua"), "weather.addWind{ velocity = vec3(5, 0, 0) }\n").unwrap();
        let mut doc = crate::vfx::starter_effect_doc("Smoke");
        doc.lifetime = 4.0;
        doc.tracks.truncate(1);
        let t = &mut doc.tracks[0];
        t.space = VfxSpaceDoc::World;
        t.velocity = VfxPropDoc::Const(VfxValueDoc::Vec3([0.0, 0.0, 0.0]));
        t.gravity = 0.0;
        t.drag = 0.0;
        t.forces = vec![VfxForceDoc::Wind { coupling: 4.0 }];
        floptle_scene::save_vfx_effect(&doc, &dir.join("vfx/Smoke.vfx.ron")).unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        ed.vfx.rescan(&dir);
        let host = ed.world.spawn();
        ed.world.insert(host, Transform::IDENTITY);
        ed.world.insert(host, Name("Weather".into()));
        ed.world.insert(host, Matter::Empty);
        ed.world.insert(
            host,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "winds".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        let fx = ed.world.spawn();
        ed.world.insert(fx, Transform::IDENTITY);
        ed.world.insert(fx, floptle_core::ParticleSystem { asset: "vfx/Smoke".into(), play_on_start: true });
        ed.toggle_play();
        for _ in 0..60 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let (_, inst) = ed.vfx.instances.get(&fx).expect("the smoke is playing");
        let p = inst.track_particles(0);
        assert!(p.count > 0, "no smoke");
        let mean = (0..p.count).map(|i| p.vel_life[i].x).sum::<f32>() / p.count as f32;
        assert!(mean > 2.0 && mean <= 5.0 + 1e-3, "the smoke drifts at {mean} m/s in a 5 m/s wind");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **Declared water looks like water from inside.** The camera under a
    /// script's lake takes its underwater tint and visibility as it would a
    /// Water Volume node's; above it, or with the look turned off, it does not.
    #[test]
    fn a_camera_under_declared_water_sees_the_underwater_look() {
        use floptle_core::water::{DeclaredWater, Surface, UnderwaterLook};
        let world = floptle_core::World::new();
        let look = UnderwaterLook { tint: [0.2, 0.1, 0.05], visibility: 9.0 };
        let lake = DeclaredWater {
            id: 1,
            anchor: None,
            frame: floptle_core::scatter::Frame::IDENTITY,
            surface: Surface::Lake { radius: 805.0, axis: DVec3::Y, reach: 60.0 },
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            current: floptle_core::math::Vec3::ZERO,
            floor: 6.0,
            look: Some(look),
            simulated: None,
        };
        let under = |w: &DeclaredWater, y: f64| crate::shading::underwater_at(&world, std::slice::from_ref(w), DVec3::new(0.0, y, 0.0));
        assert_eq!(under(&lake, 802.0), Some((look.tint, look.visibility)));
        assert_eq!(under(&lake, 806.0), None, "above the surface");
        assert_eq!(under(&lake, 790.0), None, "in a cave under the bed");
        assert_eq!(under(&DeclaredWater { look: None, ..lake.clone() }, 802.0), None, "no look asked for");
    }

    /// **Water the engine moves drains into a hole dug beside it.** A pool
    /// declared as a sim is filled over the ground the engine finds under it;
    /// dig a pit at its edge and the water runs into it, the pool's level
    /// drops, `water.at` reads the deeper water in the pit, and no water is
    /// made or lost on the way.
    #[test]
    fn simulated_water_drains_into_a_hole_dug_beside_it() {
        use floptle_field::{Brush, BrushProfile};
        let dir = std::env::temp_dir().join(format!("floptle-water-sim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        floptle_vfs::create_dir_all(dir.join("scripts")).unwrap();
        floptle_vfs::write(
            dir.join("scripts/pool.lua"),
            "water.addVolume{ shape = \"sim\", origin = vec3(-8, 0, -8), cell = 1, columns = 17, rows = 17, level = 1 }\n",
        )
        .unwrap();
        let mut ed = crate::Editor { project_root: dir.clone(), ..Default::default() };
        let mut field = floptle_field::ChunkField::new(0.5);
        field.fill_slab(Vec3::new(-12.0, -6.0, -12.0), Vec3::new(12.0, 0.0, 12.0), 0.0, [0.5, 0.5, 0.5]);
        let e = ed.world.spawn();
        ed.world.insert(e, Transform::IDENTITY);
        ed.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(field));
        let host = ed.world.spawn();
        ed.world.insert(host, Transform::IDENTITY);
        ed.world.insert(
            host,
            floptle_core::Scripts(vec![floptle_core::ScriptInst {
                kind: "pool".into(),
                enabled: true,
                params: Vec::new(),
                refs: Vec::new(),
                strs: Vec::new(),
                pinned_bools: None,
            }]),
        );
        ed.toggle_play();
        for _ in 0..10 {
            ed.play_step(1.0 / 60.0, true);
        }
        assert!(ed.script_host.errors().is_empty(), "{:?}", ed.script_host.errors());
        let depth_at = |ed: &crate::Editor, x: f64, z: f64| {
            let w = ed.script_host.declared_water();
            w[0].at(DVec3::new(x, 0.05, z)).depth
        };
        let volume = |ed: &crate::Editor| {
            let s = ed.water_sims.sims.values().next().expect("the pool is simulated");
            s.water.volume(&s.grid)
        };
        let still = depth_at(&ed, 0.0, 0.0);
        assert!((still - 0.95).abs() < 0.1, "the pool stands {still} m deep over the floor, not about 1");
        let before = volume(&ed);

        // A pit 2 m deep at the pool's edge, through the path `terrain.dig` takes.
        let touched = {
            let t = ed.terrains.get_mut(&e).unwrap();
            t.field.sculpt(Brush::Lower, Vec3::new(6.0, 0.0, 0.0), 2.0, 1.0, BrushProfile { hardness: 1.0, ..Default::default() })
        };
        ed.mirror_terrain_chunks_to_sim(e, &touched);
        for _ in 0..4 {
            ed.pump_collision_meshes();
        }
        for _ in 0..600 {
            ed.play_step(1.0 / 60.0, true);
        }
        let after = depth_at(&ed, 0.0, 0.0);
        // Water stands deeper in the pit than over the floor beside it.
        let water_depth = |ed: &crate::Editor, i: usize, j: usize| {
            let s = ed.water_sims.sims.values().next().unwrap();
            s.water.depth[j * 17 + i]
        };
        let (pit, pool) = (water_depth(&ed, 14, 8), water_depth(&ed, 8, 8));
        assert!(after < still - 0.01, "the pool did not drain into the pit: {still} → {after}");
        assert!(pit > pool + 1.0, "the pit holds {pit} m of water against the pool's {pool}");
        let lost = (volume(&ed) - before).abs() / before;
        assert!(lost < 0.01, "{:.2}% of the water was made or lost", lost * 100.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A sealed cave under the sea is a cave.** The camera 20 m below sea
    /// level with rock overhead sees no underwater tint; under open water at
    /// the same depth it does.
    #[test]
    fn a_sealed_cave_under_the_sea_is_not_under_water() {
        use floptle_core::{Collidable, Matter, WaterKind};
        let mut ed = crate::Editor::default();
        let sea = ed.world.spawn();
        ed.world.insert(sea, Transform::IDENTITY);
        let mut water = Matter::default_water();
        if let Matter::WaterVolume { kind, radius, tint, .. } = &mut water {
            *kind = WaterKind::Sea;
            *radius = 100.0;
            *tint = [0.9, 0.1, 0.1];
        }
        ed.world.insert(sea, water);
        // A slab of rock 4 m thick over (0, 80, 0), 6 m above it.
        let roof = ed.world.spawn();
        ed.world.insert(
            roof,
            Transform { translation: DVec3::new(0.0, 88.0, 0.0), scale: Vec3::new(20.0, 4.0, 20.0), ..Transform::IDENTITY },
        );
        ed.world.insert(roof, Matter::Primitive { shape: floptle_core::Shape::Cube, color: [0.5, 0.5, 0.5] });
        ed.world.insert(roof, Collidable);
        ed.toggle_play();
        let light = floptle_core::Light::default();
        let fog = |ed: &crate::Editor, p: DVec3| {
            let roofed = crate::shading::roofed_by(ed.sim.as_ref());
            crate::shading::fog_uniforms_and_particles_at(&light, &ed.world, &[], &roofed, p).0 .0
        };
        let tinted = |c: [f32; 4]| (c[0] - 0.9).abs() < 1e-4 && (c[1] - 0.1).abs() < 1e-4;
        assert!(!tinted(fog(&ed, DVec3::new(0.0, 80.0, 0.0))), "the sealed cave took the sea's underwater tint");
        assert!(tinted(fog(&ed, DVec3::new(40.0, 80.0, 0.0))), "open water at the same depth lost it");
    }
}
