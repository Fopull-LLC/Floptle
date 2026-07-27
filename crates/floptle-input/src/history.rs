//! The fighter layer — input history, buffering, and motion recognition.
//!
//! The action layer answers "is Punch down *now*". A fighting game needs to ask
//! harder questions: "was Punch pressed within the last 4 frames, and had the
//! player finished a quarter-circle by then?" Those need memory, so [`History`]
//! keeps a ring of recent ticks per player.
//!
//! **Tick domain only.** This is fed from the fixed-step runtime, never the
//! render frame. Motion windows are counted in ticks, so a player on a 144 Hz
//! monitor and one on 60 Hz must get identical leniency, and a rollback replay
//! must reproduce the same answer.
//!
//! ## Numpad notation
//!
//! Directions use the genre's standard numpad layout, from the character's own
//! point of view:
//!
//! ```text
//!   7 8 9        up-back    up     up-forward
//!   4 5 6   =    back     neutral    forward
//!   1 2 3        dn-back   down   dn-forward
//! ```

use crate::map::Motion;

/// How many ticks of history we keep — 3 seconds at 60 Hz. Comfortably covers
/// the longest charge move plus its release window.
pub const HISTORY_TICKS: usize = 180;

/// Below this magnitude the stick reads neutral (`5`) rather than a direction.
///
/// Deliberately high: a fighter's motion inputs must not fire because someone
/// rested a thumb on the stick, and a *held* direction should feel like a
/// commitment. This is separate from an axis binding's deadzone, which exists
/// to remove hardware drift.
pub const DIRECTION_THRESHOLD: f32 = 0.5;

/// The numpad direction for an analog stick position.
///
/// `x` is positive toward the character's forward, `y` positive up. A game whose
/// character has turned around flips `x` before calling this — the engine has no
/// opinion about which way anybody is facing.
pub fn dir_of(x: f32, y: f32) -> u8 {
    let dx = if x > DIRECTION_THRESHOLD {
        1i8
    } else if x < -DIRECTION_THRESHOLD {
        -1
    } else {
        0
    };
    let dy = if y > DIRECTION_THRESHOLD {
        1i8
    } else if y < -DIRECTION_THRESHOLD {
        -1
    } else {
        0
    };
    (5 + dx + 3 * dy) as u8
}

/// One tick of remembered input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Frame {
    dir: u8,
    /// Actions that went down on this tick.
    pressed: u64,
    /// Actions held on this tick.
    held: u64,
}

/// One player's rolling input history.
#[derive(Clone, Debug)]
pub struct History {
    ring: Vec<Frame>,
    /// Absolute tick count; also the write cursor (`tick % HISTORY_TICKS`).
    tick: u64,
    /// Absolute tick of the most recent press per action (`None` = never).
    last_press: Vec<Option<u64>>,
    /// Absolute tick of the press most recently handed to `consume`.
    consumed: Vec<Option<u64>>,
}

impl Default for History {
    fn default() -> Self {
        Self::new()
    }
}

impl History {
    pub fn new() -> Self {
        Self {
            ring: vec![Frame::default(); HISTORY_TICKS],
            tick: 0,
            last_press: Vec::new(),
            consumed: Vec::new(),
        }
    }

    /// The number of ticks recorded so far — the current absolute tick.
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// Wipe all memory. Called on entering Play or changing scene, so a motion
    /// half-completed before the match can't fire inside it.
    pub fn clear(&mut self) {
        self.ring.iter_mut().for_each(|f| *f = Frame::default());
        self.tick = 0;
        self.last_press.iter_mut().for_each(|p| *p = None);
        self.consumed.iter_mut().for_each(|c| *c = None);
    }

    /// Record one tick. `dir` is this tick's numpad direction (see [`dir_of`]).
    pub fn push(&mut self, held: u64, pressed: u64, dir: u8, action_count: usize) {
        if self.last_press.len() < action_count {
            self.last_press.resize(action_count, None);
            self.consumed.resize(action_count, None);
        }
        self.ring[(self.tick % HISTORY_TICKS as u64) as usize] = Frame { dir, pressed, held };
        for (i, slot) in self.last_press.iter_mut().enumerate() {
            if i < 64 && pressed & (1u64 << i) != 0 {
                *slot = Some(self.tick);
            }
        }
        self.tick += 1;
    }

    /// This tick's direction.
    pub fn dir(&self) -> u8 {
        self.frame_ago(0).map(|f| f.dir).unwrap_or(5)
    }

