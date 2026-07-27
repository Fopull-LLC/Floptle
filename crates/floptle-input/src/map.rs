//! The **action map** — the project's whole public input model, as RON.
//!
//! Digital [`Action`]s and analog [`Axis1`]/[`Axis2`]es, each fed by a list of
//! bindings. Any binding fires its action (an OR), which is exactly how one
//! "Jump" answers to both Space and a pad's South button, and how one "Move"
//! answers to both WASD and the left stick.
//!
//! Lives at `<project>/input.ron` — its own file rather than a `project.ron`
//! field, because it's the one project asset a shipped settings menu overlays
//! at runtime, and rebinds stay diffable that way.

use serde::{Deserialize, Serialize};

use crate::source::{Key, MouseAxis, PadAxis, PadId, Source};

/// The wire format packs held/pressed/released into `u64` bitmasks, so an
/// action's index must fit. The editor refuses to add the 65th.
pub const MAX_ACTIONS: usize = 64;

/// The default analog threshold at which a trigger or stick "counts as pressed"
/// for a digital action.
pub const DEFAULT_THRESHOLD: f32 = 0.5;

fn default_threshold() -> f32 {
    DEFAULT_THRESHOLD
}
fn default_sensitivity() -> f32 {
    1.0
}
fn default_deadzone() -> f32 {
    0.15
}
fn default_players() -> u8 {
    1
}
fn is_default_threshold(v: &f32) -> bool {
    (*v - DEFAULT_THRESHOLD).abs() < f32::EPSILON
}
fn is_one(v: &f32) -> bool {
    (*v - 1.0).abs() < f32::EPSILON
}
fn is_false(v: &bool) -> bool {
    !*v
}
fn default_true() -> bool {
    true
}
fn is_true(v: &bool) -> bool {
    *v
}

/// One physical source, optionally gated behind held modifiers (a chord).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub source: Source,
    /// All of these must be held for the binding to count — `[Key(ControlLeft)]`
    /// makes `Ctrl+S`. Empty for the overwhelming majority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<Source>,
    /// Where an analog source starts counting as pressed.
    #[serde(default = "default_threshold", skip_serializing_if = "is_default_threshold")]
    pub threshold: f32,
    /// Restrict this binding to ONE local player slot (0-based). `None` — the
    /// overwhelming default — means every slot, which is right for a pad binding
    /// (`PadId::Any` already resolves per slot) and for single-player.
    ///
    /// It exists for the case a pad can't cover: **two players sharing one keyboard**.
    /// There is only one keyboard, so `Key(KeyJ)` otherwise fires `Light` for both
    /// fighters at once, and the only way out was to duplicate the whole action set
    /// under `Light2` names. Scoping the BINDING rather than the action keeps the action
    /// list — and therefore the netcode's positional indexing and [`InputMap::hash`] —
    /// untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<u8>,
}

impl Binding {
    pub fn new(source: Source) -> Self {
        Self { source, modifiers: Vec::new(), threshold: DEFAULT_THRESHOLD, player: None }
    }

    pub fn with_modifiers(source: Source, modifiers: Vec<Source>) -> Self {
        Self { source, modifiers, threshold: DEFAULT_THRESHOLD, player: None }
    }

    /// This binding, restricted to one local player slot. See [`Binding::player`].
    pub fn for_player(mut self, slot: u8) -> Self {
        self.player = Some(slot);
        self
    }

    /// Whether this binding contributes for `slot`.
    pub fn serves(&self, slot: u8) -> bool {
        self.player.is_none_or(|p| p == slot)
    }

    /// Chip text including the chord (`"⌨ Ctrl+S"`).
    pub fn chip(&self) -> String {
        if self.modifiers.is_empty() {
            return self.source.chip();
        }
        let mods: Vec<String> = self.modifiers.iter().map(|m| m.label()).collect();
        format!("{} {}+{}", self.source.device().icon(), mods.join("+"), self.source.label())
    }
}

/// A digital action: pressed / held / released, plus how long it's been down.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Binding>,
}

impl Action {
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into(), bindings: Vec::new() }
    }
}

