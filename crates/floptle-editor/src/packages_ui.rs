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

/// What the window needs from the editor: where the project is, and what the
/// last package load found. Passed in rather than reached for, because this
/// draws inside the editor's UI pass, where `&mut Editor` does not exist.
pub(crate) struct PkgCtx<'a> {
    pub(crate) project_root: &'a std::path::Path,
    pub(crate) host: &'a crate::ext::ExtHost,
    /// The signed-in account, shared with the Hub. `None` only before it has
    /// been built — the window degrades to "cannot review right now" rather
    /// than hiding the reviews it can still read.
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
    /// The review being written: a rating out of five and optional words.
    draft_rating: u32,
    draft_body: String,
    /// An in-flight post, and whatever the server last said about one.
    posting: Option<Receiver<(u64, floptle_account::CloudReply)>>,
    post_note: Option<String>,
    /// A package that arrived from somewhere else and asked for something. It is
    /// installed but NOT enabled until the person who installed it has seen what
    /// it wants — see `gate_remote_install`.
    awaiting_consent: Option<String>,
}

impl PackagesState {
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

/// Draw the 📦 Packages window. Returns what the editor should do next.
pub(crate) fn window(
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
        let problems: Vec<(Option<String>, Severity, String)> = ctx
            .host
            .report
            .problems
            .iter()
            .map(|p| (p.id.clone(), p.severity, p.message.clone()))
            .collect();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for entry in &reg.packages {
                let loaded = ctx.host.report.find(&entry.id).cloned();
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
                    for (id, sev, msg) in &problems {
                        if id.as_deref() == Some(entry.id.as_str()) {
                            let col = match sev {
                                Severity::Error => egui::Color32::from_rgb(230, 120, 110),
                                Severity::Warning => egui::Color32::from_rgb(225, 195, 110),
                            };
                            ui.colored_label(col, msg);
                        }
                    }
                    if let Some(p) = ctx.host.packages.iter().find(|p| p.id == entry.id)
                        && let Some(err) = &p.failed
                    {
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

        // Land a review that finished posting.
        if let Some(rx) = &state.posting
            && let Ok((_, reply)) = rx.try_recv()
        {
            state.posting = None;
            state.post_note = Some(match (&reply.error, reply.status) {
                (Some(e), _) => format!("could not post the review: {e}"),
                (None, s) if (200..300).contains(&s) => {
                    state.draft_body.clear();
                    state.draft_rating = 0;
                    // Whatever is on screen predates this review.
                    state.reviews = ReviewsState::Idle;
                    "thanks — your review is posted".to_string()
                }
                (None, s) => format!("the server said {s}: {}", reply.body),
            });
        }

        account_line(ui, ctx, state);
        // Listing is automatic, so this is not a shop with a buyer. Said once,
        // plainly, where somebody is choosing — not as a banner on every row,
        // which is how a warning becomes wallpaper.
        ui.small(
            "Packages here are made and managed by their authors, not by Fopull. Listing is              automatic and the checks are structural — nobody has vouched for what a package              does. A package is code that runs in your editor, so trust them at your own              discretion, and read the reviews.",
        );
        ui.add_space(4.0);

        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut state.search)
                    .hint_text("search packages")
                    .desired_width(240.0),
            );
            let fetching = matches!(state.catalogue, Catalogue::Fetching(_));
            if fetching {
                ui.spinner();
            } else if ui.button("⟲ Refresh").clicked() {
                let url = state.index_url().to_string();
                state.catalogue = Catalogue::Fetching(fetch_index(url));
            }
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
        let installed = Registry::load(ctx.project_root).unwrap_or_default();
        // Cloned out so the rows can borrow `self` mutably while drawing.
        let rows: Vec<(Listing, Option<floptle_package::Release>)> =
            match &state.catalogue {
                Catalogue::Ready(idx) => idx
                    .search(&state.search)
                    .into_iter()
                    .map(|l| (l.clone(), l.best_for(&engine).cloned()))
                    .collect(),
                _ => Vec::new(),
            };
        if rows.is_empty() {
            ui.label("Nothing matches.");
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for (listing, best) in rows {
                let have = installed.find(&listing.id);
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(&listing.name);
                        if let Some(r) = &best {
                            ui.small(r.version.to_string());
                        }
                        if !listing.author.is_empty() {
                            ui.small(format!("by {}", listing.author));
                        }
                    });
                    if !listing.description.is_empty() {
                        ui.label(&listing.description);
                    }
                    ui.horizontal(|ui| {
                        match &listing.rating {
                            Some(r) => {
                                ui.label(stars(r.score));
                                ui.small(format!(
                                    "{:.1} · {} review{}",
                                    r.score,
                                    r.count,
                                    if r.count == 1 { "" } else { "s" }
                                ));
                            }
                            // Not an empty star row: "nobody has said yet" and
                            // "everybody hated it" must not look alike.
                            None => {
                                ui.small("no reviews yet");
                            }
                        }
                        let open = state.reviews_for.as_deref() == Some(listing.id.as_str());
                        if ui.small_button(if open { "▾ reviews" } else { "▸ reviews" }).clicked() {
                            if open {
                                state.reviews_for = None;
                            } else {
                                state.reviews_for = Some(listing.id.clone());
                                state.reviews = ReviewsState::Idle;
                                state.post_note = None;
                            }
                        }
                    });
                    ui.horizontal(|ui| match (&best, have) {
                        (None, _) => {
                            // Said plainly, because "why is the button greyed
                            // out" is a question nobody should have to ask.
                            ui.small(format!(
                                "no release of this works with Floptle {engine} yet"
                            ));
                        }
                        (Some(r), Some(entry)) if entry.version >= r.version => {
                            ui.small(format!("installed ({})", entry.version));
                        }
                        (Some(r), have) => {
                            let label =
                                if have.is_some() { "⟳ Update" } else { "⬇ Install" };
                            if ui.button(label).clicked() {
                                let scratch =
                                    std::env::temp_dir().join("floptle-package-clone");
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
                    });
                    if state.reviews_for.as_deref() == Some(listing.id.as_str()) {
                        ui.separator();
                        // The gate Ty asked for: you may review what you have
                        // actually run. Installed AND enabled — a package sitting
                        // there switched off has not been tried.
                        let mine = reviewable_version(have);
                        reviews_section(ui, ctx, state, &listing.id, mine);
                    }
                    if let Some(home) = &listing.homepage
                        && ui.link(home).clicked()
                    {
                        let _ = floptle_script::open_in_browser(home);
                    }
                });
                ui.add_space(4.0);
            }
        });
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

