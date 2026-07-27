//! Resolution: [`RawInput`] + [`InputMap`] → [`ActionState`].
//!
//! An [`ActionRuntime`] owns the small amount of state resolution needs across
//! windows — the previous held mask (for edges), per-action hold durations, and
//! per-axis SOCD memory.
//!
//! **Run two of these, not one.** The engine samples input in two domains: once
//! per rendered frame for `update`, and once per fixed tick for `fixedUpdate`.
//! They advance at different rates, so sharing edge state between them would
//! make one domain eat the other's edges. Each domain gets its own runtime, and
//! the tick domain's is the one that feeds [`crate::history`].

use crate::context::AllowMask;
use crate::map::{Axis1Binding, Axis2Binding, Binding, Curve, InputMap, Socd};
use crate::raw::RawInput;

/// One window's resolved input, for one player.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ActionState {
    /// Bit `i` = action `i` is held.
    pub held: u64,
    /// Bit `i` = action `i` went down this window.
    pub just_pressed: u64,
    /// Bit `i` = action `i` went up this window.
    pub just_released: u64,
    /// Seconds action `i` has been continuously held (0 when up).
    pub held_secs: Vec<f32>,
    pub axes1: Vec<f32>,
    pub axes2: Vec<(f32, f32)>,
}

impl ActionState {
    pub fn is_held(&self, i: usize) -> bool {
        i < 64 && self.held & (1u64 << i) != 0
    }
    pub fn is_just_pressed(&self, i: usize) -> bool {
        i < 64 && self.just_pressed & (1u64 << i) != 0
    }
    pub fn is_just_released(&self, i: usize) -> bool {
        i < 64 && self.just_released & (1u64 << i) != 0
    }
    pub fn secs(&self, i: usize) -> f32 {
        self.held_secs.get(i).copied().unwrap_or(0.0)
    }
    pub fn axis1(&self, i: usize) -> f32 {
        self.axes1.get(i).copied().unwrap_or(0.0)
    }
    pub fn axis2(&self, i: usize) -> (f32, f32) {
        self.axes2.get(i).copied().unwrap_or((0.0, 0.0))
    }
}

/// Per-domain resolution state.
#[derive(Clone, Debug, Default)]
pub struct ActionRuntime {
    prev_held: u64,
    held_secs: Vec<f32>,
    /// Last non-neutral direction per 1D axis, for [`Socd::LastWins`].
    socd1: Vec<i8>,
    /// Last non-neutral (x, y) per 2D axis, for [`Socd::LastWins`].
    socd2: Vec<(i8, i8)>,
}

