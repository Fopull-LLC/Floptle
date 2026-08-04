//! The cross-node / cross-script Lua reference layer: `node` and `script`
//! handle metatables (transform/body/component access, hierarchy traversal),
//! and the `find` / `findAll` / `findScript` globals.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use floptle_core::math::{EulerRot, Quat, Vec3};
use floptle_core::{Entity, Matter, ParticleSystem, RigidBody, World};
use mlua::{Lua, Table, Value};

use crate::env::{as_num, new_component_handle, new_node_handle, new_script_handle};
use crate::{AnimCmd, AnimInfo, Shared, VfxCmd};

/// How far up a parent chain the world composers walk before giving up. A cycle
/// in the parent map must cost a frame nothing, not hang it.
const MAX_PARENT_DEPTH: usize = 64;

/// A node's transform composed all the way up the parent chain — the same
/// composition `floptle_core::world_transform` performs, against the script
/// host's live mirror (which carries this frame's writes; the ECS does not yet).
pub(crate) fn world_transform_of(s: &crate::SceneMirror, e: u32) -> floptle_core::Transform {
    let mut w = s.transforms.get(&e).copied().unwrap_or(floptle_core::Transform::IDENTITY);
    let mut cur = e;
    for _ in 0..MAX_PARENT_DEPTH {
        let Some(&up) = s.parent.get(&cur) else { break };
        let Some(ptr) = s.transforms.get(&up) else { break };
        w = ptr.mul_transform(&w);
        cur = up;
    }
    w
}

/// The composed world transform of `e`'s PARENT (identity when it has none) —
/// the frame a world position has to be brought back through to become a local
/// one.
pub(crate) fn parent_world_of(s: &crate::SceneMirror, e: u32) -> floptle_core::Transform {
    match s.parent.get(&e) {
        Some(&p) => world_transform_of(s, p),
        None => floptle_core::Transform::IDENTITY,
    }
}

/// A node's LOCAL transform as the script currently sees it: the handle's live
/// raw `x`/`y`/`z` when this is the script's own node — possibly written earlier
/// in this same hook — otherwise the mirror.
///
/// The own-node handle carries raw position fields (that is what makes
/// `node.x = node.x + 1` a plain table write), so the mirror is a frame behind
/// for the duration of a hook. Reading world space without this rule is how
/// `node.pos = p; log(node.worldX)` answers about where the node USED to be.
fn live_local_of(s: &crate::SceneMirror, this: &Table, e: u32) -> floptle_core::Transform {
    let mut t = s.transforms.get(&e).copied().unwrap_or(floptle_core::Transform::IDENTITY);
    if let (Ok(x), Ok(y), Ok(z)) =
        (this.raw_get::<f64>("x"), this.raw_get::<f64>("y"), this.raw_get::<f64>("z"))
    {
        t.translation = glam::DVec3::new(x, y, z);
    }
    t
}

/// [`world_transform_of`], but honouring a handle's live local position.
pub(crate) fn world_transform_of_handle(
    s: &crate::SceneMirror,
    this: &Table,
    e: u32,
) -> floptle_core::Transform {
    parent_world_of(s, e).mul_transform(&live_local_of(s, this, e))
}

/// The world position of whatever a Lua value refers to: a node handle (through
/// its parent chain, live local included) or any plain `{x=,y=,z=}` / vec3,
/// which is already a world point.
fn world_pos_of_value(s: &crate::SceneMirror, v: &Value) -> Option<glam::DVec3> {
    if let Value::Table(t) = v
        && let Ok(e) = t.raw_get::<u32>("__id")
    {
        return Some(world_transform_of_handle(s, t, e).translation);
    }
    crate::math_api::vec3_of(v)
}

/// Wrap into (−π, π]: the shortest way round, which is the whole point of a
/// turn-towards step.
fn wrap_pi_f64(a: f64) -> f64 {
    let tau = std::f64::consts::TAU;
    let mut x = (a + std::f64::consts::PI).rem_euclid(tau) - std::f64::consts::PI;
    if x <= -std::f64::consts::PI {
        x += tau;
    }
    x
}

/// Build a Lua colour: `{ r = , g = , b = , a = }`, also indexable `[1]`..`[4]`.
///
/// A plain table rather than a userdata, so it prints, serialises, compares and
/// can be built by hand out of a save file — and so `{1, 0, 0}` from anywhere
/// else in a project is already a colour.
pub(crate) fn new_color(lua: &Lua, c: [f32; 4]) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    for (k, v) in ["r", "g", "b", "a"].iter().zip(c) {
        t.raw_set(*k, v as f64)?;
    }
    for (i, v) in c.iter().enumerate() {
        t.raw_set(i + 1, *v as f64)?;
    }
    Ok(t)
}

/// Read a colour out of a Lua table: named `r/g/b/a` first, then `[1]`..`[4]`.
/// A missing alpha is 1 — `color(1, 0, 0)` is opaque red, not invisible red.
pub(crate) fn read_color(t: &Table) -> mlua::Result<[f32; 4]> {
    let get = |named: &str, i: usize| -> f64 {
        t.raw_get::<Option<f64>>(named)
            .ok()
            .flatten()
            .or_else(|| t.raw_get::<Option<f64>>(i).ok().flatten())
            .unwrap_or(if named == "a" { 1.0 } else { 0.0 })
    };
    Ok([
        get("r", 1) as f32,
        get("g", 2) as f32,
        get("b", 3) as f32,
        get("a", 4) as f32,
    ])
}

/// Component fields that read and write as a whole COLOUR rather than as four
/// channels: `(component, field)`.
///
/// `e.fill = color(1, 0.85, 0.35)` instead of four lines of `e.fillR = …`. The
/// channel names stay — a script that pokes one channel to fade something
/// keeps working — but nobody has to write a colour that way any more.
///
/// A separate channel from the numeric one rather than an expansion into
/// `fillR`..`fillA`, because `borderR` already means the RIGHT border width.
/// Sharing the namespace would have made a colour assignment silently resize
/// an edge.
pub fn mirror_component_colors(
    world: &World,
    e: Entity,
) -> HashMap<String, HashMap<String, [f32; 4]>> {
    let mut out: HashMap<String, HashMap<String, [f32; 4]>> = HashMap::new();
    if let Some(spec) = world.get::<floptle_ui::ElementSpec>(e) {
        let mut f: HashMap<String, [f32; 4]> = HashMap::new();
        if let Some(s) = &spec.shape {
            f.insert("fill".into(), s.fill);
            f.insert("borderColor".into(), s.border_color);
        }
        if let Some(t) = &spec.text {
            f.insert("textColor".into(), t.color);
        }
        if let Some(img) = &spec.image {
            f.insert("tint".into(), img.tint);
        }
        if let Some(fl) = &spec.field {
            f.insert("caretColor".into(), fl.caret_color);
            f.insert("selectionColor".into(), fl.selection_color);
            f.insert("placeholderColor".into(), fl.placeholder_color);
        }
        // The subtree multiplier. Named apart from the image `tint` because
        // they are genuinely different things and the channel names already
        // were (`groupR` vs `tintR`).
        f.insert("groupTint".into(), spec.tint);
        out.insert("UiElement".to_string(), f);
    }
    if let Some(Matter::PointLight { color, .. }) = world.get::<Matter>(e) {
        out.insert(
            "PointLight".to_string(),
            HashMap::from([("color".to_string(), [color[0], color[1], color[2], 1.0])]),
        );
    }
    out
}

/// Apply a colour-valued `node:getcomponent(...).field = color(...)` write.
pub fn apply_component_color(
    world: &mut World,
    ent: Entity,
    comp: &str,
    field: &str,
    v: [f32; 4],
) {
    match comp {
        "UiElement" => {
            let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent) else { return };
            match field {
                "fill" => spec.shape.get_or_insert_with(Default::default).fill = v,
                "borderColor" => {
                    spec.shape.get_or_insert_with(Default::default).border_color = v;
                }
                // Colouring text on a node that has none is a no-op rather
                // than a surprise label appearing.
                "textColor" => {
                    if let Some(t) = &mut spec.text {
                        t.color = v;
                    }
                }
                "tint" => {
                    if let Some(img) = &mut spec.image {
                        img.tint = v;
                    }
                }
                "caretColor" | "selectionColor" | "placeholderColor" => {
                    if let Some(f) = &mut spec.field {
                        match field {
                            "caretColor" => f.caret_color = v,
                            "selectionColor" => f.selection_color = v,
                            _ => f.placeholder_color = v,
                        }
                    }
                }
                "groupTint" => spec.tint = v,
                _ => {}
            }
        }
        "PointLight" => {
            if let Some(Matter::PointLight { color, .. }) = world.get_mut::<Matter>(ent)
                && field == "color"
            {
                *color = [v[0], v[1], v[2]];
            }
        }
        _ => {}
    }
}

/// Component fields that read back as a BOOLEAN, by `(component, field)`.
///
/// They are stored as 1/0 like everything else, and that is a trap in Lua:
/// `0` is **truthy**, so `if el.visible then` was always taken. Returning a
/// real boolean is the only way `if` means what it looks like.
pub fn is_bool_field(comp: &str, field: &str) -> bool {
    matches!(
        (comp, field),
        ("UiElement", "visible" | "disabled" | "selected" | "toggle" | "focusable")
            | ("UiLayer", "enabled" | "worldSpace")
            | ("Camera", "active")
            | (
                "RigidBody",
                "gravity"
                    | "kinematic"
                    | "pushboxOnly"
                    | "lock_x"
                    | "lock_y"
                    | "lock_z"
                    | "lock_rot_x"
                    | "lock_rot_y"
                    | "lock_rot_z"
            )
    )
}

/// The numeric component fields exposed to scripts via `node:getcomponent(name)`, mirrored
/// from the live ECS each frame. Extend here (and in [`apply_component_field`]) to reach
/// more components / fields.
pub fn mirror_components(world: &World, e: Entity) -> HashMap<String, HashMap<String, f64>> {
    let mut out: HashMap<String, HashMap<String, f64>> = HashMap::new();
    if let Some(Matter::PointLight { color, intensity, range }) = world.get::<Matter>(e) {
        out.insert(
            "PointLight".to_string(),
            HashMap::from([
                ("intensity".to_string(), *intensity as f64),
                ("range".to_string(), *range as f64),
                ("r".to_string(), color[0] as f64),
                ("g".to_string(), color[1] as f64),
                ("b".to_string(), color[2] as f64),
            ]),
        );
    }
    if let Some(ps) = world.get::<ParticleSystem>(e) {
        out.insert(
            "ParticleSystem".to_string(),
            HashMap::from([("play_on_start".to_string(), if ps.play_on_start { 1.0 } else { 0.0 })]),
        );
    }
    // A material's SPRITESHEET frame — the mesh-side twin of a UI image's
    // `cell`. Only the sheet fields live here: everything else about a material
    // is set through `node:setMaterial{...}` (colors and paths a f64 can't
    // carry), while `cell` wants the cheap per-frame write a mirror field is.
    if let Some(m) = world.get::<floptle_core::Material>(e) {
        out.insert(
            "Material".to_string(),
            HashMap::from([
                ("cell".to_string(), m.cell as f64),
                ("sheetCols".to_string(), m.sheet_cols as f64),
                ("sheetRows".to_string(), m.sheet_rows as f64),
            ]),
        );
    }
    // AudioSource tunables (camelCase, live during play — the audio system
    // diffs the component each frame and updates the voice). Enums are
    // numeric here (the f64 mirror): mode 0=Spatial 1=Distance 2=Flat;
    // falloff 0=Inverse 1=Linear 2=Exponential; endBehavior 0=Stop 1=Destroy
    // 2=Loop. `node:sound()` covers play/stop/setClip (strings/methods).
    if let Some(src) = world.get::<floptle_audio::AudioSource>(e) {
        let p = &src.params;
        out.insert(
            "AudioSource".to_string(),
            HashMap::from([
                ("volume".to_string(), p.volume as f64),
                ("pitch".to_string(), p.pitch as f64),
                ("pan".to_string(), p.pan as f64),
                ("minDistance".to_string(), p.min_distance as f64),
                ("maxDistance".to_string(), p.max_distance as f64),
                ("playOnStart".to_string(), if src.play_on_start { 1.0 } else { 0.0 }),
                ("mode".to_string(), match p.mode {
                    floptle_audio::SpatialMode::Spatial => 0.0,
                    floptle_audio::SpatialMode::Distance => 1.0,
                    floptle_audio::SpatialMode::Flat => 2.0,
                }),
                ("falloff".to_string(), match p.falloff {
                    floptle_audio::Falloff::Inverse => 0.0,
                    floptle_audio::Falloff::Linear => 1.0,
                    floptle_audio::Falloff::Exponential => 2.0,
                }),
                ("endBehavior".to_string(), match p.end {
                    floptle_audio::EndBehavior::Stop => 0.0,
                    floptle_audio::EndBehavior::Destroy => 1.0,
                    floptle_audio::EndBehavior::Loop => 2.0,
                }),
            ]),
        );
    }
    // Game-UI components (docs/ui-system-proposal.md): drive HUDs from scripts.
    // Fields are camelCase (user-facing API); `node.text` covers the string side.
    if let Some(spec) = world.get::<floptle_ui::ElementSpec>(e) {
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        let mut f: HashMap<String, f64> = HashMap::from([
            ("visible".to_string(), b(spec.visible)),
            ("opacity".to_string(), spec.opacity as f64),
        ]);
        // Position: the active placement's numbers (Free pos, Pin offset, or a
        // Stretch's leading margin).
        let (px, py) = match spec.place {
            floptle_ui::Place::Free { pos } => (pos[0], pos[1]),
            floptle_ui::Place::Pin { offset, .. } => (offset[0], offset[1]),
            floptle_ui::Place::Stretch { margin, .. } => (margin[0], margin[1]),
        };
        f.insert("posX".to_string(), px as f64);
        f.insert("posY".to_string(), py as f64);
        // Size: the numeric part of the current mode (Fixed px, Pct fraction,
        // Grow weight). A Fit axis has no number — the field is absent (nil).
        for (key, s) in [("width", spec.size[0]), ("height", spec.size[1])] {
            match s {
                floptle_ui::Size::Fixed(v) | floptle_ui::Size::Pct(v) | floptle_ui::Size::Grow(v) => {
                    f.insert(key.to_string(), v as f64);
                }
                floptle_ui::Size::Fit => {}
            }
        }
        if let Some(s) = &spec.shape {
            // `radius`/`border` read back the FIRST entry: with a uniform value
            // (the overwhelmingly common case) that is exactly the number the
            // designer typed, and per-corner shapes have the indexed fields
            // below to read instead.
            f.insert("radius".to_string(), s.radius[0] as f64);
            f.insert("border".to_string(), s.border[0] as f64);
            for (k, v) in ["radiusTL", "radiusTR", "radiusBR", "radiusBL"].iter().zip(s.radius.0) {
                f.insert(k.to_string(), v as f64);
            }
            for (k, v) in ["borderL", "borderT", "borderR", "borderB"].iter().zip(s.border.0) {
                f.insert(k.to_string(), v as f64);
            }
            for (k, v) in ["fillR", "fillG", "fillB", "fillA"].iter().zip(s.fill) {
                f.insert(k.to_string(), v as f64);
            }
        }
        if let Some(t) = &spec.text {
            f.insert("textSize".to_string(), t.size as f64);
            f.insert("tracking".to_string(), t.tracking as f64);
            for (k, v) in ["textR", "textG", "textB", "textA"].iter().zip(t.color) {
                f.insert(k.to_string(), v as f64);
            }
        }
        // Interaction states a script drives. `hover`/`pressed` are the
        // engine's to set; these two are the game's — "this row is the current
        // one", "this button can't be used yet".
        f.insert("disabled".to_string(), f64::from(u8::from(spec.disabled)));
        f.insert("selected".to_string(), f64::from(u8::from(spec.selected)));
        // Sibling depth: lower draws further back, and orders a stack's flow.
        // Scriptable because "raise the panel the player is talking to" is a
        // gameplay decision, not a layout one.
        f.insert("order".to_string(), spec.order as f64);
        f.insert("focusable".to_string(), f64::from(u8::from(spec.focusable)));
        // The visual transform — the press-dip / hover-pop channel. Layout is
        // unaffected, so a script can animate these every frame without ever
        // reflowing the screen.
        f.insert("rotation".to_string(), spec.rotation as f64);
        f.insert("scaleX".to_string(), spec.scale[0] as f64);
        f.insert("scaleY".to_string(), spec.scale[1] as f64);
        // Named `group*`, not `tint*`: an image already owns `tintR..A`, and
        // this one multiplies the whole SUBTREE rather than one texture.
        for (k, v) in ["groupR", "groupG", "groupB", "groupA"].iter().zip(spec.tint) {
            f.insert(k.to_string(), v as f64);
        }
        if let Some(img) = &spec.image {
            for (k, v) in ["tintR", "tintG", "tintB", "tintA"].iter().zip(img.tint) {
                f.insert(k.to_string(), v as f64);
            }
            f.insert("cell".to_string(), img.cell as f64);
        }
        if let Some(sc) = spec.scroll {
            f.insert("scrollY".to_string(), sc.offset as f64);
            f.insert("scrollX".to_string(), sc.offset_x as f64);
        }
        f.insert("toggle".to_string(), f64::from(u8::from(spec.toggle)));
        // A repeater's row count — the one number a data-driven list needs,
        // and the natural target for `ui.bind(list, "count", …)`.
        if let Some(r) = &spec.repeater {
            f.insert("count".to_string(), f64::from(r.count));
        }
        out.insert("UiElement".to_string(), f);
        if let Some(s) = spec.slider {
            out.insert(
                "UiSlider".to_string(),
                HashMap::from([
                    ("value".to_string(), s.value as f64),
                    ("min".to_string(), s.min as f64),
                    ("max".to_string(), s.max as f64),
                ]),
            );
        }
    }
    if let Some(Matter::Camera { fov_y, active, .. }) = world.get::<Matter>(e) {
        out.insert(
            "Camera".to_string(),
            HashMap::from([
                ("fovY".to_string(), *fov_y as f64),
                ("active".to_string(), if *active { 1.0 } else { 0.0 }),
            ]),
        );
    }
    if let Some(l) = world.get::<floptle_ui::UiLayer>(e) {
        out.insert(
            "UiLayer".to_string(),
            HashMap::from([
                ("enabled".to_string(), if l.enabled { 1.0 } else { 0.0 }),
                ("z".to_string(), l.z as f64),
                ("designHeight".to_string(), l.design_height as f64),
                // 0 = Screen overlay, 1 = World-space panel.
                ("worldSpace".to_string(), if l.is_world() { 1.0 } else { 0.0 }),
            ]),
        );
    }
    if let Some(rb) = world.get::<RigidBody>(e) {
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        out.insert(
            "RigidBody".to_string(),
            HashMap::from([
                ("friction".to_string(), rb.friction as f64),
                ("restitution".to_string(), rb.restitution as f64),
                ("gravity".to_string(), b(rb.gravity)),
                // Kinematic (1/0): transform-driven, never falls/pushed, but
                // pushes dynamic bodies. Assignable live (Dynamic ↔ Kinematic
                // — grab an object, ride an elevator). Static mode is
                // authoring-time (the Inspector dropdown; it's a baked
                // collider, not a body, so there's nothing here to toggle).
                ("kinematic".to_string(), b(rb.mode == floptle_core::BodyMode::Kinematic)),
                // Pushbox-only (1/0): the solver never resolves this body's
                // contacts — it integrates its velocity and nothing else. The
                // supported rollback profile; the script owns gravity, ground
                // and separation (`docs/rollback-netcode-design.md` §3).
                ("pushboxOnly".to_string(), b(rb.pushbox_only)),
                ("radius".to_string(), rb.radius as f64),
                ("height".to_string(), rb.height as f64),
                // Shape kind: 0 = sphere, 1 = capsule, 2 = box.
                ("shape".to_string(), match rb.kind {
                    floptle_core::BodyKind::Sphere => 0.0,
                    floptle_core::BodyKind::Capsule => 1.0,
                    floptle_core::BodyKind::Box => 2.0,
                }),
                ("half_x".to_string(), rb.half_extents[0] as f64),
                ("half_y".to_string(), rb.half_extents[1] as f64),
                ("half_z".to_string(), rb.half_extents[2] as f64),
                ("lock_x".to_string(), b(rb.lock_pos[0])),
                ("lock_y".to_string(), b(rb.lock_pos[1])),
                ("lock_z".to_string(), b(rb.lock_pos[2])),
                ("lock_rot_x".to_string(), b(rb.lock_rot[0])),
                ("lock_rot_y".to_string(), b(rb.lock_rot[1])),
                ("lock_rot_z".to_string(), b(rb.lock_rot[2])),
            ]),
        );
    }
    out
}

