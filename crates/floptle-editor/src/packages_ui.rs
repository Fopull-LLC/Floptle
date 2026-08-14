//! 📦 **Packages** — the window where a project's packages are installed,
//! switched off, updated, written and found.
//!
//! Three tabs, because there are three different jobs:
//!
//! - **Installed** — what this project has, what each one came from, what it is
//!   allowed to do, and its samples. Where you go when something is wrong.
//! - **Add** — from a folder, from a Git URL, or linked in place while you
//!   write one. Plus ✚ New Package, which scaffolds one that already runs.
//! - **Browse** — the registry catalogue, searchable, one click to install.
//!
//! The catalogue is fetched on a worker thread and the window says so while it
//! is fetching. A registry that is down must leave the other two tabs working:
//! installing from a folder has nothing to do with the network, and a package
//! browser that blocks the whole window when a server is slow is a package
//! browser people learn to avoid.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use floptle_package::{Index, Listing, Permission, Registry, Severity, Source};

/// What the last package load found, in the shape this tab reads it.
///
/// A snapshot rather than a borrow of the extension host. The tab draws from
/// inside the dock's tab viewer, which already holds the host mutably so a
/// package's own panels can run — and one `&mut` and one `&` to the same host
/// cannot both be alive. It is a handful of strings per installed package,
/// rebuilt per frame, which is cheaper than the per-row `.cloned()` it replaces.
#[derive(Default)]
pub(crate) struct PkgLoad {
    /// Everything the load pass had to say: `(package id, how bad, what)`.
    pub(crate) problems: Vec<(Option<String>, Severity, String)>,
    /// Each package that loaded, by id.
    pub(crate) loaded: Vec<(String, floptle_package::Loaded)>,
    /// Each package that raised while running, and what it said.
    pub(crate) failed: Vec<(String, String)>,
}

impl PkgLoad {
    pub(crate) fn of(host: &crate::ext::ExtHost) -> PkgLoad {
        PkgLoad {
            problems: host
                .report
                .problems
                .iter()
                .map(|p| (p.id.clone(), p.severity, p.message.clone()))
                .collect(),
            loaded: host.report.loaded.iter().map(|l| (l.manifest.id.clone(), l.clone())).collect(),
            failed: host
                .packages
                .iter()
                .filter_map(|p| p.failed.as_ref().map(|e| (p.id.clone(), e.clone())))
                .collect(),
        }
    }

    fn find(&self, id: &str) -> Option<&floptle_package::Loaded> {
        self.loaded.iter().find(|(k, _)| k == id).map(|(_, l)| l)
    }

    fn failure(&self, id: &str) -> Option<&str> {
        self.failed.iter().find(|(k, _)| k == id).map(|(_, e)| e.as_str())
    }
}

/// What the tab needs from the editor: where the project is, and what the last
/// package load found. Passed in rather than reached for, because this draws
/// inside the editor's UI pass, where `&mut Editor` does not exist.
pub(crate) struct PkgCtx<'a> {
    pub(crate) project_root: &'a std::path::Path,
    pub(crate) load: &'a PkgLoad,
    /// The signed-in account, shared with the Hub. `None` only before it has
    /// been built — the tab degrades to "cannot review right now" rather than
    /// hiding the reviews it can still read.
    pub(crate) account: Option<&'a floptle_account::Account>,
}

/// Which tab of the window is showing.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Tab {
    #[default]
    Installed,
    Add,
    Browse,
}

/// What a package's reviews are doing. Reading them needs no account, so this
/// works signed out — which matters, because the reviews are most useful to
/// somebody deciding whether to install, and that person may never sign in.
#[derive(Default)]
enum ReviewsState {
    #[default]
    Idle,
    Fetching(Receiver<Result<floptle_package::index::Reviews, String>>),
    Ready(floptle_package::index::Reviews),
    Failed(String),
}

/// What the catalogue fetch is doing.
#[derive(Default)]
enum Catalogue {
    #[default]
    Idle,
    Fetching(Receiver<Result<Index, String>>),
    Ready(Index),
    Failed(String),
}

/// The window's own state. Nothing here is authoritative — the project's
/// `packages.ron` is, and every action rereads it.
#[derive(Default)]
pub(crate) struct PackagesState {
    pub(crate) tab: Tab,
    /// The Git URL / revision / subfolder boxes on the Add tab.
    git_url: String,
    git_rev: String,
    git_subdir: String,
    /// ✚ New Package.
    new_id: String,
    new_name: String,
    /// Browse's search box.
    search: String,
    /// Which shelves are being asked for, and what a package must hold. Both
    /// are AND: picking two narrows, it does not widen.
    categories: Vec<floptle_package::Category>,
    contains: Vec<floptle_package::Facet>,
    sort: floptle_package::Sort,
    /// Hide anything with no release for this engine. **Off by default**:
    /// knowing something exists and wants a newer engine is more useful than
    /// silently not being shown it.
    compatible_only: bool,
    /// Decoded thumbnails, fetched once per session — see [`crate::pkg_thumbs`].
    thumbs: crate::pkg_thumbs::Thumbs,
    catalogue: Catalogue,
    /// The registry to read. Editable, so a studio can point at its own.
    index_url: String,
    /// The last thing that went wrong, shown until the next action.
    error: Option<String>,
    /// The last thing that went right.
    note: Option<String>,
    /// Which row's details are expanded.
    expanded: Option<String>,
    /// A package the user asked to remove, awaiting confirmation. Removing a
    /// package deletes files; that is not a single-click action.
    confirm_remove: Option<String>,
    /// Whose reviews are open, and what they are doing.
    reviews_for: Option<String>,
    reviews: ReviewsState,
    /// A package that arrived from somewhere else and asked for something. It is
    /// installed but NOT enabled until the person who installed it has seen what
    /// it wants — see `gate_remote_install`.
    awaiting_consent: Option<String>,
    /// A gallery image opened full size: its source, its caption, and the folder
    /// to resolve it against. A screenshot shrunk into a 220px strip is a
    /// screenshot nobody can read, and the whole point of a gallery is looking.
    lightbox: Option<(String, String, Option<std::path::PathBuf>)>,
}

impl PackagesState {
    /// The question the browser is currently asking the catalogue.
    ///
    /// Built here rather than inline so the filters, the counts and the rows all
    /// come from one description — a count computed from a different query than
    /// the rows is a shelf that says 12 and shows 3.
    fn query(&self, engine: &floptle_package::Version) -> floptle_package::Query {
        floptle_package::Query {
            search: self.search.clone(),
            categories: self.categories.clone(),
            contains: self.contains.clone(),
            compatible_only: self.compatible_only.then(|| engine.clone()),
            sort: self.sort,
        }
    }

    fn has_filters(&self) -> bool {
        !self.categories.is_empty()
            || !self.contains.is_empty()
            || self.compatible_only
            || !self.search.trim().is_empty()
    }

    fn clear_filters(&mut self) {
        self.categories.clear();
        self.contains.clear();
        self.compatible_only = false;
        self.search.clear();
    }

    fn index_url(&self) -> &str {
        if self.index_url.trim().is_empty() {
            floptle_package::index::DEFAULT_INDEX_URL
        } else {
            self.index_url.trim()
        }
    }
}

