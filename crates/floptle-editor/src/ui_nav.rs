//! Keyboard / gamepad navigation of game UI (docs/ui-system-2-proposal.md §D).
//!
//! The geometry and the auto-repeat live in `floptle_ui::nav` (headless and
//! tested). This is the editor/runtime half: reading the direction, deciding
//! which layer owns focus, and firing the hooks.
//!
//! **Bindings come from the project's action map.** If it defines `UiUp`,
//! `UiDown`, `UiLeft`, `UiRight`, `UiSubmit`, `UiCancel` — or a 2D axis named
//! `UiMove` — those are used, and they are rebindable like anything else. If it
//! defines none of them, the engine falls back to arrows / d-pad / left stick /
//! Enter / Escape so a fresh project's menu is navigable before anyone has
//! opened the Input settings. Defining even one of them takes over completely,
//! because a half-overridden control scheme is worse than either.

use floptle_input::{Domain, Key, PadAxis, PadButton, PadControl, PadId, Source};
use floptle_ui::nav::Dir4;

use crate::Editor;

/// The names the engine looks for. Nothing is auto-created in `input.ron`: a
/// file appearing on disk that nobody asked for is its own kind of rude, and
/// the fallback means it isn't needed.
pub(crate) const NAV_ACTIONS: [&str; 6] =
    ["UiUp", "UiDown", "UiLeft", "UiRight", "UiSubmit", "UiCancel"];
pub(crate) const NAV_AXIS: &str = "UiMove";

/// This frame's navigation input, as LEVELS — edges and repeat are applied
/// afterwards, so both input paths behave identically.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct NavInput {
    pub dir: Option<Dir4>,
    pub submit: bool,
    pub cancel: bool,
}

/// Stick deadzone for reading a direction. Generous: this is menu navigation,
/// not aiming, and a stick that needs a firm push is a stick that doesn't
/// double-move.
const DEADZONE: f32 = 0.55;

impl Editor {
    /// True when the project's action map defines any of the UI actions — in
    /// which case the engine's fallback bindings step aside entirely.
    fn ui_nav_mapped(&self) -> bool {
        let sys = self.script_host.input_system().borrow();
        let map = sys.map();
        NAV_ACTIONS.iter().any(|n| map.action_index(n).is_some())
            || map.axes2.iter().any(|a| a.name == NAV_AXIS)
    }

    /// Read the navigation levels for this frame.
    ///
    /// Neutral when the game view isn't focused, for the same reason raw keys
    /// are: you're editing, not playing, and arrowing through the Hierarchy
    /// must not walk a menu in the background.
    pub(crate) fn ui_nav_input(&self) -> NavInput {
        if !(self.game_view() || self.game_trap || self.player_mode) {
            return NavInput::default();
        }
        if self.ui_nav_mapped() { self.ui_nav_from_actions() } else { self.ui_nav_fallback() }
    }

    fn ui_nav_from_actions(&self) -> NavInput {
        let sys = self.script_host.input_system().borrow();
        let on = |n: &str| sys.action(Domain::Frame, 0, n);
        let (ax, ay) = sys.axis2(Domain::Frame, 0, NAV_AXIS);
        // Buttons beat the stick: a d-pad bound to UiUp should not be fighting
        // a stick resting slightly off-centre.
        let dir = if on("UiUp") {
            Some(Dir4::Up)
        } else if on("UiDown") {
            Some(Dir4::Down)
        } else if on("UiLeft") {
            Some(Dir4::Left)
        } else if on("UiRight") {
            Some(Dir4::Right)
        } else {
            Dir4::from_vector(ax, -ay, DEADZONE)
        };
        NavInput { dir, submit: on("UiSubmit"), cancel: on("UiCancel") }
    }