/// Fetch the catalogue on a worker thread.
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
fn account_line(ui: &mut egui::Ui, ctx: &PkgCtx<'_>, state: &mut PackagesState) {
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
                state.post_note = None;
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
        ui.small("Install and enable this package to review it.");
        return;
    };
    let Some(account) = ctx.account else { return };
    if !account.is_signed_in() {
        ui.small("Sign in to review it.");
        return;
    }
    if state.posting.is_some() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.small("posting…");
        });
        return;
    }
    if let Some(note) = &state.post_note {
        ui.small(note);
    }

    ui.horizontal(|ui| {
        ui.small("your rating");
        for n in 1..=5u32 {
            let picked = state.draft_rating >= n;
            if ui.selectable_label(picked, if picked { "★" } else { "☆" }).clicked() {
                state.draft_rating = n;
            }
        }
        ui.small(format!("of v{version}"));
    });
    ui.add(
        egui::TextEdit::multiline(&mut state.draft_body)
            .desired_rows(2)
            .hint_text("what was it like to use? (optional)"),
    );
    let ready = (1..=5).contains(&state.draft_rating);
    if ui
        .add_enabled(ready, egui::Button::new("Post review"))
        .on_disabled_hover_text("pick a rating first")
        .clicked()
    {
        let body = serde_json::json!({
            "rating": state.draft_rating,
            "body": state.draft_body,
            "version": version,
        })
        .to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        match account.request(
            0,
            "POST",
            &format!("/api/floptle/v1/packages/{id}/reviews"),
            Some(body),
            std::time::Duration::from_secs(20),
            tx,
        ) {
            Ok(()) => {
                state.posting = Some(rx);
                state.post_note = None;
            }
            Err(e) => state.post_note = Some(e),
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
