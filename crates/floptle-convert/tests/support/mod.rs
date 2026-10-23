//! Fixtures written by the tests, as text: an ASCII FBX 7.4 the tests can
//! state the contents of, and small flat-coloured pictures.
//!
//! Hand-written on purpose and kept small. The exporter fixtures under
//! `models/` pin what real exporters do to geometry; these pin the mapping —
//! that a picture plugged into a given slot of a given material arrives in
//! the right glTF slot — which needs a file whose every connection is known.
//! They are generated rather than stored so nothing here is a binary nobody
//! can read, and they carry the repository's own licence.

#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// A fresh, empty folder under the temp dir.
pub fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("floptle-conv-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A PNG of one colour, or of `f(x, y)`.
pub fn png_with(path: &Path, w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).unwrap();
    let bytes = out.into_inner();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, &bytes).unwrap();
    bytes
}

pub fn png(path: &Path, rgba: [u8; 4]) -> Vec<u8> {
    png_with(path, 4, 4, |_, _| rgba)
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in bytes.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

/// One FBX object block and the connections that hang it in the scene.
#[derive(Default)]
pub struct Fbx {
    objects: String,
    connections: String,
    /// Centimetres per file unit: 100 for metres, 1 for centimetres.
    pub unit: f64,
}

impl Fbx {
    pub fn new(unit: f64) -> Self {
        Self { unit, ..Default::default() }
    }
    pub fn object(&mut self, text: &str) -> &mut Self {
        self.objects.push_str(text);
        self.objects.push('\n');
        self
    }
    /// Object-object connection: `child` under `parent` (0 is the scene).
    pub fn oo(&mut self, child: u64, parent: u64) -> &mut Self {
        self.connections.push_str(&format!("\tC: \"OO\",{child},{parent}\n"));
        self
    }
    /// Object-property connection: `child` drives `prop` of `parent`.
    pub fn op(&mut self, child: u64, parent: u64, prop: &str) -> &mut Self {
        self.connections.push_str(&format!("\tC: \"OP\",{child},{parent}, \"{prop}\"\n"));
        self
    }
    pub fn text(&self) -> String {
        format!(
            "; FBX 7.4.0 project file\n\
             FBXHeaderExtension:  {{\n\tFBXHeaderVersion: 1003\n\tFBXVersion: 7400\n\tCreator: \"Floptle test fixture\"\n}}\n\
             GlobalSettings:  {{\n\tVersion: 1000\n\tProperties70:  {{\n\
             \t\tP: \"UpAxis\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"UpAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"FrontAxis\", \"int\", \"Integer\", \"\",2\n\
             \t\tP: \"FrontAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"CoordAxis\", \"int\", \"Integer\", \"\",0\n\
             \t\tP: \"CoordAxisSign\", \"int\", \"Integer\", \"\",1\n\
             \t\tP: \"UnitScaleFactor\", \"double\", \"Number\", \"\",{}\n\
             \t\tP: \"TimeMode\", \"enum\", \"\", \"\",6\n\
             \t}}\n}}\n\
             Objects:  {{\n{}}}\n\
             Connections:  {{\n{}}}\n",
            self.unit, self.objects, self.connections
        )
    }
    pub fn write(&self, path: &Path) {
        std::fs::write(path, self.text()).unwrap();
    }
}

/// An FBX float array: `*N { a: … }`.
pub fn arr(v: &[f64]) -> String {
    let body: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("*{} {{\n\t\t\ta: {}\n\t\t}}", v.len(), body.join(","))
}

pub fn iarr(v: &[i64]) -> String {
    let body: Vec<String> = v.iter().map(|x| format!("{x}")).collect();
    format!("*{} {{\n\t\t\ta: {}\n\t\t}}", v.len(), body.join(","))
}

/// A mesh geometry block: positions, quads (each `[a, b, c, d]`), optional
/// per-corner UVs (one per corner, in quad order) and a material index per
/// quad.
pub fn geometry(id: u64, name: &str, positions: &[[f64; 3]], quads: &[[i64; 4]], uvs: bool, materials: &[i64]) -> String {
    let verts: Vec<f64> = positions.iter().flatten().copied().collect();
    let mut index = Vec::new();
    for q in quads {
        index.extend_from_slice(&[q[0], q[1], q[2], -q[3] - 1]);
    }
    let mut s = format!(
        "\tGeometry: {id}, \"Geometry::{name}\", \"Mesh\" {{\n\t\tVertices: {}\n\t\tPolygonVertexIndex: {}\n\t\tGeometryVersion: 124\n",
        arr(&verts),
        iarr(&index)
    );
    let mut layers = String::new();
    if uvs {
        let uv: Vec<f64> = vec![0.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0];
        let uv_index: Vec<i64> = quads.iter().flat_map(|_| [0, 1, 2, 3]).collect();
        s.push_str(&format!(
            "\t\tLayerElementUV: 0 {{\n\t\t\tVersion: 101\n\t\t\tName: \"UVMap\"\n\t\t\tMappingInformationType: \"ByPolygonVertex\"\n\t\t\tReferenceInformationType: \"IndexToDirect\"\n\t\t\tUV: {}\n\t\t\tUVIndex: {}\n\t\t}}\n",
            arr(&uv),
            iarr(&uv_index)
        ));
        layers.push_str("\t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementUV\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n");
    }
    if !materials.is_empty() {
        s.push_str(&format!(
            "\t\tLayerElementMaterial: 0 {{\n\t\t\tVersion: 101\n\t\t\tName: \"\"\n\t\t\tMappingInformationType: \"ByPolygon\"\n\t\t\tReferenceInformationType: \"IndexToDirect\"\n\t\t\tMaterials: {}\n\t\t}}\n",
            iarr(materials)
        ));
        layers.push_str("\t\t\tLayerElement:  {\n\t\t\t\tType: \"LayerElementMaterial\"\n\t\t\t\tTypedIndex: 0\n\t\t\t}\n");
    }
    s.push_str(&format!("\t\tLayer: 0 {{\n\t\t\tVersion: 100\n{layers}\t\t}}\n\t}}"));
    s
}

/// A model (node) block. `kind` is `Mesh`, `LimbNode` or `Null`.
pub fn model(id: u64, name: &str, kind: &str, translation: [f64; 3]) -> String {
    format!(
        "\tModel: {id}, \"Model::{name}\", \"{kind}\" {{\n\t\tVersion: 232\n\t\tProperties70:  {{\n\t\t\tP: \"Lcl Translation\", \"Lcl Translation\", \"\", \"A\",{},{},{}\n\t\t}}\n\t\tShading: T\n\t\tCulling: \"CullingOff\"\n\t}}",
        translation[0], translation[1], translation[2]
    )
}

/// A file texture, its picture embedded when `content` is given.
pub fn texture(id: u64, video: u64, name: &str, file: &str, relative: &str, content: Option<&[u8]>) -> String {
    let content = content.map(|c| format!("\t\tContent: , \"{}\"\n", base64(c))).unwrap_or_default();
    let file = file.replace('\\', "\\\\");
    let relative = relative.replace('\\', "\\\\");
    format!(
        "\tTexture: {id}, \"Texture::{name}\", \"\" {{\n\t\tType: \"TextureVideoClip\"\n\t\tVersion: 202\n\t\tTextureName: \"Texture::{name}\"\n\t\tMedia: \"Video::{name}\"\n\t\tFileName: \"{file}\"\n\t\tRelativeFilename: \"{relative}\"\n\t}}\n\
         \tVideo: {video}, \"Video::{name}\", \"Clip\" {{\n\t\tType: \"Clip\"\n\t\tFileName: \"{file}\"\n\t\tRelativeFilename: \"{relative}\"\n{content}\t}}"
    )
}

/// A classic Phong material with the given extra properties.
pub fn phong(id: u64, name: &str, props: &str) -> String {
    format!(
        "\tMaterial: {id}, \"Material::{name}\", \"\" {{\n\t\tVersion: 102\n\t\tShadingModel: \"phong\"\n\t\tMultiLayer: 0\n\t\tProperties70:  {{\n{props}\t\t}}\n\t}}"
    )
}

/// One second in FBX time.
pub const SECOND: i64 = 46_186_158_000;
