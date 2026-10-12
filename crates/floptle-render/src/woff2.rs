//! **WOFF2 fonts.** The web's usual font format: every table of a TrueType
//! or OpenType font in one Brotli stream, with the outline tables rewritten
//! into a denser form first. Unpacking rebuilds an ordinary font the
//! rasterizer reads like any `.ttf`.
//!
//! The rewritten tables are `glyf` and `loca` (always, in practice) and
//! `hmtx` (when the encoder chose to). Rebuilding them gives back a font
//! that draws exactly as the original, though not byte for byte: point
//! coordinates are written out plainly rather than in the original's packing.
//! A font collection (`.ttc` inside WOFF2) is refused.

use std::io::Read;

/// The most a font may unpack to. A real one is a few megabytes; a file that
/// claims hundreds is a decompression bomb, not a font.
const MAX_SFNT: usize = 64 << 20;

/// The 63 tags a table's flags byte can name without spelling the tag out.
const KNOWN_TAGS: [&[u8; 4]; 63] = [
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post", b"cvt ", b"fpgm", b"glyf",
    b"loca", b"prep", b"CFF ", b"VORG", b"EBDT", b"EBLC", b"gasp", b"hdmx", b"kern", b"LTSH", b"PCLT",
    b"VDMX", b"vhea", b"vmtx", b"BASE", b"GDEF", b"GPOS", b"GSUB", b"EBSC", b"JSTF", b"MATH", b"CBDT",
    b"CBLC", b"COLR", b"CPAL", b"SVG ", b"sbix", b"acnt", b"avar", b"bdat", b"bloc", b"bsln", b"cvar",
    b"fdsc", b"feat", b"fmtx", b"fvar", b"gvar", b"hsty", b"just", b"lcar", b"mort", b"morx", b"opbd",
    b"prop", b"trak", b"Zapf", b"Silf", b"Glat", b"Gloc", b"Feat", b"Sill",
];

/// Is this a WOFF2 file?
pub fn is_woff2(bytes: &[u8]) -> bool {
    bytes.starts_with(b"wOF2")
}