/// How an axis resolves opposing directions being held at once — a real
/// fighting-game concern (a leverless controller can trivially hold ← and →).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Socd {
    /// Both held cancels to 0 — the tournament-standard resolution.
    #[default]
    Neutral,
    /// The most recently pressed direction wins; releasing it falls back to the
    /// one still held. This is what lets a player pivot without a neutral frame.
    LastWins,
    /// Up/right always beats down/left.
    Positive,
    /// Down/left always beats up/right.
    Negative,
}

/// The response curve applied to an analog magnitude after the deadzone.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Curve {
    #[default]
    Linear,
    /// Squared — finer control near centre, for aim.
    Expo,
}

impl Curve {
    /// Shape a 0..1 magnitude that has already had its deadzone removed.
    pub fn apply(self, m: f32) -> f32 {
        match self {
            Curve::Linear => m,
            Curve::Expo => m * m,
        }
    }
}

/// One contributor to a 1D axis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Axis1Binding {
    /// Two digital sources forming −1 / +1.
    Keys {
        minus: Source,
        plus: Source,
        /// Restrict to one local player slot — see [`Binding::player`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<u8>,
    },
    /// A single analog source passed through deadzone → curve → sensitivity.
    Analog {
        source: Source,
        #[serde(default = "default_deadzone")]
        deadzone: f32,
        #[serde(default = "default_sensitivity", skip_serializing_if = "is_one")]
        sensitivity: f32,
        #[serde(default, skip_serializing_if = "is_false")]
        invert: bool,
        #[serde(default)]
        curve: Curve,
        /// Contribute only while all of these are held. See [`Axis2Binding::Mouse`].
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gate: Vec<Source>,
    },
}

/// One contributor to a 2D axis.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Axis2Binding {
    /// WASD-style: four digital sources. Scoping these to a slot is what lets ONE
    /// `Move` axis carry WASD for player 1 and the arrow keys for player 2 — which in
    /// turn makes the map-level motion axis (`dir()`, `qcf`, …) correct for both.
    Keys {
        up: Source,
        down: Source,
        left: Source,
        right: Source,
        /// Restrict to one local player slot — see [`Binding::player`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        player: Option<u8>,
    },
    /// A gamepad stick, deadzoned radially.
    Stick {
        id: PadId,
        x: PadAxis,
        y: PadAxis,
        #[serde(default = "default_deadzone")]
        deadzone: f32,
        #[serde(default = "default_sensitivity", skip_serializing_if = "is_one")]
        sensitivity: f32,
        #[serde(default, skip_serializing_if = "is_false")]
        invert_y: bool,
        #[serde(default)]
        curve: Curve,
    },
    /// Relative mouse motion.
    Mouse {
        #[serde(default = "default_sensitivity", skip_serializing_if = "is_one")]
        sensitivity: f32,
        #[serde(default, skip_serializing_if = "is_false")]
        invert_y: bool,
        /// Report **pixels per second** rather than pixels-this-frame (default).
        ///
        /// This is what makes a mouse and a stick composable on one axis at
        /// all. A stick reports a *position* the game integrates into a turn
        /// rate; a mouse reports a *displacement* that is already the turn.
        /// Dividing the displacement by the frame time turns it into a rate
        /// too, so a script can write `yaw = yaw - lookX * dt` once and have it
        /// be correct — and frame-rate independent — on both devices.
        ///
        /// `false` gives raw per-frame pixels, for a script doing its own thing.
        #[serde(default = "default_true", skip_serializing_if = "is_true")]
        rate: bool,
        /// Contribute only while all of these are held — typically the right
        /// mouse button, for a "hold to look" camera.
        ///
        /// This is what lets ONE `Look` axis serve both devices honestly: the
        /// mouse contributes only while you're dragging (so the view never
        /// spins on its own with a free cursor), while a right-stick binding on
        /// the same axis stays live at all times, because a stick already
        /// returns to centre by itself. Without the gate a script would have to
        /// ask which device it was on, which is exactly what the action layer
        /// exists to avoid.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        gate: Vec<Source>,
    },
}

/// A named 1D analog axis (triggers, wheel, a key pair).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Axis1 {
    pub name: String,
    #[serde(default)]
    pub socd: Socd,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Axis1Binding>,
}

/// A named 2D analog axis (a stick, WASD, mouse motion).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Axis2 {
    pub name: String,
    #[serde(default)]
    pub socd: Socd,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bindings: Vec<Axis2Binding>,
}

