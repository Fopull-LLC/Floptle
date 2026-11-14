//! Meshes a game builds while it runs: a procedural plant, a fairing fitted to
//! a part, a rope.
//!
//! ```lua
//! local m = mesh.build{
//!   positions = { 0,0,0,  1,0,0,  0,1,0 },   -- x, y, z per vertex
//!   indices   = { 1, 2, 3 },                 -- 1-based, three per triangle
//!   normals   = { ... },                      -- optional: worked out if left out
//!   uvs       = { ... },                      -- optional: u, v per vertex
//!   colors    = { ... },                      -- optional: r, g, b, a per vertex, 0..1
//! }
//! node.model = m                              -- or scatter.create{ asset = m }
//! mesh.free(m)                                -- give it back
//! ```
//!
//! A surface a script moves (water, cloth, a cable) is built once with
//! `dynamic = true` and rewritten in place:
//!
//! ```lua
//! local sheet = mesh.build{ positions = p, indices = i, colors = c, dynamic = true }
//! mesh.update(sheet, { positions = p2, colors = c2 })   -- same vertices, new values
//! mesh.update(sheet, { indices = fewer })               -- draw fewer triangles
//! ```
//!
//! The name, the node wearing it and its material all stay as they are; the
//! new values are written into the mesh's own GPU buffers before the frame
//! that follows the script is drawn.
//!
//! `mesh.build` answers a name like `"mesh:3"` at once, usable anywhere a
//! model path is. The script host checks and packs the arrays; the driver
//! puts the mesh on the GPU after the pass, so it draws from the next frame.
//! A host that draws nothing still answers a name, and nothing draws it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// The most vertices one built mesh may have. A runtime mesh is a plant or a
/// part, not a level; this is far past either and well inside a GPU buffer.
pub const MAX_MESH_VERTICES: usize = 1 << 20;
/// The most indices: a little over a million triangles.
pub const MAX_MESH_INDICES: usize = 3 << 20;
/// Every key `mesh.build{...}` reads.
pub const BUILD_KEYS: &[&str] =
    &["positions", "indices", "normals", "uvs", "colors", "owner", "scene", "dynamic", "indexCapacity"];
/// Every key `mesh.update(name, {...})` reads. `dynamic` and `indexCapacity`
/// are let through and ignored, so the table `mesh.build` took (or
/// `water.surface` made) is an update as it stands.
pub const UPDATE_KEYS: &[&str] = &["positions", "indices", "normals", "uvs", "colors", "dynamic", "indexCapacity"];

/// A mesh a script built, for the driver to upload under `name`.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshRequest {
    pub name: String,
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    /// RGBA8 per vertex, when the script gave colours.
    pub colors: Option<Vec<[u8; 4]>>,
    /// 0-based, three per triangle.
    pub indices: Vec<u32>,
    /// How many indices its buffer has room for: `indices.len()`, or more
    /// for a `dynamic` mesh built with `indexCapacity`, whose triangle count
    /// a later `mesh.update` may raise that far.
    pub index_capacity: usize,
}

/// New values for a mesh a script built with `dynamic = true`, for the
/// driver to write into the mesh's own buffers.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshUpdate {
    pub name: String,
    /// Every vertex, when any of them changed: the same count as the mesh
    /// was built with.
    pub vertices: Option<MeshVertices>,
    /// RGBA8 per vertex, when the colours changed.
    pub colors: Option<Vec<[u8; 4]>>,
    /// 0-based, three per triangle, when the triangles changed: no more
    /// than the mesh's index capacity, and possibly none.
    pub indices: Option<Vec<u32>>,
}

/// A mesh's vertices as three parallel arrays.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshVertices {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
}

/// What the engine keeps of a `dynamic` mesh, so an update can leave out
/// what did not change and is checked against what the mesh is.
struct Kept {
    vertices: MeshVertices,
    indices: Vec<u32>,
    index_capacity: usize,
    /// The normals were worked out from the faces, not given: they are
    /// worked out again whenever the positions or the triangles change.
    auto_normals: bool,
    has_colors: bool,
    /// What has changed since the driver last took the updates.
    dirty_vertices: bool,
    dirty_indices: bool,
    dirty_colors: Option<Vec<[u8; 4]>>,
}

/// A mesh or texture a script made and has not let go of: who made it and
/// what it holds, for `perf.resources` and `perf.counts`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Made {
    /// The script file whose load or hook made it ("" outside any).
    pub(crate) script: String,
    /// Its bytes on the GPU, as near as is known (0 until a decoded picture's
    /// size is).
    pub(crate) bytes: u64,
    /// What it lives as long as: the session (until freed or Stop), a node,
    /// or the scene.
    pub(crate) owner: Owner,
}

/// How long a mesh or texture a script made lives, besides until it is freed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Owner {
    /// Until Stop: what every one did before there was a choice.
    #[default]
    Session,
    /// Until the node, by id, is gone.
    Node(u32),
    /// Until the next scene load.
    Scene,
}

/// The lifetime an options table asks for: `owner = node` or `scene = true`.
pub(crate) fn owner_of(t: &Table, call: &str) -> mlua::Result<Owner> {
    let node: Value = t.raw_get("owner")?;
    let scene = crate::opts::opt_bool(t, call, "scene")?.unwrap_or(false);
    match (node, scene) {
        (Value::Nil, false) => Ok(Owner::Session),
        (Value::Nil, true) => Ok(Owner::Scene),
        (Value::Table(n), false) => match n.raw_get::<Option<u32>>("__id")? {
            Some(id) => Ok(Owner::Node(id)),
            None => Err(mlua::Error::runtime(format!("{call}: `owner` is a node, the one it is freed with"))),
        },
        (Value::Table(_), true) => Err(mlua::Error::runtime(format!("{call}: `owner` and `scene` are two lifetimes; give one"))),
        (other, _) => Err(mlua::Error::runtime(format!("{call}: `owner` is a node, not a {}", other.type_name()))),
    }
}