/// A cursor over big-endian bytes that answers `None` past the end, so a
/// cut-off or lying file is refused rather than read out of bounds.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, at: 0 }
    }
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let s = self.bytes(2)?;
        Some(u16::from_be_bytes([s[0], s[1]]))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(self.u16()? as i16)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.bytes(4)?;
        Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    /// A variable-length unsigned int: seven bits a byte, high bit set on
    /// every byte but the last, at most five bytes, no leading zero byte.
    fn base128(&mut self) -> Option<u32> {
        let mut v: u32 = 0;
        for i in 0..5 {
            let b = self.u8()?;
            if i == 0 && b == 0x80 {
                return None;
            }
            if v & 0xFE00_0000 != 0 {
                return None;
            }
            v = (v << 7) | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
    /// A 16-bit value packed into one to three bytes.
    fn u255(&mut self) -> Option<u16> {
        Some(match self.u8()? {
            253 => self.u16()?,
            255 => self.u8()? as u16 + 253,
            254 => self.u8()? as u16 + 506,
            c => c as u16,
        })
    }
}

/// One table as the directory lists it.
struct Entry {
    tag: [u8; 4],
    /// Its length once rebuilt.
    orig: usize,
    /// Where its bytes start in the decompressed stream, and how many there are.
    at: usize,
    len: usize,
    transformed: bool,
}

/// The TrueType/OpenType font inside a WOFF2 file. `None` when it is damaged,
/// is a collection, or would unpack to more than any font needs.
pub fn unwrap_woff2(b: &[u8]) -> Option<Vec<u8>> {
    if !is_woff2(b) {
        return None;
    }
    let mut r = Reader::new(b);
    r.bytes(4)?;
    let flavor = r.u32()?;
    if &flavor.to_be_bytes() == b"ttcf" {
        return None;
    }
    let _length = r.u32()?;
    let n = r.u16()? as usize;
    r.u16()?;
    let total_sfnt = r.u32()? as usize;
    let compressed = r.u32()? as usize;
    r.bytes(4 + 4 * 5)?; // version, metadata and private block
    if n == 0 || n > 4096 || total_sfnt > MAX_SFNT {
        return None;
    }
    let mut tables = Vec::with_capacity(n);
    let mut stream = 0usize;
    for _ in 0..n {
        let flags = r.u8()?;
        let tag: [u8; 4] = match flags & 0x3F {
            63 => r.bytes(4)?.try_into().ok()?,
            i => *KNOWN_TAGS[i as usize],
        };
        let version = flags >> 6;
        let orig = r.base128()? as usize;
        // glyf and loca are rewritten unless their version says 3; every other
        // table is rewritten only when its version is not 0.
        let transformed = match &tag {
            b"glyf" | b"loca" => version != 3,
            _ => version != 0,
        };
        let len = if transformed { r.base128()? as usize } else { orig };
        if orig > MAX_SFNT || len > MAX_SFNT {
            return None;
        }
        tables.push(Entry { tag, orig, at: stream, len, transformed });
        stream = stream.checked_add(len)?;
    }
    if stream > MAX_SFNT {
        return None;
    }
    let packed = r.bytes(compressed)?;
    let mut data = Vec::with_capacity(stream);
    brotli_decompressor::Decompressor::new(packed, 4096)
        .take(stream as u64 + 1)
        .read_to_end(&mut data)
        .ok()?;
    if data.len() != stream {
        return None;
    }
    let raw = |e: &Entry| data.get(e.at..e.at + e.len);
    let find = |tag: &[u8; 4]| tables.iter().find(|e| &e.tag == tag);

    let mut out_tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(n);
    let mut glyf_loca: Option<Rebuilt> = None;
    if let Some(g) = find(b"glyf").filter(|g| g.transformed) {
        glyf_loca = Some(rebuild_glyf(raw(g)?)?);
    }
    for e in &tables {
        let bytes = match (&e.tag, e.transformed) {
            (_, false) => raw(e)?.to_vec(),
            (b"glyf", true) => glyf_loca.as_ref()?.0.clone(),
            (b"loca", true) => glyf_loca.as_ref()?.1.clone(),
            (b"hmtx", true) => {
                let x_min = &glyf_loca.as_ref()?.2;
                let hhea = raw(find(b"hhea")?)?;
                let metrics = u16::from_be_bytes([*hhea.get(34)?, *hhea.get(35)?]) as usize;
                rebuild_hmtx(raw(e)?, metrics, x_min)?
            }
            _ => return None,
        };
        let longer = glyf_loca.as_ref().is_some_and(|g| g.3);
        // A rebuilt loca is the length the directory promised (twice it, when
        // it went to long offsets); anything else means the glyph table and
        // its index disagree.
        if e.tag == *b"loca" && bytes.len() != e.orig * if longer { 2 } else { 1 } {
            return None;
        }
        let mut bytes = bytes;
        if e.tag == *b"head" && longer {
            bytes.get_mut(50..52)?.copy_from_slice(&1u16.to_be_bytes());
        }
        out_tables.push((e.tag, bytes));
    }
    Some(assemble(flavor, out_tables))
}

/// A rebuilt `glyf`, its `loca`, each glyph's left edge (for `hmtx`), and
/// whether `loca` had to go from short offsets to long ones.
type Rebuilt = (Vec<u8>, Vec<u8>, Vec<i16>, bool);

/// The real `glyf`, its `loca`, and each glyph's left edge (for `hmtx`), from
/// the rewritten glyph table: seven streams of glyph parts read in step.
fn rebuild_glyf(t: &[u8]) -> Option<Rebuilt> {
    let mut h = Reader::new(t);
    h.u16()?;
    let options = h.u16()?;
    let num_glyphs = h.u16()? as usize;
    let index_format = h.u16()?;
    let mut sizes = [0usize; 7];
    for s in &mut sizes {
        *s = h.u32()? as usize;
    }
    let mut cut = h.at;
    let mut streams = Vec::with_capacity(8);
    let mut lens = sizes.to_vec();
    if options & 1 != 0 {
        lens.push(num_glyphs.div_ceil(8));
    }
    for len in lens {
        streams.push(Reader::new(t.get(cut..cut.checked_add(len)?)?));
        cut += len;
    }
    let mut it = streams.into_iter();
    let (mut n_contours, mut n_points, mut flags, mut glyphs) = (it.next()?, it.next()?, it.next()?, it.next()?);
    let (mut composites, mut bbox, mut instructions) = (it.next()?, it.next()?, it.next()?);
    let overlap = it.next().map(|r| r.b);
    let bbox_bits = bbox.bytes(num_glyphs.div_ceil(32) * 4)?;
    let has_bbox = |g: usize| bbox_bits[g / 8] & (0x80 >> (g % 8)) != 0;

    let mut glyf = Vec::new();
    let mut offsets = Vec::with_capacity(num_glyphs + 1);
    let mut x_min = Vec::with_capacity(num_glyphs);
    for g in 0..num_glyphs {
        offsets.push(glyf.len());
        let contours = n_contours.i16()?;
        if contours == 0 {
            if has_bbox(g) {
                return None;
            }
            x_min.push(0);
            continue;
        }
        if contours < 0 {
            // A composite: its parts are copied as written, and its box is
            // always given.
            let start = composites.at;
            let mut more = true;
            let mut instr = false;
            while more {
                let f = composites.u16()?;
                composites.u16()?;
                let args = if f & 0x0001 != 0 { 4 } else { 2 };
                let scale = if f & 0x0008 != 0 {
                    2
                } else if f & 0x0040 != 0 {
                    4
                } else if f & 0x0080 != 0 {
                    8
                } else {
                    0
                };
                composites.bytes(args + scale)?;
                instr |= f & 0x0100 != 0;
                more = f & 0x0020 != 0;
            }
            let parts = &composites.b[start..composites.at];
            let b = bbox.bytes(8)?;
            glyf.extend((-1i16).to_be_bytes());
            glyf.extend(b);
            glyf.extend(parts);
            if instr {
                let len = glyphs.u255()? as usize;
                glyf.extend((len as u16).to_be_bytes());
                glyf.extend(instructions.bytes(len)?);
            }
            x_min.push(i16::from_be_bytes([b[0], b[1]]));
        } else {
            let mut ends = Vec::with_capacity(contours as usize);
            let mut total = 0usize;
            for _ in 0..contours {
                total += n_points.u255()? as usize;
                ends.push(total.checked_sub(1)?);
            }
            if total > 0xFFFF {
                return None;
            }
            let mut pts = Vec::with_capacity(total);
            let (mut x, mut y) = (0i32, 0i32);
            for _ in 0..total {
                let f = flags.u8()?;
                let (dx, dy) = triplet(f & 0x7F, &mut glyphs)?;
                x += dx;
                y += dy;
                pts.push((x, y, f & 0x80 == 0));
            }
            let instr_len = glyphs.u255()? as usize;
            let instr = instructions.bytes(instr_len)?;
            let b = if has_bbox(g) {
                let b = bbox.bytes(8)?;
                [0, 2, 4, 6].map(|i| i16::from_be_bytes([b[i], b[i + 1]]))
            } else {
                let xs = pts.iter().map(|p| p.0);
                let ys = pts.iter().map(|p| p.1);
                [
                    xs.clone().min().unwrap_or(0) as i16,
                    ys.clone().min().unwrap_or(0) as i16,
                    xs.max().unwrap_or(0) as i16,
                    ys.max().unwrap_or(0) as i16,
                ]
            };
            glyf.extend(contours.to_be_bytes());
            for v in b {
                glyf.extend(v.to_be_bytes());
            }
            for e in &ends {
                glyf.extend((*e as u16).to_be_bytes());
            }
            glyf.extend((instr_len as u16).to_be_bytes());
            glyf.extend(instr);
            let overlaps = overlap.is_some_and(|o| o[g / 8] & (0x80 >> (g % 8)) != 0);
            pack_points(&pts, overlaps, &mut glyf);
            x_min.push(b[0]);
        }
        while glyf.len() % 4 != 0 {
            glyf.push(0);
        }
    }
    offsets.push(glyf.len());
    // Short offsets reach 128 KB. A rebuilt table past that (it can come
    // out a little larger than the original) takes long ones, and `head`
    // is told so.
    let short = index_format == 0 && glyf.len() / 2 <= u16::MAX as usize;
    let mut loca = Vec::with_capacity((num_glyphs + 1) * 4);
    for o in offsets {
        if short {
            loca.extend(((o / 2) as u16).to_be_bytes());
        } else {
            loca.extend((o as u32).to_be_bytes());
        }
    }
    Some((glyf, loca, x_min, index_format == 0 && !short))
}

/// A simple glyph's points in TrueType's own packing: a step of under 256
/// is one byte with its sign in the flag, no step at all is no bytes, and a
/// flag repeated is written once with a count. The packing the original font
/// used, near enough, so the rebuilt table is about its size: a font indexing
/// its glyphs with 16-bit offsets has no room for one written out plainly.
fn pack_points(pts: &[(i32, i32, bool)], overlaps: bool, out: &mut Vec<u8>) {
    let mut flags = Vec::with_capacity(pts.len());
    let mut xs = Vec::with_capacity(pts.len());
    let mut ys = Vec::with_capacity(pts.len());
    let mut last = (0i32, 0i32);
    for (i, p) in pts.iter().enumerate() {
        let (dx, dy) = (p.0 - last.0, p.1 - last.1);
        last = (p.0, p.1);
        let mut f = if p.2 { 0x01u8 } else { 0 };
        if i == 0 && overlaps {
            f |= 0x40;
        }
        if dx == 0 {
            f |= 0x10;
        } else if dx.abs() < 256 {
            f |= 0x02 | if dx > 0 { 0x10 } else { 0 };
            xs.push(dx.unsigned_abs() as u8);
        } else {
            xs.extend((dx as i16).to_be_bytes());
        }
        if dy == 0 {
            f |= 0x20;
        } else if dy.abs() < 256 {
            f |= 0x04 | if dy > 0 { 0x20 } else { 0 };
            ys.push(dy.unsigned_abs() as u8);
        } else {
            ys.extend((dy as i16).to_be_bytes());
        }
        flags.push(f);
    }
    let mut i = 0;
    while i < flags.len() {
        let f = flags[i];
        let mut run = 1;
        while i + run < flags.len() && flags[i + run] == f && run < 256 {
            run += 1;
        }
        if run > 1 {
            out.push(f | 0x08);
            out.push((run - 1) as u8);
        } else {
            out.push(f);
        }
        i += run;
    }
    out.extend(xs);
    out.extend(ys);
}

/// One point's step from the last, from its flag and the bytes it takes.
fn triplet(f: u8, r: &mut Reader) -> Option<(i32, i32)> {
    let sign = |bit: u8, v: i32| if bit & 1 != 0 { v } else { -v };
    let f32_ = f as i32;
    Some(if f < 10 {
        let b0 = r.u8()? as i32;
        (0, sign(f, ((f32_ & 14) << 7) + b0))
    } else if f < 20 {
        let b0 = r.u8()? as i32;
        (sign(f, (((f32_ - 10) & 14) << 7) + b0), 0)
    } else if f < 84 {
        let b0 = f32_ - 20;
        let b1 = r.u8()? as i32;
        (sign(f, 1 + (b0 & 0x30) + (b1 >> 4)), sign(f >> 1, 1 + ((b0 & 0x0C) << 2) + (b1 & 0x0F)))
    } else if f < 120 {
        let b0 = f32_ - 84;
        let (b1, b2) = (r.u8()? as i32, r.u8()? as i32);
        (sign(f, 1 + ((b0 / 12) << 8) + b1), sign(f >> 1, 1 + (((b0 % 12) >> 2) << 8) + b2))
    } else if f < 124 {
        let (b1, b2, b3) = (r.u8()? as i32, r.u8()? as i32, r.u8()? as i32);
        (sign(f, (b1 << 4) + (b2 >> 4)), sign(f >> 1, ((b2 & 0x0F) << 8) + b3))
    } else {
        let s = r.bytes(4)?;
        let (b1, b2, b3, b4) = (s[0] as i32, s[1] as i32, s[2] as i32, s[3] as i32);
        (sign(f, (b1 << 8) + b2), sign(f >> 1, (b3 << 8) + b4))
    })
}

/// The real `hmtx` from the rewritten one, whose left side bearings may have
/// been dropped because they equal each glyph's left edge.
fn rebuild_hmtx(t: &[u8], metrics: usize, x_min: &[i16]) -> Option<Vec<u8>> {
    let num_glyphs = x_min.len();
    if metrics == 0 || metrics > num_glyphs {
        return None;
    }
    let mut r = Reader::new(t);
    let flags = r.u8()?;
    let mut advance = Vec::with_capacity(metrics);
    for _ in 0..metrics {
        advance.push(r.u16()?);
    }
    let mut lsb = Vec::with_capacity(num_glyphs);
    for (g, &edge) in x_min.iter().enumerate() {
        // Proportional glyphs (with their own advance) and the monospaced
        // tail each have their own flag.
        let derived = if g < metrics { flags & 1 != 0 } else { flags & 2 != 0 };
        lsb.push(if derived { edge } else { r.i16()? });
    }
    let mut out = Vec::with_capacity(metrics * 4 + (num_glyphs - metrics) * 2);
    for g in 0..num_glyphs {
        if g < metrics {
            out.extend(advance[g].to_be_bytes());
        }
        out.extend(lsb[g].to_be_bytes());
    }
    Some(out)
}

/// An sfnt from its tables: the directory sorted by tag, each table padded to
/// four bytes and checksummed.
fn assemble(flavor: u32, mut tables: Vec<([u8; 4], Vec<u8>)>) -> Vec<u8> {
    tables.sort_by_key(|t| t.0);
    let n = tables.len() as u16;
    let mut out = Vec::new();
    out.extend(flavor.to_be_bytes());
    out.extend(n.to_be_bytes());
    let pow = 1u16 << (15 - n.leading_zeros().min(15));
    let search = pow * 16;
    out.extend(search.to_be_bytes());
    out.extend((pow.trailing_zeros() as u16).to_be_bytes());
    out.extend((n * 16 - search).to_be_bytes());
    let mut at = 12 + tables.len() * 16;
    let mut body = Vec::new();
    for (tag, t) in &tables {
        let sum = t.chunks(4).fold(0u32, |s, c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            s.wrapping_add(u32::from_be_bytes(w))
        });
        out.extend(tag);
        out.extend(sum.to_be_bytes());
        out.extend((at as u32).to_be_bytes());
        out.extend((t.len() as u32).to_be_bytes());
        let pad = (4 - t.len() % 4) % 4;
        at += t.len() + pad;
        body.extend(t);
        body.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bundled UI font cut down to the letters tested (`é` is built from
    /// two other glyphs, so composites are covered)…
    const ROBOTO_TTF: &[u8] = include_bytes!("../test-data/roboto-subset.ttf");
    /// …compressed by Google's reference `woff2_compress`, glyf and loca
    /// rewritten…
    const ROBOTO_WOFF2: &[u8] = include_bytes!("../test-data/roboto-subset.woff2");
    /// …and by fonttools with `hmtx` rewritten too, its side bearings dropped.
    const ROBOTO_HMTX_WOFF2: &[u8] = include_bytes!("../test-data/roboto-subset.hmtx.woff2");

    fn draws_like_the_original(woff2: &[u8]) {
        let ttf = ROBOTO_TTF;
        let back = unwrap_woff2(woff2).expect("unwraps");
        let a = fontdue::Font::from_bytes(ttf, Default::default()).unwrap();
        let b = fontdue::Font::from_bytes(back.as_slice(), Default::default()).unwrap();
        assert_eq!(a.glyph_count(), b.glyph_count());
        for c in "Ag@é&%Qgjy0".chars() {
            assert_ne!(a.lookup_glyph_index(c), 0, "{c} is in the test font");
            assert_eq!(a.rasterize(c, 24.0), b.rasterize(c, 24.0), "{c} draws differently");
            assert_eq!(a.metrics(c, 13.0), b.metrics(c, 13.0), "{c} sits differently");
        }
    }

    /// Every table of an sfnt, by tag.
    fn tables_of(sfnt: &[u8]) -> std::collections::BTreeMap<[u8; 4], Vec<u8>> {
        let mut r = Reader::new(sfnt);
        r.u32().unwrap();
        let n = r.u16().unwrap();
        r.bytes(6).unwrap();
        (0..n)
            .map(|_| {
                let tag: [u8; 4] = r.bytes(4).unwrap().try_into().unwrap();
                r.u32().unwrap();
                let (at, len) = (r.u32().unwrap() as usize, r.u32().unwrap() as usize);
                (tag, sfnt[at..at + len].to_vec())
            })
            .collect()
    }

    /// The tables WOFF2 does not rewrite come back exactly, and so does a
    /// rewritten `hmtx`: rasterizers do not read side bearings, so drawing
    /// the same would not notice them wrong.
    #[test]
    fn every_table_but_the_outlines_comes_back_byte_for_byte() {
        let want = tables_of(ROBOTO_TTF);
        for file in [ROBOTO_WOFF2, ROBOTO_HMTX_WOFF2] {
            let got = tables_of(&unwrap_woff2(file).unwrap());
            assert_eq!(got.keys().collect::<Vec<_>>(), want.keys().collect::<Vec<_>>());
            for (tag, t) in &want {
                // An encoder sets two fields of `head` itself: the whole-file
                // checksum, and the flag saying the font has been through it.
                let mask = |mut h: Vec<u8>| {
                    if tag == b"head" {
                        h[8..12].fill(0);
                        h[16..18].fill(0);
                    }
                    h
                };
                if tag != b"glyf" && tag != b"loca" {
                    assert!(mask(got[tag].clone()) == mask(t.clone()), "{} differs", String::from_utf8_lossy(tag));
                }
            }
            // The outlines are repacked, not copied, and come out about the
            // size they went in: a font whose glyph index is 16-bit has no
            // room for much more.
            let (a, b) = (want[b"glyf"].len() as f32, got[b"glyf"].len() as f32);
            assert!(b < a * 1.1, "glyf grew from {a} to {b} bytes");
            assert_eq!(got[b"loca"].len(), want[b"loca"].len());
        }
    }

    #[test]
    fn a_woff2_font_unwraps_to_one_that_draws_the_same() {
        assert!(is_woff2(ROBOTO_WOFF2));
        draws_like_the_original(ROBOTO_WOFF2);
    }

    #[test]
    fn a_woff2_font_with_its_metrics_rewritten_draws_the_same() {
        draws_like_the_original(ROBOTO_HMTX_WOFF2);
    }

    #[test]
    fn a_damaged_woff2_is_refused() {
        assert!(unwrap_woff2(&ROBOTO_WOFF2[..ROBOTO_WOFF2.len() / 2]).is_none());
        let mut lying = ROBOTO_WOFF2.to_vec();
        // totalSfntSize: a gigabyte.
        lying[16..20].copy_from_slice(&(1u32 << 30).to_be_bytes());
        assert!(unwrap_woff2(&lying).is_none());
        assert!(unwrap_woff2(b"wOF2").is_none());
    }

    #[test]
    fn packed_numbers_read_as_the_spec_writes_them() {
        assert_eq!(Reader::new(&[0x3F]).base128(), Some(63));
        assert_eq!(Reader::new(&[0x81, 0x00]).base128(), Some(128));
        assert_eq!(Reader::new(&[0x80, 0x01]).base128(), None, "a leading zero byte is invalid");
        assert_eq!(Reader::new(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).base128(), None, "past 32 bits");
        assert_eq!(Reader::new(&[252]).u255(), Some(252));
        assert_eq!(Reader::new(&[255, 1]).u255(), Some(254));
        assert_eq!(Reader::new(&[254, 0]).u255(), Some(506));
        assert_eq!(Reader::new(&[253, 0x12, 0x34]).u255(), Some(0x1234));
    }
}

