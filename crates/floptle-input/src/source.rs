//! Bindable input **sources** — the physical half of a binding.
//!
//! One flat [`Source`] enum covers every device. "All key types / all mouse
//! buttons" is the point: we mirror winit's full physical-key set rather than
//! curating a subset, so a rebind prompt can capture literally anything the
//! player presses.
//!
//! Nothing here depends on `winit` or `gilrs`. The editor and runtime translate
//! their device types into these at the boundary (`Key::from_script_name`, plus
//! a `KeyCode` match on their side), which keeps the whole resolution path
//! testable with no window and no GPU.

use serde::{Deserialize, Serialize};

/// Declare every bindable key once and derive the three names we need from it:
/// the serde/RON variant, the lowercase **script name** (what `input.key("w")`
/// has always used — kept byte-identical for the overlapping subset so raw-key
/// scripts and the action map speak the same language), and the pretty **label**
/// the editor prints on a binding chip.
macro_rules! keys {
    ($($variant:ident => $script:literal, $label:literal;)*) => {
        /// A physical key, layout-independent (the `W` *position*, not the
        /// character it produces on AZERTY).
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
        #[non_exhaustive]
        pub enum Key { $($variant),* }

        impl Key {
            /// Every key, in declaration order — the rebind UI's fallback picker.
            pub const ALL: &'static [Key] = &[$(Key::$variant),*];

            /// The lowercase name scripts use (`"w"`, `"space"`, `"shift"`).
            /// Left and right modifiers deliberately collapse onto one name,
            /// matching the long-standing `input.key("shift")` behaviour.
            pub fn script_name(self) -> &'static str {
                match self { $(Key::$variant => $script),* }
            }

            /// The label a binding chip shows (`"A"`, `"Space"`, `"L-Shift"`).
            pub fn label(self) -> &'static str {
                match self { $(Key::$variant => $label),* }
            }
        }
    };
}

keys! {
    KeyA => "a", "A"; KeyB => "b", "B"; KeyC => "c", "C"; KeyD => "d", "D";
    KeyE => "e", "E"; KeyF => "f", "F"; KeyG => "g", "G"; KeyH => "h", "H";
    KeyI => "i", "I"; KeyJ => "j", "J"; KeyK => "k", "K"; KeyL => "l", "L";
    KeyM => "m", "M"; KeyN => "n", "N"; KeyO => "o", "O"; KeyP => "p", "P";
    KeyQ => "q", "Q"; KeyR => "r", "R"; KeyS => "s", "S"; KeyT => "t", "T";
    KeyU => "u", "U"; KeyV => "v", "V"; KeyW => "w", "W"; KeyX => "x", "X";
    KeyY => "y", "Y"; KeyZ => "z", "Z";

    Digit0 => "0", "0"; Digit1 => "1", "1"; Digit2 => "2", "2"; Digit3 => "3", "3";
    Digit4 => "4", "4"; Digit5 => "5", "5"; Digit6 => "6", "6"; Digit7 => "7", "7";
    Digit8 => "8", "8"; Digit9 => "9", "9";

    F1 => "f1", "F1"; F2 => "f2", "F2"; F3 => "f3", "F3"; F4 => "f4", "F4";
    F5 => "f5", "F5"; F6 => "f6", "F6"; F7 => "f7", "F7"; F8 => "f8", "F8";
    F9 => "f9", "F9"; F10 => "f10", "F10"; F11 => "f11", "F11"; F12 => "f12", "F12";

    Space => "space", "Space";
    Enter => "enter", "Enter";
    Escape => "escape", "Esc";
    Tab => "tab", "Tab";
    Backspace => "backspace", "Backspace";
    Delete => "delete", "Delete";
    Insert => "insert", "Insert";
    Home => "home", "Home";
    End => "end", "End";
    PageUp => "pageup", "PgUp";
    PageDown => "pagedown", "PgDn";

    ShiftLeft => "shift", "L-Shift";
    ShiftRight => "shift", "R-Shift";
    ControlLeft => "ctrl", "L-Ctrl";
    ControlRight => "ctrl", "R-Ctrl";
    AltLeft => "alt", "L-Alt";
    AltRight => "alt", "R-Alt";
    SuperLeft => "super", "L-Super";
    SuperRight => "super", "R-Super";
    CapsLock => "capslock", "Caps";

    ArrowLeft => "left", "←";
    ArrowRight => "right", "→";
    ArrowUp => "up", "↑";
    ArrowDown => "down", "↓";

    Comma => ",", ","; Period => ".", "."; Slash => "/", "/";
    Semicolon => ";", ";"; Quote => "'", "'"; Backquote => "`", "`";
    BracketLeft => "[", "["; BracketRight => "]", "]"; Backslash => "\\", "\\";
    Minus => "-", "-"; Equal => "=", "=";

    Numpad0 => "num0", "Num0"; Numpad1 => "num1", "Num1"; Numpad2 => "num2", "Num2";
    Numpad3 => "num3", "Num3"; Numpad4 => "num4", "Num4"; Numpad5 => "num5", "Num5";
    Numpad6 => "num6", "Num6"; Numpad7 => "num7", "Num7"; Numpad8 => "num8", "Num8";
    Numpad9 => "num9", "Num9";
    NumpadAdd => "num+", "Num+"; NumpadSubtract => "num-", "Num-";
    NumpadMultiply => "num*", "Num*"; NumpadDivide => "num/", "Num/";
    NumpadDecimal => "num.", "Num.";
}