impl MeshBook {
    /// Let go of every mesh whose owner is done: `scene` at a scene load, or
    /// the node no longer there by `alive`.
    pub(crate) fn release_owned(&mut self, scene: bool, alive: &dyn Fn(u32) -> bool) -> usize {
        let gone: Vec<String> = self
            .live
            .iter()
            .filter(|(_, m)| match m.owner {
                Owner::Session => false,
                Owner::Scene => scene,
                Owner::Node(id) => !alive(id),
            })
            .map(|(n, _)| n.clone())
            .collect();
        for name in &gone {
            self.live.remove(name);
            self.kept.remove(name);
            let before = self.requests.len();
            self.requests.retain(|r| r.name != *name);
            if self.requests.len() == before {
                self.releases.push(name.clone());
            }
        }
        gone.len()
    }
}

/// What scripts built and freed, not yet seen by the driver.
#[derive(Default)]
pub(crate) struct MeshBook {
    /// Never reset, so a name is never reused within the process: a node
    /// still wearing a freed mesh draws nothing rather than another mesh.
    next: u64,
    requests: Vec<MeshRequest>,
    releases: Vec<String>,
    /// The `dynamic` meshes, by name.
    kept: HashMap<String, Kept>,
    /// The dynamic meshes updated since the driver last asked, in the order
    /// they were first touched. Several updates of one mesh in a frame are
    /// one write.
    updated: Vec<String>,
    pub(crate) live: HashMap<String, Made>,
    /// Who is making a mesh now.
    pub(crate) running: Maker,
}

/// Who a mesh or texture is charged to: the script whose code is running.
///
/// The host's charge (`charge_memory_to`) names the script whose hook it
/// entered, and is empty for a callback it calls afterwards, so a mesh made
/// in a `createNode` or `spawn` callback was charged to no script at all. The
/// Lua stack says whose code is calling: the first frame from a script's own
/// chunk (named plainly, where the engine's chunks are `=` or `@` names and C
/// is `=[C]`), so a helper in another script file is charged to that file.
/// The host's charge stands when no script is on the stack.
#[derive(Clone, Default)]
pub(crate) struct Maker {
    running: Rc<RefCell<String>>,
    lua: Option<mlua::WeakLua>,
}

impl Maker {
    pub(crate) fn new(running: Rc<RefCell<String>>, lua: &Lua) -> Self {
        Self { running, lua: Some(lua.weak()) }
    }

    /// The script making something now.
    pub(crate) fn now(&self) -> String {
        self.lua.as_ref().and_then(mlua::WeakLua::try_upgrade).and_then(|lua| calling_script(&lua)).unwrap_or_else(|| self.running.borrow().clone())
    }
}

/// The script chunk nearest the top of the Lua stack, if any.
pub(crate) fn calling_script(lua: &Lua) -> Option<String> {
    (0..256).map_while(|level| lua.inspect_stack(level)).find_map(|d| {
        let src = d.source().source?.into_owned();
        (!src.is_empty() && !src.starts_with(['=', '@'])).then_some(src)
    })
}

impl MeshBook {
    pub(crate) fn take_requests(&mut self) -> Vec<MeshRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn take_releases(&mut self) -> Vec<String> {
        std::mem::take(&mut self.releases)
    }

    /// What `mesh.update` changed since the last call, one entry a mesh.
    pub(crate) fn take_updates(&mut self) -> Vec<MeshUpdate> {
        let names = std::mem::take(&mut self.updated);
        names
            .into_iter()
            .filter_map(|name| {
                let k = self.kept.get_mut(&name)?;
                let update = MeshUpdate {
                    vertices: std::mem::take(&mut k.dirty_vertices).then(|| k.vertices.clone()),
                    colors: k.dirty_colors.take(),
                    indices: std::mem::take(&mut k.dirty_indices).then(|| k.indices.clone()),
                    name,
                };
                Some(update)
            })
            .collect()
    }

    /// Forget a mesh that was freed.
    fn forget(&mut self, name: &str) {
        self.kept.remove(name);
        self.updated.retain(|n| n != name);
    }

    /// Stop: the driver lets go of every built mesh.
    pub(crate) fn reset(&mut self) {
        let (next, running) = (self.next, self.running.clone());
        *self = Self { next, running, ..Self::default() };
    }
}

/// Is `key` a name `mesh.build` made? Such a key is looked up, never loaded
/// from a file.
pub fn is_built_mesh(key: &str) -> bool {
    key.starts_with("mesh:")
}

