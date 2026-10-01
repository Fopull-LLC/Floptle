//! The Input settings' editor: every entry of the action map, and every field
//! of every binding, editable in place.
//!
//! Two panes. On the left, the whole map as one list — actions, directions,
//! amounts and motions — each row lit while it is triggered, so pressing a
//! control shows where it lands. On the right, the selected entry, laid out as
//! cards: one per binding, each with every setting that binding has. Nothing
//! is reached through a right-click and nothing is only in `input.ron`.
//!
//! What it does differently from the usual action-map editor:
//!
//! - **A binding is changed, not deleted and re-added.** Every source is a
//!   button: click it to press a new input or pick one from a list. Removing is
//!   its own ✕, never what a click on the binding does.
//! - **A preset is a starting point.** "WASD", "Left stick" and the rest add a
//!   binding already filled in, which is then edited like any other — so the
//!   standard controls a project starts with are as editable as its own.
//! - **The settings sit on the binding they change.** Deadzone, curve and
//!   sensitivity are a stick's; a mouse's are its sensitivity and the button
//!   held to look; a key has a chord. Each card shows its own and nothing else.
//! - **It shows what the entry is doing now.** A 2D entry draws its live
//!   vector, a 1D one its bar, an action its light — the tester is beside the
//!   thing being edited, not at the bottom of the page.
//!
//! Edits are made to a copy of the map and handed back whole when anything
//! changed (`InputCmd::SetMap`). Press-to-bind is the one thing that cannot
//! happen inside a frame of UI, so it is a [`CaptureTarget`] the editor fills
//! when the press arrives.

use floptle_input::{
    Action, ActionState, Axis1, Axis1Binding, Axis2, Axis2Binding, BindFilter, Binding, Curve,
    InputMap, Motion, PadAxis, PadButton, PadControl, PadId, Socd, Source,
};

use crate::icons;
use crate::input_scan::{InputScan, UsageKind};
use crate::input_ui::{InputCmd, InputEdits};

/// Which entry of the map is open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Sel {
    Action(String),
    Axis2(String),
    Axis1(String),
    Motion(String),
}

impl Sel {
    fn name(&self) -> &str {
        match self {
            Sel::Action(n) | Sel::Axis2(n) | Sel::Axis1(n) | Sel::Motion(n) => n,
        }
    }

    fn renamed(&self, to: String) -> Sel {
        match self {
            Sel::Action(_) => Sel::Action(to),
            Sel::Axis2(_) => Sel::Axis2(to),
            Sel::Axis1(_) => Sel::Axis1(to),
            Sel::Motion(_) => Sel::Motion(to),
        }
    }

    fn kind(&self) -> UsageKind {
        match self {
            Sel::Action(_) => UsageKind::Action,
            Sel::Axis2(_) => UsageKind::Axis2,
            Sel::Axis1(_) => UsageKind::Axis1,
            Sel::Motion(_) => UsageKind::Motion,
        }
    }

    /// The Lua that reads it.
    fn call(&self) -> String {
        match self {
            Sel::Action(n) => format!("input.action(\"{n}\")"),
            Sel::Axis2(n) => format!("local x, y = input.axis2(\"{n}\")"),
            Sel::Axis1(n) => format!("input.axis1(\"{n}\")"),
            Sel::Motion(n) => format!("input.motion(\"{n}\")"),
        }
    }
}

/// What the editor remembers between frames.
#[derive(Default)]
pub(crate) struct InputUiState {
    pub(crate) selected: Option<Sel>,
    /// The name field while it is being typed in, for the entry it belongs to.
    name_buf: Option<(Sel, String)>,
}

/// Where a pressed input goes once press-to-bind catches it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CaptureTarget {
    /// A new binding on an action.
    AddToAction(String),
    /// Replace one binding's input, keeping its other settings.
    ReplaceInAction { action: String, index: usize },
    /// One key of a four-key direction: 0 up, 1 down, 2 left, 3 right.
    Axis2Key { axis: String, index: usize, dir: usize },
    /// One key of a key pair: the minus or the plus side.
    Axis1Key { axis: String, index: usize, plus: bool },
    /// The analog input of a 1D binding.
    Axis1Source { axis: String, index: usize },
    /// A button that must be held for a mouse look (or analog amount) to count.
    Gate2 { axis: String, index: usize },
    Gate1 { axis: String, index: usize },
}

impl CaptureTarget {
    /// What kind of press it waits for.
    pub(crate) fn filter(&self) -> BindFilter {
        match self {
            CaptureTarget::Axis1Source { .. } => BindFilter::AxisOnly,
            _ => BindFilter::AnyButton,
        }
    }

    /// What the banner says it is waiting for.
    pub(crate) fn describe(&self) -> String {
        const DIRS: [&str; 4] = ["up", "down", "left", "right"];
        match self {
            CaptureTarget::AddToAction(a) => format!("“{a}”"),
            CaptureTarget::ReplaceInAction { action, .. } => format!("“{action}” (replacing a binding)"),
            CaptureTarget::Axis2Key { axis, dir, .. } => format!("“{axis}” {}", DIRS[(*dir).min(3)]),
            CaptureTarget::Axis1Key { axis, plus, .. } => {
                format!("“{axis}” {}", if *plus { "plus" } else { "minus" })
            }
            CaptureTarget::Axis1Source { axis, .. } => format!("“{axis}” (move a stick, trigger or the wheel)"),
            CaptureTarget::Gate2 { axis, .. } | CaptureTarget::Gate1 { axis, .. } => {
                format!("the button held for “{axis}”")
            }
        }
    }

