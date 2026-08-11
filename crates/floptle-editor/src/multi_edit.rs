//! Multi-select Inspector edits — one change, every selected node.
//!
//! Selecting twenty crates and setting roughness once is the whole feature. The
//! difficulty is that the Inspector is an immediate-mode panel that writes
//! straight into the *primary* selection's components: by the time anything
//! knows an edit happened, the only record of it is that the component is not
//! what it used to be.
//!
//! So this works by **difference**, not by interception. [`Snapshot::take`]
//! clones the primary's components before the panel draws; [`Snapshot::apply`]
//! compares them afterwards, field by field, and writes only the fields that
//! actually moved onto every other selected node. Change roughness and only
//! roughness travels — each node keeps its own colour, its own texture, its own
//! everything else. That is the behaviour the rest of the industry settled on
//! and it is the one that survives a mixed selection.
//!
//! **The exhaustive destructure is the point.** Every struct's diff starts by
//! taking the value apart with no `..` in the pattern, so adding a field to
//! `Material` or `RigidBody` fails to compile *here* until the field is listed.
//! A knob that silently refused to multi-edit would be indistinguishable from a
//! knob that multi-edited fine and happened to already agree.
//!
//! Three things deliberately do not travel, and each has a reason rather than an
//! omission: a `Terrain`/`MapMesh` id (two nodes pointing at one field is data
//! loss, not an edit), a scene singleton like the Skybox or PostProcess node
//! (there is only ever one), and a camera's `active` flag or render-target name
//! (both are identities, not settings — see [`matter_propagates`]).

use floptle_core::CelestialBody;
use floptle_core::Entity;
use floptle_core::Material;
use floptle_core::Matter;
use floptle_core::RigidBody;
use floptle_core::Tags;
use floptle_core::World;
use floptle_core::script::{ScriptInst, Scripts};
use floptle_core::transform::Transform;

/// A component that can hand one field's change to another instance of itself.
pub(crate) trait FieldDiff: Sized {
    /// Write every field that differs between `before` and `after` onto
    /// `target`. Returns whether anything was written.
    fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool;
}

/// Generate a [`FieldDiff`] from a field list.
///
/// The generated body destructures `Self` **exhaustively** — no `..` — so this
/// list cannot drift from the struct. Adding a field to the struct is a compile
/// error until it is added here, which is exactly the moment to decide whether
/// it should travel across a multi-selection.
macro_rules! field_diff {
    ($ty:ty { $($f:ident),+ $(,)? }) => {
        impl FieldDiff for $ty {
            #[allow(clippy::clone_on_copy)]
            fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool {
                // Exhaustive on purpose — see the module docs.
                let Self { $($f),+ } = after;
                let mut hit = false;
                $(
                    if *$f != before.$f {
                        target.$f = $f.clone();
                        hit = true;
                    }
                )+
                hit
            }
        }
    };
}

/// A transform diffs **per axis**, not per field.
///
/// Typing a height into Y with a row of props selected is how a row of props
/// gets aligned, and it only works if X and Z stay where each prop had them —
/// so translation and scale compare component by component. Rotation does not:
/// a quaternion's parts are not independent, and the panel edits it through
/// Euler angles that all three components answer to, so an orientation travels
/// whole or not at all.
impl FieldDiff for Transform {
    fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool {
        let Self { translation, rotation, scale } = after;
        let mut hit = false;
        for i in 0..3 {
            if translation[i] != before.translation[i] {
                target.translation[i] = translation[i];
                hit = true;
            }
            if scale[i] != before.scale[i] {
                target.scale[i] = scale[i];
                hit = true;
            }
        }
        if *rotation != before.rotation {
            target.rotation = *rotation;
            hit = true;
        }
        hit
    }
}

field_diff!(Material {
    texture,
    color,
    emissive,
    emissive_strength,
    specular,
    shininess,
    specular_strength,
    rim,
    rim_strength,
    unlit,
    fog,
    ambient,
    alpha,
    normal_map,
    normal_strength,
    roughness_map,
    roughness,
    metallic_map,
    metallic,
    ao_map,
    occlusion_strength,
    reflectivity,
    transmission,
    ior,
    thickness,
    shading,
    retro,
    shader,
    shader_params,
    shader_textures,
    tiling,
    sheet_cols,
    sheet_rows,
    cell,
    shader_tiling,
});

