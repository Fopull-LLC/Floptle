//! Colours as a theme file writes them, and the contrast check.
//!
//! A colour in a theme is a string, because it is a file people write by hand
//! and copy out of design tools:
//!
//! ```text
//! "#00c8a0"            hex, 3, 6 or 8 digits (the last two are alpha)
//! "rgba(0,200,160,.1)" the site's tokens.json spelling
//! "$accent"            another token of the same theme
//! "$accent/45%"        …at 45% of its opacity
//! "transparent"
//! ```
//!
//! A reference is what lets a theme change its accent in one place: the
//! built-ins write `accent_wash: "$accent/10%"`, so a theme extending one of
//! them gets a wash in its own accent without restating it.

/// A colour with **straight** (unpremultiplied) alpha, as written.
///
/// egui wants premultiplied; [`Rgba::to_egui`] converts at the last moment so
/// the file's numbers and the arithmetic here stay the numbers a person typed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba([0, 0, 0, 0]);

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba([r, g, b, 255])
    }

    pub fn to_egui(self) -> egui::Color32 {
        let [r, g, b, a] = self.0;
        egui::Color32::from_rgba_unmultiplied(r, g, b, a)
    }

    pub fn from_egui(c: egui::Color32) -> Rgba {
        Rgba(c.to_srgba_unmultiplied())
    }

    pub fn alpha(self) -> u8 {
        self.0[3]
    }

    pub fn is_opaque(self) -> bool {
        self.0[3] == 255
    }

    /// The same colour at `f` of its opacity.
    pub fn with_alpha_mul(self, f: f32) -> Rgba {
        let [r, g, b, a] = self.0;
        Rgba([r, g, b, (a as f32 * f.clamp(0.0, 1.0)).round() as u8])
    }

    /// This colour laid over `under` (which is taken as opaque), in sRGB
    /// space — the way egui blends, so the answer is what is on screen.
    pub fn over(self, under: Rgba) -> Rgba {
        let a = self.0[3] as f32 / 255.0;
        let mix = |t: u8, u: u8| (t as f32 * a + u as f32 * (1.0 - a)).round() as u8;
        Rgba([mix(self.0[0], under.0[0]), mix(self.0[1], under.0[1]), mix(self.0[2], under.0[2]), 255])
    }

    /// `#rrggbb`, or `#rrggbbaa` when not opaque. What a saved theme writes.
    pub fn to_hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 255 { format!("#{r:02x}{g:02x}{b:02x}") } else { format!("#{r:02x}{g:02x}{b:02x}{a:02x}") }
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f32 {
        let lin = |c: u8| {
            let c = c as f32 / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * lin(self.0[0]) + 0.7152 * lin(self.0[1]) + 0.0722 * lin(self.0[2])
    }

    /// The four floats a shader wants, 0–1, straight alpha.
    pub fn to_f32(self) -> [f32; 4] {
        self.0.map(|c| c as f32 / 255.0)
    }
}

/// WCAG contrast ratio between two opaque colours, 1.0 to 21.0.
pub fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (a.luminance(), b.luminance());
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// A colour before the theme's tokens are known: a literal, or a reference to
/// another token.
#[derive(Clone, Debug, PartialEq)]
pub enum ColorExpr {
    Lit(Rgba),
    Ref { token: String, alpha: f32 },
}