/// What the window decided to do, applied by the editor after the UI pass —
/// every one of these reloads the extension host, which cannot happen while the
/// host is drawing.
#[derive(Debug, Default)]
pub(crate) struct PackagesAction {
    /// Reload every package (after any change to what is installed).
    pub(crate) reload: bool,
    /// Open a folder in the file manager.
    pub(crate) open_folder: Option<PathBuf>,
}

/// Draw the 📦 Packages tab. Returns what the editor should do next.
pub(crate) fn body(
    ui: &mut egui::Ui,
    ctx: PkgCtx<'_>,
    state: &mut PackagesState,
) -> PackagesAction {
        let mut action = PackagesAction::default();
        let no_project = ctx.project_root.as_os_str().is_empty();
        if no_project {
            ui.label("Open a project first — packages are installed into a project.");
            return action;
        }

        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Installed, "Installed"),
                (Tab::Add, "✚ Add"),
                (Tab::Browse, "🌐 Browse"),
            ] {
                if ui.selectable_label(state.tab == tab, label).clicked() {
                    state.tab = tab;
                    state.error = None;
                    state.note = None;
                }
            }
            ui.separator();
            if ui
                .button("⟲ Reload all")
                .on_hover_text(
                    "re-read every package from disk and run its editor scripts again — what \
                     you press after editing one",
                )
                .clicked()
            {
                action.reload = true;
            }
        });
        ui.separator();

        if let Some(e) = state.error.clone() {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 110), e);
        }
        if let Some(n) = state.note.clone() {
            ui.colored_label(egui::Color32::from_rgb(130, 210, 150), n);
        }

    match state.tab {
        Tab::Installed => installed_tab(ui, &ctx, state, &mut action),
        Tab::Add => add_tab(ui, &ctx, state, &mut action),
        Tab::Browse => browse_tab(ui, &ctx, state, &mut action),
    }
    action
}

// ---- Installed -------------------------------------------------------------