    /// How many consecutive ticks direction `dir` has been held, counting back
    /// from the most recent tick. Zero when it isn't currently held.
    pub fn dir_held_ticks(&self, dir: u8) -> u32 {
        let mut n = 0;
        while let Some(f) = self.frame_ago(n) {
            if f.dir != dir {
                break;
            }
            n += 1;
        }
        n
    }

    /// Was action `i` pressed within the last `within` ticks, without that press
    /// having been consumed?
    ///
    /// This is the input buffer: a player who hits Punch two frames before their
    /// recovery ends still gets the punch. `within = 1` means "this tick only".
    pub fn buffered(&self, i: usize, within: u32) -> bool {
        self.pending_press(i, within).is_some()
    }

    /// Spend the buffered press for action `i` so it can only fire once.
    ///
    /// Without this a 4-tick buffer would fire an attack on all four ticks. The
    /// caller checks [`History::buffered`], acts, and then consumes.
    /// Returns whether there was anything to consume.
    pub fn consume(&mut self, i: usize, within: u32) -> bool {
        match self.pending_press(i, within) {
            Some(t) => {
                if self.consumed.len() <= i {
                    self.consumed.resize(i + 1, None);
                }
                self.consumed[i] = Some(t);
                true
            }
            None => false,
        }
    }

    /// The tick of an unconsumed press of `i` inside the window, if any.
    fn pending_press(&self, i: usize, within: u32) -> Option<u64> {
        let t = (*self.last_press.get(i)?)?;
        // `tick` is the NEXT slot to write, so the tick just recorded is
        // `tick - 1` and a press there has age 0. `within = 1` is therefore the
        // most recent tick alone, and `within = 4` covers ages 0..=3.
        let age = self.tick.checked_sub(1)?.checked_sub(t)?;
        if age >= within as u64 || age >= HISTORY_TICKS as u64 {
            return None;
        }
        match self.consumed.get(i).copied().flatten() {
            Some(c) if c >= t => None,
            _ => Some(t),
        }
    }

    /// Has `motion` been completed within its window, ending recently?
    ///
    /// Matching walks backwards from the newest tick, taking the motion's
    /// directions in reverse and skipping ticks that don't advance it. That
    /// tolerates the extra ticks a real player spends passing through a
    /// direction, while still requiring every listed direction to occur *in
    /// order* — `2, 6` alone never satisfies a quarter-circle, because the
    /// diagonal is missing.
    pub fn motion(&self, motion: &Motion, window_override: Option<u16>) -> bool {
        if motion.dirs.is_empty() {
            return false;
        }
        let window = window_override.unwrap_or(motion.window).min(HISTORY_TICKS as u16);
        let mut need = motion.dirs.len();
        let mut age = 0u32;
        while age < window as u32 {
            let Some(f) = self.frame_ago(age) else { break };
            if f.dir == motion.dirs[need - 1] {
                need -= 1;
                if need == 0 {
                    break;
                }
            }
            age += 1;
        }
        if need != 0 {
            return false;
        }
        if motion.charge == 0 {
            return true;
        }
        // A charge move additionally requires the FIRST direction to have been
        // held for `charge` ticks before the sequence ran. `age` is sitting on
        // the tick where that direction was matched, so count back from there.
        //
        // The hold must be that exact direction — a game wanting the usual
        // "down-back also charges down" leniency composes it from
        // `dirHeldTicks`, rather than the engine guessing a convention.
        let mut held = 0u32;
        while let Some(f) = self.frame_ago(age + held) {
            if f.dir != motion.dirs[0] {
                break;
            }
            held += 1;
        }
        held >= motion.charge as u32
    }

    /// Was action `i` held `n` ticks ago? Lets a script hand-roll leniency rules
    /// the built-ins don't cover.
    pub fn held_ago(&self, i: usize, n: u32) -> bool {
        i < 64 && self.frame_ago(n).is_some_and(|f| f.held & (1u64 << i) != 0)
    }

