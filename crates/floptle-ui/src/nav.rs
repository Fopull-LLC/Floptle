//! Focus and directional navigation (docs/ui-styles.md §D).
//!
//! A menu that can only be driven with a mouse is a menu that can't be driven
//! with a gamepad, and writing the navigation by hand is how you end up with a
//! 1,900-line frontend script that owns its own key-repeat, its own wrap rules
//! and its own idea of what "the next button" means.
//!
//! The model:
//!
//! - An element opts in with [`super::ElementSpec::focusable`]. Nothing is
//!   focusable by accident.
//! - Pressing a direction moves focus to the nearest focusable element *in that
//!   direction*, from the solved rects. Geometry, so it keeps working when you
//!   move a button.
//! - When geometry gets it wrong — a wrapping grid, a deliberate shortcut
//!   across the screen — [`super::Nav`] names the element to go to instead.
//!
//! **What focus LOOKS like is not decided here.** There is no built-in ring:
//! the focused element resolves its style's `focus` block, which can change the
//! border, the glow, the scale, the fill, anything. A hard-coded rectangle
//! would be the engine picking a look, and the whole point of this system is
//! that it doesn't.

use crate::{Node, Placed};

/// A direction pressed on a stick, d-pad or keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir4 {
    Up,
    Down,
    Left,
    Right,
}

impl Dir4 {
    /// `(axis, sign)` — 0 = x, 1 = y; +1 = toward increasing coordinates.
    pub fn axis_sign(self) -> (usize, f32) {
        match self {
            Dir4::Left => (0, -1.0),
            Dir4::Right => (0, 1.0),
            Dir4::Up => (1, -1.0),
            Dir4::Down => (1, 1.0),
        }
    }

    /// The one this direction is the opposite of.
    pub fn flip(self) -> Dir4 {
        match self {
            Dir4::Up => Dir4::Down,
            Dir4::Down => Dir4::Up,
            Dir4::Left => Dir4::Right,
            Dir4::Right => Dir4::Left,
        }
    }

    /// Read a stick / d-pad vector as at most one direction. The larger
    /// component wins, so a diagonal push resolves rather than doing both.
    pub fn from_vector(x: f32, y: f32, deadzone: f32) -> Option<Dir4> {
        if x.abs() < deadzone && y.abs() < deadzone {
            return None;
        }
        if x.abs() >= y.abs() {
            Some(if x > 0.0 { Dir4::Right } else { Dir4::Left })
        } else {
            Some(if y > 0.0 { Dir4::Down } else { Dir4::Up })
        }
    }
}

/// How much a candidate being off to the side counts against it, relative to
/// how far away it is in the direction you actually pressed.
///
/// Above 1 so that "straight ahead but further" beats "closer but off to one
/// side" — which is what makes a column of buttons walk down itself instead of
/// wandering into whatever happens to be diagonally nearby.
const CROSS_PENALTY: f32 = 2.5;

/// The focusable element nearest to `from` in `dir`.
///
/// Candidates must be genuinely ahead: an element that merely overlaps is not
/// "to the right", or a wide banner behind everything would swallow every
/// press.
pub fn nearest(from: [f32; 4], candidates: &[(u32, [f32; 4])], dir: Dir4) -> Option<u32> {
    let (a, s) = dir.axis_sign();
    let c = 1 - a; // the cross axis
    let from_centre = [from[0] + from[2] * 0.5, from[1] + from[3] * 0.5];
    let from_far = if s > 0.0 { from[a] + from[a + 2] } else { from[a] };

    let mut best: Option<(f32, f32, u32)> = None; // (score, cross distance, id)
    for (id, r) in candidates {
        let centre = [r[0] + r[2] * 0.5, r[1] + r[3] * 0.5];
        // Ahead, by centre — robust when rects overlap slightly, which they do
        // constantly once anything has padding.
        let ahead = (centre[a] - from_centre[a]) * s;
        if ahead <= 0.5 {
            continue;
        }
        // Gap along the pressed axis, from our far edge to their near edge.
        let near = if s > 0.0 { r[a] } else { r[a] + r[a + 2] };
        let gap = ((near - from_far) * s).max(0.0);
        // Cross axis: overlapping costs nothing, otherwise the shortfall.
        let lo = from[c].max(r[c]);
        let hi = (from[c] + from[c + 2]).min(r[c] + r[c + 2]);
        let cross_gap = (lo - hi).max(0.0);
        let cross_centre = (centre[c] - from_centre[c]).abs();
        let score = gap + cross_gap * CROSS_PENALTY;
        if best.is_none_or(|(bs, bc, _)| score < bs || (score == bs && cross_centre < bc)) {
            best = Some((score, cross_centre, *id));
        }
    }
    best.map(|(_, _, id)| id)
}

