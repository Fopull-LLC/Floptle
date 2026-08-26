//! Play mode + simulation: building the physics sim from the scene (colliders,
//! gravity field), play/pause lifecycle, and script-host synchronization.

use floptle_core::Entity;
use floptle_core::Matter;
use floptle_core::ScriptInst;
use floptle_core::Scripts;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::math::Vec3;
use floptle_core::transform::Transform;
use std::path::Path;
use crate::assets::{is_script, script_kind_of};
use crate::dock::{EditorTab};
use crate::{Editor, grab_cursor};

/// One dynamic body's state, as scripts read it (`node.vx`, `node.grounded`,
/// `node.groundNormal`, …).
///
/// One function rather than the same five-field literal at each of the play
/// loop's feed points: the frame pass, the tick pass, the post-physics pass,
/// the hidden harness server and a rollback replay all have to agree about what
/// a body looks like, and they only did by coincidence.
pub(crate) fn body_state(r: &floptle_physics::BodyReport) -> floptle_script::BodyState {
    floptle_script::BodyState {
        vel: r.vel.to_array(),
        up: r.up.to_array(),
        grounded: r.grounded,
        height: r.height,
        pos: [r.pos.x, r.pos.y, r.pos.z],
        ground_normal: r.ground_normal.map(|n| n.to_array()),
        wall_normal: r.wall_normal.map(|n| n.to_array()),
    }
}

impl Editor {
    /// Build the physics gravity field from the scene's GravityVolume nodes: `Down`
    /// volumes add uniform −Y gravity (the level's base), `Radial` volumes add a planet
    /// gravity well at the node. No GravityVolume node → ZERO gravity (a space/zero-g
    /// world). Takes `&World` (not `&self`) so it can be called from the play loop
    /// while `self.gpu`/egui are mutably borrowed — see call site.
    /// Build the scene's gravity field for the sim. `origin` is the sim's world origin
    /// (ADR-0015): radial centers are converted to the sim frame in f64 here, so a
    /// planet placed far out pulls exactly.
    pub(crate) fn build_gravity_field(world: &floptle_core::World, origin: DVec3) -> floptle_physics::GravityField {
        use floptle_core::{GravityMode, Matter};
        let mut field = floptle_physics::GravityField::default();
        for (e, m) in world.query::<Matter>() {
            if let Matter::GravityVolume { mode, strength, radius } = m {
                match mode {
                    GravityMode::Down => field
                        .sources
                        .push(floptle_physics::GravitySource::Uniform(Vec3::new(0.0, -*strength, 0.0))),
                    GravityMode::Radial => {
                        let p = floptle_core::world_transform(world, e).translation;
                        field.sources.push(floptle_physics::GravitySource::Point {
                            center: (p - origin).as_vec3(),
                            strength: *strength,
                            radius: *radius,
                        });
                    }
                }
            }
        }
        // Celestial bodies (solar demo S2): real µ/r² sources with patched-conic
        // SOI dominance — the deepest body whose SOI contains you is the ONE
        // that pulls (see `GravitySource::InvSq`). SOI 0 auto-derives Laplace
        // from the parent's µ and the orbit's semi-major axis.
        let cb: Vec<(Entity, floptle_core::CelestialBody, DVec3)> = world
            .query::<floptle_core::CelestialBody>()
            .map(|(e, b)| (e, b.clone(), floptle_core::world_transform(world, e).translation))
            .collect();
        let name_of = |e: Entity| {
            world.get::<floptle_core::Name>(e).map(|n| n.0.clone()).unwrap_or_default()
        };
        for (e, b, pos) in &cb {
            if b.mu <= 0.0 {
                continue;
            }
            let _ = e;
            let soi = if b.soi > 0.0 {
                b.soi
            } else if b.parent.is_empty() {
                0.0 // root: infinite (≤ 0 means unbounded in the source)
            } else if let Some((_, pb, _)) =
                cb.iter().find(|(pe, ..)| name_of(*pe) == b.parent)
            {
                floptle_core::frames::System::soi_radius(b.a.abs(), b.mu, pb.mu)
            } else {
                0.0
            };
            field.sources.push(floptle_physics::GravitySource::InvSq {
                center: (*pos - origin).as_vec3(),
                mu: b.mu as f32,
                soi: soi as f32,
                body_r: b.body_radius as f32,
            });
        }
        field
    }

    /// Build the sim's water field from the scene's WaterVolume nodes
    /// (`floptle/0038`) — the same shape as [`Self::build_gravity_field`], and
    /// for the same reason: a body asks the world one question and gets one
    /// answer, whether the water is a planet's ocean or a fish tank.
    ///
    /// Centres are converted into the sim frame in f64 here, so a sea placed
    /// far out is exact (ADR-0015), and the sea's own radius survives the
    /// node's scale — scaling a sea node scales the sea.
    pub(crate) fn build_water_field(
        world: &floptle_core::World,
        origin: DVec3,
    ) -> floptle_physics::WaterField {
        use floptle_core::{Matter, WaterKind};
        let mut field = floptle_physics::WaterField::default();
        for (e, m) in world.query::<Matter>() {
            let Matter::WaterVolume {
                kind,
                radius,
                half_extents,
                density,
                drag,
                angular_drag,
                frozen,
                ..
            } = m
            else {
                continue;
            };
            if floptle_core::is_disabled(world, e) {
                continue;
            }
            let wt = floptle_core::world_transform(world, e);
            let center = (wt.translation - origin).as_vec3();
            let shape = match kind {
                WaterKind::Sea => floptle_physics::WaterShape::Sphere {
                    center,
                    // A sea scales with its node uniformly — a non-uniform
                    // scale on a sphere has no single honest radius, so the
                    // largest axis wins rather than silently averaging.
                    radius: radius * wt.scale.max_element().max(1e-4),
                },
                WaterKind::Pool => floptle_physics::WaterShape::Box {
                    center,
                    half: Vec3::new(
                        half_extents[0] * wt.scale.x,
                        half_extents[1] * wt.scale.y,
                        half_extents[2] * wt.scale.z,
                    )
                    .abs()
                    .max(Vec3::splat(1e-4)),
                    rot: wt.rotation,
                },
            };
            field.volumes.push(floptle_physics::WaterVolume {
                shape,
                density: density.max(1e-3),
                drag: drag.max(0.0),
                angular_drag: angular_drag.max(0.0),
                frozen: *frozen,
                entity: e.index(),
            });
        }
        field
    }

    /// Where the sim's local frame should be centered at Play (ADR-0015): the active
    /// camera if there is one, else the first rigidbody, else the world origin —
    /// rounded to whole units so every later rebase shift stays exact in f32.
    pub(crate) fn sim_origin_hint(&self) -> DVec3 {
        use floptle_core::Matter;
        let focus = self
            .world
            .query::<Matter>()
            .find_map(|(e, m)| matches!(m, Matter::Camera { active: true, .. }).then_some(e))
            .or_else(|| self.world.query::<floptle_core::RigidBody>().map(|(e, _)| e).next());
        focus
            .map(|e| floptle_core::world_transform(&self.world, e).translation.round())
            .unwrap_or(DVec3::ZERO)
    }

    /// Every tilemap whose tileset marks tiles solid but whose node is not
    /// `Collidable`, named.
    ///
    /// This is the one shape of failure a 2D game hits first and cannot debug:
    /// the level is drawn, the tiles are ticked solid in the ▦ Tiles tab, the
    /// player falls through it, and everything an author can look at says it
    /// should be standing. The tileset is right and the node is missing a
    /// component nothing pointed at.
    ///
    /// A warning rather than a silent fix. Making a solid tileset imply
    /// `Collidable` would turn on collision in every project that ever painted
    /// one, including levels built to be walked through.
    pub(crate) fn solid_tilemaps_that_cannot_collide(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (e, m) in self.world.query::<Matter>() {
            let Matter::Tilemap { data, tileset, .. } = m else { continue };
            if tileset.is_empty() || floptle_core::is_disabled(&self.world, e) {
                continue;
            }
            // A RigidBody makes its own collider, so a tilemap carrying one is
            // not missing anything.
            if self.world.get::<floptle_core::Collidable>(e).is_some()
                || self.world.get::<floptle_core::RigidBody>(e).is_some()
            {
                continue;
            }
            let Some(set) = self.tiles.get(tileset) else { continue };
            let solid = floptle_tiles::solid_count(data, set);
            if solid > 0 {
                out.push(format!(
                    "Tilemap “{}” has {solid} solid squares but is not Collidable — nothing \
                     will collide with it. Tick Collidable on the node.",
                    self.world
                        .get::<floptle_core::Name>(e)
                        .map(|n| n.0.clone())
                        .unwrap_or_else(|| format!("node {}", e.index()))
                ));
            }
        }
        out
    }

