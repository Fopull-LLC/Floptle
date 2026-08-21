//! 🎓 Learn — follow-along tutorials that watch the project you're building.
//!
//! A written tutorial can only ever *tell* you what to do; you find out whether
//! you did it when you press Play and something isn't there. So every step here
//! carries a [`Check`] — one fact about the project the editor can verify — and
//! the step ticks itself the moment that fact becomes true. "Add a node called
//! Player" goes green when a node called Player exists, in the scene you have
//! open, however you made it.
//!
//! That is the whole design. It costs a little machinery ([`Snapshot`], rescanned
//! a few times a second while the tab is visible) and it buys the thing a
//! tutorial normally can't give you: you are never quietly three steps past a
//! mistake.
//!
//! Checks are **not** graders. Nothing is blocked, nothing is scored, and every
//! step can be ticked by hand — a check that can't tell "done differently" from
//! "not done" must never be the thing standing between you and step 7.
//!
//! The tutorials themselves live in [`crate::learn_content`]. They are written
//! once and read twice: this tab, and `docs/tutorials/*.md`, generated from the
//! same table by [`render_markdown`] (`lua_api.rs`'s trick — a reference that
//! exists in two places is two references that disagree).

use std::collections::HashMap;
use std::path::Path;

use floptle_core::{Name, Scripts, World};

/// How hard a tutorial expects you to work, in words rather than a number.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    /// No programming experience assumed. Every line is explained.
    Beginner,
    /// You've written code before, in some language.
    Intermediate,
    /// Written for someone who has shipped software and wants the model.
    Programmer,
}

impl Level {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Level::Beginner => "no experience needed",
            Level::Intermediate => "some coding",
            Level::Programmer => "for programmers",
        }
    }
}

