//! The 2D layer: sprite lighting ranks, primitive and water draws, and the 2D light uniform.

use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::math::DVec3;
use floptle_core::math::Mat4;
use floptle_core::transform::Transform;
use floptle_render::InstanceRaw;
use floptle_render::MaterialParams;
use floptle_render::MeshId;
use floptle_render::instance_of_mat;
use std::collections::HashMap;
use crate::shading::material_params;


/// A node's sorting-layer rank if it takes part in 2D lighting, else `None`.
///
/// A free function over the two fields it needs, not an `&self` method: the
/// render fns hold `self.gpu` mutably for their whole body, so nothing inside
/// them can borrow all of `self`. Asked by both gathers, so the Scene view and
/// the Game view cannot disagree about which surfaces are lit — the failure this
/// renderer has already paid for three times.
/// `reach` is [`floptle_render::Light2dUniform::reach`] — the ranks anything in
/// this frame can actually change. A surface no live light reaches is not on the
/// 2D path *this frame*, however its flag reads: the mask already decided it
/// contributes nothing, and honouring that here rather than in `fs_light` is the
/// difference between filtering a `u64` and instancing, uploading and
/// rasterizing the whole flat scene a second time to throw it away
/// (`floptle/0122`).
pub(crate) fn lit_2d_rank(
    world: &floptle_core::World,
    project: &floptle_scene::ProjectConfigDoc,
    e: Entity,
    flat_camera: bool,
    reach: u64,
) -> Option<u32> {
    if reach == 0 {
        return None;
    }
    let mode =
        world.get::<floptle_core::Lighting2D>(e).map(|l| l.mode).unwrap_or_default();
    let facts = floptle_core::Lit2DFacts { emits: false, flat_matter: true, flat_camera };
    let (is_2d, _) = floptle_core::resolve_2d(mode, facts);
    is_2d
        .then(|| {
            world
                .get::<floptle_core::Sorting>(e)
                .map(|sg| project.sorting_rank(&sg.layer))
                .unwrap_or(0)
        })
        .filter(|&r| r < 64 && reach & (1u64 << r) != 0)
}

/// The instance a `Matter::Primitive` draws, or `None` when its built-in shape
/// is not registered.
///
/// A function for the same reason [`lit_2d_rank`] is one: both gathers ask it,
/// so a cube cannot look one way in the Scene view and another in the Game
/// view. It used to be written out twice, and the two copies had already drifted
/// — the offscreen one never applied vertex PAINT, so a painted primitive was
/// painted on screen and plain in every other view.
///
/// `node_paint` is this node's own paint block (`paint_bases`). Every primitive
/// of a shape shares one MeshId, so the node's block is the only way two cubes
/// can be painted differently; falling back to the mesh's block (0 for
/// built-ins) is what an unpainted one gets. Brush paint modulates 2× (paint
/// light); a glTF import stays ×1.
pub(crate) fn primitive_draw(
    shape: floptle_core::Shape,
    color: [f32; 3],
    mat: Option<&Material>,
    model: Mat4,
    mesh_ids: &[MeshId],
    node_paint: Option<&[u32]>,
    raster: Option<&floptle_render::Raster>,
) -> Option<(MeshId, InstanceRaw)> {
    let &mesh = mesh_ids.get(shape as usize)?;
    let mut mp = mat.map(material_params).unwrap_or_else(|| MaterialParams::flat(color));
    let brush = node_paint.and_then(|v| v.first().copied()).filter(|&b| b != 0);
    mp.paint_modulate = brush.is_some();
    mp.paint_base =
        brush.unwrap_or_else(|| raster.map_or(0, |r| r.mesh_paint_base(mesh)));
    Some((mesh, instance_of_mat(model, &mp)))
}

