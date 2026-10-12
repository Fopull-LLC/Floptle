//! **Reading a level made in Tiled (`.tmx`) or LDtk (`.ldtk`).**
//!
//! Both are read into one neutral [`Level`]: the sheets its tiles are cut
//! from, its tile layers as grids of `(sheet, cell, orientation)`, and its
//! objects as named rectangles. `level_apply.rs` turns that into a tileset
//! and tilemap nodes. Pixels throughout; the importer picks the world scale.
//!
//! Tiled's flip flags and LDtk's flip bits both land on the engine's eight
//! orientations exactly, so a mirrored or turned tile looks the same as it
//! did in the editor it came from.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use floptle_core::TileXform;

/// One image tiles are cut from.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sheet {
    /// The image, as an absolute path on this machine.
    pub(crate) image: PathBuf,
    pub(crate) tile_w: u32,
    pub(crate) tile_h: u32,
    /// Pixels round the outside of the sheet, and between tiles.
    pub(crate) margin: u32,
    pub(crate) spacing: u32,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    /// Per-tile class names, by cell, which become tags.
    pub(crate) tags: HashMap<u32, Vec<String>>,
}

/// One square of a layer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Square {
    pub(crate) sheet: usize,
    pub(crate) cell: u32,
    pub(crate) xf: TileXform,
}

/// One grid of squares.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Layer {
    pub(crate) name: String,
    pub(crate) cols: u32,
    pub(crate) rows: u32,
    /// Row-major from the top-left, as the engine's tilemaps are.
    pub(crate) squares: Vec<Option<Square>>,
    /// Where the layer's top-left corner sits, in pixels from the level's.
    pub(crate) offset: (f32, f32),
    pub(crate) visible: bool,
}

/// A placed thing: a Tiled object or an LDtk entity.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Object {
    pub(crate) name: String,
    /// Tiled's class, or LDtk's entity identifier, and its tags.
    pub(crate) tags: Vec<String>,
    /// Top-left corner and size, in pixels from the level's top-left.
    pub(crate) x: f32,
    pub(crate) y: f32,
    pub(crate) w: f32,
    pub(crate) h: f32,
}

/// A whole level.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Level {
    pub(crate) name: String,
    /// The level's grid size, in pixels.
    pub(crate) tile_w: u32,
    pub(crate) tile_h: u32,
    /// Where the level sits in a world of several, in pixels.
    pub(crate) origin: (f32, f32),
    pub(crate) sheets: Vec<Sheet>,
    /// Drawn first to last: the first is at the back.
    pub(crate) layers: Vec<Layer>,
    pub(crate) objects: Vec<Object>,
    /// Things left out, one sentence each.
    pub(crate) warnings: Vec<String>,
}

// --- a small XML reader -----------------------------------------------------
//
// A TMX file is plain XML with no namespaces, DTD or CDATA worth reading, so a
// tree of elements with attributes and text is all it takes.

#[derive(Debug, Default)]
pub(crate) struct Node {
    pub(crate) name: String,
    pub(crate) attrs: Vec<(String, String)>,
    pub(crate) kids: Vec<Node>,
    pub(crate) text: String,
}

