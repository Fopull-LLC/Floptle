//! Fingers on the screen: what `input.touches()` reports, and how a finger
//! that lands on the game's UI works it the way a mouse would.
//!
//! A phone has no mouse, and winit reports a finger only as a touch — never
//! also as a cursor and a button — so without this a web build on a phone
//! received nothing at all, not even a tap on its main menu.
//!
//! **Which fingers are the UI's.** A finger that lands on an interactive
//! element of the game's UI drives the UI as the mouse does: it moves the
//! pointer, holds the left button, and lets go when it lifts, so a button
//! fires `onClicked` and a slider drags with no game code. Any other finger is
//! the game's alone: a stick or a look drag never clicks a button it passes
//! over. Every finger, the UI's included, is listed in `input.touches()`,
//! with `ui = true` on the one driving the UI so a game's controls can skip it.

use std::collections::BTreeMap;

use floptle_script::TouchPoint;

/// One finger's state across frames.
#[derive(Clone, Debug)]
struct Finger {
    x: f32,
    y: f32,
    /// Movement since the last snapshot.
    dx: f32,
    dy: f32,
    pressure: f32,
    began: bool,
    moved: bool,
    /// `Some("ended" | "cancelled")` once lifted: reported for one frame, then gone.
    lifted: Option<&'static str>,
    ui: bool,
}

/// What a touch event was.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Phase {
    Began,
    Moved,
    Ended,
    Cancelled,
}

/// The fingers on the screen, and which one (if any) is working the UI.
#[derive(Default)]
pub(crate) struct Touches {
    fingers: BTreeMap<u64, Finger>,
    /// The finger driving the UI as the mouse.
    pub(crate) ui_finger: Option<u64>,
    /// A finger that went down where the UI might be: decided against this
    /// frame's UI hit test (see `Editor::ui_interact`).
    pub(crate) pending: Option<u64>,
}

impl Touches {
    /// Record one touch event. `pressure` is 0..1, or `None` where the screen
    /// does not say.
    pub(crate) fn note(&mut self, id: u64, phase: Phase, x: f32, y: f32, pressure: Option<f32>) {
        let pressure = pressure.unwrap_or(1.0);
        match phase {
            Phase::Began => {
                self.fingers.insert(
                    id,
                    Finger { x, y, dx: 0.0, dy: 0.0, pressure, began: true, moved: false, lifted: None, ui: false },
                );
            }
            Phase::Moved => {
                if let Some(f) = self.fingers.get_mut(&id) {
                    f.dx += x - f.x;
                    f.dy += y - f.y;
                    f.x = x;
                    f.y = y;
                    f.pressure = pressure;
                    f.moved = true;
                }
            }
            Phase::Ended | Phase::Cancelled => {
                if let Some(f) = self.fingers.get_mut(&id) {
                    f.dx += x - f.x;
                    f.dy += y - f.y;
                    f.x = x;
                    f.y = y;
                    f.lifted = Some(if phase == Phase::Ended { "ended" } else { "cancelled" });
                }
            }
        }
    }

    /// Whether a finger has lifted (or never existed).
    pub(crate) fn lifted(&self, id: u64) -> bool {
        self.fingers.get(&id).is_none_or(|f| f.lifted.is_some())
    }

    /// Mark a finger as the one working the UI.
    pub(crate) fn set_ui(&mut self, id: u64) {
        if let Some(f) = self.fingers.get_mut(&id) {
            f.ui = true;
        }
        self.ui_finger = Some(id);
    }

    /// This frame's fingers, for `input.touches()`.
    pub(crate) fn snapshot(&self) -> Vec<TouchPoint> {
        self.fingers
            .iter()
            .map(|(&id, f)| TouchPoint {
                id,
                x: f.x,
                y: f.y,
                dx: f.dx,
                dy: f.dy,
                phase: match f.lifted {
                    Some(p) => p,
                    None if f.began => "began",
                    None if f.moved => "moved",
                    None => "held",
                },
                pressure: if f.lifted.is_some() { 0.0 } else { f.pressure },
                began: f.began,
                ui: f.ui,
            })
            .collect()
    }