/// A flat number array, `per` numbers per vertex, `n` vertices (when known).
/// A `mesh.build` array: a flat list of numbers, or a buffer of
/// little-endian f32s (`buffer.writef32`), which is read in one copy rather
/// than a number at a time. A 45,000-vertex mesh read as lists cost 12 ms of
/// the frame, nearly all of it crossing into the engine one number at a time.
fn floats(call: &str, v: &Value, key: &str, per: usize, n: Option<usize>) -> mlua::Result<Vec<f32>> {
    match v {
        Value::Table(t) => flat(call, t, key, per, n),
        Value::Buffer(b) => {
            let bytes = b.to_vec();
            if !bytes.len().is_multiple_of(4 * per) {
                return Err(mlua::Error::runtime(format!(
                    "{call}: the `{key}` buffer is {} bytes, not a whole number of {per} f32s ({} bytes each)",
                    bytes.len(),
                    4 * per
                )));
            }
            let out: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect();
            if let Some(n) = n
                && out.len() / per != n
            {
                return Err(mlua::Error::runtime(format!(
                    "{call}: the `{key}` buffer has {} entries for {n} vertices; it needs one per vertex",
                    out.len() / per
                )));
            }
            if let Some(i) = out.iter().position(|v| !v.is_finite()) {
                return Err(mlua::Error::runtime(format!("{call}: the `{key}` buffer's f32 {i} (0-based) is not a finite number")));
            }
            Ok(out)
        }
        other => Err(mlua::Error::runtime(format!(
            "{call}: `{key}` is a {}; it takes a flat list of numbers or a buffer of f32s",
            other.type_name()
        ))),
    }
}

fn flat(call: &str, t: &Table, key: &str, per: usize, n: Option<usize>) -> mlua::Result<Vec<f32>> {
    let len = t.raw_len();
    if !len.is_multiple_of(per) {
        return Err(mlua::Error::runtime(format!(
            "{call}: `{key}` holds {len} numbers, not a whole number of {per}s"
        )));
    }
    if let Some(n) = n
        && len / per != n
    {
        return Err(mlua::Error::runtime(format!(
            "{call}: `{key}` has {} entries for {n} vertices; it needs one per vertex",
            len / per
        )));
    }
    let mut out = Vec::with_capacity(len);
    for i in 1..=len {
        let v: f64 = match t.raw_get::<Value>(i)? {
            Value::Number(v) => v,
            Value::Integer(v) => v as f64,
            other => {
                return Err(mlua::Error::runtime(format!(
                    "{call}: `{key}`[{i}] is a {}, not a number",
                    other.type_name()
                )));
            }
        };
        if !v.is_finite() {
            return Err(mlua::Error::runtime(format!("{call}: `{key}`[{i}] is not a finite number")));
        }
        out.push(v as f32);
    }
    Ok(out)
}

/// Area-weighted vertex normals: each triangle adds its unnormalised face
/// normal to its three corners, so a big face counts for more than a sliver.
fn smooth_normals(positions: &[[f32; 3]], indices: &[u32]) -> Vec<[f32; 3]> {
    let mut acc = vec![glam::Vec3::ZERO; positions.len()];
    for tri in indices.as_chunks::<3>().0 {
        let [a, b, c] = [tri[0] as usize, tri[1] as usize, tri[2] as usize];
        let (pa, pb, pc) =
            (glam::Vec3::from(positions[a]), glam::Vec3::from(positions[b]), glam::Vec3::from(positions[c]));
        let n = (pb - pa).cross(pc - pa);
        acc[a] += n;
        acc[b] += n;
        acc[c] += n;
    }
    acc.into_iter().map(|n| n.try_normalize().unwrap_or(glam::Vec3::Y).to_array()).collect()
}

/// Read `indices`: a list numbered from 1, as Lua numbers everything, or a
/// buffer of u32s numbered from 0, as the GPU numbers vertices. `n` is the
/// vertex count, `most` the most indices allowed, and `none` whether a mesh
/// with no triangles at all is one (a dynamic mesh that is dry for now).
fn indices_of(call: &str, idx: &Value, n: usize, most: usize, none: bool) -> mlua::Result<Vec<u32>> {
    let from_buffer: Option<Vec<u32>> = match idx {
        Value::Buffer(b) => {
            let bytes = b.to_vec();
            if !bytes.len().is_multiple_of(4) {
                return Err(mlua::Error::runtime(format!("{call}: the `indices` buffer is {} bytes, not a whole number of u32s", bytes.len())));
            }
            Some(bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect())
        }
        Value::Table(_) => None,
        other => {
            return Err(mlua::Error::runtime(format!(
                "{call}: `indices` is a {}; it takes a list of vertex numbers or a buffer of u32s",
                other.type_name()
            )));
        }
    };
    let count = match (&from_buffer, idx) {
        (Some(b), _) => b.len(),
        (None, Value::Table(t)) => t.raw_len(),
        _ => 0,
    };
    if (count == 0 && !none) || count % 3 != 0 {
        return Err(mlua::Error::runtime(format!("{call}: `indices` holds {count} entries; it takes three per triangle")));
    }
    if count > most {
        return Err(mlua::Error::runtime(format!("{call}: {count} indices is more than the {most} this mesh may have")));
    }
    match (from_buffer, idx) {
        (Some(b), _) => {
            if let Some(i) = b.iter().position(|&v| v as usize >= n) {
                return Err(mlua::Error::runtime(format!(
                    "{call}: the `indices` buffer's u32 {i} is {}; numbered from 0, a vertex is 0 to {}",
                    b[i],
                    n - 1
                )));
            }
            Ok(b)
        }
        (None, Value::Table(idx)) => {
            let mut indices = Vec::with_capacity(count);
            for i in 1..=count {
                let v: f64 = idx.raw_get(i)?;
                if v.fract() != 0.0 || v < 1.0 || v > n as f64 {
                    return Err(mlua::Error::runtime(format!("{call}: `indices`[{i}] is {v}; a vertex is numbered 1 to {n}")));
                }
                indices.push(v as u32 - 1);
            }
            Ok(indices)
        }
        _ => Ok(Vec::new()),
    }
}

