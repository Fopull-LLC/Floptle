//! Palettes — the thing pixel art lives or dies on, and the thing nothing in the
//! engine understood before (proposal P7).
//!
//! `.gpl` (GIMP/Aseprite/Lospec) and `.hex` (Lospec's plain list) are the two
//! interchange formats that matter; both are a few lines to parse and mean every
//! retro palette on the internet drops straight in.

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Palette {
    pub name: String,
    pub colors: Vec<[u8; 4]>,
}

impl Palette {
    pub fn new(name: impl Into<String>) -> Self {
        Palette { name: name.into(), colors: Vec::new() }
    }

    /// The index of the nearest colour by squared RGB distance. `None` when empty.
    pub fn nearest_index(&self, c: [u8; 4]) -> Option<usize> {
        let mut best: Option<(i32, usize)> = None;
        for (i, p) in self.colors.iter().enumerate() {
            let d = (0..3)
                .map(|k| {
                    let v = c[k] as i32 - p[k] as i32;
                    v * v
                })
                .sum::<i32>();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        best.map(|(_, i)| i)
    }

    /// Snap a colour to the palette, keeping the source alpha (so a soft edge stays
    /// soft while its hue is locked to the ramp).
    pub fn snap(&self, c: [u8; 4]) -> [u8; 4] {
        match self.nearest_index(c) {
            Some(i) => [self.colors[i][0], self.colors[i][1], self.colors[i][2], c[3]],
            None => c,
        }
    }

    // --- .gpl (GIMP palette) ---------------------------------------------

    pub fn from_gpl(text: &str) -> Option<Palette> {
        let mut lines = text.lines();
        let first = lines.next()?.trim();
        if !first.eq_ignore_ascii_case("GIMP Palette") {
            return None;
        }
        let mut pal = Palette::new("palette");
        for line in lines {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("Name:") {
                pal.name = rest.trim().to_string();
                continue;
            }
            if line.starts_with("Columns:") {
                continue;
            }
            let mut it = line.split_whitespace();
            let r = it.next()?.parse::<u16>().ok()?;
            let g = it.next()?.parse::<u16>().ok()?;
            let b = it.next()?.parse::<u16>().ok()?;
            pal.colors.push([r.min(255) as u8, g.min(255) as u8, b.min(255) as u8, 255]);
        }
        Some(pal)
    }

    pub fn to_gpl(&self) -> String {
        let mut s = String::from("GIMP Palette\n");
        s.push_str(&format!("Name: {}\n", if self.name.is_empty() { "palette" } else { &self.name }));
        s.push_str("Columns: 0\n#\n");
        for c in &self.colors {
            s.push_str(&format!(
                "{:>3} {:>3} {:>3}\t#{:02X}{:02X}{:02X}\n",
                c[0], c[1], c[2], c[0], c[1], c[2]
            ));
        }
        s
    }

    // --- .hex (Lospec) ----------------------------------------------------

    /// One `RRGGBB` per line, `#` optional. Lospec's default export.
    pub fn from_hex(text: &str) -> Option<Palette> {
        let mut pal = Palette::new("palette");
        for line in text.lines() {
            let t = line.trim().trim_start_matches('#');
            if t.is_empty() {
                continue;
            }
            if t.len() != 6 && t.len() != 8 {
                return None;
            }
            let r = u8::from_str_radix(&t[0..2], 16).ok()?;
            let g = u8::from_str_radix(&t[2..4], 16).ok()?;
            let b = u8::from_str_radix(&t[4..6], 16).ok()?;
            let a = if t.len() == 8 { u8::from_str_radix(&t[6..8], 16).ok()? } else { 255 };
            pal.colors.push([r, g, b, a]);
        }
        (!pal.colors.is_empty()).then_some(pal)
    }

    pub fn to_hex(&self) -> String {
        self.colors
            .iter()
            .map(|c| format!("{:02X}{:02X}{:02X}\n", c[0], c[1], c[2]))
            .collect()
    }

    /// Read either format, guessing by content (the house rule — never by extension).
    pub fn parse(text: &str) -> Option<Palette> {
        Palette::from_gpl(text).or_else(|| Palette::from_hex(text))
    }

    /// Build a palette from an image's most common colours (median-cut-lite: a
    /// popularity histogram over a 5-bit-per-channel grid). Enough to answer
    /// "give me this texture's palette" without a full quantizer.
    pub fn from_image(px: &[u8], max_colors: usize) -> Palette {
        use std::collections::HashMap;
        let mut hist: HashMap<[u8; 3], (usize, [u32; 3])> = HashMap::new();
        for c in px.chunks_exact(4) {
            if c[3] < 8 {
                continue;
            }
            let key = [c[0] >> 3, c[1] >> 3, c[2] >> 3];
            let e = hist.entry(key).or_insert((0, [0, 0, 0]));
            e.0 += 1;
            e.1[0] += c[0] as u32;
            e.1[1] += c[1] as u32;
            e.1[2] += c[2] as u32;
        }
        let mut v: Vec<_> = hist.into_iter().collect();
        v.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.0.cmp(&b.0)));
        let mut pal = Palette::new("from image");
        for (_, (n, sum)) in v.into_iter().take(max_colors.max(1)) {
            pal.colors.push([
                (sum[0] / n as u32) as u8,
                (sum[1] / n as u32) as u8,
                (sum[2] / n as u32) as u8,
                255,
            ]);
        }
        pal
    }
}

