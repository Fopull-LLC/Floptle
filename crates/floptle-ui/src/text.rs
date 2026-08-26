//! Text styling beyond "a colour"
//! (docs/ui-styles.md §A, gap 2).
//!
//! Type is the loudest signal of whether a screen was designed or assembled.
//! The first cut gave a run `size`, `color`, `align`, `valign`, `fit`, `font` —
//! which means every project's text sits at the font's default tracking, with
//! no way to make it legible over a bright background short of writing a
//! shader. Both of Ty's projects run all of their text that way, and it shows.
//!
//! Nothing here needs a new pipeline. Strokes and shadows are extra glyph
//! instances the packer emits from the same atlas; tracking, line height, case
//! and truncation are arithmetic during layout. The cost is a few more quads on
//! text that opts in, and nothing at all on text that doesn't.

use serde::{Deserialize, Serialize};

/// An outline drawn around the glyphs. The one thing that makes small text
/// survive an arbitrary background — a HUD over a bright stage, a label over
/// artwork — without a panel behind it.
///
/// Rendered as offset copies of the run rather than a true SDF outline: exact
/// enough at UI sizes, and it keeps text in the shared glyph atlas.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextStroke {
    pub color: [f32; 4],
    /// Width in design units. Above ~3 the offset-copy approximation starts to
    /// show its corners; that is far past anything UI text wants.
    pub width: f32,
}

impl Default for TextStroke {
    fn default() -> Self {
        TextStroke { color: [0.0, 0.0, 0.0, 1.0], width: 1.5 }
    }
}

/// A dropped copy of the run behind itself. Cheaper than a stroke (one extra
/// copy, not eight) and usually enough to lift text off a busy background.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TextShadow {
    pub color: [f32; 4],
    pub offset: [f32; 2],
}

impl Default for TextShadow {
    fn default() -> Self {
        TextShadow { color: [0.0, 0.0, 0.0, 0.6], offset: [0.0, 2.0] }
    }
}

/// Letter-case transform applied at draw time, leaving the authored string
/// alone.
///
/// This exists so a *style* can impose case. "All caps" is a typographic
/// decision that belongs with tracking and weight, not baked into the content —
/// and baking it means the string you search for in the scene is not the string
/// you typed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Case {
    #[default]
    AsIs,
    Upper,
    Lower,
    /// First letter of each whitespace-separated word.
    Title,
}

impl Case {
    /// Apply the transform. `AsIs` borrows; the rest allocate only when they
    /// actually change something.
    pub fn apply<'a>(self, s: &'a str) -> std::borrow::Cow<'a, str> {
        use std::borrow::Cow;
        match self {
            Case::AsIs => Cow::Borrowed(s),
            Case::Upper => Cow::Owned(s.to_uppercase()),
            Case::Lower => Cow::Owned(s.to_lowercase()),
            Case::Title => {
                let mut out = String::with_capacity(s.len());
                let mut start_of_word = true;
                for c in s.chars() {
                    if c.is_whitespace() {
                        start_of_word = true;
                        out.push(c);
                    } else if start_of_word {
                        start_of_word = false;
                        out.extend(c.to_uppercase());
                    } else {
                        out.extend(c.to_lowercase());
                    }
                }
                Cow::Owned(out)
            }
        }
    }
}

/// What happens to a run that does not fit its element.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Overflow {
    /// Draw past the edge (the historical behaviour — and what a mask expects).
    #[default]
    Show,
    /// Cut at the element's rect.
    Clip,
    /// Drop characters and append `…` so the truncation is legible as one.
    Ellipsis,
}

/// Break a run into lines, honouring explicit `\n` and (optionally) wrapping
/// at `max_width`.
///
/// `advance` measures one line in the caller's units — the renderer passes a
/// closure over its own font metrics, tests pass something deterministic. The
/// solver and the packer both call this, so a `Fit` element's measured size and
/// the glyphs finally drawn can never disagree about where the breaks are,
/// which is the classic source of text that overflows its own auto-size.
pub fn wrap_lines(text: &str, max_width: f32, advance: &dyn Fn(&str) -> f32) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        if max_width <= 0.0 || advance(para) <= max_width {
            out.push(para.to_string());
            continue;
        }
        let mut line = String::new();
        for word in para.split_inclusive(char::is_whitespace) {
            let candidate = format!("{line}{word}");
            if !line.is_empty() && advance(candidate.trim_end()) > max_width {
                out.push(std::mem::take(&mut line).trim_end().to_string());
                line.push_str(word.trim_start());
            } else {
                line = candidate;
            }
        }
        // A single word longer than the line: hard-break it rather than let it
        // run off forever.
        if advance(line.trim_end()) > max_width && line.chars().count() > 1 {
            let mut cur = String::new();
            for c in line.trim_end().chars() {
                let mut probe = cur.clone();
                probe.push(c);
                if !cur.is_empty() && advance(&probe) > max_width {
                    out.push(std::mem::take(&mut cur));
                }
                cur.push(c);
            }
            if !cur.is_empty() {
                out.push(cur);
            }
        } else {
            out.push(line.trim_end().to_string());
        }
    }
    out
}

/// Truncate `line` to `max_width`, appending `…`. Returns the original when it
/// already fits.
pub fn ellipsize(line: &str, max_width: f32, advance: &dyn Fn(&str) -> f32) -> String {
    if max_width <= 0.0 || advance(line) <= max_width {
        return line.to_string();
    }
    let mut cut = String::new();
    for c in line.chars() {
        let mut probe = cut.clone();
        probe.push(c);
        probe.push('…');
        if advance(&probe) > max_width {
            break;
        }
        cut.push(c);
    }
    cut.push('…');
    cut
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 10 units per char — makes the expected break points obvious by eye.
    fn adv(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn case_transforms() {
        assert_eq!(Case::Upper.apply("hi there"), "HI THERE");
        assert_eq!(Case::Title.apply("hi THERE"), "Hi There");
        assert_eq!(Case::AsIs.apply("hi THERE"), "hi THERE");
    }

    #[test]
    fn explicit_newlines_always_break() {
        assert_eq!(wrap_lines("a\nb", 0.0, &adv), vec!["a", "b"]);
    }

    #[test]
    fn wraps_on_word_boundaries() {
        // 45 units = 4 chars per line; "one two" is 7.
        assert_eq!(wrap_lines("one two", 45.0, &adv), vec!["one", "two"]);
    }

    /// A word that cannot fit on any line must still terminate — the naive
    /// "push the word and move on" loop runs off the right edge forever.
    #[test]
    fn a_too_long_word_hard_breaks() {
        let lines = wrap_lines("abcdefgh", 30.0, &adv);
        assert!(lines.len() > 1, "{lines:?}");
        assert_eq!(lines.concat(), "abcdefgh");
    }

    #[test]
    fn ellipsis_fits_within_the_budget() {
        let out = ellipsize("abcdefgh", 40.0, &adv);
        assert!(out.ends_with('…'));
        assert!(adv(&out) <= 40.0, "{out}");
    }

    #[test]
    fn ellipsis_leaves_a_fitting_line_alone() {
        assert_eq!(ellipsize("abc", 100.0, &adv), "abc");
    }
}
