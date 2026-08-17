//! **PLY** — what photogrammetry and 3D scanners produce.
//!
//! Positions, usually per-vertex colour, sometimes normals, and faces that may
//! be triangles or arbitrary polygons. ASCII, binary little-endian and binary
//! big-endian, which is the whole format.
//!
//! **Parsed here rather than pulled from a crate.** The obvious dependency
//! (`ply-rs`) carries `skeptic` as a *build* dependency, which drags eight more
//! crates — a Markdown parser, `cargo_metadata`, a deprecated error library —
//! into every clean build on every platform we ship, purely to run doc tests
//! that are not ours. PLY is a header of typed fields followed by rows of them;
//! that is not worth nine crates.
//!
//! **The colour is the point.** A scan's whole value is usually its per-vertex
//! colour — there is no texture and no material, just millions of coloured
//! points. Dropping it, which a converter written for game assets would do
//! without noticing, turns a photoscan into a grey blob.
//!
//! **PLY has no units and no up axis**, exactly like STL, and for the same
//! reason nothing is scaled: the file does not say, so guessing would be
//! inventing.

use std::path::Path;

use crate::common::{Scene, SubMesh};
use crate::ConvertError;

#[derive(Clone, Copy, PartialEq)]
enum Enc {
    Ascii,
    Le,
    Be,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Ty {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl Ty {
    /// Both spellings of every type. The short names are the original ones and
    /// the sized names are what most modern exporters write; a reader that
    /// knows only one set rejects half of the files in the wild.
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "char" | "int8" => Ty::I8,
            "uchar" | "uint8" => Ty::U8,
            "short" | "int16" => Ty::I16,
            "ushort" | "uint16" => Ty::U16,
            "int" | "int32" => Ty::I32,
            "uint" | "uint32" => Ty::U32,
            "float" | "float32" => Ty::F32,
            "double" | "float64" => Ty::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Ty::I8 | Ty::U8 => 1,
            Ty::I16 | Ty::U16 => 2,
            Ty::I32 | Ty::U32 | Ty::F32 => 4,
            Ty::F64 => 8,
        }
    }
}

struct Prop {
    name: String,
    ty: Ty,
    /// `Some(count type)` when this is a list — `property list uchar int …`.
    list: Option<Ty>,
}

struct Element {
    name: String,
    count: usize,
    props: Vec<Prop>,
}

/// One value, kept as f64 so a caller can ask for it however it likes.
#[derive(Clone, Copy, Default)]
struct Val(f64);

impl Val {
    fn f32(self) -> f32 {
        self.0 as f32
    }
    /// Colour channels are `uchar` 0..255 in nearly every file, but `float`
    /// 0..1 exists. Told apart by the declared TYPE rather than by the range —
    /// a file whose colours happen to all be dark would otherwise be read as
    /// floats and come out white.
    fn u8(self, ty: Ty) -> u8 {
        match ty {
            Ty::F32 | Ty::F64 => (self.0.clamp(0.0, 1.0) * 255.0).round() as u8,
            Ty::U16 | Ty::I16 => (self.0 as i64 >> 8) as u8,
            _ => self.0.clamp(0.0, 255.0) as u8,
        }
    }
}

struct Reader<'a> {
    d: &'a [u8],
    at: usize,
    enc: Enc,
}

