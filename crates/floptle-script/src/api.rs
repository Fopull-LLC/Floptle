//! The cross-node / cross-script Lua reference layer: `node` and `script`
//! handle metatables (transform/body/component access, hierarchy traversal),
//! and the `find` / `findAll` / `findScript` globals.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use floptle_core::math::{EulerRot, Quat, Vec3};
use floptle_core::{Entity, Matter, RigidBody, World};
use mlua::{Lua, Table, Value};

use crate::env::{as_num, new_component_handle, new_node_handle, new_script_handle};
use crate::{AnimCmd, AnimInfo, Shared};

/// The numeric component fields exposed to scripts via `node:getcomponent(name)`, mirrored
/// from the live ECS each frame. Extend here (and in [`apply_component_field`]) to reach
/// more components / fields.
pub(crate) fn mirror_components(world: &World, e: Entity) -> HashMap<String, HashMap<String, f64>> {
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
    if let Some(rb) = world.get::<RigidBody>(e) {
        let b = |v: bool| if v { 1.0 } else { 0.0 };
        out.insert(
            "RigidBody".to_string(),
            HashMap::from([
                ("friction".to_string(), rb.friction as f64),
                ("restitution".to_string(), rb.restitution as f64),
                ("gravity".to_string(), b(rb.gravity)),
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

/// Apply a `node:getcomponent(name).field = value` write back to the ECS (mirror of
/// [`mirror_components`]). Unknown component/field names are ignored.
pub(crate) fn apply_component_field(world: &mut World, ent: Entity, comp: &str, field: &str, val: f64) {
    match comp {
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
        "RigidBody" => {
            if let Some(rb) = world.get_mut::<RigidBody>(ent) {
                match field {
                    "friction" => rb.friction = val as f32,
                    "restitution" => rb.restitution = val as f32,
                    "gravity" => rb.gravity = val != 0.0,
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
        let idx = lua.create_function(move |lua, (this, key): (Table, String)| {
            let e: u32 = this.raw_get("__id")?;
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
        let model_changes = shared.model_changes.clone();
        let material_changes = shared.material_changes.clone();
        let visible_changes = shared.visible_changes.clone();
        let newidx = lua.create_function(move |_, (this, key, val): (Table, String, Value)| {
            let e: u32 = this.raw_get("__id")?;
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

    // ---- globals: find / findAll / findScript ---------------------------------------
    {
        let scene = shared.scene.clone();
        lua.globals().set(
            "find",
            lua.create_function(move |lua, name: String| {
                let found = {
                    let s = scene.borrow();
                    s.order.iter().copied().find(|e| s.names.get(e).map(|n| n == &name).unwrap_or(false))
                };
                Ok(match found {
                    Some(e) => Value::Table(new_node_handle(lua, e)?),
                    None => Value::Nil,
                })
            })?,
        )?;
    }
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
    Ok(())
}