/// A grouping for the manual key picker — a flat list of ~100 keys is a menu
/// nobody can use.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyGroup {
    Letters,
    Digits,
    Function,
    Navigation,
    Modifiers,
    Numpad,
    Symbols,
}

impl KeyGroup {
    pub const ALL: &'static [KeyGroup] = &[
        KeyGroup::Letters,
        KeyGroup::Digits,
        KeyGroup::Function,
        KeyGroup::Navigation,
        KeyGroup::Modifiers,
        KeyGroup::Numpad,
        KeyGroup::Symbols,
    ];

    pub fn label(self) -> &'static str {
        match self {
            KeyGroup::Letters => "Letters",
            KeyGroup::Digits => "Digits",
            KeyGroup::Function => "Function keys",
            KeyGroup::Navigation => "Navigation",
            KeyGroup::Modifiers => "Modifiers",
            KeyGroup::Numpad => "Numpad",
            KeyGroup::Symbols => "Symbols",
        }
    }

    /// The keys in this group, in declaration order.
    pub fn keys(self) -> impl Iterator<Item = Key> {
        Key::ALL.iter().copied().filter(move |k| k.group() == self)
    }
}

impl Key {
    /// Which picker submenu this key belongs in.
    pub fn group(self) -> KeyGroup {
        use Key::*;
        match self {
            KeyA | KeyB | KeyC | KeyD | KeyE | KeyF | KeyG | KeyH | KeyI | KeyJ | KeyK | KeyL
            | KeyM | KeyN | KeyO | KeyP | KeyQ | KeyR | KeyS | KeyT | KeyU | KeyV | KeyW | KeyX
            | KeyY | KeyZ => KeyGroup::Letters,
            Digit0 | Digit1 | Digit2 | Digit3 | Digit4 | Digit5 | Digit6 | Digit7 | Digit8
            | Digit9 => KeyGroup::Digits,
            F1 | F2 | F3 | F4 | F5 | F6 | F7 | F8 | F9 | F10 | F11 | F12 => KeyGroup::Function,
            Space | Enter | Escape | Tab | Backspace | Delete | Insert | Home | End | PageUp
            | PageDown | ArrowLeft | ArrowRight | ArrowUp | ArrowDown => KeyGroup::Navigation,
            ShiftLeft | ShiftRight | ControlLeft | ControlRight | AltLeft | AltRight
            | SuperLeft | SuperRight | CapsLock => KeyGroup::Modifiers,
            Numpad0 | Numpad1 | Numpad2 | Numpad3 | Numpad4 | Numpad5 | Numpad6 | Numpad7
            | Numpad8 | Numpad9 | NumpadAdd | NumpadSubtract | NumpadMultiply | NumpadDivide
            | NumpadDecimal => KeyGroup::Numpad,
            _ => KeyGroup::Symbols,
        }
    }