/// The component fields still stored in snake_case, so a camelCase write can be
/// recognised as naming one of them rather than inventing a new key. Everything
/// added since the camelCase convention landed is absent from this list by
/// construction — it only ever shrinks.
pub(crate) const LEGACY_SNAKE_FIELDS: &[&str] = &[
    "play_on_start",
    "half_x",
    "half_y",
    "half_z",
    "lock_x",
    "lock_y",
    "lock_z",
    "lock_rot_x",
    "lock_rot_y",
    "lock_rot_z",
];

/// camelCase → snake_case, for the handful of component fields that predate the
/// camelCase convention (`lock_rot_x`, `half_y`, `play_on_start`).
///
/// Both spellings work on a component handle: the mirror stays single-keyed (so
/// the animation recorder can't produce two tracks for one change) and the
/// camelCase name a script writes is normalised on the way in. Returns None when
/// the field has no uppercase letters — nothing to translate.
pub(crate) fn snake_of(field: &str) -> Option<String> {
    if !field.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    let mut out = String::with_capacity(field.len() + 3);
    for c in field.chars() {
        if c.is_ascii_uppercase() {
            out.push('_');
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Format a Lua number the way `tostring` would (integers without the `.0`).
pub(crate) fn format_lua_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

/// Which of four corners/edges a `radiusTL`/`borderB`-style field addresses.
/// `suffixes` is in the stored order — `[TL, TR, BR, BL]` for corners,
/// `[L, T, R, B]` for edges. An unmatched suffix falls back to 0 rather than
/// panicking; the callers only pass names they already matched.
fn quad_index(field: &str, suffixes: [&str; 4]) -> usize {
    suffixes.iter().position(|s| field.ends_with(s)).unwrap_or(0)
}

/// Which RGBA channel a `fillR`/`textG`/`tintA`-style field addresses.
fn rgba_index(field: &str) -> usize {
    match field.as_bytes().last() {
        Some(b'R') => 0,
        Some(b'G') => 1,
        Some(b'B') => 2,
        _ => 3,
    }
}

/// Apply a `node:getcomponent(name).field = value` write back to the ECS (mirror of
/// [`mirror_components`]). Unknown component/field names are ignored.
pub fn apply_component_field(world: &mut World, ent: Entity, comp: &str, field: &str, val: f64) {
    match comp {
        "UiElement" => {
            if let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent) {
                let v = val as f32;
                match field {
                    "visible" => spec.visible = val != 0.0,
                    "opacity" => spec.opacity = v.clamp(0.0, 1.0),
                    "posX" | "posY" => {
                        let i = usize::from(field == "posY");
                        match &mut spec.place {
                            floptle_ui::Place::Free { pos } => pos[i] = v,
                            floptle_ui::Place::Pin { offset, .. } => offset[i] = v,
                            floptle_ui::Place::Stretch { margin, .. } => margin[i] = v,
                        }
                    }
                    "width" | "height" => {
                        let i = usize::from(field == "height");
                        // Keep the axis's sizing mode; a Fit axis becomes Fixed.
                        spec.size[i] = match spec.size[i] {
                            floptle_ui::Size::Pct(_) => floptle_ui::Size::Pct(v),
                            floptle_ui::Size::Grow(_) => floptle_ui::Size::Grow(v),
                            _ => floptle_ui::Size::Fixed(v),
                        };
                    }
                    // Writing the bare name sets ALL four — "make this pill
                    // shaped" stays one line. The indexed names below reach a
                    // single corner or edge.
                    "radius" => {
                        if let Some(s) = &mut spec.shape {
                            s.radius = floptle_ui::Corners::all(v);
                        }
                    }
                    "border" => {
                        if let Some(s) = &mut spec.shape {
                            s.border = floptle_ui::Sides::all(v);
                        }
                    }
                    "radiusTL" | "radiusTR" | "radiusBR" | "radiusBL" => {
                        if let Some(s) = &mut spec.shape {
                            s.radius[quad_index(field, ["TL", "TR", "BR", "BL"])] = v;
                        }
                    }
                    "borderL" | "borderT" | "borderR" | "borderB" => {
                        if let Some(s) = &mut spec.shape {
                            s.border[quad_index(field, ["L", "T", "R", "B"])] = v;
                        }
                    }
                    "fillR" | "fillG" | "fillB" | "fillA" => {
                        if let Some(s) = &mut spec.shape {
                            s.fill[rgba_index(field)] = v;
                        }
                    }
                    "disabled" => spec.disabled = val != 0.0,
                    "selected" => spec.selected = val != 0.0,
                    "order" => spec.order = v.round() as i32,
                    "focusable" => spec.focusable = val != 0.0,
                    "rotation" => spec.rotation = v,
                    "scaleX" => spec.scale[0] = v,
                    "scaleY" => spec.scale[1] = v,
                    "groupR" | "groupG" | "groupB" | "groupA" => {
                        spec.tint[rgba_index(field)] = v;
                    }
                    "tracking" => {
                        if let Some(t) = &mut spec.text {
                            t.tracking = v;
                        }
                    }
                    "textSize" => {
                        if let Some(t) = &mut spec.text {
                            t.size = v;
                        }
                    }
                    "textR" | "textG" | "textB" | "textA" => {
                        if let Some(t) = &mut spec.text {
                            t.color[rgba_index(field)] = v;
                        }
                    }
                    "tintR" | "tintG" | "tintB" | "tintA" => {
                        if let Some(img) = &mut spec.image {
                            img.tint[rgba_index(field)] = v;
                        }
                    }
                    // Spritesheet cell — animate (stepped) for sprite animation.
                    "cell" => {
                        if let Some(img) = &mut spec.image {
                            img.cell = val.max(0.0) as u32;
                        }
                    }
                    // Scroll-view position (the wheel drives it too; the input
                    // pass clamps to the content every frame).
                    "scrollY" => {
                        if let Some(sc) = &mut spec.scroll {
                            sc.offset = v.max(0.0);
                        }
                    }
                    "scrollX" => {
                        if let Some(sc) = &mut spec.scroll {
                            sc.offset_x = v.max(0.0);
                        }
                    }
                    "toggle" => spec.toggle = val != 0.0,
                    "count" => {
                        if let Some(r) = &mut spec.repeater {
                            r.count = val.max(0.0) as u32;
                        }
                    }
                    _ => {}
                }
            }
        }
        "UiSlider" => {
            if let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent)
                && let Some(s) = &mut spec.slider
            {
                match field {
                    "value" => s.value = val as f32,
                    "min" => s.min = val as f32,
                    "max" => s.max = val as f32,
                    _ => {}
                }
            }
        }
        "Camera" => {
            if let Some(Matter::Camera { fov_y, active, .. }) = world.get_mut::<Matter>(ent) {
                match field {
                    "fovY" => *fov_y = (val as f32).clamp(0.05, 3.0),
                    "active" => *active = val != 0.0,
                    _ => {}
                }
            }
        }
        "UiLayer" => {
            if let Some(l) = world.get_mut::<floptle_ui::UiLayer>(ent) {
                match field {
                    "enabled" => l.enabled = val != 0.0,
                    "z" => l.z = val as i32,
                    "designHeight" => l.design_height = (val as f32).max(1.0),
                    "worldSpace" => {
                        l.space = if val != 0.0 {
                            floptle_ui::UiSpace::World
                        } else {
                            floptle_ui::UiSpace::Screen
                        };
                    }
                    _ => {}
                }
            }
        }
        "ParticleSystem" => {
            if let Some(ps) = world.get_mut::<ParticleSystem>(ent)
                && field == "play_on_start"
            {
                ps.play_on_start = val != 0.0;
            }
        }
        // The spritesheet frame — a plain uniform-cheap write, so a script can
        // step it every tick (`face:getcomponent("Material").cell = f`) and an
        // animation clip can key it on a stepped track.
        "Material" => {
            if let Some(m) = world.get_mut::<floptle_core::Material>(ent) {
                let n = val.max(0.0) as u32;
                match field {
                    "cell" => m.cell = n,
                    "sheetCols" => m.sheet_cols = n,
                    "sheetRows" => m.sheet_rows = n,
                    _ => {}
                }
            }
        }
        "PointLight" => {
            if let Some(Matter::PointLight { color, intensity, range }) = world.get_mut::<Matter>(ent) {
                match field {
                    "intensity" => *intensity = val as f32,
                    "range" => *range = val as f32,
                    "r" => color[0] = val as f32,
                    "g" => color[1] = val as f32,
                    "b" => color[2] = val as f32,
                    _ => {}
                }
            }
        }
        "AudioSource" => {
            if let Some(src) = world.get_mut::<floptle_audio::AudioSource>(ent) {
                let p = &mut src.params;
                match field {
                    "volume" => p.volume = (val as f32).clamp(0.0, 4.0),
                    "pitch" => p.pitch = (val as f32).clamp(0.05, 8.0),
                    "pan" => p.pan = (val as f32).clamp(-1.0, 1.0),
                    "minDistance" => p.min_distance = (val as f32).max(0.01),
                    "maxDistance" => p.max_distance = (val as f32).max(0.02),
                    "playOnStart" => src.play_on_start = val != 0.0,
                    "mode" => {
                        p.mode = match val as i64 {
                            1 => floptle_audio::SpatialMode::Distance,
                            2 => floptle_audio::SpatialMode::Flat,
                            _ => floptle_audio::SpatialMode::Spatial,
                        }
                    }
                    "falloff" => {
                        p.falloff = match val as i64 {
                            1 => floptle_audio::Falloff::Linear,
                            2 => floptle_audio::Falloff::Exponential,
                            _ => floptle_audio::Falloff::Inverse,
                        }
                    }
                    "endBehavior" => {
                        p.end = match val as i64 {
                            1 => floptle_audio::EndBehavior::Destroy,
                            2 => floptle_audio::EndBehavior::Loop,
                            _ => floptle_audio::EndBehavior::Stop,
                        }
                    }
                    _ => {}
                }
            }
        }
        "RigidBody" => {
            if let Some(rb) = world.get_mut::<RigidBody>(ent) {
                match field {
                    "friction" => rb.friction = val as f32,
                    "restitution" => rb.restitution = val as f32,
                    "gravity" => rb.gravity = val != 0.0,
                    // Live Dynamic ↔ Kinematic (the sim re-reads the mode each
                    // tick). Never touches a Static body — that's a baked
                    // collider with no live body to switch.
                    "kinematic" => {
                        if rb.mode != floptle_core::BodyMode::Static {
                            rb.mode = if val != 0.0 {
                                floptle_core::BodyMode::Kinematic
                            } else {
                                floptle_core::BodyMode::Dynamic
                            };
                        }
                    }
                    "pushboxOnly" => rb.pushbox_only = val != 0.0,
                    "radius" => rb.radius = val as f32,
                    "height" => rb.height = val as f32,
                    "shape" => {
                        rb.kind = match val as i64 {
                            0 => floptle_core::BodyKind::Sphere,
                            1 => floptle_core::BodyKind::Capsule,
                            _ => floptle_core::BodyKind::Box,
                        }
                    }
                    "half_x" => rb.half_extents[0] = val as f32,
                    "half_y" => rb.half_extents[1] = val as f32,
                    "half_z" => rb.half_extents[2] = val as f32,
                    "lock_x" => rb.lock_pos[0] = val != 0.0,
                    "lock_y" => rb.lock_pos[1] = val != 0.0,
                    "lock_z" => rb.lock_pos[2] = val != 0.0,
                    "lock_rot_x" => rb.lock_rot[0] = val != 0.0,
                    "lock_rot_y" => rb.lock_rot[1] = val != 0.0,
                    "lock_rot_z" => rb.lock_rot[2] = val != 0.0,
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// Apply the construction-API writes (`RichSet`) queued this pass: whole
/// component field-sets — the component is INSERTED with defaults when the
/// node doesn't carry it — and Matter swaps. Unknown field names apply
/// nothing (fail-soft, like the numeric component mirror).
pub(crate) fn apply_rich_sets(
    world: &mut World,
    ents: &std::collections::HashMap<u32, Entity>,
    sets: Vec<(u32, crate::RichSet)>,
) {
    use crate::{CompVal, RichSet};
    let num = |v: &CompVal| match v {
        CompVal::Num(n) => Some(*n),
        _ => None,
    };
    let v3 = |v: &CompVal| match v {
        CompVal::Vec3(a) => Some([a[0] as f32, a[1] as f32, a[2] as f32]),
        _ => None,
    };
    for (eid, set) in sets {
        let Some(&e) = ents.get(&eid) else { continue };
        match set {
            RichSet::Celestial(fields) => {
                if world.get::<floptle_core::CelestialBody>(e).is_none() {
                    world.insert(e, floptle_core::CelestialBody::default());
                }
                let Some(c) = world.get_mut::<floptle_core::CelestialBody>(e) else { continue };
                for (k, v) in &fields {
                    match k.as_str() {
                        "mu" => c.mu = num(v).unwrap_or(c.mu),
                        "bodyRadius" => c.body_radius = num(v).unwrap_or(c.body_radius),
                        "soi" => c.soi = num(v).unwrap_or(c.soi),
                        "parent" => {
                            if let CompVal::Str(p) = v {
                                c.parent = p.clone();
                            }
                        }
                        "a" => c.a = num(v).unwrap_or(c.a),
                        "e" => c.e = num(v).unwrap_or(c.e),
                        "i" => c.i = num(v).unwrap_or(c.i),
                        "lan" => c.lan = num(v).unwrap_or(c.lan),
                        "argPe" => c.arg_pe = num(v).unwrap_or(c.arg_pe),
                        "m0" => c.m0 = num(v).unwrap_or(c.m0),
                        "atmoColor" => c.atmo_color = v3(v).unwrap_or(c.atmo_color),
                        "atmoHeight" => c.atmo_height = num(v).unwrap_or(c.atmo_height),
                        "atmoDensity" => {
                            c.atmo_density = num(v).map(|n| n as f32).unwrap_or(c.atmo_density)
                        }
                        "clouds" => c.clouds = num(v).map(|n| n as f32).unwrap_or(c.clouds),
                        "luminosity" => {
                            c.luminosity = num(v).map(|n| n as f32).unwrap_or(c.luminosity)
                        }
                        "starColor" => c.star_color = v3(v).unwrap_or(c.star_color),
                        "occluderRadius" => {
                            c.occluder_radius = num(v).unwrap_or(c.occluder_radius)
                        }
                        _ => {}
                    }
                }
            }
            RichSet::Material(fields) => {
                if world.get::<floptle_core::Material>(e).is_none() {
                    world.insert(e, floptle_core::Material::default());
                }
                let Some(m) = world.get_mut::<floptle_core::Material>(e) else { continue };
                for (k, v) in &fields {
                    match k.as_str() {
                        "color" => m.color = v3(v).unwrap_or(m.color),
                        "emissive" => m.emissive = v3(v).unwrap_or(m.emissive),
                        "emissiveStrength" => {
                            m.emissive_strength =
                                num(v).map(|n| n as f32).unwrap_or(m.emissive_strength)
                        }
                        "specular" => m.specular = v3(v).unwrap_or(m.specular),
                        "shininess" => {
                            m.shininess = num(v).map(|n| n as f32).unwrap_or(m.shininess)
                        }
                        "specularStrength" => {
                            m.specular_strength =
                                num(v).map(|n| n as f32).unwrap_or(m.specular_strength)
                        }
                        "rim" => m.rim = v3(v).unwrap_or(m.rim),
                        "rimStrength" => {
                            m.rim_strength = num(v).map(|n| n as f32).unwrap_or(m.rim_strength)
                        }
                        "unlit" => m.unlit = num(v).map(|n| n != 0.0).unwrap_or(m.unlit),
                        "ambient" => m.ambient = num(v).map(|n| n as f32).unwrap_or(m.ambient),
                        "alpha" => m.alpha = num(v).map(|n| n as f32).unwrap_or(m.alpha),
                        "texture" => {
                            if let CompVal::Str(t) = v {
                                m.texture = (!t.is_empty()).then(|| t.clone());
                            }
                        }
                        // Spritesheet: slice the base texture into a grid and pick
                        // a cell. `sheetCols`/`sheetRows` are usually authored in
                        // the Inspector (inherited from the texture's own asset
                        // settings) and only `cell` moves at runtime.
                        "cell" => m.cell = num(v).unwrap_or(m.cell as f64).max(0.0) as u32,
                        "sheetCols" => {
                            m.sheet_cols = num(v).unwrap_or(m.sheet_cols as f64).max(0.0) as u32
                        }
                        "sheetRows" => {
                            m.sheet_rows = num(v).unwrap_or(m.sheet_rows as f64).max(0.0) as u32
                        }
                        _ => {}
                    }
                }
            }
            RichSet::MatterTerrain(id) => {
                world.insert(e, Matter::Terrain { id });
            }
            RichSet::TerrainGen(spec) => match spec {
                Some(s) => {
                    world.insert(e, floptle_core::TerrainGen(s));
                }
                None => {
                    world.remove::<floptle_core::TerrainGen>(e);
                }
            },
            RichSet::MatterPrimitive(shape, color) => {
                let shape = match shape.as_str() {
                    "Sphere" | "sphere" => floptle_core::Shape::Sphere,
                    "Capsule" | "capsule" => floptle_core::Shape::Capsule,
                    "Plane" | "plane" => floptle_core::Shape::Plane,
                    _ => floptle_core::Shape::Cube,
                };
                world.insert(
                    e,
                    Matter::Primitive {
                        shape,
                        color: [color[0] as f32, color[1] as f32, color[2] as f32],
                    },
                );
            }
            // 2D (`floptle/0058`). The sheet is the node's Material, so a
            // tilemap only ever carries its grid.
            RichSet::MatterTilemap { cols, rows, tile, mut data } => {
                let want = (cols as usize) * (rows as usize);
                // A short `data` fills the rest with holes rather than
                // repeating or refusing: a caller who sized the grid and then
                // filled part of it meant the rest to be empty.
                data.resize(want, floptle_core::EMPTY_TILE);
                data.truncate(want);
                world.insert(e, Matter::Tilemap { cols, rows, tile, data });
            }
            RichSet::TileCells(writes) => {
                let Some(Matter::Tilemap { cols, rows, data, .. }) =
                    world.get_mut::<Matter>(e)
                else {
                    continue;
                };
                let (cols, rows) = (*cols, *rows);
                for (x, y, cell) in writes {
                    // Out of bounds is a no-op, not a panic and not a wrap: a
                    // loop that runs one past the edge is a bug in the caller's
                    // bounds, and wrapping would silently paint the far side.
                    if x < cols && y < rows {
                        let i = (y * cols + x) as usize;
                        if let Some(slot) = data.get_mut(i) {
                            *slot = cell;
                        }
                    }
                }
            }
        }
    }
}

/// The handle `node:tilemap()` returns: read and write single squares.
///
/// Deliberately tiny. A game re-dresses a room per floor, so `set` and `fill`
/// are what it needs; anything richer belongs in Lua on top of these.
fn new_tilemap_handle(
    lua: &Lua,
    e: u32,
    q: crate::RichSetQueue,
    scene: std::rc::Rc<std::cell::RefCell<crate::SceneMirror>>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;

    // tm:set(x, y, cell) — 0-based from the TOP-LEFT, matching the data order.
    let qs = q.clone();
    t.raw_set(
        "set",
        lua.create_function(move |_, (this, x, y, cell): (Table, u32, u32, u32)| {
            let e: u32 = this.raw_get("__id")?;
            qs.borrow_mut().push((e, crate::RichSet::TileCells(vec![(x, y, cell)])));
            Ok(())
        })?,
    )?;

    // tm:fill(cell) — every square, including the empty ones.
    let qf = q.clone();
    let sf = scene.clone();
    t.raw_set(
        "fill",
        lua.create_function(move |_, (this, cell): (Table, u32)| {
            let e: u32 = this.raw_get("__id")?;
            let (cols, rows) = sf.borrow().tilemaps.get(&e).map(|(c, r, _)| (*c, *r)).unwrap_or((0, 0));
            let mut writes = Vec::with_capacity((cols * rows) as usize);
            for y in 0..rows {
                for x in 0..cols {
                    writes.push((x, y, cell));
                }
            }
            qf.borrow_mut().push((e, crate::RichSet::TileCells(writes)));
            Ok(())
        })?,
    )?;

    // tm:size() -> cols, rows
    let ss = scene.clone();
    t.raw_set(
        "size",
        lua.create_function(move |_, this: Table| {
            let e: u32 = this.raw_get("__id")?;
            let (c, r) = ss.borrow().tilemaps.get(&e).map(|(c, r, _)| (*c, *r)).unwrap_or((0, 0));
            Ok((c, r))
        })?,
    )?;

    // tm:get(x, y) -> cell, or nil outside the grid / on an empty square.
    t.raw_set(
        "get",
        lua.create_function(move |_, (this, x, y): (Table, u32, u32)| {
            let e: u32 = this.raw_get("__id")?;
            let s = scene.borrow();
            // Outside the grid reads as absent rather than wrapping round to
            // the far side: a loop that runs one past the edge is a bug in the
            // caller's bounds, and wrapping would hide it.
            let cell = s.tilemaps.get(&e).and_then(|(cols, rows, data)| {
                (x < *cols && y < *rows).then(|| data.get((y * cols + x) as usize).copied())?
            });
            match cell {
                Some(c) if c != floptle_core::EMPTY_TILE => Ok(Value::Integer(c as i64)),
                _ => Ok(Value::Nil),
            }
        })?,
    )?;
    Ok(t)
}

/// The handle `node:sprites()` returns.
///
/// One method, on purpose. `b:draw(...)` is IMMEDIATE MODE — the same contract
/// as `draw.*` and `gizmo.*`: what you draw this frame is what shows, and next
/// frame starts empty. There is nothing to allocate, nothing to pool, and no
/// `clear()` to forget on the frame a wave dies.
fn new_sprite_batch_handle(
    lua: &Lua,
    e: u32,
    draws: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<u32, Vec<floptle_core::Sprite>>>>,
) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.raw_set("__id", e)?;
    #[allow(clippy::type_complexity)]
    let f = lua.create_function(
        move |_,
              (this, x, y, z, scale, rot, cell, r, g, b, a): (
            Table,
            f32,
            f32,
            Option<f32>,
            Option<f32>,
            Option<f32>,
            Option<u32>,
            Option<f32>,
            Option<f32>,
            Option<f32>,
            Option<f32>,
        )| {
            let e: u32 = this.raw_get("__id")?;
            let sprite = floptle_core::Sprite {
                pos: [x, y, z.unwrap_or(0.0)],
                rot: rot.unwrap_or(0.0),
                scale: scale.unwrap_or(1.0),
                cell: cell.unwrap_or(0),
                // The tint defaults to white, so the common call is short and
                // a game only pays for colour where it wants colour.
                tint: [r.unwrap_or(1.0), g.unwrap_or(1.0), b.unwrap_or(1.0), a.unwrap_or(1.0)],
            };
            draws.borrow_mut().entry(e).or_default().push(sprite);
            Ok(())
        },
    )?;
    t.raw_set("draw", f)?;
    Ok(t)
}

/// Apply a STRING-valued component field — the string counterpart of
/// [`apply_component_field`], for path/text fields that a number can't express.
/// The headline use is animating a UI image's texture (sprite frame-swapping);
/// also covers a Material's texture and a text element's string. Used by the
/// animation system's property tracks (and available for future Lua setters).
pub fn apply_component_field_str(world: &mut World, ent: Entity, comp: &str, field: &str, val: &str) {
    match comp {
        "UiElement" => {
            if let Some(spec) = world.get_mut::<floptle_ui::ElementSpec>(ent) {
                match field {
                    // Swap the image's texture; create the image slot on demand
                    // so a track can turn a bare element into a sprite.
                    "image" | "texture" => match &mut spec.image {
                        Some(img) => img.texture = val.to_string(),
                        None => {
                            spec.image = Some(floptle_ui::ImageSpec {
                                texture: val.to_string(),
                                ..Default::default()
                            })
                        }
                    },
                    "text" => {
                        if let Some(t) = &mut spec.text {
                            t.text = val.to_string();
                        }
                    }
                    // Swap which named style paints this element — a row that
                    // becomes an error row, a button that turns primary.
                    "style" => spec.style = val.to_string(),
                    _ => {}
                }
            }
        }
        "Material" => {
            if field == "texture"
                && let Some(m) = world.get_mut::<floptle_core::Material>(ent)
            {
                m.texture = if val.is_empty() { None } else { Some(val.to_string()) };
            }
        }
        _ => {}
    }
}

/// Install the cross-node / cross-script reference layer into the Lua state: the `node`
/// and `script` metatables (transform/body access + hierarchy traversal + method/state
/// access) and the `find` / `findAll` / `findScript` globals. The handle closures share
/// the scene mirror + body bridges + env map via `shared`.
pub(crate) fn install_handle_api(lua: &Lua, shared: &Shared) -> mlua::Result<()> {
    // ---- node metatable -------------------------------------------------------------
    let node_mt = lua.create_table()?;
    {
        let scene = shared.scene.clone();
        let bodies = shared.bodies.clone();
        let body_changes = shared.body_changes.clone();
        let ui_text_changes = shared.ui_text_changes.clone();
        let ui_style_changes = shared.ui_style_changes.clone();
        let node_strs_r = shared.component_strs.clone();
        let ui_focus = shared.ui_focus.clone();
        let layer_changes = shared.layer_changes.clone();
        let enabled_changes = shared.enabled_changes.clone();
        let persistent_changes = shared.persistent_changes.clone();
        let tag_changes = shared.tag_changes.clone();
        let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            // `node.pos` — the position as a vec3 value. The script's OWN node
            // table carries live raw x/y/z (possibly written earlier this
            // hook), so prefer those; cross-node handles read the mirror.
            if key == "pos" {
                if let (Ok(x), Ok(y), Ok(z)) = (
                    this.raw_get::<f64>("x"),
                    this.raw_get::<f64>("y"),
                    this.raw_get::<f64>("z"),
                ) {
                    return Ok(Value::UserData(lua.create_userdata(
                        crate::math_api::LuaVec3(glam::DVec3::new(x, y, z)),
                    )?));
                }
                if let Some(tr) = scene.borrow().transforms.get(&e) {
                    return Ok(Value::UserData(
                        lua.create_userdata(crate::math_api::LuaVec3(tr.translation))?,
                    ));
                }
                return Ok(Value::Nil);
            }
            // `node.tickPos` / `node.tickX|Y|Z` — the BODY's pose at the start
            // of this tick, in absolute world coordinates.
            //
            // `x`/`y`/`z` are the *interpolated render pose* between ticks, so
            // building a hurtbox from them inside `fixedUpdate` is an
            // alpha-dependent read: frame-rate-dependent, and therefore
            // impossible for any replay to reproduce
            // (`docs/rollback-netcode-design.md` §3). The own-node table
            // carries live raw tick fields (possibly written earlier this
            // hook), so prefer those; a cross-node handle reads the body
            // bridge. Neither answers on a node with no body.
            if matches!(key.as_str(), "tickPos" | "tickX" | "tickY" | "tickZ") {
                let own = (
                    this.raw_get::<f64>("tickX"),
                    this.raw_get::<f64>("tickY"),
                    this.raw_get::<f64>("tickZ"),
                );
                let p = match own {
                    (Ok(x), Ok(y), Ok(z)) => Some([x, y, z]),
                    _ => bodies.borrow().get(&e).map(|b| b.pos),
                };
                let Some(p) = p else { return Ok(Value::Nil) };
                return Ok(match key.as_str() {
                    "tickX" => Value::Number(p[0]),
                    "tickY" => Value::Number(p[1]),
                    "tickZ" => Value::Number(p[2]),
                    _ => Value::UserData(lua.create_userdata(crate::math_api::LuaVec3(
                        glam::DVec3::new(p[0], p[1], p[2]),
                    ))?),
                });
            }
            // Transform reads.
            {
                let s = scene.borrow();
                // `node.worldX/worldY/worldZ` / `node.worldPos` — the position in
                // WORLD space, composed up the parent chain. Read-only, and the
                // answer to a whole class of "my unit walked off forever": x/y/z
                // are LOCAL, so a script that compares a node under a moved
                // parent against a world-space target never arrives.
                if matches!(key.as_str(), "worldX" | "worldY" | "worldZ" | "worldPos") {
                    if !s.transforms.contains_key(&e) {
                        return Ok(Value::Nil);
                    }
                    // Live local position when this is the script's own node, so
                    // `node.pos = p` is visible to `node.worldX` on the very next
                    // line rather than one hook later.
                    let w = world_transform_of_handle(&s, &this, e).translation;
                    return Ok(match key.as_str() {
                        "worldX" => Value::Number(w.x),
                        "worldY" => Value::Number(w.y),
                        "worldZ" => Value::Number(w.z),
                        _ => Value::UserData(
                            lua.create_userdata(crate::math_api::LuaVec3(w))?,
                        ),
                    });
                }
                if let Some(tr) = s.transforms.get(&e) {
                    match key.as_str() {
                        "x" => return Ok(Value::Number(tr.translation.x)),
                        "y" => return Ok(Value::Number(tr.translation.y)),
                        "z" => return Ok(Value::Number(tr.translation.z)),
                        "scale" | "scale_x" | "scaleX" => {
                            return Ok(Value::Number(tr.scale.x as f64));
                        }
                        "scale_y" | "scaleY" => return Ok(Value::Number(tr.scale.y as f64)),
                        "scale_z" | "scaleZ" => return Ok(Value::Number(tr.scale.z as f64)),
                        // `node.size` — the whole scale as a vec3, for the
                        // non-uniform case (`node.scale` stays the uniform
                        // shortcut it has always been).
                        "size" => {
                            return Ok(Value::UserData(lua.create_userdata(
                                crate::math_api::LuaVec3(glam::DVec3::new(
                                    tr.scale.x as f64,
                                    tr.scale.y as f64,
                                    tr.scale.z as f64,
                                )),
                            )?));
                        }
                        "yaw" | "pitch" | "roll" => {
                            let (y, p, r) = tr.rotation.to_euler(EulerRot::YXZ);
                            let v = match key.as_str() {
                                "yaw" => y,
                                "pitch" => p,
                                _ => r,
                            };
                            return Ok(Value::Number(v as f64));
                        }
                        _ => {}
                    }
                }
            }
            // Identity / hierarchy fields.
            match key.as_str() {
                "id" => return Ok(Value::Integer(e as i64)),
                "name" => {
                    let n = scene.borrow().names.get(&e).cloned();
                    return Ok(match n {
                        Some(n) => Value::String(lua.create_string(&n)?),
                        None => Value::Nil,
                    });
                }
                "valid" => return Ok(Value::Boolean(scene.borrow().transforms.contains_key(&e))),
                "parent" => {
                    let p = scene.borrow().parent.get(&e).copied();
                    return Ok(match p {
                        Some(p) => Value::Table(new_node_handle(lua, p)?),
                        None => Value::Nil,
                    });
                }
                // The mesh node's current model path (nil on non-mesh nodes). Assigning it
                // (see __newindex) swaps the model at runtime.
                "model" => {
                    let m = scene.borrow().models.get(&e).cloned();
                    return Ok(match m {
                        Some(p) => Value::String(lua.create_string(&p)?),
                        None => Value::Nil,
                    });
                }
                // Whether the node's geometry is drawn (true unless explicitly hidden).
                "visible" => {
                    let v = scene.borrow().visible.get(&e).copied().unwrap_or(true);
                    return Ok(Value::Boolean(v));
                }
                // Read-your-writes within the frame, then the scene mirror.
                "enabled" => {
                    let v = enabled_changes
                        .borrow()
                        .get(&e)
                        .copied()
                        .unwrap_or_else(|| !scene.borrow().disabled.contains(&e));
                    return Ok(Value::Boolean(v));
                }
                // Whether the node survives a scene swap (read-your-writes, as
                // above). Reports what was SET on this node — the subtree rule
                // means a child of a persistent folder also survives, but it is
                // the folder that carries the flag.
                "persistent" => {
                    let v = persistent_changes
                        .borrow()
                        .get(&e)
                        .copied()
                        .unwrap_or_else(|| scene.borrow().persistent.contains(&e));
                    return Ok(Value::Boolean(v));
                }
                // The node's collision/query layer, by name ("Default" when unset) —
                // read-your-writes within the frame via the pending-changes map.
                "layer" => {
                    let l = layer_changes
                        .borrow()
                        .get(&e)
                        .cloned()
                        .or_else(|| scene.borrow().layers.get(&e).cloned())
                        .unwrap_or_else(|| floptle_core::layers::DEFAULT_LAYER.to_string());
                    return Ok(Value::String(lua.create_string(&l)?));
                }
                // The node's tags as a fresh array table (possibly empty) —
                // read-your-writes via the pending map, like `layer`.
                "tags" => {
                    let tags = tag_changes
                        .borrow()
                        .get(&e)
                        .cloned()
                        .or_else(|| scene.borrow().tags.get(&e).cloned())
                        .unwrap_or_default();
                    let arr = lua.create_table()?;
                    for (i, t) in tags.iter().enumerate() {
                        arr.set(i + 1, lua.create_string(t)?)?;
                    }
                    return Ok(Value::Table(arr));
                }
                // A UI element's text (nil on non-text nodes). Assigning it (see
                // __newindex) changes what the label shows — read-your-writes within
                // the frame via the pending-changes map.
                "text" => {
                    let t = ui_text_changes
                        .borrow()
                        .get(&e)
                        .cloned()
                        .or_else(|| scene.borrow().ui_texts.get(&e).cloned());
                    return Ok(match t {
                        Some(t) => Value::String(lua.create_string(&t)?),
                        None => Value::Nil,
                    });
                }
                // A UI image's texture path (nil on elements with no image).
                // Readable as well as writable, so `node.texture` behaves like
                // `node.text` rather than being a write-only corner.
                "texture" => {
                    let t = node_strs_r
                        .borrow()
                        .get(&(e, "UiElement".to_string(), "texture".to_string()))
                        .cloned()
                        .or_else(|| scene.borrow().ui_textures.get(&e).cloned());
                    return Ok(match t {
                        Some(t) => Value::String(lua.create_string(&t)?),
                        None => Value::Nil,
                    });
                }
                // The element's style name. Same read-your-writes rule as
                // `text`: a write earlier this frame reads back before the
                // flush to the ECS.
                "style" => {
                    let s = ui_style_changes
                        .borrow()
                        .get(&e)
                        .cloned()
                        .or_else(|| scene.borrow().ui_styles.get(&e).cloned());
                    return Ok(match s {
                        Some(s) => Value::String(lua.create_string(&s)?),
                        None => Value::Nil,
                    });
                }
                // Is the keyboard/gamepad ring on this element right now? Read
                // only — moving focus is `ui.focus(node)`, so there is exactly
                // one place that can change it and one place to look for bugs.
                "focused" => return Ok(Value::Boolean(*ui_focus.borrow() == Some(e))),
                // Which row of a repeater this is, 0-based. `nil` on anything
                // a repeater didn't spawn — so `if node.index then` is a
                // perfectly good "am I a row".
                "index" => {
                    return Ok(match scene.borrow().repeat_index.get(&e) {
                        Some(i) => Value::Integer(i64::from(*i)),
                        None => Value::Nil,
                    });
                }
                _ => {}
            }
            // Physics body fields.
            match key.as_str() {
                "vx" | "vy" | "vz" => {
                    let vel = body_changes
                        .borrow()
                        .get(&e)
                        .copied()
                        .or_else(|| bodies.borrow().get(&e).map(|b| b.vel));
                    return Ok(match vel {
                        Some(v) => Value::Number(match key.as_str() {
                            "vx" => v[0],
                            "vy" => v[1],
                            _ => v[2],
                        } as f64),
                        None => Value::Nil,
                    });
                }
                "up_x" | "up_y" | "up_z" | "upX" | "upY" | "upZ" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::Number(match key.as_str() {
                            "up_x" | "upX" => b.up[0],
                            "up_y" | "upY" => b.up[1],
                            _ => b.up[2],
                        } as f64),
                        None => Value::Nil,
                    });
                }
                // ---- the VECTOR reads ------------------------------------
                // `node.vel`, `node.up`, `node.forward`, `node.right`: the same
                // state the scalar fields above expose, as one vec3 each — so a
                // controller writes `node.vel = node.vel + up * jump` instead of
                // three lines and a hand-rolled `norm(x, y, z)`.
                "vel" => {
                    let vel = body_changes
                        .borrow()
                        .get(&e)
                        .copied()
                        .or_else(|| bodies.borrow().get(&e).map(|b| b.vel));
                    return Ok(match vel {
                        Some(v) => Value::UserData(lua.create_userdata(
                            crate::math_api::LuaVec3(glam::DVec3::new(
                                v[0] as f64,
                                v[1] as f64,
                                v[2] as f64,
                            )),
                        )?),
                        None => Value::Nil,
                    });
                }
                "up" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::UserData(lua.create_userdata(
                            crate::math_api::LuaVec3(glam::DVec3::new(
                                b.up[0] as f64,
                                b.up[1] as f64,
                                b.up[2] as f64,
                            )),
                        )?),
                        None => Value::Nil,
                    });
                }
                // What the body is touching: the floor under it, and the
                // steepest thing it is pressed against. `nil` when there is no
                // such surface this step — `if node.wallNormal then` is the
                // whole test. A controller uses the second one to stop pushing
                // into a cliff, which is what otherwise fires it into the sky.
                "groundNormal" | "wallNormal" => {
                    let n = bodies.borrow().get(&e).and_then(|b| {
                        if key == "groundNormal" { b.ground_normal } else { b.wall_normal }
                    });
                    return Ok(match n {
                        Some(v) => Value::UserData(lua.create_userdata(
                            crate::math_api::LuaVec3(glam::DVec3::new(
                                v[0] as f64,
                                v[1] as f64,
                                v[2] as f64,
                            )),
                        )?),
                        None => Value::Nil,
                    });
                }
                // Facing, from the node's ROTATION (not the body) so it answers
                // on anything with a transform. −Z forward matches the camera
                // convention (`floptle_render::camera`), +X right, +Y local up.
                "forward" | "right" | "localUp" => {
                    let rot = scene.borrow().transforms.get(&e).map(|t| t.rotation);
                    return Ok(match rot {
                        Some(r) => {
                            let v = match key.as_str() {
                                "forward" => r * glam::Vec3::NEG_Z,
                                "right" => r * glam::Vec3::X,
                                _ => r * glam::Vec3::Y,
                            };
                            Value::UserData(lua.create_userdata(crate::math_api::LuaVec3(
                                glam::DVec3::new(v.x as f64, v.y as f64, v.z as f64),
                            ))?)
                        }
                        None => Value::Nil,
                    });
                }
                "grounded" => {
                    return Ok(Value::Boolean(
                        bodies.borrow().get(&e).map(|b| b.grounded).unwrap_or(false),
                    ));
                }
                "height" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::Number(b.height as f64),
                        None => Value::Nil,
                    });
                }
                _ => {}
            }
            // Otherwise a method (children / getchild / getscript / find …) or nil.
            let methods: Table = lua.named_registry_value("floptle_node_methods")?;
            methods.get::<Value>(key)
        })?;
        node_mt.set("__index", idx)?;
    }
    {
        let scene = shared.scene.clone();
        let bodies = shared.bodies.clone();
        let body_changes = shared.body_changes.clone();
        let body_height = shared.body_height_changes.clone();
        let body_pos = shared.body_pos_changes.clone();
        let model_changes = shared.model_changes.clone();
        let material_changes = shared.material_changes.clone();
        let visible_changes = shared.visible_changes.clone();
        let enabled_changes = shared.enabled_changes.clone();
        let persistent_changes = shared.persistent_changes.clone();
        let layer_changes = shared.layer_changes.clone();
        let tag_changes = shared.tag_changes.clone();
        let layer_table = shared.layer_table.clone();
        let ui_text_changes = shared.ui_text_changes.clone();
        let ui_style_changes = shared.ui_style_changes.clone();
        let node_strs = shared.component_strs.clone();
        let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
            let e: u32 = this.raw_get("__id")?;
            // `node.pos = vec3(...)` (or any {x=,y=,z=} / node) — the own-node
            // table writes its live raw fields (the normal read-back path);
            // cross-node handles write the mirror.
            if key == "pos" {
                let Some(v) = crate::math_api::vec3_of(&val) else {
                    return Err(mlua::Error::RuntimeError(
                        "node.pos takes a vec3 (or anything with x/y/z)".into(),
                    ));
                };
                let own = this.raw_get::<f64>("x").is_ok();
                if own {
                    this.raw_set("x", v.x)?;
                    this.raw_set("y", v.y)?;
                    this.raw_set("z", v.z)?;
                } else {
                    let mut s = scene.borrow_mut();
                    if let Some(tr) = s.transforms.get_mut(&e) {
                        tr.translation = v;
                        s.dirty.insert(e);
                        // A body node: the physics writeback would stomp this —
                        // queue a real TELEPORT for the driver.
                        if bodies.borrow().contains_key(&e) {
                            body_pos.borrow_mut().insert(e, [v.x, v.y, v.z]);
                        }
                    }
                }
                return Ok(());
            }
            // `node.tickPos = vec3(...)` / `node.tickX = n` — move the BODY in
            // the tick channel, without touching the render transform. The
            // transform would be overwritten by the interpolated writeback
            // anyway, which is what makes `node.x = node.x + d` inside
            // fixedUpdate teleport a fighter back onto its visual position:
            // the classic "the visuals take the knockback, the hitbox stays
            // put" bug (`docs/rollback-netcode-design.md` §3).
            if matches!(key.as_str(), "tickPos" | "tickX" | "tickY" | "tickZ") {
                let own = this.raw_get::<f64>("tickX").is_ok();
                let mut p = match (
                    this.raw_get::<f64>("tickX"),
                    this.raw_get::<f64>("tickY"),
                    this.raw_get::<f64>("tickZ"),
                ) {
                    (Ok(x), Ok(y), Ok(z)) => [x, y, z],
                    _ => match bodies.borrow().get(&e) {
                        Some(b) => b.pos,
                        // No body means no tick channel; a silent no-op here
                        // would look exactly like a working teleport.
                        None => {
                            return Err(mlua::Error::RuntimeError(
                                "node.tickPos is the physics body's tick pose — this node has \
                                 no RigidBody. Use node.pos for a plain transform move."
                                    .into(),
                            ))
                        }
                    },
                };
                match key.as_str() {
                    "tickX" => p[0] = as_num(&val).unwrap_or(p[0]),
                    "tickY" => p[1] = as_num(&val).unwrap_or(p[1]),
                    "tickZ" => p[2] = as_num(&val).unwrap_or(p[2]),
                    _ => {
                        let Some(v) = crate::math_api::vec3_of(&val) else {
                            return Err(mlua::Error::RuntimeError(
                                "node.tickPos takes a vec3 (or anything with x/y/z)".into(),
                            ));
                        };
                        p = [v.x, v.y, v.z];
                    }
                }
                if own {
                    // The own-node read-back picks these up after the hook,
                    // alongside every other body write.
                    this.raw_set("tickX", p[0])?;
                    this.raw_set("tickY", p[1])?;
                    this.raw_set("tickZ", p[2])?;
                } else {
                    body_pos.borrow_mut().insert(e, p);
                }
                return Ok(());
            }
            // Transform writes.
            {
                let mut s = scene.borrow_mut();
                if let Some(tr) = s.transforms.get_mut(&e) {
                    let mut handled = true;
                    match key.as_str() {
                        "x" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.x = n;
                            }
                        }
                        "y" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.y = n;
                            }
                        }
                        "z" => {
                            if let Some(n) = as_num(&val) {
                                tr.translation.z = n;
                            }
                        }
                        // A number splats (the classic form); a vec3 sets each
                        // axis, so `node.scale = vec3(2, 1, 1)` no longer needs
                        // three statements. `node.size` is the same setter.
                        "scale" | "size" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale = Vec3::splat(n as f32);
                            } else if let Some(v) = crate::math_api::vec3_of(&val) {
                                tr.scale = Vec3::new(v.x as f32, v.y as f32, v.z as f32);
                            }
                        }
                        "scale_x" | "scaleX" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.x = n as f32;
                            }
                        }
                        "scale_y" | "scaleY" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.y = n as f32;
                            }
                        }
                        "scale_z" | "scaleZ" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.z = n as f32;
                            }
                        }
                        "yaw" | "pitch" | "roll" => {
                            if let Some(n) = as_num(&val) {
                                let (mut y, mut p, mut r) = tr.rotation.to_euler(EulerRot::YXZ);
                                let changed = match key.as_str() {
                                    "yaw" => n != y as f64,
                                    "pitch" => n != p as f64,
                                    _ => n != r as f64,
                                };
                                if changed {
                                    match key.as_str() {
                                        "yaw" => y = n as f32,
                                        "pitch" => p = n as f32,
                                        _ => r = n as f32,
                                    }
                                    tr.rotation = Quat::from_euler(EulerRot::YXZ, y, p, r);
                                }
                            }
                        }
                        _ => handled = false,
                    }
                    if handled {
                        // Position writes on a BODY node also teleport the body
                        // (the writeback would revert the transform otherwise).
                        if matches!(key.as_str(), "x" | "y" | "z")
                            && bodies.borrow().contains_key(&e)
                        {
                            let t = tr.translation;
                            body_pos.borrow_mut().insert(e, [t.x, t.y, t.z]);
                        }
                        s.dirty.insert(e);
                        return Ok(());
                    }
                }
            }
            // Physics body writes.
            match key.as_str() {
                "vx" | "vy" | "vz" => {
                    if let Some(n) = as_num(&val) {
                        let mut bc = body_changes.borrow_mut();
                        let mut v = bc
                            .get(&e)
                            .copied()
                            .or_else(|| bodies.borrow().get(&e).map(|b| b.vel))
                            .unwrap_or([0.0; 3]);
                        match key.as_str() {
                            "vx" => v[0] = n as f32,
                            "vy" => v[1] = n as f32,
                            _ => v[2] = n as f32,
                        }
                        bc.insert(e, v);
                    }
                    return Ok(());
                }
                // `node.vel = vec3(...)` — the whole velocity in one write (or
                // anything with x/y/z, so `node.vel = other.vel` works).
                "vel" => {
                    let Some(v) = crate::math_api::vec3_of(&val) else {
                        return Err(mlua::Error::RuntimeError(
                            "node.vel takes a vec3 (or anything with x/y/z)".into(),
                        ));
                    };
                    body_changes.borrow_mut().insert(e, [v.x as f32, v.y as f32, v.z as f32]);
                    return Ok(());
                }
                "height" => {
                    if let Some(n) = as_num(&val) {
                        body_height.borrow_mut().insert(e, n as f32);
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Component swaps (applied to the ECS at the end of `run`): the mesh model path
            // and a material (preset name or `assets.getFile("materials/X.ron")`).
            match key.as_str() {
                "model" => {
                    if let Value::String(s) = &val {
                        model_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                    }
                    return Ok(());
                }
                "material" => {
                    if let Value::String(s) = &val {
                        material_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                    }
                    return Ok(());
                }
                "visible" => {
                    if let Value::Boolean(b) = val {
                        visible_changes.borrow_mut().insert(e, b);
                    }
                    return Ok(());
                }
                // Switch the node — and everything under it — off or on. Stronger than
                // `visible`, which only stops the draw: this also takes the node out of
                // physics and stops its scripts. A node cannot re-enable ITSELF (its
                // scripts aren't running); something else has to.
                "enabled" => {
                    if let Value::Boolean(b) = val {
                        enabled_changes.borrow_mut().insert(e, b);
                    }
                    return Ok(());
                }
                // Carry the node — and everything under it — across a scene
                // swap: the DontDestroyOnLoad equivalent. Its scripts keep
                // running rather than re-`start`ing, because the node never
                // stopped existing.
                "persistent" => {
                    let Value::Boolean(b) = val else {
                        return Err(mlua::Error::RuntimeError(
                            "node.persistent takes a boolean".into(),
                        ));
                    };
                    persistent_changes.borrow_mut().insert(e, b);
                    return Ok(());
                }
                // `node.layer = "Enemies"` — validated against the project's
                // layer table NOW, so a typo errors at the assignment (never a
                // silently-Default node). Applied to the ECS after the pass;
                // a dynamic body re-resolves its bit next frame (live).
                "layer" => {
                    let Value::String(s) = &val else {
                        return Err(mlua::Error::RuntimeError(
                            "node.layer takes a layer name (a string)".into(),
                        ));
                    };
                    let name = s.to_string_lossy().to_string();
                    let lt = layer_table.borrow();
                    if lt.index_of(&name).is_none() {
                        return Err(mlua::Error::RuntimeError(format!(
                            "no layer named '{name}' (project layers: {})",
                            lt.names.join(", ")
                        )));
                    }
                    drop(lt);
                    layer_changes.borrow_mut().insert(e, name);
                    return Ok(());
                }
                // `node.tags = {"enemy", "boss"}` — replace the whole list
                // (use node:addTag / node:removeTag for single edits).
                "tags" => {
                    let Value::Table(t) = &val else {
                        return Err(mlua::Error::RuntimeError(
                            "node.tags takes an array of strings".into(),
                        ));
                    };
                    let mut tags: Vec<String> = Vec::new();
                    for v in t.sequence_values::<String>() {
                        let v = v?;
                        if !tags.contains(&v) {
                            tags.push(v);
                        }
                    }
                    tag_changes.borrow_mut().insert(e, tags);
                    return Ok(());
                }
                // Which named style paints this element ("" = none).
                "style" => {
                    if let Value::String(s) = &val {
                        ui_style_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                    }
                    return Ok(());
                }
                // `node.texture = "textures/ui/portrait.png"` — the UI image's
                // texture, creating the image slot if the element has none, so
                // a bare element can become a sprite. Raises on a non-string
                // rather than dropping it: this write did NOTHING for months and
                // nobody could tell, which is the whole of floptle/0052.
                "texture" => {
                    let Value::String(s) = &val else {
                        return Err(mlua::Error::RuntimeError(
                            "node.texture takes an asset path (a string)".into(),
                        ));
                    };
                    node_strs.borrow_mut().insert(
                        (e, "UiElement".to_string(), "texture".to_string()),
                        s.to_string_lossy().to_string(),
                    );
                    return Ok(());
                }
                // UI element text: numbers coerce (hp counters write numbers directly).
                "text" => {
                    match &val {
                        Value::String(s) => {
                            ui_text_changes.borrow_mut().insert(e, s.to_string_lossy().to_string());
                        }
                        Value::Number(n) => {
                            ui_text_changes.borrow_mut().insert(e, format_lua_number(*n));
                        }
                        Value::Integer(n) => {
                            ui_text_changes.borrow_mut().insert(e, n.to_string());
                        }
                        _ => {}
                    }
                    return Ok(());
                }
                _ => {}
            }
            // Unknown key: stash it on the handle table (harmless; lets scripts tag nodes).
            this.raw_set(key, val)?;
            Ok(())
        })?;
        node_mt.set("__newindex", newidx)?;
    }
    lua.set_named_registry_value("floptle_node_mt", node_mt)?;

    // ---- component handle metatable (node:getcomponent) -----------------------------
    // A component handle reads its numeric fields from the mirror (or this frame's pending
    // writes) and records assignments; the writes are flushed to the ECS after `run`.
    {
        let comp_mt = lua.create_table()?;
        {
            let scene = shared.scene.clone();
            let changes = shared.component_changes.clone();
            let colors = shared.component_colors.clone();
            let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                // Colours first: a colour field never has a numeric twin.
                if let Some(c) = colors.borrow().get(&(e, comp.clone(), key.clone())) {
                    return Ok(Value::Table(new_color(lua, *c)?));
                }
                let s = scene.borrow();
                if let Some(c) =
                    s.component_colors.get(&e).and_then(|m| m.get(&comp)).and_then(|m| m.get(&key))
                {
                    return Ok(Value::Table(new_color(lua, *c)?));
                }
                // Booleans read back as booleans, because 0 is truthy in Lua
                // and `if el.visible then` was always taken.
                let wrap = |v: f64| {
                    if is_bool_field(&comp, &key) {
                        Value::Boolean(v != 0.0)
                    } else {
                        Value::Number(v)
                    }
                };
                if let Some(v) = changes.borrow().get(&(e, comp.clone(), key.clone())) {
                    return Ok(wrap(*v));
                }
                if let Some(v) =
                    s.components.get(&e).and_then(|c| c.get(&comp)).and_then(|m| m.get(&key))
                {
                    return Ok(wrap(*v));
                }
                // `rb.lockRotX` → the mirror's `lock_rot_x`: the camelCase
                // spelling the docs teach, over the snake_case names a few
                // components still store.
                if let Some(alt) = snake_of(&key) {
                    if let Some(v) = changes.borrow().get(&(e, comp.clone(), alt.clone())) {
                        return Ok(wrap(*v));
                    }
                    if let Some(v) =
                        s.components.get(&e).and_then(|c| c.get(&comp)).and_then(|m| m.get(&alt))
                    {
                        return Ok(wrap(*v));
                    }
                }
                Ok(Value::Nil)
            })?;
            comp_mt.set("__index", idx)?;
        }
        {
            let changes = shared.component_changes.clone();
            let colors = shared.component_colors.clone();
            let strs = shared.component_strs.clone();
            let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                // A table is a colour: `e.fill = color(1, 0.85, 0.35)`, or any
                // `{r,g,b,a}` / `{1,0,0}` table, so a palette read out of a
                // save file works without a conversion step.
                // A camelCase spelling of a legacy snake_case field writes the
                // field it names (see `snake_of`), so the mirror keeps ONE key
                // per field. An unknown camelCase name is left alone — its
                // "unknown field" behaviour is unchanged.
                let key = snake_of(&key)
                    .filter(|alt| LEGACY_SNAKE_FIELDS.contains(&alt.as_str()))
                    .unwrap_or(key);
                if let Value::Table(t) = &val {
                    let c = read_color(t)?;
                    colors.borrow_mut().insert((e, comp, key), c);
                    return Ok(());
                }
                // A string is a path or a label: a UI image's texture, a
                // Material's texture, a text element's string. This used to
                // raise "must be a number, a boolean or a color", which was the
                // one path that failed LOUDLY and it pointed nowhere useful
                // (floptle/0052).
                if let Value::String(s) = &val {
                    strs.borrow_mut().insert((e, comp, key), s.to_string_lossy().to_string());
                    return Ok(());
                }
                let n = match val {
                    Value::Number(n) => n,
                    Value::Integer(n) => n as f64,
                    Value::Boolean(b) => f64::from(u8::from(b)),
                    _ => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "component field '{key}' must be a number, a boolean, a string or a \
                             color"
                        )));
                    }
                };
                changes.borrow_mut().insert((e, comp, key), n);
                Ok(())
            })?;
            comp_mt.set("__newindex", newidx)?;
        }
        lua.set_named_registry_value("floptle_component_mt", comp_mt)?;
    }

    // ---- node methods (children / getchild / getparent / getscript / find) ----------
    let methods = lua.create_table()?;
    {
        let scene = shared.scene.clone();
        methods.set(
            "children",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let kids = scene.borrow().children.get(&e).cloned().unwrap_or_default();
                let arr = lua.create_table()?;
                for (i, c) in kids.iter().enumerate() {
                    arr.set(i + 1, new_node_handle(lua, *c)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let found = {
                let s = scene.borrow();
                s.children
                    .get(&e)
                    .into_iter()
                    .flatten()
                    .copied()
                    .find(|c| s.names.get(c).map(|n| n == &name).unwrap_or(false))
            };
            Ok(match found {
                Some(c) => Value::Table(new_node_handle(lua, c)?),
                None => Value::Nil,
            })
        })?;
        methods.set("child", f.clone())?;
        methods.set("getchild", f)?;
    }
    {
        let scene = shared.scene.clone();
        methods.set(
            "getparent",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let p = scene.borrow().parent.get(&e).copied();
                Ok(match p {
                    Some(p) => Value::Table(new_node_handle(lua, p)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let has = scene
                .borrow()
                .scripts
                .get(&e)
                .map(|v| v.iter().any(|k| k == &name))
                .unwrap_or(false);
            Ok(if has {
                Value::Table(new_script_handle(lua, e, &name)?)
            } else {
                Value::Nil
            })
        })?;
        methods.set("script", f.clone())?;
        methods.set("getscript", f)?;
    }
    // node:getcomponent("PointLight" | "RigidBody") → a component handle whose numeric
    // fields you can read + assign (writes flush to the ECS after the frame), or nil if the
    // node has no such component.
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, name): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let has =
                scene.borrow().components.get(&e).map(|c| c.contains_key(&name)).unwrap_or(false);
            Ok(if has {
                Value::Table(new_component_handle(lua, e, &name)?)
            } else {
                Value::Nil
            })
        })?;
        methods.set("component", f.clone())?;
        methods.set("getcomponent", f)?;
    }
    // node:uiRect() -> x, y, w, h — this UI element's SOLVED screen rect in
    // WINDOW physical pixels: the same space input.mouse() reports and
    // camera.worldToScreen() returns, so a docked editor Game tab's rects carry
    // that tab's offset. 0,0,0,0 when it has no screen-space rect this frame.
    // Lets a script hit-test the cursor against a panel's ACTUAL rendered
    // position instead of guessing its geometry.
    {
        let ui_rects = shared.ui_rects.clone();
        methods.set(
            "uiRect",
            lua.create_function(move |_, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                match ui_rects.borrow().get(&e).copied() {
                    Some(r) => Ok((r[0], r[1], r[2], r[3])),
                    None => Ok((0.0f32, 0.0, 0.0, 0.0)),
                }
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        methods.set(
            "find",
            lua.create_function(move |lua, (this, name): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let found = {
                    let s = scene.borrow();
                    let mut stack: Vec<u32> =
                        s.children.get(&e).cloned().unwrap_or_default();
                    let mut hit = None;
                    while let Some(c) = stack.pop() {
                        if s.names.get(&c).map(|n| n == &name).unwrap_or(false) {
                            hit = Some(c);
                            break;
                        }
                        if let Some(cc) = s.children.get(&c) {
                            stack.extend(cc.iter().copied());
                        }
                    }
                    hit
                };
                Ok(match found {
                    Some(c) => Value::Table(new_node_handle(lua, c)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    // ---- the construction API (editor actions / procgen scripts) ----------
    // node:setCelestial{mu=..., bodyRadius=..., parent="Sun", atmoColor={r,g,b}, ...}
    // node:setMaterial{color={..}, emissive={..}, emissiveStrength=2, unlit=true, texture="..."}
    // node:setTerrain(id)   node:setPrimitive("Sphere" [, {r,g,b}])
    // All queued as RichSet writes; the component is inserted (defaults) if
    // the node doesn't have it. Field names are the Lua-facing camelCase.
    {
        let q = shared.rich_sets.clone();
        // A 3-vector in ANY of the Lua spellings: vec3(..), {x=,y=,z=}, {r,g,b}.
        /// Every spelling of a three-component value the docs promise: a `vec3`, an
        /// `{x=,y=,z=}` table, an array `{1,2,3}`, and — for colours, which is what most
        /// of these fields are — `{r=,g=,b=}`. `{r,g,b}` was documented in `floptle.lua`
        /// and named in this function's own error message while being the one shape it
        /// refused (floptle/0025).
        fn triple_of(v: &Value) -> Option<[f64; 3]> {
            if let Some(p) = crate::math_api::vec3_of(v) {
                return Some([p.x, p.y, p.z]);
            }
            if let Value::Table(t) = v {
                if let (Ok(Some(r)), Ok(Some(g)), Ok(Some(b))) = (
                    t.get::<Option<f64>>("r"),
                    t.get::<Option<f64>>("g"),
                    t.get::<Option<f64>>("b"),
                ) {
                    // Alpha is accepted and dropped: these fields are all three-component.
                    return Some([r, g, b]);
                }
                let a = t.raw_get::<Option<f64>>(1).ok().flatten()?;
                let b = t.raw_get::<Option<f64>>(2).ok().flatten()?;
                let c = t.raw_get::<Option<f64>>(3).ok().flatten()?;
                return Some([a, b, c]);
            }
            None
        }
        let fields_of = |t: &Table| -> mlua::Result<Vec<(String, crate::CompVal)>> {
            let mut out = Vec::new();
            for pair in t.pairs::<String, Value>() {
                let (k, v) = pair?;
                let cv = match &v {
                    Value::Number(n) => crate::CompVal::Num(*n),
                    Value::Integer(n) => crate::CompVal::Num(*n as f64),
                    Value::Boolean(b) => crate::CompVal::Num(if *b { 1.0 } else { 0.0 }),
                    Value::String(st) => crate::CompVal::Str(st.to_string_lossy().to_string()),
                    other => match triple_of(other) {
                        Some(p) => crate::CompVal::Vec3(p),
                        None => {
                            return Err(mlua::Error::runtime(format!(
                                "set*: field '{k}' must be a number, bool, string, vec3/{{x,y,z}} or {{r,g,b}}"
                            )))
                        }
                    },
                };
                out.push((k, cv));
            }
            Ok(out)
        };
        {
            let q = q.clone();
            let fo = fields_of;
            methods.set(
                "setCelestial",
                lua.create_function(move |_, (this, t): (Table, Table)| {
                    let e: u32 = this.raw_get("__id")?;
                    q.borrow_mut().push((e, crate::RichSet::Celestial(fo(&t)?)));
                    Ok(())
                })?,
            )?;
        }
        {
            let q = q.clone();
            let fo = fields_of;
            methods.set(
                "setMaterial",
                lua.create_function(move |_, (this, t): (Table, Table)| {
                    let e: u32 = this.raw_get("__id")?;
                    q.borrow_mut().push((e, crate::RichSet::Material(fo(&t)?)));
                    Ok(())
                })?,
            )?;
        }
        {
            let q = q.clone();
            methods.set(
                "setTerrain",
                lua.create_function(move |_, (this, id): (Table, u32)| {
                    let e: u32 = this.raw_get("__id")?;
                    q.borrow_mut().push((e, crate::RichSet::MatterTerrain(id)));
                    Ok(())
                })?,
            )?;
        }
        {
            // node:setTerrainGen(opts) — attach an ON-DEMAND generation spec (the
            // same opts table terrain.generatePlanet takes): the body's field
            // generates from it, on a background thread, when something first
            // approaches — no .cfield on disk, no up-front generation (G2 galaxy
            // streaming; docs/galaxy-streaming-proposal.md). Player edits saved
            // under terrain.saveDir take priority over regeneration. nil clears.
            let q = q.clone();
            methods.set(
                "setTerrainGen",
                lua.create_function(move |_, (this, opts): (Table, Option<Table>)| {
                    let e: u32 = this.raw_get("__id")?;
                    let spec = match &opts {
                        Some(t) => {
                            let fill = crate::terrain_api::planet_fill_from_table(Some(t));
                            Some(ron::to_string(&fill).map_err(|err| {
                                mlua::Error::runtime(format!("setTerrainGen: {err}"))
                            })?)
                        }
                        None => None,
                    };
                    q.borrow_mut().push((e, crate::RichSet::TerrainGen(spec)));
                    Ok(())
                })?,
            )?;
        }
        {
            let q = q.clone();
        // ---- 2D: node:setTilemap{...} and node:tilemap() (`floptle/0058`) ----
        {
            let q = q.clone();
            methods.set(
                "setTilemap",
                lua.create_function(move |_, (this, t): (Table, Table)| {
                    let e: u32 = this.raw_get("__id")?;
                    let cols: u32 = t.get::<Option<u32>>("cols")?.unwrap_or(0);
                    let rows: u32 = t.get::<Option<u32>>("rows")?.unwrap_or(0);
                    let tile: f32 = t.get::<Option<f32>>("tile")?.unwrap_or(1.0);
                    if cols == 0 || rows == 0 {
                        return Err(mlua::Error::runtime(
                            "setTilemap{ cols =, rows =, tile = }: cols and rows must be > 0",
                        ));
                    }
                    // `data` is optional: a grid with no cells yet is a blank
                    // room you then paint with tm:set, which is how a game that
                    // re-dresses a floor actually works.
                    let data: Vec<u32> = match t.get::<Option<Table>>("data")? {
                        Some(list) => {
                            let mut v = Vec::with_capacity(list.raw_len());
                            for i in 1..=list.raw_len() {
                                // Lua is 1-based; a nil hole is an empty tile.
                                v.push(list.raw_get::<Option<u32>>(i)?.unwrap_or(floptle_core::EMPTY_TILE));
                            }
                            v
                        }
                        None => Vec::new(),
                    };
                    q.borrow_mut().push((e, crate::RichSet::MatterTilemap { cols, rows, tile, data }));
                    Ok(())
                })?,
            )?;
        }
        {
            let q = q.clone();
            let scene = shared.scene.clone();
            methods.set(
                "tilemap",
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    new_tilemap_handle(lua, e, q.clone(), scene.clone())
                })?,
            )?;
        }
        {
            let draws = shared.sprite_draws.clone();
            methods.set(
                "sprites",
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    new_sprite_batch_handle(lua, e, draws.clone())
                })?,
            )?;
        }
            methods.set(
                "setPrimitive",
                lua.create_function(move |_, (this, shape, color): (Table, String, Value)| {
                    let e: u32 = this.raw_get("__id")?;
                    let c = match &color {
                        Value::Nil => [0.8, 0.8, 0.8],
                        other => triple_of(other).ok_or_else(|| {
                            mlua::Error::runtime("setPrimitive(shape [, color]): bad color")
                        })?,
                    };
                    q.borrow_mut().push((e, crate::RichSet::MatterPrimitive(shape, c)));
                    Ok(())
                })?,
            )?;
        }
    }
    // Tags: node:hasTag("enemy") → bool; node:addTag / node:removeTag edit the
    // list (dedup on add, no-op removes are fine). Reads see this frame's
    // node:destroy() — remove this node (and its whole subtree) from the scene.
    // Queued like every other write: the driver despawns after the pass, so the
    // handle stays safely readable for the rest of this call.
    {
        let q = shared.destroy_queue.clone();
        methods.set(
            "destroy",
            lua.create_function(move |_, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                q.borrow_mut().push(e);
                Ok(())
            })?,
        )?;
    }
    // pending edits (read-your-writes), the ECS component updates after the pass.
    {
        let scene = shared.scene.clone();
        let tag_changes = shared.tag_changes.clone();
        methods.set(
            "hasTag",
            lua.create_function(move |_, (this, tag): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let has = tag_changes
                    .borrow()
                    .get(&e)
                    .map(|t| t.contains(&tag))
                    .unwrap_or_else(|| {
                        scene.borrow().tags.get(&e).map(|t| t.contains(&tag)).unwrap_or(false)
                    });
                Ok(has)
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let tag_changes = shared.tag_changes.clone();
        methods.set(
            "addTag",
            lua.create_function(move |_, (this, tag): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let mut ch = tag_changes.borrow_mut();
                let tags = ch
                    .entry(e)
                    .or_insert_with(|| scene.borrow().tags.get(&e).cloned().unwrap_or_default());
                if !tags.contains(&tag) {
                    tags.push(tag);
                }
                Ok(())
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let tag_changes = shared.tag_changes.clone();
        methods.set(
            "removeTag",
            lua.create_function(move |_, (this, tag): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let mut ch = tag_changes.borrow_mut();
                let tags = ch
                    .entry(e)
                    .or_insert_with(|| scene.borrow().tags.get(&e).cloned().unwrap_or_default());
                tags.retain(|t| t != &tag);
                Ok(())
            })?,
        )?;
    }
    // node:animator() → the animation handle: play/stop/fade animation states on the
    // node's AnimationController (or a rigged model's embedded clips). Setters queue
    // into `anim_commands` (applied before the animators advance, same frame); getters
    // read the `anim_info` mirror the editor feeds each frame.
    {
        let anim_methods = lua.create_table()?;
        let queue = |cmds: &Rc<RefCell<Vec<(u32, AnimCmd)>>>, e: u32, c: AnimCmd| {
            cmds.borrow_mut().push((e, c));
        };
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "play",
                lua.create_function(
                    move |_, (this, state, fade, layer): (Table, String, Option<f64>, Option<String>)| {
                        let e: u32 = this.raw_get("__id")?;
                        queue(&cmds, e, AnimCmd::Play {
                            state,
                            layer,
                            fade: fade.map(|f| f as f32),
                            restart: false,
                        });
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "restart",
                lua.create_function(
                    move |_, (this, state, fade, layer): (Table, String, Option<f64>, Option<String>)| {
                        let e: u32 = this.raw_get("__id")?;
                        queue(&cmds, e, AnimCmd::Play {
                            state,
                            layer,
                            fade: fade.map(|f| f as f32),
                            restart: true,
                        });
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "crossfade",
                lua.create_function(
                    move |_, (this, state, fade, layer): (Table, String, f64, Option<String>)| {
                        let e: u32 = this.raw_get("__id")?;
                        queue(&cmds, e, AnimCmd::Play {
                            state,
                            layer,
                            fade: Some(fade as f32),
                            restart: false,
                        });
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "stop",
                lua.create_function(
                    move |_, (this, layer, fade): (Table, Option<String>, Option<f64>)| {
                        let e: u32 = this.raw_get("__id")?;
                        queue(&cmds, e, AnimCmd::Stop { layer, fade: fade.map(|f| f as f32) });
                        Ok(())
                    },
                )?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "setSpeed",
                lua.create_function(move |_, (this, s): (Table, f64)| {
                    let e: u32 = this.raw_get("__id")?;
                    queue(&cmds, e, AnimCmd::SetSpeed(s as f32));
                    Ok(())
                })?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "setLayerWeight",
                lua.create_function(move |_, (this, layer, w): (Table, String, f64)| {
                    let e: u32 = this.raw_get("__id")?;
                    queue(&cmds, e, AnimCmd::SetLayerWeight { layer, weight: w as f32 });
                    Ok(())
                })?,
            )?;
        }
        {
            let cmds = shared.anim_commands.clone();
            anim_methods.set(
                "seek",
                lua.create_function(move |_, (this, t, layer): (Table, f64, Option<String>)| {
                    let e: u32 = this.raw_get("__id")?;
                    queue(&cmds, e, AnimCmd::Seek { t: t as f32, layer });
                    Ok(())
                })?,
            )?;
        }
        // The layer whose state "shows": the topmost active layer, else the base.
        fn showing(info: &AnimInfo) -> Option<&(String, Option<String>, f32, bool)> {
            info.layers.iter().rev().find(|(_, s, _, _)| s.is_some()).or(info.layers.first())
        }
        {
            let inf = shared.anim_info.clone();
            let f = lua.create_function(move |lua, (this, layer): (Table, Option<String>)| {
                let e: u32 = this.raw_get("__id")?;
                let info = inf.borrow();
                let Some(i) = info.get(&e) else { return Ok(Value::Nil) };
                let slot = match &layer {
                    Some(l) => i.layers.iter().find(|(n, _, _, _)| n == l),
                    None => showing(i),
                };
                Ok(match slot.and_then(|(_, s, _, _)| s.as_ref()) {
                    Some(s) => Value::String(lua.create_string(s)?),
                    None => Value::Nil,
                })
            })?;
            anim_methods.set("state", f.clone())?;
            anim_methods.set("current", f)?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "time",
                lua.create_function(move |_, (this, layer): (Table, Option<String>)| {
                    let e: u32 = this.raw_get("__id")?;
                    let info = inf.borrow();
                    let Some(i) = info.get(&e) else { return Ok(Value::Nil) };
                    let slot = match &layer {
                        Some(l) => i.layers.iter().find(|(n, _, _, _)| n == l),
                        None => showing(i),
                    };
                    Ok(slot.map(|(_, _, t, _)| Value::Number(*t as f64)).unwrap_or(Value::Nil))
                })?,
            )?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "finished",
                lua.create_function(move |_, (this, layer): (Table, Option<String>)| {
                    let e: u32 = this.raw_get("__id")?;
                    let info = inf.borrow();
                    let Some(i) = info.get(&e) else { return Ok(Value::Boolean(false)) };
                    let slot = match &layer {
                        Some(l) => i.layers.iter().find(|(n, _, _, _)| n == l),
                        None => showing(i),
                    };
                    Ok(Value::Boolean(slot.map(|(_, _, _, f)| *f).unwrap_or(false)))
                })?,
            )?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "isPlaying",
                lua.create_function(move |_, (this, state): (Table, Option<String>)| {
                    let e: u32 = this.raw_get("__id")?;
                    let info = inf.borrow();
                    let Some(i) = info.get(&e) else { return Ok(Value::Boolean(false)) };
                    Ok(Value::Boolean(match &state {
                        Some(s) => i
                            .layers
                            .iter()
                            .any(|(_, cur, _, fin)| cur.as_deref() == Some(s) && !fin),
                        None => i.layers.iter().any(|(_, cur, _, _)| cur.is_some()),
                    }))
                })?,
            )?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "clips",
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    let arr = lua.create_table()?;
                    if let Some(i) = inf.borrow().get(&e) {
                        for (n, c) in i.clips.iter().enumerate() {
                            arr.set(n + 1, lua.create_string(&c.name)?)?;
                        }
                    }
                    Ok(arr)
                })?,
            )?;
        }
        // A clip's AUTHORED duration + events, read from the asset rather than from
        // playback — so a game can bake integer frame data once at load. Runtime event
        // dispatch is unchanged; these are read-only.
        //
        // A fighter cannot let clip events drive gameplay: they fire off float playback
        // time, stepped playback (`sample_fps`) quantises them to the step grid, clip
        // time and state frame disagree mid-crossfade, and a prediction replay
        // deliberately does not re-fire them. Baking at load sidesteps all four — every
        // machine loads the same `.anim.ron`, so the numbers are identical and constant.
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "duration",
                lua.create_function(move |_, (this, clip): (Table, String)| {
                    let e: u32 = this.raw_get("__id")?;
                    let info = inf.borrow();
                    let Some(i) = info.get(&e) else { return Ok(Value::Nil) };
                    Ok(i.clips
                        .iter()
                        .find(|c| c.name == clip)
                        .map(|c| Value::Number(c.duration as f64))
                        .unwrap_or(Value::Nil))
                })?,
            )?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "events",
                lua.create_function(move |lua, (this, clip): (Table, String)| {
                    let e: u32 = this.raw_get("__id")?;
                    let info = inf.borrow();
                    let Some(i) = info.get(&e) else { return Ok(Value::Nil) };
                    // Unknown clip → nil, so `if anim:events(c) then` guards work; a
                    // clip with no events → an empty array, which is a different answer.
                    let Some(c) = i.clips.iter().find(|c| c.name == clip) else {
                        return Ok(Value::Nil);
                    };
                    let arr = lua.create_table()?;
                    for (n, (t, func)) in c.events.iter().enumerate() {
                        let ev = lua.create_table()?;
                        ev.set("t", *t as f64)?;
                        ev.set("func", lua.create_string(func)?)?;
                        arr.set(n + 1, ev)?;
                    }
                    Ok(Value::Table(arr))
                })?,
            )?;
        }
        {
            let inf = shared.anim_info.clone();
            anim_methods.set(
                "layers",
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    let arr = lua.create_table()?;
                    if let Some(i) = inf.borrow().get(&e) {
                        for (n, (name, _, _, _)) in i.layers.iter().enumerate() {
                            arr.set(n + 1, lua.create_string(name)?)?;
                        }
                    }
                    Ok(arr)
                })?,
            )?;
        }
        // Method lookup goes through a function so a CASING typo fails with a
        // fix instead of a bare nil-call: the animator API is camelCase
        // (`anim:isPlaying`), and `anim:IsPlaying(...)` used to die with
        // "attempt to call a nil value (method 'IsPlaying')" — no hint at all.
        // A case-insensitive near-miss now errors with "did you mean
        // 'isPlaying'?". Genuinely unknown keys still index to nil, so
        // feature probes (`if anim.someday then`) keep working.
        let anim_mt = lua.create_table()?;
        anim_mt.set(
            "__index",
            lua.create_function(move |_, (_this, key): (Table, Value)| {
                let Value::String(k) = &key else { return Ok(Value::Nil) };
                let name = k.to_string_lossy().to_string();
                let hit: Value = anim_methods.raw_get(name.as_str())?;
                if hit != Value::Nil {
                    return Ok(hit);
                }
                for pair in anim_methods.pairs::<String, Value>() {
                    let (known, _) = pair?;
                    if known.eq_ignore_ascii_case(&name) {
                        return Err(mlua::Error::RuntimeError(format!(
                            "animator has no method '{name}' — did you mean '{known}'? \
                             (animator methods are camelCase)"
                        )));
                    }
                }
                Ok(Value::Nil)
            })?,
        )?;
        lua.set_named_registry_value("floptle_anim_mt", anim_mt)?;

        methods.set(
            "animator",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let t = lua.create_table()?;
                t.raw_set("__id", e)?;
                if let Ok(mt) = lua.named_registry_value::<Table>("floptle_anim_mt") {
                    t.set_metatable(Some(mt));
                }
                Ok(t)
            })?,
        )?;
    }
    // node:particles() → the particle-system handle: play / stop / restart the node's
    // ParticleSystem effect, and read its live state. Setters queue into `vfx_commands`
    // (applied before the effects advance, same frame); getters read the `vfx_info`
    // mirror the editor feeds each frame.
    {
        let vfx_methods = lua.create_table()?;
        for (name, cmd) in
            [("play", VfxCmd::Play), ("stop", VfxCmd::Stop), ("restart", VfxCmd::Restart)]
        {
            let cmds = shared.vfx_commands.clone();
            let cmd = cmd.clone();
            vfx_methods.set(
                name,
                lua.create_function(move |_, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    cmds.borrow_mut().push((e, cmd.clone()));
                    Ok(())
                })?,
            )?;
        }
        {
            let cmds = shared.vfx_commands.clone();
            vfx_methods.set(
                "setIntensity",
                lua.create_function(move |_, (this, i): (Table, f32)| {
                    let e: u32 = this.raw_get("__id")?;
                    cmds.borrow_mut().push((e, VfxCmd::Intensity(i)));
                    Ok(())
                })?,
            )?;
        }
        {
            // ps:setBeamEnd(x, y, z) — aim every Beam track of the node's effect at a
            // WORLD-space point (the engine converts it to effect-local, so the beam
            // tracks the target as the emitter moves/rotates).
            let cmds = shared.vfx_commands.clone();
            vfx_methods.set(
                "setBeamEnd",
                lua.create_function(move |_, (this, x, y, z): (Table, f64, f64, f64)| {
                    let e: u32 = this.raw_get("__id")?;
                    cmds.borrow_mut().push((e, VfxCmd::SetBeamEnd([x, y, z])));
                    Ok(())
                })?,
            )?;
        }
        {
            let inf = shared.vfx_info.clone();
            vfx_methods.set(
                "isPlaying",
                lua.create_function(move |_, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    Ok(Value::Boolean(inf.borrow().get(&e).map(|i| i.playing).unwrap_or(false)))
                })?,
            )?;
        }
        {
            let inf = shared.vfx_info.clone();
            vfx_methods.set(
                "alive",
                lua.create_function(move |_, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    Ok(Value::Number(inf.borrow().get(&e).map(|i| i.alive as f64).unwrap_or(0.0)))
                })?,
            )?;
        }
        {
            let inf = shared.vfx_info.clone();
            vfx_methods.set(
                "asset",
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    match inf.borrow().get(&e) {
                        Some(i) => Ok(Value::String(lua.create_string(&i.asset)?)),
                        None => Ok(Value::Nil),
                    }
                })?,
            )?;
        }
        let vfx_mt = lua.create_table()?;
        vfx_mt.set("__index", vfx_methods)?;
        lua.set_named_registry_value("floptle_vfx_mt", vfx_mt)?;

        methods.set(
            "particles",
            lua.create_function(move |lua, this: Table| {
                let e: u32 = this.raw_get("__id")?;
                let t = lua.create_table()?;
                t.raw_set("__id", e)?;
                if let Ok(mt) = lua.named_registry_value::<Table>("floptle_vfx_mt") {
                    t.set_metatable(Some(mt));
                }
                Ok(t)
            })?,
        )?;

        // node:setShaderParam(name, x, y?, z?, w?) — drive a `.flsl` uniform
        // from a script every tick (a uniform write on the GPU, never a
        // recompile). Works on a mesh Material's shader AND on a UI element's
        // `stage ui` shader — instruments like the navball live on this.
        {
            let sets = shared.shader_param_sets.clone();
            methods.set(
                "setShaderParam",
                lua.create_function(
                    move |_,
                          (this, name, x, y, z, w): (
                        Table,
                        String,
                        f32,
                        Option<f32>,
                        Option<f32>,
                        Option<f32>,
                    )| {
                        let e: u32 = this.raw_get("__id")?;
                        sets.borrow_mut().push((
                            e,
                            name,
                            [x, y.unwrap_or(0.0), z.unwrap_or(0.0), w.unwrap_or(0.0)],
                        ));
                        Ok(())
                    },
                )?,
            )?;
        }
    }

    // ---- orientation, local ↔ world, movement ---------------------------------------
    //
    // The half of the API that used to be written out longhand in every script:
    // `atan2` with two minus signs, a four-line project-onto-plane, and an
    // inverse-parent-transform nobody wanted to derive. Each one names the
    // intent, so it cannot get the sign wrong.
    //
    // They all go through the handle's own `__index`/`__newindex` (`this.get` /
    // `this.set`, never `raw_*`), so the own-node-vs-mirror rule, the body
    // teleport queue and the read-your-writes behaviour stay in ONE place.
    {
        // node:lookAt(target [, up]) — point at a node handle or a world point.
        // Sets yaw + pitch; roll only when you pass an `up` (and then it is
        // whatever puts that up over the node's head — a level horizon on a
        // planet, in one call instead of twenty lines of undo-yaw-then-pitch).
        //
        // WORLD space on both ends: the node's own world position against the
        // target's, then the angles written back as the LOCAL yaw/pitch the
        // fields are. Under an unrotated parent (the overwhelmingly common
        // case) those coincide; under a rotated one, aim with `:lookAt` on the
        // parent or read `node:worldForward()` to see what actually happened.
        let scene = shared.scene.clone();
        methods.set(
            "lookAt",
            lua.create_function(move |_, (this, target, up): (Table, Value, Option<Value>)| {
                let e: u32 = this.raw_get("__id")?;
                // A node handle aims at where it WORLD is; a bare vec3 is taken
                // as the world point it plainly is.
                let (t, here) = {
                    let s = scene.borrow();
                    let Some(t) = world_pos_of_value(&s, &target) else {
                        return Err(mlua::Error::RuntimeError(
                            "node:lookAt(target [, up]) — target is a node or a vec3".into(),
                        ));
                    };
                    (t, world_transform_of_handle(&s, &this, e).translation)
                };
                let up = match up {
                    Some(u) => Some(crate::math_api::vec3_of(&u).ok_or_else(|| {
                        mlua::Error::RuntimeError("node:lookAt's up is a vec3".into())
                    })?),
                    None => None,
                };
                let (yaw, pitch, roll) = crate::math_api::look_rotation(t - here, up);
                this.set("yaw", yaw)?;
                this.set("pitch", pitch)?;
                if up.is_some() {
                    this.set("roll", roll)?;
                }
                Ok(())
            })?,
        )?;
    }
    {
        // node:turnTowards(target, maxRadians) — the shortest-arc step toward
        // facing something, capped. Pass `rate * dt` and the turn is
        // frame-rate independent; the ±π seam is handled (`math.approachAngle`),
        // which is where every hand-written version went the long way round.
        // The target may be a node, a world point, or a DIRECTION vector.
        let scene = shared.scene.clone();
        methods.set(
            "turnTowards",
            lua.create_function(move |_, (this, target, max): (Table, Value, f64)| {
                let e: u32 = this.raw_get("__id")?;
                // A node handle or a point is somewhere to face; a short vector
                // that isn't a position would be ambiguous, so the rule is
                // simple and stated: handles resolve to their world position,
                // everything else is taken as a DIRECTION.
                let dir = match &target {
                    Value::Table(tt) if tt.raw_get::<u32>("__id").is_ok() => {
                        let s = scene.borrow();
                        world_pos_of_value(&s, &target).unwrap_or_default()
                            - world_transform_of_handle(&s, &this, e).translation
                    }
                    _ => crate::math_api::vec3_of(&target).ok_or_else(|| {
                        mlua::Error::RuntimeError(
                            "node:turnTowards(target, maxRadians) — target is a node, a world \
                             point or a direction"
                                .into(),
                        )
                    })?,
                };
                if dir.length_squared() < 1e-18 {
                    return Ok(()); // nowhere to turn: leave the facing alone
                }
                let step = |cur: f64, want: f64| -> f64 {
                    let d = wrap_pi_f64(want - cur);
                    if d.abs() <= max.abs() { want } else { cur + d.signum() * max.abs() }
                };
                let yaw: f64 = this.get("yaw").unwrap_or(0.0);
                let pitch: f64 = this.get("pitch").unwrap_or(0.0);
                this.set("yaw", step(yaw, crate::math_api::yaw_of(dir)))?;
                this.set("pitch", step(pitch, crate::math_api::pitch_of(dir)))?;
                Ok(())
            })?,
        )?;
    }
    {
        // node:toWorld(v) / node:toLocal(v) — a point through this node's own
        // frame (its position, rotation AND scale, composed up the parent
        // chain). "Where is the muzzle?" is `gun:toWorld(vec3(0, 0, -1.2))`.
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, (this, v, to_local): (Table, Value, bool)| {
            let e: u32 = this.raw_get("__id")?;
            let Some(v) = crate::math_api::vec3_of(&v) else {
                return Err(mlua::Error::RuntimeError(
                    "node:toWorld/toLocal take a vec3 (or anything with x/y/z)".into(),
                ));
            };
            let w = world_transform_of_handle(&scene.borrow(), &this, e);
            let p = if to_local {
                w.inv_mul(&floptle_core::Transform::from_translation(v)).translation
            } else {
                w.mul_transform(&floptle_core::Transform::from_translation(v)).translation
            };
            Ok(Value::UserData(lua.create_userdata(crate::math_api::LuaVec3(p))?))
        })?;
        let to_world = f.clone();
        methods.set(
            "toWorld",
            lua.create_function(move |_, (this, v): (Table, Value)| {
                to_world.call::<Value>((this, v, false))
            })?,
        )?;
        methods.set(
            "toLocal",
            lua.create_function(move |_, (this, v): (Table, Value)| {
                f.call::<Value>((this, v, true))
            })?,
        )?;
    }
    {
        // node:setWorldPos(v) — put a node at a WORLD point without deriving the
        // parent inverse by hand. Through `Transform::inv_mul`, the componentwise
        // TRS inverse: a matrix decomposition attributes a mirrored parent's
        // negative determinant to X regardless of which axis is actually
        // flipped, so a child of a mirrored character would land off by a
        // reflection.
        let scene = shared.scene.clone();
        methods.set(
            "setWorldPos",
            lua.create_function(move |_, (this, v): (Table, Value)| {
                let e: u32 = this.raw_get("__id")?;
                let Some(v) = crate::math_api::vec3_of(&v) else {
                    return Err(mlua::Error::RuntimeError(
                        "node:setWorldPos takes a vec3 (or anything with x/y/z)".into(),
                    ));
                };
                let local = parent_world_of(&scene.borrow(), e)
                    .inv_mul(&floptle_core::Transform::from_translation(v))
                    .translation;
                this.set("pos", crate::math_api::LuaVec3(local))?;
                Ok(())
            })?,
        )?;
    }
    {
        // node:worldForward() / worldRight() / worldUp() — the node's axes after
        // the parent chain. `node.forward` is the LOCAL one: a gun barrel
        // parented to an arm points where the ARM says, not where the gun's own
        // rotation says, and shooting along the local forward misses.
        let scene = shared.scene.clone();
        for (name, axis) in
            [("worldForward", Vec3::NEG_Z), ("worldRight", Vec3::X), ("worldUp", Vec3::Y)]
        {
            let scene = scene.clone();
            methods.set(
                name,
                lua.create_function(move |lua, this: Table| {
                    let e: u32 = this.raw_get("__id")?;
                    let r = world_transform_of(&scene.borrow(), e).rotation * axis;
                    Ok(Value::UserData(lua.create_userdata(crate::math_api::LuaVec3(
                        glam::DVec3::new(r.x as f64, r.y as f64, r.z as f64),
                    ))?))
                })?,
            )?;
        }
    }
    {
        // node:distanceTo(other) and node:distanceFlat(other [, up]) — measured
        // in WORLD space, because that is the answer people mean. `distance(a,
        // b)` compares LOCAL positions, which reads correctly right up until one
        // of the two is parented and then quietly answers about the wrong frame.
        // `distanceFlat` drops the component along `up` (default +Y): the "have
        // I arrived?" test for anything that walks on ground it doesn't control
        // the height of.
        let scene = shared.scene.clone();
        let f = lua.create_function(
            move |_, (this, other, up, flat): (Table, Value, Option<Value>, bool)| {
                let e: u32 = this.raw_get("__id")?;
                let s = scene.borrow();
                let a = world_transform_of_handle(&s, &this, e).translation;
                let b = world_pos_of_value(&s, &other).ok_or_else(|| {
                    mlua::Error::RuntimeError("node:distanceTo takes a node or a vec3".into())
                })?;
                let d = b - a;
                if !flat {
                    return Ok(d.length());
                }
                let up = match up {
                    Some(u) => crate::math_api::vec3_of(&u)
                        .and_then(|u| u.try_normalize())
                        .unwrap_or(glam::DVec3::Y),
                    None => glam::DVec3::Y,
                };
                Ok((d - up * d.dot(up)).length())
            },
        )?;
        let plain = f.clone();
        methods.set(
            "distanceTo",
            lua.create_function(move |_, (this, other): (Table, Value)| {
                plain.call::<f64>((this, other, Value::Nil, false))
            })?,
        )?;
        methods.set(
            "distanceFlat",
            lua.create_function(move |_, (this, other, up): (Table, Value, Value)| {
                f.call::<f64>((this, other, up, true))
            })?,
        )?;
    }
    {
        // node:moveTowards(target, maxDelta) — walk toward a WORLD point at a
        // speed, never overshooting it. Pass `speed * dt`. Returns true once it
        // has arrived, so `if node:moveTowards(goal, s * dt) then ... end` is the
        // whole patrol step. World-space and placed with setWorldPos, so a node
        // under a container arrives where you actually pointed.
        let scene = shared.scene.clone();
        methods.set(
            "moveTowards",
            lua.create_function(move |_, (this, target, max): (Table, Value, f64)| {
                let e: u32 = this.raw_get("__id")?;
                let (here, goal, parent) = {
                    let s = scene.borrow();
                    let goal = world_pos_of_value(&s, &target).ok_or_else(|| {
                        mlua::Error::RuntimeError(
                            "node:moveTowards(target, maxDelta) — target is a node or a vec3"
                                .into(),
                        )
                    })?;
                    (
                        world_transform_of_handle(&s, &this, e).translation,
                        goal,
                        parent_world_of(&s, e),
                    )
                };
                let next = crate::math_api::towards(here, goal, max);
                let local = parent
                    .inv_mul(&floptle_core::Transform::from_translation(next))
                    .translation;
                this.set("pos", crate::math_api::LuaVec3(local))?;
                Ok((next - goal).length() < 1e-9)
            })?,
        )?;
    }

    lua.set_named_registry_value("floptle_node_methods", methods)?;

    // ---- script metatable -----------------------------------------------------------
    let script_mt = lua.create_table()?;
    {
        let envs = shared.envs.clone();
        let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
            let name: String = this.raw_get("__script")?;
            match key.as_str() {
                "node" => return Ok(Value::Table(new_node_handle(lua, e)?)),
                "kind" | "name" => return Ok(Value::String(lua.create_string(&name)?)),
                "valid" => {
                    return Ok(Value::Boolean(envs.borrow().contains_key(&(e, name.clone()))));
                }
                _ => {}
            }
            let env = envs.borrow().get(&(e, name)).cloned();
            match env {
                Some(env) => env.get::<Value>(key),
                None => Ok(Value::Nil),
            }
        })?;
        script_mt.set("__index", idx)?;
    }
    {
        let envs = shared.envs.clone();
        let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
            let e: u32 = this.raw_get("__id")?;
            let name: String = this.raw_get("__script")?;
            let env = envs.borrow().get(&(e, name)).cloned();
            if let Some(env) = env {
                env.set(key, val)?;
            }
            Ok(())
        })?;
        script_mt.set("__newindex", newidx)?;
    }
    lua.set_named_registry_value("floptle_script_mt", script_mt)?;

    // ---- globals: find / findAll / findScript / noderef -----------------------------
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "find",
            lua.create_function(move |lua, name: String| {
                // O(1): the name index built each sync (first node in scene order wins).
                let found = scene.borrow().by_name.get(&name).copied();
                Ok(match found {
                    Some(e) => Value::Table(new_node_handle(lua, e)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
    // noderef(): mark a `defaults` entry as a node-reference param — the Inspector
    // shows a node picker for it and the script receives a node handle (or nil).
    lua.globals().set(
        "noderef",
        lua.create_function(|_, ()| Ok(crate::env::NODEREF_SENTINEL))?,
    )?;
    // scriptref("health"): the param binds to that SCRIPT on the wired node — the
    // Inspector only lists nodes carrying it, and the script gets a script handle
    // directly (call its functions, read its state). componentref("RigidBody"):
    // same idea for a component handle. Both read nil while unwired/invalid.
    lua.globals().set(
        "scriptref",
        lua.create_function(|_, kind: String| {
            Ok(format!("{}{kind}", crate::env::SCRIPTREF_PREFIX))
        })?,
    )?;
    lua.globals().set(
        "componentref",
        lua.create_function(|_, name: String| {
            Ok(format!("{}{name}", crate::env::COMPREF_PREFIX))
        })?,
    )?;
    // moveTowards(node, target, maxDelta) — the free-function spelling of
    // `node:moveTowards`, so it reads the same way as `dirTo` and `distance`
    // beside it. One implementation; this is a forward.
    lua.globals().set(
        "moveTowards",
        lua.create_function(|lua, (node, target, max): (Table, Value, f64)| {
            let methods: Table = lua.named_registry_value("floptle_node_methods")?;
            let f: mlua::Function = methods.get("moveTowards")?;
            f.call::<bool>((node, target, max))
        })?,
    )?;
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "findAll",
            lua.create_function(move |lua, name: String| {
                let ids: Vec<u32> = {
                    let s = scene.borrow();
                    s.order.iter().copied().filter(|e| s.names.get(e).map(|n| n == &name).unwrap_or(false)).collect()
                };
                let arr = lua.create_table()?;
                for (i, e) in ids.iter().enumerate() {
                    arr.set(i + 1, new_node_handle(lua, *e)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    {
        let scene = shared.scene.clone();
        let f = lua.create_function(move |lua, kind: String| {
            let found = {
                let s = scene.borrow();
                s.order
                    .iter()
                    .copied()
                    .find(|e| s.scripts.get(e).map(|v| v.iter().any(|k| k == &kind)).unwrap_or(false))
                    .map(|e| (e, kind.clone()))
            };
            Ok(match found {
                Some((e, k)) => Value::Table(new_script_handle(lua, e, &k)?),
                None => Value::Nil,
            })
        })?;
        lua.globals().set("findScript", f.clone())?;
        lua.globals().set("findScriptInScene", f)?;
    }
    // findScripts(kind): EVERY node carrying that script, as script handles in
    // scene order — for picking among several instances (e.g. a camera finding
    // the one player controller that is net.isMine, out of many avatars).
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "findScripts",
            lua.create_function(move |lua, kind: String| {
                let ids: Vec<u32> = {
                    let s = scene.borrow();
                    s.order
                        .iter()
                        .copied()
                        .filter(|e| {
                            s.scripts.get(e).map(|v| v.iter().any(|k| k == &kind)).unwrap_or(false)
                        })
                        .collect()
                };
                let arr = lua.create_table()?;
                for (i, e) in ids.iter().enumerate() {
                    arr.set(i + 1, new_script_handle(lua, *e, &kind)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    // findTagged(tag): EVERY node carrying that tag, as node handles in scene
    // order (an empty table when none). `findTagged("enemy")[1]` for the first.
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "findTagged",
            lua.create_function(move |lua, tag: String| {
                let ids: Vec<u32> = {
                    let s = scene.borrow();
                    s.order
                        .iter()
                        .copied()
                        .filter(|e| s.tags.get(e).map(|t| t.contains(&tag)).unwrap_or(false))
                        .collect()
                };
                let arr = lua.create_table()?;
                for (i, e) in ids.iter().enumerate() {
                    arr.set(i + 1, new_node_handle(lua, *e)?)?;
                }
                Ok(arr)
            })?,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_core::Material;

    /// A material's spritesheet frame is reachable from a script the same way a UI
    /// image's is: `getcomponent("Material").cell = n`. That means BOTH halves —
    /// the mirror (which is also what makes the field animatable and what
    /// `getcomponent` gates presence on) and the write-back.
    #[test]
    fn a_scripts_material_handle_carries_the_sprite_cell() {
        let mut world = World::new();
        let e = world.spawn();
        // No Material yet ⇒ no handle at all (getcomponent must return nil).
        assert!(!mirror_components(&world, e).contains_key("Material"));

        world.insert(e, Material { sheet_cols: 4, sheet_rows: 4, cell: 2, ..Material::default() });
        let mir = mirror_components(&world, e);
        let m = mir.get("Material").expect("a Material node exposes its handle");
        assert_eq!(m.get("cell"), Some(&2.0));
        assert_eq!(m.get("sheetCols"), Some(&4.0));

        apply_component_field(&mut world, e, "Material", "cell", 9.0);
        assert_eq!(world.get::<Material>(e).unwrap().cell, 9);
        // Negative frames clamp instead of wrapping to four billion.
        apply_component_field(&mut world, e, "Material", "cell", -3.0);
        assert_eq!(world.get::<Material>(e).unwrap().cell, 0);
    }

    /// Both spellings of a legacy snake_case component field reach the same
    /// mirror key — so the docs can teach camelCase (`rb.lockRotX`) while every
    /// script already written against `rb.lock_rot_x` keeps working, and the
    /// animation recorder still sees exactly one field change.
    #[test]
    fn camel_case_and_snake_case_name_the_same_component_field() {
        assert_eq!(snake_of("lockRotX").as_deref(), Some("lock_rot_x"));
        assert_eq!(snake_of("playOnStart").as_deref(), Some("play_on_start"));
        assert_eq!(snake_of("halfY").as_deref(), Some("half_y"));
        // Already snake_case, or a single word: nothing to translate.
        assert_eq!(snake_of("lock_rot_x"), None);
        assert_eq!(snake_of("friction"), None);
        // Every name it can produce for a legacy field is in the list the write
        // path filters on — otherwise a camelCase write would silently create a
        // second key that nothing reads.
        for f in LEGACY_SNAKE_FIELDS {
            let camel = {
                let mut out = String::new();
                let mut up = false;
                for c in f.chars() {
                    if c == '_' {
                        up = true;
                    } else if up {
                        out.push(c.to_ascii_uppercase());
                        up = false;
                    } else {
                        out.push(c);
                    }
                }
                out
            };
            assert_eq!(snake_of(&camel).as_deref(), Some(*f), "round trip for {f}");
        }
    }

    /// `node:setMaterial{ cell = n }` — the construction-API spelling — reaches the
    /// same field, and inserts a Material if the node had none.
    #[test]
    fn set_material_can_slice_a_sheet_and_pick_a_cell() {
        let mut world = World::new();
        let e = world.spawn();
        let ents = std::collections::HashMap::from([(e.index(), e)]);
        apply_rich_sets(
            &mut world,
            &ents,
            vec![(
                e.index(),
                crate::RichSet::Material(vec![
                    ("sheetCols".into(), crate::CompVal::Num(8.0)),
                    ("sheetRows".into(), crate::CompVal::Num(2.0)),
                    ("cell".into(), crate::CompVal::Num(11.0)),
                ]),
            )],
        );
        let m = world.get::<Material>(e).expect("setMaterial inserts the component");
        assert_eq!((m.sheet_cols, m.sheet_rows, m.cell), (8, 2, 11));
        assert!(m.is_sheet());
    }
}