/// A fighting-game motion: a sequence of numpad directions that must all occur,
/// in order, within `window` ticks.
///
/// Numpad notation is the genre's lingua franca — 5 is neutral, 6 is forward,
/// 2 is down, so a quarter-circle-forward is `[2, 3, 6]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Motion {
    pub name: String,
    /// Directions in numpad notation, oldest first.
    pub dirs: Vec<u8>,
    /// How many ticks the whole sequence may span.
    pub window: u16,
    /// Optional charge: the FIRST direction must be held this many ticks before
    /// the rest of the sequence counts (a Guile-style charge move).
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub charge: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

/// The whole project input map.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputMap {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub axes1: Vec<Axis1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub axes2: Vec<Axis2>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub motions: Vec<Motion>,
    /// How many local players the project supports — sizes the per-slot runtimes.
    #[serde(default = "default_players")]
    pub players: u8,
    /// Which 2D axis feeds [`crate::history`]'s numpad direction (motion inputs).
    /// Defaults to `"Move"` when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub motion_axis: Option<String>,
}

impl Default for InputMap {
    fn default() -> Self {
        Self {
            actions: Vec::new(),
            axes1: Vec::new(),
            axes2: Vec::new(),
            motions: Vec::new(),
            players: 1,
            motion_axis: None,
        }
    }
}

impl InputMap {
    pub fn action_index(&self, name: &str) -> Option<usize> {
        self.actions.iter().position(|a| a.name == name)
    }
    pub fn axis1_index(&self, name: &str) -> Option<usize> {
        self.axes1.iter().position(|a| a.name == name)
    }
    pub fn axis2_index(&self, name: &str) -> Option<usize> {
        self.axes2.iter().position(|a| a.name == name)
    }
    pub fn motion(&self, name: &str) -> Option<&Motion> {
        self.motions.iter().find(|m| m.name == name)
    }

    /// The 2D axis that drives numpad directions for motion inputs.
    pub fn motion_axis_index(&self) -> Option<usize> {
        self.axis2_index(self.motion_axis.as_deref().unwrap_or("Move"))
    }