    /// The frame `n` ticks before the most recent one (`n = 0` is newest).
    /// `None` once `n` runs past what we've recorded or past the ring.
    fn frame_ago(&self, n: u32) -> Option<Frame> {
        let n = n as u64;
        if n >= HISTORY_TICKS as u64 {
            return None;
        }
        // `tick` is the NEXT slot to write, so the newest frame is at tick - 1.
        let abs = self.tick.checked_sub(n + 1)?;
        Some(self.ring[(abs % HISTORY_TICKS as u64) as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed a sequence of directions with no buttons pressed.
    fn feed(h: &mut History, dirs: &[u8]) {
        for &d in dirs {
            h.push(0, 0, d, 4);
        }
    }

    fn motion(dirs: &[u8], window: u16) -> Motion {
        Motion { name: "m".into(), dirs: dirs.to_vec(), window, charge: 0 }
    }

    #[test]
    fn numpad_directions_cover_the_grid() {
        let t = DIRECTION_THRESHOLD + 0.1;
        assert_eq!(dir_of(0.0, 0.0), 5);
        assert_eq!(dir_of(t, 0.0), 6);
        assert_eq!(dir_of(-t, 0.0), 4);
        assert_eq!(dir_of(0.0, t), 8);
        assert_eq!(dir_of(0.0, -t), 2);
        assert_eq!(dir_of(t, t), 9);
        assert_eq!(dir_of(-t, t), 7);
        assert_eq!(dir_of(t, -t), 3);
        assert_eq!(dir_of(-t, -t), 1);
    }

    #[test]
    fn a_light_lean_is_neutral_not_a_direction() {
        assert_eq!(dir_of(0.4, 0.0), 5, "resting a thumb must not read as forward");
        assert_eq!(dir_of(0.0, -0.49), 5);
    }

    #[test]
    fn qcf_matches_a_clean_input() {
        let mut h = History::new();
        feed(&mut h, &[5, 5, 2, 3, 6]);
        assert!(h.motion(&motion(&[2, 3, 6], 12), None));
    }

    #[test]
    fn qcf_tolerates_repeated_ticks_per_direction() {
        // What a real player actually produces at 60 Hz.
        let mut h = History::new();
        feed(&mut h, &[5, 2, 2, 2, 3, 3, 6, 6]);
        assert!(h.motion(&motion(&[2, 3, 6], 12), None));
    }

    #[test]
    fn qcf_rejects_a_skipped_diagonal() {
        // 2 then 6 with no 3 between is NOT a quarter circle — this is the
        // single most important negative case.
        let mut h = History::new();
        feed(&mut h, &[5, 5, 2, 2, 6, 6]);
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None));
    }

    #[test]
    fn qcf_rejects_the_reverse_order() {
        let mut h = History::new();
        feed(&mut h, &[6, 3, 2]);
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None));
    }

    #[test]
    fn a_motion_expires_outside_its_window() {
        let mut h = History::new();
        feed(&mut h, &[2, 3, 6]);
        assert!(h.motion(&motion(&[2, 3, 6], 12), None), "fresh");
        feed(&mut h, &[5; 20]);
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None), "stale after 20 idle ticks");
    }

    #[test]
    fn a_motion_survives_a_few_ticks_of_slack_after_completing() {
        // The player finishes the quarter-circle then presses the button two
        // ticks later — that must still count.
        let mut h = History::new();
        feed(&mut h, &[2, 3, 6, 5, 5]);
        assert!(h.motion(&motion(&[2, 3, 6], 12), None));
    }

    #[test]
    fn an_explicit_window_overrides_the_map() {
        let mut h = History::new();
        feed(&mut h, &[2, 3, 6, 5, 5, 5, 5, 5, 5]);
        assert!(h.motion(&motion(&[2, 3, 6], 30), None), "generous window matches");
        assert!(!h.motion(&motion(&[2, 3, 6], 30), Some(3)), "a tight override does not");
    }

    #[test]
    fn dp_and_qcf_are_told_apart() {
        let dp = motion(&[6, 2, 3], 14);
        let qcf = motion(&[2, 3, 6], 12);

        let mut h = History::new();
        feed(&mut h, &[6, 2, 3]);
        assert!(h.motion(&dp, None));
        assert!(!h.motion(&qcf, None), "a dragon punch is not a quarter circle");

        let mut h = History::new();
        feed(&mut h, &[2, 3, 6]);
        assert!(h.motion(&qcf, None));
    }

    #[test]
    fn half_circles_match() {
        let mut h = History::new();
        feed(&mut h, &[4, 4, 1, 2, 2, 3, 6]);
        assert!(h.motion(&motion(&[4, 1, 2, 3, 6], 22), None));
    }

    #[test]
    fn charge_requires_the_hold() {
        let charge = Motion { name: "chargeF".into(), dirs: vec![4, 6], window: 10, charge: 40 };

        // Tapping back then forward is not a charge move.
        let mut h = History::new();
        feed(&mut h, &[4, 4, 6]);
        assert!(!h.motion(&charge, None), "3 ticks of back is not a charge");

        // Holding back long enough is.
        let mut h = History::new();
        feed(&mut h, &[4; 45]);
        feed(&mut h, &[6]);
        assert!(h.motion(&charge, None));
    }

    #[test]
    fn charge_is_spent_by_leaving_the_direction() {
        let charge = Motion { name: "chargeF".into(), dirs: vec![4, 6], window: 10, charge: 40 };
        let mut h = History::new();
        feed(&mut h, &[4; 45]);
        feed(&mut h, &[5, 5, 5]); // let go of back…
        feed(&mut h, &[4, 4]); //    …then a brief re-press
        feed(&mut h, &[6]);
        assert!(!h.motion(&charge, None), "the charge must not survive going neutral");
    }

    #[test]
    fn buffering_fires_within_the_window_and_stops_after() {
        // A 4-tick buffer covers the press tick plus the next three.
        let mut h = History::new();
        h.push(1, 1, 5, 4); // Punch pressed — age 0
        assert!(h.buffered(0, 4));
        for _ in 0..3 {
            h.push(0, 0, 5, 4); // ages 1, 2, 3
            assert!(h.buffered(0, 4), "still inside a 4-tick buffer");
        }
        h.push(0, 0, 5, 4); // age 4
        assert!(!h.buffered(0, 4), "expired");
    }

    #[test]
    fn a_one_tick_buffer_means_this_tick_only() {
        let mut h = History::new();
        h.push(1, 1, 5, 4);
        assert!(h.buffered(0, 1));
        h.push(0, 0, 5, 4);
        assert!(!h.buffered(0, 1));
    }

    #[test]
    fn consuming_a_press_fires_it_exactly_once() {
        // Without consume, a 4-tick buffer would fire the attack 4 times.
        let mut h = History::new();
        h.push(1, 1, 5, 4);
        assert!(h.buffered(0, 4));
        assert!(h.consume(0, 4));
        assert!(!h.buffered(0, 4), "spent");
        h.push(0, 0, 5, 4);
        assert!(!h.buffered(0, 4), "still spent on later ticks");
        assert!(!h.consume(0, 4), "nothing left to consume");
    }

    #[test]
    fn a_new_press_after_consuming_buffers_again() {
        let mut h = History::new();
        h.push(1, 1, 5, 4);
        h.consume(0, 4);
        h.push(0, 0, 5, 4);
        h.push(1, 1, 5, 4); // pressed again
        assert!(h.buffered(0, 4), "a fresh press is not covered by the old consume");
    }

    #[test]
    fn actions_buffer_independently() {
        let mut h = History::new();
        h.push(0b11, 0b11, 5, 4); // Punch (0) and Kick (1) together
        assert!(h.consume(0, 4));
        assert!(!h.buffered(0, 4));
        assert!(h.buffered(1, 4), "consuming Punch must not spend Kick");
    }

    #[test]
    fn direction_hold_time_counts_back_from_now() {
        let mut h = History::new();
        feed(&mut h, &[4; 10]);
        assert_eq!(h.dir_held_ticks(4), 10);
        assert_eq!(h.dir_held_ticks(6), 0);
        feed(&mut h, &[5]);
        assert_eq!(h.dir_held_ticks(4), 0, "broken by neutral");
    }

    #[test]
    fn history_wraps_without_reporting_stale_frames() {
        let mut h = History::new();
        feed(&mut h, &[2, 3, 6]);
        // Push well past the ring size; the old motion must not resurface.
        feed(&mut h, &[5; HISTORY_TICKS + 50]);
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None));
        assert_eq!(h.dir(), 5);
        assert!(h.frame_ago(HISTORY_TICKS as u32).is_none(), "the ring bounds lookback");
    }

    #[test]
    fn an_empty_history_answers_everything_negatively() {
        let h = History::new();
        assert_eq!(h.dir(), 5);
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None));
        assert!(!h.buffered(0, 4));
        assert_eq!(h.dir_held_ticks(5), 0);
    }

    #[test]
    fn clear_wipes_a_half_finished_motion() {
        let mut h = History::new();
        feed(&mut h, &[2, 3, 6]);
        h.push(1, 1, 6, 4);
        h.clear();
        assert!(!h.motion(&motion(&[2, 3, 6], 12), None));
        assert!(!h.buffered(0, 4));
        assert_eq!(h.tick(), 0);
    }

    #[test]
    fn an_empty_motion_never_matches() {
        let mut h = History::new();
        feed(&mut h, &[5, 5, 5]);
        assert!(!h.motion(&motion(&[], 12), None));
    }

    #[test]
    fn held_ago_reads_back_through_the_ring() {
        let mut h = History::new();
        h.push(0b1, 0b1, 5, 4);
        h.push(0b1, 0, 5, 4);
        h.push(0, 0, 5, 4);
        assert!(!h.held_ago(0, 0), "released on the newest tick");
        assert!(h.held_ago(0, 1));
        assert!(h.held_ago(0, 2));
    }
}