/// A few palettes worth shipping, so the panel isn't empty on first open.
pub fn builtin() -> Vec<Palette> {
    fn pal(name: &str, hexes: &[&str]) -> Palette {
        Palette {
            name: name.to_string(),
            colors: hexes
                .iter()
                .map(|h| {
                    let r = u8::from_str_radix(&h[0..2], 16).unwrap_or(0);
                    let g = u8::from_str_radix(&h[2..4], 16).unwrap_or(0);
                    let b = u8::from_str_radix(&h[4..6], 16).unwrap_or(0);
                    [r, g, b, 255]
                })
                .collect(),
        }
    }
    vec![
        pal(
            "Sweetie 16",
            &[
                "1a1c2c", "5d275d", "b13e53", "ef7d57", "ffcd75", "a7f070", "38b764", "257179",
                "29366f", "3b5dc9", "41a6f6", "73eff7", "f4f4f4", "94b0c2", "566c86", "333c57",
            ],
        ),
        pal(
            "PICO-8",
            &[
                "000000", "1D2B53", "7E2553", "008751", "AB5236", "5F574F", "C2C3C7", "FFF1E8",
                "FF004D", "FFA300", "FFEC27", "00E436", "29ADFF", "83769C", "FF77A8", "FFCCAA",
            ],
        ),
        pal("Game Boy", &["0f380f", "306230", "8bac0f", "9bbc0f"]),
        pal(
            "Endesga 8",
            &["fdfdf8", "d3d3cf", "8b8b88", "4a4a48", "2b2b2a", "a53030", "e07438", "f0c05a"],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpl_round_trips() {
        let p = Palette { name: "test".into(), colors: vec![[255, 0, 0, 255], [0, 128, 64, 255]] };
        let back = Palette::from_gpl(&p.to_gpl()).expect("parse");
        assert_eq!(back.name, "test");
        assert_eq!(back.colors, p.colors);
    }

    #[test]
    fn hex_round_trips_and_tolerates_hashes() {
        let p = Palette::from_hex("#1a1c2c\n5d275d\n\n#b13e53\n").expect("parse");
        assert_eq!(p.colors.len(), 3);
        assert_eq!(p.colors[0], [0x1a, 0x1c, 0x2c, 255]);
        let back = Palette::from_hex(&p.to_hex()).expect("re-parse");
        assert_eq!(back.colors, p.colors);
    }

    #[test]
    fn parse_guesses_the_format() {
        assert!(Palette::parse("GIMP Palette\nName: x\n255 255 255\n").is_some());
        assert!(Palette::parse("ff00ff\n00ff00\n").is_some());
        assert!(Palette::parse("this is not a palette").is_none());
    }

    #[test]
    fn nearest_snaps_and_keeps_alpha() {
        let p = Palette { name: "x".into(), colors: vec![[0, 0, 0, 255], [255, 255, 255, 255]] };
        assert_eq!(p.snap([200, 200, 200, 77]), [255, 255, 255, 77]);
        assert_eq!(p.snap([30, 10, 20, 255]), [0, 0, 0, 255]);
        assert_eq!(Palette::new("empty").snap([1, 2, 3, 4]), [1, 2, 3, 4]);
    }

    #[test]
    fn from_image_finds_the_dominant_colours() {
        let mut px = Vec::new();
        for i in 0..100 {
            let c: [u8; 4] = if i < 70 { [200, 30, 30, 255] } else { [10, 10, 200, 255] };
            px.extend_from_slice(&c);
        }
        let p = Palette::from_image(&px, 4);
        assert_eq!(p.colors.len(), 2);
        assert_eq!(p.colors[0], [200, 30, 30, 255]);
    }

    #[test]
    fn builtins_are_well_formed() {
        for p in builtin() {
            assert!(!p.name.is_empty());
            assert!(p.colors.len() >= 4, "{}", p.name);
            assert!(p.colors.iter().all(|c| c[3] == 255));
        }
    }
}
