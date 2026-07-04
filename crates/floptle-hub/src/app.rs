//! The eframe application: a top tab bar over Projects / Installs / Settings, plus the
//! background jobs (manifest fetch + install) whose channels are polled each frame.

use crate::config::{HubConfig, Paths};
use crate::registry::{self, Install, Project};
use crate::releases::{GithubReleases, LocalBuilds, Manifest, VersionSource};
use crate::{install, launch};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Projects,
    Installs,
    Settings,
}

enum ManifestState {
    Idle,
    Loading(Receiver<Result<Manifest, String>>),
    Loaded(Manifest),
    Error(String),
}

struct InstallJob {
    version: String,
    rx: Receiver<install::Progress>,
    line: String,
    frac: f32,
}

/// A create/upgrade running off the UI thread (it shells out to the editor's headless
/// --new / --migrate, which can be slow on a big project — so it must not block repaint).
enum ProcOutcome {
    Created(Project),
    Upgraded(usize),
    Failed(String),
}
struct ProcJob {
    rx: Receiver<ProcOutcome>,
    label: String,
}

/// A pending "create project" form.
#[derive(Default)]
struct NewProjectForm {
    name: String,
    location: String,
    version: String,
}

pub struct HubApp {
    paths: Paths,
    config: HubConfig,
    installs: Vec<Install>,
    tab: Tab,
    manifest: ManifestState,
    job: Option<InstallJob>,
    /// Session-only auth token for a private manifest/download (from `FLOPTLE_HUB_TOKEN` at
    /// start; not persisted — a keyring store is a later hardening step).
    token: String,
    new_project: Option<NewProjectForm>,
    add_path: String,
    proc: Option<ProcJob>,
    toast: Option<(String, bool)>,
    /// Toast auto-expiry: the message shown last frame + the time it first appeared, so a
    /// new toast resets the timer without threading a clock through every set-site.
    toast_seen: Option<String>,
    toast_at: f64,
}

impl HubApp {
    pub fn new(paths: Paths) -> Self {
        let _ = paths.ensure();
        let config = HubConfig::load(&paths);
        let installs = registry::scan_installs(&paths.versions_dir());
        let token = std::env::var("FLOPTLE_HUB_TOKEN").unwrap_or_default();
        let mut app = Self {
            paths,
            config,
            installs,
            tab: Tab::Projects,
            manifest: ManifestState::Idle,
            job: None,
            token,
            new_project: None,
            add_path: String::new(),
            proc: None,
            toast: None,
            toast_seen: None,
            toast_at: 0.0,
        };
        app.refresh_projects();
        app
    }

    fn refresh_projects(&mut self) {
        for p in &mut self.config.projects {
            if p.exists() {
                p.refresh();
            }
        }
    }

    fn rescan_installs(&mut self) {
        self.installs = registry::scan_installs(&self.paths.versions_dir());
    }

    fn save(&mut self) {
        if let Err(e) = self.config.save(&self.paths) {
            self.toast = Some((format!("could not save settings: {e}"), true));
        }
    }

    /// The install a project resolves to. For an explicit version, that exact install; for
    /// the fallback (no pin), the default if it's VALID, else the newest valid install — a
    /// corrupt newest install shouldn't shadow an older working one.
    fn install_for(&self, version: Option<&str>) -> Option<&Install> {
        match version {
            Some(v) => self.installs.iter().find(|i| i.version == v),
            None => {
                let def = self.config.settings.default_version.as_deref();
                def.and_then(|v| self.installs.iter().find(|i| i.version == v && i.is_valid()))
                    .or_else(|| self.installs.iter().rfind(|i| i.is_valid()))
            }
        }
    }

    fn token_opt(&self) -> Option<&str> {
        (!self.token.trim().is_empty()).then_some(self.token.trim())
    }

    // ---- background jobs ---------------------------------------------------