    /// Put a captured input where this target says. `true` when the map changed.
    pub(crate) fn apply(&self, map: &mut InputMap, source: Source, modifiers: Vec<Source>) -> bool {
        match self {
            CaptureTarget::AddToAction(a) => {
                let Some(action) = map.actions.iter_mut().find(|x| &x.name == a) else { return false };
                let b = Binding::with_modifiers(source, modifiers);
                if action.bindings.iter().any(|x| x.same_source(&b)) {
                    return false;
                }
                action.bindings.push(b);
                true
            }
            CaptureTarget::ReplaceInAction { action, index } => {
                let Some(b) = map.actions.iter_mut().find(|x| &x.name == action).and_then(|a| a.bindings.get_mut(*index))
                else {
                    return false;
                };
                b.source = source;
                b.modifiers = modifiers;
                true
            }
            CaptureTarget::Axis2Key { axis, index, dir } => {
                match map.axes2.iter_mut().find(|x| &x.name == axis).and_then(|a| a.bindings.get_mut(*index)) {
                    Some(Axis2Binding::Keys { up, down, left, right, .. }) => {
                        match dir {
                            0 => *up = source,
                            1 => *down = source,
                            2 => *left = source,
                            _ => *right = source,
                        }
                        true
                    }
                    _ => false,
                }
            }
            CaptureTarget::Axis1Key { axis, index, plus: p } => {
                match map.axes1.iter_mut().find(|x| &x.name == axis).and_then(|a| a.bindings.get_mut(*index)) {
                    Some(Axis1Binding::Keys { minus, plus, .. }) => {
                        if *p {
                            *plus = source;
                        } else {
                            *minus = source;
                        }
                        true
                    }
                    _ => false,
                }
            }
            CaptureTarget::Axis1Source { axis, index } => {
                match map.axes1.iter_mut().find(|x| &x.name == axis).and_then(|a| a.bindings.get_mut(*index)) {
                    Some(Axis1Binding::Analog { source: s, .. }) => {
                        *s = source;
                        true
                    }
                    _ => false,
                }
            }
            CaptureTarget::Gate2 { axis, index } => {
                match map.axes2.iter_mut().find(|x| &x.name == axis).and_then(|a| a.bindings.get_mut(*index)) {
                    Some(Axis2Binding::Mouse { gate, .. }) if !gate.contains(&source) => {
                        gate.push(source);
                        true
                    }
                    _ => false,
                }
            }
            CaptureTarget::Gate1 { axis, index } => {
                match map.axes1.iter_mut().find(|x| &x.name == axis).and_then(|a| a.bindings.get_mut(*index)) {
                    Some(Axis1Binding::Analog { gate, .. }) if !gate.contains(&source) => {
                        gate.push(source);
                        true
                    }
                    _ => false,
                }
            }
        }
    }
}

const WARN: egui::Color32 = egui::Color32::from_rgb(224, 168, 64);
const LIVE: egui::Color32 = egui::Color32::LIGHT_GREEN;
/// Below this the editor stacks its two panes rather than splitting.
const SPLIT_MIN: f32 = 600.0;
const LIST_W: f32 = 210.0;

/// The list and the editor. Edits land in `edits`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn editor(
    ui: &mut egui::Ui,
    map: &InputMap,
    state: &mut InputUiState,
    capture: Option<&CaptureTarget>,
    scan: &InputScan,
    test: &ActionState,
    query: &str,
    edits: &mut InputEdits,
) {
    // Keep the selection on something that exists: the first entry, after a
    // delete or on first open.
    if state.selected.as_ref().is_none_or(|s| !exists(map, s)) {
        state.selected = first(map);
    }
    let mut draft = map.clone();
    let wide = ui.available_width() >= SPLIT_MIN;
    if wide {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(LIST_W, 0.0),
                egui::Layout::top_down(egui::Align::Min),
                |ui| list(ui, &mut draft, state, test, query),
            );
            ui.separator();
            ui.vertical(|ui| detail(ui, &mut draft, state, capture, scan, test, edits));
        });
    } else {
        list(ui, &mut draft, state, test, query);
        ui.separator();
        detail(ui, &mut draft, state, capture, scan, test, edits);
    }
    if draft != *map {
        edits.commands.push(InputCmd::SetMap(Box::new(draft)));
        edits.save = true;
    }
}

/// Every binding of an entry, one per line — the list's hover.
fn summary(map: &InputMap, s: &Sel) -> String {
    let lines: Vec<String> = match s {
        Sel::Action(n) => map.actions.iter().filter(|a| &a.name == n).flat_map(|a| a.bindings.iter().map(|b| b.chip())).collect(),
        Sel::Axis2(n) => map.axes2.iter().filter(|a| &a.name == n).flat_map(|a| a.bindings.iter().map(crate::input_ui::axis2_chip)).collect(),
        Sel::Axis1(n) => map.axes1.iter().filter(|a| &a.name == n).flat_map(|a| a.bindings.iter().map(crate::input_ui::axis1_chip)).collect(),
        Sel::Motion(n) => map.motions.iter().filter(|m| &m.name == n).map(|m| m.dirs.iter().map(|d| d.to_string()).collect::<Vec<_>>().join(" ")).collect(),
    };
    lines.join("\n")
}

fn exists(map: &InputMap, s: &Sel) -> bool {
    match s {
        Sel::Action(n) => map.action_index(n).is_some(),
        Sel::Axis2(n) => map.axis2_index(n).is_some(),
        Sel::Axis1(n) => map.axis1_index(n).is_some(),
        Sel::Motion(n) => map.motion(n).is_some(),
    }
}

fn first(map: &InputMap) -> Option<Sel> {
    map.actions
        .first()
        .map(|a| Sel::Action(a.name.clone()))
        .or_else(|| map.axes2.first().map(|a| Sel::Axis2(a.name.clone())))
        .or_else(|| map.axes1.first().map(|a| Sel::Axis1(a.name.clone())))
        .or_else(|| map.motions.first().map(|m| Sel::Motion(m.name.clone())))
}

/// A name no entry uses yet: `base`, else `base 2`, `base 3`…
fn fresh_name(map: &InputMap, base: &str) -> String {
    let taken = |n: &str| {
        map.action_index(n).is_some() || map.axis2_index(n).is_some() || map.axis1_index(n).is_some() || map.motion(n).is_some()
    };
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base}{i}")).find(|n| !taken(n)).unwrap_or_default()
}

// ---- the list ---------------------------------------------------------------