/// Parse one colour string. The error is a sentence fragment that names what
/// was wrong, for the caller to put after the key it came from.
pub fn parse(s: &str) -> Result<ColorExpr, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("transparent") || t.eq_ignore_ascii_case("none") {
        return Ok(ColorExpr::Lit(Rgba::TRANSPARENT));
    }
    if let Some(r) = t.strip_prefix('$') {
        let (name, alpha) = match r.split_once('/') {
            Some((n, a)) => {
                let a = a.trim();
                let v = if let Some(p) = a.strip_suffix('%') {
                    p.trim().parse::<f32>().map(|p| p / 100.0)
                } else {
                    a.parse::<f32>()
                }
                .map_err(|_| format!("{s:?}: the opacity after '/' must be a number like 45% or 0.45"))?;
                (n.trim(), v)
            }
            None => (r.trim(), 1.0),
        };
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(format!("{s:?}: '$' must be followed by a token name such as $accent"));
        }
        return Ok(ColorExpr::Ref { token: name.to_string(), alpha: alpha.clamp(0.0, 1.0) });
    }
    if let Some(h) = t.strip_prefix('#') {
        let hex = |s: &str| u8::from_str_radix(s, 16);
        let bad = || format!("{s:?} is not a colour: write #rgb, #rrggbb or #rrggbbaa");
        if !h.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let c = match h.len() {
            3 => {
                let d = |i: usize| hex(&h[i..i + 1]).map(|v| v * 17);
                [d(0), d(1), d(2), Ok(255)]
            }
            6 => [hex(&h[0..2]), hex(&h[2..4]), hex(&h[4..6]), Ok(255)],
            8 => [hex(&h[0..2]), hex(&h[2..4]), hex(&h[4..6]), hex(&h[6..8])],
            _ => return Err(bad()),
        };
        let mut out = [0u8; 4];
        for (o, v) in out.iter_mut().zip(c) {
            *o = v.map_err(|_| bad())?;
        }
        return Ok(ColorExpr::Lit(Rgba(out)));
    }
    let lower = t.to_ascii_lowercase();
    if let Some(inner) = lower.strip_prefix("rgba(").or_else(|| lower.strip_prefix("rgb(")) {
        let inner = inner
            .strip_suffix(')')
            .ok_or_else(|| format!("{s:?}: rgba( is missing its closing bracket"))?;
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() != 3 && parts.len() != 4 {
            return Err(format!("{s:?}: rgba() takes three channels and an optional alpha"));
        }
        let mut out = [255u8; 4];
        for (i, p) in parts.iter().enumerate().take(3) {
            out[i] = p
                .parse::<f32>()
                .map(|v| v.clamp(0.0, 255.0).round() as u8)
                .map_err(|_| format!("{s:?}: {p:?} is not a channel value from 0 to 255"))?;
        }
        if let Some(a) = parts.get(3) {
            let a = a
                .parse::<f32>()
                .map_err(|_| format!("{s:?}: {a:?} is not an alpha from 0 to 1"))?;
            out[3] = (a.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        return Ok(ColorExpr::Lit(Rgba(out)));
    }
    Err(format!("{s:?} is not a colour: write #rrggbb, rgba(r, g, b, a) or $token"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spellings_a_theme_uses_all_parse() {
        assert_eq!(parse("#00c8a0"), Ok(ColorExpr::Lit(Rgba([0, 200, 160, 255]))));
        assert_eq!(parse("#fff"), Ok(ColorExpr::Lit(Rgba([255, 255, 255, 255]))));
        assert_eq!(parse("#00c8a080"), Ok(ColorExpr::Lit(Rgba([0, 200, 160, 128]))));
        assert_eq!(parse("rgba(0, 200, 160, 0.10)"), Ok(ColorExpr::Lit(Rgba([0, 200, 160, 26]))));
        assert_eq!(parse("transparent"), Ok(ColorExpr::Lit(Rgba::TRANSPARENT)));
        assert_eq!(parse("$accent/45%"), Ok(ColorExpr::Ref { token: "accent".into(), alpha: 0.45 }));
        assert_eq!(parse("$text"), Ok(ColorExpr::Ref { token: "text".into(), alpha: 1.0 }));
    }

    #[test]
    fn a_wrong_colour_says_what_to_write_instead() {
        for bad in ["#12345", "#gggggg", "teal", "rgba(1,2)", "$", "$accent/lots"] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(bad.trim_start_matches('$')) || e.contains("token"), "{bad}: {e}");
        }
    }

    /// The brand pair passes, and a grey-on-grey pair does not. Values that
    /// differ on purpose: a check that answered the same for both would be
    /// asserting nothing.
    #[test]
    fn contrast_tells_a_readable_pair_from_an_unreadable_one() {
        let ground = Rgba::rgb(0x14, 0x15, 0x17);
        assert!(contrast(Rgba::rgb(0xe8, 0xe8, 0xea), ground) > 14.0);
        assert!(contrast(Rgba::rgb(0x40, 0x40, 0x44), ground) < 4.5);
        assert!((contrast(Rgba::rgb(0, 0, 0), Rgba::rgb(255, 255, 255)) - 21.0).abs() < 0.01);
    }
}
