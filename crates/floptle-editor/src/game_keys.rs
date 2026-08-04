//! Which keys reach a running game, and which the editor keeps (`floptle/0084`).
//!
//! A game running in the Game view is supposed to behave like a build. The raw
//! key set scripts read is filled from the window events directly, so almost
//! every key already arrives — but egui also sees the same events, and egui uses
//! **Tab** for widget focus traversal before anything else gets a look. A game
//! bound to Tab therefore received nothing, the editor's tab bar took the
//! keypress, and `input.pressed("tab")` returned `false` — which is
//! indistinguishable from "the player did not press it".
//!
//! That mattered because Tab is *the* convention for opening an inventory
//! (Minecraft, Terraria, Valheim, Don't Starve). It is the first key a player
//! tries and the first a developer reaches for, and it worked in an exported
//! build — so the binding appeared broken for the whole time you were making the
//! game and correct only once you stopped testing it. A game shipped a bag on
//! Tab, passed its headless tests (the harness stubs `input`), and found out from
//! a player.
//!
//! Two halves here:
//!
//! * [`claim_keys_for_game`] takes the keyboard away from egui while the game is
//!   being played and focused, so Tab (and anything focus gives egui a route to)
//!   reaches the game.
//! * [`RESERVED`] is the short list the editor genuinely keeps — Play, Pause and
//!   Step — with the reason written down. A script polling one of those gets a
//!   Console line the first time rather than silence, which is the same remedy as
//!   `0072` and `0083`: the bug was never that a thing was unavailable, it was
//!   that being unavailable looked exactly like working.

/// Keys the editor answers even while the game is focused, with the reason.
///
/// Deliberately three. These are the transport controls — a game that could take
/// F1 could stop you stopping it, which is the one key you need when a script
/// has gone wrong. Everything else belongs to the game.
///
/// Script names, as [`floptle_input::Key::script_name`] spells them, because
/// that is what a script passes to `input.pressed`.
pub(crate) const RESERVED: &[(&str, &str)] = &[
    ("f1", "Play / Stop"),
    ("f2", "Pause"),
    ("f3", "Step one tick (Shift+F3 steps back)"),
];

/// Why the editor is holding this key back, or `None` if the game gets it.
pub(crate) fn reserved_reason(name: &str) -> Option<&'static str> {
    RESERVED.iter().find(|(k, _)| *k == name).map(|(_, why)| *why)
}

/// Take the keyboard away from egui for one frame, so the game gets the keys
/// egui would otherwise intercept.
///
/// Called only while the game is playing, the Game view is focused, and no text
/// field wants input — so the editor is not being typed into and a click is
/// still how you go back to it.
///
/// Two things happen, and both are needed:
///
/// 1. **Tab events are dropped** before egui runs. This is the reported bug:
///    egui hands Tab to focus traversal, which lands on the dock's tab-bar
///    buttons, and the game sees nothing.
/// 2. **egui's focused widget is surrendered.** Tab is how egui acquires focus
///    in the first place, but a click can do it too, and a *focused* widget
///    turns Enter, Space and the arrows into egui's business as well. Dropping
///    Tab without this would fix one key and leave the same shape behind for
///    four more.
///
/// Modifiers are not consulted: Ctrl+Tab is not an editor binding, and a game
/// that reads `input.key("tab")` while the player happens to hold Shift should
/// still see it.
pub(crate) fn claim_keys_for_game(raw: &mut egui::RawInput, ctx: &egui::Context) {
    raw.events.retain(|e| !matches!(e, egui::Event::Key { key: egui::Key::Tab, .. }));
    if let Some(id) = ctx.memory(|m| m.focused()) {
        ctx.memory_mut(|m| m.surrender_focus(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reserved list is exactly the transport controls, and the reason is
    /// filled in — the whole point of the list is that a warning can quote it.
    #[test]
    fn only_the_transport_controls_are_reserved() {
        let names: Vec<&str> = RESERVED.iter().map(|(k, _)| *k).collect();
        assert_eq!(names, ["f1", "f2", "f3"]);
        for (k, why) in RESERVED {
            assert!(!why.is_empty(), "{k} is reserved with no reason given");
        }
        // Tab is the key this task is about. It must NOT be on the list: the fix
        // is that the game gets it, not that the editor documents keeping it.
        assert!(reserved_reason("tab").is_none(), "the game is supposed to get Tab now");
        assert!(reserved_reason("i").is_none());
        assert_eq!(reserved_reason("f1"), Some("Play / Stop"));
    }

    /// Every reserved name is a key the input crate can actually name, so a
    /// script polling it reaches the warning rather than typoing past it.
    #[test]
    fn every_reserved_name_is_a_real_key() {
        for (k, _) in RESERVED {
            assert!(
                floptle_input::Key::from_script_name(k).is_some(),
                "{k} is not a key floptle-input knows, so nothing can ever match it"
            );
        }
    }

    /// Tab is dropped from egui's events and nothing else is.
    #[test]
    fn claiming_the_keyboard_drops_only_tab() {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput {
            events: vec![
                key_event(egui::Key::Tab),
                key_event(egui::Key::I),
                key_event(egui::Key::Enter),
                egui::Event::Text("i".into()),
            ],
            ..Default::default()
        };
        claim_keys_for_game(&mut raw, &ctx);
        assert_eq!(raw.events.len(), 3, "one event removed, and it is the Tab");
        assert!(
            !raw.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::Tab, .. })),
            "Tab survived, so the dock's tab bar still eats it"
        );
        assert!(raw.events.iter().any(|e| matches!(e, egui::Event::Key { key: egui::Key::I, .. })));
        assert!(raw.events.iter().any(|e| matches!(e, egui::Event::Text(_))));
    }

    /// Shift+Tab goes too — it is the same traversal, backwards.
    #[test]
    fn a_modified_tab_is_claimed_as_well() {
        let ctx = egui::Context::default();
        let mut raw = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Tab,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::SHIFT,
            }],
            ..Default::default()
        };
        claim_keys_for_game(&mut raw, &ctx);
        assert!(raw.events.is_empty());
    }

    fn key_event(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }
}
