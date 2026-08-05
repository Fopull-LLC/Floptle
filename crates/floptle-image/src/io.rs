//! The `.flimg` container, and the PNG contract around it.
//!
//! **The document is the source of truth; the PNG is a build artifact.** Saving
//! a `.flimg` always writes the flattened `.png` beside it, so scenes, materials
//! and the asset browser keep referencing PNGs and never learn the document
//! exists — and a project whose `.flimg`s were deleted still builds and ships.
//!
//! The container is shaped like `.tpaint` (magic + version + header + PNG-
//! compressed payloads) for the same reason: layers are mostly flat and PNG
//! shrinks them by an order of magnitude. A version from the FUTURE is
//! **refused, not scrambled**; a version from the past is read with the fields
//! it had, because adding a field must never cost anybody their art.

use std::path::Path;

use crate::adjust::Adjustment;
use crate::doc::{Image, Layer, LayerKind, Mode};
use crate::effect::Effect;
use crate::palette::Palette;
use crate::select::Mask;
use crate::tiles::TileGrid;
use crate::vector::VPath;
use crate::Blend;

const MAGIC: &[u8; 4] = b"FLIM";
/// The layout this build WRITES. Bump it together with a reader that still
/// handles every version back to [`MIN_VERSION`], and append new fields at the
/// end so the older layout's byte offsets are untouched.
///
/// * v1 — the first shipped layout.
/// * v2 — the sheet cell grid ([`V_SHEET`]).
pub const VERSION: u16 = 2;

/// The oldest container this build reads. Versions between this and
/// [`VERSION`] are read with the fields they had — a `.flimg` written before a
/// field existed must keep opening, or adding one costs everybody their art.
const MIN_VERSION: u16 = 1;

/// The version that added the sheet cell grid ([`Image::sheet`]).
const V_SHEET: u16 = 2;

/// The document file extension.
pub const DOC_EXT: &str = "flimg";

// --- little-endian primitives -------------------------------------------

fn put_u16(o: &mut Vec<u8>, v: u16) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_i32(o: &mut Vec<u8>, v: i32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_f32(o: &mut Vec<u8>, v: f32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_str(o: &mut Vec<u8>, s: &str) {
    put_u32(o, s.len() as u32);
    o.extend_from_slice(s.as_bytes());
}
fn put_blob(o: &mut Vec<u8>, b: &[u8]) {
    put_u32(o, b.len() as u32);
    o.extend_from_slice(b);
}

struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.p)?;
        self.p += 1;
        Some(v)
    }
    fn u16(&mut self) -> Option<u16> {
        let v = u16::from_le_bytes(self.b.get(self.p..self.p + 2)?.try_into().ok()?);
        self.p += 2;
        Some(v)
    }
    fn u32(&mut self) -> Option<u32> {
        let v = u32::from_le_bytes(self.b.get(self.p..self.p + 4)?.try_into().ok()?);
        self.p += 4;
        Some(v)
    }
    fn i32(&mut self) -> Option<i32> {
        Some(self.u32()? as i32)
    }
    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(self.u32()?))
    }
    fn str(&mut self) -> Option<String> {
        let n = self.u32()? as usize;
        let s = std::str::from_utf8(self.b.get(self.p..self.p + n)?).ok()?.to_string();
        self.p += n;
        Some(s)
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        let s = self.b.get(self.p..self.p + n)?;
        self.p += n;
        Some(s)
    }
}

// --- PNG helpers ---------------------------------------------------------

/// Encode straight-RGBA8 to PNG bytes.
pub fn encode_png(px: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(w, h, px.to_vec())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .ok()?;
    Some(buf.into_inner())
}

