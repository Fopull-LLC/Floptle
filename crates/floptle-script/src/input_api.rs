//! The Lua **action** API — `input.action("Jump")` and friends.
//!
//! Installed onto the same `input` table that carries the raw key/mouse
//! functions, so a project can migrate one call at a time. Raw polling stays
//! supported for single-player; actions are what work on a pad, what a player
//! can rebind, and what replicate.
//!
//! Every function exists twice: once on `input` (bound to player 1) and once on
//! the table `input.player(n)` returns. The closures differ only in their slot,
//! so there is exactly one implementation of each rule.

use std::cell::Cell;
use std::rc::Rc;

use mlua::{Lua, Table, Value};

use floptle_input::{BindFilter, ConsumeMode, Context, Domain, InputSystem};

/// Shared handle to the host's input system. The driver resolves into it each
/// frame and tick; scripts read (and, for `consume`, write) through it.
pub type SharedInput = Rc<std::cell::RefCell<InputSystem>>;

/// Which domain the currently-running pass reads. The driver flips this before
/// each script pass: `fixedUpdate` reads the tick domain, `update` the frame
/// domain. Without it a script could not tell which set of edges it is seeing.
pub type SharedDomain = Rc<Cell<Domain>>;

/// A default buffer window, in ticks, when a script doesn't name one.
///
/// Three ticks is the usual "one frame of leniency either side" a fighting game
/// ships with; anything larger starts feeling like the game is pressing buttons
/// for the player.
const DEFAULT_BUFFER: u32 = 3;

