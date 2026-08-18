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
    ActionState, Axis1Binding, Axis2Binding, BindFilter, Device, InputMap, PadId, PendingRebind,
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
    AddAction(String),
    /// A binding chosen from the picker rather than pressed.
    AddBinding {
        action: String,
        source: Source,
    },
    /// Add whatever kind of entry a script call site implies.
    AddEntry {
        name: String,
        kind: UsageKind,
    },
    RemoveAction(String),
    RemoveBinding {
        action: String,
        index: usize,
    },
    /// Scope a binding to one local player (`None` = every player).
    SetBindingPlayer {
        action: String,
        index: usize,
        player: Option<u8>,
    },
    StartRebind {
        action: String,
        filter: BindFilter,
    },
    CancelRebind,
    SeedStarter,
    SetSocd {
        axis: usize,
        socd: Socd,
    },
    SetPlayers(u8),
}

/// Draw the Input section.
///
/// `test` is a live, focus-independent resolve of the current devices — the
/// tester has to light up while you're editing settings, which is exactly when
/// the game view is *not* focused and gameplay input is deliberately neutral.
#[allow(clippy::too_many_arguments)]
pub(crate) fn input_section(
    ui: &mut egui::Ui,
    map: &InputMap,
    pending: Option<&PendingRebind>,
    scan: &InputScan,
    test: &ActionState,
    pad_names: &[Option<String>],
    new_action: &mut String,
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
    rebind_banner(ui, pending, &mut edits);
    gaps_banner(ui, map, &mut edits);

    let show = |name: &str| crate::settings_ui::matches(query, name);

    // ---- actions -------------------------------------------------------
    let actions: Vec<_> = map.actions.iter().filter(|a| show(&a.name)).collect();
    if !actions.is_empty() {
        group_header(
            ui,
            "Actions",
            "Buttons: pressed, held, released. Read with input.action(\"Name\").",
        );
        for action in actions {
            let idx = map.action_index(&action.name).unwrap_or(0);
            action_row(ui, action, idx, scan, test, map.players, &mut edits);
        }
        ui.add_space(4.0);
        add_action_row(ui, map, new_action, &mut edits);
    }

    // ---- axes ----------------------------------------------------------
    let axes2: Vec<_> =
        map.axes2.iter().enumerate().filter(|(_, a)| show(&a.name)).collect();
    if !axes2.is_empty() {
        group_header(
            ui,
            "Directions (2D)",
            "A stick or WASD, as one value. Read with local x, y = input.axis2(\"Name\").",
        );
        for (i, ax) in axes2 {
            axis2_row(ui, i, ax, scan, test, &mut edits);
        }
    }

    let axes1: Vec<_> = map.axes1.iter().enumerate().filter(|(_, a)| show(&a.name)).collect();
    if !axes1.is_empty() {
        group_header(
            ui,
            "Amounts (1D)",
            "A trigger, the wheel, or a key pair. Read with input.axis1(\"Name\").",
        );
        for (i, ax) in axes1 {
            axis1_row(ui, i, ax, scan, test, &mut edits);
        }
    }

    // ---- motions -------------------------------------------------------
    let motions: Vec<_> = map.motions.iter().filter(|m| show(&m.name)).collect();
    if !motions.is_empty() {
        group_header(
            ui,
            "Motions",
            "Fighting-game direction sequences. Read with input.motion(\"qcf\") inside fixedUpdate.",
        );
        ui.horizontal_wrapped(|ui| {
            ui.add_space(NAME_W);
            for m in motions {
                let dirs: Vec<String> = m.dirs.iter().map(|d| d.to_string()).collect();
                let charge =
                    if m.charge > 0 { format!("\nhold {} for {} ticks first", m.dirs[0], m.charge) } else { String::new() };
                ui.label(egui::RichText::new(&m.name).monospace()).on_hover_text(format!(
                    "{} within {} ticks{charge}\n\nNumpad directions: 5 is neutral, 6 is forward.",
                    dirs.join(" → "),
                    m.window
                ));
            }
        });
        ui.label(
            egui::RichText::new("Edit their directions and windows in input.ron.").weak().small(),
        );
    }

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
    egui::CollapsingHeader::new(crate::responsive::header_text(ui, "How do I use this?")).id_salt("input_howto").show(ui, |ui| {
        ui.label("1.  In a script, read the action by name:");
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.add_space(16.0);
            ui.label(egui::RichText::new("if input.justPressed(\"Jump\") then …").monospace());
        });
        ui.add_space(6.0);
        ui.label(format!(
            "2.  It appears in the list below (this page reads your scripts). If it's new it \
             shows a {} — nothing is bound to it yet.",
            icons::WARN
        ));
        ui.add_space(6.0);
        ui.label(format!(
            "3.  Click {}  to bind by PRESSING a key or button, or {} to pick one from a \
             list — the list needs no controller plugged in.",
            icons::ADD,
            icons::MENU
        ));
        ui.add_space(6.0);
        ui.label(
            "4.  Bind it twice: once on the keyboard, once on a pad. Both trigger the same \
             action, so the script never asks which you're using.",
        );
        ui.add_space(6.0);
        ui.label("5.  Mash the control and watch the LIVE strip at the bottom light up.");
    });
    ui.add_space(8.0);
}

