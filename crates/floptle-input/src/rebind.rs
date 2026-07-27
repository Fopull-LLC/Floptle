//! Press-to-bind capture.
//!
//! One code path serves both the editor's `[+]` binding chips and a shipped
//! game's settings menu: arm a capture with a [`BindFilter`], then feed it each
//! window's [`RawInput`] until it returns a [`Capture`].
//!
//! Escape is deliberately never captured, so "press the input you want" always
//! has a way out.

use crate::raw::RawInput;
use crate::source::{Device, Key, MouseAxis, MouseButton, PadAxis, PadButton, PadControl, PadId, Source};

/// How far an analog control must travel to count as a deliberate press.
///
/// Much higher than any binding deadzone: a rebind prompt that accepted 0.2
/// would grab a worn stick's drift the moment it was armed, and the player
/// would never see the prompt.
pub const CAPTURE_THRESHOLD: f32 = 0.7;

/// Restricts what a capture will accept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BindFilter {
    /// Any button-like input on any device.
    #[default]
    AnyButton,
    KeyboardOnly,
    PadOnly,
    /// Sticks, triggers and mouse motion only — for binding an axis.
    AxisOnly,
}

impl BindFilter {
    fn accepts(self, src: Source) -> bool {
        match self {
            BindFilter::AnyButton => !src.is_analog(),
            BindFilter::KeyboardOnly => src.device() == Device::Keyboard,
            BindFilter::PadOnly => src.device() == Device::Pad,
            BindFilter::AxisOnly => src.is_analog(),
        }
    }
}

/// What a capture produced: the bound source plus any modifiers held with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Capture {
    pub source: Source,
    /// Modifier keys held at the moment of capture — this is what turns a plain
    /// `S` press into a `Ctrl+S` chord without a separate UI for it.
    pub modifiers: Vec<Source>,
}

impl Capture {
    /// Into a map binding.
    pub fn binding(self) -> crate::map::Binding {
        crate::map::Binding::with_modifiers(self.source, self.modifiers)
    }
}

/// Try to read a binding out of this window's input.
///
/// `slot` is the player being rebound: it decides which pad an `Any` binding
/// reads, and captured pad sources are pinned to `PadId::Slot(slot)` when the
/// project has more than one player, so P2's rebind can't steal P1's pad.
pub fn capture(raw: &RawInput, filter: BindFilter, slot: u8, multiplayer: bool) -> Option<Capture> {
    let pad_id = if multiplayer { PadId::Slot(slot) } else { PadId::Any };

    if filter == BindFilter::AxisOnly {
        return capture_axis(raw, slot, pad_id).map(|source| Capture { source, modifiers: vec![] });
    }

    // Digital: prefer this window's banked edges (they survive a press that
    // ended between ticks), then fall back to whatever is held.
    let candidate = digital_candidates(raw, pad_id)
        .into_iter()
        .find(|s| filter.accepts(*s) && !is_cancel(*s) && !is_modifier(*s));

    let source = candidate?;
    Some(Capture { source, modifiers: held_modifiers(raw) })
}

/// Every digital source that looks pressed this window, edges first.
fn digital_candidates(raw: &RawInput, pad_id: PadId) -> Vec<Source> {
    let mut out: Vec<Source> = raw.pressed.iter().copied().filter(|s| !s.is_analog()).collect();
    // Deterministic order — a HashSet would otherwise pick a different key each
    // time two go down on the same window.
    out.sort_by_key(|s| format!("{s:?}"));

    for &k in Key::ALL {
        if raw.keys.contains(&k) {
            out.push(Source::Key(k));
        }
    }
    for &b in MouseButton::ALL {
        if raw.mouse_buttons[b.index()] {
            out.push(Source::Mouse(b));
        }
    }
    for &b in PadButton::ALL {
        let src = Source::Pad { id: pad_id, ctrl: PadControl::Button(b) };
        if raw.held(src, slot_of(pad_id), 0.5) {
            out.push(src);
        }
    }
    out
}

/// An analog control pushed past [`CAPTURE_THRESHOLD`].
fn capture_axis(raw: &RawInput, slot: u8, pad_id: PadId) -> Option<Source> {
    let mut best: Option<(f32, Source)> = None;
    let mut consider = |v: f32, src: Source| {
        if v.abs() >= CAPTURE_THRESHOLD && best.as_ref().is_none_or(|(b, _)| v.abs() > *b) {
            best = Some((v.abs(), src));
        }
    };
    for &a in PadAxis::ALL {
        let src = Source::Pad { id: pad_id, ctrl: PadControl::Axis(a) };
        consider(raw.value(src, slot), src);
    }
    for &a in MouseAxis::ALL {
        consider(raw.value(Source::MouseAxis(a), slot), Source::MouseAxis(a));
    }
    best.map(|(_, s)| s)
}

fn slot_of(id: PadId) -> u8 {
    match id {
        PadId::Slot(n) => n,
        PadId::Any => 0,
    }
}

/// Escape always cancels rather than binding — otherwise a player who opens the
/// rebind prompt by mistake has no way back out.
fn is_cancel(src: Source) -> bool {
    matches!(src, Source::Key(Key::Escape))
}

fn is_modifier(src: Source) -> bool {
    matches!(src, Source::Key(k) if k.is_modifier())
}

