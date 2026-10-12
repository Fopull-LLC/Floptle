//! Bitmap fonts: letters drawn as pictures, the way pixel-art games letter.
//!
//! A pixel font made as an outline font (`.ttf`) only looks right at the one
//! size its grid was designed for, and only if its units-per-em was set up
//! for that grid. A bitmap font is the letters themselves on a sheet, so it is
//! exact at every whole multiple of its size. This reads the **BMFont**
//! description (`.fnt`, text or XML) that BMFont, Hiero, Littera, ShoeBox and
//! most font-sheet tools write, beside the sheet images it names.
//!
//! The renderer's glyph atlas holds coverage, one channel, so a glyph is drawn
//! in the text's colour from its sheet's alpha. A sheet with no transparency
//! at all (white letters on black) uses brightness instead.

use std::collections::HashMap;

/// One letter's place on its sheet, as the description gives it (pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BmChar {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// From the pen position to the glyph's left edge.
    pub xoffset: i32,
    /// From the top of the line to the glyph's top edge.
    pub yoffset: i32,
    pub xadvance: i32,
    pub page: u32,
}

/// A parsed `.fnt` description: everything but the sheet pixels.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BmDesc {
    /// The size the letters were drawn at, in pixels per em.
    pub size: u32,
    pub line_height: u32,
    /// From the top of the line to the baseline.
    pub base: u32,
    /// Sheet image file names, by page id, relative to the `.fnt`.
    pub pages: Vec<String>,
    pub chars: HashMap<char, BmChar>,
}

/// Read a BMFont description, text or XML.
pub fn parse_bmfont(src: &str) -> Result<BmDesc, String> {
    let mut d = BmDesc::default();
    let mut pages: Vec<(u32, String)> = Vec::new();
    for raw in src.lines() {
        // XML puts each record in a tag: `<char id="65" x="0" … />`. Strip the
        // angle brackets and the self-close and it is the text format.
        let line = raw.trim().trim_start_matches('<').trim_end_matches('>').trim_end_matches('/').trim();
        let (tag, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let kv = attrs(rest);
        let num = |k: &str| kv.get(k).and_then(|v| v.parse::<i64>().ok());
        match tag {
            "info" => d.size = num("size").map(|v| v.unsigned_abs() as u32).unwrap_or(0),
            "common" => {
                d.line_height = num("lineHeight").unwrap_or(0).max(0) as u32;
                d.base = num("base").unwrap_or(0).max(0) as u32;
            }
            "page" => {
                if let (Some(id), Some(file)) = (num("id"), kv.get("file")) {
                    pages.push((id.max(0) as u32, file.clone()));
                }
            }
            "char" => {
                let Some(id) = num("id").and_then(|v| u32::try_from(v).ok()).and_then(char::from_u32) else {
                    continue;
                };
                d.chars.insert(
                    id,
                    BmChar {
                        x: num("x").unwrap_or(0).max(0) as u32,
                        y: num("y").unwrap_or(0).max(0) as u32,
                        w: num("width").unwrap_or(0).max(0) as u32,
                        h: num("height").unwrap_or(0).max(0) as u32,
                        xoffset: num("xoffset").unwrap_or(0) as i32,
                        yoffset: num("yoffset").unwrap_or(0) as i32,
                        xadvance: num("xadvance").unwrap_or(0) as i32,
                        page: num("page").unwrap_or(0).max(0) as u32,
                    },
                );
            }
            _ => {}
        }
    }
    if d.chars.is_empty() {
        return Err("no characters — is this a BMFont .fnt in its text or XML form?".into());
    }
    if d.line_height == 0 {
        d.line_height = d.chars.values().map(|c| (c.yoffset.max(0) as u32) + c.h).max().unwrap_or(1);
    }
    if d.base == 0 {
        d.base = d.line_height;
    }
    if d.size == 0 {
        d.size = d.line_height;
    }
    pages.sort_by_key(|(id, _)| *id);
    d.pages = pages.into_iter().map(|(_, f)| f).collect();
    if d.pages.is_empty() {
        return Err("names no sheet image (no `page` line)".into());
    }
    Ok(d)
}

/// `key=value key="quoted value"` pairs.
fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s.trim();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().rsplit(char::is_whitespace).next().unwrap_or("").to_string();
        let after = &rest[eq + 1..];
        let (val, next) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(end) => (q[..end].to_string(), &q[end + 1..]),
                None => (q.to_string(), ""),
            }
        } else {
            let end = after.find(char::is_whitespace).unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        out.insert(key, val);
        rest = next.trim_start();
    }
    out
}

