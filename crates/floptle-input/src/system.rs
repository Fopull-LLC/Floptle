//! [`InputSystem`] — the single object a host (editor or runtime) owns.
//!
//! It holds the map, one [`ActionRuntime`] per player *per domain*, one
//! [`History`] per player, the context stack, and any armed rebind. Hosts call
//! [`InputSystem::resolve_frame`] once per rendered frame and
//! [`InputSystem::resolve_tick`] once per fixed tick, then read resolved state
//! by name.
//!
//! ## Why two domains
//!
//! `update` runs per rendered frame; `fixedUpdate` runs per fixed tick. They
//! advance at different rates, so a single runtime would let whichever ran first
//! eat the other's edges — press Jump on a frame between two ticks and
//! `fixedUpdate` would never see it. Each domain therefore gets its own
//! runtime, and only the tick domain feeds [`History`], because motion windows
//! are counted in ticks and must not vary with framerate.

use crate::context::{AllowMask, Context, ContextStack};
use crate::history::{dir_of, History};
use crate::map::InputMap;
use crate::raw::RawInput;
use crate::rebind::{BindFilter, Capture};
use crate::runtime::{ActionRuntime, ActionState};

/// Which sampling domain a query refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Domain {
    /// Per rendered frame — what `update` reads.
    Frame,
    /// Per fixed tick — what `fixedUpdate` reads, and the only domain with
    /// history (buffering, motions).
    Tick,
}

/// An armed press-to-bind request.
#[derive(Clone, Debug)]
pub struct PendingRebind {
    pub action: String,
    pub slot: u8,
    pub filter: BindFilter,
    /// Set once something was captured; the UI confirms or discards it.
    pub captured: Option<Capture>,
}

/// Everything the host owns for input.
pub struct InputSystem {
    map: InputMap,
    frame: Vec<ActionRuntime>,
    tick: Vec<ActionRuntime>,
    frame_state: Vec<ActionState>,
    tick_state: Vec<ActionState>,
    history: Vec<History>,
    /// Per player: +1 facing forward, −1 mirrored. A fighter flips this on
    /// cross-up so `motion("qcf")` keeps meaning "toward the opponent" — the
    /// engine has no opinion about who is facing where, so the game sets it.
    facing: Vec<f32>,
    contexts: ContextStack,
    rebind: Option<PendingRebind>,
}

impl Default for InputSystem {
    fn default() -> Self {
        Self::new(InputMap::default())
    }
}

impl InputSystem {
    pub fn new(map: InputMap) -> Self {
        let mut s = Self {
            map,
            frame: Vec::new(),
            tick: Vec::new(),
            frame_state: Vec::new(),
            tick_state: Vec::new(),
            history: Vec::new(),
            facing: Vec::new(),
            contexts: ContextStack::default(),
            rebind: None,
        };
        s.size_to_players();
        s
    }

    pub fn map(&self) -> &InputMap {
        &self.map
    }

    pub fn map_mut(&mut self) -> &mut InputMap {
        &mut self.map
    }

    /// Swap in a reloaded map (the file changed, or the Input settings edited
    /// it). All per-player state resets: indices may have moved, so keeping the
    /// old held mask would report the wrong actions as down.
    pub fn set_map(&mut self, map: InputMap) {
        self.map = map;
        self.size_to_players();
        self.reset();
    }

    /// How many local players this project declares (at least one).
    pub fn players(&self) -> usize {
        (self.map.players as usize).max(1)
    }

    fn size_to_players(&mut self) {
        let n = self.players();
        self.frame.resize_with(n, ActionRuntime::new);
        self.tick.resize_with(n, ActionRuntime::new);
        self.frame_state.resize_with(n, ActionState::default);
        self.tick_state.resize_with(n, ActionState::default);
        self.history.resize_with(n, History::new);
        self.facing.resize(n, 1.0);
    }

