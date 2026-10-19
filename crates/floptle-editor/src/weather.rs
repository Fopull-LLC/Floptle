//! The editor's side of `weather.*`: keeping each wind on the node it rides.

impl crate::Editor {
    /// Where each anchored wind's node has got to this frame, before scripts
    /// run, so a planet's wind turns with the planet for every system that
    /// reads it this frame. Every anchor is found in one pass over the names.
    pub(crate) fn refresh_wind_frames(&mut self) {
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
}
