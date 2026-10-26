//! Pictures a script draws: an emblem, a decal, a minimap, an icon.
//!
//! ```lua
//! local img = image.new(256, 256)                  -- transparent
//! img:fill(0, 0, 256, 256, 0.1, 0.2, 0.4)          -- set a rectangle
//! img:circle(128, 128, 80, 1, 0.8, 0.2)            -- smooth-edged, laid over
//! img:text(128, 100, "SOL", 48, 1, 1, 1, 1, "center")
//! local tex = img:texture()                        -- "img:N", drawable next frame
//! img:circle(128, 128, 40, 0, 0, 0); img:texture() -- the same name, new pixels
//! ```
//!
//! Every operation runs in the engine, so a picture costs what its pixels
//! cost, not a Lua call per pixel: a 256² emblem of a ring, a disc, a star
//! and a word takes about a millisecond, where a Lua loop over its pixels
//! took 150–200 ms on Luau.
//!
//! Colours are 0..1 per channel with alpha 1 when left out, as `draw.*` takes
//! them. Positions are pixels from the top-left corner. Pixels are stored as
//! plain (not premultiplied) RGBA8, which is what `img:bytes()` hands back and
//! what `assets.textureFromPixels` takes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Lua, MultiValue, Table, UserData, UserDataFields, UserDataMethods, Value};

use crate::texture_api::{MAX_PIXEL_SIDE, TextureLoads};

/// The engine's own UI font: what `img:text` draws with when no font is named.
static DEFAULT_FONT: std::sync::OnceLock<fontdue::Font> = std::sync::OnceLock::new();

fn default_font() -> &'static fontdue::Font {
    DEFAULT_FONT.get_or_init(|| {
        fontdue::Font::from_bytes(floptle_ui::DEFAULT_FONT, fontdue::FontSettings::default())
            .expect("the bundled UI font parses")
    })
}

/// A picture in memory.
pub struct Image {
    w: u32,
    h: u32,
    px: Vec<u8>,
    /// The texture `img:texture()` made, re-uploaded in place by later calls.
    tex: Option<String>,
}

/// A plain colour, 0..1 per channel.
#[derive(Clone, Copy)]
struct Rgba([f32; 4]);

impl Image {
    fn new(w: u32, h: u32) -> Self {
        Self { w, h, px: vec![0; (w * h * 4) as usize], tex: None }
    }

