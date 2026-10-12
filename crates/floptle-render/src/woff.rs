//! **WOFF fonts.** A WOFF file is an ordinary TrueType or OpenType font with
//! each table zlib-compressed and a short header in front. Unpacking it gives
//! back the font it was made from, byte for byte, which the rasterizer reads
//! like any `.ttf`.
//!
//! WOFF2 is a different format (Brotli, and the glyph tables rewritten) and is
//! not read here.

use std::io::Read;

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(at)?, *b.get(at + 1)?, *b.get(at + 2)?, *b.get(at + 3)?]))
}

/// Is this a WOFF (1) file?
pub fn is_woff(bytes: &[u8]) -> bool {
    bytes.starts_with(b"wOFF")
}

/// Is this a WOFF2 file, which is not read?
pub fn is_woff2(bytes: &[u8]) -> bool {
    bytes.starts_with(b"wOF2")
}

/// The TrueType/OpenType font inside a WOFF file. `None` when it is damaged.
pub fn unwrap_woff(b: &[u8]) -> Option<Vec<u8>> {
    if !is_woff(b) {
        return None;
    }
    let flavor = be32(b, 4)?;
    let n = be16(b, 12)? as usize;
    if n == 0 || n > 4096 {
        return None;
    }
    // Each table: tag, offset, compressed length, original length, checksum.
    let mut tables = Vec::with_capacity(n);
    for i in 0..n {
        let at = 44 + i * 20;
        tables.push((
            be32(b, at)?,
            be32(b, at + 4)? as usize,
            be32(b, at + 8)? as usize,
            be32(b, at + 12)? as usize,
            be32(b, at + 16)?,
        ));
    }
    let mut out = Vec::new();
    out.extend(flavor.to_be_bytes());
    out.extend((n as u16).to_be_bytes());
    let pow = 1u16 << (15 - (n as u16).leading_zeros().min(15));
    let search = pow * 16;
    let selector = pow.trailing_zeros() as u16;
    out.extend(search.to_be_bytes());
    out.extend(selector.to_be_bytes());
    out.extend((n as u16 * 16 - search).to_be_bytes());
    let mut data_at = 12 + n * 16;
    let mut data = Vec::new();
    for &(tag, off, comp, orig, sum) in &tables {
        let raw = b.get(off..off.checked_add(comp)?)?;
        let table = if comp < orig {
            let mut t = Vec::with_capacity(orig);
            flate2::read::ZlibDecoder::new(raw).take(orig as u64).read_to_end(&mut t).ok()?;
            if t.len() != orig {
                return None;
            }
            t
        } else {
            raw.to_vec()
        };
        out.extend(tag.to_be_bytes());
        out.extend(sum.to_be_bytes());
        out.extend((data_at as u32).to_be_bytes());
        out.extend((orig as u32).to_be_bytes());
        let pad = (4 - table.len() % 4) % 4;
        data_at += table.len() + pad;
        data.extend(table);
        data.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(data);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Wrap a font as WOFF, the way a converter does: every table zlibbed
    /// unless that would not make it smaller.
    pub(crate) fn wrap(sfnt: &[u8]) -> Vec<u8> {
        let n = be16(sfnt, 4).unwrap() as usize;
        let mut dir = Vec::new();
        let mut body = Vec::new();
        let base = 44 + n * 20;
        for i in 0..n {
            let at = 12 + i * 16;
            let (tag, sum, off, len) =
                (be32(sfnt, at).unwrap(), be32(sfnt, at + 4).unwrap(), be32(sfnt, at + 8).unwrap() as usize, be32(sfnt, at + 12).unwrap() as usize);
            let raw = &sfnt[off..off + len];
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            z.write_all(raw).unwrap();
            let zipped = z.finish().unwrap();
            let stored = if zipped.len() < raw.len() { zipped } else { raw.to_vec() };
            dir.extend(tag.to_be_bytes());
            dir.extend(((base + body.len()) as u32).to_be_bytes());
            dir.extend((stored.len() as u32).to_be_bytes());
            dir.extend((len as u32).to_be_bytes());
            dir.extend(sum.to_be_bytes());
            body.extend(&stored);
            while body.len() % 4 != 0 {
                body.push(0);
            }
        }
        let mut out = b"wOFF".to_vec();
        out.extend(&sfnt[0..4]);
        out.extend(((base + body.len()) as u32).to_be_bytes());
        out.extend((n as u16).to_be_bytes());
        out.extend([0u8; 2]);
        out.extend((sfnt.len() as u32).to_be_bytes());
        out.resize(44, 0);
        out.extend(dir);
        out.extend(body);
        out
    }

    /// The bundled UI font, wrapped as WOFF and unwrapped again, parses and
    /// draws the same letter as the original.
    #[test]
    fn a_woff_font_unwraps_to_one_that_draws_the_same() {
        let ttf: &[u8] = include_bytes!("../fonts/Roboto-Regular.ttf");
        let woff = wrap(ttf);
        assert!(is_woff(&woff) && woff.len() < ttf.len(), "the test font must actually be compressed");
        let back = unwrap_woff(&woff).expect("unwraps");
        let a = fontdue::Font::from_bytes(ttf, Default::default()).unwrap();
        let b = fontdue::Font::from_bytes(back.as_slice(), Default::default()).unwrap();
        assert_eq!(a.rasterize('g', 24.0), b.rasterize('g', 24.0));
        assert!(unwrap_woff(&woff[..60]).is_none(), "a cut-off file is refused");
    }
}