    fn start_manifest_fetch(&mut self) {
        let url = self.config.settings.manifest_url.clone();
        let token = self.token_opt().map(str::to_string);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            // http(s) → the real GitHub pipeline; anything else is treated as a local file
            // path (a dev manifest produced by the packaging step).
            let result = if url.starts_with("http") {
                GithubReleases { manifest_url: url, token }.manifest()
            } else {
                LocalBuilds { manifest_path: PathBuf::from(url) }.manifest()
            };
            let _ = tx.send(result);
        });
        self.manifest = ManifestState::Loading(rx);
    }

    fn poll_manifest(&mut self) {
        use std::sync::mpsc::TryRecvError;
        if let ManifestState::Loading(rx) = &self.manifest {
            self.manifest = match rx.try_recv() {
                Ok(Ok(m)) => ManifestState::Loaded(m),
                Ok(Err(e)) => ManifestState::Error(e),
                // The worker died without sending (e.g. a panic) — don't leave the UI stuck
                // on "fetching…" forever.
                Err(TryRecvError::Disconnected) => {
                    ManifestState::Error("the version check stopped unexpectedly".into())
                }
                Err(TryRecvError::Empty) => return,
            };
        }
    }

    fn start_install(&mut self, version: String, artifact: crate::releases::Artifact) {
        let paths = self.paths.clone();
        let token = self.token_opt().map(str::to_string);
        let (tx, rx) = std::sync::mpsc::channel();
        let v = version.clone();
        std::thread::spawn(move || {
            install::install(&v, &artifact, &paths, token.as_deref(), &tx);
        });
        self.job = Some(InstallJob { version, rx, line: "starting…".into(), frac: 0.0 });
    }

    fn poll_install(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(job) = &mut self.job else { return };
        let mut finished = None;
        loop {
            match job.rx.try_recv() {
                Ok(install::Progress::Downloading { done, total }) => {
                    job.frac = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    job.line = format!("downloading {:.0}%", job.frac * 100.0);
                }
                Ok(install::Progress::Verifying) => job.line = "verifying checksum…".into(),
                Ok(install::Progress::Unpacking) => job.line = "unpacking…".into(),
                Ok(install::Progress::Done(dir)) => {
                    log::info!("installed to {}", dir.display());
                    finished = Some(Ok(()));
                }
                Ok(install::Progress::Failed(e)) => finished = Some(Err(e)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // Worker gone (e.g. a panic) without a terminal message — don't wedge.
                    if finished.is_none() {
                        finished = Some(Err("the install stopped unexpectedly".into()));
                    }
                    break;
                }
            }
        }
        if let Some(res) = finished {
            let v = job.version.clone();
            self.job = None;
            match res {
                Ok(()) => {
                    self.rescan_installs();
                    if self.config.settings.default_version.is_none() {
                        self.config.settings.default_version = Some(v.clone());
                        self.save();
                    }
                    self.toast = Some((format!("installed {v}"), false));
                }
                Err(e) => self.toast = Some((format!("install failed: {e}"), true)),
            }
        }
    }

    // ---- project operations ------------------------------------------------

    /// Poll the create/upgrade worker and apply its result once.
    fn poll_proc(&mut self) {
        use std::sync::mpsc::TryRecvError;
        let Some(job) = &self.proc else { return };
        let outcome = match job.rx.try_recv() {
            Ok(o) => o,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => ProcOutcome::Failed("the operation stopped unexpectedly".into()),
        };
        self.proc = None;
        match outcome {
            ProcOutcome::Created(p) => {
                self.toast = Some((format!("created {}", p.name), false));
                self.config.upsert_project(p);
                self.save();
            }
            ProcOutcome::Upgraded(idx) => {
                if let Some(p) = self.config.projects.get_mut(idx) {
                    p.refresh();
                }
                self.save();
                self.toast = Some(("project upgraded".into(), false));
            }
            ProcOutcome::Failed(e) => self.toast = Some((e, true)),
        }
    }

    /// Validate + start a "create project" (editor `--new`) on a worker thread; returns
    /// true when a job was started (so the form can close), false on a validation error
    /// (the form stays open with a toast).
    fn start_create(&mut self, form: &NewProjectForm) -> bool {
        let name = form.name.trim().to_string();
        if name.is_empty() {
            self.toast = Some(("give the project a name".into(), true));
            return false;
        }
        if form.location.trim().is_empty() {
            self.toast = Some(("choose a location".into(), true));
            return false;
        }
        let install = match self.install_for(Some(&form.version)).or_else(|| self.install_for(None)) {
            Some(i) => i.clone(),
            None => {
                self.toast = Some(("install an engine version first (Installs tab)".into(), true));
                return false;
            }
        };
        let path = PathBuf::from(form.location.trim()).join(&name);
        if path.exists() {
            self.toast = Some((format!("{} already exists", path.display()), true));
            return false;
        }
        let bin = install.editor_bin();
        let label = format!("creating {name}…");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = match std::process::Command::new(&bin).arg("--new").arg(&path).status() {
                Ok(s) if s.success() => {
                    let mut p = Project { name, path, engine_version: None, last_opened: None };
                    p.refresh();
                    ProcOutcome::Created(p)
                }
                Ok(_) => ProcOutcome::Failed("the editor could not scaffold the project".into()),
                Err(e) => ProcOutcome::Failed(format!("run editor --new: {e}")),
            };
            let _ = tx.send(out);
        });
        self.proc = Some(ProcJob { rx, label });
        true
    }

    fn add_existing(&mut self, raw: &str) -> Result<Project, String> {
        let path = PathBuf::from(raw.trim());
        if !path.is_dir() {
            return Err(format!("{} is not a folder", path.display()));
        }
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "project".into());
        let mut project = Project { name, path, engine_version: None, last_opened: None };
        project.refresh();
        Ok(project)
    }

    /// The newest installed version strictly newer than the project's pinned one — the
    /// "Upgrade to X" target, if any.
    fn upgrade_target(&self, project: &Project) -> Option<Install> {
        let pinned = project.engine_version.clone();
        self.installs
            .iter()
            .filter(|i| match &pinned {
                Some(p) => crate::releases::version_key(&i.version) > crate::releases::version_key(p),
                None => false,
            })
            .max_by(|a, b| {
                crate::releases::version_key(&a.version).cmp(&crate::releases::version_key(&b.version))
            })
            .cloned()
    }

    /// Re-point a project to a newer installed engine on a worker thread: run that engine's
    /// headless `--migrate` (re-serializes assets + stamps engine_version), then refresh the
    /// cached version in poll_proc. The migration is the engine's job — the Hub drives it.
    fn start_upgrade(&mut self, idx: usize, target: &Install) {
        if !target.is_valid() {
            self.toast = Some((format!("engine {} is missing its binary", target.version), true));
            return;
        }
        let Some(project) = self.config.projects.get(idx).cloned() else { return };
        let bin = target.editor_bin();
        let path = project.path.clone();
        let label = format!("upgrading {} to {}…", project.name, target.version);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = match std::process::Command::new(&bin).arg("--migrate").arg(&path).status() {
                Ok(s) if s.success() => ProcOutcome::Upgraded(idx),
                Ok(_) => ProcOutcome::Failed("migration exited with an error".into()),
                Err(e) => ProcOutcome::Failed(format!("upgrade failed: {e}")),
            };
            let _ = tx.send(out);
        });
        self.proc = Some(ProcJob { rx, label });
    }

    fn launch_project(&mut self, idx: usize) {
        let Some(project) = self.config.projects.get(idx).cloned() else { return };
        let install = self.install_for(project.engine_version.as_deref()).cloned();
        match install {
            Some(install) => match launch::launch(&install, &project) {
                Ok(()) => self.toast = Some((format!("launched {}", project.name), false)),
                Err(e) => self.toast = Some((e, true)),
            },
            None => {
                self.toast = Some((
                    match project.engine_version {
                        Some(v) => format!("engine {v} isn't installed — install it in the Installs tab"),
                        None => "no engine installed — install one in the Installs tab".into(),
                    },
                    true,
                ))
            }
        }
    }
}

