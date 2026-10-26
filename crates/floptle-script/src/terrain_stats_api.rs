//! What the terrains hold in memory, part by part.
//!
//! ```lua
//! local s = terrain.stats()      -- every terrain summed
//! local p = terrain.stats(1)     -- terrain 1 alone; nil if there is none
//! print(s.total, s.field, s.meshesGpu, s.age)
//! ```
//!
//! The engine counts once a second while a game runs and hands the count over
//! here, so a call costs a table and `age` says how old the count is. Bytes
//! shared between copies of a field (the physics' copy, Play's snapshot) are
//! counted once.

use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Table};

/// One terrain's memory, in bytes, as the engine counted it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TerrainStats {
    /// The terrain's id (`Matter::Terrain`).
    pub id: u32,
    /// Its voxels at full detail.
    pub field: u64,
    /// Chunks put away as coarse stand-ins.
    pub stand_ins: u64,
    /// The field's tables of chunks.
    pub tables: u64,
    /// Drawn chunk meshes on the GPU, at their capacity.
    pub meshes_gpu: u64,
    /// Chunk meshes kept on the CPU: far ones, and ones made ahead.
    pub meshes_cpu: u64,
    /// The physics' collision surface.
    pub collision: u64,
    /// The volume its shadows and AO are marched through.
    pub shadow: u64,
    /// Undo snapshots of its edits.
    pub undo: u64,
    /// Chunks at full detail, and chunks that are stand-ins.
    pub data_chunks: u64,
    pub stand_in_chunks: u64,
}

impl TerrainStats {
    /// Every byte counted.
    pub fn total(&self) -> u64 {
        self.field + self.stand_ins + self.tables + self.meshes_gpu + self.meshes_cpu + self.collision + self.shadow + self.undo
    }

    fn add(&mut self, o: &TerrainStats) {
        self.field += o.field;
        self.stand_ins += o.stand_ins;
        self.tables += o.tables;
        self.meshes_gpu += o.meshes_gpu;
        self.meshes_cpu += o.meshes_cpu;
        self.collision += o.collision;
        self.shadow += o.shadow;
        self.undo += o.undo;
        self.data_chunks += o.data_chunks;
        self.stand_in_chunks += o.stand_in_chunks;
    }
}

/// The latest count, and when it was taken; and the budget a game set.
#[derive(Default)]
pub(crate) struct Counted {
    pub(crate) terrains: Vec<TerrainStats>,
    pub(crate) at: Option<floptle_core::time::Instant>,
    /// `terrain.setBudget`: the most bytes every terrain together may hold.
    pub(crate) budget: Option<u64>,
    /// How much of its detail the terrain keeps to stay in the budget, 0..1
    /// (1 = all of it), as of the count.
    pub(crate) detail: f32,
}

pub(crate) fn install(lua: &Lua, state: Rc<RefCell<Counted>>) -> mlua::Result<()> {
    let terrain: Table = lua.globals().get("terrain")?;
    let st = state.clone();
    terrain.set(
        "setBudget",
        lua.create_function(move |_, bytes: Option<f64>| {
            let budget = match bytes {
                None => None,
                Some(b) if b > 0.0 => Some(b as u64),
                Some(_) => {
                    return Err(mlua::Error::RuntimeError(
                        "terrain.setBudget: the budget is a number of bytes above 0, or nil for none".into(),
                    ));
                }
            };
            st.borrow_mut().budget = budget;
            Ok(())
        })?,
    )?;
    terrain.set(
        "stats",
        lua.create_function(move |lua, id: Option<u32>| {
            let s = state.borrow();
            let mut sum = TerrainStats::default();
            let mut found = false;
            for t in s.terrains.iter().filter(|t| id.is_none_or(|i| i == t.id)) {
                sum.add(t);
                found = true;
            }
            if id.is_some() && !found {
                return Ok(mlua::Value::Nil);
            }
            let out = lua.create_table()?;
            out.set("total", sum.total())?;
            out.set("field", sum.field)?;
            out.set("standIns", sum.stand_ins)?;
            out.set("tables", sum.tables)?;
            out.set("meshesGpu", sum.meshes_gpu)?;
            out.set("meshesCpu", sum.meshes_cpu)?;
            out.set("collision", sum.collision)?;
            out.set("shadow", sum.shadow)?;
            out.set("undo", sum.undo)?;
            out.set("dataChunks", sum.data_chunks)?;
            out.set("standInChunks", sum.stand_in_chunks)?;
            out.set("terrains", if id.is_some() { 1 } else { s.terrains.len() })?;
            out.set("age", s.at.map_or(-1.0, |at| at.elapsed().as_secs_f64()))?;
            out.set("budget", s.budget)?;
            out.set("detail", if s.at.is_some() { s.detail } else { 1.0 })?;
            Ok(mlua::Value::Table(out))
        })?,
    )?;
    Ok(())
}
