//! Gamepads — the `gilrs` backend, behind the `pads` feature.
//!
//! This is the *only* module that knows `gilrs` exists. It pumps the event
//! queue, keeps a stable slot↔device mapping across hot-plug, and writes
//! per-slot [`PadState`] into a [`RawInput`]. Everything downstream sees plain
//! data and stays testable without a controller.
//!
//! ## Why slots are sticky
//!
//! `gilrs` hands out a fresh `GamepadId` per connection, and iteration order is
//! not stable. If P2's battery dies mid-match and they replug, a naive
//! "index into the connected list" mapping would silently promote them to P1 —
//! they'd start driving P1's character. So a slot is claimed on first connect
//! and **held** by that device's UUID: a replug returns to the same slot, and a
//! vacated slot is only reused by a genuinely new device.

use crate::raw::{PadState, RawInput};
use crate::source::{PadAxis, PadButton, PadControl, PadId, Source};

/// How many local players we track pads for. Four is the console convention and
/// bounds the bookkeeping; a fifth pad connects but gets no slot.
pub const MAX_SLOTS: usize = 4;

/// Our button vocabulary → gilrs's.
///
/// Note the trigger naming: gilrs calls the **bumper** `LeftTrigger` and the
/// analog **trigger** `LeftTrigger2`. Getting this backwards silently swaps L1
/// and L2 on every pad, so it is spelled out rather than inferred.
fn to_gilrs_button(b: PadButton) -> gilrs::Button {
    use gilrs::Button as G;
    match b {
        PadButton::South => G::South,
        PadButton::East => G::East,
        PadButton::West => G::West,
        PadButton::North => G::North,
        PadButton::LeftBumper => G::LeftTrigger,
        PadButton::RightBumper => G::RightTrigger,
        PadButton::LeftTrigger => G::LeftTrigger2,
        PadButton::RightTrigger => G::RightTrigger2,
        PadButton::Select => G::Select,
        PadButton::Start => G::Start,
        PadButton::Mode => G::Mode,
        PadButton::LeftStick => G::LeftThumb,
        PadButton::RightStick => G::RightThumb,
        PadButton::DPadUp => G::DPadUp,
        PadButton::DPadDown => G::DPadDown,
        PadButton::DPadLeft => G::DPadLeft,
        PadButton::DPadRight => G::DPadRight,
    }
}

/// gilrs's button vocabulary → ours. `None` for buttons we don't model (`C`,
/// `Z`, `Unknown` — legacy pads and unmapped hardware).
fn from_gilrs_button(b: gilrs::Button) -> Option<PadButton> {
    use gilrs::Button as G;
    Some(match b {
        G::South => PadButton::South,
        G::East => PadButton::East,
        G::West => PadButton::West,
        G::North => PadButton::North,
        G::LeftTrigger => PadButton::LeftBumper,
        G::RightTrigger => PadButton::RightBumper,
        G::LeftTrigger2 => PadButton::LeftTrigger,
        G::RightTrigger2 => PadButton::RightTrigger,
        G::Select => PadButton::Select,
        G::Start => PadButton::Start,
        G::Mode => PadButton::Mode,
        G::LeftThumb => PadButton::LeftStick,
        G::RightThumb => PadButton::RightStick,
        G::DPadUp => PadButton::DPadUp,
        G::DPadDown => PadButton::DPadDown,
        G::DPadLeft => PadButton::DPadLeft,
        G::DPadRight => PadButton::DPadRight,
        _ => return None,
    })
}

fn to_gilrs_axis(a: PadAxis) -> gilrs::Axis {
    use gilrs::Axis as G;
    match a {
        PadAxis::LeftStickX => G::LeftStickX,
        PadAxis::LeftStickY => G::LeftStickY,
        PadAxis::RightStickX => G::RightStickX,
        PadAxis::RightStickY => G::RightStickY,
        PadAxis::LeftZ => G::LeftZ,
        PadAxis::RightZ => G::RightZ,
    }
}

/// The device identity a slot is claimed by. Survives disconnect, so a replug
/// reclaims the same slot instead of shuffling the players around.
type Uuid = [u8; 16];

/// The gamepad pump.
pub struct Pads {
    gilrs: Option<gilrs::Gilrs>,
    /// Claimed device per slot; `None` = never claimed. A claimed-but-unplugged
    /// slot keeps its uuid, which is the whole point.
    slots: Vec<Option<Uuid>>,
}

impl Default for Pads {
    fn default() -> Self {
        Self::new()
    }
}