    pub(crate) fn add_static_colliders(&self, sim: &mut floptle_physics::Sim) {
        // Union of Collidable + legacy MeshCollider entities (dedup; a node flagged both
        // is added once). A node with a RigidBody is a *dynamic* body (Sim::build made it
        // one) — skip it here so its dynamic body doesn't fight a static collider sitting at
        // the same spot (which would freeze/eject it). Collidable = static world geometry
        // only when there's no RigidBody.
        let mut ents: Vec<Entity> = self
            .world
            .query::<floptle_core::Collidable>()
            .map(|(e, _)| e)
            .filter(|e| self.world.get::<floptle_core::RigidBody>(*e).is_none())
            .collect();
        for (e, _) in self.world.query::<floptle_core::MeshCollider>() {
            if !ents.contains(&e) && self.world.get::<floptle_core::RigidBody>(e).is_none() {
                ents.push(e);
            }
        }
        // A switched-off node is off for physics too. Leaving an invisible wall standing
        // where a disabled node used to be is the bug people spend an evening on.
        ents.retain(|e| !floptle_core::is_disabled(&self.world, *e));
        for e in ents {
            let wt = floptle_core::world_transform(&self.world, e);
            // Anchor each collider on its own node (full f64) and bake geometry
            // RELATIVE to it — the residuals stay small and exact no matter how far
            // out the node sits (ADR-0015); the sim re-anchors them per rebase.
            let anchor = wt.translation;
            let s = wt.scale;
            // The node's identity for this collider: resolved layer bit (the
            // collision matrix + masked raycasts filter with it), entity (what
            // touch events name), and the trigger flag (sensor: events only).
            let layer = sim.tag_for(&self.world, e);
            match self.world.get::<Matter>(e) {
                Some(Matter::Mesh { asset_path }) => {
                    let path = asset_path.clone();
                    let Ok(model) = floptle_assets::gltf_import::import(std::path::Path::new(&path)) else {
                        eprintln!("collidable mesh: failed to load {path}");
                        continue;
                    };
                    // Scale + rotate locally (f32 is exact here — model-sized numbers);
                    // the node's translation lives in the f64 anchor, never the verts.
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, Vec3::ZERO);
                    let mut verts: Vec<Vec3> = Vec::new();
                    let mut indices: Vec<u32> = Vec::new();
                    for part in &model.parts {
                        let base = verts.len() as u32;
                        verts.extend(part.mesh.vertices.iter().map(|v| m.transform_point3(Vec3::from(v.pos))));
                        indices.extend(part.mesh.indices.iter().map(|i| i + base));
                    }
                    sim.add_static_mesh(anchor, &verts, &indices, layer);
                }
                // Map meshes: the kernel geometry IS the collider (all slots
                // concatenated) — a blockout wall collides exactly where it draws.
                Some(Matter::MapMesh { id }) => {
                    let Some(mesh) = self.maps.meshes.get(id) else { continue };
                    let m = Mat4::from_scale_rotation_translation(s, wt.rotation, Vec3::ZERO);
                    let mut verts: Vec<Vec3> = Vec::new();
                    let mut indices: Vec<u32> = Vec::new();
                    for sm in floptle_map::triangulate(mesh) {
                        let base = verts.len() as u32;
                        verts.extend(sm.positions.iter().map(|p| m.transform_point3(Vec3::from(*p))));
                        indices.extend(sm.indices.iter().map(|i| i + base));
                    }
                    if indices.len() >= 3 {
                        sim.add_static_mesh(anchor, &verts, &indices, layer);
                    }
                }
                // Primitive geometry → matching analytic collider, sized to match the
                // mesh the renderer draws (cube half 0.7, sphere r 0.85, capsule r/half 0.5).
                // A TILEMAP's solid tiles, merged into as few boxes as the shape
                // allows (`floptle_tiles::collision_boxes`). Two reasons it is
                // merged rather than one box per square:
                //
                // 1. A 100x100 solid floor is 10,000 squares and ONE box. Ten
                //    thousand static colliders is more than most whole 3D levels
                //    have, and the sim rebuilds its index over all of them.
                // 2. A character sliding along a row of separate boxes catches on
                //    the seams between them — each box's face is its own plane, and
                //    at a shallow angle the depenetration pass ticks across each
                //    boundary. One merged box has no interior seams.
                //
                // Depth is one tile: a 2D game's collider has to have SOME depth to
                // be a box, and a tile's own size is the only defensible choice —
                // it keeps a character with any thickness at all inside the layer
                // rather than passing through a paper-thin wall.
                Some(m @ Matter::Tilemap { .. }) => {
                    crate::tile_edit::add_tilemap_colliders(sim, &self.tiles, &wt, m, layer);
                }
                Some(Matter::Primitive { shape, .. }) => match shape {
                    floptle_core::Shape::Cube => {
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.7 * s.z), wt.rotation, layer);
                    }
                    floptle_core::Shape::Plane => {
                        // Flat in Z → a thin box so you can stand on / collide with the quad.
                        sim.add_static_box(anchor, Vec3::new(0.7 * s.x, 0.7 * s.y, 0.02 * s.z.max(1.0)), wt.rotation, layer);
                    }
                    floptle_core::Shape::Sphere => {
                        sim.add_static_sphere(anchor, 0.85 * s.max_element(), layer);
                    }
                    floptle_core::Shape::Capsule => {
                        let up = wt.rotation * Vec3::Y;
                        sim.add_static_capsule(anchor, up, 0.5 * s.y, 0.5 * s.x.max(s.z), layer);
                    }
                },
                _ => {}
            }
        }
    }

    /// Build the play sim under the PROJECT'S LAYER TABLE: terrain + static
    /// colliders carry their node's layer bit, dynamic bodies resolve theirs,
    /// the collision matrix lands in the world, and the script host is lent
    /// the same table (`node.layer` validation + `raycast` layer filters).
    /// Nodes naming a layer the project no longer defines get a Console
    /// warning (they behave as Default). The one path every sim build takes —
    /// Play start, mid-play rebuilds, and scene switches.
    pub(crate) fn build_play_sim(&mut self) -> floptle_physics::Sim {
        let layers = self.project.build_layers();
        for (e, l) in self.world.query::<floptle_core::Layer>() {
            if layers.index_of(&l.0).is_none() {
                let name = self
                    .world
                    .get::<floptle_core::Name>(e)
                    .map(|n| n.0.clone())
                    .unwrap_or_else(|| format!("#{}", e.index()));
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    format!(
                        "node '{name}' is on unknown layer '{}' — treated as Default \
                         (define it in Project Settings → Layers)",
                        l.0
                    ),
                    None,
                );
            }
        }
        // Foot-gun guard: a celestial scene with a UNIFORM-Down GravityVolume
        // adds a constant world −Y pull on top of µ/r² — on the far side of a
        // planet that pushes AWAY from it, pumping orbital energy every pass
        // (it cost two debugging sessions as a mystery "orbit escape").
        {
            let has_celestial = self
                .world
                .query::<floptle_core::CelestialBody>()
                .any(|(_, b)| b.mu > 0.0);
            let down_volume = self.world.query::<floptle_core::Matter>().any(|(_, m)| {
                matches!(
                    m,
                    floptle_core::Matter::GravityVolume {
                        mode: floptle_core::GravityMode::Down,
                        strength,
                        ..
                    } if *strength != 0.0
                )
            });
            if has_celestial && down_volume {
                self.console.push(
                    floptle_script::LogLevel::Warn,
                    "scene mixes Celestial-Body µ/r² gravity with a uniform DOWN \
                     GravityVolume — the constant world −Y pull adds energy to orbits \
                     on a planet's far side (looks like mysterious escapes). Set the \
                     volume's strength to 0 or delete it."
                        .into(),
                    None,
                );
            }
        }
        let origin = self.sim_origin_hint();
        let gravity = Self::build_gravity_field(&self.world, origin);
        let terrain_vols = self.terrain_volumes(&layers);
        let mut sim =
            floptle_physics::Sim::build_layered(&self.world, &terrain_vols, gravity, origin, layers);
        drop(terrain_vols);
        // Add static colliders (any node flagged "Collidable", plus legacy mesh
        // colliders) so a character can walk on / bump into them, not just terrain.
        self.add_static_colliders(&mut sim);
        // …and say so, loudly, if a level was painted solid and left unable to
        // collide. See `solid_tilemaps_that_cannot_collide`.
        for warning in self.solid_tilemaps_that_cannot_collide() {
            self.console.push(floptle_script::LogLevel::Warn, warning, None);
        }
        // Water is a static field like gravity, sampled per step (`floptle/0038`).
        sim.world.water = Self::build_water_field(&self.world, origin);
        self.script_host.set_layers(sim.layers().clone());
        // …and the tilesets this scene's tilemaps reference, so `tm:solid` /
        // `tm:tags` / `tm:autotile` can answer. Lent rather than loaded by the
        // host: the host does no file I/O, so who owns the parse is unambiguous.
        self.script_host.set_tilesets(self.scene_tilesets());
        // …and every imported model's material slots, so `node:materials()` can
        // answer what a character's parts are CALLED. Same deal as the tilesets:
        // the parts are the importer's knowledge and the host does no file I/O.
        self.script_host.set_model_slots(self.model_slots());
        sim
    }

    /// [`Self::build_play_sim`] for a world that is NOT `self.world` — the
    /// referee's and a replay's ([`crate::shadow::ShadowSim`]).
    ///
    /// Same gravity field, same layers, same terrain volumes, same static
    /// colliders. That identity is the shadow's entire claim to authority: it
    /// agrees with the live simulation because it is running the same physics,
    /// not because two implementations happened to land in the same place.
    ///
    /// Skips only the diagnostics `build_play_sim` prints — the live build
    /// already said all of it, about the same scene, one line earlier.
    pub(crate) fn build_sim_for_world(&self, world: &floptle_core::World) -> floptle_physics::Sim {
        let layers = self.project.build_layers();
        // The origin comes from OUR world on purpose: it is a precision anchor,
        // and the two sims must round to the same one or every f64→f32 residual
        // differs. The shadow is the same scene, so this is the same answer.
        let origin = self.sim_origin_hint();
        let gravity = Self::build_gravity_field(world, origin);
        let terrain_vols = self.terrain_volumes(&layers);
        let mut sim =
            floptle_physics::Sim::build_layered(world, &terrain_vols, gravity, origin, layers);
        drop(terrain_vols);
        self.add_static_colliders_for_world(world, &mut sim);
        // The shadow's authority is that it runs the SAME physics — which now
        // includes the same water. A referee whose seas were dry would call
        // every splashdown a desync.
        sim.world.water = Self::build_water_field(world, origin);
        sim
    }

    /// Rebuild the live physics sim from the current scene. A no-op unless playing —
    /// called after a physics component (rigidbody / collider / type) changes mid-Play
    /// so the edit takes effect immediately. Bodies re-seed at their current transforms.
    pub(crate) fn rebuild_sim(&mut self) {
        if !self.playing {
            return;
        }
        // Bodies re-seed at their current transforms, but their VELOCITIES must
        // survive the rebuild — terrain streaming in mid-flight used to zero
        // them, dropping ships out of orbit ("the sun just sucked me up": zero
        // relative velocity under warp is a plummet straight into the star).
        let saved: Vec<(u32, floptle_core::math::Vec3)> = self
            .sim
            .as_ref()
            .map(|s| s.body_states().map(|r| (r.entity.index(), r.vel)).collect())
            .unwrap_or_default();
        // COMPOUNDS carry more runtime state that the rebuild must not drop:
        // the `anchored` flag AND angular velocity. Losing `anchored` silently
        // freed a launch-clamped vessel whenever terrain streamed in mid-clamp
        // (loaded saves stream terrain during Play), which left the ship's
        // damage model permanently disarmed — it flew fine but bounced off the
        // ground with no crashes. Snapshot and restore both.
        let saved_c: Vec<(u32, bool, floptle_core::math::Vec3, floptle_core::math::Vec3)> = self
            .sim
            .as_ref()
            .map(|s| s.compound_runtime_states())
            .unwrap_or_default();
        let mut sim = self.build_play_sim();
        for (eid, vel) in saved {
            sim.set_body_velocity(eid, vel);
        }
        for (eid, anchored, vel, ang) in saved_c {
            if anchored {
                // Re-clamp; set_compound_anchored zeroes velocities as it should.
                sim.set_compound_anchored(eid, true);
            } else {
                sim.set_compound_velocity(eid, vel);
                sim.set_compound_angular_velocity(eid, ang);
            }
        }
        self.sim = Some(sim);
    }

    /// Every terrain volume as `(node world translation, node-local field, layer
    /// bit, node entity)` — what the sim colliders anchor on (the entity is what
    /// touch events name). Each volume collides at its NATIVE resolution (the
    /// combined field is render-only), placed in full `f64` (ADR-0015).
    pub(crate) fn terrain_volumes(
        &self,
        layers: &floptle_core::Layers,
    ) -> Vec<floptle_physics::TerrainVolume<'_>> {
        self.terrains
            .iter()
            .map(|(&e, t)| {
                let (anchor, rot, scale) = self.terrain_world_frame_of(e);
                floptle_physics::TerrainVolume {
                    anchor,
                    field: &t.field,
                    layer: layers.index_for(&self.world, e),
                    eid: Some(e.index()),
                    rot,
                    scale,
                }
            })
            .collect()
    }

    /// The UNREADY terrains the game cannot start without: any celestial
    /// terrain body with NO resident field that a dynamic node (the player,
    /// the ship) is practically ON — within `RESIDENT_SYNC_RADII` body radii.
    /// Deliberately not just the COLD set: a spawn planet whose first
    /// generation is still running (▶ Generate then Play before it lands) is
    /// in NEITHER set — it was the hole that let the player fall through when
    /// Play started mid-generation. Falling through one of these is the bug
    /// class this exists to kill. Returns (entity, id, is_cold) — only cold
    /// entries are kickable (mid-generation ones land via the generation
    /// queue's own adopt).
    pub(crate) fn required_unready_terrains(&self) -> Vec<(Entity, u32, bool)> {
        let anchors: Vec<floptle_core::math::DVec3> = self
            .world
            .query::<floptle_core::RigidBody>()
            .map(|(e, _)| floptle_core::world_transform(&self.world, e).translation)
            .collect();
        self.world
            .query::<floptle_core::Matter>()
            .filter_map(|(e, m)| match m {
                floptle_core::Matter::Terrain { id } => Some((e, *id)),
                _ => None,
            })
            .filter(|(e, _)| !self.terrains.contains_key(e))
            .filter_map(|(e, id)| {
                let cb = self.world.get::<floptle_core::CelestialBody>(e)?;
                let p = floptle_core::world_transform(&self.world, e).translation;
                let reach =
                    cb.body_radius.max(1.0) * crate::terrain_edit::RESIDENT_SYNC_RADII;
                anchors
                    .iter()
                    .any(|a| (*a - p).length() < reach)
                    .then_some((e, id, self.terrain_cold.contains_key(&e)))
            })
            .collect()
    }

    /// G1/G2 residency, Play start: if the terrain under the player is still
    /// cold, HOLD the run (auto-pause) and stream it in the BACKGROUND — the
    /// game must not start until the ground exists, and it must not freeze the
    /// UI loading it either (the no-stutter rule). The residency driver
    /// releases the hold the moment nothing required is left cold.
    pub(crate) fn begin_play_terrain_hold(&mut self) {
        let need = self.required_unready_terrains();
        if need.is_empty() {
            return;
        }
        for (e, id, cold) in need {
            if cold {
                self.kick_terrain_load(e, id);
            } // mid-generation bodies land via the generation queue's adopt
        }
        self.play_stream_hold = true;
        self.paused = true;
        self.toast = Some(("⏳  Streaming world in…".into(), 60.0));
        self.console.push(
            floptle_script::LogLevel::Debug,
            "⏳ play held — streaming the terrain under the player (starts automatically)"
                .into(),
            None,
        );
    }

    /// G1 residency, Stop: terrains that streamed in during Play hand over to
    /// NORMAL edit-mode residency — an UNTOUCHED field simply stays resident
    /// (its RAM copy equals its disk source; dropping it only to re-stream it
    /// where the editor camera sits made Stop flicker and hitch). Only a field
    /// DUG during Play reverts to cold (Play changes are never kept) — after
    /// flushing to the save SLOT first when one is set (`terrain.saveDir`):
    /// that's player state, exactly what a slot is for (G2).
    pub(crate) fn drop_play_loaded_terrains(&mut self) {
        // Exit-path guarantee: settle the background checkpoint and put every
        // dirty field on disk in the slot BEFORE anything drops — the per-entity
        // writes below then skip whatever this already saved.
        self.flush_slot_terrains_sync();
        let dropped: Vec<Entity> = self.play_loaded_terrains.drain().collect();
        for e in dropped {
            if !self.world.is_alive(e) || !self.terrains.contains_key(&e) {
                continue;
            }
            if !self.terrain_disk_dirty.contains(&e) {
                continue; // clean: keep it resident, normal residency owns it now
            }
            let Some(floptle_core::Matter::Terrain { id }) =
                self.world.get::<floptle_core::Matter>(e).cloned()
            else {
                continue;
            };
            if self.terrain_disk_dirty.contains(&e)
                && let Some(sd) = self.script_host.terrain_save_dir()
            {
                let path = self
                    .project_root
                    .join(&sd)
                    .join(format!("{}.{id}.cfield", self.scene_name));
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                if let Some(t) = self.terrains.get(&e) {
                    let _ = std::fs::write(&path, t.field.to_bytes());
                }
            }
            let color = self
                .terrain_render
                .get(&e)
                .and_then(|r| r.impostor_color)
                .unwrap_or([0.75, 0.75, 0.78]);
            self.terrains.remove(&e);
            self.terrain_disk_dirty.remove(&e); // in-Play digs are not edits
            let render = self.terrain_render.entry(e).or_default();
            // The session may have flown right up to this body — free its live
            // chunk meshes (unlike an eviction, which only fires far away where
            // impostor mode already emptied them).
            if let Some(raster) = self.raster.as_mut() {
                for (_, (mid, _)) in render.slots.drain() {
                    raster.free_dynamic(mid);
                }
            }
            render.pending.clear();
            render.empty.clear();
            render.born.clear();
            render.impostor = true;
            render.impostor_color = Some(color);
            self.terrain_cold
                .insert(e, crate::terrain_edit::ColdTerrain { id, color });
            self.terrain_gpu_dirty = true;
        }
    }

    /// Enter/leave play mode. Play snapshots the authored scene and runs scripts;
    /// Stop restores the authored scene so script-driven changes aren't persisted.
    /// Drop every animator runtime + the Animating tab's entity bindings —
    /// called whenever the World is rebuilt (scene/project switches), since
    /// entity handles from the old world alias entities in the new one.
    pub(crate) fn reset_anim_bindings(&mut self) {
        self.stop_recording();
        self.anim.clear_instances();
        self.anim_ui.target = None;
        self.anim_ui.sel_anim = None;
        self.anim_ui.clip_doc = None;
        self.anim_ui.preview_playing = false;
        self.anim_ui.last_scene_local.clear();
    }

    /// Turn ● Record off and put the posed subtree back exactly as it was
    /// when recording started — recording authors the CLIP, never the scene.
    /// One implementation for every path (transport, play start, undo, save):
    /// restores transforms AND recorded property values, and forgets the
    /// preview snapshot (stale mid-record state — never to be applied).
    pub(crate) fn stop_recording(&mut self) {
        if !self.anim_ui.record && self.anim_ui.record_restore.is_empty() {
            return;
        }
        crate::anim_ui::stop_record_ui(&mut self.world, &mut self.anim_ui);
        self.anim.forget_preview();
    }

    pub(crate) fn toggle_play(&mut self) {
        // Fresh animator runtimes both ways (Play binds against the live scene;
        // Stop drops them so the restored scene isn't posed by stale animators).
        self.anim.clear_instances();
        // Same for particle instances — nothing emits outside Play (phase 1).
        self.vfx.clear_instances();
        self.anim_ui.preview_playing = false;
        // Recording must never run during Play (gameplay motion would bake into
        // the clip asset), and stale queued animator commands must not leak
        // across sessions.
        self.stop_recording();
        self.script_host.clear_anim_state();
        self.script_gizmos.clear();
        self.script_lines.clear();
        self.script_rects.clear();
        self.script_texts.clear();
        // Both directions of the Play toggle wipe action state: a key held
        // while editing must not read as a press the instant Play starts, and a
        // half-finished motion must not survive into (or out of) the session.
        self.reset_action_state();
        // The packages hear about it before anything else moves: a tool that
        // has to stand down for Play (an overlay, a pending edit) needs to do
        // so while the scene is still the one it was reasoning about.
        self.ext.fire(if self.playing {
            crate::ext::HookKind::Stop
        } else {
            crate::ext::HookKind::Play
        });
        self.drain_ext_log();
        if self.playing {
            self.playing = false;
            self.paused = false;
            // Everything on the wire belonged to the session that just ended.
            self.script_host.set_playing(false);
            self.play_stream_hold = false;
            // Make the revert EXPLICIT — "where did my tweaks go" is a classic
            // lost-work surprise: Play-mode changes are a simulation, not edits.
            self.console.push(
                floptle_script::LogLevel::Debug,
                "⏹ stopped — the scene reverted to its pre-Play state (changes made \
                 during Play are not kept)"
                    .into(),
                None,
            );
            // Silence the play session's sounds and revert Lua mixer tweaks.
            let mixer = self.project.mixer.clone();
            self.audio.stop_play(&mixer);
            self.sim = None; // drop the physics sim; restore reverts moved transforms
            // Multiplayer sessions live inside a play session — never across Stop.
            self.net_stop("play stopped");
            // Release any script-held mouse lock or Game-view cursor trap so you're not
            // stuck grabbed after Stop.
            if self.script_mouse_lock || self.game_trap {
                self.script_mouse_lock = false;
                self.game_trap = false;
                if let Some(window) = self.window.as_ref() {
                    self.cursor_lock_soft = grab_cursor(window, false);
                }
            }
            // …and the editor's own override goes with them. It only ever means
            // "a running game wants this and is not getting it"; there is no
            // running game now, and leaving it set would swallow the next
            // session's first legitimate lock.
            self.cursor_freed = false;
            // A mid-play `scene.load(...)` renamed the scene for the session —
            // the restored world is the PRE-PLAY scene, so its name must come
            // back BEFORE `restore()` runs: restore's `adopt_terrain()` loads
            // terrain fields by scene name, and doing this after it once made
            // Stop fill the editor scene's terrain nodes with the PLAYED
            // scene's fields (the next save then overwrote the real terrain
            // on disk — real lost work).
            self.pending_scene.clear();
            // Did a `scene.load` actually happen? The live name is the played scene's and
            // the snapshot holds the pre-Play one, so a difference IS the switch. Worth
            // knowing because the switch now reloads the paint stores (it has to — see
            // `switch_scene_during_play`), and those have no snapshot to come back from.
            let switched =
                self.play_scene_name.as_ref().is_some_and(|(name, _)| *name != self.scene_name);
            if let Some((name, rel)) = self.play_scene_name.take() {
                self.scene_name = name;
                self.scene_rel = rel;
            }
            if let Some(snap) = self.play_snapshot.take() {
                self.restore(snap);
            }
            // Any environment an additive layer was holding goes back with the
            // rest of Play: the restored world is the authored one, whose
            // environment was never lent out.
            self.env_layer = None;
            // Paint belongs to the scene it was painted on. A switch swapped these for the
            // played scene's, and unlike terrain and map geometry they are far too big to
            // snapshot per Play — texture paint is images — so they come back off disk.
            //
            // The cost is narrow and worth naming: paint edited but NOT saved before
            // pressing Play, in a session where a script then switched scenes, reverts to
            // what is on disk. Leaving another scene's paint loaded instead would be worse
            // and much harder to notice. Nothing is written here, so nothing is destroyed.
            if switched {
                self.adopt_paint();
                self.adopt_tex_paint();
                self.paint_meshes.clear();
                self.mesh_wire_cache.clear();
            }
            // Terrain fields live OUTSIDE the scene doc, so the snapshot above
            // doesn't carry them — bring back the exact pre-Play fields (+
            // texture palette). Disk can't stand in: it may be behind unsaved
            // sculpts, and a mid-play scene switch swapped the live fields for
            // the played scene's.
            // Persistent `save.*` data flushes on Stop — the one guarantee scripts
            // rely on (periodic flushes during Play only bound crash loss).
            self.script_host.flush_save();
            // Map geometry lives outside the scene doc too — and unlike terrain
            // it has no in-Play authoring path, so a restore here is purely a
            // guard against a script (or a stray Map-tab click) mutating the
            // level during a run.
            if let Some(meshes) = self.play_maps.take()
                && self.maps.meshes != meshes
            {
                for id in meshes.keys().chain(self.maps.meshes.keys()) {
                    self.maps.dirty.insert(*id);
                }
                self.maps.meshes = meshes;
            }
            if let Some((fields, palette)) = self.play_terrains.take() {
                for (id, t) in fields {
                    if let Some(e) = self.terrain_entity_of_id(id) {
                        self.terrains.insert(e, crate::terrain_edit::EditorTerrain::new(t));
                    }
                }
                self.terrain_textures = palette;
                self.terrain_textures_dirty = true;
                self.terrain_gpu_dirty = !self.terrains.is_empty();
            }
            // G1 residency: terrains that streamed IN during Play were cold at
            // Play start (not in the snapshot above) — drop them back to cold so
            // Play can't leak residency or persist in-Play digs on them. Their
            // on-disk field is untouched (nothing saves to the PROJECT during
            // Play), so cold + disk file IS the pre-Play state. (Fields dug
            // during Play with a save SLOT set flushed to the slot inside
            // drop_play_loaded_terrains — player state, not authoring.)
            self.drop_play_loaded_terrains();
            // The save slot never outlives its run.
            self.script_host.clear_terrain_save_dir();
        } else {
            // Scripts run from what's on DISK — flush unsaved IDE edits first so
            // Play always tests the code you're looking at.
            let mut flushed = 0;
            for f in self.ide.open.iter_mut().filter(|f| f.dirty) {
                if std::fs::write(&f.path, &f.text).is_ok() {
                    f.dirty = false;
                    flushed += 1;
                }
            }
            self.play_snapshot = Some(self.snapshot());
            self.play_scene_name = Some((self.scene_name.clone(), self.scene_rel.clone()));
            // Snapshot the live terrain fields (id-keyed) + texture palette so
            // Stop restores them exactly — unsaved sculpts survive Play, and a
            // mid-play scene switch can never leak another scene's terrain
            // into this one (see Stop above).
            self.play_maps = Some(self.maps.meshes.clone());
            self.play_terrains = Some((
                self.terrains
                    .iter()
                    .filter_map(|(&e, t)| match self.world.get::<floptle_core::Matter>(e) {
                        Some(floptle_core::Matter::Terrain { id }) => {
                            Some((*id, t.field.clone()))
                        }
                        _ => None,
                    })
                    .collect(),
                self.terrain_textures.clone(),
            ));
            self.pending_scene.clear();
            self.play_t = 0.0;
            self.paused = false;
            self.terrain_mirror_warned = false; // fresh Play, fresh one-shot warning
            self.space_time = 0.0; // rails restart from the authored epoch
            self.space_warp = 1.0;
            self.physics_paused = false; // a fresh run starts unpaused
            self.script_host.set_physics_paused(false);
            self.space_coast.clear();
            self.space_frame.clear(); // dominant-frame tracking restarts too
            self.compound_lod.clear(); // distant-craft LOD restarts with them
            self.lod_keep_live.clear(); // keep-live exemptions don't persist runs
            self.compound_coast.clear();
            self.script_lines.clear(); // no stale map lines across runs
            self.script_rects.clear();
            self.script_texts.clear();
            // Every Play is a FRESH RUN: drop all script instances so top-level
            // script state can't leak across sessions (Ty's ship still thought
            // he was piloting after Stop → Play). `start()` re-fires for all.
            self.script_host.reset_instances();
            // Fresh gameplay-tick clock (the netcode timebase): no banked time, tick 0,
            // and no stale per-tick input edges from before Play.
            self.game_tick.reset();
            self.game_tick_no = 0;
            self.tick_keys_pressed.clear();
            self.tick_keys_released.clear();
            self.tick_buttons_pressed = [false; 3];
            self.tick_mouse_delta = (0.0, 0.0);
            self.tick_scroll = 0.0;
            // G1/G2 residency: if the terrain under the player is still cold,
            // the run HOLDS (auto-paused) while it streams in the background —
            // the game never starts on an intangible planet, and the UI never
            // freezes loading one. Released by the residency driver.
            self.begin_play_terrain_hold();
            // Build the physics sim from the scene: RigidBody nodes + every terrain
            // volume (its own anchored SDF collider, native resolution) + the gravity
            // field from GravityVolume nodes + static colliders — all under the
            // project's layer table (collision matrix + raycast filters).
            let sim = self.build_play_sim();
            self.sim = Some(sim);
            // Start play with a clean Console so you only see this run's output.
            self.console.entries.clear();
            if flushed > 0 {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("⏵ auto-saved {flushed} edited script(s)"),
                    None,
                );
            }
            // Press Play → bring the Game tab to the front (active-camera view), so it's
            // clear you're testing the game, not the editor scene view.
            if let Some(dock) = self.dock_state.as_mut()
                && let Some(path) = dock.find_tab(&EditorTab::Game) {
                    let _ = dock.set_active_tab(path);
                }
            // Spawn play-on-start particle effects on their nodes.
            self.vfx.start_play(&self.world);
            // Fire play-on-start sounds through the project mixer.
            let mixer = self.project.mixer.clone();
            let root = self.project_root.clone();
            self.audio.start_play(&self.world, &root, &mixer);
            self.playing = true;
            // `http.*` and `openUrl` come alive with the session and not before.
            self.script_host.set_playing(true);
            // Outside a session, only player slot #1 takes input: extra
            // Predicted nodes (multiplayer slots) idle instead of mirroring
            // the keyboard into every copy of the controller.
            self.net_apply_offline_slots();
        }
    }

    /// Freeze/unfreeze the script clock while playing.
    pub(crate) fn toggle_pause(&mut self) {
        if self.playing {
            self.paused = !self.paused;
        }
    }

    /// Frame-step: release exactly `n` gameplay ticks. Pauses first if the game is
    /// running, because "advance one frame" only means something from a standstill —
    /// so ⏭ / F3 / `physics.step()` all do the obvious thing from either state.
    pub(crate) fn step_tick(&mut self, n: u32) {
        if !self.playing {
            return;
        }
        self.paused = true;
        self.tick_steps = self.tick_steps.saturating_add(n);
    }

    /// Frame-step BACKWARDS: put the simulation back exactly one gameplay tick
    /// (`docs/multiplayer.md` §7 P5 — closes 0024's deferred item).
    ///
    /// A simulation is not invertible, so this is not a general feature: it
    /// reads the rollback driver's state ring, which exists because rollback
    /// needs it anyway. That means it works in a rollback session and reaches
    /// back exactly as far as the ring does — a fifth of a second — and says so
    /// plainly rather than doing nothing when it can't.
    pub(crate) fn step_tick_back(&mut self) {
        if !self.playing {
            return;
        }
        self.paused = true;
        self.tick_steps = 0;
        let step = self.game_tick.step;
        let stepped = match (self.net_rollback.take(), self.sim.as_mut()) {
            (Some(mut d), Some(sim)) => {
                let mut ctx = crate::rollback::Ctx {
                    world: &mut self.world,
                    sim,
                    host: &mut self.script_host,
                    step,
                };
                let at = d.step_back(&mut ctx);
                self.net_rollback = Some(d);
                at
            }
            (d, _) => {
                self.net_rollback = d;
                None
            }
        };
        match stepped {
            Some(t) => {
                self.game_tick_no = self.game_tick_no.saturating_sub(1);
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    format!("⏮ stepped back to rollback tick {t}"),
                    None,
                );
            }
            None => self.console.push(
                floptle_script::LogLevel::Warn,
                "⏮ nothing to step back to. Backwards frame-step reads the ROLLBACK state \
                 ring, so it needs a rollback session running, and it only reaches as far \
                 back as the ring keeps (about a fifth of a second)."
                    .into(),
                None,
            ),
        }
    }

    /// Resolve a `scene.load(...)` argument to a scene file: a name ("arena"),
    /// a scenes-relative name ("arenas/desert"), or a project-relative path
    /// ("scenes/arena.ron"). Escapes are REJECTED — in multiplayer the string
    /// arrives over the wire, so it must never reach outside the project.
    pub(crate) fn resolve_scene_request(&self, req: &str) -> Option<std::path::PathBuf> {
        let r = req.trim().replace('\\', "/");
        if r.is_empty() || r.contains("..") || r.starts_with('/') || r.contains(':') {
            return None;
        }
        let with_ext = if r.ends_with(".ron") { r.clone() } else { format!("{r}.ron") };
        [with_ext.clone(), format!("scenes/{with_ext}")]
            .into_iter()
            .map(|c| self.project_root.join(c))
            .find(|p| p.is_file())
    }

    /// Perform a scene transition while Play runs: swap the world to the new
    /// scene and rebuild every play-session runtime (scripts, physics, anim,
    /// vfx, audio) against it — `start` re-fires everywhere, exactly like the
    /// scene booting fresh. The editor's own scene (play snapshot + name) is
    /// untouched: Stop still restores exactly what you were editing. Returns
    /// the new scene's project-relative path (what a server announces).
    ///
    /// Session roles (filters, prediction, NetId rebinds) are the CALLER's job
    /// — see [`Self::perform_scene_request`].
    pub(crate) fn switch_scene_during_play(&mut self, req: &str) -> Option<String> {
        let Some(path) = self.resolve_scene_request(req) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("scene.load(\"{req}\"): no such scene (looked in scenes/)"),
                None,
            );
            return None;
        };
        let doc = match floptle_scene::load(&path) {
            Ok(d) => d,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("scene.load(\"{req}\"): {e}"),
                    None,
                );
                return None;
            }
        };
        // WHAT SURVIVES. `node.persistent` marks a subtree as outliving the
        // swap — a HUD, a party, a save-game manager, the music. Collected
        // before anything is torn down, because the answer is about the world
        // that is still standing.
        let keepers: Vec<floptle_core::Entity> = self
            .world
            .query::<floptle_core::Matter>()
            .map(|(e, _)| e)
            .filter(|&e| floptle_core::is_persistent(&self.world, e))
            .collect();
        let keep_ids: std::collections::HashSet<u32> =
            keepers.iter().map(|e| e.index()).collect();
        // Tear down the old scene's play runtimes…
        self.reset_anim_bindings();
        self.anim.clear_instances();
        self.vfx.clear_instances();
        self.script_host.clear_anim_state();
        self.script_host.reset_instances_keeping(&keep_ids);
        // A scatter source names a region of the world that is about to stop
        // existing, and its resolved chunks were dropped onto ground that is
        // about to be replaced.
        self.script_host.clear_scatter();
        self.scatter_cache.clear();
        self.script_gizmos.clear();
        let mixer = self.project.mixer.clone();
        self.audio.stop_play(&mixer);
        // …swap the world…
        //
        // DESPAWN IN PLACE rather than `World::new()`, so a persistent node
        // keeps its ENTITY. That is not a micro-optimisation — script
        // instances, UI bindings and net handlers are all keyed by entity
        // index, and a survivor that came back under a different index would
        // have to be rebuilt, which is exactly what "persistent" promises it
        // won't be. The ECS hands out freed indices from a free list, so the
        // incoming scene cannot be given an index a survivor is still holding.
        let doomed: Vec<floptle_core::Entity> = {
            let alive: Vec<floptle_core::Entity> = self
                .world
                .query::<floptle_core::transform::Transform>()
                .map(|(e, _)| e)
                .collect();
            alive.into_iter().filter(|e| !keep_ids.contains(&e.index())).collect()
        };
        for e in doomed {
            self.world.despawn(e);
        }
        // A survivor parented to a node that did NOT survive is now a child of
        // nothing, and `world_transform` would fold in a transform that no
        // longer exists. Re-root it: it keeps the world pose it had, which is
        // where the player last saw it.
        for &e in &keepers {
            let Some(floptle_core::Parent(p)) = self.world.get::<floptle_core::Parent>(e).copied()
            else {
                continue;
            };
            if self.world.is_alive(p) {
                continue;
            }
            let world_pose = floptle_core::world_transform(&self.world, e);
            self.world.remove::<floptle_core::Parent>(e);
            self.world.insert(e, world_pose);
        }
        floptle_scene::spawn_into(&doc, &mut self.world);
        // The incoming scene brings its own Lighting/Skybox/PostProcess. If a
        // survivor carried one too, the world now has two and query order picks
        // the winner — drop the survivor's, since the scene you just loaded is
        // the one whose environment you meant.
        if !keepers.is_empty() {
            self.drop_duplicate_scene_singletons(&keep_ids);
        }
        // Every additive layer went with the old world, so any environment loan
        // is void — and its entity ids name a world that no longer exists. The
        // scene just loaded owns its environment outright.
        self.env_layer = None;
        self.set_scene_file(&path);
        self.adopt_terrain();
        // THE OUT-OF-DOCUMENT STORES, which a scene switch has to reload exactly as
        // opening a scene does.
        //
        // Map geometry, vertex paint and texture paint live in sidecars keyed by SCENE
        // NAME, not in the scene .ron. `set_scene_file` above just repointed every one of
        // those paths at the new scene — but until these run, the in-memory stores still
        // hold the previous scene's contents, and map node ids start at 0 in every scene,
        // so they collide rather than come up empty. A node whose id survives draws the
        // *other* scene's geometry under the *other* scene's slot names; a node whose id
        // doesn't gets seeded with a default box. Either way the per-face materials in the
        // scene .ron are keyed by slot NAME and match nothing, so the map renders grey.
        // Colliders are baked from the same store, so the collision follows the picture.
        //
        // This was missing here and present in all five editor entry points, which is why
        // a map looks right when you open its scene and wrong when a game loads it.
        //
        // Maps FIRST: paint is keyed to a triangulation that comes out of the map store.
        self.adopt_maps();
        self.adopt_paint();
        self.adopt_tex_paint();
        // `adopt_maps` frees its own GPU parts, but these two caches are keyed by mesh id
        // and would hand the new scene the old one's CPU geometry to paint and to wire.
        self.paint_meshes.clear();
        self.mesh_wire_cache.clear();
        self.register_scene_meshes();
        self.selection.clear();
        self.grabbed = None;
        self.drag = None;
        // …and rebuild the play session against it (the same steps as Play).
        let sim = self.build_play_sim();
        self.sim = Some(sim);
        self.vfx.start_play(&self.world);
        let root = self.project_root.clone();
        self.audio.start_play(&self.world, &root, &mixer);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("⏵ scene → {}", self.scene_name),
            None,
        );
        Some(self.scene_rel_or_default())
    }

    /// After a swap that carried persistent nodes across: if a survivor brought
    /// a scene singleton (lighting, skybox, post-processing) and the incoming
    /// scene supplied its own, drop the survivor's copy.
    ///
    /// The rule is "the scene you loaded owns the environment". Two Lighting
    /// nodes is not an error the engine can resolve sensibly — whichever the
    /// query reaches first wins, which reads as a scene whose lighting depends
    /// on load order.
    fn drop_duplicate_scene_singletons(&mut self, kept: &std::collections::HashSet<u32>) {
        let lights: Vec<floptle_core::Entity> =
            self.world.query::<floptle_core::Light>().map(|(e, _)| e).collect();
        if lights.len() > 1 {
            for e in lights.into_iter().filter(|e| kept.contains(&e.index())) {
                self.world.despawn(e);
            }
        }
        for want_sky in [true, false] {
            let found: Vec<floptle_core::Entity> = self
                .world
                .query::<floptle_core::Matter>()
                .filter(|(_, m)| {
                    if want_sky {
                        matches!(m, floptle_core::Matter::Skybox { .. })
                    } else {
                        matches!(m, floptle_core::Matter::PostProcess { .. })
                    }
                })
                .map(|(e, _)| e)
                .collect();
            if found.len() > 1 {
                for e in found.into_iter().filter(|e| kept.contains(&e.index())) {
                    self.world.despawn(e);
                }
            }
        }
    }

    /// `scene.load(name, { additive = true })` — layer a scene on top of the
    /// running one instead of replacing it.
    ///
    /// Nothing is torn down: no script restarts, no physics rebuild from
    /// scratch, no audio stop. The new nodes are spawned into the live world,
    /// tagged with the scene they came from, and wired into the running sim the
    /// same way a `spawn(...)`ed prefab is — which is what makes this cheap
    /// enough to use for streaming a level in pieces.
    pub(crate) fn perform_scene_additive(&mut self, req: &str, environment: bool) {
        let Some(path) = self.resolve_scene_request(req) else {
            self.console.push(
                floptle_script::LogLevel::Error,
                format!("scene.load(\"{req}\", {{additive = true}}): no such scene (looked in scenes/)"),
                None,
            );
            return;
        };
        let doc = match floptle_scene::load(&path) {
            Ok(d) => d,
            Err(e) => {
                self.console.push(
                    floptle_script::LogLevel::Error,
                    format!("scene.load(\"{req}\", {{additive = true}}): {e}"),
                    None,
                );
                return;
            }
        };
        // The TAG is the request string as written, so `scene.unload` takes the
        // same name back. Loading the same scene twice is allowed and both
        // copies carry the tag — `unload` then removes both, which is the only
        // answer that isn't arbitrary.
        let tag = req.trim().to_string();
        // The handover happens BEFORE the layer's own nodes exist, so the
        // environment being put to sleep is exactly the base scene's.
        if environment {
            self.take_environment(&doc, &tag);
        }
        let ents = floptle_scene::spawn_additive(&doc, &mut self.world, &tag);
        if ents.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("scene.load(\"{req}\", {{additive = true}}): the scene has no nodes"),
                None,
            );
            return;
        }
        // Meshes need GPU parts before they can draw; map/paint sidecars are
        // keyed by SCENE NAME and belong to the base scene, so an additive
        // layer deliberately does not touch them.
        self.register_scene_meshes();
        // Physics: the same incremental wiring a spawned prefab gets. Bodies
        // first, then compounds — `add_body_for` refuses an assembly's parts,
        // and `add_compound_for` claims the whole hierarchy at its root.
        if let Some(sim) = self.sim.as_mut() {
            for &e in &ents {
                sim.add_body_for(e, &self.world);
            }
            for &e in &ents {
                sim.add_compound_for(e, &self.world);
            }
        }
        // Static colliders and gravity sources are built wholesale from the
        // world, so they need the rebuild — which preserves live velocities and
        // compound state (see `rebuild_sim`), so nothing in flight is disturbed.
        if ents.iter().any(|&e| {
            self.world.get::<floptle_core::Collidable>(e).is_some()
                || self.world.get::<floptle_core::MeshCollider>(e).is_some()
                || matches!(
                    self.world.get::<floptle_core::Matter>(e),
                    Some(floptle_core::Matter::GravityVolume { .. })
                )
        }) {
            self.rebuild_sim();
        }
        // Audio sources in the layer start now; the running scene's voices are
        // untouched (`start_play` is additive over live voices).
        let mixer = self.project.mixer.clone();
        let root = self.project_root.clone();
        self.audio.start_play(&self.world, &root, &mixer);
        self.vfx.start_play(&self.world);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("⏵ scene + {tag} ({} nodes)", ents.len()),
            None,
        );
        // The layer's own scripts have not run yet — they start on the next
        // frame's pass, like any node that appears mid-play. `onLoaded` fires
        // here anyway: the nodes exist, which is what the caller asked about.
        self.script_host.fire_scene_loaded(&mut self.world, &tag, true);
    }

    /// Hand the world's environment to an additive layer
    /// (`{ additive = true, environment = true }`).
    ///
    /// A world has ONE environment. Without this, a layer carrying a Skybox is
    /// a second Skybox, and the renderer resolves both with a first-match query
    /// (`shading::skybox_uniforms`) — so the look would be decided by spawn
    /// order, which is the "the additive scene broke my lighting" failure the
    /// nodes-only rule exists to prevent. This makes the handover EXPLICIT
    /// instead: the base scene's environment steps aside, the layer's takes
    /// over, and `scene.unload` puts the first one back.
    ///
    /// Its nodes are DISABLED rather than despawned, because a node that comes
    /// back is a node whose authored values were never lost — and because the
    /// base scene is not reloadable from disk mid-session without also undoing
    /// everything else Play has done to it.
    fn take_environment(&mut self, doc: &floptle_scene::SceneDoc, tag: &str) {
        // A second environment layer over a first: the base's nodes are already
        // asleep and its Light is already saved, so the loan must NOT be
        // re-taken from the world (that would record the outgoing layer's
        // environment as the base's, and unloading would restore the wrong
        // one). Only the owning tag moves.
        let first = self.env_layer.is_none();
        let slept: Vec<floptle_core::Entity> = if first {
            let sleepers: Vec<floptle_core::Entity> = self
                .world
                .query::<floptle_core::Matter>()
                .filter(|(e, m)| {
                    matches!(
                        m,
                        floptle_core::Matter::Skybox { .. }
                            | floptle_core::Matter::PostProcess { .. }
                    ) && self.world.get::<floptle_core::Disabled>(*e).is_none()
                })
                .map(|(e, _)| e)
                .collect();
            for &e in &sleepers {
                self.world.insert(e, floptle_core::Disabled);
            }
            sleepers
        } else {
            // Keep the ORIGINAL loan; only its owner changes.
            self.env_layer.as_ref().map(|(_, s, _)| s.clone()).unwrap_or_default()
        };
        let base_light = if first {
            self.world.query::<floptle_core::Light>().map(|(_, l)| *l).next()
        } else {
            self.env_layer.as_ref().map(|(_, _, l)| *l)
        };
        // The scene-level block is the half an additive load cannot bring on
        // its own: `spawn_additive` spawns nodes, and the sun + all of the fog
        // live beside them rather than in one.
        let lights: Vec<floptle_core::Entity> =
            self.world.query::<floptle_core::Light>().map(|(e, _)| e).collect();
        let incoming = doc.lighting.to_light();
        for e in lights {
            self.world.insert(e, incoming);
        }
        self.env_layer = Some((tag.to_string(), slept, base_light.unwrap_or(incoming)));
    }

    /// Give the base scene's environment back when the layer holding it leaves.
    ///
    /// The layer's own Skybox/PostProcess nodes are already gone with it (they
    /// carried its `SceneTag`), so only the sleepers have to wake.
    fn return_environment(&mut self) {
        let Some((_, slept, light)) = self.env_layer.take() else { return };
        for e in slept {
            // A node the layer's own lifetime outlived — the world may have
            // despawned it since — is simply not there to wake.
            if self.world.is_alive(e) {
                self.world.remove::<floptle_core::Disabled>(e);
            }
        }
        let lights: Vec<floptle_core::Entity> =
            self.world.query::<floptle_core::Light>().map(|(e, _)| e).collect();
        for e in lights {
            self.world.insert(e, light);
        }
    }

    /// `scene.unload(name)` — remove an additively-loaded layer.
    ///
    /// Only nodes an additive load tagged are candidates: the scene you opened
    /// can never be unloaded out from under you, and a node the game spawned
    /// itself is the game's to destroy.
    pub(crate) fn perform_scene_unload(&mut self, req: &str) {
        let tag = req.trim().to_string();
        let doomed: Vec<floptle_core::Entity> = self
            .world
            .query::<floptle_core::SceneTag>()
            .filter(|(_, t)| t.0 == tag)
            .map(|(e, _)| e)
            .collect();
        if doomed.is_empty() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                format!("scene.unload(\"{req}\"): no additively-loaded scene by that name"),
                None,
            );
            return;
        }
        let n = floptle_scene::despawn_tagged(&mut self.world, &tag);
        // If this layer held the environment, the base scene's comes back —
        // after the despawn, so the nodes waking up are the only ones left.
        if self.env_layer.as_ref().is_some_and(|(t, _, _)| *t == tag) {
            self.return_environment();
        }
        // Everything keyed by entity has to let go: physics bodies, audio
        // voices, effects, and the scripts that were running on them.
        self.rebuild_sim();
        let mixer = self.project.mixer.clone();
        self.audio.stop_play(&mixer);
        let root = self.project_root.clone();
        self.audio.start_play(&self.world, &root, &mixer);
        self.vfx.clear_instances();
        self.vfx.start_play(&self.world);
        self.reset_anim_bindings();
        self.paint_meshes.clear();
        self.mesh_wire_cache.clear();
        self.register_scene_meshes();
        self.selection.retain(|e| self.world.is_alive(*e));
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("⏵ scene − {tag} ({n} nodes)"),
            None,
        );
    }

    /// A script's declared `defaults`, cached by file mtime so we only re-parse the Lua
    /// when the file actually changes (keeps the per-frame inspector sync cheap).
    /// Returns `(numeric params, node-ref param names)`.
    pub(crate) fn cached_script_defaults(
        &mut self,
        name: &str,
    ) -> crate::ScriptDefaults {
        let path = self.project_root.join("scripts").join(format!("{name}.lua"));
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let key = name.to_string();
        if let (Some(mt), Some((cached_mt, vals))) = (mtime, self.script_defaults_cache.get(&key))
            && *cached_mt == mt {
                return vals.clone();
            }
        let vals = self.script_host.script_defaults(&path);
        if let Some(mt) = mtime {
            self.script_defaults_cache.insert(key, (mt, vals.clone()));
        }
        vals
    }

    /// Keep the selected node's script `params` in step with each script's current
    /// `defaults`, so editing a script (adding/removing/renaming a `defaults` key)
    /// is reflected live in the Inspector: new defaults appear as tweakable params,
    /// keys removed from `defaults` drop off, and the user's overridden values for
    /// keys that still exist are preserved. Display-only (the runtime already merges
    /// defaults at call time) and not recorded as an undo step.
    pub(crate) fn sync_selected_script_params(&mut self) {
        let Some(e) = self.selection.last().copied() else { return };
        let names: Vec<String> = match self.world.get::<Scripts>(e) {
            Some(s) => s.0.iter().map(|i| i.kind.clone()).collect(),
            None => return,
        };
        // Resolve each script's current defaults first (needs &mut self for the cache).
        let defaults: Vec<crate::ScriptDefaults> =
            names.iter().map(|n| self.cached_script_defaults(n)).collect();
        // Refresh the Inspector's ref-kind map for this selection.
        self.ref_kinds.clear();
        for (name, (_, refs, _)) in names.iter().zip(&defaults) {
            for (param, kind) in refs {
                self.ref_kinds.insert((name.clone(), param.clone()), kind.clone());
            }
        }
        let Some(scr) = self.world.get_mut::<Scripts>(e) else { return };
        for (inst, (defs, ref_decls, str_decls)) in scr.0.iter_mut().zip(defaults) {
            // An empty result means "no defaults declared" OR a transient parse error
            // (e.g. mid-edit) — never wipe the user's overrides in that case.
            if defs.is_empty() && ref_decls.is_empty() && str_decls.is_empty() {
                continue;
            }
            // Drop params no longer declared in defaults.
            inst.params.retain(|(k, _)| defs.iter().any(|(dk, _)| dk == k));
            // Add any newly-declared defaults (preserving the order defaults come in).
            for (dk, dv) in &defs {
                if !inst.params.iter().any(|(k, _)| k == dk) {
                    inst.params.push((dk.clone(), *dv));
                }
            }
            // Same for reference params (wired targets survive; stale keys drop).
            inst.refs.retain(|(k, _)| ref_decls.iter().any(|(rk, _)| rk == k));
            for (rk, _) in &ref_decls {
                if !inst.refs.iter().any(|(k, _)| k == rk) {
                    inst.refs.push((rk.clone(), String::new()));
                }
            }
            // Same for string params (overridden text survives; stale keys drop).
            inst.strs.retain(|(k, _)| str_decls.iter().any(|(sk, _)| sk == k));
            for (sk, sv) in &str_decls {
                if !inst.strs.iter().any(|(k, _)| k == sk) {
                    inst.strs.push((sk.clone(), sv.clone()));
                }
            }
        }
    }

    /// Attach the `.lua` script at `path` to `target`, seeding its `params` from
    /// the script's declared `defaults`.
    pub(crate) fn attach_script_file(&mut self, path: &str, target: Option<Entity>) {
        let Some(e) = target else { return };
        if self.world.get::<Transform>(e).is_none() || !is_script(path) {
            return;
        }
        if !Path::new(path).exists() {
            eprintln!("  script not found: {path}");
            return;
        }
        let kind = script_kind_of(path, &self.scripts_dir());
        let (params, ref_decls, strs) = self.script_host.script_defaults(Path::new(path));
        self.record();
        let refs: Vec<(String, String)> =
            ref_decls.into_iter().map(|(k, _)| (k, String::new())).collect();
        // Attaching to a multi-selection attaches to all of it — twenty enemies
        // get their behaviour in one drag.
        let group = self.selected_group(e);
        // Across a group, a node already running this script is left alone
        // rather than running it twice; on a single node two instances of one
        // script with different params stays a legitimate thing to build.
        let skip_dupes = group.len() > 1;
        for e in group {
            if self.world.get::<Transform>(e).is_none() {
                continue;
            }
            let inst = ScriptInst {
                kind: kind.clone(),
                enabled: true,
                params: params.clone(),
                refs: refs.clone(),
                strs: strs.clone(),
            };
            if let Some(scr) = self.world.get_mut::<Scripts>(e) {
                if skip_dupes && scr.0.iter().any(|s| s.kind == inst.kind) {
                    continue;
                }
                scr.0.push(inst);
            } else {
                self.world.insert(e, Scripts(vec![inst]));
            }
        }
    }
}

