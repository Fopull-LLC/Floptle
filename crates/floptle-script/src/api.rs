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
        // Position: the active placement's numbers (Free pos or Pin offset).
        let (px, py) = match spec.place {
            floptle_ui::Place::Free { pos } => (pos[0], pos[1]),
            floptle_ui::Place::Pin { offset, .. } => (offset[0], offset[1]),
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
        if let Some(s) = spec.shape {
            f.insert("radius".to_string(), s.radius as f64);
            f.insert("border".to_string(), s.border as f64);
            for (k, v) in ["fillR", "fillG", "fillB", "fillA"].iter().zip(s.fill) {
                f.insert(k.to_string(), v as f64);
            }
        }
        if let Some(t) = &spec.text {
            f.insert("textSize".to_string(), t.size as f64);
            for (k, v) in ["textR", "textG", "textB", "textA"].iter().zip(t.color) {
                f.insert(k.to_string(), v as f64);
            }
        }
        if let Some(img) = &spec.image {
            for (k, v) in ["tintR", "tintG", "tintB", "tintA"].iter().zip(img.tint) {
                f.insert(k.to_string(), v as f64);
            }
            f.insert("cell".to_string(), img.cell as f64);
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
    if let Some(Matter::Camera { fov_y, active }) = world.get::<Matter>(e) {
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

/// Format a Lua number the way `tostring` would (integers without the `.0`).
fn format_lua_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
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
                    "radius" => {
                        if let Some(s) = &mut spec.shape {
                            s.radius = v;
                        }
                    }
                    "border" => {
                        if let Some(s) = &mut spec.shape {
                            s.border = v;
                        }
                    }
                    "fillR" | "fillG" | "fillB" | "fillA" => {
                        if let Some(s) = &mut spec.shape {
                            s.fill[rgba_index(field)] = v;
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
            if let Some(Matter::Camera { fov_y, active }) = world.get_mut::<Matter>(ent) {
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
        let layer_changes = shared.layer_changes.clone();
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
            // Transform reads.
            {
                let s = scene.borrow();
                if let Some(tr) = s.transforms.get(&e) {
                    match key.as_str() {
                        "x" => return Ok(Value::Number(tr.translation.x)),
                        "y" => return Ok(Value::Number(tr.translation.y)),
                        "z" => return Ok(Value::Number(tr.translation.z)),
                        "scale" | "scale_x" => return Ok(Value::Number(tr.scale.x as f64)),
                        "scale_y" => return Ok(Value::Number(tr.scale.y as f64)),
                        "scale_z" => return Ok(Value::Number(tr.scale.z as f64)),
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
                "up_x" | "up_y" | "up_z" => {
                    return Ok(match bodies.borrow().get(&e) {
                        Some(b) => Value::Number(match key.as_str() {
                            "up_x" => b.up[0],
                            "up_y" => b.up[1],
                            _ => b.up[2],
                        } as f64),
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
        let layer_changes = shared.layer_changes.clone();
        let tag_changes = shared.tag_changes.clone();
        let layer_table = shared.layer_table.clone();
        let ui_text_changes = shared.ui_text_changes.clone();
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
                        "scale" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale = Vec3::splat(n as f32);
                            }
                        }
                        "scale_x" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.x = n as f32;
                            }
                        }
                        "scale_y" => {
                            if let Some(n) = as_num(&val) {
                                tr.scale.y = n as f32;
                            }
                        }
                        "scale_z" => {
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
            let idx = lua.create_function(move |_, (this, key): (Table, String)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                if let Some(v) = changes.borrow().get(&(e, comp.clone(), key.clone())) {
                    return Ok(Value::Number(*v));
                }
                let s = scene.borrow();
                if let Some(v) =
                    s.components.get(&e).and_then(|c| c.get(&comp)).and_then(|m| m.get(&key))
                {
                    return Ok(Value::Number(*v));
                }
                Ok(Value::Nil)
            })?;
            comp_mt.set("__index", idx)?;
        }
        {
            let changes = shared.component_changes.clone();
            let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
                let e: u32 = this.raw_get("__id")?;
                let comp: String = this.raw_get("__comp")?;
                let n = match val {
                    Value::Number(n) => n,
                    Value::Integer(n) => n as f64,
                    Value::Boolean(b) => {
                        if b {
                            1.0
                        } else {
                            0.0
                        }
                    }
                    _ => {
                        return Err(mlua::Error::RuntimeError(format!(
                            "component field '{key}' must be a number"
                        )))
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
                        for (n, s) in i.states.iter().enumerate() {
                            arr.set(n + 1, lua.create_string(s)?)?;
                        }
                    }
                    Ok(arr)
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
        let anim_mt = lua.create_table()?;
        anim_mt.set("__index", anim_methods)?;
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