    /// True when nothing at all is defined — the "fresh project" case the
    /// editor offers to seed.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.axes1.is_empty() && self.axes2.is_empty()
    }

    /// A stable fingerprint of the map's **shape** — the ordered names of every
    /// action and axis.
    ///
    /// The netcode wire indexes actions by declaration order, so a client and
    /// server running differently-ordered maps would decode each other's inputs
    /// as the wrong actions and desync silently. The session handshake compares
    /// this and refuses a mismatch. Bindings deliberately do NOT contribute —
    /// a player rebinding Jump to their own liking must not lock them out.
    pub fn hash(&self) -> u64 {
        // FNV-1a, spelled out so no dependency (and no hasher-version drift)
        // can change the value between builds.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for a in &self.actions {
            eat(b"A");
            eat(a.name.as_bytes());
        }
        for a in &self.axes1 {
            eat(b"1");
            eat(a.name.as_bytes());
        }
        for a in &self.axes2 {
            eat(b"2");
            eat(a.name.as_bytes());
        }
        h
    }

    /// Fill in anything `other` defines that this map is missing — entries that
    /// don't exist at all, and bindings absent from entries that do.
    ///
    /// Nothing already here is changed or removed: your bindings, your SOCD
    /// choices and your own actions all survive. This is what "add the starter
    /// bindings" means on a map that isn't empty — the common case being an
    /// entry created from the *used in scripts, not in the map* warning, which
    /// lands **unbound** by design and would otherwise be a control that
    /// silently does nothing.
    ///
    /// Returns how many entries and bindings were added.
    pub fn merge_missing(&mut self, other: &InputMap) -> usize {
        let mut added = 0;

        for a in &other.actions {
            match self.actions.iter().position(|x| x.name == a.name) {
                Some(i) => {
                    for b in &a.bindings {
                        if !self.actions[i].bindings.contains(b) {
                            self.actions[i].bindings.push(b.clone());
                            added += 1;
                        }
                    }
                }
                // Silently stop at the cap rather than pushing a 65th that the
                // wire's bitmask could never address.
                None if self.actions.len() < MAX_ACTIONS => {
                    self.actions.push(a.clone());
                    added += 1;
                }
                None => {}
            }
        }

        for a in &other.axes1 {
            match self.axes1.iter_mut().find(|x| x.name == a.name) {
                Some(mine) => {
                    for b in &a.bindings {
                        if !mine.bindings.contains(b) {
                            mine.bindings.push(b.clone());
                            added += 1;
                        }
                    }
                }
                None => {
                    self.axes1.push(a.clone());
                    added += 1;
                }
            }
        }

        for a in &other.axes2 {
            match self.axes2.iter_mut().find(|x| x.name == a.name) {
                Some(mine) => {
                    for b in &a.bindings {
                        if !mine.bindings.contains(b) {
                            mine.bindings.push(b.clone());
                            added += 1;
                        }
                    }
                }
                None => {
                    self.axes2.push(a.clone());
                    added += 1;
                }
            }
        }

        for m in &other.motions {
            if self.motion(&m.name).is_none() {
                self.motions.push(m.clone());
                added += 1;
            }
        }

        added
    }

    /// Parse a map from RON text.
    pub fn parse(text: &str) -> Result<InputMap, ron::de::SpannedError> {
        ron::from_str(text)
    }

    /// Print the map as pretty RON, ready to write to `input.ron`.
    pub fn to_ron(&self) -> String {
        let cfg = ron::ser::PrettyConfig::new().struct_names(true).indentor("    ".to_string());
        // `to_string_pretty` only fails on types serde can't represent; ours are
        // all plain data, so a failure here is a bug, not a runtime condition.
        ron::ser::to_string_pretty(self, cfg).unwrap_or_else(|e| {
            log::error!("input map failed to serialize: {e}");
            String::new()
        })
    }

    /// The starting point a new project gets — and the map the shipped default
    /// scripts (`freelook`, `first_person`, `third_person`, …) are written
    /// against.
    ///
    /// **Every entry is bound on both a keyboard/mouse AND a gamepad**, so a
    /// fresh project plays with either, with both plugged in, or with a pad
    /// connected halfway through. That is the whole point: a script written
    /// against these names never asks which device it is on.
    pub fn starter() -> InputMap {
        use crate::source::{MouseButton, PadButton, PadControl};
        let key = |k: Key| Source::Key(k);
        let pad = |b: PadButton| Source::Pad { id: PadId::Any, ctrl: PadControl::Button(b) };
        let pad_axis = |a: PadAxis| Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(a) };
        let act = |name: &str, a: Source, b: Source| Action {
            name: name.into(),
            bindings: vec![Binding::new(a), Binding::new(b)],
        };

        InputMap {
            actions: vec![
                act("Jump", key(Key::Space), pad(PadButton::South)),
                act("Fire", Source::Mouse(MouseButton::Left), pad_axis(PadAxis::RightZ)),
                act("Interact", key(Key::KeyE), pad(PadButton::West)),
                act("Sprint", key(Key::ShiftLeft), pad(PadButton::LeftStick)),
                act("Crouch", key(Key::KeyC), pad(PadButton::East)),
                act("Pause", key(Key::Escape), pad(PadButton::Start)),
                // Hold-to-look, for the free cursor. Bound on the mouse only,
                // and deliberately so: a right stick recentres itself, so there
                // is nothing to gate, and the `Look` axis below leaves its stick
                // binding ungated. A pad therefore looks around at all times
                // while a mouse only does so while dragging.
                act(
                    "LookEnable",
                    Source::Mouse(MouseButton::Right),
                    pad(PadButton::RightStick),
                ),
                // Shift-lock / first-person toggle for the orbit camera.
                act("ShiftLock", key(Key::ShiftLeft), pad(PadButton::RightStick)),
            ],
            axes1: vec![
                Axis1 {
                    name: "Zoom".into(),
                    socd: Socd::Neutral,
                    bindings: vec![
                        Axis1Binding::Analog {
                            source: Source::MouseAxis(MouseAxis::ScrollY),
                            deadzone: 0.0,
                            sensitivity: 1.0,
                            invert: false,
                            curve: Curve::Linear,
                            gate: Vec::new(),
                        },
                        // The d-pad zooms on a pad — a wheel it doesn't have.
                        Axis1Binding::Keys {
                            minus: pad(PadButton::DPadDown),
                            plus: pad(PadButton::DPadUp),
                            player: None,
                        },
                    ],
                },
                // Vertical movement for a fly camera: keys, or the triggers.
                Axis1 {
                    name: "Fly".into(),
                    socd: Socd::Neutral,
                    bindings: vec![
                        Axis1Binding::Keys {
                            minus: key(Key::ControlLeft),
                            plus: key(Key::Space),
                            player: None,
                        },
                        Axis1Binding::Analog {
                            source: pad_axis(PadAxis::RightZ),
                            deadzone: 0.1,
                            sensitivity: 1.0,
                            invert: false,
                            curve: Curve::Linear,
                            gate: Vec::new(),
                        },
                        Axis1Binding::Analog {
                            source: pad_axis(PadAxis::LeftZ),
                            deadzone: 0.1,
                            sensitivity: 1.0,
                            invert: true,
                            curve: Curve::Linear,
                            gate: Vec::new(),
                        },
                    ],
                },
            ],
            axes2: vec![
                Axis2 {
                    name: "Move".into(),
                    socd: Socd::Neutral,
                    bindings: vec![
                        Axis2Binding::Keys {
                            up: key(Key::KeyW),
                            down: key(Key::KeyS),
                            left: key(Key::KeyA),
                            right: key(Key::KeyD),
                            player: None,
                        },
                        Axis2Binding::Stick {
                            id: PadId::Any,
                            x: PadAxis::LeftStickX,
                            y: PadAxis::LeftStickY,
                            deadzone: 0.15,
                            sensitivity: 1.0,
                            invert_y: false,
                            curve: Curve::Linear,
                        },
                    ],
                },
                Axis2 {
                    name: "Look".into(),
                    socd: Socd::Neutral,
                    bindings: vec![
                        // Gated on the right mouse button: a free cursor must
                        // never spin the view. The stick below is NOT gated.
                        // Sensitivity converts pixels-per-second into
                        // radians-per-second, so it lands in the same range as
                        // the stick below and a script's `* dt` is correct for
                        // both. ~300 px/s of drag ≈ 1.8 rad/s of turn.
                        Axis2Binding::Mouse {
                            sensitivity: 0.006,
                            invert_y: false,
                            gate: vec![Source::Mouse(MouseButton::Right)],
                            rate: true,
                        },
                        Axis2Binding::Stick {
                            id: PadId::Any,
                            x: PadAxis::RightStickX,
                            y: PadAxis::RightStickY,
                            deadzone: 0.12,
                            // Radians per second at full deflection.
                            sensitivity: 2.5,
                            invert_y: false,
                            curve: Curve::Expo,
                        },
                    ],
                },
                // The same look, minus the hold-to-drag gate — for when the
                // cursor is already captured (shift lock, first person) and
                // there is no free pointer left to protect. A camera reads
                // this instead of `Look` while it owns the cursor, which beats
                // reaching back to a raw mouse delta and re-deriving the rate.
                Axis2 {
                    name: "LookFree".into(),
                    socd: Socd::Neutral,
                    bindings: vec![
                        Axis2Binding::Mouse {
                            sensitivity: 0.006,
                            invert_y: false,
                            gate: Vec::new(),
                            rate: true,
                        },
                        Axis2Binding::Stick {
                            id: PadId::Any,
                            x: PadAxis::RightStickX,
                            y: PadAxis::RightStickY,
                            deadzone: 0.12,
                            sensitivity: 2.5,
                            invert_y: false,
                            curve: Curve::Expo,
                        },
                    ],
                },
            ],
            motions: Motion::standard(),
            players: 1,
            motion_axis: None,
        }
    }
}