/// Decode any supported image bytes to `(pixels, w, h)`. Format is guessed from
/// the CONTENT, never the extension — the house rule (`floptle_assets::decode`).
pub fn decode_image(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

/// Read an image file from disk to straight RGBA8.
pub fn load_image(path: &Path) -> Option<(Vec<u8>, u32, u32)> {
    decode_image(&std::fs::read(path).ok()?)
}

/// Write straight RGBA8 to `path` as a PNG, creating parent directories.
pub fn save_png(path: &Path, px: &[u8], w: u32, h: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = encode_png(px, w, h)
        .ok_or_else(|| std::io::Error::other("PNG encode failed"))?;
    std::fs::write(path, bytes)
}

fn encode_mask(m: &Mask) -> Option<Vec<u8>> {
    let img = image::GrayImage::from_raw(m.w, m.h, m.data.clone())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img).write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

fn decode_mask(bytes: &[u8]) -> Option<Mask> {
    let img = image::load_from_memory(bytes).ok()?.to_luma8();
    let (w, h) = img.dimensions();
    Some(Mask { w, h, data: img.into_raw() })
}

// --- the container -------------------------------------------------------

/// Serialize a document.
pub fn encode(img: &Image) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(MAGIC);
    // Write the OLDEST layout that can carry this document. A file that uses
    // nothing new stays readable by an older build, so adding a field costs
    // forward compatibility only for the documents that actually use it.
    let version = if img.sheet.is_some() { V_SHEET } else { MIN_VERSION };
    put_u16(&mut o, version);
    put_u32(&mut o, img.w);
    put_u32(&mut o, img.h);
    o.push(match img.mode {
        Mode::Pixel => 0,
        Mode::Painterly => 1,
        Mode::Vector => 2,
    });
    put_u32(&mut o, img.frames as u32);
    put_f32(&mut o, img.fps);
    o.push(u8::from(img.palette_lock) | (u8::from(img.tiling) << 1));
    put_u32(&mut o, img.active as u32);
    // Palette.
    match &img.palette {
        Some(p) => {
            put_str(&mut o, &p.name);
            put_u32(&mut o, p.colors.len() as u32);
            for c in &p.colors {
                o.extend_from_slice(c);
            }
        }
        None => {
            put_str(&mut o, "");
            put_u32(&mut o, 0);
        }
    }
    put_u32(&mut o, img.layers.len() as u32);
    for l in &img.layers {
        put_str(&mut o, &l.name);
        o.push(match &l.kind {
            LayerKind::Raster { .. } => 0,
            LayerKind::Vector { .. } => 1,
            LayerKind::Adjust(_) => 2,
        });
        o.push(Blend::ALL.iter().position(|b| *b == l.blend).unwrap_or(0) as u8);
        put_f32(&mut o, l.opacity);
        o.push(
            u8::from(l.visible)
                | (u8::from(l.locked) << 1)
                | (u8::from(l.clip_below) << 2)
                | (u8::from(l.mask_enabled) << 3),
        );
        put_i32(&mut o, l.offset.0);
        put_i32(&mut o, l.offset.1);
        // Mask (PNG L8, empty when absent).
        match l.mask.as_ref().and_then(encode_mask) {
            Some(b) => put_blob(&mut o, &b),
            None => put_u32(&mut o, 0),
        }
        put_str(&mut o, &ron::to_string(&l.effects).unwrap_or_else(|_| "[]".into()));
        match &l.kind {
            LayerKind::Raster { frames } => {
                put_u32(&mut o, frames.len() as u32);
                for g in frames {
                    let png = encode_png(&g.to_rgba(), g.width(), g.height()).unwrap_or_default();
                    put_blob(&mut o, &png);
                }
            }
            LayerKind::Vector { paths } => {
                put_str(&mut o, &ron::to_string(paths).unwrap_or_else(|_| "[]".into()));
            }
            LayerKind::Adjust(a) => {
                put_str(&mut o, &ron::to_string(a).unwrap_or_default());
            }
        }
    }
    // v2: the sheet cell grid, appended AFTER the layers so a v1 file's bytes
    // are unchanged up to here and the old reader's offsets all still hold.
    if let Some((sc, sr)) = img.sheet {
        put_u32(&mut o, sc);
        put_u32(&mut o, sr);
    }
    o
}

