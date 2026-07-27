//! Editor-side plumbing for the action layer (`floptle-input`).
//!
//! Three jobs:
//!
//! 1. Translate winit `KeyCode` into `floptle_input::Key` — the only place the
//!    two vocabularies meet, so the input crate stays winit-free and headlessly
//!    testable.
//! 2. Keep [`Editor::raw_input`] current: key and mouse levels from window
//!    events, pad levels from the gilrs pump.
//! 3. Resolve, twice per the two domains — once per rendered frame for
//!    `update`, once per gameplay tick for `fixedUpdate`. The tick resolve
//!    consumes banked edges so a button tapped between ticks is never lost.

use floptle_input::{InputMap, Key, MouseButton, Source};
use winit::keyboard::KeyCode;

use crate::Editor;

/// winit's physical key → ours. `None` for keys the action map doesn't model
/// (media keys, IME, F13+); they simply aren't bindable.
pub(crate) fn action_key(code: KeyCode) -> Option<Key> {
    use KeyCode as C;
    Some(match code {
        C::KeyA => Key::KeyA, C::KeyB => Key::KeyB, C::KeyC => Key::KeyC, C::KeyD => Key::KeyD,
        C::KeyE => Key::KeyE, C::KeyF => Key::KeyF, C::KeyG => Key::KeyG, C::KeyH => Key::KeyH,
        C::KeyI => Key::KeyI, C::KeyJ => Key::KeyJ, C::KeyK => Key::KeyK, C::KeyL => Key::KeyL,
        C::KeyM => Key::KeyM, C::KeyN => Key::KeyN, C::KeyO => Key::KeyO, C::KeyP => Key::KeyP,
        C::KeyQ => Key::KeyQ, C::KeyR => Key::KeyR, C::KeyS => Key::KeyS, C::KeyT => Key::KeyT,
        C::KeyU => Key::KeyU, C::KeyV => Key::KeyV, C::KeyW => Key::KeyW, C::KeyX => Key::KeyX,
        C::KeyY => Key::KeyY, C::KeyZ => Key::KeyZ,

        C::Digit0 => Key::Digit0, C::Digit1 => Key::Digit1, C::Digit2 => Key::Digit2,
        C::Digit3 => Key::Digit3, C::Digit4 => Key::Digit4, C::Digit5 => Key::Digit5,
        C::Digit6 => Key::Digit6, C::Digit7 => Key::Digit7, C::Digit8 => Key::Digit8,
        C::Digit9 => Key::Digit9,

        C::F1 => Key::F1, C::F2 => Key::F2, C::F3 => Key::F3, C::F4 => Key::F4,
        C::F5 => Key::F5, C::F6 => Key::F6, C::F7 => Key::F7, C::F8 => Key::F8,
        C::F9 => Key::F9, C::F10 => Key::F10, C::F11 => Key::F11, C::F12 => Key::F12,

        C::Space => Key::Space,
        // Both Enters bind as one key, matching the legacy raw-key table.
        C::Enter | C::NumpadEnter => Key::Enter,
        C::Escape => Key::Escape,
        C::Tab => Key::Tab,
        C::Backspace => Key::Backspace,
        C::Delete => Key::Delete,
        C::Insert => Key::Insert,
        C::Home => Key::Home,
        C::End => Key::End,
        C::PageUp => Key::PageUp,
        C::PageDown => Key::PageDown,

        C::ShiftLeft => Key::ShiftLeft, C::ShiftRight => Key::ShiftRight,
        C::ControlLeft => Key::ControlLeft, C::ControlRight => Key::ControlRight,
        C::AltLeft => Key::AltLeft, C::AltRight => Key::AltRight,
        C::SuperLeft => Key::SuperLeft, C::SuperRight => Key::SuperRight,
        C::CapsLock => Key::CapsLock,

        C::ArrowLeft => Key::ArrowLeft, C::ArrowRight => Key::ArrowRight,
        C::ArrowUp => Key::ArrowUp, C::ArrowDown => Key::ArrowDown,

        C::Comma => Key::Comma, C::Period => Key::Period, C::Slash => Key::Slash,
        C::Semicolon => Key::Semicolon, C::Quote => Key::Quote, C::Backquote => Key::Backquote,
        C::BracketLeft => Key::BracketLeft, C::BracketRight => Key::BracketRight,
        C::Backslash => Key::Backslash, C::Minus => Key::Minus, C::Equal => Key::Equal,

        C::Numpad0 => Key::Numpad0, C::Numpad1 => Key::Numpad1, C::Numpad2 => Key::Numpad2,
        C::Numpad3 => Key::Numpad3, C::Numpad4 => Key::Numpad4, C::Numpad5 => Key::Numpad5,
        C::Numpad6 => Key::Numpad6, C::Numpad7 => Key::Numpad7, C::Numpad8 => Key::Numpad8,
        C::Numpad9 => Key::Numpad9,
        C::NumpadAdd => Key::NumpadAdd, C::NumpadSubtract => Key::NumpadSubtract,
        C::NumpadMultiply => Key::NumpadMultiply, C::NumpadDivide => Key::NumpadDivide,
        C::NumpadDecimal => Key::NumpadDecimal,

        _ => return None,
    })
}

