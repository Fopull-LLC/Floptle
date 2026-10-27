//! Compute shaders from a game script: GPU buffers and textures, and the
//! shaders that read and write them.
//!
//! ```lua
//! local cells = gpu.buffer{ count = 512 * 512, type = "u32" }          -- zeroed
//! local parts = gpu.buffer{ count = 1000, layout = { "pos: vec3", "life: f32" } }
//! local skin  = gpu.texture{ w = 512, h = 512 }                      -- skin.name == "gpu:1"
//! cells:write({ 1, 0, 1 }, 0)              -- numbers, vectors, structs or a byte string
//! cells:read(function(values, err) end)    -- a frame or two later, never waits
//! compute.run("shaders/life.wgsl", "step", {
//!   size = { 512, 512 },                   -- or groups = { 64, 64 }
//!   bind = { cells = cells, skin = skin, before = gpu.read(old) },
//!   params = { rate = 0.5 },
//! })
//! gpu.free(cells)                          -- or cells:free()
//! ```
//!
//! The book is kept here; the driver makes the GPU objects, runs the queued
//! shaders before the frame is drawn, and answers reads without waiting on the
//! GPU. A host that draws nothing (a dedicated server, `floptle run`) makes
//! nothing and answers `nil, "no renderer"`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use floptle_core::gpu::Elem;
use mlua::{Function, Lua, Table, Value};

use crate::{LogLevel, ScriptLog};

/// The answer on a host with no GPU.
pub const NO_RENDERER: &str = "no renderer";

/// What a shader binds under one of its names.
#[derive(Clone, Debug, PartialEq)]
pub enum ComputeBind {
    Buffer(u32),
    /// A texture the shader writes.
    Write(u32),
    /// A texture the shader reads.
    Read(u32),
}

/// What scripts asked the driver for, in the order they asked.
#[derive(Clone, Debug, PartialEq)]
pub enum ComputeRequest {
    Buffer { id: u32, elem: Elem, count: u32 },
    Write { id: u32, first: u32, bytes: Vec<u8> },
    Texture { id: u32, w: u32, h: u32 },
    FreeBuffer(u32),
    FreeTexture(u32),
    Run {
        path: String,
        entry: String,
        /// Workgroups, or the invocations wanted (the driver divides by the
        /// shader's own workgroup size).
        groups: Option<[u32; 3]>,
        size: Option<[u32; 3]>,
        bind: Vec<(String, ComputeBind)>,
        params: Vec<(String, f32)>,
    },
    ReadBuffer { id: u32, token: u64 },
    ReadTexture { id: u32, token: u64 },
}

enum Reading {
    Buffer(Elem),
    Texture,
}

/// What scripts made and asked for, and who is waiting.
#[derive(Default)]
pub(crate) struct ComputeBook {
    next_buffer: u32,
    next_texture: u32,
    next_token: u64,
    requests: Vec<ComputeRequest>,
    waiting: HashMap<u64, (Function, Reading)>,
    answers: Vec<(u64, Result<Vec<u8>, String>)>,
    buffers: HashMap<u32, (Elem, u32)>,
    textures: HashMap<u32, (u32, u32)>,
    limits: Vec<(String, u64)>,
    no_gpu: bool,
}

impl ComputeBook {
    pub(crate) fn take_requests(&mut self) -> Vec<ComputeRequest> {
        std::mem::take(&mut self.requests)
    }

    pub(crate) fn set_can_draw(&mut self, can: bool) {
        self.no_gpu = !can;
    }

    pub(crate) fn set_limits(&mut self, limits: Vec<(String, u64)>) {
        self.limits = limits;
    }

    pub(crate) fn answer(&mut self, token: u64, result: Result<Vec<u8>, String>) {
        self.answers.push((token, result));
    }

    /// Stop, or a scene load: everything made is let go, and the callbacks,
    /// which close over nodes that are gone, are dropped. Numbers are not
    /// reused, so a handle kept from before reaches nothing.
    pub(crate) fn reset(&mut self) {
        self.requests.clear();
        self.waiting.clear();
        self.answers.clear();
        self.buffers.clear();
        self.textures.clear();
    }

    fn limit(&self, name: &str) -> Option<u64> {
        self.limits.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }
}