impl Node {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }
    fn num(&self, k: &str) -> Option<f64> {
        self.attr(k)?.trim().parse().ok()
    }
    fn u(&self, k: &str) -> u32 {
        self.num(k).unwrap_or(0.0).max(0.0) as u32
    }
    fn kids<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.kids.iter().filter(move |k| k.name == name)
    }
    fn kid(&self, name: &str) -> Option<&Node> {
        self.kids.iter().find(|k| k.name == name)
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';') else { break };
        let ent = &rest[1..end];
        match ent {
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "amp" => out.push('&'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let code = ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()));
                match code.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[..=end]),
                }
            }
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Parse an XML document into its root element.
pub(crate) fn parse_xml(src: &str) -> Result<Node, String> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Node> = vec![Node::default()];
    let bad = || "this is not well-formed XML".to_string();
    while i < b.len() {
        if b[i] != b'<' {
            let end = src[i..].find('<').map(|j| i + j).unwrap_or(b.len());
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&unescape(&src[i..end]));
            }
            i = end;
            continue;
        }
        if src[i..].starts_with("<?") {
            i = src[i..].find("?>").map(|j| i + j + 2).ok_or_else(bad)?;
            continue;
        }
        if src[i..].starts_with("<!--") {
            i = src[i..].find("-->").map(|j| i + j + 3).ok_or_else(bad)?;
            continue;
        }
        if src[i..].starts_with("<![CDATA[") {
            let end = src[i..].find("]]>").map(|j| i + j).ok_or_else(bad)?;
            if let Some(top) = stack.last_mut() {
                top.text.push_str(&src[i + 9..end]);
            }
            i = end + 3;
            continue;
        }
        if src[i..].starts_with("<!") {
            i = src[i..].find('>').map(|j| i + j + 1).ok_or_else(bad)?;
            continue;
        }
        if src[i..].starts_with("</") {
            i = src[i..].find('>').map(|j| i + j + 1).ok_or_else(bad)?;
            let done = stack.pop().ok_or_else(bad)?;
            stack.last_mut().ok_or_else(bad)?.kids.push(done);
            continue;
        }
        // An opening tag: its name, then attributes up to `>` or `/>`.
        i += 1;
        let name_end = src[i..]
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .map(|j| i + j)
            .ok_or_else(bad)?;
        let mut node = Node { name: src[i..name_end].to_string(), ..Default::default() };
        i = name_end;
        loop {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if i >= b.len() {
                return Err(bad());
            }
            if b[i] == b'>' {
                i += 1;
                stack.push(node);
                break;
            }
            if src[i..].starts_with("/>") {
                i += 2;
                stack.last_mut().ok_or_else(bad)?.kids.push(node);
                break;
            }
            let eq = src[i..].find('=').map(|j| i + j).ok_or_else(bad)?;
            let key = src[i..eq].trim().to_string();
            let mut j = eq + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            let q = *b.get(j).ok_or_else(bad)?;
            if q != b'"' && q != b'\'' {
                return Err(bad());
            }
            let close = src[j + 1..].find(q as char).map(|k| j + 1 + k).ok_or_else(bad)?;
            node.attrs.push((key, unescape(&src[j + 1..close])));
            i = close + 1;
        }
    }
    let mut root = stack.pop().ok_or_else(bad)?;
    if !stack.is_empty() {
        return Err("this XML ends before its elements are closed".into());
    }
    root.kids.pop().ok_or_else(|| "this XML has no elements".into())
}