/// WATER (`floptle/0038`). The instance a `Matter::WaterVolume` draws: a
/// translucent, specular surface sized to the volume the SOLVER uses, so what
/// you see is what floats you — the sea and the buoyancy can't drift apart,
/// which is exactly what happened while the ocean was a hand-placed sphere the
/// game kept in step by hand.
///
/// A frozen sea drops the translucency and the shine: ice is a surface you stand
/// on, and it should not look like something you could swim through.
///
/// Asked by both gathers. It was inline in the Scene view's gather only, so an
/// ocean was there while you edited and gone the moment you looked through the
/// game's camera — the fourth time this file's two gathers have disagreed about
/// whether something exists, and the reason this is a function.
///
/// `None` for any other matter, and for a shape that is not registered.
///
/// `material` is the node's own `Material` component, if any (`floptle/0144`).
/// **Absent → drawn exactly as before this card**: the hand-tuned defaults
/// below, untouched. Present → those defaults are the FALLBACK and the
/// material's own `specular`/`specular_strength`/`shininess` win outright, the
/// same "the node's Material wins whole" rule the rest of this file uses
/// (`part_look_rule`). `alpha` is the one field that does not follow that rule:
/// every unauthored `Material` defaults to `alpha = 1.0`, and a water volume
/// that carries one for some other reason (today, that is almost always
/// `retro: (exempt: true)` and nothing else) must not go opaque just because
/// nobody touched the number. A frozen volume stays opaque ice regardless of
/// what the material says.
///
/// `raster` doubles as "is a Raster available at all" (a thumbnail render may
/// have none) and, when a material is present, is what turns its `retro`
/// flags, `reflectivity` and the rest of the PBR surface extras into an
/// `ext_index` — the same store `material_draw` interns into, so a water
/// volume marked `retro: (exempt: true)` reads as exempt through the exact
/// path everything else does, in both gathers that call this function.
pub(crate) fn water_draw(
    matter: &Matter,
    material: Option<&Material>,
    t: &Transform,
    cam_world: DVec3,
    mesh_ids: &[MeshId],
    raster: Option<&mut floptle_render::Raster>,
) -> Option<(MeshId, InstanceRaw)> {
    use floptle_core::WaterKind;
    let Matter::WaterVolume { kind, radius, half_extents, frozen, tint, .. } = matter else {
        return None;
    };
    // The built-in sphere is r = 0.85 and the cube is half = 0.7; scale the
    // node's own transform so the drawn surface lands exactly on the volume's
    // extent.
    let (shape, fit) = match kind {
        WaterKind::Sea => {
            (floptle_core::Shape::Sphere, floptle_core::math::Vec3::splat(radius / 0.85))
        }
        WaterKind::Pool => {
            (floptle_core::Shape::Cube, floptle_core::math::Vec3::from(*half_extents) / 0.7)
        }
    };
    let &mesh = mesh_ids.get(shape as usize)?;
    let mut wt = *t;
    wt.scale *= fit;
    let model = wt.render_matrix(cam_world);
    let mut mp = MaterialParams::flat(*tint);
    if *frozen {
        mp.alpha = 1.0;
        mp.specular_strength = 0.15;
        mp.shininess = 8.0;
    } else {
        mp.alpha = 0.55;
        // Specular is what makes water read as water at a distance where no
        // wave is more than a pixel.
        mp.specular_strength = 0.9;
        mp.shininess = 96.0;
        mp.specular = [1.0, 1.0, 1.0];
    }
    if let Some(m) = material {
        mp.specular = m.specular;
        mp.specular_strength = m.specular_strength;
        mp.shininess = m.shininess;
        if !*frozen && m.alpha != 1.0 {
            mp.alpha = m.alpha;
        }
    }
    if let Some(raster) = raster {
        mp.paint_base = raster.mesh_paint_base(mesh);
        if let Some(m) = material {
            let ext = floptle_render::SurfaceExtras::from_material(m);
            mp.ext_index = raster.push_surface_extras(ext);
        }
    }
    Some((mesh, instance_of_mat(model, &mp)))
}