    /// Drop all edge, hold and history state. Used when Play starts or stops and
    /// when the game view loses focus, so a key held in the editor doesn't
    /// register as a press inside the game (and vice versa).
    pub fn reset(&mut self) {
        self.frame.iter_mut().for_each(ActionRuntime::reset);
        self.tick.iter_mut().for_each(ActionRuntime::reset);
        self.frame_state.iter_mut().for_each(|s| *s = ActionState::default());
        self.tick_state.iter_mut().for_each(|s| *s = ActionState::default());
        self.history.iter_mut().for_each(History::clear);
    }

    // --- contexts ---------------------------------------------------------

    pub fn contexts(&self) -> &ContextStack {
        &self.contexts
    }

    pub fn push_context(&mut self, ctx: Context) {
        self.contexts.push(ctx);
    }

    pub fn pop_context(&mut self, name: &str) -> bool {
        self.contexts.pop(name)
    }

    pub fn clear_contexts(&mut self) {
        self.contexts.clear();
    }

    // --- facing -----------------------------------------------------------

    /// Set which way player `slot` is facing (`+1` or `-1`). Directions are
    /// mirrored before they reach the history, so motion inputs stay written
    /// from the character's point of view.
    pub fn set_facing(&mut self, slot: u8, facing: f32) {
        if let Some(f) = self.facing.get_mut(slot as usize) {
            *f = if facing < 0.0 { -1.0 } else { 1.0 };
        }
    }

    pub fn facing(&self, slot: u8) -> f32 {
        self.facing.get(slot as usize).copied().unwrap_or(1.0)
    }

    // --- resolution -------------------------------------------------------

    /// Resolve every player for the render-frame domain.
    pub fn resolve_frame(&mut self, raw: &RawInput, dt: f32) {
        let allow = self.contexts.allow_mask(&self.map);
        for slot in 0..self.players() {
            self.frame_state[slot] =
                self.frame[slot].resolve(&self.map, raw, slot as u8, dt, allow);
        }
    }

    /// Resolve every player for the fixed-tick domain and advance their history.
    pub fn resolve_tick(&mut self, raw: &RawInput, dt: f32) {
        let allow = self.contexts.allow_mask(&self.map);
        let motion_axis = self.map.motion_axis_index();
        let n_actions = self.map.actions.len();
        for slot in 0..self.players() {
            let state = self.tick[slot].resolve(&self.map, raw, slot as u8, dt, allow);
            let dir = match motion_axis {
                Some(i) => {
                    let (x, y) = state.axis2(i);
                    dir_of(x * self.facing[slot], y)
                }
                None => 5,
            };
            self.history[slot].push(state.held, state.just_pressed, dir, n_actions);
            self.tick_state[slot] = state;
        }
    }

    /// Resolved state for a player in a domain.
    pub fn state(&self, domain: Domain, slot: u8) -> &ActionState {
        let states = match domain {
            Domain::Frame => &self.frame_state,
            Domain::Tick => &self.tick_state,
        };
        states.get(slot as usize).unwrap_or(&states[0])
    }

    /// Overwrite a domain's resolved state directly.
    ///
    /// The netcode path needs this: a replayed or remote tick arrives already
    /// resolved from the wire, so it bypasses device resolution entirely. The
    /// history still advances, because a replay must reproduce the same motion
    /// and buffer answers the original client saw.
    pub fn set_tick_state(&mut self, slot: u8, state: ActionState) {
        let Some(i) = (slot as usize).lt(&self.players()).then_some(slot as usize) else { return };
        let motion_axis = self.map.motion_axis_index();
        let dir = match motion_axis {
            Some(a) => {
                let (x, y) = state.axis2(a);
                dir_of(x * self.facing[i], y)
            }
            None => 5,
        };
        self.history[i].push(state.held, state.just_pressed, dir, self.map.actions.len());
        self.tick_state[i] = state;
    }

    pub fn history(&self, slot: u8) -> &History {
        self.history.get(slot as usize).unwrap_or(&self.history[0])
    }

    // --- named queries ----------------------------------------------------

    pub fn action(&self, domain: Domain, slot: u8, name: &str) -> bool {
        self.map.action_index(name).is_some_and(|i| self.state(domain, slot).is_held(i))
    }

    pub fn just_pressed(&self, domain: Domain, slot: u8, name: &str) -> bool {
        self.map.action_index(name).is_some_and(|i| self.state(domain, slot).is_just_pressed(i))
    }