fn list(ui: &mut egui::Ui, map: &mut InputMap, state: &mut InputUiState, test: &ActionState, query: &str) {
    ui.horizontal(|ui| {
        ui.menu_button(format!("{} New", icons::ADD), |ui| {
            let mut add = |ui: &mut egui::Ui, label: &str, hint: &str, make: &dyn Fn(&mut InputMap) -> Sel| {
                if ui.button(label).on_hover_text(hint).clicked() {
                    state.selected = Some(make(map));
                    ui.close();
                }
            };
            add(ui, "Action", "a button: pressed, held, released — Jump, Fire, Pause", &|m| {
                let n = fresh_name(m, "NewAction");
                m.actions.push(Action::new(n.clone()));
                Sel::Action(n)
            });
            add(ui, "Direction (2D)", "two numbers at once: a stick, WASD, mouse look", &|m| {
                let n = fresh_name(m, "NewDirection");
                m.axes2.push(Axis2 { name: n.clone(), socd: Socd::Neutral, bindings: Vec::new() });
                Sel::Axis2(n)
            });
            add(ui, "Amount (1D)", "one number: a trigger, the wheel, a key pair", &|m| {
                let n = fresh_name(m, "NewAmount");
                m.axes1.push(Axis1 { name: n.clone(), socd: Socd::Neutral, bindings: Vec::new() });
                Sel::Axis1(n)
            });
            add(ui, "Motion", "a direction sequence, like a fighting game's quarter-circle", &|m| {
                let n = fresh_name(m, "newMotion");
                m.motions.push(Motion { name: n.clone(), dirs: vec![2, 3, 6], window: 12, charge: 0 });
                Sel::Motion(n)
            });
        });
    });
    ui.add_space(4.0);
    let show = |name: &str| crate::settings_ui::matches(query, name);
    let mut row = |ui: &mut egui::Ui, sel: Sel, live: bool, empty: bool| {
        let on = state.selected.as_ref() == Some(&sel);
        let dot = egui::RichText::new(if live { icons::ON } else { icons::OFF })
            .color(if live { LIVE } else { ui.visuals().weak_text_color() });
        ui.horizontal(|ui| {
            ui.label(dot);
            let w = crate::responsive::usable_width(ui);
            let shown = crate::responsive::elide(ui, sel.name(), (w - 12.0).max(8.0));
            let mut text = egui::RichText::new(shown);
            if empty {
                text = text.color(WARN);
            }
            let r = ui
                .with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| ui.selectable_label(on, text))
                .inner;
            let r = if empty { r.on_hover_text("nothing is bound — this does nothing yet") } else { r.on_hover_text(summary(map, &sel)) };
            if r.clicked() {
                state.selected = Some(sel.clone());
            }
        });
    };
    let section = |ui: &mut egui::Ui, title: &str, n: usize| {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(title).strong());
            ui.label(egui::RichText::new(n.to_string()).small().weak());
        });
    };
    let actions: Vec<(usize, String, bool)> =
        map.actions.iter().enumerate().filter(|(_, a)| show(&a.name)).map(|(i, a)| (i, a.name.clone(), a.bindings.is_empty())).collect();
    section(ui, "Actions", actions.len());
    for (i, n, empty) in actions {
        row(ui, Sel::Action(n), test.is_held(i), empty);
    }
    let axes2: Vec<(usize, String, bool)> =
        map.axes2.iter().enumerate().filter(|(_, a)| show(&a.name)).map(|(i, a)| (i, a.name.clone(), a.bindings.is_empty())).collect();
    section(ui, "Directions", axes2.len());
    for (i, n, empty) in axes2 {
        let (x, y) = test.axis2(i);
        row(ui, Sel::Axis2(n), x.abs() > 0.01 || y.abs() > 0.01, empty);
    }
    let axes1: Vec<(usize, String, bool)> =
        map.axes1.iter().enumerate().filter(|(_, a)| show(&a.name)).map(|(i, a)| (i, a.name.clone(), a.bindings.is_empty())).collect();
    section(ui, "Amounts", axes1.len());
    for (i, n, empty) in axes1 {
        row(ui, Sel::Axis1(n), test.axis1(i).abs() > 0.01, empty);
    }
    let motions: Vec<String> = map.motions.iter().filter(|m| show(&m.name)).map(|m| m.name.clone()).collect();
    section(ui, "Motions", motions.len());
    for n in motions {
        row(ui, Sel::Motion(n), false, false);
    }
}

// ---- the selected entry -----------------------------------------------------

fn detail(
    ui: &mut egui::Ui,
    map: &mut InputMap,
    state: &mut InputUiState,
    capture: Option<&CaptureTarget>,
    scan: &InputScan,
    test: &ActionState,
    edits: &mut InputEdits,
) {
    let Some(sel) = state.selected.clone() else {
        ui.label(egui::RichText::new("Nothing here yet — ✚ New adds an action.").weak());
        return;
    };
    ui.push_id(("input_detail", sel.name().to_string()), |ui| {
        header(ui, map, state, &sel, scan);
        let Some(sel) = state.selected.clone() else { return };
        ui.add_space(6.0);
        let multiplayer = map.players > 1;
        let players = map.players;
        match &sel {
            Sel::Action(n) => {
                let idx = map.action_index(n).unwrap_or(0);
                let live = test.is_held(idx);
                if let Some(a) = map.actions.iter_mut().find(|a| &a.name == n) {
                    action_editor(ui, a, live, players, multiplayer, capture, edits);
                }
            }
            Sel::Axis2(n) => {
                let live = map.axis2_index(n).map(|i| test.axis2(i)).unwrap_or((0.0, 0.0));
                if let Some(a) = map.axes2.iter_mut().find(|a| &a.name == n) {
                    axis2_editor(ui, a, live, players, multiplayer, capture, edits);
                }
            }
            Sel::Axis1(n) => {
                let live = map.axis1_index(n).map(|i| test.axis1(i)).unwrap_or(0.0);
                if let Some(a) = map.axes1.iter_mut().find(|a| &a.name == n) {
                    axis1_editor(ui, a, live, players, multiplayer, capture, edits);
                }
            }
            Sel::Motion(n) => {
                if let Some(m) = map.motions.iter_mut().find(|m| &m.name == n) {
                    motion_editor(ui, m);
                }
            }
        }
    });
}

/// The name, the Lua that reads it, and the entry's own actions.
fn header(ui: &mut egui::Ui, map: &mut InputMap, state: &mut InputUiState, sel: &Sel, scan: &InputScan) {
    // The name, editable. Committed when the field lets go, so a half-typed
    // name never renames anything.
    let mut buf = match &state.name_buf {
        Some((s, b)) if s == sel => b.clone(),
        _ => sel.name().to_string(),
    };
    ui.horizontal_wrapped(|ui| {
        let w = crate::responsive::usable_width(ui).clamp(60.0, 220.0);
        let r = ui.add(egui::TextEdit::singleline(&mut buf).font(egui::TextStyle::Heading).desired_width(w));
        if r.changed() {
            state.name_buf = Some((sel.clone(), buf.clone()));
        }
        let typed = buf.trim().to_string();
        let clash = typed != sel.name() && !typed.is_empty() && fresh_name(map, &typed) != typed;
        if clash {
            ui.colored_label(WARN, format!("{} another entry is called that", icons::WARN));
        }
        if r.lost_focus() {
            state.name_buf = None;
            if !typed.is_empty() && typed != sel.name() && !clash {
                rename(map, sel, &typed);
                state.selected = Some(sel.renamed(typed));
            }
        }
    });
    let uses = scan.entries().find(|u| u.name == sel.name() && u.kind == sel.kind());
    let pending_rename = state.name_buf.as_ref().is_some_and(|(s, b)| s == sel && b.trim() != sel.name());
    crate::responsive::para(ui, egui::RichText::new(sel.call()).monospace().weak());
    crate::responsive::para(
        ui,
        egui::RichText::new(match uses {
            Some(u) => format!("read {}× — first at {}:{}", u.count, u.file, u.line),
            None => "no script reads this yet".to_string(),
        })
        .weak()
        .small(),
    );
    if pending_rename && let Some(u) = uses {
        ui.colored_label(
            WARN,
            format!(
                "{} {} script call(s) read it as “{}” — rename them too, or they read nothing",
                icons::WARN,
                u.count,
                sel.name()
            ),
        );
    }
    ui.horizontal_wrapped(|ui| {
        if ui.small_button("⬆").on_hover_text("move up the list").clicked() {
            shift(map, sel, -1);
        }
        if ui.small_button("⬇").on_hover_text("move down the list").clicked() {
            shift(map, sel, 1);
        }
        crate::input_ui::wrap_before(ui, 70.0);
        if ui.small_button("Duplicate").on_hover_text("a copy with every binding, to start a variant from").clicked() {
            state.selected = duplicate(map, sel);
        }
        crate::input_ui::wrap_before(ui, 64.0);
        if ui.small_button(format!("{} Delete", icons::REMOVE)).on_hover_text("remove this entry from the map").clicked() {
            delete(map, sel);
            state.selected = None;
        }
    });
}