fn installed_tab(ui: &mut egui::Ui, ctx: &PkgCtx<'_>, state: &mut PackagesState, action: &mut PackagesAction) {
        let reg = match Registry::load(ctx.project_root) {
            Ok(r) => r,
            Err(e) => {
                ui.colored_label(egui::Color32::from_rgb(230, 120, 110), e);
                return;
            }
        };
        if reg.packages.is_empty() {
            ui.add_space(8.0);
            ui.label("No packages in this project yet.");
            ui.small(
                "A package can hold editor tools, scripts your game can use, art — or all \
                 three. Add one from the ✚ Add tab, or write your own.",
            );
            return;
        }

        // Everything the load pass has to say, indexed so a row can show its own
        // problem next to itself rather than in a list at the top.
        let problems = &ctx.load.problems;

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for entry in &reg.packages {
                let loaded = ctx.load.find(&entry.id).cloned();
                let name = loaded
                    .as_ref()
                    .map(|l| l.manifest.name.clone())
                    .unwrap_or_else(|| entry.id.clone());
                let expanded = state.expanded.as_deref() == Some(entry.id.as_str());

                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let mut on = entry.enabled;
                        if ui
                            .checkbox(&mut on, "")
                            .on_hover_text("load this package (off keeps it installed)")
                            .changed()
                        {
                            match floptle_package::install::set_enabled(
                                ctx.project_root,
                                &entry.id,
                                on,
                            ) {
                                Ok(()) => {
                                    // Ticking the box IS the consent.
                                    if on
                                        && state.awaiting_consent.as_deref()
                                            == Some(entry.id.as_str())
                                    {
                                        state.awaiting_consent = None;
                                    }
                                    action.reload = true;
                                }
                                Err(e) => state.error = Some(e.to_string()),
                            }
                        }
                        ui.strong(&name);
                        ui.small(entry.version.to_string());
                        if entry.source.is_linked() {
                            ui.small("🔗 linked")
                                .on_hover_text("read in place — edits show up on Reload");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button(if expanded { "▾" } else { "▸" }).clicked() {
                                state.expanded =
                                    if expanded { None } else { Some(entry.id.clone()) };
                            }
                        });
                    });

                    // A package that did not load says so on its own row.
                    for (id, sev, msg) in problems {
                        if id.as_deref() == Some(entry.id.as_str()) {
                            let col = match sev {
                                Severity::Error => egui::Color32::from_rgb(230, 120, 110),
                                Severity::Warning => egui::Color32::from_rgb(225, 195, 110),
                            };
                            ui.colored_label(col, msg);
                        }
                    }
                    if let Some(err) = ctx.load.failure(&entry.id) {
                        ui.colored_label(egui::Color32::from_rgb(230, 120, 110), err);
                    }

                    // Arrived from somewhere else and asked for something, so it
                    // is sitting here NOT running. The manifest is read from
                    // disk rather than from the host, because a package that is
                    // not enabled was never loaded — and this has to say what it
                    // wants precisely when it is not yet allowed to have it.
                    if state.awaiting_consent.as_deref() == Some(entry.id.as_str()) {
                        ui.colored_label(
                            egui::Color32::from_rgb(225, 195, 110),
                            "⚠ Installed, but NOT running yet — it asked for the following. \
                             Tick the box to let it run.",
                        );
                        if let Ok(m) =
                            floptle_package::Manifest::load(&entry.root_in(ctx.project_root))
                        {
                            permissions_line(ui, &m.permissions);
                        }
                    }

                    if !expanded {
                        return;
                    }
                    ui.separator();
                    ui.small(&entry.id);
                    ui.small(entry.source.describe());
                    if let Some(l) = &loaded {
                        if !l.manifest.description.is_empty() {
                            ui.label(&l.manifest.description);
                        }
                        if let Some(a) = &l.manifest.author {
                            ui.small(format!("by {}", a.name));
                        }
                        permissions_line(ui, &l.manifest.permissions);
                        let counts = (
                            l.editor_scripts().len(),
                            l.manifest
                                .dirs_that_exist(&l.root, floptle_package::DirKind::Scripts)
                                .len(),
                            l.manifest
                                .dirs_that_exist(&l.root, floptle_package::DirKind::Assets)
                                .len(),
                        );
                        ui.small(format!(
                            "{} editor script(s) · {} script folder(s) · {} asset folder(s)",
                            counts.0, counts.1, counts.2
                        ));
                        for sample in &l.manifest.samples {
                            ui.horizontal(|ui| {
                                if ui
                                    .button(format!("⬇ {}", sample.name))
                                    .on_hover_text(
                                        "copy this sample into the project's samples/ folder",
                                    )
                                    .clicked()
                                {
                                    match floptle_package::install::import_sample(
                                        ctx.project_root,
                                        &l.root,
                                        &l.manifest.name,
                                        &sample.name,
                                        &sample.path,
                                    ) {
                                        Ok(p) => {
                                            state.note =
                                                Some(format!("imported to {}", p.display()));
                                        }
                                        Err(e) => {
                                            state.error = Some(e.to_string());
                                        }
                                    }
                                }
                                if !sample.description.is_empty() {
                                    ui.small(&sample.description);
                                }
                            });
                        }
                        if let Some(home) = &l.manifest.homepage
                            && ui.link(home).clicked()
                        {
                            let _ = floptle_script::open_in_browser(home);
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("📂 Show files").clicked() {
                            action.open_folder = Some(entry.root_in(ctx.project_root));
                        }
                        if state.confirm_remove.as_deref() == Some(entry.id.as_str()) {
                            ui.colored_label(
                                egui::Color32::from_rgb(230, 120, 110),
                                if entry.source.is_linked() {
                                    "Unlink it?"
                                } else {
                                    "Delete its files?"
                                },
                            );
                            if ui.button("Yes, remove").clicked() {
                                match floptle_package::install::remove(
                                    ctx.project_root,
                                    &entry.id,
                                ) {
                                    Ok(_) => {
                                        state.note = Some(format!("removed {name}"));
                                        action.reload = true;
                                    }
                                    Err(e) => state.error = Some(e.to_string()),
                                }
                                state.confirm_remove = None;
                            }
                            if ui.button("Cancel").clicked() {
                                state.confirm_remove = None;
                            }
                        } else if ui.button("🗑 Remove").clicked() {
                            state.confirm_remove = Some(entry.id.clone());
                        }
                        // Updating a copied package means re-copying it from
                        // wherever it came from. A linked one is already live.
                        if let Source::Folder(from) = &entry.source
                            && ui
                                .button("⟳ Update")
                                .on_hover_text(format!("re-copy from {from}"))
                                .clicked()
                        {
                            match floptle_package::install::install_from_dir(
                                ctx.project_root,
                                std::path::Path::new(from),
                                true,
                            ) {
                                Ok(e) => {
                                    state.note =
                                        Some(format!("updated to {}", e.version));
                                    action.reload = true;
                                }
                                Err(e) => state.error = Some(e.to_string()),
                            }
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
    }

// ---- Add -------------------------------------------------------------------

fn add_tab(ui: &mut egui::Ui, ctx: &PkgCtx<'_>, state: &mut PackagesState, action: &mut PackagesAction) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.heading("From a folder");
            ui.small(
                "Copies the package into this project, so a teammate who clones the project \
                 gets it too.",
            );
            ui.horizontal(|ui| {
                if ui.button("📂 Choose folder…").clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    match floptle_package::install::install_from_dir(
                        ctx.project_root,
                        &dir,
                        false,
                    ) {
                        Ok(e) => {
                            state.note = Some(format!("installed {} {}", e.id, e.version));
                            action.reload = true;
                        }
                        Err(e) => state.error = Some(e.to_string()),
                    }
                }
                if ui
                    .button("🔗 Link folder…")
                    .on_hover_text(
                        "read it where it is, without copying — what you use while WRITING a \
                         package, so every edit shows up on Reload",
                    )
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new().pick_folder()
                {
                    match floptle_package::install::link_dir(ctx.project_root, &dir, false) {
                        Ok(e) => {
                            state.note = Some(format!("linked {}", e.id));
                            action.reload = true;
                        }
                        Err(e) => state.error = Some(e.to_string()),
                    }
                }
            });

            ui.add_space(12.0);
            ui.heading("From a repository");
            ui.small("Needs Git on your PATH. The revision can be a branch, a tag or a commit.");
            egui::Grid::new("pkg_git").num_columns(2).show(ui, |ui| {
                ui.label("URL");
                ui.add(
                    egui::TextEdit::singleline(&mut state.git_url)
                        .hint_text("https://github.com/someone/their-package.git")
                        .desired_width(360.0),
                );
                ui.end_row();
                ui.label("Revision");
                ui.add(
                    egui::TextEdit::singleline(&mut state.git_rev)
                        .hint_text("(default branch)")
                        .desired_width(200.0),
                );
                ui.end_row();
                ui.label("Subfolder");
                ui.add(
                    egui::TextEdit::singleline(&mut state.git_subdir)
                        .hint_text("(the repository root)")
                        .desired_width(200.0),
                );
                ui.end_row();
            });
            let can_clone = !state.git_url.trim().is_empty();
            if ui.add_enabled(can_clone, egui::Button::new("⬇ Install from Git")).clicked() {
                let url = state.git_url.trim().to_string();
                let rev = non_empty(&state.git_rev);
                let sub = non_empty(&state.git_subdir);
                let scratch = std::env::temp_dir().join("floptle-package-clone");
                match floptle_package::install::install_from_git(
                    ctx.project_root,
                    &scratch,
                    &url,
                    rev.as_deref(),
                    sub.as_deref(),
                    false,
                ) {
                    Ok(e) => {
                        gate_remote_install(ctx.project_root, &e, state);
                        action.reload = true;
                    }
                    Err(e) => state.error = Some(e.to_string()),
                }
            }

            ui.add_space(12.0);
            ui.heading("Write one");
            ui.small(
                "Scaffolds a package inside this project with a manifest and an editor script \
                 that already draws something.",
            );
            egui::Grid::new("pkg_new").num_columns(2).show(ui, |ui| {
                ui.label("Id");
                ui.add(
                    egui::TextEdit::singleline(&mut state.new_id)
                        .hint_text("com.you.yourtool")
                        .desired_width(240.0),
                );
                ui.end_row();
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut state.new_name)
                        .hint_text("Your Tool")
                        .desired_width(240.0),
                );
                ui.end_row();
            });
            let id = state.new_id.trim().to_string();
            let name = state.new_name.trim().to_string();
            // The id rule is checked as it is typed, so the reason it is refused
            // is on screen before the button is pressed.
            if !id.is_empty()
                && let Err(e) = floptle_package::manifest::validate_id(&id)
            {
                ui.small(egui::RichText::new(e).color(egui::Color32::from_rgb(225, 195, 110)));
            }
            let ok = !id.is_empty()
                && !name.is_empty()
                && floptle_package::manifest::validate_id(&id).is_ok();
            if ui.add_enabled(ok, egui::Button::new("✚ New Package")).clicked() {
                match floptle_package::install::scaffold(ctx.project_root, &id, &name) {
                    Ok(_) => {
                        state.note = Some(format!("created packages/{id}"));
                        state.new_id.clear();
                        state.new_name.clear();
                        state.tab = Tab::Installed;
                        action.reload = true;
                    }
                    Err(e) => state.error = Some(e.to_string()),
                }
            }
        });
    }

// ---- Browse ----------------------------------------------------------------
//
// A **catalogue**, not a list. The registry started as editor extensions, where
// a name and a paragraph is a fair description of a package; it now has to hold
// texture kits, SFX libraries and model packs, and a texture kit described in
// words is a texture kit nobody installs.
//
// So: a grid of thumbnails, filtered by what a package *is* (the author's
// declared categories) and by what it demonstrably *holds* (facets derived from
// the files themselves — see `floptle_package::contents`). The second is the one
// people actually reach for: *I need SFX* is a different question from *show me
// what somebody filed under Audio*.
//
// Every counter respects the other filters, so a shelf that says 12 shows 12.

/// One grid cell's side, in points. Big enough that a piece of art is
/// recognisable, small enough that a screenful is a screenful.
const CELL: f32 = 168.0;