    /// The no-configuration path: arrows, d-pad, left stick, Enter, Escape.
    fn ui_nav_fallback(&self) -> NavInput {
        let raw = &self.raw_input;
        let key = |k: Key| raw.held(Source::Key(k), 0, 0.5);
        let pad = |b: PadButton| {
            raw.held(Source::Pad { id: PadId::Any, ctrl: PadControl::Button(b) }, 0, 0.5)
        };
        let axis = |a: PadAxis| raw.value(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(a) }, 0);
        let up = key(Key::ArrowUp) || pad(PadButton::DPadUp);
        let down = key(Key::ArrowDown) || pad(PadButton::DPadDown);
        let left = key(Key::ArrowLeft) || pad(PadButton::DPadLeft);
        let right = key(Key::ArrowRight) || pad(PadButton::DPadRight);
        let dir = if up {
            Some(Dir4::Up)
        } else if down {
            Some(Dir4::Down)
        } else if left {
            Some(Dir4::Left)
        } else if right {
            Some(Dir4::Right)
        } else {
            // Stick Y is +up on a pad; screen Y is +down.
            Dir4::from_vector(axis(PadAxis::LeftStickX), -axis(PadAxis::LeftStickY), DEADZONE)
        };
        NavInput {
            dir,
            submit: key(Key::Enter) || key(Key::Space) || pad(PadButton::South),
            cancel: key(Key::Escape) || pad(PadButton::East),
        }
    }

    /// Move focus and fire submit/cancel. Runs inside the interact pass, before
    /// hover and click, so a gamepad press and a mouse click end up in the same
    /// event queue in the same order.
    ///
    /// `layers` is every screen's `(layer, roots, solved rects)` this frame, in
    /// draw order — focus belongs to the FRONT-most layer that has anything
    /// focusable, which is what makes a modal over a menu behave.
    pub(crate) fn ui_navigate(
        &mut self,
        layers: &[(floptle_ui::UiLayer, Vec<floptle_ui::Node>, Vec<floptle_ui::Placed>)],
        dt: f32,
    ) {
        let input = self.ui_nav_input();

        // Front-most layer with focusable elements owns focus.
        let owner = layers.iter().rev().find_map(|(layer, roots, placed)| {
            let f = floptle_ui::nav::focusables(roots, placed);
            (!f.is_empty()).then_some((*layer, roots, f))
        });
        let Some((layer, roots, focusables)) = owner else {
            // Nothing focusable on screen: drop focus rather than keep pointing
            // at an element that may not even exist any more.
            self.ui_focus_set(None);
            self.ui_nav_repeat.clear();
            self.ui_submit_was = input.submit;
            self.ui_cancel_was = input.cancel;
            return;
        };

        // A focus that no longer resolves (screen changed, element hidden)
        // falls back to the first focusable rather than vanishing.
        let mut focus = self.ui_focus.filter(|id| focusables.iter().any(|(f, _)| f == id));

        let moved = self.ui_nav_repeat.step(input.dir, dt, layer.nav_delay, layer.nav_repeat);
        if moved && let Some(dir) = input.dir {
            focus = Some(match focus {
                // Nothing focused yet: the first direction press focuses the
                // first element rather than moving from nowhere.
                None => floptle_ui::nav::first(&focusables).unwrap_or_default(),
                Some(from) => {
                    let from_rect =
                        focusables.iter().find(|(id, _)| *id == from).map(|(_, r)| *r);
                    let by_name = self.ui_nav_override(roots, from, dir, &focusables);
                    match (by_name, from_rect) {
                        (Some(id), _) => id,
                        (None, Some(r)) => floptle_ui::nav::nearest(r, &focusables, dir)
                            .or_else(|| {
                                layer
                                    .nav_wrap
                                    .then(|| floptle_ui::nav::wrap(r, &focusables, dir))
                                    .flatten()
                            })
                            .unwrap_or(from),
                        (None, None) => from,
                    }
                }
            });
        }
        self.ui_focus_set(focus);

        // Submit fires the SAME `clicked` hook a mouse fires. A button that
        // works with a pointer works with a pad, with no second code path in
        // anyone's script.
        let submit_edge = input.submit && !self.ui_submit_was;
        let cancel_edge = input.cancel && !self.ui_cancel_was;
        self.ui_submit_was = input.submit;
        self.ui_cancel_was = input.cancel;
        if let Some(id) = self.ui_focus {
            if submit_edge {
                self.ui_events.push((id, "pressed"));
                self.ui_events.push((id, "released"));
                self.ui_events.push((id, "clicked"));
            }
            if cancel_edge {
                self.ui_events.push((id, "cancelled"));
            }
        }
    }

    /// The `nav` override for a direction, resolved from an element NAME to an
    /// id within this layer. An override naming something that isn't focusable
    /// is ignored rather than swallowing the press.
    fn ui_nav_override(
        &self,
        roots: &[floptle_ui::Node],
        from: u32,
        dir: Dir4,
        focusables: &[(u32, [f32; 4])],
    ) -> Option<u32> {
        fn find(ns: &[floptle_ui::Node], id: u32) -> Option<&floptle_ui::Node> {
            for n in ns {
                if n.id == id {
                    return Some(n);
                }
                if let Some(f) = find(&n.children, id) {
                    return Some(f);
                }
            }
            None
        }
        let want = find(roots, from)?.spec.nav.as_ref()?.get(dir)?.to_string();
        let target = self
            .world
            .query::<floptle_core::Name>()
            .find(|(_, n)| n.0 == want)
            .map(|(e, _)| e.index())?;
        focusables.iter().any(|(id, _)| *id == target).then_some(target)
    }

    /// Change focus, firing `focusExit` / `focusEnter` on the way.
    pub(crate) fn ui_focus_set(&mut self, next: Option<u32>) {
        if next == self.ui_focus {
            return;
        }
        if let Some(old) = self.ui_focus {
            self.ui_events.push((old, "focusExit"));
        }
        if let Some(new) = next {
            self.ui_events.push((new, "focusEnter"));
        }
        self.ui_focus = next;
    }
}