const BUFFER_KEYS: &[&str] = &["count", "type", "layout"];
const TEXTURE_KEYS: &[&str] = &["w", "h"];
const RUN_KEYS: &[&str] = &["groups", "size", "bind", "params"];

fn err(call: &str, msg: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(format!("{call}: {msg}"))
}

/// The element type of `gpu.buffer`'s options: `type = "f32"` or a struct
/// `layout = { "pos: vec3", "life: f32" }`.
fn parse_elem(call: &str, opts: &Table) -> mlua::Result<Elem> {
    match (opts.get::<Option<String>>("type")?, opts.get::<Option<Table>>("layout")?) {
        (Some(_), Some(_)) => Err(err(call, "give a type or a layout, not both")),
        (Some(t), None) => Elem::parse_scalar(&t)
            .ok_or_else(|| err(call, format!("\"{t}\" is not a type a buffer holds (f32, u32, i32, vec2, vec3, vec4)"))),
        (None, Some(layout)) => {
            let mut fields = Vec::new();
            for line in layout.sequence_values::<String>() {
                let line = line?;
                let (name, ty) = line
                    .split_once(':')
                    .ok_or_else(|| err(call, format!("layout entry \"{line}\" is not \"name: type\"")))?;
                let name = name.trim();
                let valid = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    return Err(err(call, format!("\"{name}\" is not a field name")));
                }
                let elem = Elem::parse_scalar(ty)
                    .ok_or_else(|| err(call, format!("\"{}\" is not a type a field holds", ty.trim())))?;
                fields.push((name.to_string(), elem));
            }
            if fields.is_empty() {
                return Err(err(call, "a layout needs at least one field"));
            }
            Ok(Elem::Struct(fields))
        }
        (None, None) => Ok(Elem::F32),
    }
}

fn lanes(e: &Elem) -> usize {
    match e {
        Elem::Vec2 => 2,
        Elem::Vec3 => 3,
        Elem::Vec4 => 4,
        _ => 1,
    }
}

/// A scalar's four bytes, as the shader reads that type.
fn put(out: &mut Vec<u8>, e: &Elem, v: f64) {
    match e {
        Elem::U32 => out.extend_from_slice(&(v.max(0.0) as u32).to_le_bytes()),
        Elem::I32 => out.extend_from_slice(&(v as i32).to_le_bytes()),
        _ => out.extend_from_slice(&(v as f32).to_le_bytes()),
    }
}

/// The numbers of one vector value: a `vec3`, `{ x, y, z, w }` or `{ 1, 2, 3 }`.
fn vector(v: &Value, n: usize) -> Option<[f64; 4]> {
    if let Some(d) = crate::math_api::vec3_of(v) {
        return Some([d.x, d.y, d.z, 0.0]);
    }
    let Value::Table(t) = v else { return None };
    let mut out = [0.0; 4];
    for (i, k) in ["x", "y", "z", "w"].iter().enumerate().take(n) {
        out[i] = t.get::<Option<f64>>(*k).ok().flatten().or_else(|| t.get::<Option<f64>>(i + 1).ok().flatten())?;
    }
    Some(out)
}

/// One element's bytes, padded to its stride.
fn encode_one(out: &mut Vec<u8>, e: &Elem, v: &Value) -> Result<(), String> {
    let start = out.len();
    match e {
        Elem::F32 | Elem::U32 | Elem::I32 => match v {
            Value::Number(n) => put(out, e, *n),
            Value::Integer(n) => put(out, e, *n as f64),
            other => return Err(format!("a {} is a number, not a {}", type_name(e), other.type_name())),
        },
        Elem::Vec2 | Elem::Vec3 | Elem::Vec4 => {
            let n = lanes(e);
            let c = vector(v, n).ok_or_else(|| format!("a {} is a vector or {{ x, y, ... }}", type_name(e)))?;
            for x in c.iter().take(n) {
                put(out, &Elem::F32, *x);
            }
        }
        Elem::Struct(_) => {
            let Value::Table(t) = v else { return Err("a struct element is a table of its fields".into()) };
            for (name, f, at) in e.field_offsets() {
                out.resize(start + at as usize, 0);
                let fv = t.get::<Value>(name.as_str()).map_err(|e| e.to_string())?;
                if fv.is_nil() {
                    out.resize(start + at as usize + f.size_align().0 as usize, 0);
                } else {
                    encode_one(out, &f, &fv).map_err(|m| format!("field `{name}`: {m}"))?;
                }
            }
        }
    }
    out.resize(start + e.stride() as usize, 0);
    Ok(())
}

