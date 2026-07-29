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

/// The tick domain's whole state, for a rollback's state ring.
///
/// Cloned rather than rewound: `History` carries absolute tick cursors plus the
/// per-action "already consumed" marks, and those cannot be reconstructed by
/// winding a cursor backwards. It is ~3 KB per player, which is nothing next to
/// being correct.
#[derive(Clone)]
pub struct TickSnapshot {
    runtimes: Vec<ActionRuntime>,
    state: Vec<ActionState>,
    history: Vec<History>,
    facing: Vec<f32>,
    contexts: ContextStack,
}

/// Everything the host owns for input.
pub struct InputSystem {
    map: InputMap,
    frame: Vec<ActionRuntime>,
    tick: Vec<ActionRuntime>,
    /// A THIRD runtime set, used only by [`InputSystem::sample_tick`]: in a
    /// rollback session the tick domain is written entirely by the driver
    /// ([`InputSystem::set_tick_state`]), so sampling the local devices through
    /// `tick` would push history twice per tick and desync every motion window.
    /// This set reads the same devices on the same cadence without touching any
    /// of that. It is outside the simulation and therefore outside
    /// [`TickSnapshot`], for the same reason the frame domain is.
    sample: Vec<ActionRuntime>,
    /// Last frame's device snapshot — what `input.pads()` reports. Read-only
    /// reporting of state the frame already holds; no new ownership. floptle/0047.
    pads: Vec<crate::raw::PadState>,
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
            pads: Vec::new(),
            map,
            frame: Vec::new(),
            tick: Vec::new(),
            sample: Vec::new(),
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
        self.sample.resize_with(n, ActionRuntime::new);
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
        self.sample.iter_mut().for_each(ActionRuntime::reset);
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
    /// The devices as of the last resolved frame: index-stable, connected flag,
    /// and the pad's reported name. floptle/0047.
    pub fn pads(&self) -> &[crate::raw::PadState] {
        &self.pads
    }

    /// How many pads are currently connected.
    pub fn pad_count(&self) -> usize {
        self.pads.iter().filter(|p| p.connected).count()
    }