field_diff!(RigidBody {
    kind,
    mode,
    radius,
    height,
    half_extents,
    restitution,
    friction,
    slope_limit,
    gravity,
    lock_pos,
    lock_rot,
    two_d,
    align_up,
    mass,
    assembly,
    pushbox_only,
});

field_diff!(CelestialBody {
    mu,
    body_radius,
    soi,
    parent,
    a,
    e,
    i,
    lan,
    arg_pe,
    m0,
    atmo_color,
    atmo_height,
    atmo_density,
    clouds,
    luminosity,
    star_color,
    occluder_radius,
});

field_diff!(floptle_audio::PlayParams {
    volume,
    pitch,
    pan,
    mode,
    falloff,
    min_distance,
    max_distance,
    track,
    end,
});

impl FieldDiff for floptle_audio::AudioSource {
    fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool {
        let Self { clip, params, play_on_start } = after;
        let mut hit = false;
        if *clip != before.clip {
            target.clip = clip.clone();
            hit = true;
        }
        if *play_on_start != before.play_on_start {
            target.play_on_start = *play_on_start;
            hit = true;
        }
        // Recurse rather than copying the whole block: turning one source's
        // volume up must not also hand every other source the primary's pitch.
        hit |= FieldDiff::apply_diff(&before.params, params, &mut target.params);
        hit
    }
}

/// Tags travel as **additions and removals**, not as a list.
///
/// Copying the primary's list wholesale would delete every per-node tag in the
/// selection, which is the one thing tags are for. Adding "flammable" to the
/// primary adds it to the rest; removing it removes it from the rest; anything
/// else each node already carried is left alone.
impl FieldDiff for Tags {
    fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool {
        let mut hit = false;
        for t in &after.0 {
            if !before.has(t) && !target.has(t) {
                target.0.push(t.clone());
                hit = true;
            }
        }
        for t in &before.0 {
            if !after.has(t)
                && let Some(i) = target.0.iter().position(|x| x == t)
            {
                target.0.remove(i);
                hit = true;
            }
        }
        hit
    }
}

/// Script tunables travel **per script kind and per parameter name**.
///
/// A selection of twenty enemies all running `chase.lua` is the case this is
/// for: retune `speed` once. Matching by kind rather than by position is what
/// makes it work on nodes that carry their scripts in a different order, or
/// carry other scripts as well — and matching by parameter name is what stops a
/// script that gained a parameter last week from shifting everyone else's.
impl FieldDiff for Scripts {
    fn apply_diff(before: &Self, after: &Self, target: &mut Self) -> bool {
        let mut hit = false;
        for a in &after.0 {
            let Some(b) = before.0.iter().find(|b| b.kind == a.kind) else {
                // A script attached during this frame: attaching it to the rest
                // of the selection is an add, not an edit, and adds are routed
                // through the Inspector's own commands.
                continue;
            };
            let Some(t) = target.0.iter_mut().find(|t| t.kind == a.kind) else { continue };
            hit |= diff_inst(b, a, t);
        }
        hit
    }
}

fn diff_inst(before: &ScriptInst, after: &ScriptInst, target: &mut ScriptInst) -> bool {
    // Exhaustive on purpose — a new kind of tunable must be handled here.
    let ScriptInst { kind: _, enabled, params, refs, strs } = after;
    let mut hit = false;
    if *enabled != before.enabled {
        target.enabled = *enabled;
        hit = true;
    }
    hit |= diff_named(params, &before.params, &mut target.params);
    hit |= diff_named(refs, &before.refs, &mut target.refs);
    hit |= diff_named(strs, &before.strs, &mut target.strs);
    hit
}

/// Diff one name→value list: any name whose value moved is written to `target`,
/// appended if `target` never had it.
fn diff_named<V: Clone + PartialEq>(
    after: &[(String, V)],
    before: &[(String, V)],
    target: &mut Vec<(String, V)>,
) -> bool {
    let mut hit = false;
    for (name, v) in after {
        let was = before.iter().find(|(n, _)| n == name).map(|(_, v)| v);
        if was == Some(v) {
            continue;
        }
        match target.iter_mut().find(|(n, _)| n == name) {
            Some((_, tv)) => *tv = v.clone(),
            None => target.push((name.clone(), v.clone())),
        }
        hit = true;
    }
    hit
}