    /// The frame is over: lifted fingers go, and the rest start the next frame
    /// held still.
    pub(crate) fn end_frame(&mut self) {
        self.fingers.retain(|_, f| f.lifted.is_none());
        for f in self.fingers.values_mut() {
            f.dx = 0.0;
            f.dy = 0.0;
            f.began = false;
            f.moved = false;
        }
        if self.ui_finger.is_some_and(|id| !self.fingers.contains_key(&id)) {
            self.ui_finger = None;
        }
        if self.pending.is_some_and(|id| !self.fingers.contains_key(&id)) {
            self.pending = None;
        }
    }
}

impl crate::Editor {
    /// One touch event from the window, in the window's pixel space.
    ///
    /// A finger landing while no other finger works the UI moves the pointer
    /// to it and waits for this frame's UI hit test (`ui_interact`) to say
    /// whether it is the UI's. The UI's finger carries the pointer with it and
    /// lets go of the left button when it lifts.
    pub(crate) fn note_touch(&mut self, id: u64, phase: Phase, x: f32, y: f32, pressure: Option<f32>) {
        self.touch_device = true;
        self.touches.note(id, phase, x, y, pressure);
        let pos = floptle_core::math::Vec2::new(x, y);
        match phase {
            Phase::Began if self.touches.ui_finger.is_none() && self.touches.pending.is_none() => {
                self.touches.pending = Some(id);
                self.cursor = Some(pos);
            }
            Phase::Moved if self.touches.ui_finger == Some(id) => self.cursor = Some(pos),
            Phase::Ended | Phase::Cancelled if self.touches.ui_finger == Some(id) => {
                self.cursor = Some(pos);
                self.track_mouse_button(0, false);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finger_reports_began_moved_held_and_ended_once_each() {
        let mut t = Touches::default();
        t.note(7, Phase::Began, 10.0, 20.0, None);
        let s = t.snapshot();
        assert_eq!((s[0].phase, s[0].began, s[0].pressure), ("began", true, 1.0));
        t.end_frame();
        t.note(7, Phase::Moved, 15.0, 22.0, Some(0.5));
        let s = t.snapshot();
        assert_eq!((s[0].phase, s[0].dx, s[0].dy, s[0].began), ("moved", 5.0, 2.0, false));
        t.end_frame();
        assert_eq!(t.snapshot()[0].phase, "held");
        assert_eq!((t.snapshot()[0].dx, t.snapshot()[0].dy), (0.0, 0.0));
        t.end_frame();
        t.note(7, Phase::Ended, 15.0, 22.0, None);
        assert_eq!(t.snapshot()[0].phase, "ended");
        t.end_frame();
        assert!(t.snapshot().is_empty(), "an ended finger is reported once");
    }

    #[test]
    fn a_tap_inside_one_frame_still_says_it_began() {
        let mut t = Touches::default();
        t.note(1, Phase::Began, 0.0, 0.0, None);
        t.note(1, Phase::Ended, 0.0, 0.0, None);
        let s = t.snapshot();
        assert_eq!((s[0].phase, s[0].began), ("ended", true));
    }

    #[test]
    fn several_fingers_are_tracked_apart() {
        let mut t = Touches::default();
        t.note(1, Phase::Began, 0.0, 0.0, None);
        t.note(2, Phase::Began, 100.0, 0.0, None);
        t.end_frame();
        t.note(2, Phase::Moved, 110.0, 0.0, None);
        t.note(1, Phase::Cancelled, 0.0, 0.0, None);
        let s = t.snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].id, s[0].phase), (1, "cancelled"));
        assert_eq!((s[1].id, s[1].phase, s[1].dx), (2, "moved", 10.0));
    }
}
