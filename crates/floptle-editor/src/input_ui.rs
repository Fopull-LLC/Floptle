//! The **Input** section of ⚙ Settings — the action map's editor.
//!
//! The job this screen has to do is explain itself. Someone opening it has a
//! game that reads `input.action("Jump")` and needs to understand, without
//! reading docs, that:
//!
//! 1. an **action** is a name their script asks about,
//! 2. a **binding** is a key/button that triggers it, and there can be several,
//! 3. every action wants one on the keyboard *and* one on a pad, and
//! 4. an action with no bindings is a control that silently does nothing.
//!
//! So each row states the Lua call that reads it, bindings are grouped by
//! device with the empty side called out, and the live tester at the bottom
//! proves a binding works without entering Play.

use floptle_input::{
    ActionState, Axis1Binding, Axis2Binding, InputMap, PendingRebind,
    Socd, Source,
};

use crate::icons;
use crate::input_scan::{InputScan, UsageKind};

/// Every row is this tall regardless of how many chips it holds, so the list
/// doesn't jump around as bindings are added.
const ROW_H: f32 = 26.0;
/// Width of the name column, so names and chips line up down the page.
const NAME_W: f32 = 120.0;

/// Edits collected during the pass, applied after — the map is borrowed for
/// display while the rows draw.
#[derive(Default)]
pub(crate) struct InputEdits {
    pub(crate) commands: Vec<InputCmd>,
    /// The map changed and should be written to `input.ron`.
    pub(crate) save: bool,
    pub(crate) rescan: bool,
}

pub(crate) enum InputCmd {
    /// The editor's edited copy of the whole map.
    SetMap(Box<InputMap>),
    /// Arm press-to-bind for one slot of one binding.
    Capture(crate::input_editor::CaptureTarget),
    /// Add whatever kind of entry a script call site implies.
    AddEntry {
        name: String,
        kind: UsageKind,
    },
    CancelRebind,
    SeedStarter,
    SetPlayers(u8),
}

/// Draw the Input section.
///
/// `test` is a live, focus-independent resolve of the current devices — the
/// tester has to light up while you're editing settings, which is exactly when
/// the game view is *not* focused and gameplay input is neutral.
#[allow(clippy::too_many_arguments)]
pub(crate) fn input_section(
    ui: &mut egui::Ui,
    map: &InputMap,
    pending: Option<&PendingRebind>,
    scan: &InputScan,
    test: &ActionState,
    pad_names: &[Option<String>],
    state: &mut crate::input_editor::InputUiState,
    capture: Option<&crate::input_editor::CaptureTarget>,
    query: &str,
) -> InputEdits {
    let mut edits = InputEdits::default();

    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(icons::RESCAN)
                .on_hover_text("re-read your scripts for action names")
                .clicked()
            {
                edits.rescan = true;
            }
        });
    });
    primer(ui);
    rebind_banner(ui, pending, capture, &mut edits);
    gaps_banner(ui, map, &mut edits);

    // Every entry and every binding, editable — see `input_editor`.
    crate::input_editor::editor(ui, map, state, capture, scan, test, query, &mut edits);

    // ---- scripts referencing things the map doesn't define ---------------
    missing_entries(ui, map, scan, &mut edits);
    raw_key_notice(ui, scan);

    // ---- players -------------------------------------------------------
    ui.add_space(12.0);
    crate::settings_ui::row(
        ui,
        "Local players",
        // Rollback needs this raised too, which is not obvious: a rollback
        // match gives every fighter its own input slot whether the players are
        // on one couch or two continents apart. Left at 1, the second fighter
        // has nowhere to read input from and stands still all match — so say so
        // here rather than only in the fault it eventually raises.
        Some("split-screen / same-couch versus — and one slot per fighter in a rollback match"),
        |ui| {
            let mut n = map.players.max(1);
            if ui
                .add(egui::DragValue::new(&mut n).range(1..=4u8))
                .on_hover_text(
                    "how many input slots exist. Raise it for split-screen, AND for a \
                     rollback (fighting-game) scene: every Rollback node reads its own slot, \
                     so two fighters need two slots even when the opponent is remote.",
                )
                .changed()
            {
                edits.commands.push(InputCmd::SetPlayers(n));
                edits.save = true;
            }
            if n > 1 {
                ui.label(
                    egui::RichText::new("read with input.player(n) — 1-based").weak().small(),
                );
            }
        },
    );

    live_tester(ui, map, test, pad_names);
    edits
}

