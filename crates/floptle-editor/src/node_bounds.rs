//! How big a node's geometry is, so the draw loop can skip what is off screen
//! (`floptle/0075`).
//!
//! One function, [`local_radius`], answering per `Matter` kind. It returns
//! `Option<f32>`: `None` means **do not cull this**, and that is the answer for
//! everything whose extent is not knowable from the scene alone. A node that
//! cannot be measured must draw, because the cost of a wasted instance is a few
//! microseconds and the cost of a wrong cull is geometry popping in and out at
//! the screen edge.
//!
//! Radii are LOCAL — before the node's scale, which
//! [`floptle_render::cull`] applies. Everything is derived from a real
//! measurement with the source named; nothing here is a guess.

use floptle_core::Matter;

/// Half the diagonal of the built-in cube, which is the largest of the four
/// primitives — so one number covers all of them, conservatively.
///
/// `PRIMITIVE_HALF` is 0.7 on each axis, giving a corner at `0.7·√3 ≈ 1.212`.
/// The sphere (0.85), the capsule (`√(0.5² + 1.0²) ≈ 1.118`) and the plane
/// (`0.7·√2 ≈ 0.990`) all fit inside that, so using the cube's figure for every
/// shape is safe and means this list cannot fall out of step with
/// `matter_catalog::primitive_mesh` one shape at a time.
const PRIMITIVE_RADIUS: f32 = crate::matter_catalog::PRIMITIVE_HALF * 1.732_050_8;

/// What the last gather submitted, and what it skipped.
///
/// Counts, not times. `floptle/0071` — a scatter field asking for 117,000 props,
/// reported as "currently unplayable" — was diagnosable from a count alone, and
/// the engine kept none. `floptle/0077` extends this with per-subsystem times and
/// a Lua surface; these are the numbers frustum culling moves, so they start here.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Nodes the gather walked.
    pub(crate) nodes: usize,
    /// …of which were rejected as off screen before any work was done for them.
    pub(crate) culled: usize,
    /// Raster instances submitted, scatter and terrain included.
    pub(crate) instances: usize,
}

/// What the draw loop already knows about a node, gathered so this stays pure.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Measured {
    /// `MeshAsset::size` for a `Mesh`/`MapMesh` — the longest edge of the
    /// model's AABB, measured at import after recentering. `None` if the asset
    /// is not registered (the arm draws nothing then anyway).
    pub(crate) model_size: Option<f32>,
    /// The furthest a sprite in this frame's batch reaches from the batch node's
    /// origin, in local units, including its own scale. `None` on a batch that
    /// drew nothing.
    pub(crate) sprite_reach: Option<f32>,
}

/// The local bounding radius of a node's drawn geometry, or `None` for "always
/// submit".
pub(crate) fn local_radius(matter: &Matter, m: Measured) -> Option<f32> {
    match matter {
        Matter::Primitive { .. } => Some(PRIMITIVE_RADIUS),
        // The mesh path and the map path both go through the registry, so both
        // get the import-measured size — the longest edge of the model's AABB,
        // which converts to a radius that reaches its corners.
        Matter::Mesh { .. } | Matter::MapMesh { .. } => m.model_size.map(|s| {
            floptle_render::cull::radius_from_longest_edge(s, floptle_core::math::Vec3::ONE)
        }),
        // A tilemap's grid is `cols × rows` squares of `tile` units. Using the
        // FULL extent as if it were the half-extent is twice as loose as it
        // needs to be, and deliberately so: where the generated mesh puts its
        // origin is a detail of the mesh builder, and a cull that silently
        // depends on that would break the day somebody recentres it.
        Matter::Tilemap { cols, rows, tile, .. } => {
            let (w, h) = (*cols as f32 * *tile, *rows as f32 * *tile);
            (w.is_finite() && h.is_finite()).then(|| (w * w + h * h).sqrt())
        }
        // Sprites live at arbitrary positions in the batch node's local space,
        // so the batch's own `size` says nothing about where they are. The
        // reach is measured from this frame's actual draws, which is the only
        // honest answer — and it is immediate-mode, so it is already to hand.
        Matter::SpriteBatch { .. } => m.sprite_reach,
        // A sea is a sphere of `radius`; a pool is a box of `half_extents`.
        Matter::WaterVolume { kind, radius, half_extents, .. } => match kind {
            floptle_core::WaterKind::Sea => Some(*radius),
            floptle_core::WaterKind::Pool => {
                let h = floptle_core::math::Vec3::from(*half_extents);
                Some(h.length())
            }
        },
        // A Blob is not an instance at all — it becomes an SDF primitive in the
        // raymarch, where it also feeds shadows and AO for things that ARE on
        // screen. Culling it by the camera frustum would delete shadows cast
        // from off screen.
        Matter::Blob { .. } => None,
        // These draw nothing here (they are cameras, lights, terrain, sky,
        // post) — the match arm below is a no-op either way, so there is
        // nothing to gain by measuring them.
        Matter::Empty
        | Matter::Terrain { .. }
        | Matter::Camera { .. }
        | Matter::PointLight { .. }
        | Matter::GravityVolume { .. }
        | Matter::FieldShape { .. }
        | Matter::LightProbes { .. }
        | Matter::NavMesh { .. }
        | Matter::NavLink { .. }
        | Matter::NavArea { .. }
        | Matter::ReflectionProbe { .. }
        | Matter::Skybox { .. }
        | Matter::PostProcess { .. } => None,
    }
}