#[cfg(test)]
mod scene_request_tests {
    use crate::Editor;
    use super::{Transform, Scripts, ScriptInst};

    /// `scene.load` strings resolve inside the project only: names,
    /// scenes-relative paths, and project-relative paths all work; escapes
    /// never do — in multiplayer the string arrives over the WIRE, so it must
    /// not be able to name anything outside the project.
    #[test]
    fn scene_requests_resolve_safely() {
        let root =
            std::env::temp_dir().join(format!("floptle-scene-req-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("scenes/arenas")).unwrap();
        std::fs::write(root.join("scenes/first.ron"), "()").unwrap();
        std::fs::write(root.join("scenes/arenas/desert.ron"), "()").unwrap();
        let ed = Editor { project_root: root.clone(), ..Default::default() };

        let first = root.join("scenes/first.ron");
        assert_eq!(ed.resolve_scene_request("first").as_deref(), Some(first.as_path()));
        assert_eq!(ed.resolve_scene_request("first.ron").as_deref(), Some(first.as_path()));
        assert_eq!(ed.resolve_scene_request("scenes/first.ron").as_deref(), Some(first.as_path()));
        let desert = root.join("scenes/arenas/desert.ron");
        assert_eq!(ed.resolve_scene_request("arenas/desert").as_deref(), Some(desert.as_path()));

        assert!(ed.resolve_scene_request("nope").is_none(), "missing scenes are None");
        assert!(ed.resolve_scene_request("../first").is_none(), "no escaping the project");
        assert!(ed.resolve_scene_request("/etc/passwd").is_none(), "no absolute paths");
        assert!(ed.resolve_scene_request("C:\\x").is_none(), "no Windows drives");
        assert!(ed.resolve_scene_request("").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `floptle/0159`: the docs promise `find("Lighting")` always resolves —
    /// "every scene has exactly one Lighting node and the loader makes it". It
    /// didn't, because `spawn_into` never gave the Lighting entity a
    /// `Transform`, and the script mirror (`sync_scene`) only mirrors entities
    /// it can find one on. A real scene, a real script, a real `find()` call.
    #[test]
    fn find_lighting_is_reachable_from_a_game_script() {
        let dir = std::env::temp_dir()
            .join(format!("floptle-find-lighting-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::write(
            dir.join("scripts/probe.lua"),
            "function update(node, dt)\n\
             \x20 local l = find(\"Lighting\")\n\
             \x20 if l then log(\"FOUND \" .. tostring(l:getcomponent(\"Light\") ~= nil)) else log(\"NIL\") end\n\
             end\n",
        )
        .unwrap();

        let doc = floptle_scene::SceneDoc {
            name: "test".into(),
            lighting: floptle_scene::LightDoc::default(),
            nodes: vec![],
        };
        let mut world = floptle_core::World::default();
        floptle_scene::spawn_into(&doc, &mut world);

        let probe = world.spawn();
        world.insert(probe, Transform::IDENTITY);
        world.insert(probe, Scripts(vec![ScriptInst::new("probe")]));

        let mut host = floptle_script::ScriptHost::new();
        host.run(&mut world, &dir.join("scripts"), 1.0 / 60.0, 0.0);

        let logs = host.drain_logs();
        let msgs: Vec<&str> = logs.iter().map(|l| l.msg.as_str()).collect();
        assert!(
            msgs.iter().any(|m| m.starts_with("FOUND true")),
            "find(\"Lighting\") should resolve a handle with a Light component; got {msgs:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod water_streaming_tests {
    use crate::Editor;
    use floptle_core::{GravityMode, Matter, Name, RigidBody, WaterKind};
    use super::{Transform};

    /// `floptle/0141`: `build_water_field` used to run once, at Play start —
    /// exactly like `build_gravity_field`, except gravity is rebuilt every
    /// frame and water was not. A pool spawned while the game is already
    /// running (a streamed level's ordinary case) was drawn — `water_draw`
    /// gathers from the live world every frame — but never entered the
    /// solver's field, so it floated nothing.
    ///
    /// Falling body, no water yet, no water for the first several ticks —
    /// then a `WaterVolume` node is spawned into the live world exactly the
    /// way a script's `scene.add`/streamer would, and the body must be
    /// caught by it the same tick class water always has been.
    #[test]
    fn a_pool_spawned_mid_session_is_in_the_sim_the_frame_it_exists() {
        let mut ed = Editor::default();

        // Down gravity, or nothing falls at all.
        let g = ed.world.spawn();
        ed.world.insert(g, Name("Gravity".into()));
        ed.world.insert(g, Transform::IDENTITY);
        ed.world.insert(g, Matter::GravityVolume { mode: GravityMode::Down, strength: 15.0, radius: 0.0 });

        // A dynamic sphere, falling from height with nothing below it — no
        // floor, no water, so left alone it falls forever.
        let ball = ed.world.spawn();
        ed.world.insert(ball, Name("Ball".into()));
        ed.world.insert(ball, Transform { translation: [0.0, 10.0, 0.0].into(), ..Transform::IDENTITY });
        ed.world.insert(ball, RigidBody::default());

        ed.toggle_play();
        assert!(ed.playing, "the session must actually start");

        const DT: f32 = 1.0 / 60.0;
        let pos_of = |ed: &Editor| -> f64 {
            ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == ball).unwrap().pos.y
        };
        let vel_of = |ed: &Editor| -> f32 {
            ed.sim.as_ref().unwrap().body_states().find(|r| r.entity == ball).unwrap().vel.y
        };

        // A few ticks of ordinary free fall — no water exists yet.
        for _ in 0..15 {
            ed.play_step(DT, true);
        }
        let falling_at = pos_of(&ed);
        assert!(vel_of(&ed) < -1.0, "the ball should be falling under gravity before any water exists");
        assert!(falling_at < 10.0, "it should have actually fallen");

        // Spawn the pool now, mid-session — the way a streamer or a script's
        // scene.add would, not through Play start. Centred where the ball
        // already is, tall enough to still be under it.
        let pool = ed.world.spawn();
        ed.world.insert(pool, Name("Pool".into()));
        ed.world.insert(
            pool,
            Transform { translation: [0.0, falling_at - 1.0, 0.0].into(), ..Transform::IDENTITY },
        );
        ed.world.insert(
            pool,
            Matter::WaterVolume {
                kind: WaterKind::Pool,
                radius: 0.0,
                half_extents: [5.0, 5.0, 5.0],
                density: 1000.0,
                drag: 1.0,
                angular_drag: 1.0,
                frozen: false,
                tint: [0.1, 0.3, 0.4],
                visibility: 20.0,
            },
        );

        // ONE tick is the actual claim: the pool must be in the solver's
        // field the same frame it exists in the world, not next Play. Water
        // this dense (1000, real-water-like, against a 1 kg 0.5 m sphere)
        // flips the ball's velocity from falling to sharply buoyant
        // immediately — the clean, unambiguous signal that the field is
        // being read at all, before anything has had time to rise out the
        // top of the pool and start free-falling again.
        ed.play_step(DT, true);
        assert_eq!(
            ed.sim.as_ref().unwrap().world.water.volumes.len(),
            1,
            "the pool must be in the solver's water field the very next tick"
        );
        assert!(
            vel_of(&ed) > 0.0,
            "a body inside a pool this dense must be pushed UP by buoyancy on the very next \
             tick — instead it kept falling at {} — the pool never reached the solver",
            vel_of(&ed)
        );

        // …and it keeps floating rather than sinking through to wherever it
        // would have landed in empty space — the property that matters to a
        // player, not just to the solver's bookkeeping.
        for _ in 0..10 {
            ed.play_step(DT, true);
        }
        assert!(
            pos_of(&ed) > falling_at - 2.0,
            "the ball should be held up near the pool, not have fallen through it: {}",
            pos_of(&ed)
        );
    }
}