fn browse_tab(ui: &mut egui::Ui, ctx: &PkgCtx<'_>, state: &mut PackagesState, action: &mut PackagesAction) {
    // Poll the fetch before drawing, so a catalogue that arrived this frame
    // is shown this frame.
    if let Catalogue::Fetching(rx) = &state.catalogue
        && let Ok(result) = rx.try_recv()
    {
        state.catalogue = match result {
            Ok(i) => Catalogue::Ready(i),
            Err(e) => Catalogue::Failed(e),
        };
    }

    account_line(ui, ctx);
    // Listing is automatic, so this is not a shop with a buyer. Said once,
    // plainly, where somebody is choosing — not as a banner on every row,
    // which is how a warning becomes wallpaper.
    ui.small(
        "Packages here are made and managed by their authors, not by Fopull. Listing is \
         automatic and the checks are structural — nobody has vouched for what a package \
         does. A package is code that runs in your editor, so trust them at your own \
         discretion, and read the reviews.",
    );
    ui.add_space(4.0);

    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut state.search)
                .hint_text("search packages")
                .desired_width(220.0),
        );
        let fetching = matches!(state.catalogue, Catalogue::Fetching(_));
        if fetching {
            ui.spinner();
        } else if ui.button("⟲ Refresh").clicked() {
            let url = state.index_url().to_string();
            state.catalogue = Catalogue::Fetching(fetch_index(url));
        }
        ui.separator();
        ui.label("sort");
        // A registry that does not count downloads is entitled not to, and the
        // catalogue is careful to leave the field absent rather than publish a
        // zero for everything. Offering "most downloaded" against that is a
        // control that appears to work and reorders nothing — so it is offered
        // only when something in the catalogue actually carries a count.
        let counts_downloads = match &state.catalogue {
            Catalogue::Ready(i) => i.packages.iter().any(|p| p.downloads.is_some()),
            _ => false,
        };
        if !counts_downloads && state.sort == floptle_package::Sort::Downloads {
            state.sort = floptle_package::Sort::default();
        }
        egui::ComboBox::from_id_salt("pkg-sort")
            .selected_text(state.sort.label())
            .width(150.0)
            .show_ui(ui, |ui| {
                for s in floptle_package::Sort::ALL {
                    if *s == floptle_package::Sort::Downloads && !counts_downloads {
                        continue;
                    }
                    ui.selectable_value(&mut state.sort, *s, s.label());
                }
            });
        ui.checkbox(&mut state.compatible_only, "runs on this engine")
            .on_hover_text(
                "hide packages with no release for Floptle as it is here. Off by default: \
                 knowing something exists and needs a newer engine is more useful than not \
                 being shown it",
            );
    });

    ui.collapsing("Registry", |ui| {
        ui.horizontal(|ui| {
            ui.label("Catalogue");
            ui.add(
                egui::TextEdit::singleline(&mut state.index_url)
                    .hint_text(floptle_package::index::DEFAULT_INDEX_URL)
                    .desired_width(360.0),
            );
        });
        ui.small("Point this at your own catalogue to browse a private registry.");
    });
    ui.separator();

    match &state.catalogue {
        Catalogue::Idle => {
            ui.add_space(8.0);
            ui.label("Nothing fetched yet.");
            if ui.button("🌐 Load the catalogue").clicked() {
                let url = state.index_url().to_string();
                state.catalogue = Catalogue::Fetching(fetch_index(url));
            }
            return;
        }
        Catalogue::Fetching(_) => {
            ui.label("Fetching…");
            return;
        }
        Catalogue::Failed(e) => {
            ui.colored_label(egui::Color32::from_rgb(230, 120, 110), e.clone());
            ui.small(
                "Installing from a folder or a Git URL does not need the catalogue — the \
                 ✚ Add tab still works.",
            );
            return;
        }
        Catalogue::Ready(_) => {}
    }

    let engine = crate::Editor::engine_version();
    let query = state.query(&engine);
    // Cloned out of the catalogue so the grid can borrow `state` mutably while
    // drawing. A catalogue is small and this is once per frame.
    let (rows, cat_counts, facet_counts, total) = match &state.catalogue {
        Catalogue::Ready(idx) => (
            idx.query(&query)
                .into_iter()
                .map(|l| (l.clone(), l.best_for(&engine).cloned()))
                .collect::<Vec<_>>(),
            idx.category_counts(&query),
            idx.facet_counts(&query),
            idx.packages.len(),
        ),
        _ => (Vec::new(), Vec::new(), Vec::new(), 0),
    };

    filter_bar(ui, state, &cat_counts, &facet_counts);
    ui.horizontal(|ui| {
        ui.small(format!("{} of {total}", rows.len()));
        if state.has_filters() && ui.small_button("clear filters").clicked() {
            state.clear_filters();
        }
    });
    ui.add_space(2.0);

    if rows.is_empty() {
        ui.add_space(12.0);
        ui.label("Nothing matches.");
        if state.has_filters() {
            ui.small("Try clearing a filter — the counts above show what each one would leave.");
        }
        return;
    }

    let installed = Registry::load(ctx.project_root).unwrap_or_default();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // A grid that reflows: as many columns as fit, recomputed per frame, so
        // the browser works docked narrow beside the viewport and wide across
        // the window without a setting for it.
        let per_row = ((ui.available_width() / (CELL + 8.0)).floor() as usize).max(1);
        for chunk in rows.chunks(per_row) {
            ui.horizontal_top(|ui| {
                for (listing, best) in chunk {
                    grid_cell(ui, ctx, state, action, listing, best, &installed, &engine);
                }
            });
            ui.add_space(6.0);
        }
        // The detail panel for whichever cell is open, under the grid rather
        // than in a window — the grid stays visible, so comparing two packages
        // does not mean closing one to see the other.
        if let Some(id) = state.expanded.clone()
            && let Some((listing, best)) = rows.iter().find(|(l, _)| l.id == id)
        {
            ui.separator();
            detail_panel(ui, ctx, state, action, listing, best, &installed, &engine);
        }
    });
}

