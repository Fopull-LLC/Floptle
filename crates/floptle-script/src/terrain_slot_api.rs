//! `terrain.slotMaterial` / `terrain.setSlotMaterial` / `terrain.slotCount`:
//! a script reads and sets the Material a terrain palette slot carries, the
//! same keys `node:setMaterial{...}` takes.
//!
//! The engine publishes every slot's Material ([`SlotMaterials::view`]) and
//! takes the sets a pass made ([`SlotMaterials::sets`]) after it. A set
//! writes the view at once, so a read straight after it answers what the
//! script just said.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// The palette slots' Materials as scripts see them, and the changes they made.
#[derive(Default)]
pub struct SlotMaterials {
    /// One entry per palette slot: its own Material, or `None` for a slot
    /// that draws with its terrain's.
    pub view: Vec<Option<floptle_core::Material>>,
    /// `(slot, Material)` sets since the engine last took them, 0-based;
    /// `None` takes a slot's Material away.
    pub sets: Vec<(usize, Option<floptle_core::Material>)>,
}

pub(crate) type SlotShared = Rc<RefCell<SlotMaterials>>;

/// A palette slot from a script's 1-based number, checked against the palette.
fn slot_index(call: &str, slot: i64, len: usize) -> mlua::Result<usize> {
    if slot < 1 || slot as usize > len {
        return Err(mlua::Error::runtime(format!(
            "{call}: slot {slot} is not in the palette, which has {len} slot{} (numbered from 1, as terrain.paintTexture's)",
            if len == 1 { "" } else { "s" }
        )));
    }
    Ok(slot as usize - 1)
}

/// Material `m` as the table `setMaterial{...}` takes: every key, so a script
/// can read a slot, change one value and hand the table back.
pub(crate) fn material_table(lua: &Lua, m: &floptle_core::Material) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    let rgb = |c: [f32; 3]| crate::math_api::LuaVec3(glam::DVec3::new(c[0] as f64, c[1] as f64, c[2] as f64));
    let path = |p: &Option<String>| p.clone().unwrap_or_default();
    t.set("color", rgb(m.color))?;
    t.set("emissive", rgb(m.emissive))?;
    t.set("emissiveStrength", m.emissive_strength)?;
    t.set("specular", rgb(m.specular))?;
    t.set("shininess", m.shininess)?;
    t.set("specularStrength", m.specular_strength)?;
    t.set("rim", rgb(m.rim))?;
    t.set("rimStrength", m.rim_strength)?;
    t.set("unlit", m.unlit)?;
    t.set("fog", m.fog)?;
    t.set("ambient", m.ambient)?;
    t.set("alpha", m.alpha)?;
    t.set("texture", path(&m.texture))?;
    t.set("normalMap", path(&m.normal_map))?;
    t.set("normalStrength", m.normal_strength)?;
    t.set("roughnessMap", path(&m.roughness_map))?;
    t.set("roughness", m.roughness)?;
    t.set("metallicMap", path(&m.metallic_map))?;
    t.set("metallic", m.metallic)?;
    t.set("occlusionMap", path(&m.ao_map))?;
    t.set("occlusionStrength", m.occlusion_strength)?;
    t.set("emissiveMap", path(&m.emissive_map))?;
    t.set("reflectivity", m.reflectivity)?;
    t.set("shading", m.shading.as_str())?;
    Ok(t)
}

pub(crate) fn install(lua: &Lua, shared: SlotShared) -> mlua::Result<()> {
    let terrain: Table = lua.globals().get("terrain")?;

    // terrain.slotCount() → the palette's slots.
    {
        let s = shared.clone();
        terrain.set("slotCount", lua.create_function(move |_, ()| Ok(s.borrow().view.len()))?)?;
    }

    // terrain.slotMaterial(slot) → the slot's own Material as a setMaterial
    // table, or nil when the slot draws with its terrain's.
    {
        let s = shared.clone();
        terrain.set(
            "slotMaterial",
            lua.create_function(move |lua, slot: i64| {
                let s = s.borrow();
                let i = slot_index("terrain.slotMaterial", slot, s.view.len())?;
                match &s.view[i] {
                    Some(m) => Ok(Value::Table(material_table(lua, m)?)),
                    None => Ok(Value::Nil),
                }
            })?,
        )?;
    }

    // terrain.setSlotMaterial(slot, { ... } | nil): lay the keys given over
    // the slot's own Material (a slot without one starts from the default
    // Material, as a new one in the Inspector does); nil takes it away, and
    // the slot draws with its terrain's Material again.
    {
        let s = shared.clone();
        terrain.set(
            "setSlotMaterial",
            lua.create_function(move |_, (slot, opts): (i64, Option<Table>)| {
                let mut s = s.borrow_mut();
                let i = slot_index("terrain.setSlotMaterial", slot, s.view.len())?;
                let next = match opts {
                    None => None,
                    Some(t) => {
                        crate::opts::check_keys(&t, crate::api::MATERIAL_KEYS, "terrain.setSlotMaterial")?;
                        let fields = crate::api::rich_fields(&t)?;
                        let mut m = s.view[i].clone().unwrap_or_default();
                        crate::api::apply_material_fields(&mut m, &fields);
                        Some(m)
                    }
                };
                s.view[i] = next.clone();
                s.sets.push((i, next));
                Ok(())
            })?,
        )?;
    }
    Ok(())
}