    /// The first key whose script name matches — the inverse of
    /// [`Key::script_name`] for the unambiguous majority, and the LEFT variant
    /// for collapsed modifiers (`"shift"` → `ShiftLeft`). Case-insensitive so
    /// a hand-edited RON or a script string can say `"Space"` or `"space"`.
    pub fn from_script_name(name: &str) -> Option<Key> {
        let lower = name.to_ascii_lowercase();
        Key::ALL.iter().copied().find(|k| k.script_name() == lower)
    }

    /// True for keys that only ever act as chord modifiers — a rebind prompt
    /// filters them out of a plain "press a button" capture so holding Shift to
    /// reach a key doesn't bind Shift itself.
    pub fn is_modifier(self) -> bool {
        matches!(
            self,
            Key::ShiftLeft
                | Key::ShiftRight
                | Key::ControlLeft
                | Key::ControlRight
                | Key::AltLeft
                | Key::AltRight
                | Key::SuperLeft
                | Key::SuperRight
        )
    }
}

/// A mouse button. 0/1/2 keep the meaning the script API has always had
/// (`input.button(0)` = left) so the two layers never disagree.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

impl MouseButton {
    pub const ALL: &'static [MouseButton] = &[
        MouseButton::Left,
        MouseButton::Right,
        MouseButton::Middle,
        MouseButton::Back,
        MouseButton::Forward,
    ];

    /// Index into [`crate::RawInput::mouse_buttons`].
    pub fn index(self) -> usize {
        match self {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            MouseButton::Back => 3,
            MouseButton::Forward => 4,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MouseButton::Left => "LMB",
            MouseButton::Right => "RMB",
            MouseButton::Middle => "MMB",
            MouseButton::Back => "Mouse4",
            MouseButton::Forward => "Mouse5",
        }
    }
}

/// Relative mouse movement, bindable exactly like a stick axis — this is what
/// lets one "Look" axis accept both a mouse and a right stick.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum MouseAxis {
    MotionX,
    MotionY,
    ScrollX,
    ScrollY,
}

impl MouseAxis {
    pub const ALL: &'static [MouseAxis] =
        &[MouseAxis::MotionX, MouseAxis::MotionY, MouseAxis::ScrollX, MouseAxis::ScrollY];

    pub fn label(self) -> &'static str {
        match self {
            MouseAxis::MotionX => "Mouse X",
            MouseAxis::MotionY => "Mouse Y",
            MouseAxis::ScrollX => "Wheel X",
            MouseAxis::ScrollY => "Wheel",
        }
    }
}

/// A gamepad button, in the layout-neutral naming `gilrs` uses: `South` is
/// A on Xbox and ✕ on PlayStation, so a binding travels across pad brands.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadButton {
    South,
    East,
    West,
    North,
    LeftBumper,
    RightBumper,
    LeftTrigger,
    RightTrigger,
    Select,
    Start,
    Mode,
    LeftStick,
    RightStick,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

impl PadButton {
    /// Look a button up by its variant name ("South", "leftbumper"), case-
    /// insensitively — what a script names it. floptle/0047.
    pub fn from_name(name: &str) -> Option<PadButton> {
        Self::ALL.iter().copied().find(|b| format!("{b:?}").eq_ignore_ascii_case(name))
    }