/// One fact about the project the editor can check, so a step knows it's done.
///
/// Each variant answers a question the user just acted on — and answers it from
/// what's actually in the scene and on disk, not from a button they clicked.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Check {
    /// Nothing to verify — reading, deciding, looking at something. Tick it
    /// yourself. Used for the steps that are genuinely just prose, rather than
    /// inventing a check that would be theatre.
    Read,
    /// A node with this name exists in the open scene.
    Node(&'static str),
    /// The node named `node` carries the script `script` (its file stem).
    NodeRuns { node: &'static str, script: &'static str },
    /// `scripts/<stem>.lua` exists in the project.
    Script(&'static str),
    /// `scripts/<stem>.lua` exists and contains `needle`. `what` says what is
    /// being looked for in words, because a needle is code and the panel is
    /// talking to a person.
    Contains { script: &'static str, needle: &'static str, what: &'static str },
    /// The node named `node` carries the tag `tag`.
    Tagged { node: &'static str, tag: &'static str },
    /// A scene by this name exists under `scenes/`.
    Scene(&'static str),
    /// `prefabs/<name>.prefab.ron` exists.
    Prefab(&'static str),
    /// Play has been pressed at least once this session.
    Played,
}

impl Check {
    /// What the panel says it is watching for. Present tense, because it is a
    /// statement about the project right now, not an instruction.
    pub(crate) fn describe(self) -> String {
        match self {
            Check::Read => "nothing to check — tick it when you've read it".into(),
            Check::Node(n) => format!("a node called {n} is in the scene"),
            Check::NodeRuns { node, script } => format!("{node} runs {script}.lua"),
            Check::Script(s) => format!("scripts/{s}.lua exists"),
            Check::Contains { script, what, .. } => format!("{script}.lua {what}"),
            Check::Tagged { node, tag } => format!("{node} is tagged \"{tag}\""),
            Check::Scene(s) => format!("scenes/{s}.ron exists"),
            Check::Prefab(p) => format!("prefabs/{p}.prefab.ron exists"),
            Check::Played => "you've pressed Play".into(),
        }
    }

    /// Whether the project satisfies this check right now.
    pub(crate) fn satisfied(self, snap: &Snapshot) -> bool {
        match self {
            Check::Read => false,
            Check::Node(n) => snap.nodes.iter().any(|(name, _)| name == n),
            Check::NodeRuns { node, script } => snap
                .nodes
                .iter()
                .any(|(name, scripts)| name == node && scripts.iter().any(|s| s == script)),
            Check::Script(s) => snap.scripts.contains_key(s),
            Check::Contains { script, needle, .. } => {
                snap.scripts.get(script).is_some_and(|src| squeezed(src).contains(&squeezed(needle)))
            }
            Check::Tagged { node, tag } => {
                snap.tags.iter().any(|(name, tags)| name == node && tags.iter().any(|t| t == tag))
            }
            Check::Scene(s) => snap.scenes.iter().any(|n| n == s),
            Check::Prefab(p) => snap.prefabs.iter().any(|n| n == p),
            Check::Played => snap.played,
        }
    }
}

/// Drop whitespace entirely before comparing, so a `Contains` check survives the
/// reader's own formatting. `node.vel = vec3(0, 1, 0)` and `node.vel=vec3(0,1,0)`
/// are the same line to a person, and a tutorial that disagrees is a tutorial
/// that punishes you for typing it out instead of pasting it.
///
/// Ignoring whitespace can in principle join two words into a match that wasn't
/// there — but the needles are written here, next to the step, so that stays a
/// theoretical problem rather than one a reader ever meets.
fn squeezed(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

/// One step: what to do, why, and how the editor knows you did it.
pub(crate) struct Step {
    pub(crate) title: &'static str,
    /// The body, in the same restricted markdown [`crate::EditorTabViewer::doc_body_ui`]
    /// renders: `## ` headings, `- ` bullets, and fenced code.
    pub(crate) body: &'static str,
    /// A script this step writes: `(stem, source)`. The panel offers to create
    /// `scripts/<stem>.lua` with it when the file doesn't exist yet, and to open
    /// it when it does — it never overwrites work.
    pub(crate) code: Option<(&'static str, &'static str)>,
    pub(crate) check: Check,
}

/// A whole tutorial: what you'll have built, and the steps that get you there.
pub(crate) struct Tutorial {
    /// Stable slug — the progress file and the generated doc are keyed on it.
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    /// One line: what you will have when you're done.
    pub(crate) tagline: &'static str,
    pub(crate) level: Level,
    pub(crate) minutes: u32,
    /// The starter template holding the finished version, if there is one.
    pub(crate) template: Option<&'static str>,
    /// Read before step 1: what this builds and what it assumes.
    pub(crate) intro: &'static str,
    pub(crate) steps: &'static [Step],
}

/// What the project looks like right now, as far as the checks care.
///
/// Rebuilt on a timer while the tab is open rather than per frame: it reads
/// every script in the project, and a file walk at 120 Hz for a panel nobody is
/// looking at is a waste of somebody's battery.
#[derive(Default)]
pub(crate) struct Snapshot {
    /// Every named node in the open scene, with the script stems it runs.
    pub(crate) nodes: Vec<(String, Vec<String>)>,
    /// The same nodes, with their tags.
    pub(crate) tags: Vec<(String, Vec<String>)>,
    /// Script stem → source, for every `.lua` under `scripts/`.
    pub(crate) scripts: HashMap<String, String>,
    /// Scene names (file stems) under `scenes/`.
    pub(crate) scenes: Vec<String>,
    /// Prefab names under `prefabs/` (without the `.prefab.ron` tail).
    pub(crate) prefabs: Vec<String>,
    /// Play has been pressed at least once this session.
    pub(crate) played: bool,
}

/// The tab's own state: which tutorial is open, where you are in it, what you've
/// ticked by hand, and the last snapshot the checks were answered from.
#[derive(Default)]
pub(crate) struct LearnState {
    /// Index into [`crate::learn_content::TUTORIALS`], or none while browsing.
    pub(crate) open: Option<usize>,
    /// The step currently expanded.
    pub(crate) step: usize,
    /// Steps ticked by hand, per tutorial id.
    pub(crate) manual: HashMap<String, Vec<usize>>,
    pub(crate) snap: Snapshot,
    /// Seconds (editor clock) until the next rescan.
    pub(crate) next_scan: f32,
    /// Set the first time Play runs, so `Check::Played` can be answered after
    /// the user has stopped again.
    pub(crate) played: bool,
    /// Something the panel just did, shown under the step it happened in.
    pub(crate) note: Option<String>,
}

/// How often the checks re-read the project while the tab is visible.
pub(crate) const RESCAN_SECS: f32 = 0.4;

/// Read the open scene and the project's files into a [`Snapshot`].
pub(crate) fn scan(world: &World, root: &Path, played: bool) -> Snapshot {
    let mut nodes: Vec<(String, Vec<String>)> = Vec::new();
    let mut tags: Vec<(String, Vec<String>)> = Vec::new();
    for (e, n) in world.query::<Name>() {
        let scripts = world
            .get::<Scripts>(e)
            .map(|s| s.0.iter().map(|i| i.kind.clone()).collect())
            .unwrap_or_default();
        nodes.push((n.0.clone(), scripts));
        if let Some(t) = world.get::<floptle_core::Tags>(e) {
            tags.push((n.0.clone(), t.0.clone()));
        }
    }
    nodes.sort();
    tags.sort();

    let mut scripts = HashMap::new();
    for e in std::fs::read_dir(root.join("scripts")).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "lua")
            && let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned())
            && let Ok(src) = std::fs::read_to_string(&p)
        {
            scripts.insert(stem, src);
        }
    }

    let mut scenes = Vec::new();
    for e in std::fs::read_dir(root.join("scenes")).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "ron")
            && let Some(stem) = p.file_stem()
        {
            scenes.push(stem.to_string_lossy().into_owned());
        }
    }
    scenes.sort();

    let mut prefabs = Vec::new();
    for e in std::fs::read_dir(root.join("prefabs")).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(floptle_scene::PREFAB_EXT) {
            prefabs.push(stem.to_string());
        }
    }
    prefabs.sort();

    Snapshot { nodes, tags, scripts, scenes, prefabs, played }
}