/// This frame's 2D lights, in the shape the accumulation shader reads.
///
/// The ambient is the scene's own — a flat surface with no 2D light near it then
/// composites to exactly what the raster pass already drew, so switching 2D
/// lighting on in a scene with no lights placed changes nothing. Compositing to
/// black there would read as the feature having broken the game.
/// Every flat node on the 2D lighting path this frame, and its sorting rank.
///
/// One function, called by both gathers, because 0122 asks for exactly that:
/// *the Scene-view and the Game-view gathers make the same decision, by
/// construction.* It used to be the same nine lines written out twice, which is
/// the shape this file has already paid for four times — see
/// `tests/offscreen_draws_the_same_world.rs`.
///
/// Empty when nothing can be reached, and empty *without walking the world*:
/// that is the "a scene with 2D lighting available but no light placed does zero
/// 2D lighting work" property, and a bullet hell was building a 366-entry map
/// twice a frame to reach it.
pub(crate) fn lit_2d_ranks(
    world: &floptle_core::World,
    project: &floptle_scene::ProjectConfigDoc,
    flat_camera: bool,
    reach: u64,
) -> HashMap<Entity, (u32, bool)> {
    if reach == 0 {
        return HashMap::new();
    }
    world
        .query::<Matter>()
        // A Sprite joins the flat set for the same reason the other two are in
        // it: it is flat, so a 2D light should reach it. Leaving it out would
        // make the one node type actually called "Sprite" the one a torch does
        // not touch.
        .filter(|(_, m)| {
            matches!(
                m,
                Matter::Tilemap { .. } | Matter::SpriteBatch { .. } | Matter::Sprite { .. }
            )
        })
        .filter_map(|(e, _)| {
            lit_2d_rank(world, project, e, flat_camera, reach).map(|r| (e, (r, casts_2d(world, e))))
        })
        .collect()
}

/// Whether this flat surface blocks 2D light (`floptle/0125`).
///
/// The three-valued answer the Inspector has been showing since the control
/// existed, asked here for real. Under `auto` **a tilemap casts exactly where it
/// is solid** — from the colliders its tileset already declares — so a level's
/// collision *is* its light occlusion and the two can never drift apart. The
/// cover that stops a bullet is the cover that stops the light, from one piece
/// of data.
pub(crate) fn casts_2d(world: &floptle_core::World, e: Entity) -> bool {
    let cast = world.get::<floptle_core::Shadow2D>(e).map(|s| s.0).unwrap_or_default();
    let flat_matter = matches!(world.get::<Matter>(e), Some(Matter::Tilemap { .. }));
    let collidable = world.get::<floptle_core::Collidable>(e).is_some();
    floptle_core::resolve_shadow_2d(cast, flat_matter, collidable).0
}

/// Takes the 2D half of a split that has already happened rather than asking for
/// one: both gathers need this before the draw loop (to know what a light can
/// reach — `floptle/0122`) and again at the pass, and each was walking the
/// scene's lights a second time to build the same value twice.
pub(crate) fn light2d_uniform(
    world: &floptle_core::World,
    two_d: &crate::shading::LightSlots,
    view_proj: floptle_core::math::Mat4,
) -> floptle_render::Light2dUniform {
    let (n, pos, color, mask, falloff) = (two_d.count, two_d.pos, two_d.color, two_d.mask, two_d.falloff);
    // The scene's **2D base light**, always — not the 3D ambient, and not a
    // special case for "no lights placed".
    //
    // It used to be white when no 2D light existed and the 3D ambient the moment
    // one did, which put a cliff exactly where somebody places their first
    // light: a whole level dropped to 12% brightness and the tilemap read as
    // having vanished. That is how it was reported, and it is the wrong way
    // round — **adding a light must only ever add light.** So the base is its
    // own field, it defaults to white, and turning it down is the deliberate act
    // that makes a dark room for a torch to carve a circle out of.
    let a = world
        .query::<floptle_core::Light>()
        .next()
        .map(|(_, l)| l.ambient_2d)
        .unwrap_or(floptle_core::Light::default().ambient_2d);
    let ambient = [a[0], a[1], a[2], 0.0];
    floptle_render::Light2dUniform {
        count: [n as f32, 0.0, 0.0, 0.0],
        ambient,
        inv_view_proj: view_proj.inverse().to_cols_array_2d(),
        // `view_proj`, `viewport` and the shadow budget are stamped by
        // `light2d_pass` from what it is actually drawing — they are facts about
        // the frame rather than about the scene's lights, and putting them here
        // would be one more thing for two gathers to forget differently.
        pos,
        color,
        falloff,
        mask,
        ..Default::default()
    }
}