/// Does an edit to this kind of node travel to the rest of the selection?
///
/// Exhaustive with no wildcard, so a new [`Matter`] variant does not compile
/// until someone answers the question for it. Answering `true` also means
/// adding the variant's fields to [`matter_diff`] — `true` alone is a no-op.
fn matter_propagates(m: &Matter) -> bool {
    match m {
        // The id IS the data: two nodes sharing a terrain or map-mesh id both
        // point at one field, and the second node's geometry is gone.
        Matter::Terrain { .. } | Matter::MapMesh { .. } => false,
        // One per scene. There is nothing to propagate to.
        Matter::Skybox { .. } | Matter::PostProcess { .. } => false,
        // Nothing to edit.
        Matter::Empty => false,
        Matter::Primitive { .. }
        | Matter::Blob { .. }
        | Matter::Mesh { .. }
        | Matter::Camera { .. }
        | Matter::PointLight { .. }
        | Matter::GravityVolume { .. }
        | Matter::WaterVolume { .. }
        | Matter::FieldShape { .. }
        | Matter::Tilemap { .. }
        | Matter::SpriteBatch { .. }
        | Matter::LightProbes { .. }
        | Matter::ReflectionProbe { .. } => true,
    }
}

/// Field-level diff between two nodes **of the same kind**. Mixed kinds share
/// no fields, so a selection of a light and a camera simply has nothing to
/// exchange — which is why this returns `false` rather than converting one.
fn matter_diff(before: &Matter, after: &Matter, target: &mut Matter) -> bool {
    if !matter_propagates(after) {
        return false;
    }
    match (after, before, target) {
        (
            Matter::Primitive { shape, color },
            Matter::Primitive { shape: bs, color: bc },
            Matter::Primitive { shape: ts, color: tc },
        ) => {
            let mut hit = false;
            set(shape, bs, ts, &mut hit);
            set(color, bc, tc, &mut hit);
            hit
        }
        (Matter::Blob { scale }, Matter::Blob { scale: b }, Matter::Blob { scale: t }) => {
            let mut hit = false;
            set(scale, b, t, &mut hit);
            hit
        }
        (
            Matter::Mesh { asset_path },
            Matter::Mesh { asset_path: b },
            Matter::Mesh { asset_path: t },
        ) => {
            let mut hit = false;
            set(asset_path, b, t, &mut hit);
            hit
        }
        (
            Matter::Camera {
                fov_y,
                active: _,
                target: _,
                cull_mask,
                target_w,
                target_h,
                target_hz,
                ortho,
                ortho_height,
            },
            Matter::Camera {
                fov_y: bf,
                cull_mask: bcm,
                target_w: bw,
                target_h: bh,
                target_hz: bhz,
                ortho: bo,
                ortho_height: boh,
                ..
            },
            Matter::Camera {
                fov_y: tf,
                cull_mask: tcm,
                target_w: tw,
                target_h: th,
                target_hz: thz,
                ortho: to,
                ortho_height: toh,
                ..
            },
        ) => {
            // `active` and `target` are identities, not settings: exactly one
            // camera holds play authority and a render-target name has to be
            // unique to be addressable. Copying either would break the scene
            // rather than edit it.
            let mut hit = false;
            set(fov_y, bf, tf, &mut hit);
            set(cull_mask, bcm, tcm, &mut hit);
            set(target_w, bw, tw, &mut hit);
            set(target_h, bh, th, &mut hit);
            set(target_hz, bhz, thz, &mut hit);
            set(ortho, bo, to, &mut hit);
            set(ortho_height, boh, toh, &mut hit);
            hit
        }
        (
            Matter::PointLight { color, intensity, range, shape, shadows },
            Matter::PointLight { color: bc, intensity: bi, range: br, shape: bsh, shadows: bsd },
            Matter::PointLight { color: tc, intensity: ti, range: tr, shape: tsh, shadows: tsd },
        ) => {
            let mut hit = false;
            set(color, bc, tc, &mut hit);
            set(intensity, bi, ti, &mut hit);
            set(range, br, tr, &mut hit);
            set(shape, bsh, tsh, &mut hit);
            set(shadows, bsd, tsd, &mut hit);
            hit
        }
        (
            Matter::GravityVolume { mode, strength, radius },
            Matter::GravityVolume { mode: bm, strength: bs, radius: br },
            Matter::GravityVolume { mode: tm, strength: ts, radius: tr },
        ) => {
            let mut hit = false;
            set(mode, bm, tm, &mut hit);
            set(strength, bs, ts, &mut hit);
            set(radius, br, tr, &mut hit);
            hit
        }
        (
            Matter::WaterVolume {
                kind,
                radius,
                half_extents,
                density,
                drag,
                angular_drag,
                frozen,
                tint,
                visibility,
            },
            Matter::WaterVolume {
                kind: bk,
                radius: br,
                half_extents: bhe,
                density: bd,
                drag: bdr,
                angular_drag: bad,
                frozen: bf,
                tint: bt,
                visibility: bv,
            },
            Matter::WaterVolume {
                kind: tk,
                radius: tr,
                half_extents: the,
                density: td,
                drag: tdr,
                angular_drag: tad,
                frozen: tf,
                tint: tt,
                visibility: tv,
            },
        ) => {
            let mut hit = false;
            set(kind, bk, tk, &mut hit);
            set(radius, br, tr, &mut hit);
            set(half_extents, bhe, the, &mut hit);
            set(density, bd, td, &mut hit);
            set(drag, bdr, tdr, &mut hit);
            set(angular_drag, bad, tad, &mut hit);
            set(frozen, bf, tf, &mut hit);
            set(tint, bt, tt, &mut hit);
            set(visibility, bv, tv, &mut hit);
            hit
        }
        (
            Matter::FieldShape { radius },
            Matter::FieldShape { radius: b },
            Matter::FieldShape { radius: t },
        ) => {
            let mut hit = false;
            set(radius, b, t, &mut hit);
            hit
        }
        (
            Matter::Tilemap { cols, rows, tile, data: _, tileset },
            Matter::Tilemap { cols: bc, rows: br, tile: bt, tileset: bts, .. },
            Matter::Tilemap { cols: tc, rows: tr, tile: tt, tileset: tts, .. },
        ) => {
            // `data` is what the Tiles tool paints, not what this panel edits —
            // and handing one map's squares to another map is not an edit.
            let mut hit = false;
            set(cols, bc, tc, &mut hit);
            set(rows, br, tr, &mut hit);
            set(tile, bt, tt, &mut hit);
            set(tileset, bts, tts, &mut hit);
            hit
        }
        (
            Matter::SpriteBatch { size },
            Matter::SpriteBatch { size: b },
            Matter::SpriteBatch { size: t },
        ) => {
            let mut hit = false;
            set(size, b, t, &mut hit);
            hit
        }
        (
            Matter::LightProbes {
                half_extents,
                spacing,
                enabled,
                intensity,
                bounces,
                quality,
                leak,
                normal_bias,
                exclude_layers,
            },
            Matter::LightProbes {
                half_extents: bhe,
                spacing: bsp,
                enabled: ben,
                intensity: bin,
                bounces: bbo,
                quality: bq,
                leak: bl,
                normal_bias: bnb,
                exclude_layers: bex,
            },
            Matter::LightProbes {
                half_extents: the,
                spacing: tsp,
                enabled: ten,
                intensity: tin,
                bounces: tbo,
                quality: tq,
                leak: tl,
                normal_bias: tnb,
                exclude_layers: tex,
            },
        ) => {
            let mut hit = false;
            set(half_extents, bhe, the, &mut hit);
            set(spacing, bsp, tsp, &mut hit);
            set(enabled, ben, ten, &mut hit);
            set(intensity, bin, tin, &mut hit);
            set(bounces, bbo, tbo, &mut hit);
            set(quality, bq, tq, &mut hit);
            set(leak, bl, tl, &mut hit);
            set(normal_bias, bnb, tnb, &mut hit);
            set(exclude_layers, bex, tex, &mut hit);
            hit
        }
        // Different kinds of node, or a kind that deliberately stays put.
        _ => false,
    }
}