    pub fn resolve_frame(&mut self, raw: &RawInput, dt: f32) {
        // Snapshot the devices so a SCRIPT can ask "is there a controller here"
        // — the action API can only ever answer the resolved question, so
        // "not bound" and "not plugged in" are indistinguishable through it.
        // floptle/0047.
        self.pads = raw.pads.clone();
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

    /// Resolve one player's devices for the wire WITHOUT touching the tick
    /// domain — the local-input sample a rollback session ships to its peers
    /// (`docs/rollback-netcode-design.md` §2.2).
    ///
    /// In a rollback session the tick domain is authored entirely by the
    /// driver: every peer's input, including the local player's, arrives back
    /// through [`InputSystem::set_tick_state`] at its *applied* tick, `delay`
    /// ticks after it was sampled. Sampling through the tick runtimes would
    /// advance history a second time per tick — silently doubling every motion
    /// window and buffer answer. Call this exactly once per tick per local
    /// player, with the same drained edges [`InputSystem::resolve_tick`] would
    /// have consumed.
    pub fn sample_tick(&mut self, raw: &RawInput, slot: u8, dt: f32) -> ActionState {
        let allow = self.contexts.allow_mask(&self.map);
        let i = (slot as usize).min(self.sample.len().saturating_sub(1));
        match self.sample.get_mut(i) {
            Some(rt) => rt.resolve(&self.map, raw, slot, dt, allow),
            None => ActionState::default(),
        }
    }

    /// The whole TICK domain, captured for a rollback.
    ///
    /// This is the part of rollback that is easy to forget and impossible to
    /// skip. `buffered`, `consume` and every motion answer read a per-tick ring
    /// with **absolute** tick cursors, and `consume` records a decision that
    /// cannot be recomputed from the ring — it is a choice the script made, not
    /// a function of the input. Re-simulate a tick without restoring this and a
    /// buffered punch fires twice, or a quarter-circle that matched once fails
    /// to match on the replay. Neither shows up as an error; both show up as a
    /// desync.
    ///
    /// The FRAME domain is deliberately excluded. It advances per rendered
    /// frame, is not part of the simulation, and must not be rewound by one.
    pub fn snapshot_tick(&self) -> TickSnapshot {
        TickSnapshot {
            runtimes: self.tick.clone(),
            state: self.tick_state.clone(),
            history: self.history.clone(),
            facing: self.facing.clone(),
            contexts: self.contexts.clone(),
        }
    }

    /// Put the tick domain back as [`InputSystem::snapshot_tick`] found it.
    ///
    /// A player count that changed since the capture (someone joined or left)
    /// makes the snapshot meaningless, so it is refused rather than applied to
    /// the wrong slots.
    pub fn restore_tick(&mut self, s: &TickSnapshot) -> bool {
        if s.runtimes.len() != self.tick.len() {
            return false;
        }
        self.tick = s.runtimes.clone();
        self.tick_state = s.state.clone();
        self.history = s.history.clone();
        self.facing = s.facing.clone();
        self.contexts = s.contexts.clone();
        true
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
                    player: None,
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

    /// A couch map, the shape that made the networked bug invisible: one
    /// keyboard, so each fighter's buttons are scoped to a player slot.
    fn couch_map() -> InputMap {
        let scoped = |k: Key, p: u8| Binding { player: Some(p), ..Binding::new(Source::Key(k)) };
        InputMap {
            actions: vec![Action {
                name: "Punch".into(),
                // Player one punches with J, player two with Numpad1.
                bindings: vec![scoped(Key::KeyJ, 0), scoped(Key::Numpad1, 1)],
            }],
            axes2: vec![],
            axes1: vec![],
            motions: vec![],
            players: 2,
            motion_axis: None,
        }
    }

    /// Why a rollback peer must sample its own DEVICE slot rather than its
    /// roster slot.
    ///
    /// On a couch the two are the same number and everything works by
    /// coincidence. Over a network they diverge: the joiner is roster slot 1,
    /// but they are sitting alone at their own keyboard as its player one. Read
    /// by roster slot, their J does nothing and they are silently handed the
    /// other seat's Numpad layout.
    #[test]
    fn a_joiner_reads_its_own_player_one_bindings_not_the_couchs_player_two() {
        let mut sys = InputSystem::new(couch_map());
        let j = keys(&[Key::KeyJ]);

        // Device slot 0 — this machine's own player one. What a joiner is.
        let own = sys.sample_tick(&j, 0, 0.016);
        assert!(
            own.is_held(0),
            "the local player pressed their own punch button; it must register"
        );

        // Roster slot 1 — what the driver used to pass, and the bug.
        let mut sys = InputSystem::new(couch_map());
        let by_roster = sys.sample_tick(&j, 1, 0.016);
        assert!(
            !by_roster.is_held(0),
            "sampling by ROSTER slot reads player two's bindings, so the joiner's own \
             keyboard drives nothing — this is the assertion that documents the bug, and \
             it is why the driver samples local_device_slot() instead"
        );
    }

    /// The gamepad half of the same story, which is narrower than it looks.
    ///
    /// A lone joiner's only pad enumerates at index 0, and roster slot 1 asks
    /// for pad 1. Whether that matters depends entirely on how the pad is
    /// bound, and the difference is worth pinning down because the two spellings
    /// look interchangeable in the Input settings and are not:
    ///
    /// - `PadId::Any` — the default, and what most projects have — reads the
    ///   slot's own pad and then **falls back to any connected pad**. The lone
    ///   joiner is rescued by that fallback and always worked.
    /// - `PadId::Slot(n)` — what a couch map uses to pin pad 0 to player one and
    ///   pad 1 to player two — has no fallback by design, because the whole
    ///   point is to keep the two seats apart. That joiner drove nothing.
    #[test]
    fn a_pinned_pad_follows_the_device_slot_while_an_any_pad_falls_back() {
        use crate::source::{PadButton, PadControl, PadId};
        let map_with = |id: PadId| InputMap {
            actions: vec![Action {
                name: "Punch".into(),
                bindings: vec![Binding::new(Source::Pad {
                    id,
                    ctrl: PadControl::Button(PadButton::West),
                })],
            }],
            axes2: vec![],
            axes1: vec![],
            motions: vec![],
            players: 2,
            motion_axis: None,
        };
        // One pad, plugged into this machine, holding West. Slot 1 is empty:
        // there is no second player sitting here.
        let mut pad = crate::raw::PadState { connected: true, ..Default::default() };
        pad.buttons.insert(PadButton::West);
        let raw =
            RawInput { pads: vec![pad, crate::raw::PadState::default()], ..Default::default() };

        // ONE pad, `Any`: the fallback rescues it. This case was never broken,
        // whatever slot it was sampled at — asserted so nobody removes the
        // fallback while tidying up.
        let any = map_with(PadId::Any);
        assert!(
            InputSystem::new(any.clone()).sample_tick(&raw, 0, 0.016).is_held(0),
            "an Any binding reads the slot's own pad"
        );
        assert!(
            InputSystem::new(any.clone()).sample_tick(&raw, 1, 0.016).is_held(0),
            "with one pad connected, Any falls back to it — which is what kept the common \
             single-pad joiner working even while being sampled at the wrong slot"
        );

        // `Slot(n)` names a pad outright and never consults the resolving slot,
        // so it is unaffected in both directions.
        let pinned = map_with(PadId::Slot(0));
        for slot in [0, 1] {
            assert!(
                InputSystem::new(pinned.clone()).sample_tick(&raw, slot, 0.016).is_held(0),
                "Slot(0) means pad 0 whoever is asking"
            );
        }

        // The REAL couch shape, and the one Fofighter actually ships: each
        // seat's pad is pinned, P1 to pad 0 and P2 to pad 1. `Slot(n)` never
        // consults the resolving slot — but the BINDING SCOPE does, and that is
        // enough. Sampled at roster slot 1, only player-two's bindings serve,
        // and they name a second pad that a lone joiner does not own. Their
        // controller drives nothing, with no error anywhere.
        let pinned_couch = InputMap {
            actions: vec![Action {
                name: "Punch".into(),
                bindings: vec![
                    Binding {
                        player: Some(0),
                        ..Binding::new(Source::Pad {
                            id: PadId::Slot(0),
                            ctrl: PadControl::Button(PadButton::West),
                        })
                    },
                    Binding {
                        player: Some(1),
                        ..Binding::new(Source::Pad {
                            id: PadId::Slot(1),
                            ctrl: PadControl::Button(PadButton::West),
                        })
                    },
                ],
            }],
            axes2: vec![],
            axes1: vec![],
            motions: vec![],
            players: 2,
            motion_axis: None,
        };
        assert!(
            InputSystem::new(pinned_couch.clone()).sample_tick(&raw, 0, 0.016).is_held(0),
            "the joiner's own pad, read at their device slot"
        );
        assert!(
            !InputSystem::new(pinned_couch).sample_tick(&raw, 1, 0.016).is_held(0),
            "sampled by roster slot, a lone joiner on a pad drove NOTHING — the scope picked \
             player two's bindings, which name a pad that isn't plugged in"
        );

        // The case that IS wrong: a joiner with TWO pads connected. `Any`
        // prefers the resolving slot's own pad, so roster slot 1 reads their
        // second controller and their first — the one in their hands — does
        // nothing.
        let mut first = crate::raw::PadState { connected: true, ..Default::default() };
        first.buttons.insert(PadButton::West);
        let second = crate::raw::PadState { connected: true, ..Default::default() };
        let two = RawInput { pads: vec![first, second], ..Default::default() };
        assert!(
            InputSystem::new(any.clone()).sample_tick(&two, 0, 0.016).is_held(0),
            "device slot 0 is the pad they are actually holding"
        );
        assert!(
            !InputSystem::new(any).sample_tick(&two, 1, 0.016).is_held(0),
            "sampled by roster slot, a joiner with two pads connected drives their fighter \
             from the wrong one"
        );
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

    /// Two local players on ONE keyboard, each with their own quarter-circle. This is
    /// the end of floptle/0028: the motion recogniser reads the map-level `Move` axis,
    /// and before per-player bindings that axis was player 1's for everyone — so P2's
    /// `dir()` and every motion answered with P1's stick, silently.
    #[test]
    fn both_local_players_get_their_own_motions_on_one_keyboard() {
        let mut map = fighter_map(2);
        // One "Punch", two keyboards' worth of bindings.
        map.actions[0].bindings = vec![
            Binding::new(Source::Key(Key::KeyJ)).for_player(0),
            Binding::new(Source::Key(Key::Numpad1)).for_player(1),
        ];
        // One "Move" axis: WASD for P1, arrows for P2.
        map.axes2[0].bindings = vec![
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
        ];
        let mut sys = InputSystem::new(map);
        // P1 does a qcf on WASD while P2 stands still, then the reverse — interleaved on
        // the same ticks, because that is how a couch match actually plays.
        for k in [
            vec![Key::KeyS],
            vec![Key::KeyS, Key::KeyD],
            vec![Key::KeyD],
        ] {
            sys.resolve_tick(&keys(&k), 0.016);
        }
        assert!(sys.motion(0, "qcf", None), "P1's quarter circle");
        assert!(!sys.motion(1, "qcf", None), "P2 did nothing and must read as nothing");

        let mut sys = InputSystem::new(sys.map().clone());
        for k in [
            vec![Key::ArrowDown],
            vec![Key::ArrowDown, Key::ArrowRight],
            vec![Key::ArrowRight],
        ] {
            sys.resolve_tick(&keys(&k), 0.016);
        }
        assert!(sys.motion(1, "qcf", None), "P2's quarter circle, on their own keys");
        assert!(!sys.motion(0, "qcf", None), "and it must not read as P1's");

        // The shared action name resolves to each player's own key.
        sys.resolve_tick(&keys(&[Key::ArrowRight, Key::Numpad1]), 0.016);
        assert!(sys.buffered(1, "Punch", 4), "P2's punch key");
        assert!(!sys.buffered(0, "Punch", 4), "which is not P1's");
    }

    /// The buffer/motion state must survive a rollback intact. `consume` is the
    /// case that proves it: it records a DECISION the script made, which no
    /// amount of replaying the raw inputs can reconstruct — so a replay that
    /// didn't restore it would let one buffered press fire the attack twice.
    #[test]
    fn a_tick_snapshot_restores_buffers_motions_and_consumption() {
        let mut sys = InputSystem::new(fighter_map(1));
        // qcf, then the punch — the state a fighter is actually in mid-special.
        for k in [vec![Key::KeyS], vec![Key::KeyS, Key::KeyD], vec![Key::KeyD]] {
            sys.resolve_tick(&keys(&k), 0.016);
        }
        sys.resolve_tick(&keys(&[Key::KeyD, Key::KeyJ]), 0.016);
        sys.set_facing(0, -1.0);
        let saved = sys.snapshot_tick();
        assert!(sys.motion(0, "qcf", None) && sys.buffered(0, "Punch", 4));

        // The tick runs: the script spends the buffered press and the motion
        // window ages out.
        sys.consume(0, "Punch", 4);
        assert!(!sys.buffered(0, "Punch", 4), "spent");
        for _ in 0..30 {
            sys.resolve_tick(&keys(&[]), 0.016);
        }
        sys.set_facing(0, 1.0);
        assert!(!sys.motion(0, "qcf", None), "window long gone");

        // Roll back to the saved tick: everything comes back, including the fact
        // that the press had NOT yet been consumed.
        assert!(sys.restore_tick(&saved));
        assert!(sys.buffered(0, "Punch", 4), "the press is unspent again");
        assert!(sys.motion(0, "qcf", None), "and the motion still matches");
        assert_eq!(sys.facing(0), -1.0, "facing rolls back with everything else");

        // And re-simulating from there reproduces the same answers.
        assert!(sys.consume(0, "Punch", 4));
        assert!(!sys.consume(0, "Punch", 4), "still fires exactly once");
    }

    /// Sampling the local devices for the wire must leave the tick domain
    /// untouched. A rollback session writes that domain itself, one
    /// `set_tick_state` per peer per tick; if sampling advanced history too,
    /// every motion window and buffer would age at twice the rate on the local
    /// player and at the right rate on the remote one — a desync that looks
    /// like "my quarter-circles only come out online".
    #[test]
    fn sampling_for_the_wire_never_advances_the_tick_domain() {
        let mut sys = InputSystem::new(fighter_map(1));
        let down = keys(&[Key::KeyJ]);

        let sampled = sys.sample_tick(&down, 0, 0.016);
        assert!(sampled.is_just_pressed(0), "the sample sees the press");
        assert!(!sys.action(Domain::Tick, 0, "Punch"), "…and the tick domain saw nothing");
        assert_eq!(sys.history(0).tick(), 0, "no history was pushed");

        // Held on the next sample: the sampler carries its own edge state, so a
        // held button is not re-reported as a fresh press every tick.
        let again = sys.sample_tick(&down, 0, 0.016);
        assert!(again.is_held(0) && !again.is_just_pressed(0));

        // The driver then writes the applied tick, and THAT is what history and
        // the script-facing queries see — exactly once.
        sys.set_tick_state(0, sampled);
        assert!(sys.action(Domain::Tick, 0, "Punch"));
        assert_eq!(sys.history(0).tick(), 1);
    }

    /// A snapshot taken before the player count changed cannot be applied to the
    /// wrong slots — it is refused, not silently misapplied.
    #[test]
    fn a_tick_snapshot_from_a_different_player_count_is_refused() {
        let mut sys = InputSystem::new(fighter_map(1));
        let saved = sys.snapshot_tick();
        sys.set_map(fighter_map(2));
        assert!(!sys.restore_tick(&saved));
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