/// The category and contains chips, each carrying how many rows it would leave.
fn filter_bar(
    ui: &mut egui::Ui,
    state: &mut PackagesState,
    cats: &[(floptle_package::Category, usize)],
    facets: &[(floptle_package::Facet, usize)],
) {
    let chip = |ui: &mut egui::Ui, label: String, on: bool, n: usize| -> bool {
        // A shelf nobody is on is shown greyed rather than hidden: a filter row
        // that changes shape as you type is a filter row you cannot aim at.
        ui.add_enabled_ui(n > 0 || on, |ui| ui.selectable_label(on, label).clicked()).inner
    };
    ui.horizontal_wrapped(|ui| {
        for (c, n) in cats {
            let on = state.categories.contains(c);
            if chip(ui, format!("{} {} {n}", c.glyph(), c.label()), on, *n) {
                toggle(&mut state.categories, *c);
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.small("has");
        for (f, n) in facets {
            let on = state.contains.contains(f);
            if chip(ui, format!("{} {n}", f.label()), on, *n) {
                toggle(&mut state.contains, *f);
            }
        }
    });
}

fn toggle<T: PartialEq + Copy>(list: &mut Vec<T>, v: T) {
    match list.iter().position(|x| *x == v) {
        Some(i) => {
            list.remove(i);
        }
        None => list.push(v),
    }
}

/// One package, as a picture with a name under it.
#[allow(clippy::too_many_arguments)]
fn grid_cell(
    ui: &mut egui::Ui,
    ctx: &PkgCtx<'_>,
    state: &mut PackagesState,
    action: &mut PackagesAction,
    listing: &Listing,
    best: &Option<floptle_package::Release>,
    installed: &Registry,
    engine: &floptle_package::Version,
) {
    let open = state.expanded.as_deref() == Some(listing.id.as_str());
    let have = installed.find(&listing.id);
    ui.allocate_ui(egui::vec2(CELL, CELL + 76.0), |ui| {
        egui::Frame::group(ui.style())
            .fill(if open {
                ui.style().visuals.selection.bg_fill.gamma_multiply(0.35)
            } else {
                ui.style().visuals.faint_bg_color
            })
            .show(ui, |ui| {
                ui.set_width(CELL);
                thumbnail(ui, state, listing);
                ui.horizontal(|ui| {
                    ui.strong(crate::assets::truncate_label(&listing.name, 20));
                    if have.is_some() {
                        ui.small("✔").on_hover_text("installed in this project");
                    }
                });
                if !listing.author.is_empty() {
                    ui.small(format!("by {}", listing.author));
                }
                ui.horizontal(|ui| match &listing.rating {
                    Some(r) => {
                        ui.small(stars(r.score));
                        ui.small(format!("{}", r.count));
                    }
                    // Not an empty star row: "nobody has said yet" and
                    // "everybody hated it" must not look alike.
                    None => {
                        ui.small(egui::RichText::new("no reviews yet").weak());
                    }
                });
                ui.horizontal(|ui| {
                    if ui.small_button(if open { "▾ close" } else { "▸ details" }).clicked() {
                        state.expanded = if open { None } else { Some(listing.id.clone()) };
                        state.reviews_for = None;
                    }
                    install_button(ui, ctx, state, action, best, have, engine, true);
                });
            });
    });
}

/// The picture, or an honest placeholder.
///
/// A package with no thumbnail gets its category's glyph on a flat tile rather
/// than a broken-image frame — it reads as "this one has no picture", which is
/// true, instead of as "something went wrong", which is not.
fn thumbnail(ui: &mut egui::Ui, state: &mut PackagesState, listing: &Listing) {
    let side = CELL - 12.0;
    let tex = listing
        .thumbnail
        .as_deref()
        .and_then(|src| state.thumbs.get(ui.ctx(), src, None, crate::pkg_thumbs::Detail::Grid))
        .cloned();
    match tex {
        Some(tex) => {
            ui.add(
                egui::Image::new(&tex)
                    .fit_to_exact_size(egui::vec2(side, side))
                    .corner_radius(4.0),
            );
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::hover());
            ui.painter().rect_filled(rect, 4.0, ui.style().visuals.extreme_bg_color);
            let glyph = listing
                .categories
                .first()
                .map(|c| c.glyph())
                .unwrap_or("▣");
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                glyph,
                egui::FontId::proportional(side * 0.35),
                ui.style().visuals.weak_text_color(),
            );
            // Still coming, or never coming — and which of those it is matters
            // most to the author of the package, who is the one person who can
            // fix a `thumbnail:` that points at nothing.
            if let Some(src) = listing.thumbnail.as_deref() {
                let failed = state.thumbs.failure(src, None).map(str::to_string);
                let (mark, hint) = match &failed {
                    Some(why) => ("✖", format!("this package's picture could not be shown — {why}")),
                    None => ("…", "fetching this package's picture".into()),
                };
                ui.painter().text(
                    rect.center_bottom() - egui::vec2(0.0, 10.0),
                    egui::Align2::CENTER_CENTER,
                    mark,
                    egui::FontId::proportional(12.0),
                    ui.style().visuals.weak_text_color(),
                );
                ui.interact(rect, ui.id().with(("thumb", &listing.id)), egui::Sense::hover())
                    .on_hover_text(hint);
            }
        }
    }
}

/// Everything about one package, under the grid.
#[allow(clippy::too_many_arguments)]
fn detail_panel(
    ui: &mut egui::Ui,
    ctx: &PkgCtx<'_>,
    state: &mut PackagesState,
    action: &mut PackagesAction,
    listing: &Listing,
    best: &Option<floptle_package::Release>,
    installed: &Registry,
    engine: &floptle_package::Version,
) {
    let have = installed.find(&listing.id);
    ui.horizontal(|ui| {
        ui.heading(&listing.name);
        ui.small(egui::RichText::new(&listing.id).monospace());
    });
    ui.horizontal_wrapped(|ui| {
        if !listing.author.is_empty() {
            ui.small(format!("by {}", listing.author));
        }
        for c in &listing.categories {
            ui.small(format!("{} {}", c.glyph(), c.label()));
        }
        if let Some(d) = &listing.updated {
            ui.small(format!("updated {d}"));
        }
        // Absent is not zero — a registry that does not count installs must not
        // make every package on it look like one nobody has ever installed.
        if let Some(n) = listing.downloads {
            ui.small(format!("{n} installs"));
        }
    });
    if !listing.description.is_empty() {
        ui.label(&listing.description);
    }
    // What it holds. For an installed package this is counted off the files on
    // this disk — "5 models · 4 audio" says something a list of shelf names
    // does not, and it is the answer to "what did I actually just install".
    // For one that is only listed, the catalogue reports which facets are
    // present but not how many, so the labels stand alone rather than inventing
    // a number.
    let held = have
        .map(|e| e.root_in(ctx.project_root))
        .and_then(|root| {
            floptle_package::Manifest::load(&root)
                .ok()
                .map(|m| floptle_package::contents::Contents::scan(&m, &root))
        })
        .filter(|c| !c.is_empty());
    match &held {
        Some(c) => {
            ui.small(
                c.facets()
                    .into_iter()
                    .map(|(f, n)| format!("{n} {}", f.label().to_lowercase()))
                    .collect::<Vec<_>>()
                    .join(" · "),
            );
        }
        None if !listing.contains.is_empty() => {
            ui.small(
                listing.contains.iter().map(|f| f.label()).collect::<Vec<_>>().join(" · "),
            );
        }
        None => {}
    }

    // An installed package's pictures are on this disk, so they are read from
    // there rather than fetched: it is faster, it works with no network, and it
    // is the only way an author sees their own gallery before it is published.
    let media_base = have.map(|e| e.root_in(ctx.project_root));

    // The gallery. Videos are a link out — the editor is not a video player and
    // pretending otherwise is a worse experience than a browser tab.
    if !listing.media.is_empty() {
        ui.add_space(4.0);
        egui::ScrollArea::horizontal().id_salt("pkg-media").show(ui, |ui| {
            ui.horizontal(|ui| {
                for m in &listing.media {
                    ui.vertical(|ui| {
                        let tex = m
                            .still()
                            .and_then(|s| {
                                state.thumbs.get(
                                    ui.ctx(),
                                    s,
                                    media_base.as_deref(),
                                    crate::pkg_thumbs::Detail::Gallery,
                                )
                            })
                            .cloned();
                        match tex {
                            Some(tex) => {
                                let r = ui.add(
                                    egui::Image::new(&tex)
                                        .fit_to_exact_size(egui::vec2(220.0, 124.0))
                                        .corner_radius(4.0)
                                        .sense(egui::Sense::click()),
                                );
                                if r.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                // A gallery is for looking at, and a 220px strip
                                // is not looking at anything. Clicking a still
                                // opens it as big as the window allows; a video
                                // goes to a browser, which is where video lives.
                                if r.clicked() {
                                    match &m.video {
                                        Some(url) => {
                                            let _ = floptle_script::open_in_browser(url);
                                        }
                                        None => {
                                            if let Some(src) = m.still() {
                                                state.lightbox = Some((
                                                    src.to_string(),
                                                    m.caption.clone(),
                                                    media_base.clone(),
                                                ));
                                            }
                                        }
                                    }
                                }
                                r.on_hover_text(if m.is_video() {
                                    "watch in your browser"
                                } else {
                                    "click to see it full size"
                                });
                            }
                            None => {
                                ui.allocate_exact_size(
                                    egui::vec2(220.0, 124.0),
                                    egui::Sense::hover(),
                                );
                            }
                        }
                        if let Some(url) = &m.video
                            && ui.small_button("▶ watch").clicked()
                        {
                            let _ = floptle_script::open_in_browser(url);
                        }
                        if !m.caption.is_empty() {
                            ui.small(crate::assets::truncate_label(&m.caption, 34));
                        }
                    });
                }
            });
        });
    }

    ui.add_space(4.0);
    ui.horizontal(|ui| {
        install_button(ui, ctx, state, action, best, have, engine, false);
        if ui.small_button("open page ↗").clicked() {
            let _ = floptle_script::open_in_browser(
                &floptle_package::index::package_page_for(&listing.id),
            );
        }
        if let Some(home) = &listing.homepage
            && ui.small_button("repository ↗").clicked()
        {
            let _ = floptle_script::open_in_browser(home);
        }
    });

    // Which versions exist, and which one this engine would take. A row that
    // offers a version the editor will refuse on install wastes an afternoon.
    if !listing.versions.is_empty() {
        ui.collapsing("Versions", |ui| {
            for r in {
                let mut v = listing.versions.clone();
                v.sort_by(|a, b| b.version.cmp(&a.version));
                v
            } {
                ui.horizontal(|ui| {
                    ui.small(egui::RichText::new(r.version.to_string()).monospace());
                    match &r.engine {
                        Some(req) if !req.matches(engine) => {
                            ui.small(
                                egui::RichText::new(format!("needs {}", req.as_str()))
                                    .color(egui::Color32::from_rgb(225, 195, 110)),
                            );
                        }
                        Some(req) => {
                            ui.small(format!("needs {}", req.as_str()));
                        }
                        None => {
                            ui.small("any engine");
                        }
                    }
                    if let Some(rev) = &r.rev {
                        ui.small(egui::RichText::new(rev).monospace().weak());
                    }
                });
            }
        });
    }

    ui.separator();
    // The gate Ty asked for: you may review what you have actually run.
    // Installed AND enabled — a package sitting there switched off has not been
    // tried.
    let mine = reviewable_version(have);
    if state.reviews_for.as_deref() != Some(listing.id.as_str()) {
        state.reviews_for = Some(listing.id.clone());
        state.reviews = ReviewsState::Idle;
    }
    reviews_section(ui, ctx, state, &listing.id, mine);

    lightbox(ui, state);
}