#[cfg(test)]
mod lit_2d_tests {
    use super::*;
    use floptle_core::{Lighting2D, Lit2D, Sorting, World};

    /// **A material applies whole, or it is not the material.**
    ///
    /// Reported as: "I override a model's material, my new material has a
    /// texture, but it is still showing the texture of the normal model — and
    /// if I change the emission I can see the object get brighter." That is one
    /// rule applied to three quarters of a material: the node Material's params
    /// were taken (hence the emission), its colour was MULTIPLIED into the
    /// part's imported colour, and its texture replaced only if it had one.
    ///
    /// The rule is now: the most specific material wins, entire.
    #[test]
    fn the_most_specific_material_wins_whole() {
        use crate::mesh_instances::{PartLook, part_look_rule};

        let imported = [0.2, 0.4, 0.6];
        // A node Material that says nothing but its colour. It still supersedes.
        let node = MaterialParams::flat([1.0, 0.0, 0.0]);

        match part_look_rule(None, Some("Torso#2"), Some("Clothing"), Some(&node), imported) {
            PartLook::Node(m) => assert_eq!(
                m.color,
                [1.0, 0.0, 0.0],
                "a node Material is the model's look, not a tint over it — multiplying it \
                 into {imported:?} is what made a new material keep the old look"
            ),
            _ => panic!("a node Material must supersede the imported look"),
        }

        // Nothing on the node: the part keeps exactly what it was imported with.
        match part_look_rule(None, Some("Torso#2"), Some("Clothing"), None, imported) {
            PartLook::Imported(c) => assert_eq!(c, imported),
            _ => panic!("with no Material anywhere the model wears its own"),
        }

        // An override beats the node Material, for its object only.
        let mut om = floptle_core::ObjectMaterials::default();
        om.0.insert("Torso#2".into(), floptle_core::Material::tinted([0.0, 1.0, 0.0]));
        match part_look_rule(Some(&om), Some("Torso#2"), Some("Clothing"), Some(&node), imported) {
            PartLook::Override(k, m) => {
                assert_eq!(m.color, [0.0, 1.0, 0.0]);
                // The key matters as much as the material: it is how a
                // per-part `.flsl` binding for this override gets found again
                // at draw time — the wrong key silently loses the shader.
                assert_eq!(k, "Torso#2");
            }
            _ => panic!("the object's own material is the most specific one"),
        }
        // …and the parts it does not name still take the node Material.
        match part_look_rule(Some(&om), Some("LeftLeg#2"), Some("Pants"), Some(&node), imported) {
            PartLook::Node(m) => assert_eq!(m.color, [1.0, 0.0, 0.0]),
            _ => panic!("an override is for ITS object, not the model"),
        }

        // **The material name addresses its parts too**, which is the whole
        // clothing case: `Clothing` is one material across a torso and two arms,
        // and import renamed every one of those objects (`Torso` → `Torso#2`),
        // so the object name is not a name anybody has.
        let mut by_mat = floptle_core::ObjectMaterials::default();
        by_mat.0.insert("Clothing".into(), floptle_core::Material::tinted([0.0, 0.0, 1.0]));
        for object in ["Torso#2", "RightArm#2", "LeftArm#2"] {
            match part_look_rule(Some(&by_mat), Some(object), Some("Clothing"), Some(&node), imported)
            {
                PartLook::Override(k, m) => {
                    assert_eq!(m.color, [0.0, 0.0, 1.0], "{object}");
                    // Matched by MATERIAL name here (no per-object entry
                    // exists), so the key handed back is "Clothing", not
                    // the object's own name — that is the string a
                    // per-part shader binding for it is stored under.
                    assert_eq!(k, "Clothing", "{object}");
                }
                _ => panic!("a material name must reach every part wearing it ({object})"),
            }
        }
        // …and reach no further than that.
        match part_look_rule(Some(&by_mat), Some("RightLeg#2"), Some("Pants"), Some(&node), imported)
        {
            PartLook::Node(_) => {}
            _ => panic!("`Pants` is a different material and keeps the node's"),
        }
        // The precise name still wins where both are present.
        let mut both = floptle_core::ObjectMaterials::default();
        both.0.insert("Clothing".into(), floptle_core::Material::tinted([0.0, 0.0, 1.0]));
        both.0.insert("Torso#2".into(), floptle_core::Material::tinted([1.0, 1.0, 0.0]));
        match part_look_rule(Some(&both), Some("Torso#2"), Some("Clothing"), None, imported) {
            PartLook::Override(k, m) => {
                assert_eq!(m.color, [1.0, 1.0, 0.0], "object beats material");
                assert_eq!(k, "Torso#2", "the returned key must be the one that actually matched");
            }
            _ => panic!("the object's own override is the most specific"),
        }

        // A model whose parts have neither name cannot be addressed per object,
        // and must not accidentally match somebody else's override.
        match part_look_rule(Some(&om), None, None, None, imported) {
            PartLook::Imported(c) => assert_eq!(c, imported),
            _ => panic!("no key, no override"),
        }
    }