/// The two sentences that make the rest of the screen make sense.
fn primer(ui: &mut egui::Ui) {
    ui.label(
        egui::RichText::new(
            "Your scripts ask for an ACTION by name. Here you decide which keys, mouse \
             buttons and gamepad controls trigger it — so one script works on every device, \
             and players can rebind it.",
        )
        .weak(),
    );
    ui.add_space(4.0);
    egui::CollapsingHeader::new(crate::responsive::header_text(ui, "How do I use this?")).id_salt("input_howto").default_open(crate::responsive::start_open(false)).show(ui, |ui| {
        // Every line through `para`: this is a page of prose, and a bare label
        // extends rather than wraps.
        crate::responsive::para(ui, "1.  In a script, read the action by name:");
        ui.add_space(2.0);
        crate::responsive::para(
            ui,
            egui::RichText::new("     if input.justPressed(\"Jump\") then …").monospace(),
        );
        ui.add_space(6.0);
        crate::responsive::para(
            ui,
            format!(
                "2.  It appears in the list below (this page reads your scripts). If it's new \
                 it shows a {} — nothing is bound to it yet.",
                icons::WARN
            ),
        );
        ui.add_space(6.0);
        crate::responsive::para(
            ui,
            format!(
                "3.  Click {}  to bind by PRESSING a key or button, or {} to pick one from a \
                 list — the list needs no controller plugged in.",
                icons::ADD,
                icons::MENU
            ),
        );
        ui.add_space(6.0);
        crate::responsive::para(
            ui,
            "4.  Bind it twice: once on the keyboard, once on a pad. Both trigger the same \
             action, so the script never asks which you're using.",
        );
        ui.add_space(6.0);
        crate::responsive::para(ui, "5.  Mash the control and watch the LIVE strip at the bottom light up.");
    });
    ui.add_space(8.0);
}

/// The armed press-to-bind prompt.
fn rebind_banner(
    ui: &mut egui::Ui,
    pending: Option<&PendingRebind>,
    capture: Option<&crate::input_editor::CaptureTarget>,
    edits: &mut InputEdits,
) {
    let Some(p) = pending else { return };
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                match &p.captured {
                    Some(c) => {
                        ui.colored_label(
                            egui::Color32::LIGHT_GREEN,
                            format!("bound {}", c.clone().binding().chip()),
                        );
                    }
                    None => {
                        ui.label(
                            egui::RichText::new(format!(
                                "Press any key, mouse button or gamepad control for {}…",
                                capture.map_or_else(|| format!("“{}”", p.action), |c| c.describe())
                            ))
                            .strong(),
                        );
                    }
                }
                if ui.button("Cancel").clicked() {
                    edits.commands.push(InputCmd::CancelRebind);
                }
                ui.label(egui::RichText::new("or press Esc").weak().small());
            });
        });
    ui.add_space(8.0);
}

/// One call-to-action covering everything currently unbound or missing.
fn gaps_banner(ui: &mut egui::Ui, map: &InputMap, edits: &mut InputEdits) {
    let unbound = map.actions.iter().filter(|a| a.bindings.is_empty()).count()
        + map.axes2.iter().filter(|a| a.bindings.is_empty()).count()
        + map.axes1.iter().filter(|a| a.bindings.is_empty()).count();
    if map.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label("This project has no actions yet.");
            if ui.button("Set up the standard controls").on_hover_text(STARTER_TIP).clicked() {
                edits.commands.push(InputCmd::SeedStarter);
                edits.save = true;
            }
        });
        ui.add_space(8.0);
        return;
    }
    if unbound > 0 {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(
                egui::Color32::from_rgb(224, 168, 64),
                format!("{} {unbound} unbound", icons::WARN),
            );
            ui.label(
                egui::RichText::new("— these do nothing until something triggers them.").weak(),
            );
            if ui.small_button("Fill in the standard ones").on_hover_text(STARTER_TIP).clicked() {
                edits.commands.push(InputCmd::SeedStarter);
                edits.save = true;
            }
        });
        ui.add_space(8.0);
    }
}

const STARTER_TIP: &str = "Adds Move / Look / Jump / Fire / Interact / Sprint / Crouch / Pause \
                           and friends, each bound on BOTH keyboard and gamepad — the names the \
                           shipped default scripts use.\n\nOnly fills gaps: your own actions, \
                           bindings and settings are left alone.";

