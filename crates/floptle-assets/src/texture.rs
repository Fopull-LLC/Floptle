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
    let bytes = floptle_vfs::read(path).ok()?;
    image::ImageReader::new(std::io::Cursor::new(bytes))
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

/// The widest or tallest picture [`decode_untrusted`] accepts, in pixels.
pub const UNTRUSTED_MAX_SIDE: u32 = 4096;

/// The most memory a decoder may ask for while decoding an untrusted picture:
/// the RGBA of the largest accepted one, twice over for the decoder's own
/// buffers.
const UNTRUSTED_MAX_ALLOC: u64 = UNTRUSTED_MAX_SIDE as u64 * UNTRUSTED_MAX_SIDE as u64 * 4 * 2;

/// Decode bytes that came from somewhere other than the project (a download,
/// a player's upload) to RGBA8, or say why not.
///
/// Pixels only. The format is read from the bytes' own signature, never from a
/// name or a server's content type, and only PNG, JPEG and WebP are accepted:
/// three decoders, each of which yields pixels and nothing else. A picture
/// wider or taller than [`UNTRUSTED_MAX_SIDE`] is refused from its header,
/// before its pixels are decoded, and the decoder's allocations are capped, so
/// a small file that claims to be enormous costs nothing.
pub fn decode_untrusted(bytes: &[u8], max_side: u32) -> Result<TextureData, String> {
    use image::ImageFormat;
    let format = image::guess_format(bytes).map_err(|_| "not a PNG, JPEG or WebP image".to_string())?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP) {
        return Err(format!("a {} image; only PNG, JPEG and WebP are accepted", format.extensions_str()[0]));
    }
    let side = max_side.min(UNTRUSTED_MAX_SIDE);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(side);
    limits.max_image_height = Some(side);
    limits.max_alloc = Some(UNTRUSTED_MAX_ALLOC);
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(bytes), format);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| match e {
        image::ImageError::Limits(_) => format!("larger than {side}×{side} pixels"),
        e => format!("could not be decoded: {e}"),
    })?;
    let img = img.to_rgba8();
    let (width, height) = img.dimensions();
    Ok(TextureData { pixels: img.into_raw(), width, height })
}

/// Write an RGBA8 [`TextureData`] to `path` as a PNG.
pub fn save_texture_png(tex: &TextureData, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        floptle_vfs::create_dir_all(parent)?;
    }
    let bytes = encode_png(tex).ok_or_else(|| std::io::Error::other("PNG encode failed"))?;
    floptle_vfs::write(path, bytes)
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

    fn encoded(w: u32, h: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img).to_rgb8().write_to(&mut out, format).unwrap();
        out.into_inner()
    }

    /// A downloaded picture is pixels or nothing: the three accepted formats
    /// decode, anything else is refused by its signature, and so is a picture
    /// past the size limit.
    #[test]
    fn an_untrusted_picture_is_png_jpeg_or_webp_and_no_bigger_than_the_limit() {
        use image::ImageFormat::*;
        for f in [Png, Jpeg, WebP] {
            let t = decode_untrusted(&encoded(3, 2, f), 64).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert_eq!((t.width, t.height, t.pixels.len()), (3, 2, 24), "{f:?}");
        }
        for f in [Bmp, Gif, Tiff] {
            let why = decode_untrusted(&encoded(3, 2, f), 64).expect_err("accepted a format outside the three");
            assert!(why.contains("only PNG, JPEG and WebP"), "{f:?}: {why}");
        }
        assert!(decode_untrusted(b"<svg xmlns='http://www.w3.org/2000/svg'/>", 64).is_err());
        assert!(decode_untrusted(b"", 64).is_err());

        let why = decode_untrusted(&encoded(65, 8, Png), 64).expect_err("accepted a picture past the limit");
        assert!(why.contains("64×64"), "{why}");
        assert!(decode_untrusted(&encoded(64, 64, Png), 64).is_ok(), "refused a picture exactly at the limit");
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut c = !0u32;
        for b in bytes {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }

    /// A PNG header can claim any size in a few dozen bytes. The claim is
    /// refused from the header, by the size limit, before a decoder sizes a
    /// buffer to it.
    #[test]
    fn a_picture_that_claims_to_be_enormous_is_refused_from_its_header() {
        let mut png = encoded(1, 1, image::ImageFormat::Png);
        // IHDR's width and height are bytes 16..24, and its CRC (over the
        // chunk type and data, 12..29) is 29..33. A valid CRC, so the only
        // thing wrong with this file is the size it claims.
        png[16..20].copy_from_slice(&5_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&5_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        let why = decode_untrusted(&png, UNTRUSTED_MAX_SIDE).expect_err("decoded a 5000² claim");
        assert!(why.contains("larger than 4096×4096"), "refused for another reason: {why}");
    }
}