impl Motion {
    /// The genre-standard motion vocabulary, seeded into every new map so a
    /// fighter can call `input.motion("qcf")` without authoring anything.
    pub fn standard() -> Vec<Motion> {
        let m = |name: &str, dirs: &[u8], window: u16| Motion {
            name: name.into(),
            dirs: dirs.to_vec(),
            window,
            charge: 0,
        };
        vec![
            m("qcf", &[2, 3, 6], 12),           // quarter-circle forward
            m("qcb", &[2, 1, 4], 12),           // quarter-circle back
            m("dp", &[6, 2, 3], 14),            // dragon punch
            m("rdp", &[4, 2, 1], 14),           // reverse dragon punch
            m("hcf", &[4, 1, 2, 3, 6], 22),     // half-circle forward
            m("hcb", &[6, 3, 2, 1, 4], 22),     // half-circle back
            m("dd", &[2, 5, 2], 16),            // double down
            m("ff", &[6, 5, 6], 14),            // forward dash
            m("bb", &[4, 5, 4], 14),            // back dash
            Motion { name: "chargeF".into(), dirs: vec![4, 6], window: 10, charge: 40 },
            Motion { name: "chargeU".into(), dirs: vec![2, 8], window: 10, charge: 40 },
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{PadButton, PadControl};

    #[test]
    fn starter_map_round_trips_through_ron() {
        let map = InputMap::starter();
        let text = map.to_ron();
        let back = InputMap::parse(&text).expect("starter map re-parses");
        assert_eq!(map, back);
    }

    /// Scoping a binding to a local player must NOT move the handshake hash — the wire
    /// indexes actions by position, and a per-player BINDING doesn't change the action
    /// list. Round-tripping it through RON matters for the same reason: a hand-edit that
    /// silently dropped `player` would put both fighters back on one set of keys.
    #[test]
    fn a_per_player_binding_round_trips_and_leaves_the_hash_alone() {
        let mut map = InputMap::starter();
        let before = map.hash();
        map.actions[0].bindings[0].player = Some(1);
        map.axes2[0].bindings[0] = match map.axes2[0].bindings[0].clone() {
            Axis2Binding::Keys { up, down, left, right, .. } => {
                Axis2Binding::Keys { up, down, left, right, player: Some(1) }
            }
            other => other,
        };
        assert_eq!(map.hash(), before, "a binding's player must not invalidate a session");

        let text = map.to_ron();
        assert!(text.contains("player: Some(1)"), "the scope must be written out:\n{text}");
        assert_eq!(InputMap::parse(&text).expect("re-parses"), map);

        // Absent in the file = every slot, so every existing input.ron is unchanged.
        let plain = InputMap::starter();
        assert!(!plain.to_ron().contains("player:"), "unscoped bindings stay clean");
        assert!(plain.actions[0].bindings[0].serves(0));
        assert!(plain.actions[0].bindings[0].serves(3));
    }

    #[test]
    fn empty_map_round_trips() {
        let map = InputMap::default();
        let back = InputMap::parse(&map.to_ron()).unwrap();
        assert_eq!(map, back);
        assert!(back.is_empty());
    }

    #[test]
    fn hand_written_ron_parses() {
        // What someone would actually type into input.ron — omitting every
        // optional field. If this breaks, the format stopped being hand-editable.
        let text = r#"
            InputMap(
                actions: [
                    Action(name: "Punch", bindings: [
                        Binding(source: Key(KeyJ)),
                        Binding(source: Pad(id: Slot(0), ctrl: Button(West))),
                    ]),
                ],
                axes2: [
                    Axis2(name: "Move", socd: LastWins, bindings: [
                        Keys(up: Key(KeyW), down: Key(KeyS), left: Key(KeyA), right: Key(KeyD)),
                    ]),
                ],
                motions: [ Motion(name: "qcf", dirs: [2, 3, 6], window: 12) ],
                players: 2,
            )
        "#;
        let map = InputMap::parse(text).expect("hand-written RON parses");
        assert_eq!(map.players, 2);
        assert_eq!(map.action_index("Punch"), Some(0));
        assert_eq!(map.axes2[0].socd, Socd::LastWins);
        assert_eq!(map.motion("qcf").unwrap().dirs, vec![2, 3, 6]);
        assert_eq!(map.motion("qcf").unwrap().charge, 0, "charge defaults to none");
        assert_eq!(
            map.actions[0].bindings[0].threshold, DEFAULT_THRESHOLD,
            "omitted threshold takes the default"
        );
    }

    #[test]
    fn merge_fills_gaps_without_touching_what_is_there() {
        // The real case: someone added "Move" from the used-but-unbound warning
        // (so it exists with NO bindings), bound Jump to Space only, and has
        // their own "Punch". Merging the starter must bind Move, add Jump's pad
        // binding, and leave Punch and their SOCD choice alone.
        let mut mine = InputMap {
            actions: vec![
                Action {
                    name: "Jump".into(),
                    bindings: vec![Binding::new(Source::Key(Key::Space))],
                },
                Action {
                    name: "Punch".into(),
                    bindings: vec![Binding::new(Source::Mouse(crate::source::MouseButton::Left))],
                },
            ],
            axes2: vec![Axis2 { name: "Move".into(), socd: Socd::Positive, bindings: vec![] }],
            ..Default::default()
        };

        let added = mine.merge_missing(&InputMap::starter());
        assert!(added > 0);

        // Move is now actually bound — it was a dead control before.
        let mv = &mine.axes2[mine.axis2_index("Move").unwrap()];
        assert!(!mv.bindings.is_empty(), "the unbound axis got the starter bindings");
        assert_eq!(mv.socd, Socd::Positive, "their SOCD choice survives");

        // Jump keeps Space and gains the pad binding.
        let jump = &mine.actions[mine.action_index("Jump").unwrap()];
        assert!(jump.bindings.iter().any(|b| b.source == Source::Key(Key::Space)));
        assert!(jump.bindings.iter().any(|b| b.source.device() == crate::source::Device::Pad));

        // Their own action is untouched and still theirs.
        let punch = &mine.actions[mine.action_index("Punch").unwrap()];
        assert_eq!(punch.bindings.len(), 1);

        // And the starter's other entries arrived.
        assert!(mine.action_index("Sprint").is_some());
        assert!(mine.axis2_index("Look").is_some());
    }

    #[test]
    fn merging_twice_adds_nothing_the_second_time() {
        let mut m = InputMap::starter();
        assert_eq!(m.merge_missing(&InputMap::starter()), 0, "idempotent");
    }

    #[test]
    fn merge_respects_the_action_cap() {
        let mut m = InputMap::default();
        for i in 0..MAX_ACTIONS {
            m.actions.push(Action::new(format!("A{i}")));
        }
        m.merge_missing(&InputMap::starter());
        assert_eq!(m.actions.len(), MAX_ACTIONS, "the wire's 64-bit mask still fits");
    }

    #[test]
    fn hash_tracks_shape_not_bindings() {
        let a = InputMap::starter();
        let mut b = a.clone();
        // A player rebinding Jump must NOT invalidate their multiplayer session.
        b.actions[0].bindings =
            vec![Binding::new(Source::Pad { id: PadId::Any, ctrl: PadControl::Button(PadButton::North) })];
        assert_eq!(a.hash(), b.hash());

        // Reordering actions DOES change it — that's the desync the hash exists
        // to catch, since the wire indexes by position.
        let mut c = a.clone();
        c.actions.swap(0, 1);
        assert_ne!(a.hash(), c.hash());

        // So does adding one.
        let mut d = a.clone();
        d.actions.push(Action::new("Taunt"));
        assert_ne!(a.hash(), d.hash());
    }

    #[test]
    fn hash_is_plain_fnv1a_over_the_shape() {
        // Checked against an independent FNV-1a here rather than a magic
        // constant: the point is that the value is reproducible from the spec,
        // so a client and a server built at different times agree.
        let mut m = InputMap::default();
        m.actions.push(Action::new("Jump"));
        m.axes2.push(Axis2 { name: "Move".into(), socd: Socd::Neutral, bindings: vec![] });

        let mut want: u64 = 0xcbf2_9ce4_8422_2325;
        for b in b"AJump2Move" {
            want ^= *b as u64;
            want = want.wrapping_mul(0x0000_0100_0000_01b3);
        }
        assert_eq!(m.hash(), want);
    }

    #[test]
    fn motion_axis_defaults_to_move() {
        let m = InputMap::starter();
        assert_eq!(m.motion_axis_index(), m.axis2_index("Move"));
    }

    #[test]
    fn chords_render_in_the_chip() {
        let b = Binding::with_modifiers(
            Source::Key(Key::KeyS),
            vec![Source::Key(Key::ControlLeft)],
        );
        assert_eq!(b.chip(), "⌨ L-Ctrl+S");
    }

    #[test]
    fn starter_binds_every_action_on_both_device_families() {
        // The promise of the starter map: pick it up with a pad, it works.
        for a in &InputMap::starter().actions {
            let kb = a.bindings.iter().any(|b| {
                matches!(b.source.device(), crate::source::Device::Keyboard | crate::source::Device::Mouse)
            });
            let pad = a.bindings.iter().any(|b| b.source.device() == crate::source::Device::Pad);
            assert!(kb && pad, "{} is not bound on both families", a.name);
        }
    }
}