impl Editor {
    /// Record a key event for the action layer, alongside the legacy string
    /// sets. Called from the same winit handler so the two views of the
    /// keyboard can never disagree.
    pub(crate) fn note_action_key(&mut self, code: KeyCode, pressed: bool) {
        let Some(key) = action_key(code) else { return };
        let src = Source::Key(key);
        if pressed {
            // Only a genuine transition banks an edge — winit repeats a held
            // key, and a repeat is not a new press.
            if self.raw_input.keys.insert(key) {
                self.tick_input_edges.0.insert(src);
            }
        } else if self.raw_input.keys.remove(&key) {
            self.tick_input_edges.1.insert(src);
        }
    }

    /// Record a mouse-button event for the action layer.
    pub(crate) fn note_action_button(&mut self, index: usize, pressed: bool) {
        let Some(btn) = MouseButton::ALL.get(index).copied() else { return };
        let src = Source::Mouse(btn);
        let slot = &mut self.raw_input.mouse_buttons[btn.index()];
        if *slot != pressed {
            *slot = pressed;
            let bank = if pressed { &mut self.tick_input_edges.0 } else { &mut self.tick_input_edges.1 };
            bank.insert(src);
        }
    }

    /// Pump the gamepads and refresh per-frame device levels. Call once per
    /// frame, before anything resolves.
    pub(crate) fn pump_input_devices(&mut self) {
        // A fresh frame's banked pad edges; keyboard/mouse edges live in
        // `tick_input_edges` and are drained per tick instead.
        self.raw_input.pressed.clear();
        self.raw_input.released.clear();
        self.pads.pump(&mut self.raw_input);
        // Pad edges must reach the tick domain too.
        for s in self.raw_input.pressed.iter() {
            self.tick_input_edges.0.insert(*s);
        }
        for s in self.raw_input.released.iter() {
            self.tick_input_edges.1.insert(*s);
        }
        self.raw_input.mouse_pos = self.cursor.map(|c| (c.x, c.y)).unwrap_or((0.0, 0.0));
        self.raw_input.mouse_delta = self.input_mouse_delta;
        self.raw_input.scroll = (0.0, self.input_scroll);

        // Feed an armed rebind. Doing it here means the editor's binding chips
        // and a shipped game's settings menu capture through the identical path.
        let host = self.script_host.input_system().clone();
        let armed = host.borrow().pending_rebind().is_some();
        if armed {
            let raw = self.raw_input.clone();
            let _ = host.borrow_mut().poll_rebind(&raw);
        }

        // The Input settings' live tester: resolve the real devices regardless
        // of play state, so mashing a pad lights the strip up while you edit.
        if self.show_project_settings {
            let map = host.borrow().map().clone();
            let raw = self.raw_input.clone();
            self.input_test_state = self.input_test_rt.resolve(
                &map,
                &raw,
                0,
                1.0 / 60.0,
                floptle_input::AllowMask::ALL,
            );
        }
    }

    /// Resolve the FRAME domain (what `update` reads).
    ///
    /// Unfocused game view resolves neutral for the same reason raw keys do:
    /// you're editing, not playing, so the character must stop moving even
    /// though physics keeps simulating.
    pub(crate) fn resolve_frame_actions(&mut self, dt: f32, game_focused: bool) {
        let sys = self.script_host.input_system().clone();
        if game_focused {
            let raw = self.raw_input.clone();
            sys.borrow_mut().resolve_frame(&raw, dt);
        } else {
            sys.borrow_mut().resolve_frame(&floptle_input::RawInput::default(), dt);
        }
    }