impl ActionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget all edge and hold state — call when input should go dead without
    /// synthesising a release storm (leaving Play, losing window focus).
    pub fn reset(&mut self) {
        self.prev_held = 0;
        self.held_secs.iter_mut().for_each(|s| *s = 0.0);
        self.socd1.iter_mut().for_each(|s| *s = 0);
        self.socd2.iter_mut().for_each(|s| *s = (0, 0));
    }

    /// Resolve one window for one player.
    ///
    /// `slot` is the player index (0 = P1) — it decides which pad a
    /// [`crate::PadId::Any`] binding reads. `dt` advances hold timers. `allow`
    /// comes from the context stack; blocked entries resolve fully neutral.
    pub fn resolve(
        &mut self,
        map: &InputMap,
        raw: &RawInput,
        slot: u8,
        dt: f32,
        allow: AllowMask,
    ) -> ActionState {
        let n = map.actions.len().min(64);
        self.held_secs.resize(map.actions.len(), 0.0);
        self.socd1.resize(map.axes1.len(), 0);
        self.socd2.resize(map.axes2.len(), (0, 0));

        // --- digital actions -------------------------------------------------
        let mut held = 0u64;
        let mut banked_press = 0u64;
        let mut banked_release = 0u64;
        for (i, action) in map.actions.iter().take(n).enumerate() {
            let bit = 1u64 << i;
            if allow.actions & bit == 0 {
                continue;
            }
            for b in &action.bindings {
                if !modifiers_held(b, raw, slot) {
                    continue;
                }
                if raw.held(b.source, slot, b.threshold) {
                    held |= bit;
                }
                // A press that happened and ended between two windows still
                // counts: without this a key tapped between ticks is invisible
                // to `fixedUpdate`.
                if raw.pressed.contains(&b.source) {
                    banked_press |= bit;
                }
                if raw.released.contains(&b.source) {
                    banked_release |= bit;
                }
            }
        }

        // Edges combine the level diff with the banked edges. `held` stays
        // level-only, matching the long-standing `input.key` semantics: a tap
        // that started and ended inside one window reports `justPressed` but
        // not `held`. The buffer layer is what catches those for a fighter.
        let rising = held & !self.prev_held;
        let falling = !held & self.prev_held;
        let just_pressed = rising | banked_press;
        let just_released = falling | banked_release;
        self.prev_held = held;

        for i in 0..map.actions.len() {
            if i < 64 && held & (1u64 << i) != 0 {
                self.held_secs[i] += dt;
            } else {
                self.held_secs[i] = 0.0;
            }
        }

        // --- analog axes -----------------------------------------------------
        let axes1 = map
            .axes1
            .iter()
            .enumerate()
            .map(|(i, ax)| {
                if allow.axes1 & (1u64 << i.min(63)) == 0 {
                    return 0.0;
                }
                let mut best = 0.0f32;
                for b in &ax.bindings {
                    let v = resolve_axis1(b, raw, slot, dt, ax.socd, &mut self.socd1[i]);
                    // Dominant source: the strongest contributor wins outright,
                    // so bumping the keyboard while on a stick can't fight it.
                    if v.abs() > best.abs() {
                        best = v;
                    }
                }
                // No blanket clamp: each binding bounds ITSELF (a key pair is
                // ±1, an analog source is bounded by its sensitivity), while a
                // rate-style source such as the wheel legitimately exceeds 1.
                best
            })
            .collect();

        let axes2 = map
            .axes2
            .iter()
            .enumerate()
            .map(|(i, ax)| {
                if allow.axes2 & (1u64 << i.min(63)) == 0 {
                    return (0.0, 0.0);
                }
                let mut best = (0.0f32, 0.0f32);
                let mut best_mag = 0.0f32;
                for b in &ax.bindings {
                    let v = resolve_axis2(b, raw, slot, dt, ax.socd, &mut self.socd2[i]);
                    let mag = (v.0 * v.0 + v.1 * v.1).sqrt();
                    // Strictly greater, so an exact tie (full stick vs a held
                    // key — both magnitude 1) keeps the earlier binding. The
                    // property that matters is that ONE source wins whole:
                    // summing them would let a brushed key deaden the stick.
                    if mag > best_mag {
                        best_mag = mag;
                        best = v;
                    }
                }
                // No clamp here: the unit-disk rule belongs to the `Keys` form
                // (where diagonals would otherwise outrun cardinals) and a
                // stick is bounded by construction, while a rate-style mouse
                // binding legitimately reports far more than 1.
                let _ = best_mag;
                best
            })
            .collect();

        ActionState {
            held,
            just_pressed,
            just_released,
            held_secs: self.held_secs.clone(),
            axes1,
            axes2,
        }
    }
}

/// Every modifier in a chord must be held for the binding to count.
fn modifiers_held(b: &Binding, raw: &RawInput, slot: u8) -> bool {
    b.modifiers.iter().all(|m| raw.held(*m, slot, crate::map::DEFAULT_THRESHOLD))
}

/// An axis binding's gate: empty means always-on, otherwise every listed source
/// must be held. Lets one axis mix a hold-to-drag mouse with an always-live stick.
fn gate_open(gate: &[crate::source::Source], raw: &RawInput, slot: u8) -> bool {
    gate.iter().all(|g| raw.held(*g, slot, crate::map::DEFAULT_THRESHOLD))
}

