//! The editor's side of `weather.*`: keeping each wind on the node it rides.

impl crate::Editor {
    /// Where each anchored wind's node has got to this frame, before scripts
    /// run, so a planet's wind turns with the planet for every system that
    /// reads it this frame. Every anchor is found in one pass over the names.
    pub(crate) fn refresh_wind_frames(&mut self) {
        self.refresh_water_frames();
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
    use floptle_core::math::{DVec3, Quat};
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
}
