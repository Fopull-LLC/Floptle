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
//! `mesh.build` answers a name like `"mesh:3"` at once, usable anywhere a
//! model path is. The script host checks and packs the arrays; the driver
//! puts the mesh on the GPU after the pass, so it draws from the next frame.
//! A host that draws nothing still answers a name, and nothing draws it.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

/// The most vertices one built mesh may have. A runtime mesh is a plant or a
/// part, not a level; this is far past either and well inside a GPU buffer.
pub const MAX_MESH_VERTICES: usize = 1 << 20;
/// The most indices: a little over a million triangles.
pub const MAX_MESH_INDICES: usize = 3 << 20;
/// Every key `mesh.build{...}` reads.
pub const BUILD_KEYS: &[&str] = &["positions", "indices", "normals", "uvs", "colors"];

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
}

/// What scripts built and freed, not yet seen by the driver.
#[derive(Default)]
pub(crate) struct MeshBook {
    /// Never reset, so a name is never reused within the process: a node
    /// still wearing a freed mesh draws nothing rather than another mesh.
    next: u64,
    requests: Vec<MeshRequest>,
    releases: Vec<String>,
    live: HashSet<String>,
}

impl MeshBook {
    pub(crate) fn take_requests(&mut self) -> Vec<MeshRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn take_releases(&mut self) -> Vec<String> {
        std::mem::take(&mut self.releases)
    }

    /// Stop: the driver lets go of every built mesh.
    pub(crate) fn reset(&mut self) {
        let next = self.next;
        *self = Self { next, ..Self::default() };
    }
}

/// Is `key` a name `mesh.build` made? Such a key is looked up, never loaded
/// from a file.
pub fn is_built_mesh(key: &str) -> bool {
    key.starts_with("mesh:")
}

/// A flat number array, `per` numbers per vertex, `n` vertices (when known).
fn flat(t: &Table, key: &str, per: usize, n: Option<usize>) -> mlua::Result<Vec<f32>> {
    let len = t.raw_len();
    if !len.is_multiple_of(per) {
        return Err(mlua::Error::runtime(format!(
            "mesh.build: `{key}` holds {len} numbers, not a whole number of {per}s"
        )));
    }
    if let Some(n) = n
        && len / per != n
    {
        return Err(mlua::Error::runtime(format!(
            "mesh.build: `{key}` has {} entries for {n} vertices; it needs one per vertex",
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
                    "mesh.build: `{key}`[{i}] is a {}, not a number",
                    other.type_name()
                )));
            }
        };
        if !v.is_finite() {
            return Err(mlua::Error::runtime(format!("mesh.build: `{key}`[{i}] is not a finite number")));
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

/// Read and check a `mesh.build{...}` table.
pub(crate) fn parse_build(t: &Table) -> mlua::Result<MeshRequest> {
    crate::opts::check_keys(t, BUILD_KEYS, "mesh.build")?;
    let need = |key: &str| -> mlua::Result<Table> {
        t.raw_get::<Option<Table>>(key)?
            .ok_or_else(|| mlua::Error::runtime(format!("mesh.build needs `{key}`, a flat list of numbers")))
    };
    let pos = flat(&need("positions")?, "positions", 3, None)?;
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

    let idx = need("indices")?;
    let count = idx.raw_len();
    if count == 0 || count % 3 != 0 {
        return Err(mlua::Error::runtime(format!(
            "mesh.build: `indices` holds {count} entries; it takes three per triangle"
        )));
    }
    if count > MAX_MESH_INDICES {
        return Err(mlua::Error::runtime(format!(
            "mesh.build: {count} indices is more than the {MAX_MESH_INDICES} one mesh may have"
        )));
    }
    let mut indices = Vec::with_capacity(count);
    for i in 1..=count {
        let v: f64 = idx.raw_get(i)?;
        if v.fract() != 0.0 || v < 1.0 || v > n as f64 {
            return Err(mlua::Error::runtime(format!(
                "mesh.build: `indices`[{i}] is {v}; a vertex is numbered 1 to {n}"
            )));
        }
        indices.push(v as u32 - 1);
    }

    let normals = match t.raw_get::<Option<Table>>("normals")? {
        Some(nt) => flat(&nt, "normals", 3, Some(n))?.as_chunks::<3>().0.to_vec(),
        None => smooth_normals(&positions, &indices),
    };
    let uvs = match t.raw_get::<Option<Table>>("uvs")? {
        Some(ut) => flat(&ut, "uvs", 2, Some(n))?.as_chunks::<2>().0.to_vec(),
        None => vec![[0.0, 0.0]; n],
    };
    let colors = match t.raw_get::<Option<Table>>("colors")? {
        Some(ct) => Some(
            flat(&ct, "colors", 4, Some(n))?
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| c.map(|v| (v.clamp(0.0, 1.0) * 255.0).round() as u8))
                .collect(),
        ),
        None => None,
    };
    Ok(MeshRequest { name: String::new(), positions, normals, uvs, colors, indices })
}

/// Install the `mesh` global.
pub(crate) fn install(lua: &Lua, book: Rc<RefCell<MeshBook>>) -> mlua::Result<()> {
    let t = lua.create_table()?;
    {
        let book = book.clone();
        t.set(
            "build",
            lua.create_function(move |_, spec: Table| {
                let mut req = parse_build(&spec)?;
                let mut b = book.borrow_mut();
                b.next += 1;
                req.name = format!("mesh:{}", b.next);
                b.live.insert(req.name.clone());
                let name = req.name.clone();
                b.requests.push(req);
                Ok(name)
            })?,
        )?;
    }
    t.set(
        "free",
        lua.create_function(move |_, name: String| {
            let mut b = book.borrow_mut();
            if !b.live.remove(&name) {
                return Ok(false);
            }
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