/// One gallery image, as big as the window allows.
///
/// Drawn as a window rather than inline because it has to sit over the panel it
/// was opened from — the alternative is a strip that grows and shoves the rest
/// of the page around, which is the thing the layout is not allowed to do.
fn lightbox(ui: &mut egui::Ui, state: &mut PackagesState) {
    let Some((src, caption, base)) = state.lightbox.clone() else { return };
    let screen = ui.ctx().input(|i| i.content_rect());
    let mut open = true;
    egui::Window::new("gallery")
        .title_bar(false)
        .resizable(false)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ui.ctx(), |ui| {
            let tex = state
                .thumbs
                .get(ui.ctx(), &src, base.as_deref(), crate::pkg_thumbs::Detail::Gallery)
                .cloned();
            match tex {
                Some(tex) => {
                    // Fit inside the window, never past it, and never enlarged
                    // past its own pixels — an upscaled screenshot looks worse
                    // than a small one.
                    let size = tex.size_vec2();
                    let room = egui::vec2(screen.width() * 0.86, screen.height() * 0.8);
                    let k = (room.x / size.x).min(room.y / size.y).min(1.0);
                    ui.add(egui::Image::new(&tex).fit_to_exact_size(size * k).corner_radius(4.0));
                }
                None => {
                    ui.spinner();
                }
            }
            ui.horizontal(|ui| {
                if !caption.is_empty() {
                    ui.small(&caption);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✖ close").clicked() {
                        open = false;
                    }
                });
            });
        });
    // Escape as well as the button, because a picture filling the screen with
    // one small ✖ on it is a thing people press Escape at.
    let escaped = ui.ctx().input(|i| i.key_pressed(egui::Key::Escape));
    if !open || escaped {
        state.lightbox = None;
    }
}

/// Install / Update / why neither is offered.
#[allow(clippy::too_many_arguments)]
fn install_button(
    ui: &mut egui::Ui,
    ctx: &PkgCtx<'_>,
    state: &mut PackagesState,
    action: &mut PackagesAction,
    best: &Option<floptle_package::Release>,
    have: Option<&floptle_package::Entry>,
    engine: &floptle_package::Version,
    small: bool,
) {
    match (best, have) {
        (None, _) => {
            // Said plainly, because "why is the button greyed out" is a
            // question nobody should have to ask.
            let msg = format!("nothing for Floptle {engine} yet");
            if small {
                ui.small(egui::RichText::new("—").weak()).on_hover_text(msg);
            } else {
                ui.small(msg);
            }
        }
        (Some(r), Some(entry)) if entry.version >= r.version => {
            ui.small(format!("installed ({})", entry.version));
        }
        (Some(r), have) => {
            let label = if have.is_some() { "⟳ Update" } else { "⬇ Install" };
            let hit = if small {
                ui.small_button(label).clicked()
            } else {
                ui.button(label).clicked()
            };
            if hit {
                let scratch = std::env::temp_dir().join("floptle-package-clone");
                match floptle_package::install::install_from_git(
                    ctx.project_root,
                    &scratch,
                    &r.git,
                    r.rev.as_deref(),
                    r.subdir.as_deref(),
                    true,
                ) {
                    Ok(e) => {
                        gate_remote_install(ctx.project_root, &e, state);
                        action.reload = true;
                    }
                    Err(e) => {
                        state.error = Some(e.to_string());
                    }
                }
            }
        }
    }
}

/// A package installed from a Git remote — the catalogue or a URL somebody was
/// given — is code from a stranger, and enabling it runs that code. If it asks
/// for anything beyond its own folder, it lands **installed but not enabled**
/// with what it asked for on screen, and a person turns it on.
///
/// A package that declares no permissions is enabled as before: it can read its
/// own folder and nothing else, which is the same standing every built-in tool
/// already has, and a confirmation nobody can act on teaches people to click
/// through the ones that matter.
///
/// Listing on fopull.com is automatic once a submission passes its checks, so
/// this is the last point at which anybody looks. That is the whole reason it
/// exists.
fn gate_remote_install(
    project_root: &std::path::Path,
    entry: &floptle_package::Entry,
    state: &mut PackagesState,
) {
    let asked = floptle_package::Manifest::load(&entry.root_in(project_root))
        .map(|m| m.permissions)
        .unwrap_or_default();
    if asked.is_empty() {
        state.note = Some(format!("installed {} {}", entry.id, entry.version));
        return;
    }
    match floptle_package::install::set_enabled(project_root, &entry.id, false) {
        Ok(()) => {
            state.awaiting_consent = Some(entry.id.clone());
            state.expanded = Some(entry.id.clone());
            state.note = Some(format!(
                "installed {} {} — not running yet, it asked for something",
                entry.id, entry.version
            ));
        }
        // Could not disable it, so do not pretend it is gated.
        Err(e) => state.error = Some(e.to_string()),
    }
}