/// `target = after` when `after` moved off `before`.
fn set<T: Clone + PartialEq>(after: &T, before: &T, target: &mut T, hit: &mut bool) {
    if after != before {
        *target = after.clone();
        *hit = true;
    }
}

/// The primary selection's components as they stood before the Inspector drew.
pub(crate) struct Snapshot {
    primary: Entity,
    transform: Option<Transform>,
    matter: Option<Matter>,
    material: Option<Material>,
    body: Option<RigidBody>,
    celestial: Option<CelestialBody>,
    audio: Option<floptle_audio::AudioSource>,
    scripts: Option<Scripts>,
    tags: Option<Tags>,
}

impl Snapshot {
    /// Clone what a multi-selection can share. `None` when the selection is a
    /// single node, so the ordinary case costs one length check.
    pub(crate) fn take(world: &World, selection: &[Entity]) -> Option<Self> {
        if selection.len() < 2 {
            return None;
        }
        let primary = *selection.last()?;
        Some(Self {
            primary,
            transform: world.get::<Transform>(primary).cloned(),
            matter: world.get::<Matter>(primary).cloned(),
            material: world.get::<Material>(primary).cloned(),
            body: world.get::<RigidBody>(primary).cloned(),
            celestial: world.get::<CelestialBody>(primary).cloned(),
            audio: world.get::<floptle_audio::AudioSource>(primary).cloned(),
            scripts: world.get::<Scripts>(primary).cloned(),
            tags: world.get::<Tags>(primary).cloned(),
        })
    }