    /// Resolve the TICK domain (what `fixedUpdate` reads) and advance input
    /// history. Consumes the banked edges, so call exactly once per tick.
    pub(crate) fn resolve_tick_actions(&mut self, dt: f32, game_focused: bool) {
        let sys = self.script_host.input_system().clone();
        let mut raw = if game_focused { self.raw_input.clone() } else { floptle_input::RawInput::default() };
        // Even when the view isn't focused the banked edges must be DRAINED, or
        // a press made while editing would fire the moment play regains focus.
        raw.pressed = std::mem::take(&mut self.tick_input_edges.0);
        raw.released = std::mem::take(&mut self.tick_input_edges.1);
        if !game_focused {
            raw.pressed.clear();
            raw.released.clear();
        }
        sys.borrow_mut().resolve_tick(&raw, dt);
    }

    /// Forget all action state — on Play start/stop, so a key held in the
    /// editor isn't seen as a press inside the game (and vice versa).
    pub(crate) fn reset_action_state(&mut self) {
        self.script_host.input_system().borrow_mut().reset();
        self.tick_input_edges.0.clear();
        self.tick_input_edges.1.clear();
    }

    /// Load `input.ron` into the script host.
    ///
    /// A **missing** file falls back to [`InputMap::starter`] in memory rather
    /// than to an empty map, and deliberately does NOT write anything to disk.
    /// The shipped default scripts (`freelook`, `first_person`, `third_person`,
    /// …) are written against the starter names, so an empty map would leave a
    /// fresh project's camera unable to move — while a file appearing on disk
    /// that nobody asked for is its own kind of rude. It gets written the first
    /// time you actually edit a binding.
    ///
    /// A malformed file is reported and the previous map kept, because silently
    /// unbinding every control looks like broken hardware.
    pub(crate) fn load_input_map(&mut self) {
        self.input_map_mtime = input_map_mtime(&self.project_root);
        match floptle_input::load_map(&self.project_root) {
            Ok(Some(map)) => self.script_host.set_input_map(map),
            Ok(None) => self.script_host.set_input_map(InputMap::starter()),
            Err(e) => self.console.push(
                floptle_script::LogLevel::Error,
                format!("input.ron: {e}"),
                None,
            ),
        }
    }

    /// Reload the map if the file changed on disk (someone edited it in an IDE,
    /// or a merge landed). Cheap: one stat per frame, same as the shader watcher.
    pub(crate) fn poll_input_map_reload(&mut self) {
        let now = input_map_mtime(&self.project_root);
        if now != self.input_map_mtime && now.is_some() {
            self.load_input_map();
            self.console.push(floptle_script::LogLevel::Debug, "input.ron reloaded".into(), None);
        }
    }

    /// Write the in-memory map back to `input.ron`.
    pub(crate) fn save_input_map(&mut self) {
        let map = self.script_host.input_system().borrow().map().clone();
        match floptle_input::save_map(&map, &self.project_root) {
            Ok(()) => self.input_map_mtime = input_map_mtime(&self.project_root),
            Err(e) => self.console.push(
                floptle_script::LogLevel::Error,
                format!("could not save input.ron: {e}"),
                None,
            ),
        }
    }

    /// This project's action-map fingerprint, for the multiplayer handshake.
    /// Peers whose maps differ in SHAPE are refused, because input commands
    /// index actions by their position in the map.
    pub(crate) fn input_map_hash(&self) -> u64 {
        self.script_host.input_system().borrow().map().hash()
    }

    /// This tick's resolved actions in wire form — what a predicted node's
    /// owner ships to the server.
    pub(crate) fn current_net_input(&self) -> floptle_net::NetInput {
        let sys = self.script_host.input_system().borrow();
        floptle_script::input_to_net(
            sys.state(floptle_input::Domain::Tick, 0),
            self.last_tick_input.aim,
        )
    }

