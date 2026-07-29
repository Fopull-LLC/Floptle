//! Sprite-sheet packing — and specifically the ONE layout the engine can read.
//!
//! `TexSetting.sheet_cols`/`sheet_rows` slice a texture into a **uniform grid**
//! that UI images address by `cell` (animatable in the dopesheet) and VFX
//! billboards flipbook through. So this packer emits uniform cells, row-major,
//! left-to-right then top-to-bottom — no atlas rectangles, no trimming, no
//! padding. Anything cleverer would produce sheets the runtime cannot address.

/// A packed sheet plus the grid the engine must be told about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sheet {
    pub pixels: Vec<u8>,
    pub w: u32,
    pub h: u32,
    pub cols: u32,
    pub rows: u32,
}

/// A near-square column count for `n` frames — the default that keeps a sheet
/// inside GPU texture limits without anyone thinking about it.
pub fn suggest_cols(n: usize) -> u32 {
    (n as f32).sqrt().ceil().max(1.0) as u32
}

/// Pack `frames` (each `fw`×`fh` straight RGBA8) into one sheet.
///
/// `cols` of `None` picks [`suggest_cols`]. Cells past the last frame are left
/// transparent, which is exactly how the runtime's grid indexing expects to find
/// a partly-filled last row.
pub fn pack(frames: &[Vec<u8>], fw: u32, fh: u32, cols: Option<u32>) -> Sheet {
    let n = frames.len().max(1);
    let cols = cols.unwrap_or_else(|| suggest_cols(n)).max(1);
    let rows = (n as u32).div_ceil(cols).max(1);
    let (w, h) = (fw * cols, fh * rows);
    let mut pixels = vec![0u8; w as usize * h as usize * 4];
    for (i, f) in frames.iter().enumerate() {
        let cx = (i as u32 % cols) * fw;
        let cy = (i as u32 / cols) * fh;
        for y in 0..fh {
            let src = (y as usize * fw as usize) * 4;
            let dst = ((cy + y) as usize * w as usize + cx as usize) * 4;
            let len = fw as usize * 4;
            if src + len <= f.len() && dst + len <= pixels.len() {
                pixels[dst..dst + len].copy_from_slice(&f[src..src + len]);
            }
        }
    }
    Sheet { pixels, w, h, cols, rows }
}

/// The inverse: slice a sheet back into frames (importing someone else's sheet).
pub fn unpack(pixels: &[u8], w: u32, h: u32, cols: u32, rows: u32) -> Vec<Vec<u8>> {
    let (cols, rows) = (cols.max(1), rows.max(1));
    let (fw, fh) = (w / cols, h / rows);
    if fw == 0 || fh == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let mut f = vec![0u8; fw as usize * fh as usize * 4];
            for y in 0..fh {
                let src = ((r * fh + y) as usize * w as usize + (c * fw) as usize) * 4;
                let dst = y as usize * fw as usize * 4;
                let len = fw as usize * 4;
                if src + len <= pixels.len() {
                    f[dst..dst + len].copy_from_slice(&pixels[src..src + len]);
                }
            }
            out.push(f);
        }
    }
    out
}

/// Encode frames as an animated GIF at `fps`. GIF is 8-bit with 1-bit alpha, so
/// this is a preview/share format, not the pipeline one — sheets are.
pub fn encode_gif(frames: &[Vec<u8>], w: u32, h: u32, fps: f32) -> Option<Vec<u8>> {
    use image::codecs::gif::{GifEncoder, Repeat};
    use image::{Delay, Frame, RgbaImage};
    if frames.is_empty() || w == 0 || h == 0 {
        return None;
    }
    let ms = (1000.0 / fps.max(1.0)).round().max(20.0) as u32;
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(std::io::Cursor::new(&mut out));
        enc.set_repeat(Repeat::Infinite).ok()?;
        for f in frames {
            let img = RgbaImage::from_raw(w, h, f.clone())?;
            let frame = Frame::from_parts(img, 0, 0, Delay::from_numer_denom_ms(ms, 1));
            enc.encode_frame(frame).ok()?;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(fw: u32, fh: u32, c: u8) -> Vec<u8> {
        (0..fw * fh).flat_map(|_| [c, c, c, 255]).collect()
    }

    #[test]
    fn packing_is_row_major_and_uniform() {
        let frames: Vec<Vec<u8>> = (0..4).map(|i| frame(4, 4, i as u8 * 10 + 1)).collect();
        let s = pack(&frames, 4, 4, Some(2));
        assert_eq!((s.cols, s.rows), (2, 2));
        assert_eq!((s.w, s.h), (8, 8));
        // frame 0 top-left, frame 1 top-right, frame 2 bottom-left.
        assert_eq!(s.pixels[0], 1);
        assert_eq!(s.pixels[4 * 4], 11, "frame 1 starts at x=4 of row 0");
        assert_eq!(s.pixels[(4 * 8) * 4], 21, "frame 2 starts at row 4");
    }

    #[test]
    fn a_partial_last_row_stays_transparent() {
        let frames: Vec<Vec<u8>> = (0..3).map(|_| frame(2, 2, 255)).collect();
        let s = pack(&frames, 2, 2, Some(2));
        assert_eq!((s.cols, s.rows), (2, 2));
        // Bottom-right cell has no frame.
        let o = ((2 * 4 + 2) * 4) as usize + 3;
        assert_eq!(s.pixels[o], 0);
    }

    #[test]
    fn pack_unpack_round_trips() {
        let frames: Vec<Vec<u8>> = (0..6).map(|i| frame(3, 5, i as u8 + 1)).collect();
        let s = pack(&frames, 3, 5, Some(3));
        let back = unpack(&s.pixels, s.w, s.h, s.cols, s.rows);
        assert_eq!(back.len(), 6);
        for (a, b) in frames.iter().zip(back.iter()) {
            assert_eq!(a, b);
        }
    }

    #[test]
    fn suggested_columns_stay_near_square() {
        assert_eq!(suggest_cols(1), 1);
        assert_eq!(suggest_cols(4), 2);
        assert_eq!(suggest_cols(8), 3);
        assert_eq!(suggest_cols(16), 4);
    }

    #[test]
    fn gif_encodes_something_gif_shaped() {
        let frames: Vec<Vec<u8>> = (0..2).map(|i| frame(8, 8, i as u8 * 200)).collect();
        let bytes = encode_gif(&frames, 8, 8, 12.0).expect("gif");
        assert_eq!(&bytes[0..3], b"GIF");
        assert!(bytes.len() > 20);
    }
}