fn held_modifiers(raw: &RawInput) -> Vec<Source> {
    Key::ALL
        .iter()
        .copied()
        .filter(|k| k.is_modifier() && raw.keys.contains(k))
        .map(Source::Key)
        // Collapse L/R pairs: binding "L-Ctrl+S" then pressing R-Ctrl should
        // still work, so we keep only the first of each script name.
        .fold(Vec::new(), |mut acc, s| {
            let name = match s {
                Source::Key(k) => k.script_name(),
                _ => "",
            };
            if !acc.iter().any(|e| matches!(e, Source::Key(k) if k.script_name() == name)) {
                acc.push(s);
            }
            acc
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::PadState;

    fn keys(k: &[Key]) -> RawInput {
        RawInput { keys: k.iter().copied().collect(), ..Default::default() }
    }

    #[test]
    fn a_key_press_is_captured() {
        let c = capture(&keys(&[Key::KeyJ]), BindFilter::AnyButton, 0, false).unwrap();
        assert_eq!(c.source, Source::Key(Key::KeyJ));
        assert!(c.modifiers.is_empty());
    }

    #[test]
    fn nothing_pressed_captures_nothing() {
        assert!(capture(&RawInput::default(), BindFilter::AnyButton, 0, false).is_none());
    }

    #[test]
    fn escape_cancels_rather_than_binding() {
        assert!(capture(&keys(&[Key::Escape]), BindFilter::AnyButton, 0, false).is_none());
    }

    #[test]
    fn a_bare_modifier_is_not_captured_but_rides_along_as_a_chord() {
        // Holding Ctrl to reach a key must not bind Ctrl itself…
        assert!(capture(&keys(&[Key::ControlLeft]), BindFilter::AnyButton, 0, false).is_none());
        // …and pressing S while holding it produces the chord.
        let c = capture(&keys(&[Key::ControlLeft, Key::KeyS]), BindFilter::AnyButton, 0, false)
            .unwrap();
        assert_eq!(c.source, Source::Key(Key::KeyS));
        assert_eq!(c.modifiers, vec![Source::Key(Key::ControlLeft)]);
        assert_eq!(c.binding().chip(), "⌨ L-Ctrl+S");
    }

    #[test]
    fn filters_reject_the_wrong_device() {
        let mut raw = keys(&[Key::KeyJ]);
        assert!(capture(&raw, BindFilter::PadOnly, 0, false).is_none());
        assert!(capture(&raw, BindFilter::KeyboardOnly, 0, false).is_some());

        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).buttons.insert(PadButton::West);
        let c = capture(&raw, BindFilter::PadOnly, 0, false).unwrap();
        assert_eq!(
            c.source,
            Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::West) }
        );
    }

    #[test]
    fn stick_drift_never_satisfies_a_button_prompt() {
        // The bug this guards: arm "press a button", a worn stick sitting at
        // 0.2 immediately answers it and the player never sees the prompt.
        let mut axes = [0.0; PadAxis::COUNT];
        axes[PadAxis::LeftStickX.index()] = 0.2;
        let raw = RawInput {
            pads: vec![PadState { connected: true, buttons: Default::default(), axes }],
            ..Default::default()
        };
        assert!(capture(&raw, BindFilter::AnyButton, 0, false).is_none());
        assert!(capture(&raw, BindFilter::AxisOnly, 0, false).is_none(), "under the threshold");
    }

    #[test]
    fn a_decisive_stick_push_binds_an_axis() {
        let mut axes = [0.0; PadAxis::COUNT];
        axes[PadAxis::RightStickY.index()] = -0.9;
        let raw = RawInput {
            pads: vec![PadState { connected: true, buttons: Default::default(), axes }],
            ..Default::default()
        };
        let c = capture(&raw, BindFilter::AxisOnly, 0, false).unwrap();
        assert_eq!(
            c.source,
            Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::RightStickY) }
        );
    }

    #[test]
    fn the_strongest_axis_wins_when_several_move() {
        let mut axes = [0.0; PadAxis::COUNT];
        axes[PadAxis::LeftStickX.index()] = 0.75;
        axes[PadAxis::RightStickX.index()] = 0.98;
        let raw = RawInput {
            pads: vec![PadState { connected: true, buttons: Default::default(), axes }],
            ..Default::default()
        };
        let c = capture(&raw, BindFilter::AxisOnly, 0, false).unwrap();
        assert_eq!(
            c.source,
            Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::RightStickX) }
        );
    }

    #[test]
    fn multiplayer_pins_a_captured_pad_to_the_player_being_bound() {
        let mut raw = RawInput::default();
        raw.pad_mut(1).connected = true;
        raw.pad_mut(1).buttons.insert(PadButton::South);
        let c = capture(&raw, BindFilter::PadOnly, 1, true).unwrap();
        assert_eq!(
            c.source,
            Source::Pad { id: PadId::Slot(1), ctrl: PadControl::Button(PadButton::South) },
            "P2's binding must not read P1's pad"
        );
    }

    #[test]
    fn a_banked_edge_is_captured_even_if_already_released() {
        // The pad pump banks edges; a quick tap must still bind.
        let raw = RawInput {
            pressed: [Source::Key(Key::KeyK)].into_iter().collect(),
            ..Default::default()
        };
        let c = capture(&raw, BindFilter::AnyButton, 0, false).unwrap();
        assert_eq!(c.source, Source::Key(Key::KeyK));
    }

    #[test]
    fn capture_is_deterministic_when_two_keys_land_together() {
        let raw = RawInput {
            pressed: [Source::Key(Key::KeyA), Source::Key(Key::KeyB)].into_iter().collect(),
            ..Default::default()
        };
        let first = capture(&raw, BindFilter::AnyButton, 0, false).unwrap();
        for _ in 0..20 {
            assert_eq!(capture(&raw, BindFilter::AnyButton, 0, false).unwrap(), first);
        }
    }
}