    /// Apply the Input settings' collected edits.
    pub(crate) fn apply_input_edits(&mut self, edits: crate::input_ui::InputEdits) {
        use crate::input_ui::InputCmd;
        if edits.rescan {
            let dir = self.project_root.join("scripts");
            self.input_scan.rescan(&dir);
        }
        if edits.commands.is_empty() {
            return;
        }
        let sys = self.script_host.input_system().clone();
        for cmd in edits.commands {
            let mut sys = sys.borrow_mut();
            match cmd {
                // Fill gaps, never replace: someone who already bound half a
                // game must not lose it to a button labelled "starter".
                InputCmd::SeedStarter => {
                    let mut map = sys.map().clone();
                    map.merge_missing(&InputMap::starter());
                    sys.set_map(map);
                }
                InputCmd::AddAction(name) => {
                    let map = sys.map_mut();
                    if map.action_index(&name).is_none() && map.actions.len() < floptle_input::MAX_ACTIONS {
                        map.actions.push(floptle_input::Action::new(name));
                    }
                }
                InputCmd::AddEntry { name, kind } => add_entry(sys.map_mut(), kind, name),
                InputCmd::AddBinding { action, source } => {
                    // A picked pad source binds to THIS player's pad when the
                    // project has several, matching what press-to-bind does —
                    // otherwise P2's binding would read P1's controller.
                    let multiplayer = sys.players() > 1;
                    let source = match (multiplayer, source) {
                        (true, floptle_input::Source::Pad { ctrl, .. }) => {
                            floptle_input::Source::Pad { id: floptle_input::PadId::Slot(0), ctrl }
                        }
                        (_, s) => s,
                    };
                    let binding = floptle_input::Binding::new(source);
                    if let Some(a) = sys.map_mut().actions.iter_mut().find(|a| a.name == action)
                        && !a.bindings.contains(&binding)
                    {
                        a.bindings.push(binding);
                    }
                }
                InputCmd::RemoveAction(name) => {
                    sys.map_mut().actions.retain(|a| a.name != name);
                }
                InputCmd::RemoveBinding { action, index } => {
                    if let Some(a) = sys.map_mut().actions.iter_mut().find(|a| a.name == action)
                        && index < a.bindings.len()
                    {
                        a.bindings.remove(index);
                    }
                }
                InputCmd::StartRebind { action, filter } => sys.start_rebind(action, 0, filter),
                InputCmd::CancelRebind => sys.cancel_rebind(),
                InputCmd::SetSocd { axis, socd } => {
                    if let Some(a) = sys.map_mut().axes2.get_mut(axis) {
                        a.socd = socd;
                    }
                }
                InputCmd::SetPlayers(n) => {
                    sys.map_mut().players = n.max(1);
                    // Re-size the per-player runtimes for the new count.
                    let map = sys.map().clone();
                    sys.set_map(map);
                }
            }
        }
        if edits.save {
            self.save_input_map();
        }
    }

    /// A capture completed while the Input settings were open: commit it and
    /// persist. Auto-committing is the behaviour people expect from
    /// press-to-bind — click ＋, press the key, done.
    pub(crate) fn settle_pending_rebind(&mut self, cancel: bool) {
        let sys = self.script_host.input_system().clone();
        let captured = sys.borrow().pending_rebind().and_then(|p| p.captured.clone());
        if cancel {
            sys.borrow_mut().cancel_rebind();
            return;
        }
        let Some(c) = captured else { return };
        let changed = sys.borrow_mut().commit_rebind(c);
        if changed {
            self.save_input_map();
        }
    }
}

/// Feed a wire input to a script host — the server running a client's command,
/// or a client replaying its own after a correction. Free-standing because the
/// hidden-server harness owns a second [`floptle_script::ScriptHost`], and
/// because both replay paths call it while `self` is already borrowed.
///
/// The raw key/mouse snapshot goes **neutral** here, carrying only `aim`. That
/// is the deliberate consequence of an actions-only wire: raw polls have
/// nothing to replay from, so they read neutral identically on the client, the
/// server, and the replay. A predicted controller that still polls raw keys
/// therefore visibly does nothing instead of silently desyncing — and the
/// Input settings list every such call site so the migration has a worklist.
pub(crate) fn apply_net_input_to(host: &floptle_script::ScriptHost, inp: &floptle_net::NetInput) {
    host.set_input(floptle_script::InputSnapshot {
        aim: floptle_script::net_aim(inp),
        ..Default::default()
    });
    let sys = host.input_system().clone();
    let count = sys.borrow().map().actions.len();
    let state = floptle_script::net_to_input(inp, count);
    // Slot 0: the tick being run belongs to whichever owner the caller is
    // driving right now, and the caller restores its own input afterwards.
    sys.borrow_mut().set_tick_state(0, state);
}