impl<'a> Reader<'a> {
    fn scalar(&mut self, ty: Ty) -> Result<Val, ConvertError> {
        match self.enc {
            Enc::Ascii => {
                let tok = self.token()?;
                tok.parse::<f64>()
                    .map(Val)
                    .map_err(|_| ConvertError::Malformed(format!("`{tok}` is not a number")))
            }
            _ => {
                let n = ty.size();
                if self.at + n > self.d.len() {
                    return Err(ConvertError::Malformed(
                        "the file ends in the middle of a value".into(),
                    ));
                }
                let b = &self.d[self.at..self.at + n];
                self.at += n;
                let be = self.enc == Enc::Be;
                let v = match ty {
                    Ty::I8 => b[0] as i8 as f64,
                    Ty::U8 => b[0] as f64,
                    Ty::I16 => {
                        let a = [b[0], b[1]];
                        (if be { i16::from_be_bytes(a) } else { i16::from_le_bytes(a) }) as f64
                    }
                    Ty::U16 => {
                        let a = [b[0], b[1]];
                        (if be { u16::from_be_bytes(a) } else { u16::from_le_bytes(a) }) as f64
                    }
                    Ty::I32 => {
                        let a = [b[0], b[1], b[2], b[3]];
                        (if be { i32::from_be_bytes(a) } else { i32::from_le_bytes(a) }) as f64
                    }
                    Ty::U32 => {
                        let a = [b[0], b[1], b[2], b[3]];
                        (if be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) }) as f64
                    }
                    Ty::F32 => {
                        let a = [b[0], b[1], b[2], b[3]];
                        (if be { f32::from_be_bytes(a) } else { f32::from_le_bytes(a) }) as f64
                    }
                    Ty::F64 => {
                        let a = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
                        if be { f64::from_be_bytes(a) } else { f64::from_le_bytes(a) }
                    }
                };
                Ok(Val(v))
            }
        }
    }

    /// The next whitespace-separated word, for the ASCII flavour.
    fn token(&mut self) -> Result<&'a str, ConvertError> {
        while self.at < self.d.len() && self.d[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        let start = self.at;
        while self.at < self.d.len() && !self.d[self.at].is_ascii_whitespace() {
            self.at += 1;
        }
        if start == self.at {
            return Err(ConvertError::Malformed("the file ends too early".into()));
        }
        std::str::from_utf8(&self.d[start..self.at])
            .map_err(|_| ConvertError::Malformed("the file contains invalid text".into()))
    }
}

/// Split the header from the body, and describe what the body holds.
fn parse_header(bytes: &[u8]) -> Result<(Enc, Vec<Element>, usize), ConvertError> {
    // The header is ASCII and ends at `end_header`; the body may be binary, so
    // only the header may be treated as text.
    let scan = &bytes[..bytes.len().min(64 * 1024)];
    let text = String::from_utf8_lossy(scan);
    let end = text
        .find("end_header")
        .ok_or_else(|| ConvertError::Malformed("this is not a PLY file — no header".into()))?;
    // Past `end_header` and its line ending, however the file spells it.
    let mut body = end + "end_header".len();
    if scan.get(body) == Some(&b'\r') {
        body += 1;
    }
    if scan.get(body) == Some(&b'\n') {
        body += 1;
    }

    let mut enc = None;
    let mut elements: Vec<Element> = Vec::new();
    for line in text[..end].lines() {
        let mut w = line.split_ascii_whitespace();
        match w.next() {
            Some("ply") | Some("comment") | Some("obj_info") | None => {}
            Some("format") => {
                enc = match w.next() {
                    Some("ascii") => Some(Enc::Ascii),
                    Some("binary_little_endian") => Some(Enc::Le),
                    Some("binary_big_endian") => Some(Enc::Be),
                    other => {
                        return Err(ConvertError::Malformed(format!(
                            "unknown PLY format `{}`",
                            other.unwrap_or("")
                        )));
                    }
                };
            }
            Some("element") => {
                let name = w.next().unwrap_or("").to_string();
                let count = w.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                elements.push(Element { name, count, props: Vec::new() });
            }
            Some("property") => {
                let Some(el) = elements.last_mut() else { continue };
                let first = w.next().unwrap_or("");
                if first == "list" {
                    let count_ty = Ty::parse(w.next().unwrap_or(""));
                    let item_ty = Ty::parse(w.next().unwrap_or(""));
                    let name = w.next().unwrap_or("").to_string();
                    if let (Some(c), Some(i)) = (count_ty, item_ty) {
                        el.props.push(Prop { name, ty: i, list: Some(c) });
                    }
                } else if let Some(ty) = Ty::parse(first) {
                    el.props.push(Prop { name: w.next().unwrap_or("").to_string(), ty, list: None });
                }
            }
            _ => {}
        }
    }

    let enc = enc.ok_or_else(|| ConvertError::Malformed("the PLY header has no format line".into()))?;
    Ok((enc, elements, body))
}