/// Colours as the GPU keeps them: a byte a channel.
fn colors_of(call: &str, v: &Value, n: usize) -> mlua::Result<Vec<[u8; 4]>> {
    Ok(floats(call, v, "colors", 4, Some(n))?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8))
        .collect())
}

/// Read and check a `mesh.build{...}` table. The second answer is whether it
/// asked to be `dynamic`, and if so whether its normals were worked out.
pub(crate) fn parse_build(t: &Table) -> mlua::Result<(MeshRequest, Option<bool>)> {
    const CALL: &str = "mesh.build";
    crate::opts::check_keys(t, BUILD_KEYS, CALL)?;
    let need = |key: &str| -> mlua::Result<Value> {
        match t.raw_get::<Value>(key)? {
            Value::Nil => Err(mlua::Error::runtime(format!("mesh.build needs `{key}`, a flat list of numbers or a buffer"))),
            v => Ok(v),
        }
    };
    let pos = floats(CALL, &need("positions")?, "positions", 3, None)?;
    let n = pos.len() / 3;
    if n == 0 {
        return Err(mlua::Error::runtime("mesh.build: `positions` is empty"));
    }
    if n > MAX_MESH_VERTICES {
        return Err(mlua::Error::runtime(format!(
            "mesh.build: {n} vertices is more than the {MAX_MESH_VERTICES} one mesh may have"
        )));
    }
    let positions: Vec<[f32; 3]> = pos.as_chunks::<3>().0.to_vec();

    let dynamic = crate::opts::opt_bool(t, CALL, "dynamic")?.unwrap_or(false);
    // A dynamic mesh may start with no triangles: water that is dry for now.
    let indices = indices_of(CALL, &need("indices")?, n, MAX_MESH_INDICES, dynamic)?;
    let index_capacity = match t.raw_get::<Value>("indexCapacity")? {
        Value::Nil => indices.len(),
        v => {
            let cap = match v {
                Value::Number(c) => c,
                Value::Integer(c) => c as f64,
                other => return Err(mlua::Error::runtime(format!("mesh.build: `indexCapacity` is a number of indices, not a {}", other.type_name()))),
            };
            if !dynamic {
                return Err(mlua::Error::runtime(
                    "mesh.build: `indexCapacity` is room for a later mesh.update, so it goes with `dynamic = true`",
                ));
            }
            if cap.fract() != 0.0 || cap < indices.len() as f64 || cap > MAX_MESH_INDICES as f64 {
                return Err(mlua::Error::runtime(format!(
                    "mesh.build: `indexCapacity` is {cap}; it is a whole number from the {} indices given to {MAX_MESH_INDICES}",
                    indices.len()
                )));
            }
            cap as usize
        }
    };

    let (normals, auto_normals) = match t.raw_get::<Value>("normals")? {
        Value::Nil => (smooth_normals(&positions, &indices), true),
        v => (floats(CALL, &v, "normals", 3, Some(n))?.as_chunks::<3>().0.to_vec(), false),
    };
    let uvs = match t.raw_get::<Value>("uvs")? {
        Value::Nil => vec![[0.0, 0.0]; n],
        v => floats(CALL, &v, "uvs", 2, Some(n))?.as_chunks::<2>().0.to_vec(),
    };
    let colors = match t.raw_get::<Value>("colors")? {
        Value::Nil => None,
        v => Some(colors_of(CALL, &v, n)?),
    };
    Ok((MeshRequest { name: String::new(), positions, normals, uvs, colors, indices, index_capacity }, dynamic.then_some(auto_normals)))
}