/// What a package is allowed to do, in one line, in the words of somebody
/// deciding whether to trust it.
fn permissions_line(ui: &mut egui::Ui, perms: &[Permission]) {
    if perms.is_empty() {
        ui.small("Asks for nothing: no network, no files outside its own folder.");
        return;
    }
    ui.small(egui::RichText::new("This package can:").strong());
    for p in perms {
        ui.small(format!("• {}", p.describe()));
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// The version you may review, or `None` if you may not.
///
/// **Installed AND enabled.** Not installed is obvious; installed but switched
/// off is the interesting one — a package sitting there disabled has not been
/// tried, and since v0.55.3 that is exactly the state a package arrives in when
/// it asks for a permission. Reviewing from that state would mean reviewing
/// something that never ran.
///
/// The version comes back with it because it is the one fact the editor knows
/// for certain about a review.
fn reviewable_version(entry: Option<&floptle_package::Entry>) -> Option<String> {
    entry.filter(|e| e.enabled).map(|e| e.version.to_string())
}

/// Five characters that say a score at a glance. Half a star is the nearest
/// honest rounding — 4.3 is not 4, and pretending otherwise loses the thing the
/// number was for.
fn stars(score: f32) -> String {
    let halves = (score.clamp(0.0, 5.0) * 2.0).round() as i32;
    let full = (halves / 2) as usize;
    let half = halves % 2 == 1;
    let mut out = "★".repeat(full);
    if half {
        // A half-filled disc rather than a half star: U+2BE8 (half star) is in
        // none of the bundled fonts and drew as an empty box, which the glyph
        // coverage test caught. This one the editor already uses elsewhere.
        out.push('◐');
    }
    out.push_str(&"☆".repeat(5usize.saturating_sub(full + usize::from(half))));
    out
}

/// Who you are, shared with the Hub. Signing in anywhere signs you in
/// everywhere, so most of the time this line just says your name and the
/// account was never a step you had to think about.
fn account_line(ui: &mut egui::Ui, ctx: &PkgCtx<'_>) {
    let Some(account) = ctx.account else { return };
    ui.horizontal(|ui| match account.phase() {
        floptle_account::Phase::SignedIn => {
            let who = account
                .session()
                .and_then(|s| s.name.or(s.email))
                .unwrap_or_else(|| "your account".into());
            ui.small(format!("signed in as {who}"));
            if ui.small_button("sign out").clicked() {
                account.sign_out();
            }
        }
        floptle_account::Phase::Waiting { user_code, url, .. } => {
            ui.small(format!("enter {user_code} at"));
            if ui.link(&url).clicked() {
                let _ = floptle_script::open_in_browser(&url);
            }
            if ui.small_button("cancel").clicked() {
                account.cancel_sign_in();
            }
        }
        floptle_account::Phase::Starting => {
            ui.spinner();
            ui.small("signing in…");
        }
        floptle_account::Phase::SignedOut | floptle_account::Phase::Failed(_) => {
            ui.small("not signed in");
            if ui.small_button("sign in").on_hover_text(
                "the same account as the Hub — signing in here signs you in there too",
            ).clicked() {
                account.sign_in();
            }
        }
    });
}

/// What people who ran this package thought, and — if you are one of them — the
/// box to say so yourself.
fn reviews_section(
    ui: &mut egui::Ui,
    ctx: &PkgCtx<'_>,
    state: &mut PackagesState,
    id: &str,
    installed_version: Option<String>,
) {
    if let ReviewsState::Fetching(rx) = &state.reviews
        && let Ok(result) = rx.try_recv()
    {
        state.reviews = match result {
            Ok(r) => ReviewsState::Ready(r),
            Err(e) => ReviewsState::Failed(e),
        };
    }
    if matches!(state.reviews, ReviewsState::Idle) {
        let url = floptle_package::index::reviews_url_for(state.index_url(), id);
        state.reviews = ReviewsState::Fetching(fetch_reviews(url));
    }

    match &state.reviews {
        ReviewsState::Fetching(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.small("reading reviews…");
            });
        }
        ReviewsState::Failed(e) => {
            ui.small(format!("no reviews to show — {e}"));
        }
        ReviewsState::Ready(r) if r.reviews.is_empty() => {
            ui.small("Nobody has reviewed this yet.");
        }
        ReviewsState::Ready(r) => {
            egui::ScrollArea::vertical().max_height(160.0).id_salt(("reviews", id)).show(ui, |ui| {
                for review in &r.reviews {
                    ui.horizontal(|ui| {
                        ui.small(stars(review.rating as f32));
                        ui.small(&review.author);
                        // The version is the part the editor knew for certain,
                        // and a review of 1.0.0 is not a review of 3.0.0.
                        if !review.version.is_empty() {
                            ui.small(format!("· v{}", review.version));
                        }
                        if review.edited {
                            ui.small("· edited");
                        }
                    });
                    if !review.body.is_empty() {
                        ui.label(&review.body);
                    }
                    ui.add_space(4.0);
                }
            });
        }
        ReviewsState::Idle => {}
    }

    ui.add_space(4.0);
    let Some(version) = installed_version else {
        ui.small("Install and enable this package to review it, then the button appears here.");
        return;
    };

    // Whether one of the reviews on screen is yours. The reviews document
    // already carries an author, so this costs nothing and needs no endpoint —
    // but the server is the authority on whose review is whose, so this is a
    // read of what is published rather than a claim about your account.
    let me = ctx.account.and_then(|a| a.session()).and_then(|s| s.name.or(s.email));
    let mine = match (&state.reviews, me) {
        (ReviewsState::Ready(r), Some(name)) => {
            r.reviews.iter().find(|v| v.author == name).cloned()
        }
        _ => None,
    };

    // Reviewing happens on the site, and the editor's job is to get you there
    // with the package already picked. Writing a review means signing in,
    // agreeing to what you are publishing under your name, and being told when
    // something is refused — a browser does all three properly, and it is the
    // one place the account already lives.
    //
    // Not gated on being signed in HERE: the site will ask if it has to, and a
    // button that refuses to open until you sign in twice is the friction this
    // replaced.
    ui.horizontal(|ui| {
        let (label, hint) = match &mine {
            Some(_) => (
                "★  Edit your review",
                "opens fopull.com, where you can change or remove it",
            ),
            None => (
                "★  Write a review",
                "opens fopull.com to review this package — it appears here once it is posted",
            ),
        };
        if ui.button(label).on_hover_text(hint).clicked() {
            let _ = floptle_script::open_in_browser(&floptle_package::index::review_page_for(id));
            // Whatever gets posted over there is not in the copy fetched over
            // here, so the next look asks again rather than showing a document
            // that predates the review someone just wrote.
            state.reviews = ReviewsState::Idle;
        }
        if ui
            .small_button("open page ↗")
            .on_hover_text("this package on fopull.com — every version and every review")
            .clicked()
        {
            let _ = floptle_script::open_in_browser(&floptle_package::index::package_page_for(id));
        }
    });
    match &mine {
        Some(v) if v.version == version => {
            ui.small(format!("you reviewed v{version}"));
        }
        // A review of 1.0.0 says very little about the 3.0.0 in the project,
        // and that is worth saying to the person who wrote it too.
        Some(v) if !v.version.is_empty() => {
            ui.small(format!("you reviewed v{} — you have v{version} now", v.version));
        }
        Some(_) => {
            ui.small("you reviewed this");
        }
        None => {
            ui.small(format!("you have v{version} installed and enabled"));
        }
    }
}