impl Pads {
    /// Start the backend. A failure here (no udev, no permissions, a headless
    /// CI box) is **not** fatal: the editor must still open and the keyboard
    /// must still work, so we log once and run pad-less.
    pub fn new() -> Self {
        let gilrs = match gilrs::Gilrs::new() {
            Ok(g) => Some(g),
            Err(e) => {
                log::warn!("gamepads unavailable ({e}); keyboard and mouse still work");
                None
            }
        };
        let mut slots = Vec::new();
        slots.resize_with(MAX_SLOTS, || None);
        let mut pads = Self { gilrs, slots };
        // Claim whatever is already plugged in, so slot 0 is a real pad from the
        // first frame rather than waiting for someone to press a button.
        let live = pads.live_uuids();
        assign_slots(&mut pads.slots, &live);
        pads
    }

    /// True when the backend came up.
    pub fn available(&self) -> bool {
        self.gilrs.is_some()
    }

    /// Slot → human-readable pad name (`None` for empty or unplugged slots),
    /// for the editor's live-tester device readout.
    pub fn slot_names(&self) -> Vec<Option<String>> {
        let Some(gilrs) = self.gilrs.as_ref() else {
            return vec![None; self.slots.len()];
        };
        self.slots
            .iter()
            .map(|claimed| {
                let uuid = (*claimed)?;
                gilrs
                    .gamepads()
                    .find(|(_, gp)| gp.uuid() == uuid)
                    .map(|(_, gp)| gp.name().to_string())
            })
            .collect()
    }

    /// Every currently-connected device's uuid, in gilrs order.
    fn live_uuids(&self) -> Vec<Uuid> {
        self.gilrs.as_ref().map(|g| g.gamepads().map(|(_, gp)| gp.uuid()).collect()).unwrap_or_default()
    }

    /// Drain this frame's gilrs events and fill `raw`'s pad slots.
    ///
    /// Banked edges are appended to `raw.pressed`/`raw.released` so a button
    /// tapped between two fixed ticks survives, exactly as keyboard edges do.
    /// The caller decides when to clear those.
    pub fn pump(&mut self, raw: &mut RawInput) {
        if self.gilrs.is_none() {
            return;
        }

        // Events FIRST: `next_event` is what advances gilrs's internal gamepad
        // state, so polling levels before draining would read last frame's pose
        // and miss a pad that connected this frame.
        let mut edges: Vec<(gilrs::GamepadId, gilrs::Button, bool)> = Vec::new();
        if let Some(gilrs) = self.gilrs.as_mut() {
            while let Some(ev) = gilrs.next_event() {
                match ev.event {
                    gilrs::EventType::ButtonPressed(b, _) => edges.push((ev.id, b, true)),
                    gilrs::EventType::ButtonReleased(b, _) => edges.push((ev.id, b, false)),
                    // Connect/disconnect need no handling of their own: slot
                    // assignment is derived from the live device list below,
                    // which is authoritative and can't drift out of sync.
                    _ => {}
                }
            }
        }

        // Hot-plug: (re)claim slots from whoever is actually connected now.
        let live: Vec<(gilrs::GamepadId, Uuid)> = self
            .gilrs
            .as_ref()
            .map(|g| g.gamepads().map(|(id, gp)| (id, gp.uuid())).collect())
            .unwrap_or_default();
        let live_uuids: Vec<Uuid> = live.iter().map(|(_, u)| *u).collect();
        assign_slots(&mut self.slots, &live_uuids);

        // Banked edges, resolved through uuid so they land on the right player.
        for (id, btn, down) in edges {
            let Some(b) = from_gilrs_button(btn) else { continue };
            let Some(uuid) = live.iter().find(|(i, _)| *i == id).map(|(_, u)| *u) else { continue };
            let Some(slot) = self.slots.iter().position(|s| *s == Some(uuid)) else { continue };
            let out = if down { &mut raw.pressed } else { &mut raw.released };
            // Bank BOTH forms: an `Any` binding must see the edge too, and once
            // the frame is over the resolver can no longer work out which pad an
            // `Any` binding would have read.
            out.insert(Source::Pad { id: PadId::Slot(slot as u8), ctrl: PadControl::Button(b) });
            out.insert(Source::Pad { id: PadId::Any, ctrl: PadControl::Button(b) });
        }

        // Levels.
        let Some(gilrs) = self.gilrs.as_ref() else { return };
        for slot_idx in 0..self.slots.len() {
            let live_id = self.slots[slot_idx]
                .and_then(|uuid| live.iter().find(|(_, u)| *u == uuid))
                .map(|(id, _)| *id);
            let state = raw.pad_mut(slot_idx as u8);
            let Some(gp) = live_id.and_then(|id| gilrs.connected_gamepad(id)) else {
                // Unplugged reads neutral, never frozen — otherwise a yanked pad
                // holds its last direction down forever.
                *state = PadState::default();
                continue;
            };
            state.connected = true;
            state.buttons.clear();
            for &b in PadButton::ALL {
                if gp.is_pressed(to_gilrs_button(b)) {
                    state.buttons.insert(b);
                }
            }
            for &a in PadAxis::ALL {
                state.axes[a.index()] = gp.value(to_gilrs_axis(a));
            }
        }
    }
}