// ---- progress, saved with the project ----------------------------------------

/// Where a project remembers how far through a tutorial you are.
///
/// In the PROJECT, not the editor's config: progress is a fact about this game
/// you are building, so it should still be there on another machine, and two
/// projects should not share one bookmark.
pub(crate) fn progress_path(root: &Path) -> std::path::PathBuf {
    root.join(".floptle").join("learn.txt")
}

/// Load `<project>/.floptle/learn.txt` — one line per tutorial:
/// `<id> <step> <comma-separated hand-ticked steps>`.
pub(crate) fn load_progress(root: &Path) -> (HashMap<String, usize>, HashMap<String, Vec<usize>>) {
    let mut steps = HashMap::new();
    let mut manual: HashMap<String, Vec<usize>> = HashMap::new();
    let Ok(text) = std::fs::read_to_string(progress_path(root)) else {
        return (steps, manual);
    };
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(id), Some(step)) = (f.next(), f.next()) else { continue };
        steps.insert(id.to_string(), step.parse().unwrap_or(0));
        let ticked = f
            .next()
            .map(|t| t.split(',').filter_map(|n| n.parse().ok()).collect())
            .unwrap_or_default();
        manual.insert(id.to_string(), ticked);
    }
    (steps, manual)
}

/// Write the progress file. Best-effort: a project on a read-only checkout
/// still works, it just doesn't remember where you were.
pub(crate) fn save_progress(
    root: &Path,
    steps: &HashMap<String, usize>,
    manual: &HashMap<String, Vec<usize>>,
) {
    let path = progress_path(root);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut ids: Vec<&String> = steps.keys().chain(manual.keys()).collect();
    ids.sort();
    ids.dedup();
    let mut out = String::from("# 🎓 Learn: how far you've got. Safe to delete.\n");
    for id in ids {
        let step = steps.get(id).copied().unwrap_or(0);
        let mut ticked = manual.get(id).cloned().unwrap_or_default();
        ticked.sort_unstable();
        ticked.dedup();
        let list: Vec<String> = ticked.iter().map(|n| n.to_string()).collect();
        out.push_str(&format!("{id} {step} {}\n", list.join(",")));
    }
    let _ = std::fs::write(path, out);
}

// ---- the written docs, from the same table -----------------------------------