fn base64(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for &c in s.as_bytes() {
        if c.is_ascii_whitespace() || c == b'=' {
            continue;
        }
        acc = (acc << 6) | val(c).ok_or("a layer's base64 data has a character that is not base64")?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Ok(out)
}

// --- orientation ------------------------------------------------------------

/// What an orientation does to a point of the tile, centred, y down.
fn matrix_of(xf: TileXform) -> [i32; 4] {
    // A quarter turn clockwise on screen: (x, y) -> (-y, x).
    let mut m = [1, 0, 0, 1];
    for _ in 0..xf.rot % 4 {
        m = [-m[2], -m[3], m[0], m[1]];
    }
    if xf.flip_x {
        m = [-m[0], -m[1], m[2], m[3]];
    }
    m
}

/// The engine orientation for a flip-and-transpose: transpose first (Tiled's
/// diagonal flag), then mirror left-right, then top-bottom.
pub(crate) fn xform_of_flips(h: bool, v: bool, d: bool) -> TileXform {
    let mut m = [1, 0, 0, 1];
    if d {
        m = [m[2], m[3], m[0], m[1]];
    }
    if h {
        m = [-m[0], -m[1], m[2], m[3]];
    }
    if v {
        m = [m[0], m[1], -m[2], -m[3]];
    }
    for rot in 0..4u8 {
        for flip_x in [false, true] {
            let xf = TileXform::new(rot, flip_x);
            if matrix_of(xf) == m {
                return xf;
            }
        }
    }
    TileXform::default()
}

// --- Tiled ------------------------------------------------------------------

const FLIP_H: u32 = 0x8000_0000;
const FLIP_V: u32 = 0x4000_0000;
const FLIP_D: u32 = 0x2000_0000;
const ROT_HEX: u32 = 0x1000_0000;

struct TiledSet {
    first_gid: u32,
    count: u32,
    sheet: Option<usize>,
}

fn read_text(path: &Path) -> Result<String, String> {
    floptle_vfs::read_to_string(path).map_err(|e| format!("could not read {}: {e}", path.display()))
}

/// Read a Tiled tileset element (inline, or the root of a `.tsx`), whose
/// image paths are relative to `dir`.
fn tiled_sheet(ts: &Node, dir: &Path, warnings: &mut Vec<String>) -> Option<Sheet> {
    let Some(img) = ts.kid("image") else {
        warnings.push(format!(
            "the tileset \"{}\" is a collection of separate images, which is not imported",
            ts.attr("name").unwrap_or("?")
        ));
        return None;
    };
    let tile_w = ts.u("tilewidth");
    let tile_h = ts.u("tileheight");
    let margin = ts.u("margin");
    let spacing = ts.u("spacing");
    let (iw, ih) = (img.u("width"), img.u("height"));
    let fit = |size: u32, tile: u32| {
        if tile == 0 { 0 } else { (size.saturating_sub(2 * margin) + spacing) / (tile + spacing) }
    };
    let cols = match ts.u("columns") {
        0 => fit(iw, tile_w),
        c => c,
    };
    let count = ts.u("tilecount");
    let rows = if cols > 0 && count > 0 { count.div_ceil(cols) } else { fit(ih, tile_h) };
    let mut tags: HashMap<u32, Vec<String>> = HashMap::new();
    for t in ts.kids("tile") {
        let class = t.attr("class").or_else(|| t.attr("type")).unwrap_or("").trim();
        if !class.is_empty() {
            tags.entry(t.u("id")).or_default().push(class.to_string());
        }
    }
    Some(Sheet {
        image: dir.join(img.attr("source").unwrap_or("")),
        tile_w,
        tile_h,
        margin,
        spacing,
        cols: cols.max(1),
        rows: rows.max(1),
        tags,
    })
}

/// The gids of a `<data>` element, row-major, plus its chunks for an infinite
/// map as `(x, y, w, h, gids)`.
/// A run of gids: its top-left square, its width and height (zero for a
/// whole finite layer), and the gids row-major.
type Chunk = (i32, i32, u32, u32, Vec<u32>);

fn tiled_data(data: &Node) -> Result<Vec<Chunk>, String> {
    let decode = |n: &Node| -> Result<Vec<u32>, String> {
        match data.attr("encoding") {
            Some("csv") => Ok(n
                .text
                .split(',')
                .filter_map(|v| v.trim().parse::<u32>().ok())
                .collect()),
            Some("base64") => {
                let raw = base64(n.text.trim())?;
                let bytes = match data.attr("compression") {
                    None | Some("") => raw,
                    Some("zlib") => {
                        let mut out = Vec::new();
                        flate2::read::ZlibDecoder::new(&raw[..])
                            .read_to_end(&mut out)
                            .map_err(|e| format!("a layer's zlib data does not decompress: {e}"))?;
                        out
                    }
                    Some("gzip") => {
                        let mut out = Vec::new();
                        flate2::read::GzDecoder::new(&raw[..])
                            .read_to_end(&mut out)
                            .map_err(|e| format!("a layer's gzip data does not decompress: {e}"))?;
                        out
                    }
                    Some(other) => {
                        return Err(format!(
                            "a layer is compressed with {other}, which is not read. In Tiled, set the map's \
                             Tile Layer Format to CSV, or Base64 with zlib or gzip."
                        ))
                    }
                };
                Ok(bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect())
            }
            _ => Ok(n.kids("tile").map(|t| t.num("gid").unwrap_or(0.0) as u32).collect()),
        }
    };
    let chunks: Vec<&Node> = data.kids("chunk").collect();
    if chunks.is_empty() {
        return Ok(vec![(0, 0, 0, 0, decode(data)?)]);
    }
    chunks
        .into_iter()
        .map(|c| {
            Ok((
                c.num("x").unwrap_or(0.0) as i32,
                c.num("y").unwrap_or(0.0) as i32,
                c.u("width"),
                c.u("height"),
                decode(c)?,
            ))
        })
        .collect()
}

/// Read a Tiled `.tmx` map. `path` locates the tilesets and images it names.
pub(crate) fn read_tmx(src: &str, path: &Path) -> Result<Level, String> {
    let map = parse_xml(src)?;
    if map.name != "map" {
        return Err("this is not a Tiled map (its root is not <map>)".into());
    }
    if map.attr("orientation").is_some_and(|o| o != "orthogonal") {
        return Err(format!(
            "this map is {}; only orthogonal maps import, as the engine's tile layers are square grids",
            map.attr("orientation").unwrap_or("?")
        ));
    }
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut warnings = Vec::new();
    let mut sheets = Vec::new();
    let mut sets: Vec<TiledSet> = Vec::new();
    for ts in map.kids("tileset") {
        let first_gid = ts.u("firstgid");
        let (node, ts_dir);
        let owned;
        if let Some(source) = ts.attr("source") {
            let tsx = dir.join(source);
            owned = parse_xml(&read_text(&tsx)?)?;
            node = &owned;
            ts_dir = tsx.parent().map(Path::to_path_buf).unwrap_or_default();
        } else {
            node = ts;
            ts_dir = dir.to_path_buf();
        }
        let sheet = tiled_sheet(node, &ts_dir, &mut warnings);
        let count = match node.u("tilecount") {
            0 => sheet.as_ref().map(|s| s.cols * s.rows).unwrap_or(0),
            n => n,
        };
        let sheet = sheet.map(|s| {
            sheets.push(s);
            sheets.len() - 1
        });
        sets.push(TiledSet { first_gid, count, sheet });
    }
    sets.sort_by_key(|s| s.first_gid);

    let (tile_w, tile_h) = (map.u("tilewidth"), map.u("tileheight"));
    if tile_w == 0 || tile_h == 0 {
        return Err("this map has no tile size".into());
    }
    struct Out {
        layers: Vec<Layer>,
        objects: Vec<Object>,
        warnings: Vec<String>,
    }
    // Group layers nest; their offsets and visibility add up down the tree.
    fn walk(n: &Node, off: (f32, f32), shown: bool, sets: &[TiledSet], tile: (u32, u32), out: &mut Out) -> Result<(), String> {
        for k in &n.kids {
            let off = (
                off.0 + k.num("offsetx").unwrap_or(0.0) as f32,
                off.1 + k.num("offsety").unwrap_or(0.0) as f32,
            );
            let shown = shown && k.attr("visible") != Some("0");
            let name = k.attr("name").unwrap_or("Layer").to_string();
            match k.name.as_str() {
                "group" => walk(k, off, shown, sets, tile, out)?,
                "layer" => {
                    let Some(data) = k.kid("data") else { continue };
                    let chunks = tiled_data(data)?;
                    let infinite = chunks.len() > 1 || chunks[0].2 > 0;
                    let (x0, y0, x1, y1) = if infinite {
                        chunks.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |a, c| {
                            (a.0.min(c.0), a.1.min(c.1), a.2.max(c.0 + c.2 as i32), a.3.max(c.1 + c.3 as i32))
                        })
                    } else {
                        (0, 0, k.u("width") as i32, k.u("height") as i32)
                    };
                    let (cols, rows) = ((x1 - x0).max(0) as u32, (y1 - y0).max(0) as u32);
                    let mut squares = vec![None; (cols * rows) as usize];
                    let mut rotated_hex = false;
                    for (cx, cy, cw, _, gids) in &chunks {
                        let cw = if infinite { *cw } else { cols };
                        for (i, &raw) in gids.iter().enumerate() {
                            let gid = raw & !(FLIP_H | FLIP_V | FLIP_D | ROT_HEX);
                            if gid == 0 {
                                continue;
                            }
                            rotated_hex |= raw & ROT_HEX != 0;
                            let x = cx + (i as u32 % cw.max(1)) as i32 - x0;
                            let y = cy + (i as u32 / cw.max(1)) as i32 - y0;
                            if x < 0 || y < 0 || x >= cols as i32 || y >= rows as i32 {
                                continue;
                            }
                            let Some(set) = sets.iter().rev().find(|s| s.first_gid <= gid) else { continue };
                            let (Some(sheet), local) = (set.sheet, gid - set.first_gid) else { continue };
                            if local >= set.count.max(1) {
                                continue;
                            }
                            squares[(y as u32 * cols + x as u32) as usize] = Some(Square {
                                sheet,
                                cell: local,
                                xf: xform_of_flips(raw & FLIP_H != 0, raw & FLIP_V != 0, raw & FLIP_D != 0),
                            });
                        }
                    }
                    if rotated_hex {
                        out.warnings.push(format!("layer \"{name}\" has hexagonal turns, which are left out"));
                    }
                    out.layers.push(Layer {
                        name,
                        cols,
                        rows,
                        squares,
                        offset: (off.0 + x0 as f32 * tile.0 as f32, off.1 + y0 as f32 * tile.1 as f32),
                        visible: shown,
                    });
                }
                "objectgroup" => {
                    for o in k.kids("object") {
                        let class = o.attr("class").or_else(|| o.attr("type")).unwrap_or("");
                        let mut tags = vec![name.clone()];
                        if !class.is_empty() {
                            tags.push(class.to_string());
                        }
                        let (w, h) = (o.num("width").unwrap_or(0.0) as f32, o.num("height").unwrap_or(0.0) as f32);
                        let mut y = o.num("y").unwrap_or(0.0) as f32 + off.1;
                        // A tile object is anchored at its bottom-left.
                        if o.attr("gid").is_some() {
                            y -= h;
                        }
                        out.objects.push(Object {
                            name: o.attr("name").filter(|n| !n.is_empty()).unwrap_or(if class.is_empty() { "Object" } else { class }).to_string(),
                            tags,
                            x: o.num("x").unwrap_or(0.0) as f32 + off.0,
                            y,
                            w,
                            h,
                        });
                    }
                }
                "imagelayer" => out.warnings.push(format!("image layer \"{name}\" is left out")),
                _ => {}
            }
        }
        Ok(())
    }
    let mut out = Out { layers: Vec::new(), objects: Vec::new(), warnings };
    walk(&map, (0.0, 0.0), true, &sets, (tile_w, tile_h), &mut out)?;
    let Out { layers, objects, warnings } = out;
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "level".into());
    Ok(Level { name, tile_w, tile_h, origin: (0.0, 0.0), sheets, layers, objects, warnings })
}