/// `mesh.update(name, {...})`: check the new values against the mesh and
/// keep them for the driver.
fn update(b: &mut MeshBook, name: &str, spec: &Table) -> mlua::Result<()> {
    let call = format!("mesh.update(\"{name}\")");
    if !b.live.contains_key(name) {
        return Err(mlua::Error::runtime(format!("{call}: there is no such mesh (it was freed, or never built)")));
    }
    let Some(k) = b.kept.get_mut(name) else {
        return Err(mlua::Error::runtime(format!(
            "{call}: this mesh was not built to be updated. Build it with `dynamic = true`, and the engine keeps \
             what it needs to rewrite it in place"
        )));
    };
    crate::opts::check_keys(spec, UPDATE_KEYS, "mesh.update")?;
    let n = k.vertices.positions.len();
    let given = |key: &str| -> mlua::Result<Option<Value>> {
        Ok(match spec.raw_get::<Value>(key)? {
            Value::Nil => None,
            v => Some(v),
        })
    };
    // Everything is read and checked before anything is kept, so a refused
    // update leaves the mesh exactly as it was.
    let positions = given("positions")?.map(|v| floats(&call, &v, "positions", 3, Some(n))).transpose()?;
    let normals = given("normals")?.map(|v| floats(&call, &v, "normals", 3, Some(n))).transpose()?;
    let uvs = given("uvs")?.map(|v| floats(&call, &v, "uvs", 2, Some(n))).transpose()?;
    let colors = given("colors")?.map(|v| colors_of(&call, &v, n)).transpose()?;
    let indices = given("indices")?.map(|v| indices_of(&call, &v, n, k.index_capacity, true)).transpose()?;
    if colors.is_some() && !k.has_colors {
        return Err(mlua::Error::runtime(format!(
            "{call}: this mesh was built without `colors`, so it has none to update. Give it colours when it is built"
        )));
    }
    if positions.is_none() && normals.is_none() && uvs.is_none() && colors.is_none() && indices.is_none() {
        return Err(mlua::Error::runtime(format!(
            "{call}: nothing to update. It takes any of `positions`, `normals`, `uvs`, `colors` and `indices`"
        )));
    }

    let moved = positions.is_some() || indices.is_some();
    if let Some(p) = positions {
        k.vertices.positions.copy_from_slice(p.as_chunks::<3>().0);
        k.dirty_vertices = true;
    }
    if let Some(i) = indices {
        k.indices = i;
        k.dirty_indices = true;
    }
    match normals {
        Some(nm) => {
            k.vertices.normals.copy_from_slice(nm.as_chunks::<3>().0);
            k.auto_normals = false;
            k.dirty_vertices = true;
        }
        None if moved && k.auto_normals => {
            k.vertices.normals = smooth_normals(&k.vertices.positions, &k.indices);
            k.dirty_vertices = true;
        }
        None => {}
    }
    if let Some(u) = uvs {
        k.vertices.uvs.copy_from_slice(u.as_chunks::<2>().0);
        k.dirty_vertices = true;
    }

    // Built this same frame and not yet seen by the driver: the build
    // itself takes the new values, and there is nothing to update.
    if let Some(req) = b.requests.iter_mut().find(|r| r.name == name) {
        req.positions.clone_from(&k.vertices.positions);
        req.normals.clone_from(&k.vertices.normals);
        req.uvs.clone_from(&k.vertices.uvs);
        req.indices.clone_from(&k.indices);
        if let Some(c) = colors {
            req.colors = Some(c);
        }
        (k.dirty_vertices, k.dirty_indices) = (false, false);
        return Ok(());
    }
    if let Some(c) = colors {
        k.dirty_colors = Some(c);
    }
    if !b.updated.iter().any(|u| u == name) {
        b.updated.push(name.to_string());
    }
    Ok(())
}