fn group_header(ui: &mut egui::Ui, title: &str, blurb: &str) {
    ui.add_space(12.0);
    ui.label(egui::RichText::new(title).strong());
    // Through `para`, which wraps to the panel: these blurbs carry a Lua call
    // in them and are therefore long by nature, and a bare label extends.
    crate::responsive::para(ui, egui::RichText::new(blurb).weak().small());
    ui.add_space(4.0);
}






/// How wide a chip carrying `text` will be, including the padding a button puts
/// round it and the gap before the next widget.
fn chip_w(ui: &egui::Ui, text: &str) -> f32 {
    crate::responsive::text_w_in(ui, egui::TextStyle::Monospace, text)
        + ui.spacing().button_padding.x * 2.0
        + ui.spacing().item_spacing.x
}

/// Break the line first if something `want` wide will not fit on what is left
/// of it.
///
/// See `binding_chips` for why measuring is necessary rather than trusting the
/// wrapped layout to break by itself.
pub(crate) fn wrap_before(ui: &mut egui::Ui, want: f32) {
    // Guarded on `main_wrap` for the reason in `responsive::fit_here_wrapping`:
    // `end_row` is also how a `Grid` row ends, and breaking one of those would
    // move every later control into the wrong column.
    if ui.layout().main_wrap && crate::responsive::usable_width(ui) < want {
        ui.end_row();
    }
}

/// Make room for a chip: break the line if it will not fit on what is left of
/// one, and shorten it if it will not fit on a whole one.
///
/// Returns what to actually draw. A binding's chip names a control and some
/// controls have long names — "🖱 Motion x0.006 (hold RMB)" is wider than a
/// 200px docked panel is, so there is no line for it to move to and the only
/// remaining lever is the string. The hover text carries the full name.
fn wrap_before_chip(ui: &mut egui::Ui, text: &str) -> String {
    wrap_before(ui, chip_w(ui, text));
    let line = crate::responsive::usable_width(ui);
    let pad = ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x;
    crate::responsive::elide_in(
        ui,
        egui::TextStyle::Monospace,
        text,
        (line - pad).max(8.0),
    )
}

/// A label in a wrapped row, moved to the next line if it will not fit.
fn wrapped_label(ui: &mut egui::Ui, text: egui::RichText, plain: &str) -> egui::Response {
    wrap_before(
        ui,
        crate::responsive::text_w_in(ui, egui::TextStyle::Body, plain)
            + ui.spacing().item_spacing.x,
    );
    ui.label(text)
}





fn missing_entries(
    ui: &mut egui::Ui,
    map: &InputMap,
    scan: &InputScan,
    edits: &mut InputEdits,
) {
    let missing: Vec<_> = scan.entries().filter(|u| !defined(map, u.kind, &u.name)).collect();
    if missing.is_empty() {
        return;
    }
    group_header(
        ui,
        "Used by your scripts, but not defined here",
        "Each of these is a control that currently does nothing.",
    );
    for u in missing {
        ui.push_id(("missing", &u.name, u.kind.label()), |ui| {
            ui.horizontal(|ui| {
                ui.set_min_height(ROW_H);
                ui.colored_label(egui::Color32::from_rgb(224, 168, 64), icons::WARN);
                ui.add_sized(
                    [crate::responsive::usable_width(ui).min(NAME_W - 20.0), 20.0],
                    egui::Label::new(crate::responsive::elide(
                        ui,
                        &u.name,
                        crate::responsive::usable_width(ui).min(NAME_W - 20.0),
                    ))
                    .selectable(false),
                )
                    .on_hover_text(format!("{}:{} — {} use(s)", u.file, u.line, u.count));
                ui.label(egui::RichText::new(u.kind.label()).weak().small());
                if ui.button("Add it").clicked() {
                    edits.commands.push(InputCmd::AddEntry {
                        name: u.name.clone(),
                        kind: u.kind,
                    });
                    edits.save = true;
                }
            });
        });
    }
}