pub fn read(src: &Path) -> Result<Scene, ConvertError> {
    let bytes = std::fs::read(src).map_err(|e| ConvertError::Io(e.to_string()))?;
    let (enc, elements, body) = parse_header(&bytes)?;
    let mut r = Reader { d: &bytes, at: body, enc };

    let mut sm = SubMesh {
        name: src
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "mesh".into()),
        base_color: [1.0, 1.0, 1.0, 1.0],
        ..Default::default()
    };
    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut any_color = false;
    let mut ngons = 0usize;

    // **Every element is read in order, including ones we do not want.** PLY is
    // positional: the body is elements back to back with no markers, so an
    // element that is skipped rather than consumed leaves the reader pointing
    // at the middle of it and everything after is nonsense.
    for el in &elements {
        for _ in 0..el.count {
            let mut vals: Vec<(&str, Ty, Val)> = Vec::with_capacity(el.props.len());
            let mut lists: Vec<(&str, Vec<u32>)> = Vec::new();
            for p in &el.props {
                match p.list {
                    Some(count_ty) => {
                        let n = r.scalar(count_ty)?.0 as usize;
                        let mut items = Vec::with_capacity(n.min(1024));
                        for _ in 0..n {
                            items.push(r.scalar(p.ty)?.0 as u32);
                        }
                        lists.push((p.name.as_str(), items));
                    }
                    None => vals.push((p.name.as_str(), p.ty, r.scalar(p.ty)?)),
                }
            }

            let get = |name: &str| vals.iter().find(|(n, _, _)| *n == name).map(|(_, t, v)| (*t, *v));

            if el.name == "vertex" {
                let x = get("x").map(|(_, v)| v.f32()).unwrap_or(0.0);
                let y = get("y").map(|(_, v)| v.f32()).unwrap_or(0.0);
                let z = get("z").map(|(_, v)| v.f32()).unwrap_or(0.0);
                sm.positions.push([x, y, z]);

                if let (Some((_, nx)), Some((_, ny)), Some((_, nz))) =
                    (get("nx"), get("ny"), get("nz"))
                {
                    normals.push([nx.f32(), ny.f32(), nz.f32()]);
                }
                // `red`/`green`/`blue` is the standard spelling; `r`/`g`/`b` is
                // what several exporters actually write.
                let rr = get("red").or_else(|| get("r"));
                let gg = get("green").or_else(|| get("g"));
                let bb = get("blue").or_else(|| get("b"));
                match (rr, gg, bb) {
                    (Some((tr, vr)), Some((tg, vg)), Some((tb, vb))) => {
                        let a = get("alpha").map(|(t, v)| v.u8(t)).unwrap_or(255);
                        colors.push([vr.u8(tr), vg.u8(tg), vb.u8(tb), a]);
                        any_color = true;
                    }
                    _ => colors.push([255, 255, 255, 255]),
                }
            } else if el.name == "face" {
                let list = lists
                    .iter()
                    .find(|(n, _)| *n == "vertex_indices" || *n == "vertex_index")
                    .map(|(_, v)| v);
                if let Some(list) = list
                    && list.len() >= 3
                {
                    if list.len() > 3 {
                        ngons += 1;
                    }
                    // Fan-triangulate. Scanner output is convex per face in
                    // practice, and a fan is what every other reader does.
                    for i in 1..list.len() - 1 {
                        sm.indices.push(list[0]);
                        sm.indices.push(list[i]);
                        sm.indices.push(list[i + 1]);
                    }
                }
            }
        }
    }

    if sm.positions.is_empty() {
        return Err(ConvertError::NoGeometry);
    }
    if normals.len() == sm.positions.len() {
        sm.normals = normals;
    }
    if any_color {
        sm.colors = Some(colors);
    }

    // **A PLY with no faces is a point cloud, and that is a real thing to be.**
    // glTF has no point-cloud primitive this writer can emit, so rather than
    // produce a file that opens empty, say what it is and what to do about it.
    if sm.indices.is_empty() {
        return Err(ConvertError::Malformed(format!(
            "`{}` is a point cloud — {} points and no faces. Floptle imports surfaces, so it \
             needs meshing first (MeshLab's Poisson reconstruction, or your scanner's own \
             export-as-mesh).",
            src.file_name().unwrap_or_default().to_string_lossy(),
            sm.positions.len()
        )));
    }

    let mut out = Scene::default();
    if ngons > 0 {
        out.report
            .warnings
            .push(format!("{ngons} face(s) had more than three corners and were split."));
    }
    if any_color {
        out.report.warnings.push("Per-vertex colour carried across.".into());
    }
    out.report
        .warnings
        .push("PLY carries no units, so the model is converted at its original scale.".into());
    out.meshes.push(sm);
    Ok(out)
}