fn type_name(e: &Elem) -> &'static str {
    match e {
        Elem::F32 => "f32",
        Elem::U32 => "u32",
        Elem::I32 => "i32",
        Elem::Vec2 => "vec2",
        Elem::Vec3 => "vec3",
        Elem::Vec4 => "vec4",
        Elem::Struct(_) => "struct",
    }
}

/// A write's bytes: a byte string as given, or a list of values. A list of
/// plain numbers into a vector buffer is read as flat lanes.
fn encode(e: &Elem, v: &Value) -> Result<Vec<u8>, String> {
    match v {
        Value::String(s) => {
            let b = s.as_bytes().to_vec();
            if !b.len().is_multiple_of(4) {
                return Err("a byte string is whole 4-byte values".into());
            }
            Ok(b)
        }
        Value::Table(t) => {
            let mut out = Vec::new();
            let flat = matches!(e, Elem::Vec2 | Elem::Vec3 | Elem::Vec4)
                && matches!(t.raw_get::<Value>(1), Ok(Value::Number(_) | Value::Integer(_)));
            if flat {
                let n = lanes(e);
                let nums: Vec<f64> = t.sequence_values::<f64>().collect::<Result<_, _>>().map_err(|e| e.to_string())?;
                for chunk in nums.chunks(n) {
                    let start = out.len();
                    for x in chunk {
                        put(&mut out, &Elem::F32, *x);
                    }
                    out.resize(start + e.stride() as usize, 0);
                }
            } else {
                for (i, item) in t.sequence_values::<Value>().enumerate() {
                    let item = item.map_err(|e| e.to_string())?;
                    encode_one(&mut out, e, &item).map_err(|m| format!("element {}: {m}", i + 1))?;
                }
            }
            Ok(out)
        }
        other => Err(format!("write takes a list of values or a byte string, not a {}", other.type_name())),
    }
}

fn scalar_at(lua: &Lua, e: &Elem, b: &[u8]) -> mlua::Result<Value> {
    let w = [b[0], b[1], b[2], b[3]];
    Ok(match e {
        Elem::U32 => Value::Integer(u32::from_le_bytes(w) as mlua::Integer),
        Elem::I32 => Value::Integer(i32::from_le_bytes(w) as mlua::Integer),
        _ => {
            let _ = lua;
            Value::Number(f32::from_le_bytes(w) as f64)
        }
    })
}

fn decode_one(lua: &Lua, e: &Elem, b: &[u8]) -> mlua::Result<Value> {
    match e {
        Elem::F32 | Elem::U32 | Elem::I32 => scalar_at(lua, e, b),
        Elem::Vec2 | Elem::Vec3 | Elem::Vec4 => {
            let t = lua.create_table()?;
            for (i, k) in ["x", "y", "z", "w"].iter().enumerate().take(lanes(e)) {
                t.set(*k, scalar_at(lua, &Elem::F32, &b[i * 4..])?)?;
            }
            Ok(Value::Table(t))
        }
        Elem::Struct(_) => {
            let t = lua.create_table()?;
            for (name, f, at) in e.field_offsets() {
                t.set(name, decode_one(lua, &f, &b[at as usize..])?)?;
            }
            Ok(Value::Table(t))
        }
    }
}

/// A buffer's bytes back as the list of values a script wrote.
fn decode(lua: &Lua, e: &Elem, bytes: &[u8]) -> mlua::Result<Table> {
    let out = lua.create_table()?;
    for (i, chunk) in bytes.chunks_exact(e.stride() as usize).enumerate() {
        out.raw_set(i + 1, decode_one(lua, e, chunk)?)?;
    }
    Ok(out)
}

fn three(call: &str, what: &str, t: Table) -> mlua::Result<[u32; 3]> {
    let mut out = [1u32; 3];
    let n = t.raw_len();
    if n == 0 || n > 3 {
        return Err(err(call, format!("`{what}` is one to three whole numbers")));
    }
    for (i, slot) in out.iter_mut().enumerate().take(n) {
        let v: f64 = t.raw_get(i + 1)?;
        if v < 0.0 || v.fract() != 0.0 {
            return Err(err(call, format!("`{what}` is whole numbers, not {v}")));
        }
        *slot = v as u32;
    }
    Ok(out)
}

