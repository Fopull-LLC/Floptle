//! Text-field editing: the pure half.
//!
//! Everything here is a function of `(value, caret, anchor, op)` and nothing
//! else — no window, no clipboard, no font. The editor owns the keyboard and
//! the OS clipboard and calls in here; the renderer owns where a caret lands on
//! screen. That split is what makes "backspace over a selection", "one leading
//! minus", "the cap counts characters not bytes" testable without a GPU.
//!
//! Positions are **character** indices, not byte offsets. A field is the one
//! place in the engine where a player types arbitrary text, so it is also the
//! one place where "the fifth character" and "the fifth byte" routinely
//! disagree — and where getting it wrong panics on a slice boundary in front of
//! whoever was trying to enter their name.

use crate::FieldSpec;

/// One editing operation. Chord decoding (which key with which modifier means
/// which of these) belongs to whatever owns the keyboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Edit {
    /// Type text at the caret, replacing any selection. Also how a paste
    /// arrives — a paste is typing that happens to be fast.
    Insert(String),
    Backspace,
    /// Backspace to the start of the previous word (Ctrl-Backspace).
    BackspaceWord,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    SelectAll,
}

/// The selected range as `(start, end)` character indices, ordered.
pub fn selection(caret: usize, anchor: usize) -> (usize, usize) {
    if caret <= anchor { (caret, anchor) } else { (anchor, caret) }
}

/// Byte offset of a character index (clamped to the string's length).
pub fn byte_of(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map(|(i, _)| i).unwrap_or(s.len())
}

/// The state a field's editor carries between keystrokes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub caret: usize,
    pub anchor: usize,
}

impl Cursor {
    /// Collapse the selection to a single point.
    pub fn at(i: usize) -> Cursor {
        Cursor { caret: i, anchor: i }
    }
    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }
    /// Keep both ends inside a value that changed underneath us — a script can
    /// assign the text while the player is typing into it.
    pub fn clamp(&mut self, len: usize) {
        self.caret = self.caret.min(len);
        self.anchor = self.anchor.min(len);
    }
}

/// How far a field's text run slides LEFT (a negative number, or 0) so the
/// caret stays inside the box — a value longer than its field scrolls out from
/// under itself as you type past the end.
///
/// Lives here, in the unit-free half, because two places need the same answer
/// and they work in different units: the renderer applies it in physical
/// pixels, and the editor SUBTRACTS it in design units to turn a click x back
/// into a character index. When only one of them knew about it, clicking into a
/// scrolled value put the caret several characters from the pointer — which
/// reads as the mouse being offset, not as a text bug.
///
/// Clamped so a value that fits never pulls away from the edge it was aligned
/// against.
pub fn scroll_shift(caret_x: f32, run_w: f32, rect_x: f32, rect_w: f32, pad: f32) -> f32 {
    let over = caret_x - (rect_x + rect_w - pad);
    if over <= 0.0 {
        return 0.0;
    }
    -over.min((run_w - rect_w + pad * 2.0).max(0.0))
}

/// Which gap between characters a click at `x` lands in — the inverse of where
/// the renderer put the glyphs.
///
/// `width` measures a prefix of `shown` in the caller's units (the editor works
/// in design units, so `x`, `left`, `rect` and `pad` are too); the font belongs
/// to the caller. `drawn_caret` is the caret index that was on screen in the
/// frame being clicked, which is what decides how far the run had scrolled —
/// `None` for a field that wasn't focused, and therefore hadn't scrolled.
///
/// Lands on the NEAREST gap, not the one before: clicking the right half of a
/// letter puts the caret after it, which is what every text field does and what
/// nobody notices until it doesn't.
pub fn caret_at(
    shown: &str,
    left: f32,
    rect: [f32; 4],
    drawn_caret: Option<usize>,
    x: f32,
    pad: f32,
    width: &dyn Fn(&str) -> f32,
) -> usize {
    let prefix = |i: usize| -> String { shown.chars().take(i).collect() };
    let full = width(shown);
    let shift = drawn_caret
        .map(|c| scroll_shift(left + width(&prefix(c)), full, rect[0], rect[2], pad))
        .unwrap_or(0.0);
    let target = x - left - shift;
    let mut best = (0usize, f32::INFINITY);
    for i in 0..=shown.chars().count() {
        let d = (width(&prefix(i)) - target).abs();
        if d < best.1 {
            best = (i, d);
        }
    }
    best.0
}