/// One tutorial as a standalone markdown page.
#[cfg(test)]
pub(crate) fn render_markdown(t: &Tutorial) -> String {
    let mut out = String::new();
    out.push_str(&format!("# {}\n\n{}\n\n", t.title, t.tagline));
    out.push_str(&format!(
        "**{}** · about {} minutes · {} steps\n\n",
        t.level.label(),
        t.minutes,
        t.steps.len()
    ));
    if let Some(tpl) = t.template {
        out.push_str(&format!(
            "The finished project is a starter template: create a new project with the \
             **{tpl}** template (in the Hub, or `floptle --new <dir> --template {tpl}`) to \
             read the answer.\n\n"
        ));
    }
    out.push_str(
        "> Follow this along **inside the editor** — the 🎓 Learn tab has the same steps and \
         ticks each one off as your project starts to match it.\n\n",
    );
    out.push_str(t.intro.trim());
    out.push_str("\n\n");
    for (i, s) in t.steps.iter().enumerate() {
        out.push_str(&format!("## {}. {}\n\n", i + 1, s.title));
        // A step's own sub-headings sit UNDER its number. In the panel they are
        // all just "a heading", but a markdown file grows a table of contents
        // and an outline, and a body heading level with the step it belongs to
        // reads as a sibling of it.
        for line in s.body.trim().lines() {
            match line.strip_prefix("## ") {
                Some(h) => out.push_str(&format!("### {h}\n")),
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        if let Some((stem, src)) = s.code {
            out.push_str(&format!("\n`scripts/{stem}.lua`\n\n```lua\n{}\n```\n", src.trim_end()));
        }
        if s.check != Check::Read {
            out.push_str(&format!("\n*Done when: {}.*\n", s.check.describe()));
        }
        out.push('\n');
    }
    out
}

/// The index page listing every tutorial.
#[cfg(test)]
pub(crate) fn render_index(tutorials: &[Tutorial]) -> String {
    let mut out = String::from(
        "# Tutorials\n\n\
         Follow-along projects that build a small, finished game from an empty one. Each is \
         also in the editor's **🎓 Learn** tab, where every step ticks itself off as your \
         project comes to match it.\n\n\
         | Tutorial | You'll build | Level | Time |\n|---|---|---|---|\n",
    );
    for t in tutorials {
        out.push_str(&format!(
            "| [{}]({}.md) | {} | {} | ~{} min |\n",
            t.title,
            t.id,
            t.tagline,
            t.level.label(),
            t.minutes
        ));
    }
    out.push_str(
        "\n## Starter templates\n\n\
         Three of these have a finished version you can create straight from the Hub's **New \
         project** screen, or from a terminal:\n\n```\nfloptle --new my-game --template platformer\n```\n\n",
    );
    for t in tutorials {
        if let Some(tpl) = t.template {
            out.push_str(&format!("- `{tpl}` — the finished [{}]({}.md).\n", t.title, t.id));
        }
    }
    out.push_str(
        "\nA template is a real project: open it, press Play, then take it apart. Nothing in \
         one is engine magic — every behaviour is a `.lua` file in `scripts/` you can read.\n",
    );
    out
}

// ---- the panel ---------------------------------------------------------------

use crate::EditorTabViewer;
use crate::learn_content::TUTORIALS;

/// Green for a step the project already satisfies.
const DONE: egui::Color32 = egui::Color32::from_rgb(120, 200, 130);
/// The step you're on.
const HERE: egui::Color32 = egui::Color32::from_rgb(120, 175, 255);

/// One row in the step list: where you are, what it's called, and whether the
/// project already satisfies it. Clicking it opens that step.
///
/// A free function rather than a method because it needs nothing from the
/// editor — which also means a test can run it through a real egui frame and
/// check that it paints, rather than trusting that it does.
fn step_row(
    ui: &mut egui::Ui,
    i: usize,
    title: &str,
    done: bool,
    current: bool,
) -> egui::Response {
    let (mark, color) = match (done, current) {
        (true, _) => ("✔", DONE),
        // `▸` read as a stray arrow rather than a status marker next to `✔`
        // and `○` — `crate::icons::ON` is the registered "present and
        // active" glyph and already pairs with `OFF` (`○`, used for
        // "pending" here), so the same convention this file's own "here"
        // step is describing now looks like one.
        (false, true) => (crate::icons::ON, HERE),
        (false, false) => ("○", ui.visuals().weak_text_color()),
    };
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(mark).color(color).monospace());
        let text = egui::RichText::new(format!("{}. {title}", i + 1));
        let text = if current { text.strong() } else { text };
        // A finished step you are not standing on recedes; the one you are on
        // stays plain, because colouring it too would make "done" and "here"
        // look like the same state.
        let text = if done && !current { text.color(color) } else { text };
        ui.add(egui::Label::new(text).sense(egui::Sense::click()))
    })
    .inner
}

