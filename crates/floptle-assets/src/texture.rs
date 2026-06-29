//! Loose-image textures for materials — decode a PNG/JPEG on disk to the RGBA8
//! [`TextureData`] the renderer uploads, and save a decoded texture back out (used
//! to extract a model's embedded textures into the project so they can be reused).

use std::path::Path;

use floptle_render::TextureData;

/// Decode an image file (PNG/JPEG) to tightly-packed RGBA8. `None` on any error.
pub fn load_texture(path: &Path) -> Option<TextureData> {
    let img = image::open(path).ok()?.to_rgba8();
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