fn rename(map: &mut InputMap, sel: &Sel, to: &str) {
    match sel {
        Sel::Action(n) => map.actions.iter_mut().filter(|a| &a.name == n).for_each(|a| a.name = to.into()),
        Sel::Axis2(n) => {
            map.axes2.iter_mut().filter(|a| &a.name == n).for_each(|a| a.name = to.into());
            // The motion inputs follow the direction they read.
            if map.motion_axis.as_deref() == Some(n.as_str()) {
                map.motion_axis = Some(to.into());
            }
        }
        Sel::Axis1(n) => map.axes1.iter_mut().filter(|a| &a.name == n).for_each(|a| a.name = to.into()),
        Sel::Motion(n) => map.motions.iter_mut().filter(|m| &m.name == n).for_each(|m| m.name = to.into()),
    }
}

fn shift(map: &mut InputMap, sel: &Sel, by: isize) {
    fn mv<T>(v: &mut [T], i: Option<usize>, by: isize) {
        let Some(i) = i else { return };
        let j = i as isize + by;
        if j >= 0 && (j as usize) < v.len() {
            v.swap(i, j as usize);
        }
    }
    match sel {
        Sel::Action(n) => {
            let i = map.actions.iter().position(|a| &a.name == n);
            mv(&mut map.actions, i, by)
        }
        Sel::Axis2(n) => {
            let i = map.axes2.iter().position(|a| &a.name == n);
            mv(&mut map.axes2, i, by)
        }
        Sel::Axis1(n) => {
            let i = map.axes1.iter().position(|a| &a.name == n);
            mv(&mut map.axes1, i, by)
        }
        Sel::Motion(n) => {
            let i = map.motions.iter().position(|m| &m.name == n);
            mv(&mut map.motions, i, by)
        }
    }
}

fn duplicate(map: &mut InputMap, sel: &Sel) -> Option<Sel> {
    let to = fresh_name(map, &format!("{}Copy", sel.name()));
    match sel {
        Sel::Action(n) => {
            if map.actions.len() >= floptle_input::MAX_ACTIONS {
                return Some(sel.clone());
            }
            let mut a = map.actions.iter().find(|a| &a.name == n)?.clone();
            a.name = to.clone();
            map.actions.push(a);
        }
        Sel::Axis2(n) => {
            let mut a = map.axes2.iter().find(|a| &a.name == n)?.clone();
            a.name = to.clone();
            map.axes2.push(a);
        }
        Sel::Axis1(n) => {
            let mut a = map.axes1.iter().find(|a| &a.name == n)?.clone();
            a.name = to.clone();
            map.axes1.push(a);
        }
        Sel::Motion(n) => {
            let mut m = map.motions.iter().find(|m| &m.name == n)?.clone();
            m.name = to.clone();
            map.motions.push(m);
        }
    }
    Some(sel.renamed(to))
}

fn delete(map: &mut InputMap, sel: &Sel) {
    match sel {
        Sel::Action(n) => map.actions.retain(|a| &a.name != n),
        Sel::Axis2(n) => map.axes2.retain(|a| &a.name != n),
        Sel::Axis1(n) => map.axes1.retain(|a| &a.name != n),
        Sel::Motion(n) => map.motions.retain(|m| &m.name != n),
    }
}

// ---- shared pieces ---------------------------------------------------------

/// A card: one binding, framed, with its ✕ and ⬆⬇ at the right.
/// Returns what the card's own row of buttons asked for.
#[derive(PartialEq)]
enum CardOp {
    None,
    Remove,
    Up,
    Down,
}

fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) -> CardOp {
    let mut op = CardOp::None;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        // The frame's own margin and stroke sit outside what is set here.
        ui.set_width((crate::responsive::usable_width(ui) - 14.0).max(0.0));
        ui.horizontal_wrapped(|ui| {
            let w = (crate::responsive::usable_width(ui) - 84.0).max(40.0);
            let shown = crate::responsive::elide(ui, title, w);
            ui.label(egui::RichText::new(shown).strong());
            for (label, tip, what) in [("⬆", "move up", CardOp::Up), ("⬇", "move down", CardOp::Down), (icons::REMOVE, "remove this binding", CardOp::Remove)] {
                crate::input_ui::wrap_before(ui, 24.0);
                if ui.small_button(label).on_hover_text(tip).clicked() {
                    op = what;
                }
            }
        });
        body(ui);
    });
    op
}

fn apply_card_op<T>(v: &mut Vec<T>, i: usize, op: CardOp) {
    match op {
        CardOp::Remove => {
            v.remove(i);
        }
        CardOp::Up if i > 0 => v.swap(i, i - 1),
        CardOp::Down if i + 1 < v.len() => v.swap(i, i + 1),
        _ => {}
    }
}

/// A labelled row inside a card.
fn field(ui: &mut egui::Ui, label: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.add_sized([96.0, 18.0], egui::Label::new(egui::RichText::new(label).weak()));
        body(ui);
    });
}

/// What a source button asked for.
enum SourceAsk {
    None,
    Press,
    Picked(Source),
}

/// An input, as a button: click to press a new one or pick it from a list.
/// `waiting` lights it while a press is armed for this very slot.
fn source_button(ui: &mut egui::Ui, text: &str, waiting: bool, analog: bool, multiplayer: bool) -> SourceAsk {
    let mut ask = SourceAsk::None;
    let label = if waiting { "press…".to_string() } else { text.to_string() };
    let rich = if waiting { egui::RichText::new(label).color(LIVE).strong() } else { egui::RichText::new(label).monospace() };
    ui.menu_button(rich, |ui| {
        ui.set_min_width(200.0);
        if ui.button(format!("{}  Press an input…", icons::ADD)).clicked() {
            ask = SourceAsk::Press;
            ui.close();
        }
        ui.separator();
        if let Some(s) = pick_menus(ui, analog, multiplayer) {
            ask = SourceAsk::Picked(s);
            ui.close();
        }
    })
    .response
    .on_hover_text("click to change: press a new input, or pick one from the list");
    ask
}