impl eframe::App for HubApp {
    // Pre-paint state update (egui 0.35 splits logic from ui). Poll the background jobs
    // and keep repainting while one runs so its progress animates.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_manifest();
        self.poll_install();
        self.poll_proc();
        // Auto-expire a toast ~6s after it appears (detect a new message by its text, so
        // the ~10 set-sites don't each need a clock).
        let now = ctx.input(|i| i.time);
        let cur = self.toast.as_ref().map(|(m, _)| m.clone());
        if cur != self.toast_seen {
            self.toast_seen = cur;
            self.toast_at = now;
        }
        if self.toast.is_some() && now - self.toast_at > 6.0 {
            self.toast = None;
            self.toast_seen = None;
        }
        // Keep repainting while anything is in flight (or a toast is counting down).
        if self.job.is_some()
            || self.proc.is_some()
            || self.toast.is_some()
            || matches!(self.manifest, ManifestState::Loading(_))
        {
            ctx.request_repaint();
        }
    }

    // egui 0.35 hands the root `Ui`; panels are shown INTO it (top/bottom first, then the
    // central content).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Floptle Hub");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Projects, "Projects");
                ui.selectable_value(&mut self.tab, Tab::Installs, "Installs");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
            });
        });

        if let Some((msg, is_err)) = self.toast.clone() {
            let mut dismiss = false;
            egui::Panel::bottom("toast").show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button("✕").clicked() {
                        dismiss = true;
                    }
                    let color = if is_err { egui::Color32::LIGHT_RED } else { egui::Color32::LIGHT_GREEN };
                    ui.colored_label(color, msg);
                });
            });
            if dismiss {
                self.toast = None;
                self.toast_seen = None;
            }
        }

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Projects => self.projects_tab(ui),
            Tab::Installs => self.installs_tab(ui),
            Tab::Settings => self.settings_tab(ui),
        });
    }
}

