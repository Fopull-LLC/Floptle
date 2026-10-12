//! **Reading an `.aseprite` file.**
//!
//! The JSON import ([`crate::aseprite`]) needs the artist to export a sheet
//! first, and to export it again after every change. Reading the file itself
//! removes that step: save in Aseprite and the game has the new frames.
//!
//! What is read is what Aseprite's own export draws: every visible layer of
//! every frame, flattened, plus the tags. Group visibility and opacity, linked
//! cels, z-index, indexed and grayscale sprites, and tilemap layers are all
//! honoured. Blend modes other than Normal are drawn as Normal and named in
//! [`Sprite::warnings`], so a sprite that relies on one says so rather than
//! quietly looking different.
//!
//! The format is Aseprite's published file spec
//! (`docs/ase-file-specs.md` in the Aseprite repository).

use std::collections::HashMap;
use std::io::Read;

/// How a tag plays its frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Direction {
    Forward,
    Reverse,
    PingPong,
    PingPongReverse,
}

/// One Aseprite tag: a named run of frames.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Tag {
    pub(crate) name: String,
    pub(crate) from: usize,
    pub(crate) to: usize,
    pub(crate) direction: Direction,
    /// How many times it plays. `0` is forever.
    pub(crate) repeat: u32,
}

/// One flattened frame.
pub(crate) struct Frame {
    /// `w × h` RGBA, not premultiplied.
    pub(crate) rgba: Vec<u8>,
    pub(crate) ms: u32,
}

/// A whole sprite, flattened frame by frame.
pub(crate) struct Sprite {
    pub(crate) w: u32,
    pub(crate) h: u32,
    pub(crate) frames: Vec<Frame>,
    pub(crate) tags: Vec<Tag>,
    /// Things drawn differently from Aseprite, one sentence each.
    pub(crate) warnings: Vec<String>,
}

struct Layer {
    name: String,
    visible: bool,
    background: bool,
    kind: u16,
    blend: u16,
    opacity: u8,
    child_level: u16,
    tileset: u32,
}

#[derive(Clone)]
enum CelImage {
    Pixels { w: u32, h: u32, rgba: Vec<u8> },
    Tiles { w: u32, h: u32, tiles: Vec<u32>, id_mask: u32, xflip: u32, yflip: u32, dflip: u32 },
}

#[derive(Clone)]
struct Cel {
    layer: usize,
    x: i32,
    y: i32,
    opacity: u8,
    z: i16,
    image: CelImage,
}

struct Tileset {
    tw: u32,
    th: u32,
    /// `n` tiles stacked vertically, RGBA.
    rgba: Vec<u8>,
    n: u32,
}

struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, n: usize) -> Result<(), String> {
        if self.at + n > self.b.len() {
            Err("the file ends partway through — is it a complete .aseprite file?".into())
        } else {
            Ok(())
        }
    }
    fn u8(&mut self) -> Result<u8, String> {
        self.need(1)?;
        self.at += 1;
        Ok(self.b[self.at - 1])
    }
    fn u16(&mut self) -> Result<u16, String> {
        self.need(2)?;
        self.at += 2;
        Ok(u16::from_le_bytes([self.b[self.at - 2], self.b[self.at - 1]]))
    }
    fn i16(&mut self) -> Result<i16, String> {
        Ok(self.u16()? as i16)
    }
    fn u32(&mut self) -> Result<u32, String> {
        self.need(4)?;
        let s = &self.b[self.at..self.at + 4];
        self.at += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn skip(&mut self, n: usize) -> Result<(), String> {
        self.need(n)?;
        self.at += n;
        Ok(())
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], String> {
        self.need(n)?;
        self.at += n;
        Ok(&self.b[self.at - n..self.at])
    }
    fn string(&mut self) -> Result<String, String> {
        let n = self.u16()? as usize;
        Ok(String::from_utf8_lossy(self.bytes(n)?).into_owned())
    }
}

fn inflate(data: &[u8], expect: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expect);
    flate2::read::ZlibDecoder::new(data)
        .take(expect as u64)
        .read_to_end(&mut out)
        .map_err(|e| format!("a cel's compressed pixels do not decompress: {e}"))?;
    if out.len() < expect {
        return Err("a cel's compressed pixels are shorter than its size says".into());
    }
    Ok(out)
}