/// Every focusable, visible element of a layer, in draw order, paired with its
/// solved rect. Draw order is also the fallback tab order.
pub fn focusables(roots: &[Node], placed: &[Placed]) -> Vec<(u32, [f32; 4])> {
    let mut ok = std::collections::HashSet::new();
    fn walk(n: &Node, ok: &mut std::collections::HashSet<u32>) {
        // A disabled element is skipped along with everything under it: a
        // greyed-out panel whose children were still reachable would be a way
        // to press a button that visibly can't be pressed.
        if !n.spec.visible || n.spec.disabled {
            return;
        }
        // A text field is focusable whether or not it says so: an element you
        // type into and cannot reach is never what was meant, and making
        // everyone remember to tick a second box would only ever produce bug
        // reports.
        if n.spec.focusable || n.spec.field.is_some() {
            ok.insert(n.id);
        }
        for c in &n.children {
            walk(c, ok);
        }
    }
    for r in roots {
        walk(r, &mut ok);
    }
    placed.iter().filter(|p| ok.contains(&p.id)).map(|p| (p.id, p.rect)).collect()
}

/// Where focus should go when a screen appears with nothing focused: the first
/// focusable in draw order.
pub fn first(focusables: &[(u32, [f32; 4])]) -> Option<u32> {
    focusables.first().map(|(id, _)| *id)
}

/// Wrap around: the element furthest in the OPPOSITE direction, used when a
/// press runs off the end of a list and the layer wraps.
pub fn wrap(from: [f32; 4], candidates: &[(u32, [f32; 4])], dir: Dir4) -> Option<u32> {
    let (a, s) = dir.axis_sign();
    let c = 1 - a;
    let from_centre = [from[0] + from[2] * 0.5, from[1] + from[3] * 0.5];
    // Furthest back along the pressed axis; ties broken by staying in lane.
    let mut best: Option<(f32, f32, u32)> = None;
    for (id, r) in candidates {
        let centre = [r[0] + r[2] * 0.5, r[1] + r[3] * 0.5];
        let back = -(centre[a] - from_centre[a]) * s;
        let cross = (centre[c] - from_centre[c]).abs();
        if back < 0.0 {
            continue;
        }
        if best.is_none_or(|(bb, bc, _)| back > bb || (back == bb && cross < bc)) {
            best = Some((back, cross, *id));
        }
    }
    best.map(|(_, _, id)| id)
}

/// Auto-repeat for a held direction: the state machine every menu needs and
/// nobody should be writing twice.
///
/// Holding a direction moves once, waits `delay`, then moves every `repeat`.
/// Changing direction restarts from scratch — the alternative is a held press
/// that machine-guns the moment you change your mind.
#[derive(Clone, Debug, Default)]
pub struct Repeat {
    held: Option<Dir4>,
    timer: f32,
    fired_once: bool,
}

