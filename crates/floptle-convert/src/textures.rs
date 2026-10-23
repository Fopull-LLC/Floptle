//! **Finding a model's pictures, and turning them into the maps glTF has.**
//!
//! Two jobs, both of which decide whether a converted model looks the way its
//! artist left it.
//!
//! **Finding.** A model names its textures by whatever path the exporter had:
//! `C:\Users\someone\Desktop\pack\Textures\crate_N.tga`, `../maps/crate_n.png`,
//! or just `crate_n.png`. The pack that arrives is rarely laid out the same way.
//! [`Finder`] tries the path as written, relative to the model, then the file
//! name in the model's folder and the texture folders packs actually use, case
//! blind, and then the same name with the other common extensions — a pack
//! re-saved from TGA to PNG keeps its material pointing at the `.tga`.
//!
//! **Turning.** glTF has five slots and fixed packing: roughness in green and
//! metallic in blue of one picture, occlusion in red. Sources store roughness
//! and metallic as two greyscale pictures, glossiness where roughness should
//! be, a height map where a normal map should be, and opacity as a picture of
//! its own. Each of those is converted here, once, so every reader gets the
//! same answer.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use floptle_render::TextureData;

/// Folders beside a model that hold its pictures, by the names packs use.
/// Matched without regard to case.
const TEXTURE_DIRS: &[&str] = &[
    "textures", "texture", "tex", "maps", "map", "images", "image", "materials", "material", "sourceimages",
];

/// Picture extensions a texture may have been re-saved as.
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "tga", "tif", "tiff", "bmp", "webp", "dds", "gif"];

/// Looks for the pictures one model names. One per model, so a folder is
/// listed once however many textures are looked up in it.
pub struct Finder {
    model_dir: PathBuf,
    /// Folders to search by file name, nearest first.
    dirs: Vec<PathBuf>,
    /// Folder → its entries, lower-cased name to the real path.
    listings: HashMap<PathBuf, HashMap<String, PathBuf>>,
}

impl Finder {
    pub fn new(model: &Path) -> Self {
        let model_dir = model.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        let stem = model.file_stem().map(|s| s.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let mut f = Self { model_dir: model_dir.clone(), dirs: Vec::new(), listings: HashMap::new() };
        // The model's own folder, its texture folders, the `.fbm` folder an
        // FBX's embedded media is unpacked into, then the same one level up —
        // a pack's `Models/` and `Textures/` are usually siblings.
        let mut roots = vec![model_dir.clone()];
        if let Some(up) = model_dir.parent() {
            roots.push(up.to_path_buf());
        }
        for (depth, root) in roots.iter().enumerate() {
            f.dirs.push(root.clone());
            let entries: Vec<(String, PathBuf)> =
                f.listing(root).iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            for (lower, path) in entries {
                let wanted = TEXTURE_DIRS.contains(&lower.as_str())
                    || (depth == 0 && lower == format!("{stem}.fbm"));
                if wanted && path.is_dir() {
                    f.dirs.push(path.clone());
                    // One more level: `Textures/Crate/crate_d.png` is common.
                    let inner: Vec<PathBuf> =
                        f.listing(&path).values().filter(|p| p.is_dir()).cloned().collect();
                    f.dirs.extend(inner);
                }
            }
        }
        f
    }

    fn listing(&mut self, dir: &Path) -> &HashMap<String, PathBuf> {
        self.listings.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut m = HashMap::new();
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    m.insert(e.file_name().to_string_lossy().to_ascii_lowercase(), e.path());
                }
            }
            m
        })
    }

    /// The file a texture reference names, or `None`. Every candidate is
    /// tried in turn: the reference as written (relative to the model, then
    /// absolute), then its file name in each search folder, then the same
    /// stem with each other picture extension.
    pub fn find(&mut self, names: &[&str]) -> Option<PathBuf> {
        let mut base_names: Vec<String> = Vec::new();
        for raw in names {
            if raw.is_empty() {
                continue;
            }
            // Exporters write Windows separators and `file://` prefixes; a
            // converter on Linux that keeps them finds nothing and blames the
            // file.
            let cleaned = raw.trim_start_matches("file://").replace('\\', "/");
            let rel = self.model_dir.join(&cleaned);
            if rel.is_file() {
                return Some(rel);
            }
            let abs = Path::new(&cleaned);
            if abs.is_absolute() && abs.is_file() {
                return Some(abs.to_path_buf());
            }
            if let Some(n) = cleaned.rsplit('/').next()
                && !n.is_empty()
                && !base_names.iter().any(|b| b == n)
            {
                base_names.push(n.to_string());
            }
        }
        for name in &base_names {
            let lower = name.to_ascii_lowercase();
            for i in 0..self.dirs.len() {
                let dir = self.dirs[i].clone();
                if let Some(p) = self.listing(&dir).get(&lower)
                    && p.is_file()
                {
                    return Some(p.clone());
                }
            }
        }
        for name in &base_names {
            let lower = name.to_ascii_lowercase();
            let stem = lower.rsplit_once('.').map(|(s, _)| s).unwrap_or(&lower).to_string();
            for i in 0..self.dirs.len() {
                let dir = self.dirs[i].clone();
                let listing = self.listing(&dir);
                for ext in IMAGE_EXTS {
                    if let Some(p) = listing.get(&format!("{stem}.{ext}"))
                        && p.is_file()
                    {
                        return Some(p.clone());
                    }
                }
            }
        }
        None
    }
}