/// The device menus of the source picker. `analog` lists only sticks,
/// triggers and mouse axes; otherwise only buttons and keys.
pub(crate) fn pick_menus(ui: &mut egui::Ui, analog: bool, multiplayer: bool) -> Option<Source> {
    use floptle_input::{KeyGroup, MouseAxis, MouseButton};
    let mut picked = None;
    ui.menu_button(format!("{}  Gamepad", icons::PAD), |ui| {
        if multiplayer {
            ui.label(egui::RichText::new("binds to this player's own pad").weak().small());
            ui.separator();
        }
        if analog {
            for &a in PadAxis::ALL {
                if ui.button(a.label()).clicked() {
                    picked = Some(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(a) });
                    ui.close();
                }
            }
        } else {
            for &b in PadButton::ALL {
                if ui.button(b.label()).clicked() {
                    picked = Some(Source::Pad { id: PadId::Any, ctrl: PadControl::Button(b) });
                    ui.close();
                }
            }
        }
    });
    if !analog {
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
    }
    ui.menu_button(format!("{}  Mouse", icons::MOUSE), |ui| {
        if analog {
            for &a in MouseAxis::ALL {
                if ui.button(a.label()).clicked() {
                    picked = Some(Source::MouseAxis(a));
                    ui.close();
                }
            }
        } else {
            for &b in MouseButton::ALL {
                if ui.button(b.label()).clicked() {
                    picked = Some(Source::Mouse(b));
                    ui.close();
                }
            }
        }
    });
    picked
}

fn player_combo(ui: &mut egui::Ui, player: &mut Option<u8>, players: u8) {
    if players <= 1 {
        return;
    }
    field(ui, "player", |ui| {
        egui::ComboBox::from_id_salt("player")
            .selected_text(match player {
                None => "every player".to_string(),
                Some(p) => format!("player {}", *p + 1),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(player, None, "every player");
                for p in 0..players {
                    ui.selectable_value(player, Some(p), format!("player {}", p + 1));
                }
            });
    });
}

fn socd_row(ui: &mut egui::Ui, socd: &mut Socd) {
    field(ui, "opposites held", |ui| {
        egui::ComboBox::from_id_salt("socd").selected_text(crate::input_ui::socd_label(*socd)).show_ui(ui, |ui| {
            for s in [Socd::Neutral, Socd::LastWins, Socd::Positive, Socd::Negative] {
                ui.selectable_value(socd, s, crate::input_ui::socd_label(s));
            }
        })
        .response
        .on_hover_text(
            "what happens when opposite directions are held at once. Cancel is the \
             tournament standard; Last wins lets a player pivot with no neutral frame.",
        );
    });
}


fn analog_fields(ui: &mut egui::Ui, deadzone: &mut f32, sensitivity: &mut f32, curve: &mut Curve) {
    field(ui, "deadzone", |ui| {
        ui.add(egui::Slider::new(deadzone, 0.0..=0.9).fixed_decimals(2))
            .on_hover_text("how far it must move before it counts — raise it for a stick that drifts");
    });
    field(ui, "sensitivity", |ui| {
        ui.add(egui::DragValue::new(sensitivity).speed(0.01).range(0.0..=20.0)).on_hover_text("a multiplier on the value");
    });
    field(ui, "response", |ui| {
        egui::ComboBox::from_id_salt("curve")
            .selected_text(match curve {
                Curve::Linear => "linear",
                Curve::Expo => "fine near centre",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(curve, Curve::Linear, "linear");
                ui.selectable_value(curve, Curve::Expo, "fine near centre");
            });
    });
}

/// A list of held-button chips with ✕ each, plus ✚ to press or pick another.
fn source_list(
    ui: &mut egui::Ui,
    sources: &mut Vec<Source>,
    waiting: bool,
    multiplayer: bool,
    on_press: impl FnOnce(),
) {
    ui.horizontal_wrapped(|ui| {
        let mut remove = None;
        for (i, s) in sources.iter().enumerate() {
            if ui.small_button(format!("{} ×", s.chip())).on_hover_text("click to remove").clicked() {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            sources.remove(i);
        }
        match source_button(ui, &format!("{} add", icons::ADD), waiting, false, multiplayer) {
            SourceAsk::Press => on_press(),
            SourceAsk::Picked(s) if !sources.contains(&s) => sources.push(s),
            _ => {}
        }
    });
}

fn press(edits: &mut InputEdits, target: CaptureTarget) {
    edits.commands.push(InputCmd::Capture(target));
}

// ---- actions ---------------------------------------------------------------

fn action_editor(
    ui: &mut egui::Ui,
    a: &mut Action,
    live: bool,
    players: u8,
    multiplayer: bool,
    capture: Option<&CaptureTarget>,
    edits: &mut InputEdits,
) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(if live { icons::ON } else { icons::OFF }).color(if live { LIVE } else { ui.visuals().weak_text_color() }));
        ui.label(egui::RichText::new(if live { "triggered now" } else { "press a binding to test it" }).weak().small());
    });
    if a.bindings.is_empty() {
        ui.colored_label(WARN, format!("{} nothing triggers this yet — add a binding below", icons::WARN));
    }
    let name = a.name.clone();
    let mut op = None;
    for i in 0..a.bindings.len() {
        let b = &mut a.bindings[i];
        let r = ui.push_id(("b", i), |ui| {
            card(ui, &format!("{}  {}", b.source.device().icon(), device_name(b.source)), |ui| {
                field(ui, "input", |ui| {
                    let waiting = capture == Some(&CaptureTarget::ReplaceInAction { action: name.clone(), index: i });
                    match source_button(ui, &b.source.label(), waiting, false, multiplayer) {
                        SourceAsk::Press => press(edits, CaptureTarget::ReplaceInAction { action: name.clone(), index: i }),
                        SourceAsk::Picked(s) => b.source = s,
                        SourceAsk::None => {}
                    }
                });
                field(ui, "held with", |ui| {
                    // A chord's other keys: picked, because a press-to-bind
                    // already records whatever modifiers were held with it.
                    let mut remove = None;
                    for (j, m) in b.modifiers.iter().enumerate() {
                        if ui.small_button(format!("{} ×", m.label())).on_hover_text("click to remove").clicked() {
                            remove = Some(j);
                        }
                    }
                    if let Some(j) = remove {
                        b.modifiers.remove(j);
                    }
                    ui.menu_button(egui::RichText::new(format!("{} key", icons::ADD)).small(), |ui| {
                        for (label, k) in [
                            ("Ctrl", floptle_input::Key::ControlLeft),
                            ("Shift", floptle_input::Key::ShiftLeft),
                            ("Alt", floptle_input::Key::AltLeft),
                        ] {
                            if ui.button(label).clicked() {
                                let s = Source::Key(k);
                                if !b.modifiers.contains(&s) {
                                    b.modifiers.push(s);
                                }
                                ui.close();
                            }
                        }
                        ui.separator();
                        if let Some(s) = pick_menus(ui, false, multiplayer)
                            && !b.modifiers.contains(&s)
                        {
                            b.modifiers.push(s);
                        }
                    })
                    .response
                    .on_hover_text("make it a chord: these must be held too, like Ctrl for Ctrl+S");
                });
                if b.source.is_analog() {
                    field(ui, "counts past", |ui| {
                        ui.add(egui::Slider::new(&mut b.threshold, 0.05..=1.0).fixed_decimals(2))
                            .on_hover_text("how far a trigger or stick must go to count as pressed");
                    });
                }
                player_combo(ui, &mut b.player, players);
            })
        });
        if r.inner != CardOp::None {
            op = Some((i, r.inner));
        }
    }
    if let Some((i, o)) = op {
        apply_card_op(&mut a.bindings, i, o);
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        let waiting = capture == Some(&CaptureTarget::AddToAction(name.clone()));
        let add = if waiting { egui::RichText::new("press any input…").color(LIVE).strong() } else { egui::RichText::new(format!("{} Press to add", icons::ADD)) };
        if ui.button(add).on_hover_text("bind by pressing a key, mouse button or gamepad control — hold Ctrl/Shift/Alt to make a chord").clicked() {
            press(edits, CaptureTarget::AddToAction(name.clone()));
        }
        ui.menu_button(format!("{} Pick", icons::MENU), |ui| {
            if let Some(s) = pick_menus(ui, false, multiplayer) {
                let b = Binding::new(s);
                if !a.bindings.iter().any(|x| x.same_source(&b)) {
                    a.bindings.push(b);
                }
            }
        })
        .response
        .on_hover_text("pick from a list — works with nothing plugged in");
    });
    let kb = a.bindings.iter().any(|b| !matches!(b.source, Source::Pad { .. }));
    let pad = a.bindings.iter().any(|b| matches!(b.source, Source::Pad { .. }));
    if !a.bindings.is_empty() && (!kb || !pad) {
        ui.label(egui::RichText::new(if kb { "no gamepad binding yet" } else { "no keyboard or mouse binding yet" }).weak().small());
    }
}