    /// Hand every field the Inspector moved to the rest of the selection.
    /// Returns how many nodes actually took something.
    pub(crate) fn apply(&self, world: &mut World, selection: &[Entity]) -> usize {
        let mut n = 0;
        for &e in selection {
            if e == self.primary {
                continue;
            }
            let mut hit = false;
            hit |= diff_component::<Transform>(world, e, self.transform.as_ref(), self.primary);
            hit |= diff_component::<Material>(world, e, self.material.as_ref(), self.primary);
            hit |= diff_component::<RigidBody>(world, e, self.body.as_ref(), self.primary);
            hit |= diff_component::<CelestialBody>(world, e, self.celestial.as_ref(), self.primary);
            hit |= diff_component::<floptle_audio::AudioSource>(
                world,
                e,
                self.audio.as_ref(),
                self.primary,
            );
            hit |= diff_component::<Scripts>(world, e, self.scripts.as_ref(), self.primary);
            hit |= diff_component::<Tags>(world, e, self.tags.as_ref(), self.primary);
            // Matter is an enum, so it diffs by variant rather than by field
            // list — same-kind nodes only.
            if let (Some(before), Some(after)) =
                (self.matter.as_ref(), world.get::<Matter>(self.primary).cloned())
                && before != &after
                && let Some(t) = world.get_mut::<Matter>(e)
            {
                let mut t2 = t.clone();
                if matter_diff(before, &after, &mut t2) {
                    *t = t2;
                    hit = true;
                }
            }
            if hit {
                n += 1;
            }
        }
        n
    }
}