    pub fn just_released(&self, domain: Domain, slot: u8, name: &str) -> bool {
        self.map.action_index(name).is_some_and(|i| self.state(domain, slot).is_just_released(i))
    }

    pub fn held_secs(&self, domain: Domain, slot: u8, name: &str) -> f32 {
        self.map.action_index(name).map_or(0.0, |i| self.state(domain, slot).secs(i))
    }

    pub fn axis1(&self, domain: Domain, slot: u8, name: &str) -> f32 {
        self.map.axis1_index(name).map_or(0.0, |i| self.state(domain, slot).axis1(i))
    }

    pub fn axis2(&self, domain: Domain, slot: u8, name: &str) -> (f32, f32) {
        self.map.axis2_index(name).map_or((0.0, 0.0), |i| self.state(domain, slot).axis2(i))
    }

    /// The player's current numpad direction (tick domain).
    pub fn dir(&self, slot: u8) -> u8 {
        self.history(slot).dir()
    }

    /// Was `name` pressed within the last `within` ticks and not yet consumed?
    pub fn buffered(&self, slot: u8, name: &str, within: u32) -> bool {
        self.map.action_index(name).is_some_and(|i| self.history(slot).buffered(i, within))
    }

    /// Spend a buffered press so it fires once.
    pub fn consume(&mut self, slot: u8, name: &str, within: u32) -> bool {
        let Some(i) = self.map.action_index(name) else { return false };
        let idx = (slot as usize).min(self.history.len().saturating_sub(1));
        self.history[idx].consume(i, within)
    }

    /// Has the named motion been completed recently? `window` overrides the
    /// map's default when given.
    pub fn motion(&self, slot: u8, name: &str, window: Option<u16>) -> bool {
        self.map.motion(name).is_some_and(|m| self.history(slot).motion(m, window))
    }

    // --- rebinding --------------------------------------------------------

    /// Arm a capture for `action` on `slot`.
    pub fn start_rebind(&mut self, action: impl Into<String>, slot: u8, filter: BindFilter) {
        self.rebind = Some(PendingRebind { action: action.into(), slot, filter, captured: None });
    }

    pub fn pending_rebind(&self) -> Option<&PendingRebind> {
        self.rebind.as_ref()
    }

    pub fn cancel_rebind(&mut self) {
        self.rebind = None;
    }

    /// Feed a window to an armed capture. Returns the capture once something is
    /// pressed; the caller decides whether to commit it.
    pub fn poll_rebind(&mut self, raw: &RawInput) -> Option<Capture> {
        let pending = self.rebind.as_ref()?;
        let multiplayer = self.players() > 1;
        let got = crate::rebind::capture(raw, pending.filter, pending.slot, multiplayer)?;
        if let Some(p) = self.rebind.as_mut() {
            p.captured = Some(got.clone());
        }
        Some(got)
    }

    /// Append the captured binding to its action and disarm.
    ///
    /// Returns false when the action has since vanished from the map, or when
    /// the binding is already present — rebinding to a key that's already bound
    /// should be a no-op, not a duplicate chip.
    pub fn commit_rebind(&mut self, capture: Capture) -> bool {
        let Some(pending) = self.rebind.take() else { return false };
        let binding = capture.binding();
        let Some(action) = self.map.actions.iter_mut().find(|a| a.name == pending.action) else {
            return false;
        };
        if action.bindings.contains(&binding) {
            return false;
        }
        action.bindings.push(binding);
        true
    }