/// Collapse two opposing digital inputs into −1 / 0 / +1 under an SOCD rule.
/// `memory` carries the last non-neutral direction for [`Socd::LastWins`].
fn socd_axis(neg: bool, pos: bool, mode: Socd, memory: &mut i8) -> f32 {
    match (neg, pos) {
        (false, false) => {
            *memory = 0;
            0.0
        }
        (true, false) => {
            *memory = -1;
            -1.0
        }
        (false, true) => {
            *memory = 1;
            1.0
        }
        (true, true) => match mode {
            Socd::Neutral => 0.0,
            Socd::Positive => 1.0,
            Socd::Negative => -1.0,
            // Whichever was pressed FIRST is the one being overridden, so the
            // remembered direction is the older one and the other wins.
            Socd::LastWins => match *memory {
                1 => -1.0,
                -1 => 1.0,
                _ => 0.0,
            },
        },
    }
}

/// Deadzone → curve → sensitivity for a scalar analog value.
fn shape(v: f32, deadzone: f32, curve: Curve, sensitivity: f32) -> f32 {
    let m = v.abs();
    if m <= deadzone {
        return 0.0;
    }
    // Rescale so the value ramps from 0 at the deadzone edge rather than
    // snapping to `deadzone` — otherwise every stick has a visible step.
    let t = ((m - deadzone) / (1.0 - deadzone).max(f32::EPSILON)).min(1.0);
    curve.apply(t) * sensitivity * v.signum()
}

fn resolve_axis1(
    b: &Axis1Binding,
    raw: &RawInput,
    slot: u8,
    _dt: f32,
    socd: Socd,
    memory: &mut i8,
) -> f32 {
    match b {
        Axis1Binding::Keys { minus, plus } => {
            let n = raw.held(*minus, slot, crate::map::DEFAULT_THRESHOLD);
            let p = raw.held(*plus, slot, crate::map::DEFAULT_THRESHOLD);
            socd_axis(n, p, socd, memory)
        }
        Axis1Binding::Analog { source, deadzone, sensitivity, invert, curve, gate } => {
            if !gate_open(gate, raw, slot) {
                return 0.0;
            }
            let v = shape(raw.value(*source, slot), *deadzone, *curve, *sensitivity);
            if *invert { -v } else { v }
        }
    }
}