/// A glyph rasterized for the atlas: its coverage and where it sits.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Raster {
    pub xmin: i32,
    /// From the baseline up to the glyph's bottom edge.
    pub ymin: i32,
    pub width: usize,
    pub height: usize,
    pub advance: f32,
}

/// A bitmap font ready to draw: its description and its sheets' coverage.
#[derive(Clone, Debug)]
pub struct BitmapFont {
    desc: BmDesc,
    /// Per page: coverage, width, height.
    pages: Vec<(Vec<u8>, u32, u32)>,
}

impl BitmapFont {
    /// Build from a description and its sheets as RGBA8, in page order. A
    /// page that could not be read is `None`: its letters draw as nothing
    /// rather than the whole font failing.
    pub fn new(desc: BmDesc, sheets: Vec<Option<(Vec<u8>, u32, u32)>>) -> Self {
        let pages = sheets
            .into_iter()
            .map(|s| match s {
                Some((rgba, w, h)) => {
                    let alpha_used = rgba.chunks(4).any(|p| p[3] < 255);
                    let cov = rgba
                        .chunks(4)
                        .map(|p| {
                            if alpha_used {
                                p[3]
                            } else {
                                ((p[0] as u32 * 54 + p[1] as u32 * 183 + p[2] as u32 * 19) >> 8) as u8
                            }
                        })
                        .collect();
                    (cov, w, h)
                }
                None => (Vec::new(), 0, 0),
            })
            .collect();
        Self { desc, pages }
    }

    /// Pixels per design pixel at `px` pixels per em.
    fn scale(&self, px: f32) -> f32 {
        px / self.desc.size.max(1) as f32
    }

    pub fn has_glyph(&self, c: char) -> bool {
        self.desc.chars.contains_key(&c)
    }

    pub fn advance(&self, c: char, px: f32) -> f32 {
        let s = self.scale(px);
        match self.desc.chars.get(&c) {
            Some(g) => g.xadvance as f32 * s,
            // A space a sheet left out still has a width: half the size.
            None if c == ' ' => self.desc.size as f32 * 0.5 * s,
            None => 0.0,
        }
    }

    /// `(ascent, descent)` at `px`, descent negative as fontdue gives it.
    pub fn line(&self, px: f32) -> (f32, f32) {
        let s = self.scale(px);
        (self.desc.base as f32 * s, -((self.desc.line_height as f32 - self.desc.base as f32) * s))
    }