/// The armed press-to-bind prompt.
fn rebind_banner(ui: &mut egui::Ui, pending: Option<&PendingRebind>, edits: &mut InputEdits) {
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
                                "Press any key, mouse button or gamepad control for “{}”…",
                                p.action
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
    ui.label(egui::RichText::new(blurb).weak().small());
    ui.add_space(4.0);
}

/// The Lua call that reads an entry, and where scripts use it.
fn usage_hint(scan: &InputScan, name: &str, kind: UsageKind, call: &str) -> String {
    match scan.entries().find(|u| u.name == name && u.kind == kind) {
        Some(u) => format!("{call}\n\nused {} time(s) — first at {}:{}", u.count, u.file, u.line),
        None => format!("{call}\n\nNo script reads this yet."),
    }
}

fn action_row(
    ui: &mut egui::Ui,
    action: &floptle_input::Action,
    idx: usize,
    scan: &InputScan,
    test: &ActionState,
    players: u8,
    edits: &mut InputEdits,
) {
    let multiplayer = players > 1;
    // Every widget in this row is namespaced by the action name. Without it,
    // two rows' menus share an egui id and fight — which is exactly why the
    // pickers refused to stay open.
    ui.push_id(("action", &action.name), |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_H);

            // Live state, then the name.
            let held = test.is_held(idx);
            ui.label(
                egui::RichText::new(if held { icons::ON } else { icons::OFF })
                    .color(if held {
                        egui::Color32::LIGHT_GREEN
                    } else {
                        ui.visuals().weak_text_color()
                    }),
            )
            .on_hover_text("lights up while the action is triggered");

            let used = scan.entries().any(|u| u.name == action.name && u.kind == UsageKind::Action);
            let name = if used {
                egui::RichText::new(&action.name)
            } else {
                egui::RichText::new(&action.name).weak()
            };
            ui.add_sized([NAME_W - 20.0, 20.0], egui::Label::new(name).selectable(false))
                .on_hover_text(usage_hint(
                    scan,
                    &action.name,
                    UsageKind::Action,
                    &format!("input.action(\"{}\")", action.name),
                ));
            if !used {
                ui.label(egui::RichText::new(icons::UNUSED).weak())
                    .on_hover_text("no script reads this action");
            }
            // There is only ever ONE keyboard. In a local-multiplayer project an
            // unscoped key binding therefore fires this action for EVERY player at
            // once — both characters jump off one press. A pad binding has no such
            // problem (`Any` resolves per slot), so only flag the keyboard half.
            if multiplayer
                && action.bindings.iter().any(|b| {
                    b.player.is_none()
                        && matches!(b.source, Source::Key(_) | Source::Mouse(_))
                })
            {
                ui.colored_label(egui::Color32::from_rgb(224, 168, 64), icons::WARN)
                    .on_hover_text(
                        "a keyboard binding here is not scoped to a player, so it fires \
                         this action for BOTH local players at once. Right-click the chip \
                         to give it a player.",
                    );
            }

            binding_chips(ui, &action.bindings, &action.name, players, edits);
            bind_buttons(ui, &action.name, multiplayer, edits);
            if ui
                .small_button(icons::REMOVE)
                .on_hover_text("delete this action from the map")
                .clicked()
            {
                edits.commands.push(InputCmd::RemoveAction(action.name.clone()));
                edits.save = true;
            }
        });
        // Call out a device with nothing bound — the single most common way to
        // ship a control that works for you and not for someone on a pad.
        let has = |d: Device| action.bindings.iter().any(|b| b.source.device() == d);
        let kb = has(Device::Keyboard) || has(Device::Mouse);
        let pad = has(Device::Pad);
        if !action.bindings.is_empty() && (!kb || !pad) {
            ui.horizontal(|ui| {
                ui.add_space(NAME_W);
                let what = if kb { "no gamepad binding" } else { "no keyboard or mouse binding" };
                ui.label(egui::RichText::new(what).weak().small());
            });
        }
    });
}