/// Apply one edit. Returns true when the VALUE changed (caret-only moves
/// return false, so `changed` doesn't fire for pressing Left).
///
/// `extend` is the shift key: it keeps the anchor where it is so a movement
/// grows the selection instead of collapsing it.
pub fn apply(value: &mut String, cur: &mut Cursor, op: &Edit, extend: bool, spec: &FieldSpec) -> bool {
    let len = value.chars().count();
    cur.clamp(len);
    let (sel_a, sel_b) = selection(cur.caret, cur.anchor);
    match op {
        Edit::Insert(text) => {
            let filtered = filter(text, spec);
            // A rejected keystroke does NOTHING — it must not quietly eat the
            // selection. Typing `x` into a numeric field with three digits
            // highlighted has to leave the three digits there.
            if filtered.is_empty() {
                return false;
            }
            let mut next: String = value.chars().take(sel_a).collect();
            next.push_str(&filtered);
            next.extend(value.chars().skip(sel_b));
            // The cap is applied to the RESULT, not to the keystroke: pasting
            // a long string into a short field fills it rather than being
            // refused, which is what every field anyone has used does.
            let mut kept = filtered.chars().count();
            if spec.max_len > 0 && next.chars().count() > spec.max_len as usize {
                let room = (spec.max_len as usize).saturating_sub(len - (sel_b - sel_a));
                kept = kept.min(room);
                let trimmed: String = filtered.chars().take(kept).collect();
                next = value.chars().take(sel_a).collect();
                next.push_str(&trimmed);
                next.extend(value.chars().skip(sel_b));
            }
            if next == *value {
                return false;
            }
            *value = next;
            *cur = Cursor::at(sel_a + kept);
            true
        }
        Edit::Backspace | Edit::BackspaceWord | Edit::Delete => {
            let (from, to) = if sel_a != sel_b {
                (sel_a, sel_b)
            } else {
                match op {
                    Edit::Backspace => (sel_a.saturating_sub(1), sel_a),
                    Edit::BackspaceWord => (word_left(value, sel_a), sel_a),
                    _ => (sel_a, (sel_a + 1).min(len)),
                }
            };
            if from == to {
                return false;
            }
            let mut next: String = value.chars().take(from).collect();
            next.extend(value.chars().skip(to));
            *value = next;
            *cur = Cursor::at(from);
            true
        }
        Edit::SelectAll => {
            *cur = Cursor { caret: len, anchor: 0 };
            false
        }
        _ => {
            // Pure movement. Without shift, a movement out of a selection
            // lands on the edge you moved toward rather than dragging the
            // caret out of the middle of it.
            let next = match op {
                Edit::Left if !extend && sel_a != sel_b => sel_a,
                Edit::Right if !extend && sel_a != sel_b => sel_b,
                Edit::Left => cur.caret.saturating_sub(1),
                Edit::Right => (cur.caret + 1).min(len),
                Edit::WordLeft => word_left(value, cur.caret),
                Edit::WordRight => word_right(value, cur.caret),
                Edit::Home => 0,
                _ => len,
            };
            cur.caret = next;
            if !extend {
                cur.anchor = next;
            }
            false
        }
    }
}