fn raw_key_notice(ui: &mut egui::Ui, scan: &InputScan) {
    let raw: Vec<_> = scan.raw_key_uses().collect();
    if raw.is_empty() {
        return;
    }
    ui.add_space(12.0);
    egui::CollapsingHeader::new(format!("{} script(s) still poll raw keys", raw.len()))
        .id_salt("raw_key_uses")
        .default_open(crate::responsive::start_open(false))
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "These work in single player, but can't be rebound, don't work on a \
                     gamepad, and read as NOT PRESSED on a networked Predicted node.",
                )
                .weak()
                .small(),
            );
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                for u in raw.iter().take(40) {
                    let text = format!("\"{}\"", u.name);
                    let shown = wrap_before_chip(ui, &text);
                    ui.label(egui::RichText::new(shown).monospace())
                        .on_hover_text(format!("{}:{} — {} use(s)", u.file, u.line, u.count));
                }
                if raw.len() > 40 {
                    ui.label(egui::RichText::new(format!("+{} more", raw.len() - 40)).weak());
                }
            });
        });
}

fn live_tester(
    ui: &mut egui::Ui,
    map: &InputMap,
    test: &ActionState,
    pad_names: &[Option<String>],
) {
    ui.add_space(14.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label(egui::RichText::new("LIVE").strong());
        crate::responsive::para(
            ui,
            egui::RichText::new("press something — this updates without entering Play")
                .weak()
                .small(),
        );
    });
    ui.add_space(4.0);

    let pads: Vec<String> = pad_names
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.as_ref().map(|n| format!("P{}  {}  {n}", i + 1, icons::PAD)))
        .collect();
    ui.horizontal_wrapped(|ui| {
        if pads.is_empty() {
            ui.label(
                egui::RichText::new("no gamepad connected").weak(),
            )
            .on_hover_text(
                "Plug one in and it appears here immediately.\n\
                 You can still add gamepad bindings without one — use the ▾ menu.",
            );
        } else {
            for p in pads {
                let shown = wrap_before_chip(ui, &p);
                ui.label(egui::RichText::new(shown).monospace()).on_hover_text(p);
            }
        }
    });
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (i, a) in map.actions.iter().enumerate() {
            let on = test.is_held(i);
            let text =
                format!("{} {}", if on { icons::ON } else { icons::OFF }, a.name);
            let colored = egui::RichText::new(&text).color(if on {
                egui::Color32::LIGHT_GREEN
            } else {
                ui.visuals().weak_text_color()
            });
            wrapped_label(ui, colored, &text);
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (i, ax) in map.axes2.iter().enumerate() {
            let (x, y) = test.axis2(i);
            let text = format!("{}: ({x:+.2}, {y:+.2})", ax.name);
            let shown = wrap_before_chip(ui, &text);
            ui.label(egui::RichText::new(shown).monospace());
        }
        for (i, ax) in map.axes1.iter().enumerate() {
            let text = format!("{}: {:+.2}", ax.name, test.axis1(i));
            let shown = wrap_before_chip(ui, &text);
            ui.label(egui::RichText::new(shown).monospace());
        }
    });
}


/// Does the map already define this scanned reference?
fn defined(map: &InputMap, kind: UsageKind, name: &str) -> bool {
    match kind {
        UsageKind::Action => map.action_index(name).is_some(),
        UsageKind::Axis1 => map.axis1_index(name).is_some(),
        UsageKind::Axis2 => map.axis2_index(name).is_some(),
        UsageKind::Motion => map.motion(name).is_some(),
        // Raw polls aren't map entries; they're listed separately.
        UsageKind::RawKey => true,
    }
}

/// What a direction does when opposites are held at once, in words.
pub(crate) fn socd_label(s: Socd) -> &'static str {
    match s {
        Socd::Neutral => "cancel out",
        Socd::LastWins => "last pressed wins",
        Socd::Positive => "up / right wins",
        Socd::Negative => "down / left wins",
    }
}

/// `"  P2"` for a binding scoped to one local player, empty for the usual case.
fn player_suffix(player: Option<u8>) -> String {
    player.map(|p| format!("  P{}", p + 1)).unwrap_or_default()
}

/// A compact one-chip summary of a 2D axis binding.
pub(crate) fn axis2_chip(b: &Axis2Binding) -> String {
    match b {
        Axis2Binding::Keys { up, down, left, right, player } => {
            let l = |s: &Source| s.label();
            format!(
                "{} {}{}{}{}{}",
                icons::KEYBOARD,
                l(up),
                l(left),
                l(down),
                l(right),
                player_suffix(*player)
            )
        }
        Axis2Binding::Stick { x, deadzone, .. } => {
            let stick = if matches!(x, floptle_input::PadAxis::LeftStickX) { "L" } else { "R" };
            format!("{} {stick}-Stick dz{deadzone:.2}", icons::PAD)
        }
        Axis2Binding::Mouse { sensitivity, gate, .. } => {
            // Say when it's gated: "the mouse doesn't look" needs a visible
            // answer here, not a trip into input.ron.
            let hold = match gate.first() {
                Some(g) => format!(" (hold {})", g.label()),
                None => String::new(),
            };
            format!("{} Motion x{sensitivity:.3}{hold}", icons::MOUSE)
        }
    }
}

