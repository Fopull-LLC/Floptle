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
                // A binding scoped to another local player contributes nothing here —
                // this is what lets ONE `Light` action be `J` for P1 and `1` for P2 on
                // the same keyboard instead of needing a duplicate `Light2`.
                if !b.serves(slot) || !modifiers_held(b, raw, slot) {
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
        Axis1Binding::Keys { minus, plus, player } => {
            if player.is_some_and(|p| p != slot) {
                return 0.0;
            }
            let n = raw.held(*minus, slot, crate::map::DEFAULT_THRESHOLD);
            let p = raw.held(*plus, slot, crate::map::DEFAULT_THRESHOLD);
            socd_axis(n, p, socd, memory)
        }
        Axis1Binding::Analog { source, player, deadzone, sensitivity, invert, curve, gate } => {
            if player.is_some_and(|p| p != slot) {
                return 0.0;
            }
            if !gate_open(gate, raw, slot) {
                return 0.0;
            }
            let v = shape(raw.value(own_pad(*source, *player, slot), slot), *deadzone, *curve, *sensitivity);
            if *invert { -v } else { v }
        }
    }
}

/// A player-scoped binding reading `PadId::Any` means **that player's own pad**,
/// not "the first pad anyone has plugged in". Without this, in a two-player game
/// with one pad connected, player two borrows player one's stick — the binding
/// form could express neither "this device" nor "this player's device".
/// floptle/0043.
fn own_pad(source: crate::source::Source, player: Option<u8>, slot: u8) -> crate::source::Source {
    use crate::source::{PadId, Source as S};
    match (source, player) {
        (S::Pad { id: PadId::Any, ctrl }, Some(_)) => S::Pad { id: PadId::Slot(slot), ctrl },
        _ => source,
    }
}

/// The shortest frame the mouse-rate conversion will divide by.
///
/// A divide-by-zero guard and nothing else. Deliberately absurd (10 000 fps) so
/// that every frame a real machine produces divides by its own true `dt` and a
/// script's `* dt` cancels exactly — `floptle/0161` is what a floor inside the
/// real range does instead.
const MIN_RATE_DT: f32 = 1.0 / 10_000.0;

/// The most pixels-per-second a mouse axis will report.
///
/// The hitch guard, as a ceiling on the answer rather than a floor under the
/// divisor — so it bounds the pathological case without altering the ordinary
/// one. A fast human flick is a few thousand pixels a second; this is two
/// orders of magnitude above that.
const MAX_MOUSE_RATE: f32 = 500_000.0;

