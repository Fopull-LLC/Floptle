//! Loose-image textures for materials — decode an image on disk to the RGBA8
//! [`TextureData`] the renderer uploads, and save a decoded texture back out (used
//! to extract a model's embedded textures into the project so they can be reused).
//!
//! Format is detected from the file's **content** (magic bytes), not its
//! extension — VFX/game texture packs routinely ship a WebP or TGA under a `.png`
//! name, and decoding by extension would hand those bytes to the wrong decoder
//! and fail. See [`decode`].

use std::path::Path;

use floptle_render::TextureData;

/// Decode an image file to a `DynamicImage`, guessing the format from its content
/// so a mislabeled file (e.g. a WebP saved as `.png`) still loads. `None` on any
/// I/O or decode error.
fn decode(path: &Path) -> Option<image::DynamicImage> {
    image::ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()
}

/// Decode an image file to tightly-packed RGBA8. `None` on any error.
pub fn load_texture(path: &Path) -> Option<TextureData> {
    let img = decode(path)?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(TextureData { pixels: img.into_raw(), width, height })
}

/// Decode + resize an image to exactly `w`×`h` RGBA8 (for the terrain palette,
/// whose layers must all share one size).
pub fn load_texture_sized(path: &Path, w: u32, h: u32) -> Option<TextureData> {
    load_texture_sized_filtered(path, w, h, false)
}

/// Like [`load_texture_sized`], but `nearest` picks point resampling.
///
/// This matters more than the GPU sampler does. Resizing a 32² pixel-art tile up to
/// the terrain palette's 256² with a bilinear (`Triangle`) filter smears it into mush
/// **at load**, and no sampler setting downstream can recover it — which is why a
/// texture marked Pixelated still looked blurry on terrain while the identical image
/// looked crisp on a mesh (meshes upload at native size and only choose a sampler).
/// Callers that honour a texture's Pixelated setting must pass `nearest: true`.
pub fn load_texture_sized_filtered(path: &Path, w: u32, h: u32, nearest: bool) -> Option<TextureData> {
    let img = decode(path)?;
    let filter = if nearest {
        image::imageops::FilterType::Nearest
    } else {
        image::imageops::FilterType::Triangle
    };
    let out = img.resize_exact(w, h, filter).to_rgba8();
    Some(TextureData { pixels: out.into_raw(), width: w, height: h })
}

/// Encode an RGBA8 [`TextureData`] to PNG bytes in memory. Used to pack paint
/// textures into a scene's paint container (they compress well — paint is mostly flat).
pub fn encode_png(tex: &TextureData) -> Option<Vec<u8>> {
    let img = image::RgbaImage::from_raw(tex.width, tex.height, tex.pixels.clone())?;
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).ok()?;
    Some(buf.into_inner())
}

/// Decode PNG (or any guessed format) bytes to tightly-packed RGBA8. The inverse of
/// [`encode_png`].
pub fn decode_png(bytes: &[u8]) -> Option<TextureData> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (width, height) = img.dimensions();
    Some(TextureData { pixels: img.into_raw(), width, height })
}

/// Write an RGBA8 [`TextureData`] to `path` as a PNG.
pub fn save_texture_png(tex: &TextureData, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    image::save_buffer(path, &tex.pixels, tex.width, tex.height, image::ColorType::Rgba8)
        .map_err(|e| std::io::Error::other(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a texture pack that ships a real PNG under a `.jpg` name (or a
    /// WebP under `.png`, the case that hid VFX particle textures) must still load —
    /// the decoder guesses format from content, not the extension.
    #[test]
    fn decodes_by_content_not_extension() {
        let dir = std::env::temp_dir().join(format!("floptle-tex-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A 2×2 RGBA image saved as real PNG bytes...
        let src = dir.join("real.png");
        image::save_buffer(&src, &[255u8; 16], 2, 2, image::ColorType::Rgba8).unwrap();
        // ...then given a lying `.jpg` name.
        let lying = dir.join("actually_png.jpg");
        std::fs::rename(&src, &lying).unwrap();

        let t = load_texture(&lying).expect("must decode a PNG-in-.jpg-clothing by content");
        assert_eq!((t.width, t.height), (2, 2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