fn fetch_reviews(url: String) -> Receiver<Result<floptle_package::index::Reviews, String>> {
    let (tx, rx): (Sender<Result<floptle_package::index::Reviews, String>>, _) =
        std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .get(&url)
            .call()
            .map_err(|e| format!("could not reach the reviews: {e}"))
            .and_then(|r| r.into_string().map_err(|e| format!("the reviews would not read: {e}")))
            .and_then(|s| floptle_package::index::Reviews::parse(&s));
        let _ = tx.send(result);
    });
    rx
}

/// Fetch the catalogue on a worker thread.
fn fetch_index(url: String) -> Receiver<Result<Index, String>> {
    let (tx, rx): (Sender<Result<Index, String>>, _) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = ureq::AgentBuilder::new()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .get(&url)
            .call()
            .map_err(|e| format!("could not reach the package catalogue: {e}"))
            .and_then(|r| {
                r.into_string().map_err(|e| format!("the catalogue would not read: {e}"))
            })
            .and_then(|s| Index::parse(&s));
        let _ = tx.send(result);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_catalogue_url_falls_back_to_the_default() {
        let mut s = PackagesState::default();
        assert_eq!(s.index_url(), floptle_package::index::DEFAULT_INDEX_URL);
        s.index_url = "  ".into();
        assert_eq!(s.index_url(), floptle_package::index::DEFAULT_INDEX_URL);
        s.index_url = " https://example.com/i.json ".into();
        assert_eq!(s.index_url(), "https://example.com/i.json");
    }

    #[test]
    fn blank_boxes_read_as_absent_rather_than_as_empty_strings() {
        assert_eq!(non_empty("  "), None);
        assert_eq!(non_empty(" v1.0 "), Some("v1.0".to_string()));
    }
}

#[cfg(test)]
mod consent_tests {
    use super::*;

    /// A score has to survive the trip to five characters, because for most
    /// people the glyphs ARE the score — they never read the number.
    #[test]
    fn a_score_reads_the_same_as_stars_as_it_does_as_a_number() {
        assert_eq!(stars(5.0).chars().filter(|c| *c == '★').count(), 5);
        assert_eq!(stars(0.0).chars().filter(|c| *c == '☆').count(), 5);
        // 4.3 rounds to four and a half, not four — losing the half loses the
        // only part of the number a glance was going to get.
        assert_eq!(stars(4.3), "★★★★◐");
        assert_eq!(stars(4.0), "★★★★☆");
        // Always five glyphs wide, or a list of them will not line up.
        for s in [0.0, 1.2, 2.5, 3.7, 4.9, 5.0] {
            assert_eq!(stars(s).chars().count(), 5, "{s}");
        }
        // Nonsense from a server does not make a ragged row.
        assert_eq!(stars(-1.0).chars().count(), 5);
        assert_eq!(stars(99.0).chars().count(), 5);
    }

    /// You may review what you have actually run. Installed-but-disabled is the
    /// case that matters: since v0.55.3 that is how a package asking for a
    /// permission arrives, and it has not run.
    #[test]
    fn only_an_installed_and_enabled_package_can_be_reviewed() {
        let (root, entry) = install("reviewable", "com.test.rev", "");
        let project = root.join("project");
        let reg = Registry::load(&project).unwrap();
        assert_eq!(reviewable_version(reg.find("com.test.rev")), Some("1.0.0".to_string()));

        floptle_package::install::set_enabled(&project, "com.test.rev", false).unwrap();
        let off = Registry::load(&project).unwrap();
        assert_eq!(reviewable_version(off.find("com.test.rev")), None);

        assert_eq!(reviewable_version(None), None);
        let _ = std::fs::remove_dir_all(&root);
        let _ = entry;
    }

    /// Install a package from a folder, exactly as the editor does, and hand
    /// back what the gate was given.
    fn install(tag: &str, id: &str, perms: &str) -> (PathBuf, floptle_package::Entry) {
        let root = std::env::temp_dir().join(format!("flop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("src");
        let _ = std::fs::create_dir_all(src.join("editor"));
        let _ = std::fs::create_dir_all(root.join("project"));
        let _ = std::fs::write(
            src.join("package.ron"),
            format!(
                "(\n  id: \"{id}\",\n  name: \"T\",\n  version: \"1.0.0\",{perms}\n)\n"
            ),
        );
        let _ = std::fs::write(src.join("editor/main.lua"), "-- nothing\n");
        let entry =
            floptle_package::install::install_from_dir(&root.join("project"), &src, true).unwrap();
        (root, entry)
    }

    /// Listing on the catalogue is automatic, so this is the last point at which
    /// anybody looks at what a package wants. A package that asks for something
    /// must not be running before somebody has been shown what it asked for.
    #[test]
    fn a_remote_package_that_asks_for_something_does_not_run_until_allowed() {
        let (root, entry) = install("consent-asks", "com.test.asks", "\n  permissions: [Network, Files],");
        let project = root.join("project");
        let mut state = PackagesState::default();

        // As installed, before the gate: enabled, and one reload away from
        // running. That is the thing being prevented.
        assert!(entry.enabled);

        gate_remote_install(&project, &entry, &mut state);

        let reg = Registry::load(&project).unwrap();
        assert!(
            !reg.find("com.test.asks").unwrap().enabled,
            "a package that asked for something must not be left running"
        );
        assert_eq!(state.awaiting_consent.as_deref(), Some("com.test.asks"));
        // The row is opened, because a warning about something you cannot see is
        // not a warning.
        assert_eq!(state.expanded.as_deref(), Some("com.test.asks"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A package that asks for nothing can only read its own folder, and a
    /// confirmation nobody can act on teaches people to click through the ones
    /// that matter.
    #[test]
    fn a_package_that_asks_for_nothing_is_not_gated() {
        let (root, entry) = install("consent-none", "com.test.quiet", "");
        let project = root.join("project");
        let mut state = PackagesState::default();

        gate_remote_install(&project, &entry, &mut state);

        assert!(Registry::load(&project).unwrap().find("com.test.quiet").unwrap().enabled);
        assert_eq!(state.awaiting_consent, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The manifest has to be readable with the package switched OFF — a
    /// disabled package is never loaded by the host, so the consent block cannot
    /// ask the host what it wants.
    #[test]
    fn what_it_asks_for_is_readable_while_it_is_disabled() {
        let (root, entry) = install("consent-read", "com.test.off", "\n  permissions: [Browser],");
        let project = root.join("project");
        let m = floptle_package::Manifest::load(&entry.root_in(&project)).unwrap();
        assert_eq!(m.permissions, vec![Permission::Browser]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