    /// The glyph at `px`, scaled from its sheet by nearest neighbour, so a
    /// whole multiple of the drawn size stays exactly as drawn.
    pub fn rasterize(&self, c: char, px: f32) -> (Raster, Vec<u8>) {
        let s = self.scale(px);
        let Some(g) = self.desc.chars.get(&c).copied() else {
            return (Raster { advance: self.advance(c, px), ..Default::default() }, Vec::new());
        };
        let (w, h) = (((g.w as f32) * s).round() as usize, ((g.h as f32) * s).round() as usize);
        let advance = g.xadvance as f32 * s;
        let Some((cov, pw, ph)) = self.pages.get(g.page as usize) else {
            return (Raster { advance, ..Default::default() }, Vec::new());
        };
        if w == 0 || h == 0 || *pw == 0 {
            return (Raster { advance, ..Default::default() }, Vec::new());
        }
        let mut out = vec![0u8; w * h];
        for y in 0..h {
            let sy = g.y + ((y as f32 + 0.5) / s) as u32;
            for x in 0..w {
                let sx = g.x + ((x as f32 + 0.5) / s) as u32;
                if sx < *pw && sy < *ph && sx < g.x + g.w && sy < g.y + g.h {
                    out[y * w + x] = cov[(sy * pw + sx) as usize];
                }
            }
        }
        let top = (self.desc.base as f32 - g.yoffset as f32) * s;
        let raster = Raster {
            xmin: (g.xoffset as f32 * s).round() as i32,
            ymin: (top - h as f32).round() as i32,
            width: w,
            height: h,
            advance,
        };
        (raster, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "\
info face=\"Tiny\" size=8 bold=0 italic=0
common lineHeight=10 base=8 scaleW=16 scaleH=8 pages=1
page id=0 file=\"tiny_0.png\"
chars count=2
char id=65   x=0   y=0   width=4  height=6  xoffset=0  yoffset=2  xadvance=5  page=0  chnl=15
char id=66   x=4   y=0   width=4  height=6  xoffset=1  yoffset=2  xadvance=6  page=0  chnl=15
";

    const XML: &str = r#"<?xml version="1.0"?>
<font>
  <info face="Tiny" size="8"/>
  <common lineHeight="10" base="8" scaleW="16" scaleH="8" pages="1"/>
  <pages>
    <page id="0" file="tiny_0.png" />
  </pages>
  <chars count="2">
    <char id="65" x="0" y="0" width="4" height="6" xoffset="0" yoffset="2" xadvance="5" page="0" chnl="15" />
    <char id="66" x="4" y="0" width="4" height="6" xoffset="1" yoffset="2" xadvance="6" page="0" chnl="15" />
  </chars>
</font>"#;

    #[test]
    fn both_forms_of_a_description_read_the_same() {
        let a = parse_bmfont(TEXT).unwrap();
        let b = parse_bmfont(XML).unwrap();
        assert_eq!(a, b);
        assert_eq!((a.size, a.line_height, a.base), (8, 10, 8));
        assert_eq!(a.pages, vec!["tiny_0.png".to_string()]);
        assert_eq!(a.chars[&'B'], BmChar { x: 4, y: 0, w: 4, h: 6, xoffset: 1, yoffset: 2, xadvance: 6, page: 0 });
    }

    /// A 16×8 sheet: 'A' a solid 4×6 block at (0,0), 'B' one at (4,0).
    fn font() -> BitmapFont {
        let mut rgba = vec![0u8; 16 * 8 * 4];
        for y in 0..6 {
            for x in 0..8 {
                rgba[(y * 16 + x) * 4 + 3] = 255;
            }
        }
        BitmapFont::new(parse_bmfont(TEXT).unwrap(), vec![Some((rgba, 16, 8))])
    }

    #[test]
    fn a_glyph_is_its_sheet_at_its_size_and_twice_its_size() {
        let f = font();
        let (r, cov) = f.rasterize('A', 8.0);
        assert_eq!((r.width, r.height, r.advance), (4, 6, 5.0));
        // Top 2 below the line top, base 8: the glyph's bottom sits on the baseline.
        assert_eq!(r.ymin, 0);
        assert!(cov.iter().all(|&a| a == 255));
        let (r2, cov2) = f.rasterize('A', 16.0);
        assert_eq!((r2.width, r2.height, r2.advance), (8, 12, 10.0));
        assert!(cov2.iter().all(|&a| a == 255), "a whole multiple is a clean enlargement");
        assert_eq!(f.rasterize('B', 8.0).0.xmin, 1);
        assert_eq!(f.line(8.0), (8.0, -2.0));
    }

    #[test]
    fn a_letter_the_sheet_lacks_is_reported_missing() {
        let f = font();
        assert!(f.has_glyph('A') && !f.has_glyph('Z'));
        assert_eq!(f.rasterize('Z', 8.0).0.width, 0);
        assert!(f.advance(' ', 8.0) > 0.0, "a space still moves the pen");
    }

    #[test]
    fn something_that_is_not_a_bmfont_is_refused() {
        assert!(parse_bmfont("hello world").is_err());
    }
}