impl Repeat {
    /// Advance by `dt` with `dir` currently held; returns `true` on the frames a
    /// move should happen.
    pub fn step(&mut self, dir: Option<Dir4>, dt: f32, delay: f32, repeat: f32) -> bool {
        match dir {
            None => {
                *self = Repeat::default();
                false
            }
            Some(d) if self.held != Some(d) => {
                self.held = Some(d);
                self.timer = 0.0;
                self.fired_once = true;
                true // the press itself
            }
            Some(_) => {
                self.timer += dt;
                let threshold = if self.fired_once { delay.max(0.0) } else { repeat.max(0.01) };
                if self.timer >= threshold {
                    self.timer = 0.0;
                    self.fired_once = false;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Forget any held direction (on focus loss, pause, screen change).
    pub fn clear(&mut self) {
        *self = Repeat::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cands(v: &[(u32, [f32; 4])]) -> Vec<(u32, [f32; 4])> {
        v.to_vec()
    }

    #[test]
    fn a_column_walks_down_itself() {
        let c = cands(&[
            (1, [100.0, 0.0, 200.0, 40.0]),
            (2, [100.0, 60.0, 200.0, 40.0]),
            (3, [100.0, 120.0, 200.0, 40.0]),
        ]);
        assert_eq!(nearest(c[0].1, &c, Dir4::Down), Some(2));
        assert_eq!(nearest(c[1].1, &c, Dir4::Down), Some(3));
        assert_eq!(nearest(c[2].1, &c, Dir4::Down), None, "the end is the end");
        assert_eq!(nearest(c[2].1, &c, Dir4::Up), Some(2));
    }

    #[test]
    fn straight_ahead_beats_closer_but_diagonal() {
        // A button directly below at 200 units, and one only 80 away but far
        // off to the right. Pressing down must take the aligned one.
        let from = [0.0, 0.0, 100.0, 40.0];
        let c = cands(&[(1, [0.0, 240.0, 100.0, 40.0]), (2, [900.0, 120.0, 100.0, 40.0])]);
        assert_eq!(nearest(from, &c, Dir4::Down), Some(1));
    }

    #[test]
    fn a_grid_moves_within_its_row_then_between_rows() {
        //  1 2 3
        //  4 5 6
        let cell = |i: u32, cx: f32, cy: f32| (i, [cx, cy, 80.0, 60.0]);
        let c = cands(&[
            cell(1, 0.0, 0.0),
            cell(2, 100.0, 0.0),
            cell(3, 200.0, 0.0),
            cell(4, 0.0, 80.0),
            cell(5, 100.0, 80.0),
            cell(6, 200.0, 80.0),
        ]);
        assert_eq!(nearest(c[1].1, &c, Dir4::Right), Some(3));
        assert_eq!(nearest(c[1].1, &c, Dir4::Left), Some(1));
        assert_eq!(nearest(c[1].1, &c, Dir4::Down), Some(5));
        assert_eq!(nearest(c[4].1, &c, Dir4::Up), Some(2));
        // Off the right-hand edge: nothing, so the caller can wrap.
        assert_eq!(nearest(c[2].1, &c, Dir4::Right), None);
        assert_eq!(wrap(c[2].1, &c, Dir4::Right), Some(1), "wrap lands on the far left");
    }

    #[test]
    fn a_wide_banner_does_not_swallow_presses() {
        // A full-width header that overlaps the button's x range. Pressing
        // right from the button must not select it just because it's "nearby".
        let from = [100.0, 200.0, 100.0, 40.0];
        let c = cands(&[(1, [0.0, 0.0, 1280.0, 80.0]), (2, [400.0, 200.0, 100.0, 40.0])]);
        assert_eq!(nearest(from, &c, Dir4::Right), Some(2));
        assert_eq!(nearest(from, &c, Dir4::Up), Some(1), "up still reaches it");
    }

    #[test]
    fn a_diagonal_push_resolves_to_one_direction() {
        assert_eq!(Dir4::from_vector(0.9, 0.4, 0.5), Some(Dir4::Right));
        assert_eq!(Dir4::from_vector(0.4, 0.9, 0.5), Some(Dir4::Down));
        assert_eq!(Dir4::from_vector(0.2, 0.2, 0.5), None, "inside the deadzone");
        assert_eq!(Dir4::from_vector(-0.8, 0.0, 0.5), Some(Dir4::Left));
    }

    #[test]
    fn repeat_fires_once_then_waits_then_rolls() {
        let mut r = Repeat::default();
        // The press itself.
        assert!(r.step(Some(Dir4::Down), 0.016, 0.35, 0.12));
        // Nothing until the delay is up.
        let mut fired = 0;
        for _ in 0..20 {
            if r.step(Some(Dir4::Down), 0.016, 0.35, 0.12) {
                fired += 1;
            }
        }
        assert_eq!(fired, 0, "0.32s in, still inside the 0.35s delay");
        for _ in 0..4 {
            if r.step(Some(Dir4::Down), 0.016, 0.35, 0.12) {
                fired += 1;
            }
        }
        assert_eq!(fired, 1, "the delay elapsed and it rolled once");
        // Then every 0.12s.
        let mut rolls = 0;
        for _ in 0..30 {
            if r.step(Some(Dir4::Down), 0.016, 0.35, 0.12) {
                rolls += 1;
            }
        }
        assert!((3..=4).contains(&rolls), "≈0.48s of repeat at 0.12s, got {rolls}");
        // Changing direction fires immediately and restarts the delay.
        assert!(r.step(Some(Dir4::Up), 0.016, 0.35, 0.12));
        assert!(!r.step(Some(Dir4::Up), 0.016, 0.35, 0.12));
        // Release clears everything.
        assert!(!r.step(None, 0.016, 0.35, 0.12));
        assert!(r.step(Some(Dir4::Up), 0.016, 0.35, 0.12), "a fresh press always moves");
    }

    #[test]
    fn a_text_field_is_reachable_without_ticking_a_second_box() {
        // An element you can type into but cannot reach is never what anyone
        // meant, and making everyone remember a second checkbox would only
        // produce bug reports.
        let roots = vec![Node::with_children(
            1,
            crate::ElementSpec {
                field: Some(crate::FieldSpec::default()),
                ..Default::default()
            },
            vec![],
        )];
        let placed = vec![Placed { id: 1, rect: [0.0, 0.0, 100.0, 40.0] }];
        assert_eq!(focusables(&roots, &placed).len(), 1);
    }

    #[test]
    fn focusables_skips_hidden_and_disabled_subtrees() {
        use crate::ElementSpec;
        let el = |id: u32, focusable: bool, visible: bool, disabled: bool, kids: Vec<Node>| {
            Node::with_children(
                id,
                ElementSpec { focusable, visible, disabled, ..Default::default() },
                kids,
            )
        };
        let roots = vec![
            el(1, true, true, false, vec![]),
            el(2, true, false, false, vec![]),               // hidden
            el(3, false, true, true, vec![el(4, true, true, false, vec![])]), // disabled parent
            el(5, true, true, false, vec![]),
        ];
        let placed: Vec<Placed> = (1..=5)
            .map(|id| Placed { id, rect: [0.0, id as f32 * 50.0, 100.0, 40.0] })
            .collect();
        let f = focusables(&roots, &placed);
        assert_eq!(f.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(first(&f), Some(1));
    }
}