    /// The context mask currently in force — the Input settings' live tester
    /// greys out actions a context is swallowing.
    pub fn allow_mask(&self) -> AllowMask {
        self.contexts.allow_mask(&self.map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Action, Axis2, Axis2Binding, Binding, Motion, Socd};
    use crate::source::{Key, Source};

    fn fighter_map(players: u8) -> InputMap {
        InputMap {
            actions: vec![
                Action { name: "Punch".into(), bindings: vec![Binding::new(Source::Key(Key::KeyJ))] },
                Action { name: "Kick".into(), bindings: vec![Binding::new(Source::Key(Key::KeyK))] },
            ],
            axes2: vec![Axis2 {
                name: "Move".into(),
                socd: Socd::Neutral,
                bindings: vec![Axis2Binding::Keys {
                    up: Source::Key(Key::KeyW),
                    down: Source::Key(Key::KeyS),
                    left: Source::Key(Key::KeyA),
                    right: Source::Key(Key::KeyD),
                }],
            }],
            axes1: vec![],
            motions: vec![Motion { name: "qcf".into(), dirs: vec![2, 3, 6], window: 12, charge: 0 }],
            players,
            motion_axis: None,
        }
    }

    fn keys(k: &[Key]) -> RawInput {
        RawInput { keys: k.iter().copied().collect(), ..Default::default() }
    }

    #[test]
    fn the_two_domains_do_not_eat_each_others_edges() {
        // The bug this prevents: a press seen by `update` is gone by the time
        // `fixedUpdate` runs, so the jump never happens.
        let mut sys = InputSystem::new(fighter_map(1));
        let down = keys(&[Key::KeyJ]);

        sys.resolve_frame(&down, 0.016);
        assert!(sys.just_pressed(Domain::Frame, 0, "Punch"));

        sys.resolve_tick(&down, 0.016);
        assert!(
            sys.just_pressed(Domain::Tick, 0, "Punch"),
            "the tick domain gets its own edge"
        );

        // …and running frame again does not re-fire either.
        sys.resolve_frame(&down, 0.016);
        assert!(!sys.just_pressed(Domain::Frame, 0, "Punch"));
        assert!(sys.action(Domain::Frame, 0, "Punch"), "still held, though");
    }

    #[test]
    fn only_the_tick_domain_records_history() {
        let mut sys = InputSystem::new(fighter_map(1));
        for _ in 0..30 {
            sys.resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        }
        assert!(!sys.buffered(0, "Punch", 4), "frames must not fill the buffer");
        sys.resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(sys.buffered(0, "Punch", 4));
    }

    #[test]
    fn a_quarter_circle_punch_reads_end_to_end() {
        let mut sys = InputSystem::new(fighter_map(1));
        // down, down-forward, forward — then the button.
        for k in [
            vec![Key::KeyS],
            vec![Key::KeyS, Key::KeyD],
            vec![Key::KeyD],
        ] {
            sys.resolve_tick(&keys(&k), 0.016);
        }
        assert!(sys.motion(0, "qcf", None), "motion recognised");
        sys.resolve_tick(&keys(&[Key::KeyD, Key::KeyJ]), 0.016);
        assert!(sys.buffered(0, "Punch", 4));
        assert!(sys.motion(0, "qcf", None), "still inside the window when the button lands");
    }

    #[test]
    fn facing_mirrors_motion_directions() {
        // A player who crossed over presses "left" but means "forward".
        let mut sys = InputSystem::new(fighter_map(1));
        sys.set_facing(0, -1.0);
        for k in [vec![Key::KeyS], vec![Key::KeyS, Key::KeyA], vec![Key::KeyA]] {
            sys.resolve_tick(&keys(&k), 0.016);
        }
        assert!(sys.motion(0, "qcf", None), "qcf is toward the opponent, not toward screen-right");
    }

    #[test]
    fn consuming_makes_a_buffered_press_fire_once() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(sys.buffered(0, "Punch", 6));
        assert!(sys.consume(0, "Punch", 6));
        assert!(!sys.buffered(0, "Punch", 6));
    }

    #[test]
    fn players_resolve_and_remember_independently() {
        let mut sys = InputSystem::new(fighter_map(2));
        assert_eq!(sys.players(), 2);
        // Both players are on the keyboard here, so both see the press — what
        // matters is that their history and consume state are separate.
        sys.resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(sys.buffered(0, "Punch", 4));
        assert!(sys.buffered(1, "Punch", 4));
        sys.consume(0, "Punch", 4);
        assert!(!sys.buffered(0, "Punch", 4));
        assert!(sys.buffered(1, "Punch", 4), "P1 consuming must not spend P2's press");
    }