    pub const ALL: &'static [PadButton] = &[
        PadButton::South,
        PadButton::East,
        PadButton::West,
        PadButton::North,
        PadButton::LeftBumper,
        PadButton::RightBumper,
        PadButton::LeftTrigger,
        PadButton::RightTrigger,
        PadButton::Select,
        PadButton::Start,
        PadButton::Mode,
        PadButton::LeftStick,
        PadButton::RightStick,
        PadButton::DPadUp,
        PadButton::DPadDown,
        PadButton::DPadLeft,
        PadButton::DPadRight,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PadButton::South => "South",
            PadButton::East => "East",
            PadButton::West => "West",
            PadButton::North => "North",
            PadButton::LeftBumper => "L1",
            PadButton::RightBumper => "R1",
            PadButton::LeftTrigger => "L2",
            PadButton::RightTrigger => "R2",
            PadButton::Select => "Select",
            PadButton::Start => "Start",
            PadButton::Mode => "Home",
            PadButton::LeftStick => "L3",
            PadButton::RightStick => "R3",
            PadButton::DPadUp => "D-Up",
            PadButton::DPadDown => "D-Down",
            PadButton::DPadLeft => "D-Left",
            PadButton::DPadRight => "D-Right",
        }
    }
}

/// A gamepad analog axis. Sticks read −1..1; triggers (`LeftZ`/`RightZ`) read
/// 0..1 on every backend we support, which is why a trigger bound to a *digital*
/// action compares against [`Binding::threshold`] rather than a sign.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftZ,
    RightZ,
}

impl PadAxis {
    /// Look an axis up by its variant name ("LeftStickX"), case-insensitively.
    /// floptle/0047.
    pub fn from_name(name: &str) -> Option<PadAxis> {
        Self::ALL.iter().copied().find(|a| format!("{a:?}").eq_ignore_ascii_case(name))
    }

    pub const ALL: &'static [PadAxis] = &[
        PadAxis::LeftStickX,
        PadAxis::LeftStickY,
        PadAxis::RightStickX,
        PadAxis::RightStickY,
        PadAxis::LeftZ,
        PadAxis::RightZ,
    ];

    /// Index into [`crate::PadState::axes`].
    pub fn index(self) -> usize {
        match self {
            PadAxis::LeftStickX => 0,
            PadAxis::LeftStickY => 1,
            PadAxis::RightStickX => 2,
            PadAxis::RightStickY => 3,
            PadAxis::LeftZ => 4,
            PadAxis::RightZ => 5,
        }
    }

    /// How many axes a [`crate::PadState`] stores.
    pub const COUNT: usize = 6;

    pub fn label(self) -> &'static str {
        match self {
            PadAxis::LeftStickX => "L-Stick X",
            PadAxis::LeftStickY => "L-Stick Y",
            PadAxis::RightStickX => "R-Stick X",
            PadAxis::RightStickY => "R-Stick Y",
            PadAxis::LeftZ => "L-Trigger",
            PadAxis::RightZ => "R-Trigger",
        }
    }
}

/// What on a pad a binding points at.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum PadControl {
    Button(PadButton),
    Axis(PadAxis),
}

impl PadControl {
    pub fn label(self) -> &'static str {
        match self {
            PadControl::Button(b) => b.label(),
            PadControl::Axis(a) => a.label(),
        }
    }
}

/// Which physical pad a binding listens to.
///
/// `Any` is the single-player default — whichever pad is connected drives it.
/// `Slot(n)` is local multiplayer: slot 0 is player 1. Slots are assigned by
/// connection order and survive a replug, so P2 doesn't become P1 when P1's
/// battery dies mid-match.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub enum PadId {
    #[default]
    Any,
    Slot(u8),
}

impl PadId {
    /// True when this binding should read pad `slot`.
    pub fn matches(self, slot: u8) -> bool {
        match self {
            PadId::Any => true,
            PadId::Slot(n) => n == slot,
        }
    }
}

/// Anything bindable, flattened into one enum.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Source {
    Key(Key),
    Mouse(MouseButton),
    MouseAxis(MouseAxis),
    Pad { id: PadId, ctrl: PadControl },
}

/// Which family a source belongs to — drives the chip icon and the rebind
/// filter (`BindFilter::PadOnly` must not capture stray keyboard chatter).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Device {
    Keyboard,
    Mouse,
    Pad,
}

impl Device {
    /// The glyph a binding chip leads with.
    ///
    /// These have to survive the editor's font stack, which bundles only a
    /// SUBSET of the emoji block — a glyph it lacks renders as a tofu square.
    /// `🎮` is one of the missing ones, so a pad shows as an analog stick.
    /// `floptle-editor`'s `icons` module has a test that catches regressions
    /// here; keep the two in step.
    pub fn icon(self) -> &'static str {
        match self {
            Device::Keyboard => "⌨",
            Device::Mouse => "🖱",
            Device::Pad => "◉",
        }
    }
}