    /// The project's sorting layers, so a rank means something.
    fn project() -> floptle_scene::ProjectConfigDoc {
        floptle_scene::ProjectConfigDoc {
            sorting_layers: vec!["Default".into(), "Ground".into(), "Characters".into()],
            ..Default::default()
        }
    }

    fn batch_on(world: &mut World, layer: &str, mode: Lit2D) -> Entity {
        let e = world.spawn();
        world.insert(e, Matter::SpriteBatch { size: 1.0 });
        world.insert(e, Sorting { layer: layer.into(), order: 0, ..Default::default() });
        world.insert(e, Lighting2D { mode, ..Default::default() });
        e
    }

    /// `floptle/0122`: a flat surface no live light can reach is not gathered at
    /// all. The mask already said it contributes nothing — honouring that in
    /// `fs_light` instead means instancing, uploading and rasterizing the whole
    /// flat scene a second time to throw it away.
    #[test]
    fn a_surface_no_light_reaches_is_not_gathered() {
        let mut world = World::default();
        let ground = batch_on(&mut world, "Ground", Lit2D::Auto);
        let chars = batch_on(&mut world, "Characters", Lit2D::Auto);
        let p = project();
        // One light, reaching rank 1 (Ground) only — the card's own repro.
        let reach = 1u64 << p.sorting_rank("Ground");

        let got = lit_2d_ranks(&world, &p, true, reach);
        assert_eq!(got.len(), 1, "a batch the light cannot reach was still filled");
        assert_eq!(got.get(&ground).copied(), Some((p.sorting_rank("Ground"), false)));
        assert!(!got.contains_key(&chars), "Characters is on no light's mask");
    }

    /// …and with nothing to reach, the world is not walked at all. This is the
    /// "no light placed costs nothing" property, and it has to hold for a scene
    /// full of flat matter — which is every 2D scene.
    #[test]
    fn no_light_placed_gathers_nothing() {
        let mut world = World::default();
        batch_on(&mut world, "Default", Lit2D::Auto);
        batch_on(&mut world, "Ground", Lit2D::Yes);
        assert!(lit_2d_ranks(&world, &project(), true, 0).is_empty());
    }

    /// An unrestricted light reaches every rank, so the filter correctly does
    /// nothing — the ordinary case of somebody dropping one light into a scene
    /// must not start dropping surfaces.
    #[test]
    fn an_unrestricted_light_still_lights_everything() {
        let mut world = World::default();
        batch_on(&mut world, "Default", Lit2D::Auto);
        batch_on(&mut world, "Ground", Lit2D::Auto);
        batch_on(&mut world, "Characters", Lit2D::Auto);
        assert_eq!(lit_2d_ranks(&world, &project(), true, u64::MAX).len(), 3);
    }

    /// A node that says `3d` stays off the path however far a light reaches —
    /// stating it is never re-decided, which is the whole contract of the flag,
    /// and it is the workaround two games are currently standing on.
    #[test]
    fn a_node_that_said_3d_is_never_gathered() {
        let mut world = World::default();
        batch_on(&mut world, "Ground", Lit2D::No);
        assert!(lit_2d_ranks(&world, &project(), true, u64::MAX).is_empty());
    }
}