/// Turn `n` pixels of the sprite's colour depth into RGBA.
/// `transparent` is the indexed sprite's mask colour; a background layer has
/// none, and draws that palette entry as the colour it is.
fn to_rgba(raw: &[u8], n: usize, depth: u16, palette: &[[u8; 4]], transparent: Option<u8>) -> Vec<u8> {
    let mut out = vec![0u8; n * 4];
    for i in 0..n {
        let px = match depth {
            32 => [raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]],
            16 => [raw[i * 2], raw[i * 2], raw[i * 2], raw[i * 2 + 1]],
            _ => {
                let ix = raw[i];
                if Some(ix) == transparent {
                    [0; 4]
                } else {
                    palette.get(ix as usize).copied().unwrap_or([0; 4])
                }
            }
        };
        out[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    out
}

/// Draw `src` over `dst` at `alpha` (0–1 on top of the pixel's own), the
/// Normal blend on straight alpha.
fn over(dst: &mut [u8], src: [u8; 4], alpha: f32) {
    let sa = src[3] as f32 / 255.0 * alpha;
    if sa <= 0.0 {
        return;
    }
    let da = dst[3] as f32 / 255.0;
    let oa = sa + da * (1.0 - sa);
    for c in 0..3 {
        let v = (src[c] as f32 * sa + dst[c] as f32 * da * (1.0 - sa)) / oa;
        dst[c] = v.round().clamp(0.0, 255.0) as u8;
    }
    dst[3] = (oa * 255.0).round().clamp(0.0, 255.0) as u8;
}

/// Read an `.aseprite` / `.ase` file and flatten it.
pub(crate) fn read(bytes: &[u8]) -> Result<Sprite, String> {
    let mut r = Reader { b: bytes, at: 0 };
    r.need(128)?;
    let _size = r.u32()?;
    if r.u16()? != 0xA5E0 {
        return Err("this is not an Aseprite file (its header is wrong)".into());
    }
    let n_frames = r.u16()? as usize;
    let w = r.u16()? as u32;
    let h = r.u16()? as u32;
    let depth = r.u16()?;
    if !matches!(depth, 8 | 16 | 32) {
        return Err(format!("this file has a colour depth of {depth} bits, which Aseprite does not write"));
    }
    let flags = r.u32()?;
    r.skip(2 + 4 + 4)?;
    let transparent = r.u8()?;
    r.at = 128;
    if w == 0 || h == 0 {
        return Err("this sprite has no size".into());
    }

    let mut palette: Vec<[u8; 4]> = vec![[0, 0, 0, 255]; 256];
    let mut has_new_palette = false;
    let mut layers: Vec<Layer> = Vec::new();
    let mut tags: Vec<Tag> = Vec::new();
    let mut tilesets: HashMap<u32, Tileset> = HashMap::new();
    let mut frame_cels: Vec<Vec<Cel>> = Vec::with_capacity(n_frames);
    let mut durations = Vec::with_capacity(n_frames);
    let mut warnings = Vec::new();
    // Raw indexed/grayscale cels decode once the whole palette is known, which
    // in a file with several palette chunks is not until the frame is read.
    struct Pending {
        layer: usize,
        x: i32,
        y: i32,
        opacity: u8,
        z: i16,
        w: u32,
        h: u32,
        raw: Vec<u8>,
    }

    for fi in 0..n_frames {
        let start = r.at;
        let frame_len = r.u32()? as usize;
        if r.u16()? != 0xF1FA {
            return Err(format!("frame {fi} is damaged (its header is wrong)"));
        }
        let old_chunks = r.u16()? as usize;
        let ms = r.u16()? as u32;
        r.skip(2)?;
        let new_chunks = r.u32()? as usize;
        let chunks = if new_chunks == 0 { old_chunks } else { new_chunks };
        durations.push(ms);
        let mut cels: Vec<Cel> = Vec::new();
        let mut pending: Vec<Pending> = Vec::new();
        for _ in 0..chunks {
            let cstart = r.at;
            let clen = r.u32()? as usize;
            let ctype = r.u16()?;
            if clen < 6 {
                return Err(format!("frame {fi} has a damaged chunk"));
            }
            r.need(clen - 6)?;
            let cend = cstart + clen;
            match ctype {
                // Old palettes, read only when there is no new-style one.
                0x0004 | 0x0011 if !has_new_palette => {
                    let packets = r.u16()?;
                    let mut ix = 0usize;
                    for _ in 0..packets {
                        ix += r.u8()? as usize;
                        let n = match r.u8()? {
                            0 => 256,
                            n => n as usize,
                        };
                        for _ in 0..n {
                            let (cr, cg, cb) = (r.u8()?, r.u8()?, r.u8()?);
                            let scale = |v: u8| if ctype == 0x0011 { (v as u32 * 255 / 63) as u8 } else { v };
                            if ix < 256 {
                                palette[ix] = [scale(cr), scale(cg), scale(cb), 255];
                            }
                            ix += 1;
                        }
                    }
                }
                0x2019 => {
                    has_new_palette = true;
                    let size = r.u32()? as usize;
                    let first = r.u32()? as usize;
                    let last = r.u32()? as usize;
                    r.skip(8)?;
                    if palette.len() < size {
                        palette.resize(size, [0, 0, 0, 255]);
                    }
                    for ix in first..=last {
                        let ef = r.u16()?;
                        let c = [r.u8()?, r.u8()?, r.u8()?, r.u8()?];
                        if ef & 1 != 0 {
                            r.string()?;
                        }
                        if ix < palette.len() {
                            palette[ix] = c;
                        }
                    }
                }
                0x2004 => {
                    let lf = r.u16()?;
                    let kind = r.u16()?;
                    let child_level = r.u16()?;
                    r.skip(4)?;
                    let blend = r.u16()?;
                    let opacity = r.u8()?;
                    r.skip(3)?;
                    let name = r.string()?;
                    let tileset = if kind == 2 { r.u32()? } else { 0 };
                    // A reference layer is a tracing guide, not part of the
                    // picture, and Aseprite's export leaves it out too.
                    let visible = lf & 1 != 0 && lf & 64 == 0;
                    let opacity = if flags & 1 != 0 { opacity } else { 255 };
                    layers.push(Layer { name, visible, background: lf & 8 != 0, kind, blend, opacity, child_level, tileset });
                }
                0x2005 => {
                    let layer = r.u16()? as usize;
                    let x = r.i16()? as i32;
                    let y = r.i16()? as i32;
                    let opacity = r.u8()?;
                    let ct = r.u16()?;
                    let z = r.i16()?;
                    r.skip(5)?;
                    match ct {
                        0 | 2 => {
                            let cw = r.u16()? as u32;
                            let chh = r.u16()? as u32;
                            let bpp = depth as usize / 8;
                            let n = (cw * chh) as usize * bpp;
                            let raw = if ct == 0 {
                                r.bytes(n)?.to_vec()
                            } else {
                                inflate(&bytes[r.at..cend], n)?
                            };
                            pending.push(Pending { layer, x, y, opacity, z, w: cw, h: chh, raw });
                        }
                        1 => {
                            let src = r.u16()? as usize;
                            if let Some(c) = frame_cels.get(src).and_then(|cs| cs.iter().find(|c| c.layer == layer)) {
                                // The linked cel shares the pixels; its own
                                // position and opacity are the ones it says.
                                cels.push(Cel { layer, x, y, opacity, z, image: c.image.clone() });
                            }
                        }
                        3 => {
                            let tw = r.u16()? as u32;
                            let th = r.u16()? as u32;
                            let bits = r.u16()?;
                            let id_mask = r.u32()?;
                            let xflip = r.u32()?;
                            let yflip = r.u32()?;
                            let dflip = r.u32()?;
                            r.skip(10)?;
                            if bits != 32 {
                                return Err(format!("a tilemap cel uses {bits}-bit tiles; only 32-bit is in the spec"));
                            }
                            let n = (tw * th) as usize;
                            let raw = inflate(&bytes[r.at..cend], n * 4)?;
                            let tiles = raw
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .map(|c| u32::from_le_bytes(*c))
                                .collect();
                            cels.push(Cel {
                                layer,
                                x,
                                y,
                                opacity,
                                z,
                                image: CelImage::Tiles { w: tw, h: th, tiles, id_mask, xflip, yflip, dflip },
                            });
                        }
                        _ => {}
                    }
                }
                0x2018 => {
                    let n = r.u16()?;
                    r.skip(8)?;
                    for _ in 0..n {
                        let from = r.u16()? as usize;
                        let to = r.u16()? as usize;
                        let direction = match r.u8()? {
                            1 => Direction::Reverse,
                            2 => Direction::PingPong,
                            3 => Direction::PingPongReverse,
                            _ => Direction::Forward,
                        };
                        let repeat = r.u16()? as u32;
                        r.skip(6 + 3 + 1)?;
                        let name = r.string()?;
                        tags.push(Tag { name, from, to, direction, repeat });
                    }
                }
                0x2023 => {
                    let id = r.u32()?;
                    let tf = r.u32()?;
                    let n = r.u32()?;
                    let tw = r.u16()? as u32;
                    let th = r.u16()? as u32;
                    r.skip(2 + 14)?;
                    r.string()?;
                    if tf & 1 != 0 {
                        r.skip(8)?;
                    }
                    if tf & 2 != 0 {
                        let len = r.u32()? as usize;
                        let px = (tw * th * n) as usize;
                        let raw = inflate(r.bytes(len)?, px * depth as usize / 8)?;
                        let rgba = to_rgba(&raw, px, depth, &palette, Some(transparent));
                        tilesets.insert(id, Tileset { tw, th, rgba, n });
                    } else {
                        warnings.push(format!(
                            "tileset {id} lives in another file, so its tiles are left out"
                        ));
                    }
                }
                _ => {}
            }
            r.at = cend;
        }
        for p in pending {
            let mask = if layers.get(p.layer).is_some_and(|l| l.background) { None } else { Some(transparent) };
            let rgba = to_rgba(&p.raw, (p.w * p.h) as usize, depth, &palette, mask);
            cels.push(Cel {
                layer: p.layer,
                x: p.x,
                y: p.y,
                opacity: p.opacity,
                z: p.z,
                image: CelImage::Pixels { w: p.w, h: p.h, rgba },
            });
        }
        frame_cels.push(cels);
        if frame_len >= 16 {
            r.at = start + frame_len;
        }
    }

    // A layer shows when it and every group above it are visible. Opacity
    // multiplies down through groups when the file says group opacity is set.
    let mut shown = vec![false; layers.len()];
    let mut alpha = vec![1.0f32; layers.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (i, l) in layers.iter().enumerate() {
        stack.truncate(l.child_level as usize);
        let parents_shown = stack.iter().all(|&p| layers[p].visible);
        let parent_alpha: f32 = if flags & 2 != 0 {
            stack.iter().map(|&p| layers[p].opacity as f32 / 255.0).product()
        } else {
            1.0
        };
        shown[i] = l.visible && parents_shown && l.kind != 1;
        alpha[i] = l.opacity as f32 / 255.0 * parent_alpha;
        if l.kind == 1 {
            stack.push(i);
        }
    }
    let mut odd_blends: Vec<&str> = Vec::new();
    for (i, l) in layers.iter().enumerate() {
        if shown[i] && l.blend != 0 && !odd_blends.contains(&l.name.as_str()) {
            odd_blends.push(&l.name);
        }
    }
    if !odd_blends.is_empty() {
        warnings.push(format!(
            "layer {} uses a blend mode other than Normal, and is drawn as Normal",
            odd_blends.join(", ")
        ));
    }

    let mut frames = Vec::with_capacity(n_frames);
    for (fi, mut cels) in frame_cels.into_iter().enumerate() {
        // Aseprite's draw order: layer index plus z-index, and on a tie the
        // larger z-index on top.
        cels.sort_by_key(|c| (c.layer as i32 + c.z as i32, c.z));
        let mut canvas = vec![0u8; (w * h * 4) as usize];
        for c in &cels {
            if c.layer >= layers.len() || !shown[c.layer] {
                continue;
            }
            let a = alpha[c.layer] * c.opacity as f32 / 255.0;
            let mut put = |px: i32, py: i32, src: [u8; 4]| {
                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                    return;
                }
                let o = ((py as u32 * w + px as u32) * 4) as usize;
                over(&mut canvas[o..o + 4], src, a);
            };
            match &c.image {
                CelImage::Pixels { w: cw, h: chh, rgba } => {
                    for yy in 0..*chh {
                        for xx in 0..*cw {
                            let o = ((yy * cw + xx) * 4) as usize;
                            let s = [rgba[o], rgba[o + 1], rgba[o + 2], rgba[o + 3]];
                            put(c.x + xx as i32, c.y + yy as i32, s);
                        }
                    }
                }
                CelImage::Tiles { w: tw, h: th, tiles, id_mask, xflip, yflip, dflip } => {
                    let Some(ts) = tilesets.get(&layers[c.layer].tileset) else { continue };
                    for ty in 0..*th {
                        for tx in 0..*tw {
                            let t = tiles[(ty * tw + tx) as usize];
                            let id = t & id_mask;
                            // Tile 0 is the empty tile.
                            if id == 0 || id >= ts.n {
                                continue;
                            }
                            for py in 0..ts.th {
                                for px in 0..ts.tw {
                                    let (mut sx, mut sy) = (px, py);
                                    if t & dflip != 0 {
                                        std::mem::swap(&mut sx, &mut sy);
                                    }
                                    if t & xflip != 0 {
                                        sx = ts.tw - 1 - sx;
                                    }
                                    if t & yflip != 0 {
                                        sy = ts.th - 1 - sy;
                                    }
                                    if sx >= ts.tw || sy >= ts.th {
                                        continue;
                                    }
                                    let o = (((id * ts.th + sy) * ts.tw + sx) * 4) as usize;
                                    let s = [ts.rgba[o], ts.rgba[o + 1], ts.rgba[o + 2], ts.rgba[o + 3]];
                                    put(
                                        c.x + (tx * ts.tw + px) as i32,
                                        c.y + (ty * ts.th + py) as i32,
                                        s,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        frames.push(Frame { rgba: canvas, ms: durations[fi] });
    }
    if frames.is_empty() {
        return Err("this sprite has no frames".into());
    }
    tags.retain(|t| t.from < frames.len());
    for t in &mut tags {
        t.to = t.to.min(frames.len() - 1);
    }
    Ok(Sprite { w, h, frames, tags, warnings })
}

/// The frames a tag plays, in order, once round: ping-pong goes there and back
/// without repeating either end, the way Aseprite plays it.
pub(crate) fn tag_frames(from: usize, to: usize, direction: Direction) -> Vec<usize> {
    let fwd: Vec<usize> = (from..=to).collect();
    let back: Vec<usize> = fwd.iter().rev().copied().collect();
    let inner = |v: &[usize]| -> Vec<usize> {
        if v.len() > 2 { v[1..v.len() - 1].to_vec() } else { Vec::new() }
    };
    match direction {
        Direction::Forward => fwd,
        Direction::Reverse => back,
        Direction::PingPong => [fwd.clone(), inner(&back)].concat(),
        Direction::PingPongReverse => [back.clone(), inner(&fwd)].concat(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// A cel as (layer, x, y, w, h, rgba).
    pub(crate) type TestCel = (u16, i16, i16, u16, u16, Vec<u8>);

    /// A minimal Aseprite writer for tests: RGBA, one compressed cel per layer
    /// per frame, given layers (name, visible, child level, is group), tags.
    pub(crate) struct Build {
        pub w: u16,
        pub h: u16,
        pub layers: Vec<(&'static str, bool, u16, bool)>,
        /// Per frame: (duration, cels).
        pub frames: Vec<(u16, Vec<TestCel>)>,
        pub tags: Vec<(&'static str, u16, u16, u8, u16)>,
    }

    fn chunk(t: u16, body: &[u8]) -> Vec<u8> {
        let mut c = ((body.len() + 6) as u32).to_le_bytes().to_vec();
        c.extend(t.to_le_bytes());
        c.extend(body);
        c
    }

    fn string(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u16).to_le_bytes().to_vec();
        v.extend(s.as_bytes());
        v
    }

    impl Build {
        pub(crate) fn bytes(&self) -> Vec<u8> {
            let mut frames = Vec::new();
            for (fi, (ms, cels)) in self.frames.iter().enumerate() {
                let mut chunks = Vec::new();
                if fi == 0 {
                    for (name, vis, level, group) in &self.layers {
                        let mut b = Vec::new();
                        b.extend((if *vis { 1u16 } else { 0 }).to_le_bytes());
                        b.extend((if *group { 1u16 } else { 0 }).to_le_bytes());
                        b.extend(level.to_le_bytes());
                        b.extend([0u8; 4]);
                        b.extend(0u16.to_le_bytes());
                        b.push(255);
                        b.extend([0u8; 3]);
                        b.extend(string(name));
                        chunks.push(chunk(0x2004, &b));
                    }
                    if !self.tags.is_empty() {
                        let mut b = (self.tags.len() as u16).to_le_bytes().to_vec();
                        b.extend([0u8; 8]);
                        for (name, from, to, dir, rep) in &self.tags {
                            b.extend(from.to_le_bytes());
                            b.extend(to.to_le_bytes());
                            b.push(*dir);
                            b.extend(rep.to_le_bytes());
                            b.extend([0u8; 10]);
                            b.extend(string(name));
                        }
                        chunks.push(chunk(0x2018, &b));
                    }
                }
                for (layer, x, y, cw, ch, rgba) in cels {
                    let mut b = Vec::new();
                    b.extend(layer.to_le_bytes());
                    b.extend(x.to_le_bytes());
                    b.extend(y.to_le_bytes());
                    b.push(255);
                    b.extend(2u16.to_le_bytes());
                    b.extend(0i16.to_le_bytes());
                    b.extend([0u8; 5]);
                    b.extend(cw.to_le_bytes());
                    b.extend(ch.to_le_bytes());
                    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                    z.write_all(rgba).unwrap();
                    b.extend(z.finish().unwrap());
                    chunks.push(chunk(0x2005, &b));
                }
                let body: Vec<u8> = chunks.concat();
                let mut f = ((body.len() + 16) as u32).to_le_bytes().to_vec();
                f.extend(0xF1FAu16.to_le_bytes());
                f.extend((chunks.len() as u16).to_le_bytes());
                f.extend(ms.to_le_bytes());
                f.extend([0u8; 2]);
                f.extend((chunks.len() as u32).to_le_bytes());
                f.extend(body);
                frames.extend(f);
            }
            let mut out = Vec::new();
            out.extend(((128 + frames.len()) as u32).to_le_bytes());
            out.extend(0xA5E0u16.to_le_bytes());
            out.extend((self.frames.len() as u16).to_le_bytes());
            out.extend(self.w.to_le_bytes());
            out.extend(self.h.to_le_bytes());
            out.extend(32u16.to_le_bytes());
            out.extend(1u32.to_le_bytes());
            out.resize(128, 0);
            out.extend(frames);
            out
        }
    }

    pub(crate) fn solid(w: u16, h: u16, c: [u8; 4]) -> Vec<u8> {
        c.iter().copied().cycle().take(w as usize * h as usize * 4).collect()
    }

    fn px(s: &Sprite, f: usize, x: u32, y: u32) -> [u8; 4] {
        let o = ((y * s.w + x) * 4) as usize;
        let p = &s.frames[f].rgba[o..o + 4];
        [p[0], p[1], p[2], p[3]]
    }

    /// Layers stack in order, a cel lands at its own offset, and each frame
    /// keeps its duration.
    #[test]
    fn layers_flatten_in_order_at_their_offsets() {
        let red = [255, 0, 0, 255];
        let blue = [0, 0, 255, 255];
        let s = read(
            &Build {
                w: 4,
                h: 4,
                layers: vec![("bg", true, 0, false), ("fg", true, 0, false)],
                frames: vec![(
                    150,
                    vec![(0, 0, 0, 4, 4, solid(4, 4, red)), (1, 2, 1, 1, 1, solid(1, 1, blue))],
                )],
                tags: vec![],
            }
            .bytes(),
        )
        .unwrap();
        assert_eq!((s.w, s.h, s.frames.len(), s.frames[0].ms), (4, 4, 1, 150));
        assert_eq!(px(&s, 0, 2, 1), blue, "the upper layer covers the lower one");
        assert_eq!(px(&s, 0, 1, 1), red);
    }

    /// A hidden layer, and every layer inside a hidden group, is left out.
    #[test]
    fn hidden_layers_and_groups_are_left_out() {
        let s = read(
            &Build {
                w: 2,
                h: 1,
                layers: vec![
                    ("shown", true, 0, false),
                    ("group", false, 0, true),
                    ("inside", true, 1, false),
                ],
                frames: vec![(
                    100,
                    vec![(0, 0, 0, 1, 1, solid(1, 1, [9, 9, 9, 255])), (2, 1, 0, 1, 1, solid(1, 1, [7, 7, 7, 255]))],
                )],
                tags: vec![],
            }
            .bytes(),
        )
        .unwrap();
        assert_eq!(px(&s, 0, 0, 0), [9, 9, 9, 255]);
        assert_eq!(px(&s, 0, 1, 0)[3], 0, "the layer inside the hidden group must not draw");
    }

    #[test]
    fn tags_keep_their_direction_and_repeats() {
        let one = |c| vec![(0u16, 0i16, 0i16, 1u16, 1u16, solid(1, 1, c))];
        let s = read(
            &Build {
                w: 1,
                h: 1,
                layers: vec![("a", true, 0, false)],
                frames: (0..4).map(|i| (100u16, one([i as u8, 0, 0, 255]))).collect(),
                tags: vec![("walk", 0, 3, 2, 0), ("hit", 1, 2, 1, 2)],
            }
            .bytes(),
        )
        .unwrap();
        assert_eq!(s.tags[0], Tag { name: "walk".into(), from: 0, to: 3, direction: Direction::PingPong, repeat: 0 });
        assert_eq!(s.tags[1].direction, Direction::Reverse);
        assert_eq!(s.tags[1].repeat, 2);
        assert_eq!(px(&s, 3, 0, 0), [3, 0, 0, 255]);
    }

    #[test]
    fn ping_pong_does_not_repeat_its_ends() {
        assert_eq!(tag_frames(0, 3, Direction::PingPong), vec![0, 1, 2, 3, 2, 1]);
        assert_eq!(tag_frames(0, 3, Direction::PingPongReverse), vec![3, 2, 1, 0, 1, 2]);
        assert_eq!(tag_frames(2, 4, Direction::Reverse), vec![4, 3, 2]);
        assert_eq!(tag_frames(5, 5, Direction::PingPong), vec![5]);
    }

    #[test]
    fn something_that_is_not_aseprite_says_so() {
        let mut b = vec![0u8; 200];
        b[4] = 0x50;
        assert!(read(&b).err().unwrap().contains("not an Aseprite file"));
        assert!(read(&[1, 2, 3]).err().unwrap().contains("ends partway"));
    }
}

/// Flatten every `.aseprite` in `$ASE_DIR` to `<name>.f<N>.png` beside it, to
/// look at against what Aseprite shows.
#[cfg(test)]
#[test]
#[ignore]
fn probe_flatten_a_folder() {
    let dir = std::env::var("ASE_DIR").expect("set ASE_DIR");
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.extension().is_none_or(|x| x != "aseprite") {
            continue;
        }
        match read(&std::fs::read(&p).unwrap()) {
            Ok(s) => {
                eprintln!("{}: {}x{} {} frames, tags {:?}, warnings {:?}", p.display(), s.w, s.h, s.frames.len(), s.tags, s.warnings);
                for (i, f) in s.frames.iter().enumerate() {
                    let img = image::RgbaImage::from_raw(s.w, s.h, f.rgba.clone()).unwrap();
                    let big = image::imageops::resize(&img, s.w * 16, s.h * 16, image::imageops::FilterType::Nearest);
                    big.save(p.with_extension(format!("f{i}.png"))).unwrap();
                }
            }
            Err(e) => eprintln!("{}: ERROR {e}", p.display()),
        }
    }
}