fn device_name(s: Source) -> &'static str {
    match s {
        Source::Key(_) => "Key",
        Source::Mouse(_) => "Mouse button",
        Source::MouseAxis(_) => "Mouse axis",
        Source::Pad { ctrl: PadControl::Button(_), .. } => "Gamepad button",
        Source::Pad { ctrl: PadControl::Axis(_), .. } => "Gamepad axis",
    }
}

// ---- directions (2D) ---------------------------------------------------------

fn axis2_editor(
    ui: &mut egui::Ui,
    a: &mut Axis2,
    live: (f32, f32),
    players: u8,
    multiplayer: bool,
    capture: Option<&CaptureTarget>,
    edits: &mut InputEdits,
) {
    ui.horizontal(|ui| {
        vector_preview(ui, live);
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(format!("x {:+.2}   y {:+.2}", live.0, live.1)).monospace());
            socd_row(ui, &mut a.socd);
        });
    });
    if a.bindings.is_empty() {
        ui.colored_label(WARN, format!("{} nothing drives this yet — add WASD, a stick or the mouse below", icons::WARN));
    }
    let name = a.name.clone();
    let mut op = None;
    for i in 0..a.bindings.len() {
        let r = ui.push_id(("b2", i), |ui| match &mut a.bindings[i] {
            Axis2Binding::Keys { up, down, left, right, player } => card(ui, &format!("{}  Four keys", icons::KEYBOARD), |ui| {
                for (dir, (label, key)) in [("up", up), ("down", down), ("left", left), ("right", right)].into_iter().enumerate() {
                    field(ui, label, |ui| {
                        let t = CaptureTarget::Axis2Key { axis: name.clone(), index: i, dir };
                        match source_button(ui, &key.label(), capture == Some(&t), false, multiplayer) {
                            SourceAsk::Press => press(edits, t),
                            SourceAsk::Picked(s) => *key = s,
                            SourceAsk::None => {}
                        }
                    });
                }
                player_combo(ui, player, players);
            }),
            Axis2Binding::Stick { id, x, y: _, player, deadzone, sensitivity, invert_y, curve } => {
                card(ui, &format!("{}  Stick", icons::PAD), |ui| {
                    field(ui, "stick", |ui| {
                        let right = matches!(x, PadAxis::RightStickX);
                        let mut r = right;
                        ui.selectable_value(&mut r, false, "left");
                        ui.selectable_value(&mut r, true, "right");
                        if r != right {
                            *x = if r { PadAxis::RightStickX } else { PadAxis::LeftStickX };
                        }
                    });
                    pad_id_row(ui, id);
                    analog_fields(ui, deadzone, sensitivity, curve);
                    field(ui, "invert up/down", |ui| {
                        ui.checkbox(invert_y, "");
                    });
                    player_combo(ui, player, players);
                })
            }
            Axis2Binding::Mouse { sensitivity, invert_y, rate, gate } => card(ui, &format!("{}  Mouse movement", icons::MOUSE), |ui| {
                field(ui, "sensitivity", |ui| {
                    ui.add(egui::DragValue::new(sensitivity).speed(0.01).range(0.0..=20.0));
                });
                field(ui, "invert up/down", |ui| {
                    ui.checkbox(invert_y, "");
                });
                field(ui, "as a rate", |ui| {
                    ui.checkbox(rate, "").on_hover_text(
                        "pixels per second, so `yaw = yaw - x * dt` turns the same with a mouse and a stick. \
                         Off: raw pixels this frame.",
                    );
                });
                field(ui, "only while held", |ui| {
                    let t = CaptureTarget::Gate2 { axis: name.clone(), index: i };
                    let waiting = capture == Some(&t);
                    source_list(ui, gate, waiting, multiplayer, || press(edits, t));
                });
                if gate.is_empty() {
                    ui.label(egui::RichText::new("always on — add the right mouse button for a hold-to-look camera").weak().small());
                }
            }),
        });
        if r.inner != CardOp::None {
            op = Some((i, r.inner));
        }
    }
    if let Some((i, o)) = op {
        apply_card_op(&mut a.bindings, i, o);
    }
    // Presets: a binding already filled in, then edited like any other.
    use floptle_input::Key as K;
    let keys = |u, d, l, r| Axis2Binding::Keys { up: Source::Key(u), down: Source::Key(d), left: Source::Key(l), right: Source::Key(r), player: None };
    let stick = |right: bool| Axis2Binding::Stick {
        id: PadId::Any,
        x: if right { PadAxis::RightStickX } else { PadAxis::LeftStickX },
        y: if right { PadAxis::RightStickY } else { PadAxis::LeftStickY },
        player: None,
        deadzone: 0.15,
        sensitivity: 1.0,
        invert_y: false,
        curve: Curve::Linear,
    };
    ui.add_space(4.0);
    ui.menu_button(format!("{} Add", icons::ADD), |ui| {
        let mut add = |ui: &mut egui::Ui, label: &str, b: Axis2Binding| {
            if ui.button(label).clicked() {
                a.bindings.push(b);
                ui.close();
            }
        };
        add(ui, "W A S D", keys(K::KeyW, K::KeyS, K::KeyA, K::KeyD));
        add(ui, "Arrow keys", keys(K::ArrowUp, K::ArrowDown, K::ArrowLeft, K::ArrowRight));
        add(ui, "Four keys of my own…", keys(K::KeyI, K::KeyK, K::KeyJ, K::KeyL));
        ui.separator();
        add(ui, "Left stick", stick(false));
        add(ui, "Right stick", stick(true));
        ui.separator();
        add(ui, "Mouse movement", Axis2Binding::Mouse { sensitivity: 1.0, invert_y: false, rate: true, gate: Vec::new() });
        add(
            ui,
            "Mouse while right button held",
            Axis2Binding::Mouse { sensitivity: 1.0, invert_y: false, rate: true, gate: vec![Source::Mouse(floptle_input::MouseButton::Right)] },
        );
    });
}