/// Create the map entry a scanned call site implies, unbound — the developer
/// then presses the input they want for it.
fn add_entry(map: &mut InputMap, kind: crate::input_scan::UsageKind, name: String) {
    use crate::input_scan::UsageKind as K;
    match kind {
        K::Action => {
            if map.action_index(&name).is_none() && map.actions.len() < floptle_input::MAX_ACTIONS {
                map.actions.push(floptle_input::Action::new(name));
            }
        }
        K::Axis1 => {
            if map.axis1_index(&name).is_none() {
                map.axes1.push(floptle_input::Axis1 {
                    name,
                    socd: floptle_input::Socd::Neutral,
                    bindings: Vec::new(),
                });
            }
        }
        K::Axis2 => {
            if map.axis2_index(&name).is_none() {
                map.axes2.push(floptle_input::Axis2 {
                    name,
                    socd: floptle_input::Socd::Neutral,
                    bindings: Vec::new(),
                });
            }
        }
        K::Motion => {
            if map.motion(&name).is_none() {
                // A placeholder quarter-circle: the developer edits the
                // directions in input.ron. Seeding an EMPTY dirs list would
                // create a motion that can never match, which reads as a bug.
                map.motions.push(floptle_input::Motion {
                    name,
                    dirs: vec![2, 3, 6],
                    window: 12,
                    charge: 0,
                });
            }
        }
        // Raw key polls are reported, never auto-converted: only the author
        // knows which action a given key was standing in for.
        K::RawKey => {}
    }
}

fn input_map_mtime(root: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(root.join(floptle_input::MAP_FILE)).ok()?.modified().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_keys_agree_with_the_legacy_raw_key_names() {
        // Both tables are filled from one winit event; if they disagreed, a
        // script mixing `input.key("w")` and an action bound to W would see
        // contradictory answers on the same frame.
        for (code, name) in [
            (KeyCode::KeyW, "w"),
            (KeyCode::Space, "space"),
            (KeyCode::Escape, "escape"),
            (KeyCode::ShiftLeft, "shift"),
            (KeyCode::ControlRight, "ctrl"),
            (KeyCode::AltLeft, "alt"),
            (KeyCode::ArrowLeft, "left"),
            (KeyCode::Comma, ","),
            (KeyCode::Period, "."),
            (KeyCode::Enter, "enter"),
            (KeyCode::Digit7, "7"),
        ] {
            let key = action_key(code).unwrap_or_else(|| panic!("{code:?} unmapped"));
            assert_eq!(key.script_name(), name, "{code:?}");
            assert_eq!(crate::key_name(code), Some(name), "{code:?} legacy table");
        }
    }

    #[test]
    fn every_legacy_raw_key_is_also_bindable() {
        // The action map must be at least as expressive as the old API — a key
        // a script could already poll must be bindable to an action.
        for code in [
            KeyCode::KeyA, KeyCode::KeyZ, KeyCode::Digit0, KeyCode::Digit9,
            KeyCode::Space, KeyCode::Enter, KeyCode::NumpadEnter, KeyCode::Escape,
            KeyCode::Tab, KeyCode::Backspace, KeyCode::Delete,
            KeyCode::ShiftLeft, KeyCode::ShiftRight, KeyCode::ControlLeft,
            KeyCode::ControlRight, KeyCode::AltLeft, KeyCode::AltRight,
            KeyCode::Comma, KeyCode::Period,
            KeyCode::ArrowLeft, KeyCode::ArrowRight, KeyCode::ArrowUp, KeyCode::ArrowDown,
        ] {
            assert!(crate::key_name(code).is_none() || action_key(code).is_some(), "{code:?}");
        }
    }

    #[test]
    fn distinct_keys_do_not_collapse_onto_one_binding() {
        // Only Enter/NumpadEnter and the L/R modifier pairs are allowed to
        // share; anything else colliding would make two keys indistinguishable.
        let mut seen = std::collections::HashMap::new();
        for code in [
            KeyCode::KeyA, KeyCode::KeyB, KeyCode::Digit1, KeyCode::Numpad1,
            KeyCode::Minus, KeyCode::NumpadSubtract, KeyCode::Slash, KeyCode::NumpadDivide,
        ] {
            let k = action_key(code).unwrap();
            assert!(seen.insert(k, code).is_none(), "{code:?} collides with {:?}", seen.get(&k));
        }
    }

    #[test]
    fn unmodelled_keys_map_to_none_rather_than_a_wrong_key() {
        assert_eq!(action_key(KeyCode::F20), None);
        assert_eq!(action_key(KeyCode::MediaPlayPause), None);
    }
}