/// Install the action API onto `t`, bound to `slot`.
fn install_for_slot(
    lua: &Lua,
    t: &Table,
    sys: &SharedInput,
    domain: &SharedDomain,
    slot: u8,
) -> mlua::Result<()> {
    // --- digital actions -------------------------------------------------
    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "action",
        lua.create_function(move |_, name: String| {
            Ok(s.borrow().action(d.get(), slot, &name))
        })?,
    )?;

    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "justPressed",
        lua.create_function(move |_, name: String| {
            Ok(s.borrow().just_pressed(d.get(), slot, &name))
        })?,
    )?;

    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "justReleased",
        lua.create_function(move |_, name: String| {
            Ok(s.borrow().just_released(d.get(), slot, &name))
        })?,
    )?;

    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "heldSecs",
        lua.create_function(move |_, name: String| {
            Ok(s.borrow().held_secs(d.get(), slot, &name))
        })?,
    )?;

    // --- analog axes ------------------------------------------------------
    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "axis1",
        lua.create_function(move |_, name: String| Ok(s.borrow().axis1(d.get(), slot, &name)))?,
    )?;

    let (s, d) = (sys.clone(), domain.clone());
    t.set(
        "axis2",
        lua.create_function(move |_, name: String| {
            let (x, y) = s.borrow().axis2(d.get(), slot, &name);
            Ok((x, y))
        })?,
    )?;

    // --- the fighter layer (tick domain) ----------------------------------
    let s = sys.clone();
    t.set("dir", lua.create_function(move |_, ()| Ok(s.borrow().dir(slot)))?)?;

    let s = sys.clone();
    t.set(
        "dirHeldTicks",
        lua.create_function(move |_, dir: u8| Ok(s.borrow().history(slot).dir_held_ticks(dir)))?,
    )?;

    let s = sys.clone();
    t.set(
        "buffered",
        lua.create_function(move |_, (name, within): (String, Option<u32>)| {
            Ok(s.borrow().buffered(slot, &name, within.unwrap_or(DEFAULT_BUFFER)))
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "consume",
        lua.create_function(move |_, (name, within): (String, Option<u32>)| {
            Ok(s.borrow_mut().consume(slot, &name, within.unwrap_or(DEFAULT_BUFFER)))
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "motion",
        lua.create_function(move |_, (name, window): (String, Option<u16>)| {
            Ok(s.borrow().motion(slot, &name, window))
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "setFacing",
        lua.create_function(move |_, facing: f32| {
            s.borrow_mut().set_facing(slot, facing);
            Ok(())
        })?,
    )?;

    let s = sys.clone();
    t.set("facing", lua.create_function(move |_, ()| Ok(s.borrow().facing(slot)))?)?;

    // --- introspection, for in-game settings menus -------------------------
    let s = sys.clone();
    t.set(
        "actions",
        lua.create_function(move |lua, ()| {
            let names: Vec<String> =
                s.borrow().map().actions.iter().map(|a| a.name.clone()).collect();
            lua.create_sequence_from(names)
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "bindingsOf",
        lua.create_function(move |lua, name: String| {
            let sys = s.borrow();
            let chips: Vec<String> = sys
                .map()
                .actions
                .iter()
                .find(|a| a.name == name)
                .map(|a| a.bindings.iter().map(|b| b.chip()).collect())
                .unwrap_or_default();
            lua.create_sequence_from(chips)
        })?,
    )?;

    // --- runtime rebinding -------------------------------------------------
    let s = sys.clone();
    t.set(
        "startRebind",
        lua.create_function(move |_, (action, filter): (String, Option<String>)| {
            s.borrow_mut().start_rebind(action, slot, parse_filter(filter.as_deref()));
            Ok(())
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "pendingRebind",
        lua.create_function(move |_, ()| {
            // The chip text once something has been captured, else the action
            // name while still waiting, else nil — enough for a menu to print
            // "press any button…" and then the result.
            let sys = s.borrow();
            Ok(sys.pending_rebind().map(|p| match &p.captured {
                Some(c) => c.clone().binding().chip(),
                None => String::new(),
            }))
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "commitRebind",
        lua.create_function(move |_, ()| {
            let captured = s.borrow().pending_rebind().and_then(|p| p.captured.clone());
            Ok(match captured {
                Some(c) => s.borrow_mut().commit_rebind(c),
                None => false,
            })
        })?,
    )?;

    let s = sys.clone();
    t.set(
        "cancelRebind",
        lua.create_function(move |_, ()| {
            s.borrow_mut().cancel_rebind();
            Ok(())
        })?,
    )?;

    Ok(())
}

fn parse_filter(name: Option<&str>) -> BindFilter {
    match name.map(str::to_ascii_lowercase).as_deref() {
        Some("keyboard") => BindFilter::KeyboardOnly,
        Some("pad") | Some("gamepad") => BindFilter::PadOnly,
        Some("axis") => BindFilter::AxisOnly,
        _ => BindFilter::AnyButton,
    }
}

/// Install the whole action API onto the `input` table.
pub fn install(lua: &Lua, t: &Table, sys: &SharedInput, domain: &SharedDomain) {
    // Player 1's functions live directly on `input`.
    if let Err(e) = install_for_slot(lua, t, sys, domain, 0) {
        eprintln!("[lua] failed to install the input action API: {e}");
        return;
    }

    // `input.player(n)` — the same API bound to another local player. A fighter
    // gives both characters the SAME script and passes the slot in as a param:
    //   local me = input.player(params.player)
    let (s, d) = (sys.clone(), domain.clone());
    let _ = t.set(
        "player",
        lua.create_function(move |lua, n: u8| {
            let t = lua.create_table()?;
            // 1-based for scripts (player 1, player 2) — 0-based internally.
            install_for_slot(lua, &t, &s, &d, n.saturating_sub(1))?;
            Ok(t)
        })
        .ok(),
    );

    // --- contexts (global, not per-player) ---------------------------------
    let s = sys.clone();
    let _ = t.set(
        "pushContext",
        lua.create_function(move |_, (name, opts): (String, Option<Table>)| {
            let (mut priority, mut consume, mut enabled) = (0, false, Vec::new());
            if let Some(o) = opts {
                priority = o.get::<i32>("priority").unwrap_or(0);
                consume = o.get::<bool>("consume").unwrap_or(false);
                if let Ok(list) = o.get::<Table>("enabled") {
                    for v in list.sequence_values::<Value>().flatten() {
                        if let Some(s) = v.as_str() {
                            enabled.push(s.to_string());
                        }
                    }
                }
            }
            s.borrow_mut().push_context(Context {
                name,
                priority,
                enabled,
                mode: if consume { ConsumeMode::Consume } else { ConsumeMode::Passthrough },
            });
            Ok(())
        })
        .ok(),
    );

    let s = sys.clone();
    let _ = t.set(
        "popContext",
        lua.create_function(move |_, name: String| Ok(s.borrow_mut().pop_context(&name))).ok(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_input::{Action, Binding, InputMap, Key, RawInput, Source};

    /// A Lua state with the action API installed over a two-action map.
    fn fixture() -> (Lua, SharedInput, SharedDomain) {
        let map = InputMap {
            actions: vec![
                Action { name: "Punch".into(), bindings: vec![Binding::new(Source::Key(Key::KeyJ))] },
                Action { name: "Kick".into(), bindings: vec![Binding::new(Source::Key(Key::KeyK))] },
            ],
            players: 2,
            ..Default::default()
        };
        let sys: SharedInput = Rc::new(std::cell::RefCell::new(InputSystem::new(map)));
        let domain: SharedDomain = Rc::new(Cell::new(Domain::Frame));
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        install(&lua, &t, &sys, &domain);
        lua.globals().set("input", t).unwrap();
        (lua, sys, domain)
    }

    fn keys(k: &[Key]) -> RawInput {
        RawInput { keys: k.iter().copied().collect(), ..Default::default() }
    }

    #[test]
    fn actions_read_from_lua() {
        let (lua, sys, _d) = fixture();
        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        assert!(lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
        assert!(lua.load(r#"return input.justPressed("Punch")"#).eval::<bool>().unwrap());
        assert!(!lua.load(r#"return input.action("Kick")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn an_unknown_action_is_false_not_an_error() {
        // A typo in a script must not abort the whole update.
        let (lua, _s, _d) = fixture();
        assert!(!lua.load(r#"return input.action("Punhc")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn the_domain_cell_switches_which_edges_a_script_sees() {
        let (lua, sys, domain) = fixture();
        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        domain.set(Domain::Frame);
        assert!(lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
        domain.set(Domain::Tick);
        assert!(
            !lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap(),
            "the tick domain has not resolved yet"
        );
        sys.borrow_mut().resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn buffering_and_consuming_work_from_lua() {
        let (lua, sys, _d) = fixture();
        sys.borrow_mut().resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(lua.load(r#"return input.buffered("Punch", 4)"#).eval::<bool>().unwrap());
        assert!(lua.load(r#"return input.consume("Punch", 4)"#).eval::<bool>().unwrap());
        assert!(!lua.load(r#"return input.buffered("Punch", 4)"#).eval::<bool>().unwrap());
    }

    #[test]
    fn buffered_defaults_to_a_small_window() {
        let (lua, sys, _d) = fixture();
        sys.borrow_mut().resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        assert!(lua.load(r#"return input.buffered("Punch")"#).eval::<bool>().unwrap());
        for _ in 0..DEFAULT_BUFFER {
            sys.borrow_mut().resolve_tick(&RawInput::default(), 0.016);
        }
        assert!(!lua.load(r#"return input.buffered("Punch")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn axis2_returns_two_values() {
        let map = InputMap::starter();
        let sys: SharedInput = Rc::new(std::cell::RefCell::new(InputSystem::new(map)));
        let domain: SharedDomain = Rc::new(Cell::new(Domain::Frame));
        let lua = Lua::new();
        let t = lua.create_table().unwrap();
        install(&lua, &t, &sys, &domain);
        lua.globals().set("input", t).unwrap();

        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyD]), 0.016);
        let (x, y): (f32, f32) =
            lua.load(r#"return input.axis2("Move")"#).eval().unwrap();
        assert_eq!((x, y), (1.0, 0.0));
    }

    #[test]
    fn players_are_addressed_one_based_from_lua() {
        let (lua, sys, _d) = fixture();
        sys.borrow_mut().resolve_tick(&keys(&[Key::KeyJ]), 0.016);
        // Both players are on the keyboard, so both see it; what matters is
        // that player(1) maps to slot 0 and consuming is per-player.
        assert!(lua.load(r#"return input.player(1).buffered("Punch", 4)"#).eval::<bool>().unwrap());
        assert!(lua.load(r#"return input.player(2).buffered("Punch", 4)"#).eval::<bool>().unwrap());
        lua.load(r#"input.player(1).consume("Punch", 4)"#).exec().unwrap();
        assert!(!lua.load(r#"return input.player(1).buffered("Punch", 4)"#).eval::<bool>().unwrap());
        assert!(
            lua.load(r#"return input.player(2).buffered("Punch", 4)"#).eval::<bool>().unwrap(),
            "P1 consuming must not spend P2's press"
        );
    }

    #[test]
    fn player_zero_does_not_underflow_into_another_slot() {
        let (lua, _s, _d) = fixture();
        // `input.player(0)` is a script bug; it must clamp to player 1, not
        // wrap a u8 to 255 and index wildly.
        assert!(!lua.load(r#"return input.player(0).action("Punch")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn contexts_are_pushed_and_popped_from_lua() {
        let (lua, sys, _d) = fixture();
        lua.load(
            r#"input.pushContext("menu", { priority = 100, consume = true, enabled = { "Kick" } })"#,
        )
        .exec()
        .unwrap();
        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyJ, Key::KeyK]), 0.016);
        assert!(!lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
        assert!(lua.load(r#"return input.action("Kick")"#).eval::<bool>().unwrap());

        assert!(lua.load(r#"return input.popContext("menu")"#).eval::<bool>().unwrap());
        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyJ]), 0.016);
        assert!(lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn a_settings_menu_can_list_actions_and_their_bindings() {
        let (lua, _s, _d) = fixture();
        let names: Vec<String> = lua.load("return input.actions()").eval().unwrap();
        assert_eq!(names, vec!["Punch".to_string(), "Kick".to_string()]);
        let chips: Vec<String> = lua.load(r#"return input.bindingsOf("Punch")"#).eval().unwrap();
        assert_eq!(chips, vec!["⌨ J".to_string()]);
        let none: Vec<String> = lua.load(r#"return input.bindingsOf("Nope")"#).eval().unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn a_script_can_drive_a_rebind_end_to_end() {
        let (lua, sys, _d) = fixture();
        lua.load(r#"input.startRebind("Punch", "keyboard")"#).exec().unwrap();
        assert_eq!(
            lua.load("return input.pendingRebind()").eval::<Option<String>>().unwrap(),
            Some(String::new()),
            "armed, nothing captured yet"
        );

        // The host polls devices; here we do it directly.
        let got = sys.borrow_mut().poll_rebind(&keys(&[Key::KeyU])).expect("captured");
        assert_eq!(got.source, Source::Key(Key::KeyU));
        assert_eq!(
            lua.load("return input.pendingRebind()").eval::<Option<String>>().unwrap(),
            Some("⌨ U".to_string())
        );
        assert!(lua.load("return input.commitRebind()").eval::<bool>().unwrap());

        sys.borrow_mut().resolve_frame(&keys(&[Key::KeyU]), 0.016);
        assert!(lua.load(r#"return input.action("Punch")"#).eval::<bool>().unwrap());
    }

    #[test]
    fn cancelling_a_rebind_from_lua_leaves_the_map_alone() {
        let (lua, sys, _d) = fixture();
        let before = sys.borrow().map().clone();
        lua.load(r#"input.startRebind("Punch")"#).exec().unwrap();
        lua.load("input.cancelRebind()").exec().unwrap();
        assert_eq!(
            lua.load("return input.pendingRebind()").eval::<Option<String>>().unwrap(),
            None
        );
        assert!(!lua.load("return input.commitRebind()").eval::<bool>().unwrap());
        assert_eq!(sys.borrow().map(), &before);
    }

    #[test]
    fn facing_is_settable_from_lua_and_mirrors_motions() {
        let (lua, sys, _d) = fixture();
        lua.load("input.setFacing(-1)").exec().unwrap();
        assert_eq!(lua.load("return input.facing()").eval::<f32>().unwrap(), -1.0);
        assert_eq!(sys.borrow().facing(0), -1.0);
    }
}