/// Fix the stick's Y to go with its X (left with left, right with right).
pub(crate) fn settle_sticks(map: &mut InputMap) {
    for a in &mut map.axes2 {
        for b in &mut a.bindings {
            if let Axis2Binding::Stick { x, y, .. } = b {
                *y = if matches!(x, PadAxis::RightStickX) { PadAxis::RightStickY } else { PadAxis::LeftStickY };
            }
        }
    }
}

fn pad_id_row(ui: &mut egui::Ui, id: &mut PadId) {
    field(ui, "gamepad", |ui| {
        egui::ComboBox::from_id_salt("padid")
            .selected_text(match id {
                PadId::Any => "any".to_string(),
                PadId::Slot(n) => format!("pad {}", *n + 1),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(id, PadId::Any, "any").on_hover_text("whichever pad belongs to the player reading it");
                for n in 0..4u8 {
                    ui.selectable_value(id, PadId::Slot(n), format!("pad {}", n + 1));
                }
            });
    });
}

fn vector_preview(ui: &mut egui::Ui, (x, y): (f32, f32)) {
    let size = 56.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    let r = size * 0.45;
    p.circle_stroke(c, r, egui::Stroke::new(1.0, ui.visuals().weak_text_color()));
    p.line_segment([c - egui::vec2(r, 0.0), c + egui::vec2(r, 0.0)], egui::Stroke::new(0.5, ui.visuals().weak_text_color()));
    p.line_segment([c - egui::vec2(0.0, r), c + egui::vec2(0.0, r)], egui::Stroke::new(0.5, ui.visuals().weak_text_color()));
    let live = x.abs() > 0.01 || y.abs() > 0.01;
    // Screen y grows down; the axis's y is up-positive.
    let dot = c + egui::vec2(x.clamp(-1.5, 1.5), -y.clamp(-1.5, 1.5)) * r;
    p.circle_filled(dot, 4.0, if live { LIVE } else { ui.visuals().weak_text_color() });
}

// ---- amounts (1D) ------------------------------------------------------------

fn axis1_editor(
    ui: &mut egui::Ui,
    a: &mut Axis1,
    live: f32,
    players: u8,
    multiplayer: bool,
    capture: Option<&CaptureTarget>,
    edits: &mut InputEdits,
) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(160.0, 10.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_stroke(rect, 2.0, egui::Stroke::new(1.0, ui.visuals().weak_text_color()), egui::StrokeKind::Inside);
        let mid = rect.center().x;
        let end = mid + live.clamp(-1.0, 1.0) * rect.width() * 0.5;
        p.rect_filled(egui::Rect::from_x_y_ranges(mid.min(end)..=mid.max(end), rect.y_range()), 2.0, LIVE);
        ui.label(egui::RichText::new(format!("{live:+.2}")).monospace());
    });
    socd_row(ui, &mut a.socd);
    if a.bindings.is_empty() {
        ui.colored_label(WARN, format!("{} nothing drives this yet — add a trigger, the wheel or a key pair", icons::WARN));
    }
    let name = a.name.clone();
    let mut op = None;
    for i in 0..a.bindings.len() {
        let r = ui.push_id(("b1", i), |ui| match &mut a.bindings[i] {
            Axis1Binding::Keys { minus, plus, player } => card(ui, &format!("{}  Key pair", icons::KEYBOARD), |ui| {
                for (p, (label, key)) in [("minus", minus), ("plus", plus)].into_iter().enumerate() {
                    field(ui, label, |ui| {
                        let t = CaptureTarget::Axis1Key { axis: name.clone(), index: i, plus: p == 1 };
                        match source_button(ui, &key.label(), capture == Some(&t), false, multiplayer) {
                            SourceAsk::Press => press(edits, t),
                            SourceAsk::Picked(s) => *key = s,
                            SourceAsk::None => {}
                        }
                    });
                }
                player_combo(ui, player, players);
            }),
            Axis1Binding::Analog { source, player, deadzone, sensitivity, invert, curve, gate } => {
                card(ui, &format!("{}  {}", source.device().icon(), device_name(*source)), |ui| {
                    field(ui, "input", |ui| {
                        let t = CaptureTarget::Axis1Source { axis: name.clone(), index: i };
                        match source_button(ui, &source.label(), capture == Some(&t), true, multiplayer) {
                            SourceAsk::Press => press(edits, t),
                            SourceAsk::Picked(s) => *source = s,
                            SourceAsk::None => {}
                        }
                    });
                    analog_fields(ui, deadzone, sensitivity, curve);
                    field(ui, "invert", |ui| {
                        ui.checkbox(invert, "");
                    });
                    field(ui, "only while held", |ui| {
                        let t = CaptureTarget::Gate1 { axis: name.clone(), index: i };
                        let waiting = capture == Some(&t);
                        source_list(ui, gate, waiting, multiplayer, || press(edits, t));
                    });
                    player_combo(ui, player, players);
                })
            }
        });
        if r.inner != CardOp::None {
            op = Some((i, r.inner));
        }
    }
    if let Some((i, o)) = op {
        apply_card_op(&mut a.bindings, i, o);
    }
    use floptle_input::{Key as K, MouseAxis};
    let analog = |s: Source| Axis1Binding::Analog {
        source: s,
        player: None,
        deadzone: 0.1,
        sensitivity: 1.0,
        invert: false,
        curve: Curve::Linear,
        gate: Vec::new(),
    };
    ui.add_space(4.0);
    ui.menu_button(format!("{} Add", icons::ADD), |ui| {
        let mut add = |ui: &mut egui::Ui, label: &str, b: Axis1Binding| {
            if ui.button(label).clicked() {
                a.bindings.push(b);
                ui.close();
            }
        };
        add(ui, "Key pair (Q / E)", Axis1Binding::Keys { minus: Source::Key(K::KeyQ), plus: Source::Key(K::KeyE), player: None });
        add(ui, "Right trigger", analog(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::RightZ) }));
        add(ui, "Left trigger", analog(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::LeftZ) }));
        add(ui, "Mouse wheel", analog(Source::MouseAxis(MouseAxis::ScrollY)));
        add(ui, "A stick's one direction", analog(Source::Pad { id: PadId::Any, ctrl: PadControl::Axis(PadAxis::LeftStickX) }));
    });
}