/// Parse a document. `None` for the wrong magic, an unknown version, or a
/// truncated file — refused rather than half-read.
pub fn decode(bytes: &[u8]) -> Option<Image> {
    if bytes.len() < 6 || &bytes[0..4] != MAGIC {
        return None;
    }
    let mut r = Rd { b: bytes, p: 4 };
    let version = r.u16()?;
    if !(MIN_VERSION..=VERSION).contains(&version) {
        return None;
    }
    let w = r.u32()?;
    let h = r.u32()?;
    let mode = match r.u8()? {
        0 => Mode::Pixel,
        1 => Mode::Painterly,
        2 => Mode::Vector,
        _ => return None,
    };
    let frames = r.u32()? as usize;
    let fps = r.f32()?;
    let flags = r.u8()?;
    let active = r.u32()? as usize;
    let pal_name = r.str()?;
    let pal_n = r.u32()? as usize;
    let mut colors = Vec::with_capacity(pal_n);
    for _ in 0..pal_n {
        colors.push([r.u8()?, r.u8()?, r.u8()?, r.u8()?]);
    }
    let palette = (!colors.is_empty()).then_some(Palette { name: pal_name, colors });

    let n_layers = r.u32()? as usize;
    let mut layers = Vec::with_capacity(n_layers);
    for _ in 0..n_layers {
        let name = r.str()?;
        let kind_tag = r.u8()?;
        let blend = *Blend::ALL.get(r.u8()? as usize).unwrap_or(&Blend::Mix);
        let opacity = r.f32()?;
        let f = r.u8()?;
        let offset = (r.i32()?, r.i32()?);
        let mask_bytes = r.blob()?;
        let mask = (!mask_bytes.is_empty()).then(|| decode_mask(mask_bytes)).flatten();
        let effects: Vec<Effect> = ron::from_str(&r.str()?).unwrap_or_default();
        let kind = match kind_tag {
            0 => {
                let nf = r.u32()? as usize;
                let mut grids = Vec::with_capacity(nf);
                for _ in 0..nf {
                    let png = r.blob()?;
                    let g = match decode_image(png) {
                        Some((px, gw, gh)) => TileGrid::from_rgba(gw, gh, &px),
                        None => TileGrid::new(w, h),
                    };
                    grids.push(g);
                }
                if grids.is_empty() {
                    grids.push(TileGrid::new(w, h));
                }
                LayerKind::Raster { frames: grids }
            }
            1 => {
                let paths: Vec<VPath> = ron::from_str(&r.str()?).unwrap_or_default();
                LayerKind::Vector { paths }
            }
            2 => {
                let a: Adjustment = ron::from_str(&r.str()?).ok()?;
                LayerKind::Adjust(a)
            }
            _ => return None,
        };
        layers.push(Layer {
            name,
            kind,
            blend,
            opacity,
            visible: f & 1 != 0,
            locked: f & 2 != 0,
            clip_below: f & 4 != 0,
            mask,
            mask_enabled: f & 8 != 0,
            effects,
            offset,
        });
    }
    if layers.is_empty() {
        layers.push(Layer::raster("Layer 1", w, h));
    }
    // A file written before the sheet grid existed simply has no cell grid;
    // that is the honest answer, not a guessed one.
    let sheet = if version >= V_SHEET {
        match (r.u32()?, r.u32()?) {
            (0, _) | (_, 0) => None,
            (c, s) => Some((c, s)),
        }
    } else {
        None
    };

    Some(Image {
        sheet,
        w,
        h,
        mode,
        active: active.min(layers.len() - 1),
        layers,
        frames: frames.max(1),
        fps: if fps.is_finite() && fps > 0.0 { fps } else { 12.0 },
        palette,
        palette_lock: flags & 1 != 0,
        tiling: flags & 2 != 0,
        selection: None,
    })
}

/// Write `img` to `path` **and** the flattened PNG beside it (the PNG contract).
/// Returns the PNG path actually written.
pub fn save_document(path: &Path, img: &Image) -> std::io::Result<std::path::PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, encode(img))?;
    let png_path = png_path_for(path);
    let flat = crate::composite::flatten(img, 0);
    save_png(&png_path, &flat, img.w, img.h)?;
    Ok(png_path)
}

/// The PNG that belongs to a document path (`art/thing.flimg` → `art/thing.png`).
pub fn png_path_for(doc: &Path) -> std::path::PathBuf {
    doc.with_extension("png")
}

/// The document that belongs to an image path (`art/thing.png` → `art/thing.flimg`),
/// whether or not it exists yet.
pub fn doc_path_for(png: &Path) -> std::path::PathBuf {
    png.with_extension(DOC_EXT)
}

/// Read a document from disk.
pub fn load_document(path: &Path) -> Option<Image> {
    decode(&std::fs::read(path).ok()?)
}