/// Give every live device a slot, preserving prior claims.
///
/// Pure and free-standing so the sticky-slot rule — the one that decides
/// whether a replugged P2 stays P2 — is testable without a controller.
fn assign_slots(slots: &mut [Option<Uuid>], live: &[Uuid]) {
    for uuid in live {
        if slots.iter().any(|s| s.as_ref() == Some(uuid)) {
            continue; // already owns a slot; keep it
        }
        match slots.iter_mut().find(|s| s.is_none()) {
            Some(free) => *free = Some(*uuid),
            None => log::info!("gamepad connected but all {MAX_SLOTS} player slots are taken"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn button_mapping_round_trips() {
        for &b in PadButton::ALL {
            assert_eq!(from_gilrs_button(to_gilrs_button(b)), Some(b), "{b:?}");
        }
    }

    #[test]
    fn bumpers_and_triggers_are_not_swapped() {
        // gilrs's `LeftTrigger` is the BUMPER and `LeftTrigger2` is the analog
        // trigger. Inverting these is invisible until someone plays the game.
        assert_eq!(to_gilrs_button(PadButton::LeftBumper), gilrs::Button::LeftTrigger);
        assert_eq!(to_gilrs_button(PadButton::LeftTrigger), gilrs::Button::LeftTrigger2);
        assert_eq!(to_gilrs_button(PadButton::RightBumper), gilrs::Button::RightTrigger);
        assert_eq!(to_gilrs_button(PadButton::RightTrigger), gilrs::Button::RightTrigger2);
    }

    #[test]
    fn unmodelled_gilrs_buttons_are_dropped_not_mapped() {
        assert_eq!(from_gilrs_button(gilrs::Button::Unknown), None);
        assert_eq!(from_gilrs_button(gilrs::Button::C), None);
    }

    #[test]
    fn axis_mapping_covers_every_axis() {
        for &a in PadAxis::ALL {
            assert_ne!(to_gilrs_axis(a), gilrs::Axis::Unknown, "{a:?}");
        }
        // Distinct axes must not collapse onto one gilrs axis.
        let mut seen = std::collections::HashSet::new();
        for &a in PadAxis::ALL {
            assert!(seen.insert(to_gilrs_axis(a)), "{a:?} duplicates another axis");
        }
    }

    #[test]
    fn a_backend_failure_is_survivable() {
        // Whatever this box has (or doesn't), constructing must not panic and
        // pumping must leave a usable RawInput.
        let mut pads = Pads::new();
        let mut raw = RawInput::default();
        pads.pump(&mut raw);
        assert!(raw.pads.len() <= MAX_SLOTS);
    }

    #[test]
    fn slot_claims_survive_a_replug() {
        // The scenario that motivates sticky slots: P1's battery dies mid-match.
        // A naive "index into the connected list" mapping would promote P2 to
        // player 1 and hand them someone else's character.
        let (p1, p2) = ([1u8; 16], [2u8; 16]);
        let mut slots: Vec<Option<Uuid>> = vec![None; MAX_SLOTS];

        assign_slots(&mut slots, &[p1, p2]);
        assert_eq!(slots[0], Some(p1));
        assert_eq!(slots[1], Some(p2));

        // P1 drops off: only P2 is live now.
        assign_slots(&mut slots, &[p2]);
        assert_eq!(slots[0], Some(p1), "P1's slot is held, not freed");
        assert_eq!(slots[1], Some(p2), "P2 is not promoted");

        // P1 replugs — same device, so the same slot.
        assign_slots(&mut slots, &[p2, p1]);
        assert_eq!(slots[0], Some(p1));
        assert_eq!(slots[1], Some(p2));
    }

    #[test]
    fn a_new_device_takes_the_lowest_free_slot() {
        let mut slots: Vec<Option<Uuid>> = vec![None; MAX_SLOTS];
        assign_slots(&mut slots, &[[9u8; 16]]);
        assert_eq!(slots[0], Some([9u8; 16]));
        assign_slots(&mut slots, &[[9u8; 16], [8u8; 16]]);
        assert_eq!(slots[1], Some([8u8; 16]));
    }

    #[test]
    fn extra_pads_beyond_the_slot_count_are_ignored_safely() {
        let mut slots: Vec<Option<Uuid>> = vec![None; MAX_SLOTS];
        let live: Vec<Uuid> = (0..MAX_SLOTS as u8 + 3).map(|i| [i; 16]).collect();
        assign_slots(&mut slots, &live);
        assert_eq!(slots.iter().flatten().count(), MAX_SLOTS);
        // …and re-running is idempotent rather than churning the assignment.
        let before = slots.clone();
        assign_slots(&mut slots, &live);
        assert_eq!(slots, before);
    }
}