// ---- motions ----------------------------------------------------------------

fn motion_editor(ui: &mut egui::Ui, m: &mut Motion) {
    crate::responsive::para(
        ui,
        egui::RichText::new(
            "A sequence of directions, oldest first, in numpad notation: 6 is forward, 4 back, \
             2 down, 8 up, 5 neutral. Tap the pad to add one.",
        )
        .weak()
        .small(),
    );
    ui.horizontal(|ui| {
        // The numpad, laid out as it is on a keyboard.
        egui::Grid::new("numpad").spacing([2.0, 2.0]).show(ui, |ui| {
            for row in [[7u8, 8, 9], [4, 5, 6], [1, 2, 3]] {
                for d in row {
                    if ui.add_sized([30.0, 26.0], egui::Button::new(format!("{}\n{d}", arrow(d)))).clicked() && m.dirs.len() < 16 {
                        m.dirs.push(d);
                    }
                }
                ui.end_row();
            }
        });
        ui.vertical(|ui| {
            ui.horizontal_wrapped(|ui| {
                if m.dirs.is_empty() {
                    ui.colored_label(WARN, "no directions yet");
                }
                for d in &m.dirs {
                    ui.label(egui::RichText::new(format!("{} {d}", arrow(*d))).monospace().strong());
                }
            });
            ui.horizontal(|ui| {
                if ui.small_button("undo").on_hover_text("remove the last direction").clicked() {
                    m.dirs.pop();
                }
                if ui.small_button("clear").clicked() {
                    m.dirs.clear();
                }
            });
        });
    });
    field(ui, "within", |ui| {
        ui.add(egui::DragValue::new(&mut m.window).range(1..=240).suffix(" ticks"))
            .on_hover_text("the whole sequence must happen inside this many fixed ticks");
    });
    field(ui, "charge first", |ui| {
        ui.add(egui::DragValue::new(&mut m.charge).range(0..=240).suffix(" ticks"))
            .on_hover_text("hold the first direction this long before the rest counts (0 = no charge)");
    });
}

fn arrow(d: u8) -> &'static str {
    match d {
        1 => "↙",
        2 => "↓",
        3 => "↘",
        4 => "←",
        5 => "•",
        6 => "→",
        7 => "↖",
        8 => "↑",
        9 => "↗",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_input::Key;

    #[test]
    fn a_pressed_input_lands_where_the_target_says() {
        let mut map = InputMap::starter();
        let mv = map.axes2.iter().position(|a| a.name == "Move").expect("starter has Move");
        let wasd = map.axes2[mv].bindings.iter().position(|b| matches!(b, Axis2Binding::Keys { .. })).expect("WASD");
        let t = CaptureTarget::Axis2Key { axis: "Move".into(), index: wasd, dir: 0 };
        assert!(t.apply(&mut map, Source::Key(Key::KeyZ), vec![]));
        match &map.axes2[mv].bindings[wasd] {
            Axis2Binding::Keys { up, down, .. } => {
                assert_eq!(*up, Source::Key(Key::KeyZ), "the up key was not replaced");
                assert_eq!(*down, Source::Key(Key::KeyS), "a key that was not asked for changed");
            }
            other => panic!("{other:?}"),
        }
        let jump = map.actions.iter().position(|a| a.name == "Jump").expect("starter has Jump");
        let n = map.actions[jump].bindings.len();
        let t = CaptureTarget::ReplaceInAction { action: "Jump".into(), index: 0 };
        assert!(t.apply(&mut map, Source::Key(Key::KeyX), vec![Source::Key(Key::ControlLeft)]));
        assert_eq!(map.actions[jump].bindings.len(), n, "replacing added a binding");
        assert_eq!(map.actions[jump].bindings[0].source, Source::Key(Key::KeyX));
        assert_eq!(map.actions[jump].bindings[0].modifiers, vec![Source::Key(Key::ControlLeft)]);
        let t = CaptureTarget::AddToAction("Jump".into());
        assert!(!t.apply(&mut map, Source::Key(Key::KeyX), vec![Source::Key(Key::ControlLeft)]), "a duplicate was added");
    }

    #[test]
    fn rename_duplicate_move_and_delete_keep_the_map_whole() {
        let mut map = InputMap::starter();
        let sel = Sel::Axis2("Move".into());
        map.motion_axis = Some("Move".into());
        rename(&mut map, &sel, "Walk");
        assert!(map.axis2_index("Walk").is_some() && map.axis2_index("Move").is_none());
        assert_eq!(map.motion_axis.as_deref(), Some("Walk"), "motions lost the direction they read");
        let copy = duplicate(&mut map, &Sel::Axis2("Walk".into())).expect("a copy");
        assert_eq!(copy, Sel::Axis2("WalkCopy".into()));
        assert_eq!(
            map.axes2[map.axis2_index("WalkCopy").unwrap()].bindings,
            map.axes2[map.axis2_index("Walk").unwrap()].bindings
        );
        let first = map.actions[0].name.clone();
        shift(&mut map, &Sel::Action(first.clone()), 1);
        assert_eq!(map.actions[1].name, first);
        shift(&mut map, &Sel::Action(first.clone()), -5);
        assert_eq!(map.actions[1].name, first, "a move past the end must do nothing");
        delete(&mut map, &Sel::Action(first.clone()));
        assert!(map.action_index(&first).is_none());
        assert_eq!(fresh_name(&map, "Walk"), "Walk2");
    }
}
