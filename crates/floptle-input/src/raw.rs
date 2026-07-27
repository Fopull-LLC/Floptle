//! [`RawInput`] — one sampling window's device truth, producer-agnostic.
//!
//! The editor fills this from winit + gilrs; a test fills it by hand. It carries
//! two different kinds of information and the distinction matters:
//!
//! * **Levels** (`keys`, `mouse_buttons`, pad buttons/axes) — what is held *right
//!   now*. The runtime diffs these against the previous window to find edges.
//! * **Banked edges** (`pressed`, `released`) — presses that happened *since the
//!   last window closed*. The fixed-tick domain needs these: a key tapped between
//!   two ticks is up again by the time the tick samples, and without banking the
//!   press would vanish. The editor already does exactly this for raw keys
//!   (`tick_keys_pressed`); actions inherit the behaviour.
//!
//! A frame-domain producer can leave the banked sets empty and rely on level
//! diffing alone. A tick-domain producer fills them and clears them each tick.

use std::collections::HashSet;

use crate::source::{Key, PadAxis, PadButton, Source};

/// One gamepad's state. Slot index is positional in [`RawInput::pads`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PadState {
    /// False for a slot that has never been filled or whose pad unplugged. A
    /// disconnected pad reads fully neutral rather than freezing its last pose.
    pub connected: bool,
    pub buttons: HashSet<PadButton>,
    /// Indexed by [`PadAxis::index`]; sticks are −1..1, triggers 0..1.
    pub axes: [f32; PadAxis::COUNT],
}

impl PadState {
    pub fn axis(&self, a: PadAxis) -> f32 {
        if self.connected { self.axes[a.index()] } else { 0.0 }
    }

    pub fn button(&self, b: PadButton) -> bool {
        self.connected && self.buttons.contains(&b)
    }
}

/// Everything the devices report for one resolve window.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RawInput {
    pub keys: HashSet<Key>,
    /// Indexed by [`MouseButton::index`].
    pub mouse_buttons: [bool; 5],
    pub mouse_pos: (f32, f32),
    pub mouse_delta: (f32, f32),
    /// (horizontal, vertical) wheel movement accumulated over the window.
    pub scroll: (f32, f32),
    /// Per-slot pad state; slot 0 is player 1. Short vectors are fine — a
    /// missing slot reads as disconnected.
    pub pads: Vec<PadState>,
    /// Down-edges banked since the last window (see the module docs).
    pub pressed: HashSet<Source>,
    /// Up-edges banked since the last window.
    pub released: HashSet<Source>,
}

impl RawInput {
    pub fn pad(&self, slot: u8) -> Option<&PadState> {
        self.pads.get(slot as usize).filter(|p| p.connected)
    }

    /// Grow `pads` so `slot` is addressable, and hand back a mutable view.
    pub fn pad_mut(&mut self, slot: u8) -> &mut PadState {
        let idx = slot as usize;
        if self.pads.len() <= idx {
            self.pads.resize(idx + 1, PadState::default());
        }
        &mut self.pads[idx]
    }

    /// Is this source held right now? `slot` resolves [`crate::PadId::Any`] to
    /// the player being resolved, so P2's "Any" binding reads P2's pad.
    ///
    /// Analog sources report held only past `threshold` — that's how a trigger
    /// drives a digital action.
    pub fn held(&self, src: Source, slot: u8, threshold: f32) -> bool {
        match src {
            Source::Key(k) => self.keys.contains(&k),
            Source::Mouse(b) => self.mouse_buttons[b.index()],
            Source::MouseAxis(a) => self.mouse_axis(a).abs() >= threshold,
            Source::Pad { id, ctrl } => {
                // `Any` reads the resolving player's own pad first and falls back
                // to any connected pad, so single-player works with whichever pad
                // happens to be plugged in.
                self.pad_value(id, ctrl, slot).is_some_and(|v| match ctrl {
                    crate::source::PadControl::Button(_) => v > 0.5,
                    crate::source::PadControl::Axis(_) => v.abs() >= threshold,
                })
            }
        }
    }

    /// The continuous value of a source, for axis resolution. Digital sources
    /// contribute 1.0 while held so a key and a stick compose into one axis.
    pub fn value(&self, src: Source, slot: u8) -> f32 {
        match src {
            Source::Key(k) => self.keys.contains(&k) as u8 as f32,
            Source::Mouse(b) => self.mouse_buttons[b.index()] as u8 as f32,
            Source::MouseAxis(a) => self.mouse_axis(a),
            Source::Pad { id, ctrl } => self.pad_value(id, ctrl, slot).unwrap_or(0.0),
        }
    }