impl EditorTabViewer<'_> {
    pub(crate) fn learn_ui(&mut self, ui: &mut egui::Ui) {
        match self.learn.open {
            None => self.learn_index_ui(ui),
            Some(i) if i < TUTORIALS.len() => self.learn_tutorial_ui(ui, i),
            // A progress file naming a tutorial this build doesn't have.
            Some(_) => self.learn.open = None,
        }
    }

    /// The picker: what there is to learn, and what each one leaves you with.
    fn learn_index_ui(&mut self, ui: &mut egui::Ui) {
        ui.add_space(2.0);
        ui.label(egui::RichText::new("Learn by making something").strong().size(16.0));
        ui.small(
            "Each one starts from an empty project and finishes with a small game you can \
             play. Steps tick themselves off as your project comes to match them — so you \
             always know whether the last thing you did worked.",
        );
        ui.add_space(8.0);

        let mut start: Option<usize> = None;
        for (i, t) in TUTORIALS.iter().enumerate() {
            let done = self.learn_done_count(t);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width() - 8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t.title).strong().size(14.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(if done > 0 { "Resume" } else { "Start" }).clicked() {
                            start = Some(i);
                        }
                    });
                });
                ui.label(t.tagline);
                ui.horizontal(|ui| {
                    ui.weak(
                        egui::RichText::new(format!(
                            "{} · about {} min · {} steps",
                            t.level.label(),
                            t.minutes,
                            t.steps.len()
                        ))
                        .small(),
                    );
                    if done > 0 {
                        ui.weak(
                            egui::RichText::new(format!("· {done} of {} done", t.steps.len()))
                                .small()
                                .color(DONE),
                        );
                    }
                });
            });
            ui.add_space(6.0);
        }

        ui.add_space(6.0);
        ui.separator();
        ui.small(
            "Prefer to read the finished code? Three of these ship as starter templates — pick \
             one on the Hub's New project screen, or run \
             `floptle --new my-game --template platformer`. A template is an ordinary project: \
             every behaviour in it is a .lua file in scripts/ you can open and change.",
        );

        if let Some(i) = start {
            self.learn_open(i);
        }
    }

    /// Open a tutorial, restoring where this project left off in it.
    fn learn_open(&mut self, i: usize) {
        let (steps, manual) = load_progress(self.project_root);
        let id = TUTORIALS[i].id;
        self.learn.step = steps.get(id).copied().unwrap_or(0).min(TUTORIALS[i].steps.len() - 1);
        self.learn.manual = manual;
        self.learn.open = Some(i);
        self.learn.note = None;
        // Answer the checks on the first frame rather than after the timer.
        self.learn.next_scan = 0.0;
    }

    /// How many of a tutorial's steps are satisfied right now.
    fn learn_done_count(&self, t: &Tutorial) -> usize {
        let ticked = self.learn.manual.get(t.id);
        (0..t.steps.len())
            .filter(|i| {
                t.steps[*i].check.satisfied(&self.learn.snap)
                    || ticked.is_some_and(|m| m.contains(i))
            })
            .count()
    }

    /// One tutorial: the steps, the one you're on, and what the editor can see.
    fn learn_tutorial_ui(&mut self, ui: &mut egui::Ui, ti: usize) {
        let t = &TUTORIALS[ti];
        let done: Vec<bool> = {
            let ticked = self.learn.manual.get(t.id);
            t.steps
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    s.check.satisfied(&self.learn.snap) || ticked.is_some_and(|m| m.contains(&i))
                })
                .collect()
        };
        let n_done = done.iter().filter(|d| **d).count();

        ui.horizontal(|ui| {
            if ui.small_button("← All tutorials").clicked() {
                self.learn_save(ti);
                self.learn.open = None;
            }
            ui.separator();
            ui.label(egui::RichText::new(t.title).strong());
        });
        ui.horizontal(|ui| {
            ui.add(
                egui::ProgressBar::new(n_done as f32 / t.steps.len() as f32)
                    .desired_width(160.0)
                    .desired_height(8.0),
            );
            ui.weak(format!("{n_done} of {} done", t.steps.len()));
            if n_done == t.steps.len() {
                ui.label(egui::RichText::new("🏆 finished").color(DONE));
            }
        });
        ui.add_space(6.0);

        egui::CollapsingHeader::new("What you're building")
            .id_salt(("learn_intro", t.id))
            .default_open(self.learn.step == 0)
            .show(ui, |ui| {
                self.doc_body_ui(ui, t.intro);
                if let Some(tpl) = t.template {
                    ui.add_space(4.0);
                    ui.small(format!(
                        "Stuck, or want to read ahead? The finished version is the `{tpl}` \
                         template on the Hub's New project screen."
                    ));
                }
            });
        ui.add_space(4.0);

        let mut goto: Option<usize> = None;
        let mut tick: Option<(usize, bool)> = None;
        let mut write: Option<(&'static str, &'static str)> = None;

        for (i, step) in t.steps.iter().enumerate() {
            let current = i == self.learn.step;
            if step_row(ui, i, step.title, done[i], current).clicked() {
                goto = Some(i);
            }

            if !current {
                continue;
            }
            ui.indent(("learn_step", i), |ui| {
                ui.add_space(2.0);
                self.doc_body_ui(ui, step.body);

                if let Some((stem, src)) = step.code {
                    ui.add_space(2.0);
                    self.doc_body_ui(ui, &format!("```\n{}\n```", src.trim_end()));
                    let path = self.project_root.join("scripts").join(format!("{stem}.lua"));
                    let exists = path.exists();
                    ui.horizontal(|ui| {
                        if ui
                            .button("⎘ Copy")
                            .on_hover_text("copy this script to the clipboard")
                            .clicked()
                        {
                            ui.ctx().copy_text(src.to_string());
                        }
                        // Never overwrite: once the file is there it is THEIRS,
                        // even if what they typed is different from this. The
                        // whole point of typing it out is being allowed to
                        // diverge from it.
                        let label = if exists {
                            format!("Open scripts/{stem}.lua")
                        } else {
                            format!("Create scripts/{stem}.lua")
                        };
                        if ui.button(label).clicked() {
                            write = Some((stem, src));
                        }
                    });
                    if exists {
                        ui.small(
                            "Already there — opening it rather than replacing it. Copy above if \
                             you want to paste this version over your own.",
                        );
                    }
                }

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if step.check == Check::Read {
                        let mut checked = done[i];
                        if ui.checkbox(&mut checked, "Read it").changed() {
                            tick = Some((i, checked));
                        }
                    } else if done[i] {
                        ui.label(
                            egui::RichText::new(format!("✔ {}", step.check.describe()))
                                .color(DONE)
                                .small(),
                        );
                    } else {
                        ui.weak(
                            egui::RichText::new(format!("waiting for: {}", step.check.describe()))
                                .small(),
                        );
                        if ui
                            .small_button("tick anyway")
                            .on_hover_text(
                                "checks can't see every way of doing something — this never \
                                 blocks you",
                            )
                            .clicked()
                        {
                            tick = Some((i, true));
                        }
                    }
                });

                if let Some(note) = &self.learn.note
                    && current
                {
                    ui.small(egui::RichText::new(note.clone()).color(HERE));
                }

                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if i > 0 && ui.small_button("← Back").clicked() {
                        goto = Some(i - 1);
                    }
                    if i + 1 < t.steps.len() && ui.button("Next step →").clicked() {
                        goto = Some(i + 1);
                    }
                });
            });
            ui.add_space(4.0);
        }

        if let Some((stem, src)) = write {
            self.learn_write_script(stem, src);
        }
        if let Some((i, on)) = tick {
            let m = self.learn.manual.entry(t.id.to_string()).or_default();
            if on {
                if !m.contains(&i) {
                    m.push(i);
                }
            } else {
                m.retain(|x| *x != i);
            }
            self.learn_save(ti);
        }
        if let Some(i) = goto {
            self.learn.step = i;
            self.learn.note = None;
            self.learn_save(ti);
        }
    }

    /// Create `scripts/<stem>.lua` if it isn't there, then open it in the editor.
    fn learn_write_script(&mut self, stem: &str, src: &str) {
        let dir = self.project_root.join("scripts");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{stem}.lua"));
        if !path.exists() {
            match std::fs::write(&path, format!("{}\n", src.trim_end())) {
                Ok(()) => self.learn.note = Some(format!("wrote scripts/{stem}.lua")),
                Err(e) => {
                    self.learn.note = Some(format!("couldn't write scripts/{stem}.lua: {e}"));
                    return;
                }
            }
            self.cmd.refresh_assets = true;
        }
        self.cmd.open_script = Some(path.to_string_lossy().into_owned());
        self.cmd.focus_scripting = true;
        // The new file has to be in the snapshot before the step can tick.
        self.learn.next_scan = 0.0;
    }

    /// Persist this project's place in tutorial `ti`.
    fn learn_save(&mut self, ti: usize) {
        let mut steps = load_progress(self.project_root).0;
        steps.insert(TUTORIALS[ti].id.to_string(), self.learn.step);
        save_progress(self.project_root, &steps, &self.learn.manual);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn_content::TUTORIALS;

    /// The exact list the editor's own warnings strip lints against, so a
    /// snippet that passes here passes in front of the reader too.
    fn api() -> Vec<String> {
        crate::ide::api_labels()
    }

    /// Every `(stem, source)` a tutorial hands the reader.
    fn snippets() -> Vec<(&'static str, &'static str, &'static str)> {
        TUTORIALS
            .iter()
            .flat_map(|t| t.steps.iter().filter_map(move |s| s.code.map(|(n, c)| (t.id, n, c))))
            .collect()
    }

    #[test]
    fn tutorials_are_well_formed() {
        let mut ids = std::collections::HashSet::new();
        for t in TUTORIALS {
            assert!(ids.insert(t.id), "two tutorials share the id {:?}", t.id);
            assert!(!t.title.is_empty() && !t.tagline.is_empty(), "{} is missing a title", t.id);
            assert!(t.minutes > 0, "{} claims to take no time at all", t.id);
            assert!(!t.steps.is_empty(), "{} has no steps", t.id);
            assert!(t.intro.len() > 200, "{}'s intro doesn't say what it builds", t.id);
            for (i, s) in t.steps.iter().enumerate() {
                assert!(!s.title.is_empty(), "{} step {i} has no title", t.id);
                assert!(s.body.len() > 80, "{} step {i} ({}) barely says anything", t.id, s.title);
            }
        }
    }

    /// A tutorial that names a starter template must name one that exists — the
    /// panel and the generated docs both send the reader off to find it.
    #[test]
    fn every_named_template_exists() {
        for t in TUTORIALS {
            if let Some(name) = t.template {
                assert!(
                    crate::templates::find(name).is_some(),
                    "{} points at a `{name}` template that isn't there",
                    t.id
                );
            }
        }
    }

    /// **Every snippet compiles.** A tutorial that hands you code which doesn't
    /// even parse is worse than one that hands you none.
    #[test]
    fn every_snippet_is_valid_lua() {
        let host = floptle_script::ScriptHost::new();
        for (tut, stem, src) in snippets() {
            if let Some((line, msg)) = host.check_syntax(src) {
                panic!("{tut} / {stem}.lua does not parse — line {line}: {msg}");
            }
        }
    }

    /// …and passes the editor's own lints, which are the ones that catch the
    /// mistakes a parser can't see. Teaching code that trips the warnings strip
    /// the reader is looking at would be a poor start.
    #[test]
    fn every_snippet_lints_clean() {
        let api = api();
        let api: Vec<&str> = api.iter().map(|s| s.as_str()).collect();
        for (tut, stem, src) in snippets() {
            let hits = crate::lua_lint::lint(src, &api);
            assert!(
                hits.is_empty(),
                "{tut} / {stem}.lua trips the editor's own lints: {:?}",
                hits.iter().map(|l| (l.line, &l.message)).collect::<Vec<_>>()
            );
        }
    }

    /// A `Contains` check must be satisfied by the code the step ships.
    ///
    /// Otherwise the step can never tick for a reader who did exactly what it
    /// said — which is the one failure this whole feature exists to avoid.
    #[test]
    fn every_contains_check_matches_its_own_code() {
        for t in TUTORIALS {
            for s in t.steps {
                let Check::Contains { script, needle, .. } = s.check else { continue };
                let Some((stem, src)) = s.code else {
                    panic!("{} / {} watches for {needle:?} but ships no code", t.id, s.title)
                };
                assert_eq!(stem, script, "{} / {} checks a different file", t.id, s.title);
                assert!(
                    squeezed(src).contains(&squeezed(needle)),
                    "{} / {}: the step's own code never contains {needle:?}",
                    t.id,
                    s.title
                );
            }
        }
    }

    /// A step that names a node or a script in a check has to be talking about
    /// something the tutorial actually told you to make. Catches the rename that
    /// updates the prose and forgets the check.
    #[test]
    fn every_check_names_something_the_tutorial_mentions() {
        for t in TUTORIALS {
            let prose: String =
                t.steps.iter().map(|s| format!("{} {}", s.title, s.body)).collect();
            for s in t.steps {
                let needed = match s.check {
                    Check::Node(n) => n.to_string(),
                    Check::NodeRuns { node, .. } => node.to_string(),
                    Check::Tagged { node, .. } => node.to_string(),
                    Check::Scene(n) | Check::Prefab(n) => n.to_string(),
                    _ => continue,
                };
                assert!(
                    prose.contains(&needed),
                    "{} / {}: the check waits for {needed:?}, which no step ever mentions",
                    t.id,
                    s.title
                );
            }
        }
    }

    /// The scripts a step tells you to write must be the ones its check waits
    /// for, and a script named by a later step's check must have been written by
    /// an earlier one.
    #[test]
    fn every_script_check_has_a_step_that_writes_it() {
        for t in TUTORIALS {
            let written: Vec<&str> = t.steps.iter().filter_map(|s| s.code.map(|(n, _)| n)).collect();
            for s in t.steps {
                let stem = match s.check {
                    Check::Script(n) => n,
                    Check::NodeRuns { script, .. } => script,
                    Check::Contains { script, .. } => script,
                    _ => continue,
                };
                assert!(
                    written.contains(&stem),
                    "{} / {}: waits for {stem}.lua, which no step in it writes",
                    t.id,
                    s.title
                );
            }
        }
    }

    #[test]
    fn a_check_reads_the_project_it_is_given() {
        let snap = Snapshot {
            nodes: vec![("Player".into(), vec!["platformerPlayer".into()])],
            tags: vec![("Player".into(), vec!["player".into()])],
            scripts: [("coin".to_string(), "function onTriggerEnter(node)\nend\n".to_string())]
                .into_iter()
                .collect(),
            scenes: vec!["first".into()],
            prefabs: vec!["Pipe".into()],
            played: false,
        };
        assert!(Check::Node("Player").satisfied(&snap));
        assert!(!Check::Node("Enemy").satisfied(&snap));
        assert!(Check::NodeRuns { node: "Player", script: "platformerPlayer" }.satisfied(&snap));
        assert!(!Check::NodeRuns { node: "Player", script: "coin" }.satisfied(&snap));
        assert!(Check::Tagged { node: "Player", tag: "player" }.satisfied(&snap));
        assert!(Check::Script("coin").satisfied(&snap));
        assert!(Check::Scene("first").satisfied(&snap));
        assert!(Check::Prefab("Pipe").satisfied(&snap));
        assert!(!Check::Played.satisfied(&snap));
        // Never automatically true: reading is the reader's to declare.
        assert!(!Check::Read.satisfied(&snap));
        // …and formatting must not decide whether a step ticks.
        let c = Check::Contains { script: "coin", needle: "function onTriggerEnter( node )", what: "" };
        assert!(c.satisfied(&snap), "whitespace should not change the answer");
    }

    /// Collect every string egui actually painted this frame.
    fn painted(out: &egui::FullOutput) -> String {
        fn walk(shape: &egui::epaint::Shape, acc: &mut String) {
            match shape {
                egui::epaint::Shape::Text(t) => {
                    acc.push_str(&t.galley.text().replace('\n', " "));
                    acc.push(' ');
                }
                egui::epaint::Shape::Vec(v) => v.iter().for_each(|s| walk(s, acc)),
                _ => {}
            }
        }
        let mut acc = String::new();
        for p in &out.shapes {
            walk(&p.shape, &mut acc);
        }
        acc
    }

    /// **Every step of every tutorial paints.**
    ///
    /// A panel that draws nothing is the failure a data-only test can't see: the
    /// checks could all be correct while the list itself was clipped to nothing
    /// or never reached. Run the real row widget through a real egui frame with
    /// a real screen rect, and read back what was actually drawn.
    #[test]
    fn every_step_row_paints_its_title_and_its_state() {
        let ctx = crate::icons::test_context();
        for t in TUTORIALS {
            // Twice: egui sizes some things on the previous frame's data, and a
            // one-frame run can report a layout that never settles.
            let mut got = String::new();
            for _ in 0..2 {
                let out = ctx.run_ui(crate::icons::test_input(), |ui| {
                    for (i, s) in t.steps.iter().enumerate() {
                        // Step 1 done, step 2 current, the rest pending — so all
                        // three markers are exercised on every tutorial.
                        step_row(ui, i, s.title, i == 0, i == 1);
                    }
                });
                got = painted(&out);
            }
            for (i, s) in t.steps.iter().enumerate() {
                assert!(
                    got.contains(s.title),
                    "{} step {i} ({:?}) never reached the screen:\n{got}",
                    t.id,
                    s.title
                );
            }
            assert!(got.contains('✔'), "{}: a finished step draws no tick", t.id);
            assert!(
                got.contains(crate::icons::ON),
                "{}: the current step is unmarked",
                t.id
            );
            assert!(got.contains('○'), "{}: a pending step is unmarked", t.id);
        }
    }

    /// Progress survives a round trip, which is the entire contract of the file.
    #[test]
    fn progress_round_trips() {
        let dir = std::env::temp_dir().join(format!("floptle-learn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let steps = [("platformer".to_string(), 4usize)].into_iter().collect();
        let manual = [("platformer".to_string(), vec![0usize, 3])].into_iter().collect();
        save_progress(&dir, &steps, &manual);

        let (got_steps, got_manual) = load_progress(&dir);
        assert_eq!(got_steps.get("platformer"), Some(&4));
        assert_eq!(got_manual.get("platformer"), Some(&vec![0, 3]));
        // An absent project is not an error, it's a reader who hasn't started.
        assert!(load_progress(std::path::Path::new("/nonexistent-floptle")).0.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `docs/tutorials/` is generated from the table above — same discipline as
    /// `docs/lua-api.md`. Two hand-maintained copies of a tutorial are two
    /// tutorials that disagree by the second edit.
    #[test]
    fn tutorial_docs_are_current() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/tutorials");
        let update = std::env::var("UPDATE_DOCS").is_ok();
        let mut want: Vec<(String, String)> =
            vec![("README.md".into(), render_index(TUTORIALS))];
        for t in TUTORIALS {
            want.push((format!("{}.md", t.id), render_markdown(t)));
        }
        if update {
            std::fs::create_dir_all(dir).expect("make docs/tutorials");
            for (name, body) in &want {
                std::fs::write(std::path::Path::new(dir).join(name), body).expect("write");
            }
            return;
        }
        for (name, body) in &want {
            let path = std::path::Path::new(dir).join(name);
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert_eq!(
                &on_disk,
                body,
                "docs/tutorials/{name} is out of date — regenerate with \
                 `UPDATE_DOCS=1 cargo test -p floptle-editor tutorial_docs_are_current`"
            );
        }
        // …and nothing stale left behind from a tutorial that was renamed.
        let expected: std::collections::HashSet<String> =
            want.iter().map(|(n, _)| n.clone()).collect();
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            assert!(expected.contains(&name), "docs/tutorials/{name} belongs to no tutorial");
        }
    }
}