impl HubApp {
    fn projects_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        // A running create/upgrade (off-thread).
        if let Some(job) = &self.proc {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(&job.label);
            });
        }
        let busy = self.proc.is_some();
        // New / add controls.
        ui.horizontal(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("＋ New project")).clicked() {
                let version = self
                    .config
                    .settings
                    .default_version
                    .clone()
                    .or_else(|| self.installs.last().map(|i| i.version.clone()))
                    .unwrap_or_default();
                self.new_project = Some(NewProjectForm { version, ..Default::default() });
            }
            ui.label("or add existing:");
            ui.text_edit_singleline(&mut self.add_path);
            if ui.button("Add").clicked() {
                match self.add_existing(&self.add_path.clone()) {
                    Ok(p) => {
                        self.config.upsert_project(p);
                        self.save();
                        self.add_path.clear();
                    }
                    Err(e) => self.toast = Some((e, true)),
                }
            }
        });

        if let Some(mut form) = self.new_project.take() {
            let mut keep = true;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.label("New project");
                ui.horizontal(|ui| {
                    ui.label("name");
                    ui.text_edit_singleline(&mut form.name);
                });
                ui.horizontal(|ui| {
                    ui.label("location (parent folder)");
                    ui.text_edit_singleline(&mut form.location);
                });
                ui.horizontal(|ui| {
                    ui.label("engine");
                    egui::ComboBox::from_id_salt("new-proj-version")
                        .selected_text(if form.version.is_empty() { "(none installed)".into() } else { form.version.clone() })
                        .show_ui(ui, |ui| {
                            for i in &self.installs {
                                ui.selectable_value(&mut form.version, i.version.clone(), &i.version);
                            }
                        });
                });
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new("Create")).clicked() && self.start_create(&form) {
                        keep = false;
                    }
                    if ui.button("Cancel").clicked() {
                        keep = false;
                    }
                });
            });
            if keep {
                self.new_project = Some(form);
            }
        }

        ui.separator();
        if self.config.projects.is_empty() {
            ui.label("No projects yet — create one, or add an existing project folder.");
            return;
        }

        // Precompute per-project upgrade targets so the loop only reads immutably.
        let upgrades: Vec<Option<Install>> =
            self.config.projects.iter().map(|p| self.upgrade_target(p)).collect();
        let mut launch_idx = None;
        let mut remove = None;
        let mut upgrade: Option<(usize, Install)> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for (idx, p) in self.config.projects.iter().enumerate() {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.strong(&p.name);
                            ui.small(p.path.display().to_string());
                            let ver = p.engine_version.clone().unwrap_or_else(|| "unpinned".into());
                            let installed = p
                                .engine_version
                                .as_deref()
                                .map(|v| self.installs.iter().any(|i| i.version == v))
                                .unwrap_or(!self.installs.is_empty());
                            let mark = if !p.exists() {
                                "⚠ folder missing"
                            } else if installed {
                                "engine ✓"
                            } else {
                                "engine not installed"
                            };
                            ui.small(format!("engine: {ver}  ·  {mark}"));
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("🗑").on_hover_text("remove from Hub (doesn't delete files)").clicked() {
                                remove = Some(idx);
                            }
                            if ui.add_enabled(p.exists(), egui::Button::new("Open ▶")).clicked() {
                                launch_idx = Some(idx);
                            }
                            if let Some(target) = &upgrades[idx]
                                && p.exists()
                                && ui
                                    .add_enabled(!busy, egui::Button::new(format!("⬆ {}", target.version)))
                                    .on_hover_text("migrate this project to the newer installed engine")
                                    .clicked()
                            {
                                upgrade = Some((idx, target.clone()));
                            }
                        });
                    });
                });
            }
        });
        if let Some(idx) = launch_idx {
            self.launch_project(idx);
        }
        if let Some((idx, target)) = upgrade {
            self.start_upgrade(idx, &target);
        }
        if let Some(idx) = remove {
            let path = self.config.projects[idx].path.clone();
            self.config.remove_project(&path);
            self.save();
        }
    }

    fn installs_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.strong("Installed");
        if self.installs.is_empty() {
            ui.label("None installed yet.");
        } else {
            let mut set_default = None;
            let mut uninstall = None;
            for i in &self.installs {
                ui.horizontal(|ui| {
                    let is_default = self.config.settings.default_version.as_deref() == Some(i.version.as_str());
                    ui.label(if is_default { format!("● {} (default)", i.version) } else { format!("○ {}", i.version) });
                    if !i.is_valid() {
                        ui.colored_label(egui::Color32::LIGHT_RED, "invalid");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Uninstall").clicked() {
                            uninstall = Some(i.clone());
                        }
                        if !is_default && ui.button("Set default").clicked() {
                            set_default = Some(i.version.clone());
                        }
                    });
                });
            }
            if let Some(v) = set_default {
                self.config.settings.default_version = Some(v);
                self.save();
            }
            if let Some(i) = uninstall {
                let _ = std::fs::remove_dir_all(&i.path);
                if self.config.settings.default_version.as_deref() == Some(i.version.as_str()) {
                    self.config.settings.default_version = None;
                }
                self.rescan_installs();
                self.save();
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Available");
            let loading = matches!(self.manifest, ManifestState::Loading(_));
            if ui.add_enabled(!loading, egui::Button::new("↻ Check for versions")).clicked() {
                self.start_manifest_fetch();
            }
            ui.label(format!("channel: {}", self.config.settings.channel));
        });

        // A running install job.
        if let Some(job) = &self.job {
            ui.horizontal(|ui| {
                ui.label(format!("installing {} — {}", job.version, job.line));
            });
            ui.add(egui::ProgressBar::new(job.frac).show_percentage());
        }

        let mut to_install = None;
        match &self.manifest {
            ManifestState::Idle => {
                ui.label("Click “Check for versions” to fetch the release list.");
            }
            ManifestState::Loading(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("fetching…");
                });
            }
            ManifestState::Error(e) => {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("could not load versions: {e}"));
            }
            ManifestState::Loaded(m) => {
                let channel = self.config.settings.channel.clone();
                let releases = m.on_channel(&channel);
                if releases.is_empty() {
                    ui.label(format!("no versions on the '{channel}' channel"));
                }
                for r in &releases {
                    let installed = self.installs.iter().any(|i| i.version == r.version);
                    ui.horizontal(|ui| {
                        ui.label(&r.version);
                        if !r.date.is_empty() {
                            ui.small(&r.date);
                        }
                        match r.artifact_here() {
                            None => {
                                ui.small(format!("(no build for {})", crate::releases::platform_target()));
                            }
                            Some(art) => {
                                if installed {
                                    ui.small("installed ✓");
                                } else if self.job.is_none() && ui.button("Install").clicked() {
                                    to_install = Some((r.version.clone(), art.clone()));
                                }
                            }
                        }
                    });
                }
            }
        }
        if let Some((v, art)) = to_install {
            self.start_install(v, art);
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        let mut changed = false;
        egui::Grid::new("settings").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Channel");
            egui::ComboBox::from_id_salt("channel")
                .selected_text(&self.config.settings.channel)
                .show_ui(ui, |ui| {
                    for c in ["stable", "beta"] {
                        if ui.selectable_value(&mut self.config.settings.channel, c.to_string(), c).changed() {
                            changed = true;
                        }
                    }
                });
            ui.end_row();

            ui.label("Manifest URL");
            changed |= ui.text_edit_singleline(&mut self.config.settings.manifest_url).changed();
            ui.end_row();

            ui.label("Default engine");
            let cur = self.config.settings.default_version.clone().unwrap_or_default();
            egui::ComboBox::from_id_salt("default-version")
                .selected_text(if cur.is_empty() { "(none)".into() } else { cur })
                .show_ui(ui, |ui| {
                    for i in &self.installs {
                        if ui
                            .selectable_label(self.config.settings.default_version.as_deref() == Some(i.version.as_str()), &i.version)
                            .clicked()
                        {
                            self.config.settings.default_version = Some(i.version.clone());
                            changed = true;
                        }
                    }
                });
            ui.end_row();

            ui.label("Auth token (session)");
            ui.add(egui::TextEdit::singleline(&mut self.token).password(true).hint_text("for a private repo — not saved"));
            ui.end_row();

            ui.label("Data folder");
            ui.small(self.paths.data.display().to_string());
            ui.end_row();
        });
        if changed {
            self.save();
        }
        ui.separator();
        ui.small("Token is used only this session (a keyring store is a later hardening step). Point the manifest URL at a local releases.json to test against a locally-packaged bundle.");
    }
}