/// The furthest any sprite in `sprites` reaches from the batch origin, given the
/// batch's world-unit `size`.
///
/// A sprite is a quad of `size · scale` centred on `pos`, rolled about its own
/// centre — and a rotated square's corner is at `√2/2` of its edge, so the roll
/// is covered by taking the diagonal rather than the edge.
pub(crate) fn sprite_reach(sprites: &[floptle_core::Sprite], size: f32) -> f32 {
    let mut reach = 0.0f32;
    for s in sprites {
        let c = floptle_core::math::Vec3::from(s.pos).length();
        let half = size * 0.5 * s.scale[0].abs().max(s.scale[1].abs());
        reach = reach.max(c + half * std::f32::consts::SQRT_2);
    }
    reach
}

/// Is this node's geometry certainly outside `frustum`?
///
/// The one rejection test, shared by the main gather and the offscreen one, so a
/// camera rendering into a texture culls exactly the way the screen does — an
/// offscreen target that culled differently would be a mirror showing a
/// different room.
///
/// `false` whenever the answer is not certain: an unmeasurable node, a blob,
/// anything drawing through the raymarch. Wasting an instance is cheap; dropping
/// something visible is a pop.
///
/// Takes its fields individually rather than `&self` because both call sites run
/// inside the frame's GPU borrow, where the whole `Editor` is not available.
#[allow(clippy::too_many_arguments)]
pub(crate) fn node_is_off_screen(
    world: &floptle_core::World,
    mesh_registry: &std::collections::HashMap<String, crate::MeshAsset>,
    poses: &std::collections::HashMap<floptle_core::Entity, Vec<floptle_core::math::Mat4>>,
    e: floptle_core::Entity,
    matter: &Matter,
    t: &floptle_core::transform::Transform,
    cam_world: floptle_core::math::DVec3,
    frustum: &floptle_render::Frustum,
) -> bool {
    let measured = Measured {
        model_size: match matter {
            Matter::Mesh { asset_path } => mesh_registry.get(asset_path).map(|a| a.size),
            Matter::MapMesh { id } => {
                mesh_registry.get(&crate::map_edit::map_key(*id)).map(|a| a.size)
            }
            _ => None,
        },
        sprite_reach: match matter {
            Matter::SpriteBatch { size } => world
                .get::<floptle_core::Sprites>(e)
                .filter(|s| !s.0.is_empty())
                .map(|s| sprite_reach(&s.0, *size)),
            _ => None,
        },
    };
    let Some(local_r) = local_radius(matter, measured) else { return false };
    let mut r = floptle_render::cull::scale_radius(local_r, t.scale);
    // A bind-pose sphere is wrong the moment a clip reaches outside it, and the
    // symptom is a character vanishing as it swings a weapon near the screen
    // edge. So this frame's pose grows the radius.
    if let Some(pose) = poses.get(&e) {
        r = floptle_render::cull::inflate_for_pose(r, pose);
    }
    !frustum.contains_sphere((t.translation - cam_world).as_vec3(), r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::math::Vec3;
    use floptle_core::{Shape, Sprite, WaterKind};

    /// The one primitive radius covers every built-in shape, including the
    /// corners of the cube and the ends of the capsule.
    ///
    /// The four are separate meshes with separate parameters, and a per-shape
    /// table would be one more place to forget a shape. So this pins that the
    /// single figure is genuinely conservative for all four, against the same
    /// constants `primitive_mesh` builds them from.
    #[test]
    fn the_primitive_radius_contains_all_four_shapes() {
        let cube_corner = Vec3::splat(crate::matter_catalog::PRIMITIVE_HALF).length();
        let sphere = 0.85;
        let capsule = Vec3::new(0.5, 1.0, 0.0).length(); // radius 0.5, half-height 0.5
        let plane_corner = Vec3::new(0.7, 0.7, 0.0).length();
        for (name, r) in [
            ("cube", cube_corner),
            ("sphere", sphere),
            ("capsule", capsule),
            ("plane", plane_corner),
        ] {
            assert!(
                PRIMITIVE_RADIUS >= r - 1e-5,
                "{name} reaches {r} but the cull radius is only {PRIMITIVE_RADIUS}"
            );
        }
        // …and not absurdly loose, or the cull stops paying for itself.
        assert!(PRIMITIVE_RADIUS < cube_corner * 1.01);
    }

    /// A shape whose size the scene does not know is never culled.
    #[test]
    fn an_unmeasured_node_is_always_submitted() {
        // A mesh whose asset is not in the registry.
        assert_eq!(
            local_radius(&Matter::Mesh { asset_path: "gone.glb".into() }, Measured::default()),
            None
        );
        // A batch that drew nothing this frame.
        assert_eq!(local_radius(&Matter::SpriteBatch { size: 1.0 }, Measured::default()), None);
        // A blob, which is an SDF primitive and casts shadows from off screen.
        assert_eq!(local_radius(&Matter::Blob { scale: 3.0 }, Measured::default()), None);
    }

    /// The measured model size comes straight through for both mesh paths.
    #[test]
    fn a_model_reports_its_imported_size() {
        let m = Measured { model_size: Some(4.0), ..Default::default() };
        // A 4-unit longest edge is a box of ±2, whose corner is at 2·√3 ≈ 3.464.
        let want = Vec3::splat(2.0).length();
        for matter in [Matter::Mesh { asset_path: "a.glb".into() }, Matter::MapMesh { id: 7 }] {
            let r = local_radius(&matter, m).expect("measurable");
            assert!((r - want).abs() < 1e-3, "{matter:?} gave {r}, wanted {want}");
        }
    }

    /// A tilemap's radius reaches past its far corner.
    #[test]
    fn a_tilemap_covers_its_whole_grid() {
        let tm =
            Matter::Tilemap { cols: 20, rows: 12, tile: 1.5, data: Vec::new(), tileset: String::new() };
        let r = local_radius(&tm, Measured::default()).expect("measurable");
        // Half-diagonal of the real grid: the honest minimum.
        let real = ((20.0 * 1.5f32 / 2.0).powi(2) + (12.0 * 1.5f32 / 2.0).powi(2)).sqrt();
        assert!(r >= real, "{r} does not reach the grid corner at {real}");
    }

    /// A sea and a pool each measure the shape they actually are.
    #[test]
    fn water_measures_the_shape_it_is() {
        assert_eq!(local_radius(&water(WaterKind::Sea, 500.0, [1.0; 3]), Measured::default()), Some(500.0));
        // `radius` is ignored by a pool, and must stay ignored here.
        let pool = water(WaterKind::Pool, 500.0, [3.0, 4.0, 12.0]);
        let r = local_radius(&pool, Measured::default()).expect("measurable");
        assert!((r - 13.0).abs() < 1e-3, "3-4-12 has a 13 corner, got {r}");
    }

    /// A sprite far out in the batch's local space drags the radius out with it
    /// — the batch's own `size` says nothing about where its sprites are.
    #[test]
    fn the_sprite_reach_follows_the_furthest_sprite() {
        let near = Sprite { pos: [1.0, 0.0, 0.0], ..Sprite::default() };
        let far = Sprite { pos: [40.0, 30.0, 0.0], ..Sprite::default() };
        let r = sprite_reach(&[near, far], 2.0);
        assert!(r >= 50.0, "the sprite at 50 units out must be inside {r}");
        // A stretched sprite reaches further than an unstretched one.
        let big = Sprite { pos: [0.0; 3], scale: [8.0, 1.0], ..Sprite::default() };
        assert!(sprite_reach(&[big], 2.0) > sprite_reach(&[Sprite::default()], 2.0));
        // …and a roll cannot push a corner outside it (the diagonal is covered).
        let rolled = Sprite { rot: 0.785, ..Sprite::default() };
        assert!(sprite_reach(&[rolled], 2.0) >= 1.0 * 1.414);
        assert_eq!(sprite_reach(&[], 2.0), 0.0);
    }

    /// Nothing that draws through the loop is left unmeasured by accident.
    ///
    /// Written as an explicit list so ADDING a Matter kind that draws geometry
    /// makes somebody decide, here, whether it can be culled — rather than
    /// silently inheriting "always submit" and quietly costing a frame.
    #[test]
    fn every_drawing_matter_kind_has_an_answer() {
        let measured = Measured { model_size: Some(2.0), sprite_reach: Some(5.0) };
        let drawing: Vec<Matter> = vec![
            Matter::Primitive { shape: Shape::Cube, color: [1.0; 3] },
            Matter::Mesh { asset_path: "a.glb".into() },
            Matter::MapMesh { id: 1 },
            Matter::Tilemap {
                cols: 2,
                rows: 2,
                tile: 1.0,
                data: Vec::new(),
                tileset: String::new(),
            },
            Matter::SpriteBatch { size: 1.0 },
            water(WaterKind::Sea, 1.0, [1.0; 3]),
        ];
        for m in drawing {
            assert!(
                local_radius(&m, measured).is_some(),
                "{m:?} draws geometry but reports no bound, so it can never be culled"
            );
        }
    }

    /// A water volume with the shape fields set and everything this module
    /// never reads left at a plausible default.
    fn water(kind: WaterKind, radius: f32, half_extents: [f32; 3]) -> Matter {
        Matter::WaterVolume {
            kind,
            radius,
            half_extents,
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen: false,
            tint: [0.0; 3],
            visibility: 28.0,
        }
    }
}