fn axis2_row(
    ui: &mut egui::Ui,
    i: usize,
    ax: &floptle_input::Axis2,
    scan: &InputScan,
    test: &ActionState,
    edits: &mut InputEdits,
) {
    ui.push_id(("axis2", &ax.name), |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_H);
            let (x, y) = test.axis2(i);
            let live = x.abs() > 0.01 || y.abs() > 0.01;
            ui.label(
                egui::RichText::new(if live { icons::ON } else { icons::OFF }).color(if live {
                    egui::Color32::LIGHT_GREEN
                } else {
                    ui.visuals().weak_text_color()
                }),
            );
            ui.add_sized([NAME_W - 20.0, 20.0], egui::Label::new(&ax.name).selectable(false))
                .on_hover_text(usage_hint(
                    scan,
                    &ax.name,
                    UsageKind::Axis2,
                    &format!("local x, y = input.axis2(\"{}\")", ax.name),
                ));
            if ax.bindings.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(224, 168, 64),
                    format!("{} unbound", icons::WARN),
                );
            }
            for b in &ax.bindings {
                ui.label(chip_frame(ui, &axis2_chip(b)));
            }
            let mut socd = ax.socd;
            egui::ComboBox::from_id_salt("socd")
                .width(92.0)
                .selected_text(socd_label(socd))
                .show_ui(ui, |ui| {
                    for s in [Socd::Neutral, Socd::LastWins, Socd::Positive, Socd::Negative] {
                        ui.selectable_value(&mut socd, s, socd_label(s));
                    }
                });
            if socd != ax.socd {
                edits.commands.push(InputCmd::SetSocd { axis: i, socd });
                edits.save = true;
            }
        })
        .response
        .on_hover_text(
            "SOCD decides what happens when opposing directions are held at once.\n\
             Neutral cancels (the tournament standard); Last wins lets a player pivot \
             with no neutral frame.",
        );
    });
}

fn axis1_row(
    ui: &mut egui::Ui,
    i: usize,
    ax: &floptle_input::Axis1,
    scan: &InputScan,
    test: &ActionState,
    _edits: &mut InputEdits,
) {
    ui.push_id(("axis1", &ax.name), |ui| {
        ui.horizontal(|ui| {
            ui.set_min_height(ROW_H);
            let v = test.axis1(i);
            let live = v.abs() > 0.01;
            ui.label(
                egui::RichText::new(if live { icons::ON } else { icons::OFF }).color(if live {
                    egui::Color32::LIGHT_GREEN
                } else {
                    ui.visuals().weak_text_color()
                }),
            );
            ui.add_sized([NAME_W - 20.0, 20.0], egui::Label::new(&ax.name).selectable(false))
                .on_hover_text(usage_hint(
                    scan,
                    &ax.name,
                    UsageKind::Axis1,
                    &format!("input.axis1(\"{}\")", ax.name),
                ));
            if ax.bindings.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(224, 168, 64),
                    format!("{} unbound", icons::WARN),
                );
            }
            for b in &ax.bindings {
                ui.label(chip_frame(ui, &axis1_chip(b)));
            }
        });
    });
}