    /// Lay `c` over pixel `(x, y)` at `cover` (0..1) of its strength: the
    /// usual "over", in plain alpha.
    #[inline]
    fn over(&mut self, x: u32, y: u32, c: Rgba, cover: f32) {
        let sa = c.0[3] * cover;
        if sa <= 0.0 {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        let d = &mut self.px[i..i + 4];
        let da = d[3] as f32 / 255.0;
        let keep = da * (1.0 - sa);
        let oa = sa + keep;
        if oa <= 0.0 {
            return;
        }
        for (dk, ck) in d.iter_mut().zip(c.0).take(3) {
            let v = (ck * sa + *dk as f32 / 255.0 * keep) / oa;
            *dk = (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
        d[3] = (oa.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    }

    #[inline]
    fn set(&mut self, x: u32, y: u32, c: Rgba) {
        let i = ((y * self.w + x) * 4) as usize;
        for k in 0..4 {
            self.px[i + k] = (c.0[k].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        }
    }

    /// The whole-pixel span `[a, b)` a rectangle edge pair covers, clipped.
    fn span(a: f64, len: f64, max: u32) -> (u32, u32) {
        let lo = a.round().clamp(0.0, max as f64) as u32;
        let hi = (a + len).round().clamp(0.0, max as f64) as u32;
        (lo, hi.max(lo))
    }

    fn rect(&mut self, x: f64, y: f64, w: f64, h: f64, c: Rgba, blend: bool) {
        let (x0, x1) = Self::span(x, w, self.w);
        let (y0, y1) = Self::span(y, h, self.h);
        for py in y0..y1 {
            for px in x0..x1 {
                if blend { self.over(px, py, c, 1.0) } else { self.set(px, py, c) }
            }
        }
    }

    /// Lay `c` over every pixel whose centre is near the shape, `cover` its
    /// signed reach (pixels inside the edge, +0.5 for a one-pixel smooth edge).
    fn shade(&mut self, bounds: [f64; 4], c: Rgba, cover: impl Fn(f64, f64) -> f64) {
        let x0 = bounds[0].floor().max(0.0) as u32;
        let y0 = bounds[1].floor().max(0.0) as u32;
        let x1 = (bounds[2].ceil().max(0.0) as u32).min(self.w);
        let y1 = (bounds[3].ceil().max(0.0) as u32).min(self.h);
        for py in y0..y1 {
            for px in x0..x1 {
                let k = cover(px as f64 + 0.5, py as f64 + 0.5).clamp(0.0, 1.0);
                if k > 0.0 {
                    self.over(px, py, c, k as f32);
                }
            }
        }
    }

    /// A filled polygon, even-odd, with smooth edges: four sample rows per
    /// pixel, each covering exactly the length of its spans along the row.
    fn polygon(&mut self, pts: &[[f64; 2]], c: Rgba) {
        if pts.len() < 3 {
            return;
        }
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for p in pts {
            for k in 0..2 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        let x0 = lo[0].floor().max(0.0) as u32;
        let x1 = (hi[0].ceil().max(0.0) as u32).min(self.w);
        let y0 = lo[1].floor().max(0.0) as u32;
        let y1 = (hi[1].ceil().max(0.0) as u32).min(self.h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        const ROWS: usize = 4;
        let mut cover = vec![0.0f32; (x1 - x0) as usize];
        let mut xs: Vec<f64> = Vec::new();
        for py in y0..y1 {
            cover.iter_mut().for_each(|v| *v = 0.0);
            for r in 0..ROWS {
                let sy = py as f64 + (r as f64 + 0.5) / ROWS as f64;
                xs.clear();
                for i in 0..pts.len() {
                    let (a, b) = (pts[i], pts[(i + 1) % pts.len()]);
                    if (a[1] <= sy) != (b[1] <= sy) {
                        xs.push(a[0] + (sy - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
                    }
                }
                xs.sort_by(|a, b| a.total_cmp(b));
                for pair in xs.as_chunks::<2>().0 {
                    let (sa, sb) = (pair[0].max(x0 as f64), pair[1].min(x1 as f64));
                    if sb <= sa {
                        continue;
                    }
                    let mut px = sa.floor() as u32;
                    while (px as f64) < sb && px < x1 {
                        let overlap = (sb.min(px as f64 + 1.0) - sa.max(px as f64)).max(0.0);
                        cover[(px - x0) as usize] += overlap as f32 / ROWS as f32;
                        px += 1;
                    }
                }
            }
            for (i, &k) in cover.iter().enumerate() {
                if k > 0.0 {
                    self.over(x0 + i as u32, py, c, k.min(1.0));
                }
            }
        }
    }

    /// The colour at a point between pixel centres, from the four around it.
    fn sample(&self, u: f64, v: f64) -> [f32; 4] {
        let fx = (u - 0.5).clamp(0.0, (self.w - 1) as f64);
        let fy = (v - 0.5).clamp(0.0, (self.h - 1) as f64);
        let (ix, iy) = (fx.floor() as u32, fy.floor() as u32);
        let (tx, ty) = ((fx - ix as f64) as f32, (fy - iy as f64) as f32);
        let (jx, jy) = ((ix + 1).min(self.w - 1), (iy + 1).min(self.h - 1));
        let at = |x: u32, y: u32| {
            let i = ((y * self.w + x) * 4) as usize;
            let a = self.px[i + 3] as f32 / 255.0;
            // Premultiplied while mixing, so a transparent neighbour brings
            // no colour of its own into the edge.
            [self.px[i] as f32 / 255.0 * a, self.px[i + 1] as f32 / 255.0 * a, self.px[i + 2] as f32 / 255.0 * a, a]
        };
        let (p00, p10, p01, p11) = (at(ix, iy), at(jx, iy), at(ix, jy), at(jx, jy));
        let mut out = [0.0; 4];
        for k in 0..4 {
            let top = p00[k] + (p10[k] - p00[k]) * tx;
            let bot = p01[k] + (p11[k] - p01[k]) * tx;
            out[k] = top + (bot - top) * ty;
        }
        if out[3] > 0.0 {
            for k in 0..3 {
                out[k] /= out[3];
            }
        }
        out
    }

    /// Lay `src` over the rectangle `(x, y, w, h)`, scaled to fit.
    fn draw(&mut self, src: &Image, x: f64, y: f64, w: f64, h: f64, alpha: f32) {
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let (x0, x1) = Self::span(x, w, self.w);
        let (y0, y1) = Self::span(y, h, self.h);
        let (sx, sy) = (src.w as f64 / w, src.h as f64 / h);
        for py in y0..y1 {
            for px in x0..x1 {
                let c = src.sample((px as f64 + 0.5 - x) * sx, (py as f64 + 0.5 - y) * sy);
                self.over(px, py, Rgba(c), alpha);
            }
        }
    }

    /// Text with its top at `y` and its `align` edge at `x`.
    #[allow(clippy::too_many_arguments)] // `img:text`'s own argument list, one to one
    fn text(&mut self, font: &fontdue::Font, x: f64, y: f64, s: &str, size: f32, c: Rgba, align: u8) {
        let ascent = font.horizontal_line_metrics(size).map_or(size * 0.8, |m| m.ascent);
        let mut width = 0.0f32;
        let mut prev: Option<char> = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                width += font.horizontal_kern(p, ch, size).unwrap_or(0.0);
            }
            width += font.metrics(ch, size).advance_width;
            prev = Some(ch);
        }
        let mut pen = x as f32
            - match align {
                1 => width * 0.5,
                2 => width,
                _ => 0.0,
            };
        let base = y as f32 + ascent;
        prev = None;
        for ch in s.chars() {
            if let Some(p) = prev {
                pen += font.horizontal_kern(p, ch, size).unwrap_or(0.0);
            }
            let (m, bitmap) = font.rasterize(ch, size);
            let gx = pen.round() as i64 + m.xmin as i64;
            let gy = (base - m.height as f32 - m.ymin as f32).round() as i64;
            for row in 0..m.height {
                for col in 0..m.width {
                    let k = bitmap[row * m.width + col];
                    let (tx, ty) = (gx + col as i64, gy + row as i64);
                    if k > 0 && tx >= 0 && ty >= 0 && (tx as u32) < self.w && (ty as u32) < self.h {
                        self.over(tx as u32, ty as u32, c, k as f32 / 255.0);
                    }
                }
            }
            pen += m.advance_width;
            prev = Some(ch);
        }
    }

    /// A PNG of the picture: 8-bit RGBA, one stored zlib stream.
    fn png(&self) -> Vec<u8> {
        use std::io::Write;
        let mut raw = Vec::with_capacity(((self.w * 4 + 1) * self.h) as usize);
        for row in self.px.chunks_exact((self.w * 4) as usize) {
            raw.push(0);
            raw.extend_from_slice(row);
        }
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        let _ = z.write_all(&raw);
        let data = z.finish().unwrap_or_default();
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8; 4], body: &[u8]| {
            out.extend_from_slice(&(body.len() as u32).to_be_bytes());
            let mut crc = flate2::Crc::new();
            crc.update(kind);
            crc.update(body);
            out.extend_from_slice(kind);
            out.extend_from_slice(body);
            out.extend_from_slice(&crc.sum().to_be_bytes());
        };
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&self.w.to_be_bytes());
        ihdr.extend_from_slice(&self.h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        chunk(b"IHDR", &ihdr);
        chunk(b"IDAT", &data);
        chunk(b"IEND", &[]);
        out
    }
}

/// The colour at `args[from..]`: r, g, b and an optional a, 0..1.
fn colour(call: &str, args: &[Value], from: usize) -> mlua::Result<Rgba> {
    let mut c = [0.0f32, 0.0, 0.0, 1.0];
    for (k, slot) in c.iter_mut().enumerate() {
        match args.get(from + k) {
            Some(Value::Number(n)) => *slot = *n as f32,
            Some(Value::Integer(n)) => *slot = *n as f32,
            None | Some(Value::Nil) if k == 3 => {}
            other => {
                return Err(mlua::Error::runtime(format!(
                    "{call}: the colour is r, g, b [, a], each 0 to 1; argument {} is {}",
                    from + k + 1,
                    other.map_or("missing", |v| v.type_name())
                )));
            }
        }
    }
    Ok(Rgba(c))
}

fn num(call: &str, args: &[Value], i: usize, what: &str) -> mlua::Result<f64> {
    match args.get(i) {
        Some(Value::Number(n)) => Ok(*n),
        Some(Value::Integer(n)) => Ok(*n as f64),
        other => Err(mlua::Error::runtime(format!(
            "{call}: {what} (argument {}) is a number, not {}",
            i + 1,
            other.map_or("nothing", |v| v.type_name())
        ))),
    }
}

fn side(call: &str, v: f64, what: &str) -> mlua::Result<u32> {
    if v.fract() != 0.0 || v < 1.0 || v > MAX_PIXEL_SIDE as f64 {
        return Err(mlua::Error::runtime(format!(
            "{call}: the {what} is {v}; it is a whole number of pixels from 1 to {MAX_PIXEL_SIDE}"
        )));
    }
    Ok(v as u32)
}

/// Reads a project file by the path a script gave.
pub(crate) type ReadFile = Rc<dyn Fn(&str) -> Result<Vec<u8>, String>>;

/// How `image.*` reaches the rest of the engine.
pub(crate) struct ImageHost {
    pub(crate) textures: Rc<RefCell<TextureLoads>>,
    /// Reads a project file for `img:text`'s font.
    pub(crate) read: ReadFile,
    fonts: RefCell<HashMap<String, Rc<fontdue::Font>>>,
}

impl ImageHost {
    pub(crate) fn new(textures: Rc<RefCell<TextureLoads>>, read: ReadFile) -> Self {
        Self { textures, read, fonts: RefCell::default() }
    }

    fn font(&self, path: &str) -> mlua::Result<Rc<fontdue::Font>> {
        if let Some(f) = self.fonts.borrow().get(path) {
            return Ok(f.clone());
        }
        let lower = path.to_ascii_lowercase();
        if !(lower.ends_with(".ttf") || lower.ends_with(".otf")) {
            return Err(mlua::Error::runtime(format!(
                "img:text: the font \"{path}\" is not a .ttf or .otf; those are the fonts a picture draws with"
            )));
        }
        let bytes = (self.read)(path).map_err(mlua::Error::runtime)?;
        let font = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
            .map_err(|e| mlua::Error::runtime(format!("img:text: the font \"{path}\" does not load: {e}")))?;
        let font = Rc::new(font);
        self.fonts.borrow_mut().insert(path.to_string(), font.clone());
        Ok(font)
    }
}

/// A handle to an [`Image`], shared so `img:draw(other)` can read one while
/// writing another.
#[derive(Clone)]
struct LuaImage(Rc<RefCell<Image>>, Rc<ImageHost>);

impl UserData for LuaImage {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("width", |_, this| Ok(this.0.borrow().w));
        fields.add_field_method_get("height", |_, this| Ok(this.0.borrow().h));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("clear", |_, this, args: MultiValue| {
            let args: Vec<Value> = args.into_iter().collect();
            let c = if args.is_empty() { Rgba([0.0; 4]) } else { colour("img:clear", &args, 0)? };
            let mut img = this.0.borrow_mut();
            let (w, h) = (img.w as f64, img.h as f64);
            img.rect(0.0, 0.0, w, h, c, false);
            Ok(())
        });
        for (name, blend) in [("fill", false), ("blend", true)] {
            methods.add_method(name, move |_, this, args: MultiValue| {
                let call = if blend { "img:blend" } else { "img:fill" };
                let a: Vec<Value> = args.into_iter().collect();
                let (x, y) = (num(call, &a, 0, "x")?, num(call, &a, 1, "y")?);
                let (w, h) = (num(call, &a, 2, "the width")?, num(call, &a, 3, "the height")?);
                let c = colour(call, &a, 4)?;
                this.0.borrow_mut().rect(x, y, w, h, c, blend);
                Ok(())
            });
        }
        methods.add_method("circle", |_, this, args: MultiValue| {
            const CALL: &str = "img:circle";
            let a: Vec<Value> = args.into_iter().collect();
            let (cx, cy, r) = (num(CALL, &a, 0, "x")?, num(CALL, &a, 1, "y")?, num(CALL, &a, 2, "the radius")?);
            let c = colour(CALL, &a, 3)?;
            this.0.borrow_mut().shade([cx - r - 1.0, cy - r - 1.0, cx + r + 1.0, cy + r + 1.0], c, |x, y| {
                r - ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() + 0.5
            });
            Ok(())
        });
        methods.add_method("ring", |_, this, args: MultiValue| {
            const CALL: &str = "img:ring";
            let a: Vec<Value> = args.into_iter().collect();
            let (cx, cy, r) = (num(CALL, &a, 0, "x")?, num(CALL, &a, 1, "y")?, num(CALL, &a, 2, "the radius")?);
            let half = num(CALL, &a, 3, "the width")? * 0.5;
            let c = colour(CALL, &a, 4)?;
            let reach = r + half + 1.0;
            this.0.borrow_mut().shade([cx - reach, cy - reach, cx + reach, cy + reach], c, |x, y| {
                half - (((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r).abs() + 0.5
            });
            Ok(())
        });
        methods.add_method("line", |_, this, args: MultiValue| {
            const CALL: &str = "img:line";
            let a: Vec<Value> = args.into_iter().collect();
            let (x1, y1) = (num(CALL, &a, 0, "x1")?, num(CALL, &a, 1, "y1")?);
            let (x2, y2) = (num(CALL, &a, 2, "x2")?, num(CALL, &a, 3, "y2")?);
            let half = num(CALL, &a, 4, "the width")? * 0.5;
            let c = colour(CALL, &a, 5)?;
            let (dx, dy) = (x2 - x1, y2 - y1);
            let len2 = (dx * dx + dy * dy).max(1e-12);
            let pad = half + 1.0;
            let bounds = [x1.min(x2) - pad, y1.min(y2) - pad, x1.max(x2) + pad, y1.max(y2) + pad];
            this.0.borrow_mut().shade(bounds, c, |x, y| {
                let t = (((x - x1) * dx + (y - y1) * dy) / len2).clamp(0.0, 1.0);
                half - ((x - x1 - t * dx).powi(2) + (y - y1 - t * dy).powi(2)).sqrt() + 0.5
            });
            Ok(())
        });
        methods.add_method("polygon", |_, this, args: MultiValue| {
            const CALL: &str = "img:polygon";
            let a: Vec<Value> = args.into_iter().collect();
            let Some(Value::Table(t)) = a.first() else {
                return Err(mlua::Error::runtime(format!(
                    "{CALL} takes the corners as a list {{ x1, y1, x2, y2, ... }}, then r, g, b [, a]"
                )));
            };
            let flat: Vec<f64> = (1..=t.raw_len()).map(|i| t.raw_get::<f64>(i)).collect::<mlua::Result<_>>()?;
            if !flat.len().is_multiple_of(2) || flat.len() < 6 {
                return Err(mlua::Error::runtime(format!(
                    "{CALL}: {} numbers is not a list of at least three x, y corners",
                    flat.len()
                )));
            }
            let pts: Vec<[f64; 2]> = flat.as_chunks::<2>().0.to_vec();
            let c = colour(CALL, &a, 1)?;
            this.0.borrow_mut().polygon(&pts, c);
            Ok(())
        });
        methods.add_method("draw", |_, this, args: MultiValue| {
            const CALL: &str = "img:draw";
            let a: Vec<Value> = args.into_iter().collect();
            let src = match a.first() {
                Some(Value::UserData(u)) => u.borrow::<LuaImage>().map(|s| s.0.clone()).ok(),
                _ => None,
            };
            let Some(src) = src else {
                return Err(mlua::Error::runtime(format!(
                    "{CALL} takes another image first: img:draw(other, x, y [, w, h [, alpha]])"
                )));
            };
            let (x, y) = (num(CALL, &a, 1, "x")?, num(CALL, &a, 2, "y")?);
            // Drawing an image into itself reads the picture as it was.
            let own_copy;
            let guard;
            let s: &Image = if Rc::ptr_eq(&src, &this.0) {
                let b = src.borrow();
                own_copy = Image { w: b.w, h: b.h, px: b.px.clone(), tex: None };
                &own_copy
            } else {
                guard = src.borrow();
                &guard
            };
            let w = if a.len() > 3 { num(CALL, &a, 3, "the width")? } else { s.w as f64 };
            let h = if a.len() > 4 { num(CALL, &a, 4, "the height")? } else { s.h as f64 };
            let alpha = if a.len() > 5 { num(CALL, &a, 5, "alpha")? as f32 } else { 1.0 };
            this.0.borrow_mut().draw(s, x, y, w, h, alpha);
            Ok(())
        });
        methods.add_method("text", |_, this, args: MultiValue| {
            const CALL: &str = "img:text";
            let a: Vec<Value> = args.into_iter().collect();
            let (x, y) = (num(CALL, &a, 0, "x")?, num(CALL, &a, 1, "y")?);
            let s = match a.get(2) {
                Some(Value::String(s)) => s.to_str()?.to_string(),
                Some(Value::Number(n)) => n.to_string(),
                Some(Value::Integer(n)) => n.to_string(),
                _ => return Err(mlua::Error::runtime(format!("{CALL}: the text (argument 3) is a string"))),
            };
            let size = num(CALL, &a, 3, "the size")? as f32;
            let c = colour(CALL, &a, 4)?;
            let mut align = 0;
            let mut font_path = None;
            for v in a.iter().skip(8) {
                if let Value::String(t) = v {
                    match t.to_str()?.as_ref() {
                        "left" => align = 0,
                        "center" | "centre" => align = 1,
                        "right" => align = 2,
                        other => font_path = Some(other.to_string()),
                    }
                }
            }
            let custom = match &font_path {
                Some(p) => Some(this.1.font(p)?),
                None => None,
            };
            let font = custom.as_deref().unwrap_or_else(|| default_font());
            this.0.borrow_mut().text(font, x, y, &s, size, c, align);
            Ok(())
        });
        methods.add_method("getPixel", |_, this, (x, y): (f64, f64)| {
            let img = this.0.borrow();
            if x < 0.0 || y < 0.0 || x >= img.w as f64 || y >= img.h as f64 {
                return Ok((None, None, None, None));
            }
            let i = ((y as u32 * img.w + x as u32) * 4) as usize;
            let f = |k: usize| Some(img.px[i + k] as f64 / 255.0);
            Ok((f(0), f(1), f(2), f(3)))
        });
        methods.add_method("setPixel", |_, this, args: MultiValue| {
            const CALL: &str = "img:setPixel";
            let a: Vec<Value> = args.into_iter().collect();
            let (x, y) = (num(CALL, &a, 0, "x")?, num(CALL, &a, 1, "y")?);
            let c = colour(CALL, &a, 2)?;
            let mut img = this.0.borrow_mut();
            if x >= 0.0 && y >= 0.0 && x < img.w as f64 && y < img.h as f64 {
                img.set(x as u32, y as u32, c);
            }
            Ok(())
        });
        methods.add_method("copy", |_, this, ()| {
            let img = this.0.borrow();
            Ok(LuaImage(
                Rc::new(RefCell::new(Image { w: img.w, h: img.h, px: img.px.clone(), tex: None })),
                this.1.clone(),
            ))
        });
        methods.add_method("bytes", |lua, this, ()| lua.create_string(&this.0.borrow().px));
        methods.add_method("png", |lua, this, ()| lua.create_string(this.0.borrow().png()));
        methods.add_method("texture", |lua, this, ()| {
            let mut img = this.0.borrow_mut();
            let mut book = this.1.textures.borrow_mut();
            let (w, h) = (img.w, img.h);
            match book.upload_pixels(img.tex.as_deref(), w, h, img.px.clone()) {
                Some(name) => {
                    img.tex = Some(name.clone());
                    Ok((Value::String(lua.create_string(&name)?), Value::Nil))
                }
                None => Ok((Value::Nil, Value::String(lua.create_string(crate::texture_api::NO_GPU)?))),
            }
        });
    }
}

pub(crate) fn install(lua: &Lua, host: ImageHost) -> mlua::Result<()> {
    // The font takes about 13 ms to parse: on a worker now, not in the frame
    // that first draws text. A browser has no worker to give it, and parses
    // on first use.
    #[cfg(not(target_arch = "wasm32"))]
    if DEFAULT_FONT.get().is_none() {
        let _ = std::thread::Builder::new().name("floptle-image-font".into()).spawn(|| {
            default_font();
        });
    }
    let host = Rc::new(host);
    let image: Table = lua.create_table()?;
    let h = host.clone();
    image.set(
        "new",
        lua.create_function(move |_, args: MultiValue| {
            const CALL: &str = "image.new";
            let a: Vec<Value> = args.into_iter().collect();
            let w = side(CALL, num(CALL, &a, 0, "the width")?, "width")?;
            let ht = side(CALL, num(CALL, &a, 1, "the height")?, "height")?;
            let mut img = Image::new(w, ht);
            if a.len() > 2 {
                let c = colour(CALL, &a, 2)?;
                img.rect(0.0, 0.0, w as f64, ht as f64, c, false);
            }
            Ok(LuaImage(Rc::new(RefCell::new(img)), h.clone()))
        })?,
    )?;
    let h = host;
    image.set(
        "fromPixels",
        lua.create_function(move |_, (w, ht, bytes): (f64, f64, mlua::String)| {
            const CALL: &str = "image.fromPixels";
            let (w, ht) = (side(CALL, w, "width")?, side(CALL, ht, "height")?);
            let px = bytes.as_bytes().to_vec();
            if px.len() != (w * ht * 4) as usize {
                return Err(mlua::Error::runtime(format!(
                    "{CALL}: {w}x{ht} takes {} bytes (r, g, b, a per pixel), and {} were given",
                    w * ht * 4,
                    px.len()
                )));
            }
            Ok(LuaImage(Rc::new(RefCell::new(Image { w, h: ht, px, tex: None })), h.clone()))
        })?,
    )?;
    lua.globals().set("image", image)?;
    Ok(())
}

/// An image handle's pixels.
#[cfg(test)]
pub(crate) fn pixels_of(v: &mlua::AnyUserData) -> Option<(u32, u32, Vec<u8>)> {
    v.borrow::<LuaImage>().ok().map(|i| {
        let img = i.0.borrow();
        (img.w, img.h, img.px.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mlua::AnyUserData;

    fn lua() -> (Lua, Rc<RefCell<TextureLoads>>) {
        let lua = Lua::new();
        let book: Rc<RefCell<TextureLoads>> = Rc::default();
        let read: ReadFile = Rc::new(|p: &str| Err(format!("no file {p}")));
        install(&lua, ImageHost::new(book.clone(), read)).unwrap();
        (lua, book)
    }

    fn px(lua: &Lua, x: u32, y: u32) -> [u8; 4] {
        let ud: AnyUserData = lua.globals().get("img").unwrap();
        let (w, _, p) = pixels_of(&ud).unwrap();
        let i = ((y * w + x) * 4) as usize;
        [p[i], p[i + 1], p[i + 2], p[i + 3]]
    }

    /// Each operation lands where it says, with the colour it says: a fill
    /// sets, a blend lays over, a circle and a polygon have hard insides and
    /// smooth edges, a line reaches its ends, and text marks pixels.
    #[test]
    fn a_picture_draws_what_each_call_says() {
        let (lua, _) = lua();
        lua.load(
            "img = image.new(64, 64)\n\
             img:fill(0, 0, 32, 64, 1, 0, 0)\n\
             img:blend(16, 0, 32, 8, 0, 0, 1, 0.5)\n\
             img:circle(48, 48, 10, 0, 1, 0)\n\
             img:polygon({ 2, 40, 20, 40, 2, 60 }, 1, 1, 1)\n\
             img:line(40, 20, 60, 20, 3, 1, 1, 0)\n",
        )
        .exec()
        .unwrap();
        assert_eq!(px(&lua, 5, 20), [255, 0, 0, 255], "fill");
        assert_eq!(px(&lua, 20, 4), [128, 0, 128, 255], "half blue over red");
        assert_eq!(px(&lua, 40, 4), [0, 0, 255, 128], "half blue over nothing keeps its colour");
        assert_eq!(px(&lua, 48, 48), [0, 255, 0, 255], "circle centre");
        let edge = px(&lua, 55, 54)[3];
        assert!(edge > 40 && edge < 220, "the circle's edge is smooth: alpha {edge}");
        assert_eq!(px(&lua, 48, 30)[3], 0, "outside the circle");
        assert_eq!(px(&lua, 4, 42), [255, 255, 255, 255], "polygon inside");
        assert_eq!(px(&lua, 18, 58), [255, 0, 0, 255], "outside the polygon's slanted side");
        assert_eq!(px(&lua, 41, 20), [255, 255, 0, 255], "line start");
        assert_eq!(px(&lua, 59, 20), [255, 255, 0, 255], "line end");
        lua.load("img = image.new(64, 32); img:text(32, 4, 'HI', 24, 1, 1, 1, 1, 'center')").exec().unwrap();
        let ud: AnyUserData = lua.globals().get("img").unwrap();
        let (_, _, p) = pixels_of(&ud).unwrap();
        let lit: Vec<u32> = (0..64 * 32u32).filter(|i| p[*i as usize * 4 + 3] > 128).collect();
        assert!(lit.len() > 40, "the text marked {} pixels", lit.len());
        let mean_x = lit.iter().map(|i| (i % 64) as f64).sum::<f64>() / lit.len() as f64;
        assert!((mean_x - 32.0).abs() < 4.0, "centred text sits about x = 32, not {mean_x:.1}");
    }

    /// One image drawn into another, scaled, and a picture's PNG decodes back
    /// to the same pixels.
    #[test]
    fn a_picture_composes_and_saves() {
        let (lua, _) = lua();
        lua.load(
            "local dot = image.new(2, 2, 0, 0, 1)\n\
             img = image.new(8, 8)\n\
             img:draw(dot, 4, 4, 4, 4)\n\
             png = img:png()",
        )
        .exec()
        .unwrap();
        assert_eq!(px(&lua, 6, 6), [0, 0, 255, 255], "scaled into its rectangle");
        assert_eq!(px(&lua, 2, 2)[3], 0, "nothing outside it");
        let png: mlua::String = lua.globals().get("png").unwrap();
        let back = image::load_from_memory_with_format(&png.as_bytes(), image::ImageFormat::Png).unwrap().to_rgba8();
        let ud: AnyUserData = lua.globals().get("img").unwrap();
        assert_eq!(back.into_raw(), pixels_of(&ud).unwrap().2);
    }

    /// `img:texture()` names a texture once and re-uploads into that name.
    #[test]
    fn a_texture_keeps_its_name_across_uploads() {
        let (lua, book) = lua();
        lua.load("img = image.new(4, 4, 1, 1, 1); a = img:texture(); img:fill(0, 0, 1, 1, 0, 0, 0); b = img:texture()")
            .exec()
            .unwrap();
        let (a, b): (String, String) = (lua.globals().get("a").unwrap(), lua.globals().get("b").unwrap());
        assert!(a.starts_with("img:") && a == b, "{a} then {b}");
        let reqs = book.borrow_mut().take_requests();
        assert_eq!(reqs.len(), 2);
        assert!(reqs.iter().all(|r| r.name == a && r.pixels == Some([4, 4])));
        assert_eq!(&reqs[1].bytes[..4], &[0, 0, 0, 255], "the second upload carries the new pixels");
    }

    /// The point of the API: a 256² emblem in a few calls costs well under
    /// the 5 ms a frame can spare, where the per-pixel Lua loop took 150 ms.
    #[test]
    fn a_256_emblem_is_cheap() {
        let (lua, _) = lua();
        // Parsing the font is once a run, on a worker from the start: not
        // part of what a picture costs.
        default_font();
        let t = std::time::Instant::now();
        lua.load(
            "img = image.new(256, 256, 0.1, 0.15, 0.3)\n\
             img:ring(128, 128, 110, 10, 0.9, 0.8, 0.3)\n\
             img:circle(128, 128, 90, 0.2, 0.3, 0.6)\n\
             img:polygon({ 128, 50, 150, 110, 210, 110, 160, 145, 180, 205, 128, 170, 76, 205, 96, 145, 46, 110, 106, 110 }, 1, 0.9, 0.4)\n\
             img:text(128, 210, 'SOLAR', 28, 1, 1, 1, 1, 'center')",
        )
        .exec()
        .unwrap();
        let ms = t.elapsed().as_secs_f64() * 1e3;
        eprintln!("a 256² emblem: {ms:.2} ms");
        // About 1 ms in a release build; generous for an unoptimised test
        // build on a busy machine.
        assert!(ms < 30.0, "a 256² emblem took {ms:.1} ms");
    }
}