pub(crate) fn axis1_chip(b: &Axis1Binding) -> String {
    match b {
        Axis1Binding::Keys { minus, plus, player } => {
            format!(
                "{} {} / {}{}",
                icons::KEYBOARD,
                minus.label(),
                plus.label(),
                player_suffix(*player)
            )
        }
        Axis1Binding::Analog { source, invert, .. } => {
            let sign = if *invert { "-" } else { "+" };
            format!("{} {}{}", source.device().icon(), sign, source.label())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_input::{Key, PadAxis, PadId};

    #[test]
    fn axis_chips_name_their_device() {
        let keys = Axis2Binding::Keys {
            up: Source::Key(Key::KeyW),
            down: Source::Key(Key::KeyS),
            left: Source::Key(Key::KeyA),
            right: Source::Key(Key::KeyD),
            player: None,
        };
        assert_eq!(axis2_chip(&keys), format!("{} WASD", icons::KEYBOARD));

        let stick = Axis2Binding::Stick {
            player: None,
            id: PadId::Any,
            x: PadAxis::LeftStickX,
            y: PadAxis::LeftStickY,
            deadzone: 0.15,
            sensitivity: 1.0,
            invert_y: false,
            curve: floptle_input::Curve::Linear,
        };
        assert_eq!(axis2_chip(&stick), format!("{} L-Stick dz0.15", icons::PAD));
    }

    #[test]
    fn a_gated_mouse_binding_says_so_on_the_chip() {
        let gated = Axis2Binding::Mouse {
            sensitivity: 0.006,
            invert_y: false,
            rate: true,
            gate: vec![Source::Mouse(floptle_input::MouseButton::Right)],
        };
        assert!(axis2_chip(&gated).contains("hold RMB"), "{}", axis2_chip(&gated));
        let free = Axis2Binding::Mouse {
            sensitivity: 0.006,
            invert_y: false,
            rate: true,
            gate: Vec::new(),
        };
        assert!(!free.to_chip_contains_hold());
    }

    trait ChipTest {
        fn to_chip_contains_hold(&self) -> bool;
    }
    impl ChipTest for Axis2Binding {
        fn to_chip_contains_hold(&self) -> bool {
            axis2_chip(self).contains("hold")
        }
    }

    #[test]
    fn every_socd_mode_has_a_label() {
        for s in [Socd::Neutral, Socd::LastWins, Socd::Positive, Socd::Negative] {
            assert!(!socd_label(s).is_empty(), "{s:?}");
        }
    }

    /// Chips must be renderable — they're built from icon constants plus
    /// device labels, and a tofu square here is what the user reported.
    #[test]
    fn chips_and_labels_render_in_the_editor_font() {
        let ctx = crate::icons::test_context();
        let id = egui::FontId::proportional(14.0);
        let mut samples = vec![
            axis2_chip(&Axis2Binding::Mouse {
                sensitivity: 0.006,
                invert_y: false,
                rate: true,
                gate: vec![Source::Mouse(floptle_input::MouseButton::Right)],
            }),
            socd_label(Socd::LastWins).to_string(),
            STARTER_TIP.to_string(),
        ];
        for &b in floptle_input::PadButton::ALL {
            samples.push(
                Source::Pad { id: PadId::Any, ctrl: floptle_input::PadControl::Button(b) }.chip(),
            );
        }
        for &k in &[Key::Space, Key::ArrowLeft, Key::ShiftLeft, Key::NumpadAdd] {
            samples.push(Source::Key(k).chip());
        }
        let mut tofu = Vec::new();
        ctx.fonts_mut(|f| {
            for s in &samples {
                for c in s.chars() {
                    // Whitespace has no glyph and needs none.
                    if !c.is_whitespace() && !f.has_glyph(&id, c) {
                        tofu.push(format!("{s:?} (U+{:04X})", c as u32));
                    }
                }
            }
        });
        assert!(tofu.is_empty(), "unrenderable text in the input UI:\n  {}", tofu.join("\n  "));
    }
}