// --- LDtk -------------------------------------------------------------------

/// Read an LDtk project: one [`Level`] per level in it, all sharing its
/// tilesets. Levels saved in separate files are read from beside it.
pub(crate) fn read_ldtk(src: &str, path: &Path) -> Result<Vec<Level>, String> {
    use serde_json::Value;
    let v: Value = serde_json::from_str(src).map_err(|e| format!("this is not an LDtk project: {e}"))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let defs = v.get("defs").ok_or("this is not an LDtk project (it has no defs)")?;
    let n = |o: &Value, k: &str| o.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let s = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").to_string();

    // Tilesets by uid, as sheets.
    let mut sheet_of: HashMap<i64, Sheet> = HashMap::new();
    for t in defs.get("tilesets").and_then(Value::as_array).into_iter().flatten() {
        let rel = s(t, "relPath");
        if rel.is_empty() {
            continue; // an embedded atlas, such as LDtk's own icons
        }
        let grid = n(t, "tileGridSize") as u32;
        let mut tags: HashMap<u32, Vec<String>> = HashMap::new();
        for e in t.get("enumTags").and_then(Value::as_array).into_iter().flatten() {
            let tag = s(e, "enumValueId");
            for id in e.get("tileIds").and_then(Value::as_array).into_iter().flatten() {
                if let Some(id) = id.as_u64() {
                    tags.entry(id as u32).or_default().push(tag.clone());
                }
            }
        }
        sheet_of.insert(
            t.get("uid").and_then(Value::as_i64).unwrap_or(-1),
            Sheet {
                image: dir.join(rel),
                tile_w: grid,
                tile_h: grid,
                margin: n(t, "padding") as u32,
                spacing: n(t, "spacing") as u32,
                cols: (n(t, "__cWid") as u32).max(1),
                rows: (n(t, "__cHei") as u32).max(1),
                tags,
            },
        );
    }

    let grid = n(&v, "defaultGridSize").max(1.0) as u32;
    let mut levels = Vec::new();
    for lv in v.get("levels").and_then(Value::as_array).into_iter().flatten() {
        let owned;
        let lv = match lv.get("externalRelPath").and_then(Value::as_str) {
            Some(rel) if lv.get("layerInstances").is_none_or(Value::is_null) => {
                let p = dir.join(rel);
                owned = serde_json::from_str::<Value>(&read_text(&p)?)
                    .map_err(|e| format!("{} is not an LDtk level: {e}", p.display()))?;
                &owned
            }
            _ => lv,
        };
        let mut warnings = Vec::new();
        let mut sheets: Vec<Sheet> = Vec::new();
        let mut sheet_ix: HashMap<i64, usize> = HashMap::new();
        let mut layers = Vec::new();
        let mut objects = Vec::new();
        let mut tile_size = 0;
        // LDtk lists layers top first; the engine draws first at the back.
        let insts: Vec<&Value> = lv.get("layerInstances").and_then(Value::as_array).into_iter().flatten().collect();
        for li in insts.into_iter().rev() {
            let name = s(li, "__identifier");
            let visible = li.get("visible").and_then(Value::as_bool).unwrap_or(true);
            let off = (n(li, "__pxTotalOffsetX") as f32, n(li, "__pxTotalOffsetY") as f32);
            let g = n(li, "__gridSize").max(1.0) as u32;
            let (cols, rows) = (n(li, "__cWid") as u32, n(li, "__cHei") as u32);
            match s(li, "__type").as_str() {
                "Entities" => {
                    for e in li.get("entityInstances").and_then(Value::as_array).into_iter().flatten() {
                        let id = s(e, "__identifier");
                        let px = e.get("px").and_then(Value::as_array);
                        let at = |i: usize| px.and_then(|p| p.get(i)).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                        let (w, h) = (n(e, "width") as f32, n(e, "height") as f32);
                        let pivot = e.get("__pivot").and_then(Value::as_array);
                        let pv = |i: usize| pivot.and_then(|p| p.get(i)).and_then(Value::as_f64).unwrap_or(0.0) as f32;
                        let mut tags = vec![id.clone()];
                        for t in e.get("__tags").and_then(Value::as_array).into_iter().flatten() {
                            if let Some(t) = t.as_str() {
                                tags.push(t.to_string());
                            }
                        }
                        objects.push(Object {
                            name: id,
                            tags,
                            x: at(0) - pv(0) * w + off.0,
                            y: at(1) - pv(1) * h + off.1,
                            w,
                            h,
                        });
                    }
                }
                _ => {
                    let uid = li
                        .get("overrideTilesetUid")
                        .and_then(Value::as_i64)
                        .or_else(|| li.get("__tilesetDefUid").and_then(Value::as_i64));
                    let tiles: Vec<&Value> = ["gridTiles", "autoLayerTiles"]
                        .iter()
                        .flat_map(|k| li.get(*k).and_then(Value::as_array).into_iter().flatten())
                        .collect();
                    if tiles.is_empty() {
                        continue; // an IntGrid with no art: the level's logic, not its picture
                    }
                    let Some(uid) = uid else { continue };
                    let Some(sheet) = sheet_of.get(&uid) else {
                        warnings.push(format!("layer \"{name}\" uses a tileset with no image file, and is left out"));
                        continue;
                    };
                    let si = *sheet_ix.entry(uid).or_insert_with(|| {
                        sheets.push(sheet.clone());
                        sheets.len() - 1
                    });
                    tile_size = tile_size.max(g);
                    // LDtk stacks several tiles in one square on auto-layers;
                    // each extra one goes on a layer of its own above.
                    let mut stacks: Vec<Vec<Option<Square>>> = Vec::new();
                    let sheet_cols = sheet.cols.max(1);
                    for t in tiles {
                        let px = t.get("px").and_then(Value::as_array);
                        let at = |i: usize| px.and_then(|p| p.get(i)).and_then(Value::as_f64).unwrap_or(0.0);
                        let (x, y) = ((at(0) / g as f64) as i64, (at(1) / g as f64) as i64);
                        if x < 0 || y < 0 || x >= cols as i64 || y >= rows as i64 {
                            continue;
                        }
                        let src = t.get("src").and_then(Value::as_array);
                        let sat = |i: usize| src.and_then(|p| p.get(i)).and_then(Value::as_f64).unwrap_or(0.0);
                        let step = (sheet.tile_w + sheet.spacing).max(1) as f64;
                        let cell = match t.get("t").and_then(Value::as_u64) {
                            Some(id) => id as u32,
                            None => {
                                let cx = ((sat(0) - sheet.margin as f64) / step) as u32;
                                let cy = ((sat(1) - sheet.margin as f64) / step) as u32;
                                cy * sheet_cols + cx
                            }
                        };
                        let f = t.get("f").and_then(Value::as_u64).unwrap_or(0);
                        let sq = Square { sheet: si, cell, xf: xform_of_flips(f & 1 != 0, f & 2 != 0, false) };
                        let i = (y as u32 * cols + x as u32) as usize;
                        match stacks.iter_mut().find(|s| s[i].is_none()) {
                            Some(s) => s[i] = Some(sq),
                            None => {
                                let mut s = vec![None; (cols * rows) as usize];
                                s[i] = Some(sq);
                                stacks.push(s);
                            }
                        }
                    }
                    for (k, squares) in stacks.into_iter().enumerate() {
                        layers.push(Layer {
                            name: if k == 0 { name.clone() } else { format!("{name} {}", k + 1) },
                            cols,
                            rows,
                            squares,
                            offset: off,
                            visible,
                        });
                    }
                }
            }
        }
        let size = if tile_size > 0 { tile_size } else { grid };
        levels.push(Level {
            name: s(lv, "identifier"),
            tile_w: size,
            tile_h: size,
            origin: (n(lv, "worldX") as f32, n(lv, "worldY") as f32),
            sheets,
            layers,
            objects,
            warnings,
        });
    }
    if levels.is_empty() {
        return Err("this LDtk project has no levels".into());
    }
    Ok(levels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_reads_elements_attributes_and_text() {
        let n = parse_xml(
            "<?xml version=\"1.0\"?>\n<!-- c --><map a='1' b=\"x &amp; y\"><layer name=\"L\"/><data>1,2</data></map>",
        )
        .unwrap();
        assert_eq!(n.name, "map");
        assert_eq!(n.attr("b"), Some("x & y"));
        assert_eq!(n.kid("layer").unwrap().attr("name"), Some("L"));
        assert_eq!(n.kid("data").unwrap().text, "1,2");
        assert!(parse_xml("<map><layer></map>").is_err() || parse_xml("<map><layer>").is_err());
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("AQAAAAIAAAA=").unwrap(), vec![1, 0, 0, 0, 2, 0, 0, 0]);
    }

    /// Every flip-and-transpose lands on a distinct engine orientation, and the
    /// plain ones on the obvious ones.
    #[test]
    fn flips_map_onto_the_eight_orientations() {
        assert_eq!(xform_of_flips(false, false, false), TileXform::new(0, false));
        assert_eq!(xform_of_flips(true, false, false), TileXform::new(0, true));
        assert_eq!(xform_of_flips(true, true, false), TileXform::new(2, false));
        // Tiled's "rotate clockwise" is diagonal + horizontal.
        assert_eq!(xform_of_flips(true, false, true), TileXform::new(1, false));
        let mut seen = std::collections::HashSet::new();
        for i in 0..8 {
            seen.insert(xform_of_flips(i & 1 != 0, i & 2 != 0, i & 4 != 0));
        }
        assert_eq!(seen.len(), 8);
    }

    const TMX: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<map version="1.10" orientation="orthogonal" width="3" height="2" tilewidth="8" tileheight="8" infinite="0">
 <tileset firstgid="1" name="ground" tilewidth="8" tileheight="8" tilecount="4" columns="2">
  <image source="ground.png" width="16" height="16"/>
  <tile id="3" class="water"/>
 </tileset>
 <layer id="1" name="Back" width="3" height="2">
  <data encoding="csv">
1,0,2,
0,2147483652,0
</data>
 </layer>
 <objectgroup id="2" name="Spawns">
  <object id="1" name="Player" type="spawn" x="4" y="12" width="8" height="8"/>
 </objectgroup>
</map>"#;

    #[test]
    fn a_tiled_map_reads_its_layers_sheets_and_objects() {
        let lv = read_tmx(TMX, Path::new("/proj/levels/one.tmx")).unwrap();
        assert_eq!((lv.tile_w, lv.tile_h, lv.name.as_str()), (8, 8, "one"));
        assert_eq!(lv.sheets.len(), 1);
        let s = &lv.sheets[0];
        assert_eq!(s.image, PathBuf::from("/proj/levels/ground.png"));
        assert_eq!((s.cols, s.rows), (2, 2));
        assert_eq!(s.tags.get(&3), Some(&vec!["water".to_string()]));
        let l = &lv.layers[0];
        assert_eq!((l.cols, l.rows, l.name.as_str()), (3, 2, "Back"));
        assert_eq!(l.squares[0].map(|q| q.cell), Some(0));
        assert_eq!(l.squares[1], None);
        assert_eq!(l.squares[2].map(|q| q.cell), Some(1));
        // gid 4 with the horizontal flip bit.
        let flipped = l.squares[4].unwrap();
        assert_eq!((flipped.cell, flipped.xf), (3, TileXform::new(0, true)));
        let o = &lv.objects[0];
        assert_eq!((o.name.as_str(), o.x, o.y), ("Player", 4.0, 12.0));
        assert!(o.tags.contains(&"spawn".to_string()));
    }

    #[test]
    fn base64_zlib_layers_read_the_same_as_csv() {
        use std::io::Write;
        let gids: Vec<u8> = [1u32, 0, 2, 0, 2147483652, 0].iter().flat_map(|g| g.to_le_bytes()).collect();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&gids).unwrap();
        let packed = z.finish().unwrap();
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut b64 = String::new();
        for c in packed.chunks(3) {
            let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
            for k in 0..4 {
                if k <= c.len() {
                    b64.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
                } else {
                    b64.push('=');
                }
            }
        }
        let csv_start = TMX.find("<data").unwrap();
        let csv_end = TMX.find("</data>").unwrap() + 7;
        let tmx = format!(
            "{}<data encoding=\"base64\" compression=\"zlib\">{b64}</data>{}",
            &TMX[..csv_start],
            &TMX[csv_end..]
        );
        let a = read_tmx(TMX, Path::new("one.tmx")).unwrap();
        let b = read_tmx(&tmx, Path::new("one.tmx")).unwrap();
        assert_eq!(a.layers, b.layers);
    }

    #[test]
    fn an_isometric_map_says_why_it_is_refused() {
        let e = read_tmx(&TMX.replace("orthogonal", "isometric"), Path::new("a.tmx")).unwrap_err();
        assert!(e.contains("isometric"), "{e}");
    }

    const LDTK: &str = r#"{
      "defaultGridSize": 8,
      "defs": { "tilesets": [
        { "uid": 7, "relPath": "art/tiles.png", "tileGridSize": 8, "spacing": 0, "padding": 0,
          "__cWid": 4, "__cHei": 2, "enumTags": [ { "enumValueId": "Spikes", "tileIds": [5] } ] }
      ] },
      "levels": [ { "identifier": "Level_0", "worldX": 0, "worldY": 0, "layerInstances": [
        { "__identifier": "Entities", "__type": "Entities", "__gridSize": 8, "__cWid": 4, "__cHei": 3,
          "__pxTotalOffsetX": 0, "__pxTotalOffsetY": 0,
          "entityInstances": [ { "__identifier": "Player", "px": [12, 16], "width": 8, "height": 8,
                                 "__pivot": [0.5, 1], "__tags": ["hero"] } ] },
        { "__identifier": "Walls", "__type": "AutoLayer", "__gridSize": 8, "__cWid": 4, "__cHei": 3,
          "__pxTotalOffsetX": 0, "__pxTotalOffsetY": 0, "__tilesetDefUid": 7,
          "autoLayerTiles": [ { "px": [0, 16], "src": [8, 0], "f": 1, "t": 1 },
                              { "px": [0, 16], "src": [16, 8], "f": 0, "t": 6 } ] }
      ] } ]
    }"#;

    #[test]
    fn an_ldtk_level_reads_its_tiles_stacks_and_entities() {
        let lv = &read_ldtk(LDTK, Path::new("/p/world.ldtk")).unwrap()[0];
        assert_eq!(lv.name, "Level_0");
        assert_eq!(lv.sheets[0].image, PathBuf::from("/p/art/tiles.png"));
        assert_eq!(lv.sheets[0].tags.get(&5), Some(&vec!["Spikes".to_string()]));
        assert_eq!(lv.layers.len(), 2, "two tiles in one square stack onto a second layer");
        let sq = lv.layers[0].squares[2 * 4].unwrap();
        assert_eq!((sq.cell, sq.xf), (1, TileXform::new(0, true)));
        assert_eq!(lv.layers[1].squares[2 * 4].map(|q| q.cell), Some(6));
        let p = &lv.objects[0];
        assert_eq!((p.x, p.y), (8.0, 8.0), "the pivot is taken off: top-left corner");
        assert!(p.tags.contains(&"hero".to_string()));
    }
}