/// Chips read better as monospace — they're device labels, not prose.
fn chip_frame(ui: &egui::Ui, text: &str) -> egui::RichText {
    egui::RichText::new(text).monospace().background_color(ui.visuals().faint_bg_color)
}

fn binding_chips(
    ui: &mut egui::Ui,
    bindings: &[floptle_input::Binding],
    action: &str,
    players: u8,
    edits: &mut InputEdits,
) {
    if bindings.is_empty() {
        ui.colored_label(
            egui::Color32::from_rgb(224, 168, 64),
            format!("{} unbound", icons::WARN),
        );
        return;
    }
    for (i, b) in bindings.iter().enumerate() {
        // Namespaced per index: two identical chips on one row would otherwise
        // share an id and the wrong one would answer the click.
        let resp = ui.push_id(i, |ui| {
            let label = match b.player {
                Some(p) => format!("{}  P{}", b.chip(), p + 1),
                None => b.chip(),
            };
            let resp = ui.button(chip_frame(ui, &label)).on_hover_text(
                "click to remove this binding · right-click to scope it to one player",
            );
            // Which player a binding belongs to only exists as a question with more
            // than one of them — and it's the answer to "why does P2's key punch for
            // both fighters", since there is only ever one keyboard.
            if players > 1 {
                resp.context_menu(|ui| {
                    let mut set = |ui: &mut egui::Ui, label: &str, player: Option<u8>| {
                        if ui.radio(b.player == player, label).clicked() {
                            edits.commands.push(InputCmd::SetBindingPlayer {
                                action: action.into(),
                                index: i,
                                player,
                            });
                            edits.save = true;
                            ui.close();
                        }
                    };
                    set(ui, "every player", None);
                    for p in 0..players {
                        set(ui, &format!("player {}", p + 1), Some(p));
                    }
                });
            }
            resp
        });
        if resp.inner.clicked() {
            edits.commands.push(InputCmd::RemoveBinding { action: action.into(), index: i });
            edits.save = true;
        }
    }
}

fn bind_buttons(ui: &mut egui::Ui, action: &str, multiplayer: bool, edits: &mut InputEdits) {
    if ui
        .small_button(icons::ADD)
        .on_hover_text("bind by PRESSING an input — key, mouse or gamepad")
        .clicked()
    {
        edits.commands.push(InputCmd::StartRebind {
            action: action.into(),
            filter: BindFilter::AnyButton,
        });
    }
    if let Some(src) = source_picker(ui, multiplayer) {
        edits.commands.push(InputCmd::AddBinding { action: action.into(), source: src });
        edits.save = true;
    }
}

fn add_action_row(
    ui: &mut egui::Ui,
    map: &InputMap,
    new_action: &mut String,
    edits: &mut InputEdits,
) {
    let full = map.actions.len() >= floptle_input::MAX_ACTIONS;
    ui.horizontal(|ui| {
        ui.add_space(NAME_W);
        let resp = ui.add_sized(
            [160.0, 20.0],
            egui::TextEdit::singleline(new_action).hint_text("new action…"),
        );
        let commit = (resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
            || ui.small_button(icons::ADD).clicked();
        if commit && !new_action.trim().is_empty() {
            let name = new_action.trim().to_string();
            if !full && map.action_index(&name).is_none() {
                edits.commands.push(InputCmd::AddAction(name));
                edits.save = true;
            }
            new_action.clear();
        }
        if full {
            ui.colored_label(
                egui::Color32::from_rgb(224, 168, 64),
                format!("{}-action max", floptle_input::MAX_ACTIONS),
            )
            .on_hover_text("multiplayer packs actions into a 64-bit mask");
        }
    });
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
                ui.add_sized([NAME_W - 20.0, 20.0], egui::Label::new(&u.name).selectable(false))
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
                    ui.label(egui::RichText::new(format!("\"{}\"", u.name)).monospace())
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
                ui.label(egui::RichText::new(p).monospace());
            }
        }
    });
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (i, a) in map.actions.iter().enumerate() {
            let on = test.is_held(i);
            ui.label(
                egui::RichText::new(format!(
                    "{} {}",
                    if on { icons::ON } else { icons::OFF },
                    a.name
                ))
                .color(if on {
                    egui::Color32::LIGHT_GREEN
                } else {
                    ui.visuals().weak_text_color()
                }),
            );
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (i, ax) in map.axes2.iter().enumerate() {
            let (x, y) = test.axis2(i);
            ui.label(
                egui::RichText::new(format!("{}: ({x:+.2}, {y:+.2})", ax.name)).monospace(),
            );
        }
        for (i, ax) in map.axes1.iter().enumerate() {
            ui.label(
                egui::RichText::new(format!("{}: {:+.2}", ax.name, test.axis1(i))).monospace(),
            );
        }
    });
}