impl Source {
    pub fn device(self) -> Device {
        match self {
            Source::Key(_) => Device::Keyboard,
            Source::Mouse(_) | Source::MouseAxis(_) => Device::Mouse,
            Source::Pad { .. } => Device::Pad,
        }
    }

    /// True when this source produces a continuous value rather than a
    /// press — bound to a digital action it compares against a threshold.
    pub fn is_analog(self) -> bool {
        matches!(self, Source::MouseAxis(_) | Source::Pad { ctrl: PadControl::Axis(_), .. })
    }

    /// The chip's text, icon excluded (`"Space"`, `"South"`, `"P2 R2"`).
    pub fn label(self) -> String {
        match self {
            Source::Key(k) => k.label().to_string(),
            Source::Mouse(b) => b.label().to_string(),
            Source::MouseAxis(a) => a.label().to_string(),
            Source::Pad { id, ctrl } => match id {
                PadId::Any => ctrl.label().to_string(),
                PadId::Slot(n) => format!("P{} {}", n + 1, ctrl.label()),
            },
        }
    }

    /// Icon + label, ready to print (`"⌨ Space"`).
    pub fn chip(self) -> String {
        format!("{} {}", self.device().icon(), self.label())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_names_round_trip() {
        // Every key resolves from its own script name. Collapsed modifiers land
        // on the LEFT variant by design, so assert on the NAME, not the variant.
        for &k in Key::ALL {
            let back = Key::from_script_name(k.script_name()).expect("name resolves");
            assert_eq!(back.script_name(), k.script_name(), "{k:?}");
        }
    }

    #[test]
    fn script_names_match_the_legacy_raw_key_table() {
        // These are the exact strings `input.key(...)` has always accepted; the
        // action map must not quietly rename them out from under old scripts.
        for (name, want) in [
            ("w", Key::KeyW),
            ("space", Key::Space),
            ("escape", Key::Escape),
            ("shift", Key::ShiftLeft),
            ("ctrl", Key::ControlLeft),
            ("alt", Key::AltLeft),
            ("left", Key::ArrowLeft),
            (",", Key::Comma),
            (".", Key::Period),
        ] {
            assert_eq!(Key::from_script_name(name), Some(want), "{name}");
        }
    }

    #[test]
    fn unknown_key_name_is_none() {
        assert_eq!(Key::from_script_name("banana"), None);
    }

    #[test]
    fn pad_slots_gate_bindings() {
        assert!(PadId::Any.matches(3));
        assert!(PadId::Slot(1).matches(1));
        assert!(!PadId::Slot(1).matches(0));
    }

    #[test]
    fn analog_sources_are_flagged() {
        assert!(Source::MouseAxis(MouseAxis::MotionX).is_analog());
        assert!(
            Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::RightZ) }.is_analog()
        );
        assert!(!Source::Key(Key::Space).is_analog());
        assert!(
            !Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::South) }.is_analog()
        );
    }

    #[test]
    fn every_key_lands_in_exactly_one_picker_group() {
        // A key with no group would be unreachable from the manual picker —
        // i.e. unbindable without the hardware, which is the whole point.
        let mut seen = 0;
        for g in KeyGroup::ALL {
            seen += g.keys().count();
        }
        assert_eq!(seen, Key::ALL.len(), "every key appears exactly once");
    }

    #[test]
    fn picker_groups_are_not_empty() {
        for g in KeyGroup::ALL {
            assert!(g.keys().next().is_some(), "{:?} is empty", g.label());
        }
    }

    #[test]
    fn chips_name_the_player_for_slot_bindings() {
        let s = Source::Pad {
            id: PadId::Slot(1),
            ctrl: PadControl::Button(PadButton::RightTrigger),
        };
        assert_eq!(s.chip(), format!("{} P2 R2", Device::Pad.icon()));
    }
}