/// The characters a spec will accept out of a typed or pasted string.
///
/// Control characters never survive: a newline pasted out of a chat window
/// must not turn a one-line field into two, and a tab must not become a glyph.
fn filter(text: &str, spec: &FieldSpec) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if c.is_control() {
            continue;
        }
        if spec.numeric && !c.is_ascii_digit() && c != '-' && c != '.' {
            continue;
        }
        if spec.upper {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// Start of the word at or before `i` (skip spaces, then skip the word).
fn word_left(s: &str, i: usize) -> usize {
    let cs: Vec<char> = s.chars().collect();
    let mut j = i.min(cs.len());
    while j > 0 && cs[j - 1].is_whitespace() {
        j -= 1;
    }
    while j > 0 && !cs[j - 1].is_whitespace() {
        j -= 1;
    }
    j
}

/// End of the word at or after `i`.
fn word_right(s: &str, i: usize) -> usize {
    let cs: Vec<char> = s.chars().collect();
    let mut j = i.min(cs.len());
    while j < cs.len() && cs[j].is_whitespace() {
        j += 1;
    }
    while j < cs.len() && !cs[j].is_whitespace() {
        j += 1;
    }
    j
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> FieldSpec {
        FieldSpec::default()
    }

    fn run(value: &str, caret: usize, op: Edit, spec: &FieldSpec) -> (String, Cursor) {
        let mut v = value.to_string();
        let mut c = Cursor::at(caret);
        apply(&mut v, &mut c, &op, false, spec);
        (v, c)
    }

    #[test]
    fn typing_lands_at_the_caret() {
        let (v, c) = run("ac", 1, Edit::Insert("b".into()), &plain());
        assert_eq!(v, "abc");
        assert_eq!(c.caret, 2);
    }

    #[test]
    fn typing_over_a_selection_replaces_it() {
        let mut v = "hello world".to_string();
        let mut c = Cursor { anchor: 6, caret: 11 };
        assert!(apply(&mut v, &mut c, &Edit::Insert("there".into()), false, &plain()));
        assert_eq!(v, "hello there");
        assert_eq!(c, Cursor::at(11));
    }

    #[test]
    fn backspace_eats_the_selection_not_a_character() {
        let mut v = "abcdef".to_string();
        let mut c = Cursor { anchor: 1, caret: 4 };
        assert!(apply(&mut v, &mut c, &Edit::Backspace, false, &plain()));
        assert_eq!(v, "aef");
        assert_eq!(c, Cursor::at(1));
    }

    #[test]
    fn a_cap_counts_characters_not_bytes() {
        // Four characters, ten bytes. A byte-counting cap would refuse the
        // fourth and nobody named "Zoë" would be able to finish.
        let spec = FieldSpec { max_len: 4, ..plain() };
        let (v, _) = run("Zoë", 3, Edit::Insert("é".into()), &spec);
        assert_eq!(v, "Zoëé");
        let (v, _) = run("Zoëé", 4, Edit::Insert("x".into()), &spec);
        assert_eq!(v, "Zoëé", "the cap holds at four characters");
    }

    #[test]
    fn a_paste_fills_the_remaining_room_rather_than_being_refused() {
        let spec = FieldSpec { max_len: 5, ..plain() };
        let (v, c) = run("AB", 2, Edit::Insert("CDEFGH".into()), &spec);
        assert_eq!(v, "ABCDE");
        assert_eq!(c.caret, 5, "the caret follows what actually landed");
    }

    #[test]
    fn a_numeric_field_takes_digits_and_nothing_else() {
        let spec = FieldSpec { numeric: true, ..plain() };
        let (v, _) = run("", 0, Edit::Insert("-12.5abc".into()), &spec);
        assert_eq!(v, "-12.5");
    }

    #[test]
    fn an_upper_field_shouts_as_you_type() {
        // Lobby codes: the relay generates upper case, so the player should
        // not have to hold shift for eight characters.
        let spec = FieldSpec { upper: true, ..plain() };
        let (v, _) = run("", 0, Edit::Insert("5hw8z".into()), &spec);
        assert_eq!(v, "5HW8Z");
    }

    #[test]
    fn a_pasted_newline_does_not_become_a_second_line() {
        let (v, _) = run("", 0, Edit::Insert("AB\nCD\t".into()), &plain());
        assert_eq!(v, "ABCD");
    }

    #[test]
    fn moving_out_of_a_selection_lands_on_the_edge_you_moved_toward() {
        let mut v = "abcdef".to_string();
        let mut c = Cursor { anchor: 1, caret: 4 };
        apply(&mut v, &mut c, &Edit::Left, false, &plain());
        assert_eq!(c, Cursor::at(1));
        let mut c = Cursor { anchor: 1, caret: 4 };
        apply(&mut v, &mut c, &Edit::Right, false, &plain());
        assert_eq!(c, Cursor::at(4));
    }

    #[test]
    fn shift_movement_grows_the_selection() {
        let mut v = "abcdef".to_string();
        let mut c = Cursor::at(3);
        apply(&mut v, &mut c, &Edit::Right, true, &plain());
        apply(&mut v, &mut c, &Edit::Right, true, &plain());
        assert_eq!(selection(c.caret, c.anchor), (3, 5));
    }

    #[test]
    fn word_moves_step_over_words() {
        let mut v = "the quick brown".to_string();
        let mut c = Cursor::at(15);
        apply(&mut v, &mut c, &Edit::WordLeft, false, &plain());
        assert_eq!(c.caret, 10);
        apply(&mut v, &mut c, &Edit::WordLeft, false, &plain());
        assert_eq!(c.caret, 4);
        apply(&mut v, &mut c, &Edit::WordRight, false, &plain());
        assert_eq!(c.caret, 9);
    }

    #[test]
    fn a_move_is_not_a_change() {
        let mut v = "abc".to_string();
        let mut c = Cursor::at(1);
        assert!(!apply(&mut v, &mut c, &Edit::Right, false, &plain()));
        assert!(!apply(&mut v, &mut c, &Edit::SelectAll, false, &plain()));
        assert_eq!(v, "abc");
    }

    #[test]
    fn a_rejected_keystroke_does_not_eat_the_selection() {
        let spec = FieldSpec { numeric: true, ..plain() };
        let mut v = "123".to_string();
        let mut c = Cursor { anchor: 0, caret: 3 };
        assert!(!apply(&mut v, &mut c, &Edit::Insert("x".into()), false, &spec));
        assert_eq!(v, "123");
        assert!(c.has_selection(), "and it leaves the selection alone");
    }

    #[test]
    fn backspace_at_the_start_and_delete_at_the_end_do_nothing() {
        let mut v = "abc".to_string();
        let mut c = Cursor::at(0);
        assert!(!apply(&mut v, &mut c, &Edit::Backspace, false, &plain()));
        let mut c = Cursor::at(3);
        assert!(!apply(&mut v, &mut c, &Edit::Delete, false, &plain()));
        assert_eq!(v, "abc");
    }

    /// The renderer draws with this and the click-to-caret mapping undoes it.
    /// Both must read the same function or a click lands on the wrong glyph.
    #[test]
    fn a_run_only_scrolls_once_the_caret_leaves_the_box() {
        // Box 100 wide at x=10, 2 units of breathing room, run 300 wide.
        let s = |caret_x: f32| scroll_shift(caret_x, 300.0, 10.0, 100.0, 2.0);
        assert_eq!(s(50.0), 0.0, "a caret inside the box must not scroll it");
        assert_eq!(s(108.0), 0.0, "the last two units are the pad, still inside");
        assert!((s(148.0) - -40.0).abs() < 1e-4, "past the edge, scroll by the overshoot");
        // Scrolled to the end: the run's tail sits against the right edge and
        // stops — it never keeps sliding into empty space.
        assert!((s(9_999.0) - -(300.0 - 100.0 + 4.0)).abs() < 1e-4);
        // A value that FITS never scrolls, however far right the caret is.
        assert_eq!(scroll_shift(9_999.0, 40.0, 10.0, 100.0, 2.0), 0.0);
    }

    /// The whole point of sharing `scroll_shift`: a click has to come back to
    /// the character it landed on, at any scroll position. A 10-unit fixed
    /// width per glyph makes the expected answers arithmetic.
    #[test]
    fn a_click_round_trips_to_the_character_under_it_even_when_scrolled() {
        let w = |s: &str| s.chars().count() as f32 * 10.0;
        let shown = "ABCDEFGHIJKLMNOPQRST"; // 200 units of text …
        let rect = [10.0, 0.0, 100.0, 20.0]; // … in a 100-unit box at x=10.
        let (left, pad) = (10.0, 2.0);
        // Unfocused: no caret was drawn, so nothing scrolled. The 4th glyph
        // spans 30..40, so its left half lands before it and its right half
        // after — 35.0 exactly is a tie, and ties go to the earlier gap.
        assert_eq!(caret_at(shown, left, rect, None, 10.0 + 32.0, pad, &w), 3);
        assert_eq!(caret_at(shown, left, rect, None, 10.0 + 38.0, pad, &w), 4);
        // Focused with the caret at the end, the run has scrolled left by
        // 200 - 100 + 4 = 104. Every glyph on screen is 104 units further left
        // than the layout says, so a click at the box's left edge is the
        // character at 104 units into the string — glyph 10 (index 10 or 11).
        let at_edge = caret_at(shown, left, rect, Some(20), 10.0, pad, &w);
        assert!(
            (10..=11).contains(&at_edge),
            "clicking the left edge of a scrolled field gave index {at_edge}, not ~10"
        );
        // And the inverse of the renderer: whatever x the caret is DRAWN at
        // must map back to the caret's own index.
        for c in [0usize, 3, 9, 17, 20] {
            let caret_x = left + w(&shown[..c]);
            let shift = scroll_shift(caret_x, w(shown), rect[0], rect[2], pad);
            let back = caret_at(shown, left, rect, Some(c), caret_x + shift, pad, &w);
            assert_eq!(back, c, "the drawn caret for {c} did not map back to itself");
        }
    }

    #[test]
    fn a_caret_past_the_end_is_pulled_back_rather_than_panicking() {
        // A script can assign `text` while the player is mid-edit; the caret
        // must survive the value shrinking under it.
        let mut v = "ab".to_string();
        let mut c = Cursor::at(99);
        assert!(apply(&mut v, &mut c, &Edit::Backspace, false, &plain()));
        assert_eq!(v, "a");
    }
}