fn resolve_axis2(
    b: &Axis2Binding,
    raw: &RawInput,
    slot: u8,
    dt: f32,
    socd: Socd,
    memory: &mut (i8, i8),
) -> (f32, f32) {
    match b {
        Axis2Binding::Keys { up, down, left, right } => {
            let t = crate::map::DEFAULT_THRESHOLD;
            let x = socd_axis(
                raw.held(*left, slot, t),
                raw.held(*right, slot, t),
                socd,
                &mut memory.0,
            );
            let y =
                socd_axis(raw.held(*down, slot, t), raw.held(*up, slot, t), socd, &mut memory.1);
            // Unit disk, not unit square: holding W+D must not travel faster
            // than holding W. Done HERE rather than across the whole axis, so a
            // rate-style mouse binding on the same axis stays unclamped.
            let mag = (x * x + y * y).sqrt();
            if mag > 1.0 { (x / mag, y / mag) } else { (x, y) }
        }
        Axis2Binding::Stick { id, x, y, deadzone, sensitivity, invert_y, curve } => {
            use crate::source::{PadControl, Source as S};
            let rx = raw.value(S::Pad { id: *id, ctrl: PadControl::Axis(*x) }, slot);
            let ry = raw.value(S::Pad { id: *id, ctrl: PadControl::Axis(*y) }, slot);
            // RADIAL deadzone — a per-axis one would let the diagonals leak
            // drift through and would square off the stick's circle.
            let mag = (rx * rx + ry * ry).sqrt();
            if mag <= *deadzone {
                return (0.0, 0.0);
            }
            let t = ((mag - deadzone) / (1.0 - deadzone).max(f32::EPSILON)).min(1.0);
            let scaled = curve.apply(t) * sensitivity;
            let (nx, ny) = (rx / mag, ry / mag);
            (nx * scaled, if *invert_y { -ny * scaled } else { ny * scaled })
        }
        Axis2Binding::Mouse { sensitivity, invert_y, gate, rate } => {
            if !gate_open(gate, raw, slot) {
                return (0.0, 0.0);
            }
            let (dx, dy) = raw.mouse_delta;
            // Pixels-this-frame → pixels-per-second, so the value composes with
            // a stick's rate and a script's `* dt` cancels back out exactly.
            // Clamped rather than divided by a near-zero dt: a hitching frame
            // must not fling the camera.
            let s = if *rate { *sensitivity / dt.max(1.0 / 240.0) } else { *sensitivity };
            (dx * s, if *invert_y { -dy * s } else { dy * s })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Action, Axis2, Binding, InputMap};
    use crate::raw::PadState;
    use crate::source::{Key, PadAxis, PadButton, PadControl, PadId, Source};

    fn jump_map() -> InputMap {
        InputMap {
            actions: vec![Action {
                name: "Jump".into(),
                bindings: vec![
                    Binding::new(Source::Key(Key::Space)),
                    Binding::new(Source::Pad {
                        id: PadId::Any,
                        ctrl: PadControl::Button(PadButton::South),
                    }),
                ],
            }],
            ..Default::default()
        }
    }

    fn move_map(socd: Socd) -> InputMap {
        InputMap {
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd,
                bindings: vec![
                    Axis2Binding::Keys {
                        up: Source::Key(Key::KeyW),
                        down: Source::Key(Key::KeyS),
                        left: Source::Key(Key::KeyA),
                        right: Source::Key(Key::KeyD),
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
            }],
            ..Default::default()
        }
    }

    fn with_keys(keys: &[Key]) -> RawInput {
        RawInput { keys: keys.iter().copied().collect(), ..Default::default() }
    }

    fn stick(x: f32, y: f32) -> RawInput {
        let mut axes = [0.0; PadAxis::COUNT];
        axes[PadAxis::LeftStickX.index()] = x;
        axes[PadAxis::LeftStickY.index()] = y;
        RawInput {
            pads: vec![PadState { connected: true, buttons: Default::default(), axes }],
            ..Default::default()
        }
    }

    #[test]
    fn either_binding_fires_the_action() {
        let map = jump_map();
        let mut rt = ActionRuntime::new();

        let s = rt.resolve(&map, &with_keys(&[Key::Space]), 0, 0.016, AllowMask::ALL);
        assert!(s.is_held(0) && s.is_just_pressed(0), "keyboard fires it");

        rt.reset();
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).buttons.insert(PadButton::South);
        let s = rt.resolve(&map, &raw, 0, 0.016, AllowMask::ALL);
        assert!(s.is_held(0) && s.is_just_pressed(0), "the pad fires the same action");
    }

    #[test]
    fn edges_fire_once_and_hold_accumulates() {
        let map = jump_map();
        let mut rt = ActionRuntime::new();
        let down = with_keys(&[Key::Space]);

        let a = rt.resolve(&map, &down, 0, 0.5, AllowMask::ALL);
        assert!(a.is_just_pressed(0));
        assert_eq!(a.secs(0), 0.5);

        let b = rt.resolve(&map, &down, 0, 0.5, AllowMask::ALL);
        assert!(!b.is_just_pressed(0), "held is not a repeated press");
        assert_eq!(b.secs(0), 1.0);

        let c = rt.resolve(&map, &RawInput::default(), 0, 0.5, AllowMask::ALL);
        assert!(c.is_just_released(0));
        assert_eq!(c.secs(0), 0.0, "hold time clears on release");

        let d = rt.resolve(&map, &RawInput::default(), 0, 0.5, AllowMask::ALL);
        assert!(!d.is_just_released(0), "release fires once");
    }

    #[test]
    fn a_tap_between_ticks_survives_via_banked_edges() {
        // The tick domain's whole reason for banking: the key is already back
        // up by the time the tick samples, but the press must not be lost.
        let map = jump_map();
        let mut rt = ActionRuntime::new();
        let raw = RawInput {
            pressed: [Source::Key(Key::Space)].into_iter().collect(),
            released: [Source::Key(Key::Space)].into_iter().collect(),
            ..Default::default()
        };
        let s = rt.resolve(&map, &raw, 0, 0.016, AllowMask::ALL);
        assert!(s.is_just_pressed(0), "the banked press is seen");
        assert!(!s.is_held(0), "…but held stays level-only, as raw keys always have");
        assert!(s.is_just_released(0));
    }

    #[test]
    fn chords_require_their_modifier() {
        let map = InputMap {
            actions: vec![Action {
                name: "Save".into(),
                bindings: vec![Binding::with_modifiers(
                    Source::Key(Key::KeyS),
                    vec![Source::Key(Key::ControlLeft)],
                )],
            }],
            ..Default::default()
        };
        let mut rt = ActionRuntime::new();
        assert!(!rt.resolve(&map, &with_keys(&[Key::KeyS]), 0, 0.016, AllowMask::ALL).is_held(0));
        rt.reset();
        let s = rt.resolve(
            &map,
            &with_keys(&[Key::KeyS, Key::ControlLeft]),
            0,
            0.016,
            AllowMask::ALL,
        );
        assert!(s.is_held(0));
    }

    #[test]
    fn wasd_and_stick_agree_on_one_axis() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();

        let kb = rt.resolve(&map, &with_keys(&[Key::KeyW]), 0, 0.016, AllowMask::ALL).axis2(0);
        assert_eq!(kb, (0.0, 1.0));

        rt.reset();
        let pad = rt.resolve(&map, &stick(0.0, 1.0), 0, 0.016, AllowMask::ALL).axis2(0);
        assert!((pad.1 - 1.0).abs() < 1e-5 && pad.0.abs() < 1e-5, "{pad:?}");
    }

    #[test]
    fn opposing_sources_never_cancel_each_other() {
        // Stick fully up while S is held: the two must not sum to a dead axis.
        // One source wins whole — which one is the declaration-order tie-break.
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();
        let mut raw = stick(0.0, 1.0);
        raw.keys.insert(Key::KeyS); // would be (0,-1) on its own
        let v = rt.resolve(&map, &raw, 0, 0.016, AllowMask::ALL).axis2(0);
        assert!(v.1.abs() > 0.9, "a source must win outright, got {v:?}");
    }

    #[test]
    fn the_larger_source_wins() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();

        // A light stick lean loses to a decisive key press.
        let mut raw = stick(0.4, 0.0);
        raw.keys.insert(Key::KeyW);
        assert_eq!(rt.resolve(&map, &raw, 0, 0.016, AllowMask::ALL).axis2(0), (0.0, 1.0));

        // The same lean drives the axis on its own.
        rt.reset();
        let v = rt.resolve(&map, &stick(0.4, 0.0), 0, 0.016, AllowMask::ALL).axis2(0);
        assert!(v.0 > 0.2 && v.1 == 0.0, "{v:?}");
    }

    #[test]
    fn diagonals_clamp_to_the_unit_disk() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();
        let v = rt
            .resolve(&map, &with_keys(&[Key::KeyW, Key::KeyD]), 0, 0.016, AllowMask::ALL)
            .axis2(0);
        let mag = (v.0 * v.0 + v.1 * v.1).sqrt();
        assert!((mag - 1.0).abs() < 1e-5, "diagonal must not outrun cardinal: {mag}");
    }

    #[test]
    fn radial_deadzone_kills_drift_including_on_the_diagonal() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();
        // 0.1 on each axis: under the 0.15 radial threshold only if measured
        // radially (magnitude 0.141), which is the point.
        let v = rt.resolve(&map, &stick(0.1, 0.1), 0, 0.016, AllowMask::ALL).axis2(0);
        assert_eq!(v, (0.0, 0.0));
    }

    #[test]
    fn deadzone_rescales_instead_of_stepping() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();
        // Just past the deadzone should read near zero, not near 0.15.
        let v = rt.resolve(&map, &stick(0.16, 0.0), 0, 0.016, AllowMask::ALL).axis2(0);
        assert!(v.0 > 0.0 && v.0 < 0.05, "expected a soft ramp, got {v:?}");
        // Full deflection still reaches 1.
        let v = rt.resolve(&map, &stick(1.0, 0.0), 0, 0.016, AllowMask::ALL).axis2(0);
        assert!((v.0 - 1.0).abs() < 1e-5, "{v:?}");
    }

    #[test]
    fn socd_neutral_cancels_opposing_directions() {
        let map = move_map(Socd::Neutral);
        let mut rt = ActionRuntime::new();
        let v = rt
            .resolve(&map, &with_keys(&[Key::KeyA, Key::KeyD]), 0, 0.016, AllowMask::ALL)
            .axis2(0);
        assert_eq!(v.0, 0.0);
    }

    #[test]
    fn socd_last_wins_lets_a_player_pivot() {
        let map = move_map(Socd::LastWins);
        let mut rt = ActionRuntime::new();
        // Hold left…
        let v = rt.resolve(&map, &with_keys(&[Key::KeyA]), 0, 0.016, AllowMask::ALL).axis2(0);
        assert_eq!(v.0, -1.0);
        // …then add right without releasing left: right is newer, so it wins.
        let v = rt
            .resolve(&map, &with_keys(&[Key::KeyA, Key::KeyD]), 0, 0.016, AllowMask::ALL)
            .axis2(0);
        assert_eq!(v.0, 1.0, "the newer direction takes over with no neutral frame");
        // Release right, left is still held and takes back over.
        let v = rt.resolve(&map, &with_keys(&[Key::KeyA]), 0, 0.016, AllowMask::ALL).axis2(0);
        assert_eq!(v.0, -1.0);
    }

    #[test]
    fn socd_priority_modes_are_deterministic() {
        for (mode, want) in [(Socd::Positive, 1.0), (Socd::Negative, -1.0)] {
            let map = move_map(mode);
            let mut rt = ActionRuntime::new();
            let v = rt
                .resolve(&map, &with_keys(&[Key::KeyA, Key::KeyD]), 0, 0.016, AllowMask::ALL)
                .axis2(0);
            assert_eq!(v.0, want, "{mode:?}");
        }
    }

    /// A "Look" axis in the shape the shipped scripts use: hold-to-drag mouse
    /// plus an always-live right stick.
    fn look_map() -> InputMap {
        InputMap {
            axes2: vec![Axis2 {
                name: "Look".into(),
                socd: Socd::Neutral,
                bindings: vec![
                    Axis2Binding::Mouse {
                        sensitivity: 0.006,
                        invert_y: false,
                        rate: true,
                        gate: vec![Source::Mouse(crate::source::MouseButton::Right)],
                    },
                    Axis2Binding::Stick {
                        id: PadId::Any,
                        x: PadAxis::RightStickX,
                        y: PadAxis::RightStickY,
                        deadzone: 0.12,
                        sensitivity: 2.5,
                        invert_y: false,
                        curve: Curve::Linear,
                    },
                ],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_gated_mouse_binding_needs_its_button() {
        // The bug this prevents: a free cursor drifting across the window spins
        // the camera even though nobody is dragging.
        let map = look_map();
        let mut rt = ActionRuntime::new();

        let moving = RawInput { mouse_delta: (60.0, 0.0), ..Default::default() };
        assert_eq!(
            rt.resolve(&map, &moving, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0),
            (0.0, 0.0),
            "no drag, no look"
        );

        let mut dragging = moving.clone();
        dragging.mouse_buttons[crate::source::MouseButton::Right.index()] = true;
        let v = rt.resolve(&map, &dragging, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0);
        assert!(v.0 > 0.0, "dragging looks, got {v:?}");
    }

    #[test]
    fn the_stick_on_a_gated_axis_stays_live() {
        // The gate must apply to the MOUSE binding only — a pad has no button
        // to hold, and a stick recentres itself anyway.
        let map = look_map();
        let mut rt = ActionRuntime::new();
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).axes[PadAxis::RightStickX.index()] = 1.0;

        let v = rt.resolve(&map, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0);
        assert!((v.0 - 2.5).abs() < 1e-4, "full stick = 2.5 rad/s, got {v:?}");
    }

    #[test]
    fn mouse_as_a_rate_is_framerate_independent() {
        // The property the whole `rate` flag exists for: the same physical
        // gesture must turn the camera the same amount at 30 fps and 240 fps,
        // once the script multiplies by dt.
        let map = look_map();
        let mut rt = ActionRuntime::new();

        let mut drag = |px: f32, dt: f32| {
            let mut raw = RawInput { mouse_delta: (px, 0.0), ..Default::default() };
            raw.mouse_buttons[crate::source::MouseButton::Right.index()] = true;
            rt.resolve(&map, &raw, 0, dt, AllowMask::ALL).axis2(0).0 * dt
        };
        // 120 px moved over one 30 fps frame, vs 15 px over each of eight 240
        // fps frames — the same gesture, so the same total turn.
        let slow = drag(120.0, 1.0 / 30.0);
        let fast: f32 = (0..8).map(|_| drag(15.0, 1.0 / 240.0)).sum();
        assert!((slow - fast).abs() < 1e-5, "slow {slow} vs fast {fast}");
    }

    #[test]
    fn a_hitching_frame_does_not_fling_the_camera() {
        // dt near zero would otherwise divide the delta into a huge rate.
        let map = look_map();
        let mut rt = ActionRuntime::new();
        let mut raw = RawInput { mouse_delta: (40.0, 0.0), ..Default::default() };
        raw.mouse_buttons[crate::source::MouseButton::Right.index()] = true;
        let v = rt.resolve(&map, &raw, 0, 0.0, AllowMask::ALL).axis2(0);
        assert!(v.0.is_finite() && v.0 < 100.0, "clamped, got {v:?}");
    }

    #[test]
    fn a_rate_axis_is_not_clamped_to_one() {
        // Look is a turn RATE, not a direction — clamping it to the unit disk
        // would cap how fast a flick can turn you.
        let map = look_map();
        let mut rt = ActionRuntime::new();
        let mut raw = RawInput { mouse_delta: (600.0, 0.0), ..Default::default() };
        raw.mouse_buttons[crate::source::MouseButton::Right.index()] = true;
        let v = rt.resolve(&map, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0);
        assert!(v.0 > 1.0, "a fast flick must exceed 1, got {v:?}");
    }

    #[test]
    fn blocked_entries_resolve_neutral() {
        let map = jump_map();
        let mut rt = ActionRuntime::new();
        let blocked = AllowMask { actions: 0, axes1: 0, axes2: 0 };
        let s = rt.resolve(&map, &with_keys(&[Key::Space]), 0, 0.016, blocked);
        assert!(!s.is_held(0) && !s.is_just_pressed(0));
        assert_eq!(s.secs(0), 0.0);
    }

    #[test]
    fn slots_read_their_own_pads() {
        let map = jump_map();
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(1).connected = true;
        raw.pad_mut(1).buttons.insert(PadButton::South);

        let mut p1 = ActionRuntime::new();
        let mut p2 = ActionRuntime::new();
        assert!(!p1.resolve(&map, &raw, 0, 0.016, AllowMask::ALL).is_held(0));
        assert!(p2.resolve(&map, &raw, 1, 0.016, AllowMask::ALL).is_held(0));
    }

    #[test]
    fn reset_clears_edges_without_synthesising_a_release() {
        let map = jump_map();
        let mut rt = ActionRuntime::new();
        rt.resolve(&map, &with_keys(&[Key::Space]), 0, 0.016, AllowMask::ALL);
        rt.reset();
        let s = rt.resolve(&map, &RawInput::default(), 0, 0.016, AllowMask::ALL);
        assert!(!s.is_just_released(0), "reset must not fire a phantom release");
    }

    #[test]
    fn actions_beyond_the_bitmask_cap_are_ignored_not_wrapped() {
        // 70 actions: the 65th+ must not alias onto bit 0 and fire "Jump".
        let mut map = InputMap::default();
        for i in 0..70 {
            map.actions.push(Action {
                name: format!("A{i}"),
                bindings: vec![Binding::new(Source::Key(Key::Space))],
            });
        }
        let mut rt = ActionRuntime::new();
        let s = rt.resolve(&map, &with_keys(&[Key::Space]), 0, 0.016, AllowMask::ALL);
        assert_eq!(s.held, u64::MAX, "the first 64 all fire");
        assert!(!s.is_held(64), "beyond the cap reads false rather than wrapping");
    }
}