/// Install the `mesh` global.
pub(crate) fn install(lua: &Lua, book: Rc<RefCell<MeshBook>>) -> mlua::Result<()> {
    let t = lua.create_table()?;
    {
        let book = book.clone();
        t.set(
            "build",
            lua.create_function(move |_, spec: Table| {
                let (mut req, dynamic) = parse_build(&spec)?;
                let mut b = book.borrow_mut();
                b.next += 1;
                req.name = format!("mesh:{}", b.next);
                let bytes = (req.positions.len() * 12 + req.normals.len() * 12 + req.uvs.len() * 8) as u64
                    + req.colors.as_ref().map_or(0, |c| c.len() as u64 * 4)
                    + req.index_capacity as u64 * 4;
                let script = b.running.now();
                let owner = owner_of(&spec, "mesh.build")?;
                b.live.insert(req.name.clone(), Made { script, bytes, owner });
                if let Some(auto_normals) = dynamic {
                    b.kept.insert(
                        req.name.clone(),
                        Kept {
                            vertices: MeshVertices {
                                positions: req.positions.clone(),
                                normals: req.normals.clone(),
                                uvs: req.uvs.clone(),
                            },
                            indices: req.indices.clone(),
                            index_capacity: req.index_capacity,
                            auto_normals,
                            has_colors: req.colors.is_some(),
                            dirty_vertices: false,
                            dirty_indices: false,
                            dirty_colors: None,
                        },
                    );
                }
                let name = req.name.clone();
                b.requests.push(req);
                Ok(name)
            })?,
        )?;
    }
    {
        let book = book.clone();
        t.set(
            "update",
            lua.create_function(move |_, (name, spec): (String, Table)| update(&mut book.borrow_mut(), &name, &spec))?,
        )?;
    }
    t.set(
        "free",
        lua.create_function(move |_, name: String| {
            let mut b = book.borrow_mut();
            if b.live.remove(&name).is_none() {
                return Ok(false);
            }
            b.forget(&name);
            // Built and freed before the driver saw it: it never needs to exist.
            let before = b.requests.len();
            b.requests.retain(|r| r.name != name);
            if b.requests.len() == before {
                b.releases.push(name);
            }
            Ok(true)
        })?,
    )?;
    lua.globals().set("mesh", t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A mesh built from buffers is the same mesh, read in a fraction of
    /// the time** (card 0698). A 45,000-vertex sphere built from flat lists
    /// and from buffers (positions and normals as f32s, indices as u32s from
    /// 0) makes the same request, and the buffers are read at least three
    /// times faster: a list is read a number at a time across into the
    /// engine, a buffer in one copy. A buffer index past the last vertex is
    /// refused, naming it.
    #[test]
    fn a_mesh_from_buffers_is_the_same_mesh_read_much_faster() {
        let (lua, book) = lua();
        lua.load(
            "local n = 150 p, idx = {}, {}\n\
             for j = 0, n do for i = 0, n*2 do local a, b = i / (n*2) * 6.283, j / n * 3.14159\n\
               p[#p+1] = math.sin(b)*math.cos(a) p[#p+1] = math.cos(b) p[#p+1] = math.sin(b)*math.sin(a) end end\n\
             local w = n*2+1\n\
             for j = 0, n-1 do for i = 0, n*2-1 do local k = j*w + i + 1\n\
               idx[#idx+1] = k idx[#idx+1] = k + w idx[#idx+1] = k + 1 idx[#idx+1] = k + 1 idx[#idx+1] = k + w idx[#idx+1] = k + w + 1 end end\n\
             pb = buffer.create(#p * 4) for i = 1, #p do buffer.writef32(pb, (i-1)*4, p[i]) end\n\
             ib = buffer.create(#idx * 4) for i = 1, #idx do buffer.writeu32(ib, (i-1)*4, idx[i] - 1) end",
        )
        .exec()
        .unwrap();
        // Warm both paths once, then time each.
        lua.load("mesh.build{ positions = p, indices = idx, normals = p } mesh.build{ positions = pb, indices = ib, normals = pb }").exec().unwrap();
        let time = |code: &str| {
            let t = std::time::Instant::now();
            for _ in 0..3 {
                lua.load(code).exec().unwrap();
            }
            t.elapsed().as_secs_f64()
        };
        let lists = time("mesh.build{ positions = p, indices = idx, normals = p }");
        let bufs = time("mesh.build{ positions = pb, indices = ib, normals = pb }");
        let reqs = &book.borrow().requests;
        let (a, b) = (&reqs[reqs.len() - 2], &reqs[reqs.len() - 1]);
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.normals, b.normals);
        assert_eq!(a.indices, b.indices);
        assert!(bufs * 3.0 < lists, "buffers took {:.2} ms, lists {:.2} ms", bufs * 1e3, lists * 1e3);
        let e = lua
            .load("local ib = buffer.create(12) buffer.writeu32(ib, 0, 0) buffer.writeu32(ib, 4, 1) buffer.writeu32(ib, 8, 9)\n\
                   mesh.build{ positions = { 0,0,0, 1,0,0, 0,1,0 }, indices = ib }")
            .exec()
            .unwrap_err()
            .to_string();
        assert!(e.contains("u32 2") && e.contains("0 to 2"), "{e}");
    }

    fn lua() -> (Lua, Rc<RefCell<MeshBook>>) {
        let lua = Lua::new();
        let book: Rc<RefCell<MeshBook>> = Rc::default();
        install(&lua, book.clone()).unwrap();
        (lua, book)
    }

    /// **A script builds a mesh and gets a name for it at once.** Indices are
    /// 1-based in Lua and 0-based on the GPU; normals left out are worked out
    /// from the faces; colours are packed to bytes. Freeing it before the
    /// driver ever saw it hands nothing to the driver.
    #[test]
    fn a_built_mesh_is_named_packed_and_freed() {
        let (lua, book) = lua();
        let name: String = lua
            .load("return mesh.build{ positions = {0,0,0, 1,0,0, 0,0,-1}, indices = {1,2,3}, colors = {1,0,0,1, 0,1,0,1, 0,0,1,0.5} }")
            .eval()
            .unwrap();
        assert!(is_built_mesh(&name), "{name}");
        let reqs = book.borrow_mut().take_requests();
        assert_eq!(reqs.len(), 1);
        let r = &reqs[0];
        assert_eq!(r.name, name);
        assert_eq!(r.indices, vec![0, 1, 2]);
        for n in &r.normals {
            assert!((glam::Vec3::from(*n) - glam::Vec3::Y).length() < 1e-6, "a floor faces up: {n:?}");
        }
        assert_eq!(r.colors.as_ref().unwrap()[2], [0, 0, 255, 128]);

        assert!(lua.load(format!("return mesh.free('{name}')")).eval::<bool>().unwrap());
        assert_eq!(book.borrow_mut().take_releases(), vec![name.clone()], "the driver lets go of an uploaded mesh");
        assert!(!lua.load(format!("return mesh.free('{name}')")).eval::<bool>().unwrap(), "freed once");

        let early: String = lua.load("return mesh.build{ positions = {0,0,0, 1,0,0, 0,1,0}, indices = {1,2,3} }").eval().unwrap();
        assert!(lua.load(format!("return mesh.free('{early}')")).eval::<bool>().unwrap());
        assert!(book.borrow_mut().take_requests().is_empty(), "a mesh freed before upload is never uploaded");
        assert!(book.borrow_mut().take_releases().is_empty());
    }

    /// **A dynamic mesh is rewritten in place.** New positions keep the
    /// name, the vertex count and the uvs; normals the engine worked out are
    /// worked out again; colours and a shorter triangle list ride along; and
    /// however many updates a frame makes, the driver is handed one. Nothing
    /// is built or freed on the way.
    #[test]
    fn a_dynamic_mesh_takes_new_vertices_colours_and_triangles_in_place() {
        let (lua, book) = lua();
        // A quad on the floor, two triangles.
        let name: String = lua
            .load(
                "m = mesh.build{ positions = {0,0,0, 1,0,0, 0,0,-1, 1,0,-1}, indices = {1,2,3, 2,4,3},\n\
                   uvs = {0,0, 1,0, 0,1, 1,1}, colors = {1,1,1,1, 1,1,1,1, 1,1,1,1, 1,1,1,1}, dynamic = true }\n\
                 return m",
            )
            .eval()
            .unwrap();
        let built = book.borrow_mut().take_requests();
        assert_eq!((built.len(), built[0].index_capacity), (1, 6));
        assert!(book.borrow_mut().take_updates().is_empty(), "a mesh nobody updated has no update");

        // Stood on its edge: the worked-out normals turn with it.
        lua.load("mesh.update(m, { positions = {0,0,0, 1,0,0, 0,1,0, 1,1,0} })").exec().unwrap();
        // The same frame: recoloured, and down to one triangle.
        lua.load("mesh.update(m, { colors = {1,0,0,1, 0,1,0,1, 0,0,1,1, 0,0,0,0.5}, indices = {1,2,3} })").exec().unwrap();
        let updates = book.borrow_mut().take_updates();
        assert_eq!(updates.len(), 1, "two updates of one mesh in a frame are one write");
        let u = &updates[0];
        assert_eq!(u.name, name);
        let v = u.vertices.as_ref().expect("the vertices changed");
        assert_eq!(v.positions, [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]]);
        assert_eq!(v.uvs, [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]], "uvs nobody gave are kept");
        for n in &v.normals[..3] {
            assert!((glam::Vec3::from(*n) - glam::Vec3::Z).length() < 1e-6, "a wall faces out, not up: {n:?}");
        }
        assert_eq!(u.colors.as_ref().unwrap()[3], [0, 0, 0, 128]);
        assert_eq!(u.indices.as_deref(), Some(&[0u32, 1, 2][..]));
        assert_eq!(book.borrow().live.len(), 1, "an update built or freed a mesh");
        assert!(book.borrow_mut().take_requests().is_empty() && book.borrow_mut().take_releases().is_empty());

        // Only what changed is sent: colours alone leave the vertices be.
        lua.load("mesh.update(m, { colors = {1,1,1,1, 1,1,1,1, 1,1,1,1, 1,1,1,1} })").exec().unwrap();
        let u = book.borrow_mut().take_updates().remove(0);
        assert!(u.vertices.is_none() && u.indices.is_none() && u.colors.is_some());

        // Normals a script gives are its own from then on.
        lua.load("mesh.update(m, { normals = {0,1,0, 0,1,0, 0,1,0, 0,1,0} })").exec().unwrap();
        lua.load("mesh.update(m, { positions = {0,0,0, 2,0,0, 0,2,0, 2,2,0} })").exec().unwrap();
        let u = book.borrow_mut().take_updates().remove(0);
        assert_eq!(u.vertices.unwrap().normals[0], [0.0, 1.0, 0.0], "given normals were worked out over");

        // Freed with an update pending: nothing is handed over for it.
        lua.load("mesh.update(m, { positions = {0,0,0, 1,0,0, 0,1,0, 1,1,0} }) mesh.free(m)").exec().unwrap();
        assert!(book.borrow_mut().take_updates().is_empty());
    }

    /// **The triangle count can change, up to the room the mesh was built
    /// with.** A sheet that is dry for now is built with no triangles and
    /// `indexCapacity`; an update fills them in and takes them away again.
    /// And an update made before the driver has seen the build is the build.
    #[test]
    fn a_dynamic_mesh_changes_its_triangle_count_within_its_capacity() {
        let (lua, book) = lua();
        lua.load("m = mesh.build{ positions = {0,0,0, 1,0,0, 0,0,-1, 1,0,-1}, indices = {}, dynamic = true, indexCapacity = 6 }")
            .exec()
            .unwrap();
        // Before the driver looks: the build itself carries the new triangles.
        lua.load("mesh.update(m, { indices = {1,2,3} })").exec().unwrap();
        assert!(book.borrow_mut().take_updates().is_empty(), "an update of an unseen build is the build");
        let req = book.borrow_mut().take_requests().remove(0);
        assert_eq!((req.indices.clone(), req.index_capacity), (vec![0, 1, 2], 6));
        assert!((glam::Vec3::from(req.normals[0]) - glam::Vec3::Y).length() < 1e-6, "{:?}", req.normals[0]);

        lua.load("mesh.update(m, { indices = {1,2,3, 2,4,3} })").exec().unwrap();
        assert_eq!(book.borrow_mut().take_updates()[0].indices.as_ref().unwrap().len(), 6);
        lua.load("mesh.update(m, { indices = {} })").exec().unwrap();
        assert_eq!(book.borrow_mut().take_updates()[0].indices.as_deref(), Some(&[][..]), "dry again draws nothing");
        let e = lua.load("mesh.update(m, { indices = {1,2,3, 2,4,3, 1,2,4} })").exec().unwrap_err().to_string();
        assert!(e.contains("9 indices") && e.contains("6"), "{e}");
        // A buffer of u32s numbers vertices from 0, as in mesh.build.
        lua.load("local ib = buffer.create(12) buffer.writeu32(ib, 0, 0) buffer.writeu32(ib, 4, 3) buffer.writeu32(ib, 8, 2)\n\
                  mesh.update(m, { indices = ib })")
            .exec()
            .unwrap();
        assert_eq!(book.borrow_mut().take_updates()[0].indices.as_deref(), Some(&[0u32, 3, 2][..]));
    }

    /// **An update the mesh cannot take is refused, naming the mesh and what
    /// is wrong, and changes nothing.**
    #[test]
    fn a_mesh_update_that_does_not_fit_says_why_and_changes_nothing() {
        let (lua, book) = lua();
        lua.load(
            "tri = { positions = {0,0,0, 1,0,0, 0,1,0}, indices = {1,2,3} }\n\
             fixed = mesh.build(tri)\n\
             tri.dynamic = true\n\
             plain = mesh.build(tri)\n\
             gone = mesh.build(tri) mesh.free(gone)",
        )
        .exec()
        .unwrap();
        book.borrow_mut().take_requests();
        let err = |src: &str| lua.load(src).exec().unwrap_err().to_string();
        let e = err("mesh.update(plain, { positions = {0,0,0, 1,0,0, 0,1,0, 1,1,0} })");
        assert!(e.contains("mesh:2") && e.contains("`positions` has 4 entries for 3 vertices"), "{e}");
        let e = err("mesh.update(plain, { positions = {0,0,1, 1,0,1, 0,1,1}, uvs = {0,0} })");
        assert!(e.contains("`uvs` has 1 entries for 3 vertices"), "{e}");
        assert!(err("mesh.update(plain, { colors = {1,1,1,1, 1,1,1,1, 1,1,1,1} })").contains("built without `colors`"));
        assert!(err("mesh.update(plain, { indices = {1,2,4} })").contains("numbered 1 to 3"));
        assert!(err("mesh.update(plain, { indices = {1,2,3, 1,2,3} })").contains("6 indices"));
        assert!(err("mesh.update(plain, {})").contains("nothing to update"));
        assert!(err("mesh.update(plain, { position = {} })").contains("position"));
        assert!(err("mesh.update(fixed, { positions = {0,0,0, 1,0,0, 0,1,0} })").contains("dynamic = true"));
        assert!(err("mesh.update(gone, { positions = {0,0,0, 1,0,0, 0,1,0} })").contains("no such mesh"));
        assert!(err("mesh.build{ positions = {0,0,0, 1,0,0, 0,1,0}, indices = {1,2,3}, indexCapacity = 6 }").contains("dynamic = true"));
        assert!(err("mesh.build{ positions = {0,0,0, 1,0,0, 0,1,0}, indices = {1,2,3}, dynamic = true, indexCapacity = 2 }").contains("indexCapacity"));
        assert!(err("mesh.build{ positions = {0,0,0, 1,0,0, 0,1,0}, indices = {} }").contains("three per triangle"));
        // The refused update with a good `positions` beside a bad `uvs` kept neither.
        assert!(book.borrow_mut().take_updates().is_empty(), "a refused update left something behind");
        // The table mesh.build took is an update as it stands.
        lua.load("mesh.update(plain, tri)").exec().unwrap();
        assert_eq!(book.borrow_mut().take_updates().len(), 1);
    }

    /// **Updating a surface costs no more than building it.** A 48 × 48 cell
    /// sheet (2,401 vertices) rewritten from buffers, positions and colours,
    /// against building the same sheet: the update does strictly less, and
    /// is compared with the build in one run so the machine's speed cancels.
    #[test]
    fn updating_a_sheet_costs_no_more_than_building_it() {
        let (lua, book) = lua();
        lua.load(
            "local n = 48\n\
             local w = n + 1\n\
             pb, cb, ib = buffer.create(w*w*12), buffer.create(w*w*16), buffer.create(n*n*24)\n\
             for j = 0, n do for i = 0, n do local k = j*w + i\n\
               buffer.writef32(pb, k*12, i) buffer.writef32(pb, k*12+4, 0) buffer.writef32(pb, k*12+8, j)\n\
               for c = 0, 3 do buffer.writef32(cb, k*16 + c*4, 0.5) end end end\n\
             local q = 0\n\
             for j = 0, n-1 do for i = 0, n-1 do local a = j*w + i\n\
               for _, v in ipairs{ a, a+w, a+1, a+1, a+w, a+w+1 } do buffer.writeu32(ib, q, v) q = q + 4 end end end\n\
             sheet = mesh.build{ positions = pb, colors = cb, indices = ib, dynamic = true }",
        )
        .exec()
        .unwrap();
        book.borrow_mut().take_requests();
        let time = |code: &str, drain: &dyn Fn(&mut MeshBook)| {
            let mut best = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                lua.load(code).exec().unwrap();
                drain(&mut book.borrow_mut());
                best = best.min(t.elapsed().as_secs_f64());
            }
            best
        };
        let build = time("mesh.free(mesh.build{ positions = pb, colors = cb, indices = ib })", &|b| {
            b.take_requests();
            b.take_releases();
        });
        let update = time("mesh.update(sheet, { positions = pb, colors = cb })", &|b| {
            assert_eq!(b.take_updates().len(), 1);
        });
        eprintln!("a 49x49 sheet from buffers: build {:.3} ms, update {:.3} ms", build * 1e3, update * 1e3);
        assert!(update <= build * 1.5 + 20e-6, "an update took {:.3} ms, a build {:.3} ms", update * 1e3, build * 1e3);
        assert_eq!(book.borrow().live.len(), 1, "updates left meshes behind");
    }

    /// **A malformed mesh is refused at the call, saying what is wrong.**
    #[test]
    fn a_malformed_mesh_says_what_is_wrong() {
        let (lua, _) = lua();
        let err = |src: &str| lua.load(src).exec().unwrap_err().to_string();
        let tri = "positions = {0,0,0, 1,0,0, 0,1,0}";
        assert!(err("mesh.build{ indices = {1,2,3} }").contains("needs `positions`"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{1,2}} }}")).contains("three per triangle"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{1,2,4}} }}")).contains("numbered 1 to 3"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{0,1,2}} }}")).contains("numbered 1 to 3"));
        assert!(err("mesh.build{ positions = {0,0,0, 1,0}, indices = {1,1,1} }").contains("whole number of 3s"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{1,2,3}}, uvs = {{0,0}} }}")).contains("1 entries for 3 vertices"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{1,2,3}}, normals = {{0,0,0/0, 0,1,0, 0,1,0}} }}")).contains("finite"));
        assert!(err(&format!("mesh.build{{ {tri}, indices = {{1,2,3}}, colour = {{}} }}")).contains("colour"));
    }
}