/// `floptle/0144`: `water_draw` used to build its `MaterialParams` from
/// scratch and never look at the node's own `Material` — no shader, no
/// `retro: (exempt: true)`, no way to style it at all. These pin the overlay
/// rule: absent Material → today's exact numbers; present → its surface
/// params win, EXCEPT `alpha`, which only overrides when the material set one
/// (every unauthored `Material` defaults to `alpha = 1.0`, and a water volume
/// wearing one for its `retro` flag alone must not go opaque).
#[cfg(test)]
mod water_draw_tests {
    use super::*;

    /// A `Pool` water volume with the project's default numbers — same shape
    /// as `Matter::default_water()`, but with `frozen` controllable so the ice
    /// path can be pinned too.
    fn pool(frozen: bool) -> Matter {
        Matter::WaterVolume {
            kind: floptle_core::WaterKind::Pool,
            radius: 10.0,
            half_extents: [5.0, 2.0, 5.0],
            density: 1000.0,
            drag: 1.0,
            angular_drag: 1.0,
            frozen,
            tint: [0.10, 0.32, 0.38],
            visibility: 28.0,
        }
    }

    fn mesh_ids() -> Vec<MeshId> {
        // `water_draw` only ever indexes Cube/Sphere; the exact MeshId value
        // doesn't matter to these tests, only that a slot exists.
        vec![MeshId(0), MeshId(1), MeshId(2), MeshId(3)]
    }

    /// **The property the card asks to be pinned above all others**: with no
    /// Material component, water draws exactly as it always has.
    #[test]
    fn no_material_draws_unchanged() {
        let ids = mesh_ids();
        let (_, unfrozen) =
            water_draw(&pool(false), None, &Transform::IDENTITY, DVec3::ZERO, &ids, None).unwrap();
        assert_eq!(unfrozen.color[3], 0.55, "unfrozen water's alpha is untouched");
        assert_eq!(unfrozen.specular[3], 0.9, "unfrozen water's specular strength is untouched");
        assert_eq!(unfrozen.specular[0..3], [1.0, 1.0, 1.0], "unfrozen water's specular colour");
        assert_eq!(unfrozen.params[0], 96.0, "unfrozen water's shininess is untouched");

        let (_, frozen) =
            water_draw(&pool(true), None, &Transform::IDENTITY, DVec3::ZERO, &ids, None).unwrap();
        assert_eq!(frozen.color[3], 1.0, "frozen water (ice) is opaque, untouched");
        assert_eq!(frozen.specular[3], 0.15, "frozen water's specular strength is untouched");
        assert_eq!(frozen.params[0], 8.0, "frozen water's shininess is untouched");
    }