fn resolve_axis2(
    b: &Axis2Binding,
    raw: &RawInput,
    slot: u8,
    dt: f32,
    socd: Socd,
    memory: &mut (i8, i8),
) -> (f32, f32) {
    match b {
        Axis2Binding::Keys { up, down, left, right, player } => {
            if player.is_some_and(|p| p != slot) {
                return (0.0, 0.0);
            }
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
        Axis2Binding::Stick { id, x, y, player, deadzone, sensitivity, invert_y, curve } => {
            use crate::source::{PadControl, Source as S};
            if player.is_some_and(|p| p != slot) {
                return (0.0, 0.0);
            }
            // A player-scoped `Any` means THIS player's own pad, not "any
            // player's pad" — otherwise a second player with no pad of their
            // own silently mirrors the first player's stick. floptle/0043.
            let id = if player.is_some() && *id == crate::source::PadId::Any {
                crate::source::PadId::Slot(slot)
            } else {
                *id
            };
            let rx = raw.value(S::Pad { id, ctrl: PadControl::Axis(*x) }, slot);
            let ry = raw.value(S::Pad { id, ctrl: PadControl::Axis(*y) }, slot);
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
            //
            // **The cancellation is only exact if the divisor is the real
            // `dt`.** This floored it at `1/240 s`, so every frame faster than
            // 240 fps divided by 1/240 instead — and `yaw_delta = dx * sens *
            // dt * 240` is proportional to `dt` again, which is the one thing
            // the conversion exists to remove. Two consecutive 2 ms and 4 ms
            // frames turned identical mouse movement into 2x different
            // rotation, so frame-time variance fed straight into the camera at
            // exactly the frame rates where it should have been smoothest
            // (`floptle/0161`). "Hundreds of fps" is ordinary hardware now.
            //
            // The floor was doing two jobs. They are separated here, because
            // only one of them needs to touch the transfer function — and that
            // one never fires in practice.
            let (mut vx, mut vy) = (dx, dy);
            if *rate {
                // Job one: never divide by zero. A frame of no duration is the
                // only thing this has to survive, so the floor sits orders of
                // magnitude below any real frame rather than inside the band
                // real machines run in.
                vx /= dt.max(MIN_RATE_DT);
                vy /= dt.max(MIN_RATE_DT);
                // Job two: the hitch guard. A cap on the RATE, which is what
                // "must not fling the camera" actually means, set far above any
                // hand — a violent flick is a few thousand pixels a second. Real
                // input at a real frame time cannot reach it; only a
                // pathological `dt` gets near.
                vx = vx.clamp(-MAX_MOUSE_RATE, MAX_MOUSE_RATE);
                vy = vy.clamp(-MAX_MOUSE_RATE, MAX_MOUSE_RATE);
            }
            let (vx, vy) = (vx * *sensitivity, vy * *sensitivity);
            (vx, if *invert_y { -vy } else { vy })
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
                        player: None,
                    },
                    Axis2Binding::Stick {
                        player: None,
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
            pads: vec![PadState { connected: true, name: String::new(), buttons: Default::default(), axes }],
            ..Default::default()
        }
    }

    /// floptle/0043: two pads, two players, one axis. Before `Stick` carried a
    /// `player`, the obvious map — `Slot(0)` and `Slot(1)` side by side — made
    /// BOTH sticks contribute to BOTH players, and largest-magnitude-wins meant
    /// whichever stick was pushed harder drove both characters at once.
    #[test]
    fn a_player_scoped_stick_reads_only_that_players_pad() {
        let scoped = |slot: u8, id: PadId| Axis2Binding::Stick {
            player: Some(slot),
            id,
            x: PadAxis::LeftStickX,
            y: PadAxis::LeftStickY,
            deadzone: 0.15,
            sensitivity: 1.0,
            invert_y: false,
            curve: Curve::Linear,
        };
        // Two pads: P1's pushed full left, P2's pushed full right.
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).axes[PadAxis::LeftStickX.index()] = -1.0;
        raw.pad_mut(1).connected = true;
        raw.pad_mut(1).axes[PadAxis::LeftStickX.index()] = 1.0;

        let map = InputMap {
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd: Socd::Neutral,
                bindings: vec![scoped(0, PadId::Slot(0)), scoped(1, PadId::Slot(1))],
            }],
            players: 2,
            ..Default::default()
        };
        let mut rt = ActionRuntime::new();
        let (p1x, _) = rt.resolve(&map, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0);
        let mut rt2 = ActionRuntime::new();
        let (p2x, _) = rt2.resolve(&map, &raw, 1, 1.0 / 60.0, AllowMask::ALL).axis2(0);
        assert!(p1x < -0.5, "player one reads their OWN pad (left), got {p1x}");
        assert!(p2x > 0.5, "player two reads their OWN pad (right), got {p2x}");

        // The bug, demonstrated: the SAME map with the scope dropped — which is
        // all that could be written before — leaks both pads into both players,
        // so they read identically and one stick drives both fighters.
        let unscoped = |id: PadId| Axis2Binding::Stick {
            player: None,
            id,
            x: PadAxis::LeftStickX,
            y: PadAxis::LeftStickY,
            deadzone: 0.15,
            sensitivity: 1.0,
            invert_y: false,
            curve: Curve::Linear,
        };
        let leaky = InputMap {
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd: Socd::Neutral,
                bindings: vec![unscoped(PadId::Slot(0)), unscoped(PadId::Slot(1))],
            }],
            players: 2,
            ..Default::default()
        };
        let mut a = ActionRuntime::new();
        let mut b = ActionRuntime::new();
        let leak1 = a.resolve(&leaky, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0).0;
        let leak2 = b.resolve(&leaky, &raw, 1, 1.0 / 60.0, AllowMask::ALL).axis2(0).0;
        assert_eq!(leak1, leak2, "unscoped: one stick drives both players — the reported bug");
    }

    /// The other half: a player-scoped `Any` means *this player's own pad*, not
    /// "any player's pad". With one pad connected, player two must read nothing
    /// rather than mirroring player one — which is what the `Any` workaround
    /// did, visible in local training as a dummy that walks with you.
    #[test]
    fn a_scoped_any_does_not_borrow_another_players_pad() {
        let scoped = |slot: u8| Axis2Binding::Stick {
            player: Some(slot),
            id: PadId::Any,
            x: PadAxis::LeftStickX,
            y: PadAxis::LeftStickY,
            deadzone: 0.15,
            sensitivity: 1.0,
            invert_y: false,
            curve: Curve::Linear,
        };
        // ONE pad, in slot 0.
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).axes[PadAxis::LeftStickX.index()] = 1.0;

        let map = InputMap {
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd: Socd::Neutral,
                bindings: vec![scoped(0), scoped(1)],
            }],
            players: 2,
            ..Default::default()
        };
        let mut rt = ActionRuntime::new();
        let owner = rt.resolve(&map, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0).0;
        let mut rt2 = ActionRuntime::new();
        let other = rt2.resolve(&map, &raw, 1, 1.0 / 60.0, AllowMask::ALL).axis2(0).0;
        assert!(owner > 0.5, "the pad's owner reads it");
        assert_eq!(
            other,
            0.0,
            "a player with no pad of their own reads ZERO, not their neighbour's stick"
        );
    }

    /// An UNSCOPED `Any` keeps its old meaning exactly — every existing map
    /// depends on it, and the fix must not change single-player behaviour.
    #[test]
    fn an_unscoped_any_stick_is_unchanged() {
        let mut raw = RawInput::default();
        raw.pad_mut(0).connected = true;
        raw.pad_mut(0).axes[PadAxis::LeftStickX.index()] = 1.0;
        let mut rt = ActionRuntime::new();
        let map = move_map(Socd::Neutral);
        assert!(rt.resolve(&map, &raw, 0, 1.0 / 60.0, AllowMask::ALL).axis2(0).0 > 0.5);
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

    /// Two players on ONE keyboard. A binding scoped to a slot fires only for that
    /// slot, so a single action name serves both fighters instead of the map having to
    /// carry a duplicate `Light2` (floptle/0028).
    #[test]
    fn a_player_scoped_binding_serves_only_its_slot() {
        let map = InputMap {
            actions: vec![Action {
                name: "Light".into(),
                bindings: vec![
                    Binding::new(Source::Key(Key::KeyJ)).for_player(0),
                    Binding::new(Source::Key(Key::Digit1)).for_player(1),
                    // Unscoped, and `Any` already resolves per slot: each player's own
                    // pad still fires it.
                    Binding::new(Source::Pad {
                        id: PadId::Any,
                        ctrl: PadControl::Button(PadButton::West),
                    }),
                ],
            }],
            players: 2,
            ..Default::default()
        };
        let (mut p1, mut p2) = (ActionRuntime::new(), ActionRuntime::new());
        let raw = with_keys(&[Key::KeyJ]);
        assert!(p1.resolve(&map, &raw, 0, 0.016, AllowMask::ALL).is_held(0), "J is P1's");
        assert!(
            !p2.resolve(&map, &raw, 1, 0.016, AllowMask::ALL).is_held(0),
            "J must NOT also punch for P2 — that was the whole bug"
        );

        let (mut p1, mut p2) = (ActionRuntime::new(), ActionRuntime::new());
        let raw = with_keys(&[Key::Digit1]);
        assert!(!p1.resolve(&map, &raw, 0, 0.016, AllowMask::ALL).is_held(0));
        assert!(p2.resolve(&map, &raw, 1, 0.016, AllowMask::ALL).is_held(0), "1 is P2's");

        // The unscoped pad binding is untouched: each slot's own pad fires it.
        for slot in [0u8, 1] {
            let mut rt = ActionRuntime::new();
            let mut raw = RawInput::default();
            raw.pad_mut(slot).connected = true;
            raw.pad_mut(slot).buttons.insert(PadButton::West);
            assert!(
                rt.resolve(&map, &raw, slot, 0.016, AllowMask::ALL).is_held(0),
                "PadId::Any must keep working per slot (slot {slot})"
            );
        }
    }

    /// One `Move` axis, WASD for P1 and the arrows for P2 — which is what makes the
    /// map-level motion axis (`dir()`, `qcf`, …) correct for BOTH local players instead
    /// of feeding player 1's stick into everyone's history.
    #[test]
    fn one_axis_can_carry_a_different_key_set_per_player() {
        let map = InputMap {
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd: Socd::Neutral,
                bindings: vec![
                    Axis2Binding::Keys {
                        up: Source::Key(Key::KeyW),
                        down: Source::Key(Key::KeyS),
                        left: Source::Key(Key::KeyA),
                        right: Source::Key(Key::KeyD),
                        player: Some(0),
                    },
                    Axis2Binding::Keys {
                        up: Source::Key(Key::ArrowUp),
                        down: Source::Key(Key::ArrowDown),
                        left: Source::Key(Key::ArrowLeft),
                        right: Source::Key(Key::ArrowRight),
                        player: Some(1),
                    },
                ],
            }],
            players: 2,
            ..Default::default()
        };
        // P1 walks right, P2 walks left, at the same instant on the same keyboard.
        let raw = with_keys(&[Key::KeyD, Key::ArrowLeft]);
        let v1 = ActionRuntime::new().resolve(&map, &raw, 0, 0.016, AllowMask::ALL).axis2(0);
        let v2 = ActionRuntime::new().resolve(&map, &raw, 1, 0.016, AllowMask::ALL).axis2(0);
        assert!(v1.0 > 0.9, "P1 reads only WASD, got {v1:?}");
        assert!(v2.0 < -0.9, "P2 reads only the arrows, got {v2:?}");
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
                        player: None,
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

    /// **The same hand movement has to turn the camera the same amount, at any
    /// frame rate.** That is the entire promise of `rate: true` — the axis
    /// reports pixels per second so a script's `* dt` cancels the frame time
    /// back out. A floor under the divisor broke the cancellation above 240 fps
    /// and put frame-time variance straight into the camera (`floptle/0161`).
    ///
    /// So: drive one physical 600-pixel sweep three ways and integrate the
    /// rotation a documented `yaw -= lookX * dt` script would apply.
    #[test]
    fn a_sweep_turns_the_same_amount_at_60_fps_and_at_600_fps() {
        let map = look_map();

        /// One sweep, `frames` long, `total_px` of movement spread over the
        /// given frame times — integrated the way a camera script does it.
        fn sweep(map: &InputMap, total_px: f32, dts: &[f32]) -> f32 {
            let mut rt = ActionRuntime::new();
            let mut yaw = 0.0;
            let span: f32 = dts.iter().sum();
            for &dt in dts {
                // The mouse moves at a constant physical speed, so the pixels
                // a frame sees are proportional to how long it lasted — which
                // is what makes `dx` itself scale with frame time.
                let px = total_px * (dt / span);
                let raw = RawInput {
                    mouse_delta: (px, 0.0),
                    mouse_buttons: {
                        let mut b = [false; 5];
                        b[crate::source::MouseButton::Right.index()] = true;
                        b
                    },
                    ..Default::default()
                };
                let (lx, _) = rt.resolve(map, &raw, 0, dt, AllowMask::ALL).axis2(0);
                yaw += lx * dt;
            }
            yaw
        }

        let slow = sweep(&map, 600.0, &[1.0 / 60.0; 60]);
        let fast = sweep(&map, 600.0, &[1.0 / 600.0; 600]);
        // 600 fps with 3x jitter — alternating 0.83 ms and 2.5 ms frames, the
        // uneven pacing a compositor actually produces.
        let jitter: Vec<f32> = (0..600)
            .map(|i| if i % 2 == 0 { 1.0 / 1200.0 } else { 3.0 / 1200.0 })
            .collect();
        let rough = sweep(&map, 600.0, &jitter);

        // 600 px x 0.006 sensitivity = 3.6 radians, whatever the frame rate.
        assert!(
            (slow - 3.6).abs() < 1e-3,
            "60 fps is the baseline and was never broken: {slow}"
        );
        assert!(
            (fast - slow).abs() < 1e-3,
            "600 fps turned {fast} where 60 fps turned {slow} — the rate \
             conversion stopped cancelling the frame time"
        );
        assert!(
            (rough - slow).abs() < 1e-3,
            "jittered 600 fps turned {rough} where 60 fps turned {slow} — \
             frame-time variance is reaching the camera"
        );
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
        //
        // Stated against [`MAX_MOUSE_RATE`], which is where the bound now comes
        // from. It used to be a bare `< 100.0` — a number that was really
        // `40 px x 0.006 x 240`, i.e. a restatement of the `1/240 s` floor
        // rather than of the property. The floor had to move to fix
        // `floptle/0161`, and the assertion moved with it, which is exactly the
        // situation where a guard written around an implementation stops
        // guarding anything. The property is: finite, and bounded by a stated
        // ceiling.
        let map = look_map();
        let mut rt = ActionRuntime::new();
        let mut raw = RawInput { mouse_delta: (40.0, 0.0), ..Default::default() };
        raw.mouse_buttons[crate::source::MouseButton::Right.index()] = true;
        let v = rt.resolve(&map, &raw, 0, 0.0, AllowMask::ALL).axis2(0);
        assert!(v.0.is_finite(), "a zero-length frame must not divide to infinity: {v:?}");
        assert!(
            v.0.abs() <= MAX_MOUSE_RATE * 0.006 + 1e-3,
            "bounded by the rate ceiling, got {v:?}"
        );
        // And the reason it does not matter in practice: a script's `* dt` is
        // multiplying by the same zero.
        assert_eq!(v.0 * 0.0, 0.0);
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