/// The last path component of a texture reference, for messages.
pub fn short_name(p: &str) -> String {
    p.replace('\\', "/").rsplit('/').next().unwrap_or(p).to_string()
}

/// Decode a picture. The format is read from the bytes where they say, and
/// from the file's extension where they do not: TGA carries no signature, and
/// it is the most common texture format in FBX asset packs.
pub fn decode(bytes: &[u8], name_hint: &str) -> Option<TextureData> {
    let img = match image::load_from_memory(bytes) {
        Ok(i) => i,
        Err(_) => {
            let format = image::ImageFormat::from_path(name_hint).ok()?;
            image::load_from_memory_with_format(bytes, format).ok()?
        }
    };
    let img = img.to_rgba8();
    let (width, height) = img.dimensions();
    Some(TextureData { pixels: img.into_raw(), width, height })
}

/// Sample `t` at `(u, v)` in `0..1`, bilinearly, one channel.
fn sample(t: &TextureData, u: f32, v: f32, ch: usize) -> f32 {
    let (w, h) = (t.width.max(1) as usize, t.height.max(1) as usize);
    let x = (u * w as f32 - 0.5).clamp(0.0, (w - 1) as f32);
    let y = (v * h as f32 - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (x.floor() as usize, y.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (x - x0 as f32, y - y0 as f32);
    let px = |x: usize, y: usize| t.pixels.get((y * w + x) * 4 + ch).copied().unwrap_or(255) as f32;
    let top = px(x0, y0) * (1.0 - fx) + px(x1, y0) * fx;
    let bot = px(x0, y1) * (1.0 - fx) + px(x1, y1) * fx;
    top * (1.0 - fy) + bot * fy
}

/// How a greyscale map's value is read out of a picture: red for a true
/// greyscale picture, luminance for one that arrived in colour.
fn grey(t: &TextureData, u: f32, v: f32) -> f32 {
    let (r, g, b) = (sample(t, u, v, 0), sample(t, u, v, 1), sample(t, u, v, 2));
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// One greyscale source for a packed channel.
pub struct Channel<'a> {
    pub texture: &'a TextureData,
    /// The picture is glossiness (or a shininess exponent): roughness is one
    /// minus it.
    pub invert: bool,
}

/// **glTF's metallic-roughness picture from separate maps**: occlusion in
/// red, roughness in green, metallic in blue. A channel with no map is
/// written white, so the material's factor alone sets it, which is what the
/// caller puts there. The result is as large as the largest input, and each
/// input is resampled to it.
pub fn pack_orm(occlusion: Option<Channel>, roughness: Option<Channel>, metallic: Option<Channel>) -> TextureData {
    let inputs = [&occlusion, &roughness, &metallic];
    let w = inputs.iter().filter_map(|c| c.as_ref().map(|c| c.texture.width)).max().unwrap_or(1).max(1);
    let h = inputs.iter().filter_map(|c| c.as_ref().map(|c| c.texture.height)).max().unwrap_or(1).max(1);
    let mut pixels = vec![255u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let i = ((y * w + x) * 4) as usize;
            for (k, c) in inputs.iter().enumerate() {
                if let Some(c) = c {
                    let g = grey(c.texture, u, v);
                    let g = if c.invert { 255.0 - g } else { g };
                    pixels[i + k] = g.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
    }
    TextureData { pixels, width: w, height: h }
}

/// Is this picture already a tangent-space normal map, rather than a height
/// map in the same slot? Sources disagree about which one a "bump" slot holds,
/// and both are common. A normal map is mostly the pale blue of an
/// unperturbed normal — blue high, red and green near half — and is never
/// grey; a height map is grey.
pub fn looks_like_normal_map(t: &TextureData) -> bool {
    let px = t.pixels.as_chunks::<4>().0;
    if px.is_empty() {
        return false;
    }
    let step = (px.len() / 4096).max(1);
    let (mut r, mut g, mut b, mut n, mut grey) = (0f64, 0f64, 0f64, 0f64, 0usize);
    for p in px.iter().step_by(step) {
        r += p[0] as f64;
        g += p[1] as f64;
        b += p[2] as f64;
        n += 1.0;
        if p[0].abs_diff(p[1]) < 6 && p[1].abs_diff(p[2]) < 6 {
            grey += 1;
        }
    }
    let (r, g, b) = (r / n / 255.0, g / n / 255.0, b / n / 255.0);
    let mostly_grey = grey as f64 / n > 0.9;
    !mostly_grey && b > 0.65 && (0.3..0.7).contains(&r) && (0.3..0.7).contains(&g)
}

/// A height (bump) map as a tangent-space normal map, by its slope.
/// `strength` scales the slope; one is a gentle relief at the picture's own
/// resolution.
pub fn height_to_normal(t: &TextureData, strength: f32) -> TextureData {
    let (w, h) = (t.width.max(1) as i64, t.height.max(1) as i64);
    let at = |x: i64, y: i64| -> f32 {
        // Clamped at the border. Wrapping is right for a tiling texture and
        // draws a ridge round the edge of every one that does not tile, and
        // nothing in the file says which this is.
        let (x, y) = (x.clamp(0, w - 1), y.clamp(0, h - 1));
        let i = ((y * w + x) * 4) as usize;
        let p = &t.pixels[i..i + 3];
        (0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32) / 255.0
    };
    let mut pixels = vec![255u8; (w * h * 4) as usize];
    let s = strength.max(0.0) * 2.0;
    for y in 0..h {
        for x in 0..w {
            let dx = (at(x + 1, y) - at(x - 1, y)) * s;
            let dy = (at(x, y + 1) - at(x, y - 1)) * s;
            // Picture rows run down and glTF's tangent-space V runs up, so a
            // slope down the picture is a slope against +Y.
            let n = [-dx, dy, 1.0f32];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            let i = ((y * w + x) * 4) as usize;
            for k in 0..3 {
                pixels[i + k] = ((n[k] / len * 0.5 + 0.5) * 255.0).round() as u8;
            }
        }
    }
    TextureData { pixels, width: w as u32, height: h as u32 }
}

/// `base` with `opacity`'s grey written into its alpha (multiplied with any
/// alpha it already had). With no base picture a white one is made, so a
/// material whose only map is opacity still cuts out.
pub fn with_opacity(base: Option<&TextureData>, opacity: &TextureData) -> TextureData {
    let mut out = match base {
        Some(b) => b.clone(),
        None => TextureData {
            pixels: vec![255; (opacity.width * opacity.height * 4) as usize],
            width: opacity.width,
            height: opacity.height,
        },
    };
    let (w, h) = (out.width.max(1), out.height.max(1));
    for y in 0..h {
        for x in 0..w {
            let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
            let i = ((y * w + x) * 4 + 3) as usize;
            let a = out.pixels[i] as f32 * grey(opacity, u, v) / 255.0;
            out.pixels[i] = a.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// What a picture's alpha says about how it should be drawn: `None` when it is
/// fully opaque, `Some(true)` when it is a cut-out (nearly every pixel either
/// solid or empty — foliage, fences, decals), `Some(false)` when it is
/// genuinely see-through.
pub fn alpha_kind(t: &TextureData) -> Option<bool> {
    let px = t.pixels.as_chunks::<4>().0;
    let (mut clear, mut binary, mut n) = (0usize, 0usize, 0usize);
    for p in px {
        n += 1;
        if p[3] < 250 {
            clear += 1;
        }
        if p[3] < 13 || p[3] > 242 {
            binary += 1;
        }
    }
    if n == 0 || clear * 1000 < n {
        return None;
    }
    Some(binary as f64 / n as f64 > 0.9)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(rgba: [u8; 4], w: u32, h: u32) -> TextureData {
        TextureData { pixels: rgba.repeat((w * h) as usize), width: w, height: h }
    }

    /// **Each map lands in the channel glTF reads it from**, and glossiness
    /// comes out as roughness. Three different greys, so a swapped pair of
    /// channels cannot pass.
    #[test]
    fn orm_packing_puts_each_map_in_its_channel() {
        let ao = flat([40, 40, 40, 255], 2, 2);
        let gloss = flat([200, 200, 200, 255], 4, 4);
        let metal = flat([120, 120, 120, 255], 1, 1);
        let orm = pack_orm(
            Some(Channel { texture: &ao, invert: false }),
            Some(Channel { texture: &gloss, invert: true }),
            Some(Channel { texture: &metal, invert: false }),
        );
        assert_eq!((orm.width, orm.height), (4, 4), "the largest input's size");
        assert_eq!(orm.pixels[..4], [40, 55, 120, 255], "R occlusion, G roughness = 255 - gloss, B metallic");
        // A missing channel is white, so its factor alone sets it.
        let only_metal = pack_orm(None, None, Some(Channel { texture: &metal, invert: false }));
        assert_eq!(only_metal.pixels[..4], [255, 255, 120, 255]);
    }

    #[test]
    fn a_normal_map_is_told_from_a_height_map() {
        assert!(looks_like_normal_map(&flat([128, 128, 255, 255], 8, 8)));
        assert!(!looks_like_normal_map(&flat([128, 128, 128, 255], 8, 8)), "grey is height");
        let n = height_to_normal(&flat([90, 90, 90, 255], 8, 8), 1.0);
        assert_eq!(n.pixels[..3], [128, 128, 255], "a flat height map is an unperturbed normal");
        assert!(looks_like_normal_map(&n));
    }

    #[test]
    fn alpha_is_read_as_cutout_or_blend() {
        assert_eq!(alpha_kind(&flat([9, 9, 9, 255], 4, 4)), None);
        let mut cut = flat([9, 9, 9, 255], 4, 4);
        cut.pixels[3] = 0;
        assert_eq!(alpha_kind(&cut), Some(true));
        assert_eq!(alpha_kind(&flat([9, 9, 9, 128], 4, 4)), Some(false));
    }
}