    fn mouse_axis(&self, a: crate::source::MouseAxis) -> f32 {
        use crate::source::MouseAxis::*;
        match a {
            MotionX => self.mouse_delta.0,
            MotionY => self.mouse_delta.1,
            ScrollX => self.scroll.0,
            ScrollY => self.scroll.1,
        }
    }

    /// Read a pad control, resolving `Any` to this player's pad then to the
    /// first connected one. `None` when no pad can answer.
    fn pad_value(
        &self,
        id: crate::source::PadId,
        ctrl: crate::source::PadControl,
        slot: u8,
    ) -> Option<f32> {
        use crate::source::PadControl;
        let read = |p: &PadState| match ctrl {
            PadControl::Button(b) => p.button(b) as u8 as f32,
            PadControl::Axis(a) => p.axis(a),
        };
        match id {
            crate::source::PadId::Slot(n) => self.pad(n).map(read),
            crate::source::PadId::Any => self
                .pad(slot)
                .or_else(|| self.pads.iter().find(|p| p.connected))
                .map(read),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{MouseAxis, PadControl, PadId};

    fn pad(axes: [f32; PadAxis::COUNT], buttons: &[PadButton]) -> PadState {
        PadState { connected: true, buttons: buttons.iter().copied().collect(), axes }
    }

    #[test]
    fn disconnected_pads_read_neutral() {
        let mut raw = RawInput::default();
        let p = raw.pad_mut(0);
        p.axes[PadAxis::LeftStickX.index()] = 0.9;
        p.buttons.insert(PadButton::South);
        // Never marked connected — a yanked pad must not freeze its last pose.
        assert_eq!(raw.value(Source::Pad { id: PadId::Slot(0), ctrl: PadControl::Axis(PadAxis::LeftStickX) }, 0), 0.0);
        assert!(!raw.held(
            Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::South) },
            0,
            0.5
        ));
    }

    #[test]
    fn any_prefers_the_resolving_players_pad() {
        let raw = RawInput {
            pads: vec![
                pad([0.0; PadAxis::COUNT], &[]),
                pad([0.0; PadAxis::COUNT], &[PadButton::South]),
            ],
            ..Default::default()
        };
        let jump = Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::South) };
        assert!(!raw.held(jump, 0, 0.5), "P1's own pad isn't pressing it");
        assert!(raw.held(jump, 1, 0.5), "P2's own pad is");
    }

    #[test]
    fn any_falls_back_to_the_first_connected_pad() {
        // Single-player: slot 0 empty, one pad plugged into slot 2.
        let raw = RawInput {
            pads: vec![
                PadState::default(),
                PadState::default(),
                pad([0.0; PadAxis::COUNT], &[PadButton::North]),
            ],
            ..Default::default()
        };
        let src = Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::North) };
        assert!(raw.held(src, 0, 0.5));
    }

    #[test]
    fn triggers_gate_digital_actions_on_the_threshold() {
        let mut raw = RawInput::default();
        let mut axes = [0.0; PadAxis::COUNT];
        axes[PadAxis::RightZ.index()] = 0.3;
        raw.pads = vec![pad(axes, &[])];
        let fire = Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::RightZ) };
        assert!(!raw.held(fire, 0, 0.5), "light pull must not fire");
        raw.pads[0].axes[PadAxis::RightZ.index()] = 0.8;
        assert!(raw.held(fire, 0, 0.5), "full pull fires");
    }

    #[test]
    fn digital_sources_contribute_one_to_an_axis() {
        let mut raw = RawInput::default();
        raw.keys.insert(Key::KeyD);
        assert_eq!(raw.value(Source::Key(Key::KeyD), 0), 1.0);
        assert_eq!(raw.value(Source::Key(Key::KeyA), 0), 0.0);
    }

    #[test]
    fn mouse_motion_reads_as_an_axis() {
        let raw = RawInput { mouse_delta: (12.0, -3.0), ..Default::default() };
        assert_eq!(raw.value(Source::MouseAxis(MouseAxis::MotionX), 0), 12.0);
        assert_eq!(raw.value(Source::MouseAxis(MouseAxis::MotionY), 0), -3.0);
    }
}