    #[test]
    fn unknown_names_answer_falsely_rather_than_panicking() {
        let sys = InputSystem::new(fighter_map(1));
        assert!(!sys.action(Domain::Frame, 0, "Nope"));
        assert!(!sys.just_pressed(Domain::Tick, 0, "Nope"));
        assert_eq!(sys.axis2(Domain::Frame, 0, "Nope"), (0.0, 0.0));
        assert_eq!(sys.axis1(Domain::Frame, 0, "Nope"), 0.0);
        assert!(!sys.motion(0, "nope", None));
        assert!(!sys.buffered(0, "Nope", 4));
    }

    #[test]
    fn an_out_of_range_slot_falls_back_to_player_one() {
        let sys = InputSystem::new(fighter_map(1));
        // Must not panic — a script can pass any number to input.player(n).
        assert!(!sys.action(Domain::Frame, 9, "Punch"));
        assert_eq!(sys.dir(9), 5);
    }

    #[test]
    fn a_context_swallows_input_until_popped() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.push_context(Context::consuming("menu", 100, &["Kick"]));
        sys.resolve_frame(&keys(&[Key::KeyJ, Key::KeyK]), 0.016);
        assert!(!sys.action(Domain::Frame, 0, "Punch"), "swallowed");
        assert!(sys.action(Domain::Frame, 0, "Kick"), "enabled by the menu context");

        sys.pop_context("menu");
        sys.resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        assert!(sys.action(Domain::Frame, 0, "Punch"));
    }

    #[test]
    fn reset_clears_history_and_edges() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        sys.reset();
        assert!(!sys.buffered(0, "Punch", 8));
        assert!(!sys.action(Domain::Tick, 0, "Punch"));
    }

    #[test]
    fn reloading_a_map_resets_state_and_resizes_players() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        sys.set_map(fighter_map(2));
        assert_eq!(sys.players(), 2);
        assert!(!sys.buffered(0, "Punch", 8), "stale state cannot survive a reindex");
    }

    #[test]
    fn a_rebind_captures_and_commits() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.start_rebind("Punch", 0, BindFilter::AnyButton);
        assert!(sys.pending_rebind().is_some());

        let got = sys.poll_rebind(&keys(&[Key::KeyU])).expect("captured");
        assert!(sys.commit_rebind(got));
        assert!(sys.pending_rebind().is_none());

        // The new binding works…
        sys.resolve_frame(&keys(&[Key::KeyU]), 0.016);
        assert!(sys.action(Domain::Frame, 0, "Punch"));
        // …and the old one still does, since rebinding appends.
        sys.reset();
        sys.resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        assert!(sys.action(Domain::Frame, 0, "Punch"));
    }

    #[test]
    fn rebinding_to_an_existing_binding_is_a_no_op() {
        let mut sys = InputSystem::new(fighter_map(1));
        sys.start_rebind("Punch", 0, BindFilter::AnyButton);
        let got = sys.poll_rebind(&keys(&[Key::KeyJ])).unwrap();
        assert!(!sys.commit_rebind(got), "no duplicate chip");
        assert_eq!(sys.map().actions[0].bindings.len(), 1);
    }

    #[test]
    fn cancelling_a_rebind_changes_nothing() {
        let mut sys = InputSystem::new(fighter_map(1));
        let before = sys.map().clone();
        sys.start_rebind("Punch", 0, BindFilter::AnyButton);
        sys.cancel_rebind();
        assert!(sys.poll_rebind(&keys(&[Key::KeyU])).is_none());
        assert_eq!(sys.map(), &before);
    }

    #[test]
    fn replayed_state_still_advances_history() {
        // Netcode: a tick arrives already resolved, but a rollback replay must
        // reproduce the same motion and buffer answers.
        let mut sys = InputSystem::new(fighter_map(1));
        let punch = ActionState {
            held: 0b1,
            just_pressed: 0b1,
            just_released: 0,
            held_secs: vec![0.016, 0.0],
            axes1: vec![],
            axes2: vec![(0.0, 0.0)],
        };
        sys.set_tick_state(0, punch);
        assert!(sys.action(Domain::Tick, 0, "Punch"));
        assert!(sys.buffered(0, "Punch", 4));
    }
}