/// Diff one component type from the primary onto `e`.
fn diff_component<T>(world: &mut World, e: Entity, before: Option<&T>, primary: Entity) -> bool
where
    T: FieldDiff + Clone + PartialEq + 'static,
{
    let Some(before) = before else { return false };
    let Some(after) = world.get::<T>(primary).cloned() else { return false };
    if *before == after {
        return false;
    }
    let Some(t) = world.get_mut::<T>(e) else { return false };
    let mut t2 = t.clone();
    if T::apply_diff(before, &after, &mut t2) {
        *t = t2;
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::{Shading, Shape};

    fn scene(n: usize) -> (World, Vec<Entity>) {
        let mut w = World::new();
        let mut es = Vec::new();
        for i in 0..n {
            let e = w.spawn();
            w.insert(e, Matter::Primitive { shape: Shape::Cube, color: [1.0, 1.0, 1.0] });
            w.insert(e, Transform::from_translation(floptle_core::math::DVec3::new(
                i as f64, 0.0, 0.0,
            )));
            w.insert(e, Material { color: [0.1 * i as f32, 0.5, 0.5], ..Material::default() });
            es.push(e);
        }
        (w, es)
    }

    /// The whole feature in one test: change one field on the primary, every
    /// other selected node takes that field — and only that field.
    #[test]
    fn one_edit_travels_and_takes_nothing_with_it() {
        let (mut w, es) = scene(3);
        let snap = Snapshot::take(&w, &es).expect("three selected");
        // The Inspector's edit: roughness on the primary (the LAST selected).
        w.get_mut::<Material>(*es.last().unwrap()).unwrap().roughness = 0.9;
        assert_eq!(snap.apply(&mut w, &es), 2, "both other nodes take it");
        for (i, &e) in es.iter().enumerate() {
            let m = w.get::<Material>(e).unwrap();
            assert_eq!(m.roughness, 0.9, "node {i} took the roughness");
            // Each node's own colour survives — the thing a whole-component
            // copy would have destroyed.
            assert_eq!(m.color[0], 0.1 * i as f32, "node {i} kept its own colour");
        }
    }

    /// Nothing happens when the panel changed nothing, so a frame spent merely
    /// looking at a multi-selection cannot rewrite it.
    #[test]
    fn an_untouched_panel_writes_nothing() {
        let (mut w, es) = scene(3);
        let snap = Snapshot::take(&w, &es).expect("three selected");
        assert_eq!(snap.apply(&mut w, &es), 0);
        assert_eq!(w.get::<Material>(es[0]).unwrap().color[0], 0.0);
    }

    /// A single selection has nobody to tell.
    #[test]
    fn a_single_selection_takes_no_snapshot() {
        let (w, es) = scene(1);
        assert!(Snapshot::take(&w, &es).is_none());
    }

    /// Enum components travel per variant: two point lights share intensity, a
    /// camera in the same selection is left alone.
    #[test]
    fn same_kind_nodes_share_and_mixed_kinds_do_not() {
        let mut w = World::new();
        let cam = w.spawn();
        w.insert(cam, Matter::Camera {
            fov_y: 1.0,
            active: false,
            target: String::new(),
            cull_mask: u32::MAX,
            target_w: 64,
            target_h: 64,
            target_hz: 0.0,
            ortho: false,
            ortho_height: 10.0,
        });
        let a = w.spawn();
        let b = w.spawn();
        for &e in &[a, b] {
            w.insert(e, Matter::PointLight {
                color: [1.0, 1.0, 1.0],
                intensity: 1.0,
                range: 5.0,
                shape: floptle_core::LightShape::Point,
                shadows: false,
            });
        }
        // b is last, so b is the primary.
        let sel = vec![cam, a, b];
        let snap = Snapshot::take(&w, &sel).expect("three selected");
        if let Some(Matter::PointLight { intensity, .. }) = w.get_mut::<Matter>(b) {
            *intensity = 4.0;
        }
        assert_eq!(snap.apply(&mut w, &sel), 1, "only the other light takes it");
        assert!(matches!(w.get::<Matter>(a), Some(Matter::PointLight { intensity, .. }) if *intensity == 4.0));
        assert!(matches!(w.get::<Matter>(cam), Some(Matter::Camera { fov_y, .. }) if *fov_y == 1.0));
    }

    /// A camera's play authority and render-target name are identities: two
    /// cameras must not both become active, or both write one target.
    #[test]
    fn a_cameras_identity_never_travels_but_its_lens_does() {
        let mut w = World::new();
        let mk = |w: &mut World, active: bool, target: &str| {
            let e = w.spawn();
            w.insert(e, Matter::Camera {
                fov_y: 1.0,
                active,
                target: target.to_string(),
                cull_mask: u32::MAX,
                target_w: 64,
                target_h: 64,
                target_hz: 0.0,
                ortho: false,
                ortho_height: 10.0,
            });
            e
        };
        let a = mk(&mut w, false, "mirror");
        let b = mk(&mut w, true, "minimap");
        let sel = vec![a, b];
        let snap = Snapshot::take(&w, &sel).expect("two selected");
        if let Some(Matter::Camera { fov_y, .. }) = w.get_mut::<Matter>(b) {
            *fov_y = 0.5;
        }
        snap.apply(&mut w, &sel);
        let Some(Matter::Camera { fov_y, active, target, .. }) = w.get::<Matter>(a) else {
            panic!("still a camera")
        };
        assert_eq!(*fov_y, 0.5, "the lens travels");
        assert!(!*active, "authority does not");
        assert_eq!(target, "mirror", "and neither does the target name");
    }

    /// A terrain id is the node's data, not a setting — two nodes sharing one
    /// would silently discard a sculpt.
    #[test]
    fn a_terrain_id_never_travels() {
        let mut w = World::new();
        let a = w.spawn();
        w.insert(a, Matter::Terrain { id: 1 });
        let b = w.spawn();
        w.insert(b, Matter::Terrain { id: 2 });
        let sel = vec![a, b];
        let snap = Snapshot::take(&w, &sel).expect("two selected");
        if let Some(Matter::Terrain { id }) = w.get_mut::<Matter>(b) {
            *id = 7;
        }
        assert_eq!(snap.apply(&mut w, &sel), 0);
        assert!(matches!(w.get::<Matter>(a), Some(Matter::Terrain { id }) if *id == 1));
    }

    /// Tags travel as the change, so each node keeps the tags it already had.
    #[test]
    fn a_tag_added_to_one_is_added_to_all_without_flattening_them() {
        let mut w = World::new();
        let a = w.spawn();
        w.insert(a, Tags(vec!["crate".into(), "wooden".into()]));
        let b = w.spawn();
        w.insert(b, Tags(vec!["crate".into(), "metal".into()]));
        let sel = vec![a, b];
        let snap = Snapshot::take(&w, &sel).expect("two selected");
        let t = w.get_mut::<Tags>(b).unwrap();
        t.0.push("flammable".into());
        t.0.retain(|x| x != "crate");
        snap.apply(&mut w, &sel);
        let a_tags = &w.get::<Tags>(a).unwrap().0;
        assert!(a_tags.contains(&"flammable".to_string()), "the addition travelled");
        assert!(!a_tags.contains(&"crate".to_string()), "and so did the removal");
        assert!(a_tags.contains(&"wooden".to_string()), "its own tag survived");
    }

    /// Script tunables travel by kind and by parameter name — the twenty-enemies
    /// case — and a script the others do not run is skipped.
    #[test]
    fn a_script_parameter_travels_by_name_to_every_node_running_that_script() {
        let mut w = World::new();
        let mk = |w: &mut World, speed: f32, extra: bool| {
            let e = w.spawn();
            let mut list = vec![ScriptInst {
                kind: "chase".into(),
                enabled: true,
                params: vec![("speed".into(), speed), ("range".into(), 9.0)],
                refs: vec![],
                strs: vec![],
            }];
            if extra {
                list.push(ScriptInst::new("blink"));
            }
            w.insert(e, Scripts(list));
            e
        };
        let a = mk(&mut w, 1.0, false);
        let b = mk(&mut w, 2.0, true);
        let sel = vec![a, b];
        let snap = Snapshot::take(&w, &sel).expect("two selected");
        w.get_mut::<Scripts>(b).unwrap().0[0].params[0].1 = 12.0;
        assert_eq!(snap.apply(&mut w, &sel), 1);
        let s = &w.get::<Scripts>(a).unwrap().0[0];
        assert_eq!(s.param("speed", 0.0), 12.0, "the retuned parameter travelled");
        assert_eq!(s.param("range", 0.0), 9.0, "the untouched one did not");
        assert_eq!(w.get::<Scripts>(a).unwrap().0.len(), 1, "no script was attached");
    }

    /// Turning one audio source up must not hand the rest of them its pitch.
    #[test]
    fn audio_travels_one_knob_at_a_time() {
        let mut w = World::new();
        let mk = |w: &mut World, pitch: f32| {
            let e = w.spawn();
            let mut s = floptle_audio::AudioSource::default();
            s.params.pitch = pitch;
            w.insert(e, s);
            e
        };
        let a = mk(&mut w, 0.5);
        let b = mk(&mut w, 2.0);
        let sel = vec![a, b];
        let snap = Snapshot::take(&w, &sel).expect("two selected");
        w.get_mut::<floptle_audio::AudioSource>(b).unwrap().params.volume = 0.25;
        snap.apply(&mut w, &sel);
        let s = w.get::<floptle_audio::AudioSource>(a).unwrap();
        assert_eq!(s.params.volume, 0.25);
        assert_eq!(s.params.pitch, 0.5, "its own pitch survived");
    }

    /// A typed transform field is how a row of props gets aligned; the fields
    /// left alone stay per-node.
    #[test]
    fn a_typed_transform_field_aligns_the_selection() {
        let (mut w, es) = scene(3);
        let snap = Snapshot::take(&w, &es).expect("three selected");
        w.get_mut::<Transform>(*es.last().unwrap()).unwrap().translation.y = 5.0;
        snap.apply(&mut w, &es);
        for (i, &e) in es.iter().enumerate() {
            let t = w.get::<Transform>(e).unwrap();
            assert_eq!(t.translation.y, 5.0, "node {i} moved to the typed height");
            assert_eq!(t.translation.x, i as f64, "and kept its own x");
        }
    }

    /// Shading models are enums and travel like anything else — the check that
    /// the diff is not quietly float-only.
    #[test]
    fn a_shading_model_travels() {
        let (mut w, es) = scene(2);
        let snap = Snapshot::take(&w, &es).expect("two selected");
        w.get_mut::<Material>(es[1]).unwrap().shading = Shading::Physical;
        snap.apply(&mut w, &es);
        assert_eq!(w.get::<Material>(es[0]).unwrap().shading, Shading::Physical);
    }
}