fn buffer_id(t: &Table) -> Option<u32> {
    t.raw_get::<Option<u32>>("__gpubuf").ok().flatten()
}

fn texture_id(t: &Table) -> Option<u32> {
    t.raw_get::<Option<u32>>("__gputex").ok().flatten()
}

pub(crate) fn install(lua: &Lua, state: Rc<RefCell<ComputeBook>>) -> mlua::Result<()> {
    let gpu = lua.create_table()?;
    let compute = lua.create_table()?;

    // The handles' methods, one metatable each.
    let buf_mt = lua.create_table()?;
    let buf_methods = lua.create_table()?;
    let tex_mt = lua.create_table()?;
    let tex_methods = lua.create_table()?;

    let st = state.clone();
    buf_methods.set(
        "write",
        lua.create_function(move |_, (this, values, first): (Table, Value, Option<u32>)| {
            const CALL: &str = "buffer:write";
            let id = buffer_id(&this).ok_or_else(|| err(CALL, "not a buffer"))?;
            let mut s = st.borrow_mut();
            let Some((elem, count)) = s.buffers.get(&id).cloned() else {
                return Err(err(CALL, "this buffer has been freed"));
            };
            let bytes = encode(&elem, &values).map_err(|m| err(CALL, m))?;
            let first = first.unwrap_or(0);
            let room = (count.saturating_sub(first)) as usize * elem.stride() as usize;
            if bytes.len() > room {
                return Err(err(CALL, format!("{} elements from {first} run past the buffer's {count}", bytes.len() / elem.stride() as usize)));
            }
            s.requests.push(ComputeRequest::Write { id, first, bytes });
            Ok(())
        })?,
    )?;
    let st = state.clone();
    buf_methods.set(
        "read",
        lua.create_function(move |_, (this, cb): (Table, Function)| {
            const CALL: &str = "buffer:read";
            let id = buffer_id(&this).ok_or_else(|| err(CALL, "not a buffer"))?;
            let mut s = st.borrow_mut();
            s.next_token += 1;
            let token = s.next_token;
            match s.buffers.get(&id).map(|(e, _)| e.clone()) {
                Some(elem) if !s.no_gpu => {
                    s.waiting.insert(token, (cb, Reading::Buffer(elem)));
                    s.requests.push(ComputeRequest::ReadBuffer { id, token });
                }
                Some(elem) => {
                    s.waiting.insert(token, (cb, Reading::Buffer(elem)));
                    s.answers.push((token, Err(NO_RENDERER.into())));
                }
                None => return Err(err(CALL, "this buffer has been freed")),
            }
            Ok(())
        })?,
    )?;
    let st = state.clone();
    let free_buf = lua.create_function(move |_, this: Table| {
        if let Some(id) = buffer_id(&this) {
            let mut s = st.borrow_mut();
            if s.buffers.remove(&id).is_some() {
                s.requests.push(ComputeRequest::FreeBuffer(id));
            }
        }
        Ok(())
    })?;
    buf_methods.set("free", free_buf.clone())?;
    buf_mt.set("__index", buf_methods)?;

    let st = state.clone();
    tex_methods.set(
        "read",
        lua.create_function(move |_, (this, cb): (Table, Function)| {
            const CALL: &str = "texture:read";
            let id = texture_id(&this).ok_or_else(|| err(CALL, "not a gpu texture"))?;
            let mut s = st.borrow_mut();
            if !s.textures.contains_key(&id) {
                return Err(err(CALL, "this texture has been freed"));
            }
            s.next_token += 1;
            let token = s.next_token;
            s.waiting.insert(token, (cb, Reading::Texture));
            if s.no_gpu {
                s.answers.push((token, Err(NO_RENDERER.into())));
            } else {
                s.requests.push(ComputeRequest::ReadTexture { id, token });
            }
            Ok(())
        })?,
    )?;
    let st = state.clone();
    let free_tex = lua.create_function(move |_, this: Table| {
        if let Some(id) = texture_id(&this) {
            let mut s = st.borrow_mut();
            if s.textures.remove(&id).is_some() {
                s.requests.push(ComputeRequest::FreeTexture(id));
            }
        }
        Ok(())
    })?;
    tex_methods.set("free", free_tex.clone())?;
    tex_mt.set("__index", tex_methods)?;

    // gpu.buffer{ count, type | layout }
    let st = state.clone();
    let bmt = buf_mt.clone();
    gpu.set(
        "buffer",
        lua.create_function(move |lua, opts: Table| {
            const CALL: &str = "gpu.buffer";
            crate::opts::check_keys(&opts, BUFFER_KEYS, CALL)?;
            let count: f64 = opts.get::<Option<f64>>("count")?.ok_or_else(|| err(CALL, "needs `count`"))?;
            if count < 1.0 || count.fract() != 0.0 || count > u32::MAX as f64 {
                return Err(err(CALL, format!("`count` is a whole number of elements, not {count}")));
            }
            let elem = parse_elem(CALL, &opts)?;
            let mut s = st.borrow_mut();
            if s.no_gpu {
                return Ok((Value::Nil, Value::String(lua.create_string(NO_RENDERER)?)));
            }
            let bytes = elem.stride() as u64 * count as u64;
            if let Some(max) = s.limit("maxBufferBytes")
                && bytes > max
            {
                return Err(err(CALL, format!("{bytes} bytes is more than this device binds in one buffer ({max})")));
            }
            s.next_buffer += 1;
            let id = s.next_buffer;
            s.buffers.insert(id, (elem.clone(), count as u32));
            s.requests.push(ComputeRequest::Buffer { id, elem: elem.clone(), count: count as u32 });
            let t = lua.create_table()?;
            t.raw_set("__gpubuf", id)?;
            t.raw_set("count", count as u32)?;
            t.raw_set("type", if matches!(elem, Elem::Struct(_)) { "struct" } else { type_name(&elem) })?;
            t.set_metatable(Some(bmt.clone()));
            Ok((Value::Table(t), Value::Nil))
        })?,
    )?;

    // gpu.texture{ w, h }
    let st = state.clone();
    let tmt = tex_mt.clone();
    gpu.set(
        "texture",
        lua.create_function(move |lua, opts: Table| {
            const CALL: &str = "gpu.texture";
            crate::opts::check_keys(&opts, TEXTURE_KEYS, CALL)?;
            let side = |k: &str| -> mlua::Result<u32> {
                let v: f64 = opts.get::<Option<f64>>(k)?.ok_or_else(|| err(CALL, format!("needs `{k}`")))?;
                if v < 1.0 || v.fract() != 0.0 {
                    return Err(err(CALL, format!("`{k}` is a whole number of pixels, not {v}")));
                }
                Ok(v as u32)
            };
            let (w, h) = (side("w")?, side("h")?);
            let mut s = st.borrow_mut();
            if s.no_gpu {
                return Ok((Value::Nil, Value::String(lua.create_string(NO_RENDERER)?)));
            }
            if let Some(max) = s.limit("maxTextureSize")
                && (w as u64 > max || h as u64 > max)
            {
                return Err(err(CALL, format!("{w}×{h} is more than this device makes ({max} a side)")));
            }
            s.next_texture += 1;
            let id = s.next_texture;
            s.textures.insert(id, (w, h));
            s.requests.push(ComputeRequest::Texture { id, w, h });
            let t = lua.create_table()?;
            t.raw_set("__gputex", id)?;
            t.raw_set("name", format!("gpu:{id}"))?;
            t.raw_set("w", w)?;
            t.raw_set("h", h)?;
            t.set_metatable(Some(tmt.clone()));
            Ok((Value::Table(t), Value::Nil))
        })?,
    )?;

    // gpu.read(texture): bind a texture for a shader to read.
    gpu.set(
        "read",
        lua.create_function(|lua, tex: Table| {
            let id = texture_id(&tex).ok_or_else(|| err("gpu.read", "takes a gpu texture"))?;
            let t = lua.create_table()?;
            t.raw_set("__gpuread", id)?;
            Ok(t)
        })?,
    )?;

    // gpu.free(buffer or texture)
    gpu.set(
        "free",
        lua.create_function(move |_, h: Table| {
            if buffer_id(&h).is_some() {
                free_buf.call::<()>(h)
            } else {
                free_tex.call::<()>(h)
            }
        })?,
    )?;

    // gpu.limits()
    let st = state.clone();
    gpu.set(
        "limits",
        lua.create_function(move |lua, ()| {
            let t = lua.create_table()?;
            for (k, v) in &st.borrow().limits {
                t.set(k.as_str(), *v)?;
            }
            Ok(t)
        })?,
    )?;

    // compute.run(path, entry, { groups | size, bind, params })
    let st = state.clone();
    compute.set(
        "run",
        lua.create_function(move |_, (path, entry, opts): (String, String, Option<Table>)| {
            const CALL: &str = "compute.run";
            let mut s = st.borrow_mut();
            let (mut groups, mut size, mut bind, mut params) = (None, None, Vec::new(), Vec::new());
            if let Some(opts) = &opts {
                crate::opts::check_keys(opts, RUN_KEYS, CALL)?;
                groups = opts.get::<Option<Table>>("groups")?.map(|t| three(CALL, "groups", t)).transpose()?;
                size = opts.get::<Option<Table>>("size")?.map(|t| three(CALL, "size", t)).transpose()?;
                if let Some(b) = opts.get::<Option<Table>>("bind")? {
                    for pair in b.pairs::<String, Table>() {
                        let (name, h) = pair.map_err(|_| err(CALL, "`bind` maps a name to a buffer or texture"))?;
                        let what = if let Some(id) = buffer_id(&h) {
                            if !s.buffers.contains_key(&id) {
                                return Err(err(CALL, format!("`{name}`: that buffer has been freed")));
                            }
                            ComputeBind::Buffer(id)
                        } else if let Some(id) = texture_id(&h) {
                            ComputeBind::Write(id)
                        } else if let Some(id) = h.raw_get::<Option<u32>>("__gpuread")? {
                            ComputeBind::Read(id)
                        } else {
                            return Err(err(CALL, format!("`{name}` is not a gpu buffer or texture")));
                        };
                        bind.push((name, what));
                    }
                }
                if let Some(p) = opts.get::<Option<Table>>("params")? {
                    for pair in p.pairs::<String, f64>() {
                        let (name, v) = pair.map_err(|_| err(CALL, "`params` maps a name to a number"))?;
                        params.push((name, v as f32));
                    }
                }
            }
            if groups.is_some() && size.is_some() {
                return Err(err(CALL, "give groups or size, not both"));
            }
            // A fixed order, so the same call builds the same shader.
            bind.sort_by(|a, b| a.0.cmp(&b.0));
            params.sort_by(|a, b| a.0.cmp(&b.0));
            if !s.no_gpu {
                s.requests.push(ComputeRequest::Run { path, entry, groups, size, bind, params });
            }
            Ok(())
        })?,
    )?;

    lua.globals().set("gpu", gpu)?;
    lua.globals().set("compute", compute)?;
    Ok(())
}

/// Answer the reads that came back, in the frame pass.
pub(crate) fn drain(lua: &Lua, state: &Rc<RefCell<ComputeBook>>, logs: &Rc<RefCell<Vec<ScriptLog>>>) {
    let ready: Vec<(Function, Reading, Result<Vec<u8>, String>)> = {
        let Ok(mut s) = state.try_borrow_mut() else { return };
        let answers = std::mem::take(&mut s.answers);
        answers
            .into_iter()
            .filter_map(|(t, r)| s.waiting.remove(&t).map(|(cb, how)| (cb, how, r)))
            .collect()
    };
    for (cb, how, result) in ready {
        let called = match result {
            Ok(bytes) => match how {
                Reading::Buffer(elem) => decode(lua, &elem, &bytes).and_then(|t| cb.call::<()>((t, Value::Nil))),
                Reading::Texture => lua.create_string(&bytes).and_then(|b| cb.call::<()>((b, Value::Nil))),
            },
            Err(why) => lua.create_string(&why).and_then(|w| cb.call::<()>((Value::Nil, w))),
        };
        if let Err(e) = called {
            logs.borrow_mut().push(ScriptLog { level: LogLevel::Error, msg: format!("gpu read callback: {e}"), source: None });
        }
    }
}