    /// A Material's specular/shininess win outright once it exists — the same
    /// "most specific wins, whole" rule `part_look_rule` states for meshes —
    /// but its default `alpha = 1.0` must not silently make the water opaque:
    /// a water volume wearing a Material purely for `retro: (exempt: true)`
    /// keeps its translucency.
    #[test]
    fn material_overlays_specular_but_alpha_needs_an_actual_value() {
        let ids = mesh_ids();
        // alpha left at Material::default()'s 1.0 — the untouched case.
        let m = Material {
            specular: [0.2, 0.4, 0.9],
            specular_strength: 0.5,
            shininess: 40.0,
            ..Material::default()
        };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.specular[0..3], [0.2, 0.4, 0.9], "material specular colour wins");
        assert_eq!(raw.specular[3], 0.5, "material specular strength wins");
        assert_eq!(raw.params[0], 40.0, "material shininess wins");
        assert_eq!(
            raw.color[3], 0.55,
            "an untouched Material.alpha (1.0) must not override the water's own translucency"
        );
    }

    /// An author who does dial in a specific alpha gets it.
    #[test]
    fn material_alpha_is_honoured_once_actually_set() {
        let ids = mesh_ids();
        let m = Material { alpha: 0.2, ..Material::default() };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.color[3], 0.2, "an explicit material alpha is honoured");
    }

    /// Criterion 3: frozen ice stays opaque whatever the material says.
    #[test]
    fn frozen_ignores_material_alpha() {
        let ids = mesh_ids();
        let m = Material { alpha: 0.2, ..Material::default() };
        let (_, raw) = water_draw(
            &pool(true),
            Some(&m),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            None,
        )
        .unwrap();
        assert_eq!(raw.color[3], 1.0, "frozen water is opaque ice regardless of the material");
    }

    /// Criterion 5, watched against the real surface-extras store: under a
    /// project with `retro_dither_alpha: true`, a WaterVolume with no Material
    /// stays on the project's (dithered) neutral entry — matching every other
    /// undecorated surface — while one carrying `retro: (exempt: true)` lands
    /// on its own, distinct entry. `push_surface_extras` is the only thing
    /// that can answer this, so it needs a real (headless) `Raster`.
    #[test]
    fn retro_exempt_water_does_not_share_the_projects_dithered_neutral_entry() {
        let gpu = floptle_render::Gpu::headless(4, 4);
        // **A driver that cannot build the renderer is not a defect this test is
        // about.** `Raster::new` allocates a placeholder texture with a
        // reinterpretation view format (`upload_texture_mips`'s `view_formats`,
        // so a material's own picture can also be read raw for its surface
        // maps) — a capability CI's adapter does not have (the same class of
        // gap as "the raster pipeline cannot be built on OpenGL", already
        // documented in HANDOFF; wgpu's default uncaptured-error handler
        // panics, so without this the first test to build a full `Raster` on a
        // headless device anywhere finds that out by crashing the test binary).
        // Same idiom `doctor.rs` uses to answer "can this machine render" at
        // all: install a sink instead of the default panic, and if the
        // pipeline could not be built, this test has nothing to say about a
        // machine that cannot ask it the question.
        let failed = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = failed.clone();
        gpu.device.on_uncaptured_error(std::sync::Arc::new(move |e: wgpu::Error| {
            if let Ok(mut s) = sink.lock()
                && s.is_empty()
            {
                *s = e.to_string();
            }
        }));
        let mut raster = floptle_render::Raster::new(&gpu);
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
        if let Ok(why) = failed.lock()
            && !why.is_empty()
        {
            eprintln!("skipped — this machine cannot build the raster pipeline:\n{why}");
            return;
        }
        raster.set_retro_defaults(floptle_core::Retro {
            dither_alpha: true,
            ..floptle_core::Retro::default()
        });
        let ids = mesh_ids();

        // No Material at all → index 0, same as every plain surface.
        let (_, plain) = water_draw(
            &pool(false),
            None,
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        assert_eq!(
            floptle_render::ext_index_of(&plain),
            0,
            "a materialless water volume stays on the project's own (dithered) neutral entry"
        );

        // A Material attached but touching nothing (not even `retro`) still
        // follows the project's dither, same as the materialless case above —
        // it just lands on its own entry to get there, because an attached
        // Material's other neutral values (e.g. `roughness = 0.5`) genuinely
        // differ from the GPU-wide neutral (`roughness = 1.0`) index 0 holds.
        // That is an existing, general property of `push_surface_extras` and
        // not specific to water; the comparison below is against this index,
        // not against 0, so the test isolates what `retro.exempt` changes.
        let plain_material = Material::default();
        let (_, plain_mat) = water_draw(
            &pool(false),
            Some(&plain_material),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        let dithered_index = floptle_render::ext_index_of(&plain_mat);

        // `retro: (exempt: true)`, otherwise identical — must land on a
        // different entry than the (still dithered) plain material above.
        let exempt = Material {
            retro: floptle_core::Retro { exempt: true, ..floptle_core::Retro::default() },
            ..Material::default()
        };
        let (_, raw) = water_draw(
            &pool(false),
            Some(&exempt),
            &Transform::IDENTITY,
            DVec3::ZERO,
            &ids,
            Some(&mut raster),
        )
        .unwrap();
        assert_ne!(
            floptle_render::ext_index_of(&raw),
            dithered_index,
            "retro: (exempt: true) must NOT receive the project's EXT_DITHER_ALPHA — it needs \
             a surface-extras entry distinct from an otherwise-identical dithered material"
        );
    }
}