/// Open ANY image path as a document: the sibling `.flimg` if there is one,
/// otherwise the image wrapped as a one-layer document. This is what
/// double-clicking a PNG in the asset browser does.
pub fn open_any(path: &Path, default_mode: Mode) -> Option<Image> {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case(DOC_EXT)) {
        return load_document(path);
    }
    let doc = doc_path_for(path);
    if doc.is_file()
        && let Some(img) = load_document(&doc)
    {
        return Some(img);
    }
    let (px, w, h) = load_image(path)?;
    // A tiny image is almost certainly pixel art; a big one almost certainly
    // isn't. Seed the mode accordingly rather than asking.
    let mode = if w.max(h) <= 128 { Mode::Pixel } else { default_mode };
    Some(Image::from_rgba(w, h, &px, mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adjust::Dither;
    use crate::vector::VNode;
    use crate::Rect;

    fn sample_doc() -> Image {
        let mut img = Image::new(24, 16, Mode::Painterly);
        img.layers[0].grid_mut(0).unwrap().edit_rect(Rect::new(2, 2, 6, 6), |_, _, p| {
            *p = [200, 40, 40, 255]
        });
        img.layers[0].effects.push(Effect::Outline {
            color: [0, 0, 0, 255],
            width: 2,
            outside: true,
        });
        img.layers[0].mask = Some(crate::select::rect_mask(24, 16, Rect::new(0, 0, 12, 16)));
        img.layers[0].offset = (3, -4);
        img.layers[0].blend = Blend::Overlay;
        img.layers[0].opacity = 0.75;
        img.layers[0].locked = true;

        let mut v = Layer::vector("shape");
        v.kind = LayerKind::Vector {
            paths: vec![VPath { nodes: vec![VNode::curve(1.0, 1.0), VNode::corner(9.0, 9.0)], ..Default::default() }],
        };
        img.add_layer(v);
        img.add_layer(Layer::adjust(Adjustment::Quantize {
            palette: Palette { name: "p".into(), colors: vec![[1, 2, 3, 255]] },
            dither: Dither::Ordered,
            amount: 0.8,
        }));
        img.palette = Some(Palette { name: "doc pal".into(), colors: vec![[9, 8, 7, 255]] });
        img.palette_lock = true;
        img.set_frames(3);
        img.set_layer_animated(0, true);
        img
    }

    #[test]
    fn container_round_trips_every_field() {
        let img = sample_doc();
        let back = decode(&encode(&img)).expect("round trip");
        assert_eq!((back.w, back.h), (24, 16));
        assert_eq!(back.mode, Mode::Painterly);
        assert_eq!(back.frames, 3);
        assert_eq!(back.layers.len(), 3);
        assert!(back.palette_lock);
        assert_eq!(back.palette.as_ref().unwrap().name, "doc pal");

        let l = &back.layers[0];
        assert_eq!(l.blend, Blend::Overlay);
        assert!((l.opacity - 0.75).abs() < 1e-3);
        assert!(l.locked && l.visible);
        assert_eq!(l.offset, (3, -4));
        assert_eq!(l.effects.len(), 1);
        assert!(l.mask.is_some());
        assert_eq!(l.grid(0).unwrap().get(4, 4), [200, 40, 40, 255]);
        assert!(l.is_animated());

        assert!(back.layers[1].kind.is_vector());
        match &back.layers[2].kind {
            LayerKind::Adjust(Adjustment::Quantize { dither, amount, palette }) => {
                assert_eq!(*dither, Dither::Ordered);
                assert!((*amount - 0.8).abs() < 1e-3);
                assert_eq!(palette.colors.len(), 1);
            }
            _ => panic!("adjustment layer did not survive"),
        }
    }

    #[test]
    fn garbage_is_refused_not_misread() {
        assert!(decode(b"").is_none());
        assert!(decode(b"NOPE\x01\x00").is_none());
        let mut v = encode(&Image::new(4, 4, Mode::Pixel));
        v[4] = 99; // right magic, wrong version
        assert!(decode(&v).is_none());
        // Truncation is refused too, rather than yielding half a document.
        let full = encode(&sample_doc());
        assert!(decode(&full[..full.len() / 2]).is_none());
    }

    #[test]
    fn saving_writes_the_png_beside_the_document() {
        let dir = std::env::temp_dir().join(format!("flimg-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let doc = dir.join("thing.flimg");
        let mut img = Image::new(8, 8, Mode::Pixel);
        img.layers[0].grid_mut(0).unwrap().fill([10, 200, 30, 255]);
        let png = save_document(&doc, &img).expect("save");
        assert_eq!(png, dir.join("thing.png"));
        assert!(doc.is_file() && png.is_file());
        let (px, w, h) = load_image(&png).expect("read png back");
        assert_eq!((w, h), (8, 8));
        assert_eq!(&px[0..4], &[10, 200, 30, 255]);
        // …and the document reloads.
        let back = load_document(&doc).expect("reload");
        assert_eq!(back.layers[0].grid(0).unwrap().get(0, 0), [10, 200, 30, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_bare_png_wraps_it_in_a_document() {
        let dir = std::env::temp_dir().join(format!("flimg-open-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let png = dir.join("loose.png");
        save_png(&png, &[7, 8, 9, 255, 1, 2, 3, 255], 2, 1).unwrap();
        let img = open_any(&png, Mode::Painterly).expect("wrap");
        assert_eq!(img.layers.len(), 1);
        assert_eq!(img.mode, Mode::Pixel, "a tiny image is pixel art");
        assert_eq!(img.layers[0].grid(0).unwrap().get(0, 0), [7, 8, 9, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn opening_a_png_prefers_its_sibling_document() {
        let dir = std::env::temp_dir().join(format!("flimg-sib-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let doc_path = dir.join("art.flimg");
        let mut img = Image::new(4, 4, Mode::Pixel);
        img.add_raster_layer(); // two layers — proof we got the document, not the PNG
        save_document(&doc_path, &img).unwrap();
        let opened = open_any(&dir.join("art.png"), Mode::Pixel).expect("open");
        assert_eq!(opened.layers.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_helpers_pair_up() {
        assert_eq!(png_path_for(Path::new("a/b.flimg")), Path::new("a/b.png"));
        assert_eq!(doc_path_for(Path::new("a/b.png")), Path::new("a/b.flimg"));
    }

    /// The sheet cell grid is a fact about the art, so it must survive the file.
    #[test]
    fn the_sheet_cell_grid_round_trips() {
        let mut img = sample_doc();
        img.sheet = Some((3, 2));
        let back = decode(&encode(&img)).expect("decodes");
        assert_eq!(back.sheet, Some((3, 2)));
        assert_eq!(back.cell_size(), Some((8, 8)), "24x16 cut 3x2 is 8x8 cells");

        img.sheet = None;
        assert_eq!(decode(&encode(&img)).unwrap().sheet, None);
    }

    /// A grid that does not divide the canvas evenly has no cell size — a 10.6
    /// px cell is a mistake to draw against, not a number to round.
    #[test]
    fn a_grid_that_does_not_divide_the_canvas_has_no_cell_size() {
        let mut img = Image::new(24, 16, Mode::Pixel);
        img.sheet = Some((5, 2));
        assert_eq!(img.cell_size(), None);
        img.sheet = Some((0, 0));
        assert_eq!(img.cell_size(), None);
    }

    /// Adding a field to the container must not cost anybody their art: a file
    /// written by the build before it still opens, with the field defaulted.
    ///
    /// Built by taking a real v2 file, stamping the version back to 1 and
    /// dropping the bytes v1 did not have — which is byte-for-byte what the old
    /// encoder wrote.
    #[test]
    fn a_file_from_the_previous_version_still_opens() {
        let mut img = sample_doc();
        img.sheet = Some((4, 4));
        let mut bytes = encode(&img);
        bytes.truncate(bytes.len() - 8); // the two u32s v1 did not write
        bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
        let back = decode(&bytes).expect("a v1 document must still open");
        assert_eq!(back.sheet, None, "it had no cell grid, and none is invented");
        assert_eq!((back.w, back.h), (img.w, img.h));
        assert_eq!(back.layers.len(), img.layers.len());
    }

    /// A document that uses nothing new is WRITTEN in the older layout, so it
    /// still opens in the build before this one. Adding a field should cost
    /// forward compatibility only for the files that use it.
    #[test]
    fn a_document_with_no_cell_grid_is_written_in_the_older_layout() {
        let plain = sample_doc();
        assert_eq!(plain.sheet, None);
        let bytes = encode(&plain);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), 1, "no new field, no new version");
        assert_eq!(decode(&bytes).unwrap().sheet, None);

        let mut sheeted = sample_doc();
        sheeted.sheet = Some((4, 4));
        let bytes = encode(&sheeted);
        assert_eq!(u16::from_le_bytes([bytes[4], bytes[5]]), VERSION, "it uses the field");
    }

    /// And a version this build has never heard of is still refused outright,
    /// rather than half-read into something that looks like a document.
    #[test]
    fn a_version_from_the_future_is_refused() {
        let mut bytes = encode(&sample_doc());
        bytes[4..6].copy_from_slice(&(VERSION + 1).to_le_bytes());
        assert!(decode(&bytes).is_none());
    }
}