/// A menu that picks any bindable source from a list — **no hardware required**.
///
/// Press-to-bind is the fast path when the device is in your hand; this is the
/// one that always works. Gamepad comes first and is never greyed out: laying
/// out pad controls with nothing plugged in is entirely normal.
fn source_picker(ui: &mut egui::Ui, multiplayer: bool) -> Option<Source> {
    use floptle_input::{KeyGroup, MouseAxis, MouseButton, PadAxis, PadButton, PadControl};

    let mut picked = None;
    ui.menu_button(icons::MENU, |ui| {
        ui.set_min_width(190.0);
        ui.label(egui::RichText::new("Add a binding").strong());
        ui.separator();
        ui.menu_button(format!("{}  Gamepad", icons::PAD), |ui| {
            if multiplayer {
                ui.label(
                    egui::RichText::new("binds to this player's own pad").weak().small(),
                );
                ui.separator();
            }
            for &b in PadButton::ALL {
                if ui.button(b.label()).clicked() {
                    picked = Some(Source::Pad { id: PadId::Any, ctrl: PadControl::Button(b) });
                    ui.close();
                }
            }
            ui.separator();
            ui.label(egui::RichText::new("sticks & triggers").weak().small());
            for &a in PadAxis::ALL {
                if ui.button(a.label()).clicked() {
                    picked = Some(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(a) });
                    ui.close();
                }
            }
        });
        ui.menu_button(format!("{}  Keyboard", icons::KEYBOARD), |ui| {
            for &g in KeyGroup::ALL {
                ui.menu_button(g.label(), |ui| {
                    egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for k in g.keys() {
                            if ui.button(k.label()).clicked() {
                                picked = Some(Source::Key(k));
                                ui.close();
                            }
                        }
                    });
                });
            }
        });
        ui.menu_button(format!("{}  Mouse", icons::MOUSE), |ui| {
            for &b in MouseButton::ALL {
                if ui.button(b.label()).clicked() {
                    picked = Some(Source::Mouse(b));
                    ui.close();
                }
            }
            ui.separator();
            for &a in MouseAxis::ALL {
                if ui.button(a.label()).clicked() {
                    picked = Some(Source::MouseAxis(a));
                    ui.close();
                }
            }
        });
    })
    .response
    .on_hover_text("pick a binding from a list — works with nothing plugged in");
    picked
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

fn socd_label(s: Socd) -> &'static str {
    match s {
        Socd::Neutral => "Neutral",
        Socd::LastWins => "Last wins",
        Socd::Positive => "Up/right",
        Socd::Negative => "Dn/left",
    }
}

/// `"  P2"` for a binding scoped to one local player, empty for the usual case.
fn player_suffix(player: Option<u8>) -> String {
    player.map(|p| format!("  P{}", p + 1)).unwrap_or_default()
}

/// A compact one-chip summary of a 2D axis binding.
fn axis2_chip(b: &Axis2Binding) -> String {
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

fn axis1_chip(b: &Axis1Binding) -> String {
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
    use floptle_input::{Key, PadAxis};

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
