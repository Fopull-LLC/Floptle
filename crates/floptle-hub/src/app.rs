//! The eframe application: a top tab bar over Projects / Installs / Settings, plus the
//! background jobs (manifest fetch + install) whose channels are polled each frame.

use crate::auth::{self, Provider, RefreshError, TokenStore};
use crate::config::{HubConfig, Paths};
use crate::registry::{self, Install, Project};
use crate::releases::{GithubReleases, LocalBuilds, Manifest, VersionSource};
use crate::{install, launch};
use eframe::egui;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

/// Fopull LLC identity + the open-source links surfaced in the About tab.
const COMPANY: &str = "Fopull LLC";
const WEBSITE_URL: &str = "https://fopull.com/";
const REPO_URL: &str = "https://github.com/Fopull-LLC/Floptle";
const RELEASES_URL: &str = "https://github.com/Fopull-LLC/Floptle-releases/releases";
const DOCS_URL: &str = "https://github.com/Fopull-LLC/Floptle/tree/main/docs";
const ISSUES_URL: &str = "https://github.com/Fopull-LLC/Floptle/issues";

/// UI glyphs — every one is verified present in egui's bundled fonts (Ubuntu / NotoEmoji /
/// emoji-icon-font), so none render as a missing-glyph box. Anything added here must be
/// checked against that font union first: some obvious choices are NOT in the set and show
/// as tofu — fullwidth plus (U+FF0B), the light check (U+2713), the multiplication-x
/// (U+2715), and any emoji carrying a U+FE0F variation selector. Prefer U+2795 / U+2714 /
/// U+2716 instead.
///
/// `every_icon_is_drawable` asserts that against the real font stack, so this is now a
/// checked claim rather than a warning to be careful. It was a warning for four releases
/// and two glyphs still shipped as boxes.
mod ico {
    pub const NEWS: &str = "📰";
    pub const NEW: &str = "➕";
    pub const OPEN: &str = "▶";
    pub const UPGRADE: &str = "⬆";
    pub const REMOVE: &str = "🗑";
    pub const REVEAL: &str = "🗁";
    pub const REFRESH: &str = "↻";
    pub const INSTALL: &str = "⬇";
    pub const OK: &str = "✔";
    pub const WARN: &str = "⚠";
    pub const CLOSE: &str = "✖";
    pub const STAR: &str = "⭐";
    pub const PROJECTS: &str = "📁";
    pub const INSTALLS: &str = "📦";
    pub const SETTINGS: &str = "⚙";
    pub const ABOUT: &str = "ℹ";
    pub const GLOBE: &str = "🌐";
    pub const BUG: &str = "🐛";
    pub const BOOK: &str = "📖";
    pub const ROCKET: &str = "🚀";
    pub const ACCOUNT: &str = "👤";
    pub const SIGNIN: &str = "🔑";
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Projects,
    News,
    Installs,
    Settings,
    About,
}

/// Force `project.ron`'s `engine_version` to `version`. The HUB is the authority on which
/// engine it installed/selected, so it corrects whatever the editor subprocess stamped —
/// this keeps create/upgrade correct even against an OLDER editor binary that ignores
/// `--engine-version` and writes its own compiled-in version (the exact reason a bundle
/// installed as "0.1.0" could otherwise pin projects to "0.0.0"). Best-effort and
/// idempotent: uses the same `save_project` the editor does, so the file stays byte-for-byte
/// what the editor would have written; a missing/unparseable config is left untouched.
fn pin_engine_version(project_dir: &std::path::Path, version: &str) {
    let cfg_path = project_dir.join("project.ron");
    if let Ok(Some(mut cfg)) = floptle_scene::try_load_project(&cfg_path)
        && cfg.engine_version.as_deref() != Some(version)
    {
        cfg.engine_version = Some(version.to_string());
        let _ = floptle_scene::save_project(&cfg, &cfg_path);
    }
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

/// Whether the running auth worker is an interactive sign-in (shows a device-code prompt +
/// Cancel + a toast) or a silent background token refresh (invisible unless it hard-fails).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthKind {
    SignIn,
    Refresh,
}

/// An account sign-in / token-refresh running off the UI thread — the device flow polls the
/// provider for up to several minutes, so it must never block repaint. (Sign-out is fire-and-
/// forget and is NOT tracked here, so it can't wedge the Account panel.)
enum AuthEvent {
    /// The provider issued a device code — show it and open the approval page.
    Prompt { user_code: String, approve_url: String },
    /// Signed in (or a refresh completed) — the worker already persisted it; just display it.
    Signed(Box<auth::Session>),
    /// A refresh hit `invalid_grant` — the session is unrecoverable, sign the user out.
    SessionExpired,
    /// A transient/interactive failure. For a silent refresh the session is kept; for an
    /// interactive sign-in it's surfaced as an error toast.
    Failed(String),
}
struct AuthJob {
    rx: Receiver<AuthEvent>,
    /// The device code + approval URL once the provider returns them (shown in the UI while
    /// polling).
    prompt: Option<(String, String)>,
    /// Set to cancel a pending sign-in (the worker's poll loop observes it).
    cancel: Arc<AtomicBool>,
    kind: AuthKind,
}

/// Current unix time in seconds (for token-expiry checks). 0 on the impossible pre-1970 error.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    /// When the manifest was last fetched — the Hub re-checks every few hours
    /// while it's open, so "a new version shipped" surfaces without a restart.
    manifest_fetched_at: std::time::Instant,
    /// Signed-in fopull.com account (persisted in the OS keyring) + the running sign-in/refresh
    /// worker. Workers build their own [`auth::KeyringStore`] so keyring I/O stays off the UI
    /// thread; only the one-time startup load runs inline.
    session: Option<auth::Session>,
    auth_job: Option<AuthJob>,
    /// Which version's release notes the Installs tab is showing. `None` until the list
    /// first has something in it, then the newest — the release somebody opening this tab
    /// is nearly always asking about.
    selected_version: Option<String>,
    /// A running self-update, and whether the banner is hidden.
    ///
    /// **Hidden for this session only, never persisted.** The engine banner remembers a
    /// dismissal per version, which is right for "there is a newer engine you may or may
    /// not want". It is wrong for "the app you are using is out of date": one idle click
    /// would silence that forever. This comes back next launch, and the chip in the tab
    /// bar never goes away at all.
    hub_update_job: Option<HubUpdateJob>,
    hub_update_hidden: bool,
}

/// A running self-update. Same shape as [`InstallJob`] — the UI renders them alike.
struct HubUpdateJob {
    rx: Receiver<crate::selfupdate::Progress>,
    line: String,
    frac: f32,
}

/// One version in the Installs list — see [`HubApp::version_rows`].
struct VersionRow {
    version: String,
    date: String,
    title: String,
    notes_url: String,
    /// The bundle for THIS platform, if the release ships one.
    artifact: Option<crate::releases::Artifact>,
    installed: Option<Install>,
    is_default: bool,
    /// This release shipped a new Hub and the same engine as the one before it. Installable,
    /// and not a reason to move a project — so it is never labelled "new" in a list of
    /// engines. False for a release the manifest says nothing about, which is every release
    /// published before the Hub could tell the difference.
    hub_only: bool,
}

impl HubApp {
    pub fn new(paths: Paths) -> Self {
        let _ = paths.ensure();
        let mut config = HubConfig::load(&paths);
        // Seed the "new project" location once so the form isn't blank on first use.
        if config.settings.projects_dir.is_none() {
            config.settings.projects_dir = Some(crate::config::default_projects_dir());
        }
        let installs = registry::scan_installs(&paths.versions_dir());
        let token = std::env::var("FLOPTLE_HUB_TOKEN").unwrap_or_default();
        // Restore a previously signed-in account from the OS keyring (one-time, before the
        // window is interactive).
        //
        // A session from a DIFFERENT provider is dropped here rather than carried in. The
        // retired dev instance had its own database and its own signing key, so a session
        // it minted names an account that does not exist on production: every refresh
        // answers `invalid_grant` and every call answers `401`. Presenting it as signed in
        // would be a window that shows a name and then fails at everything, which is worse
        // than showing the sign-in button. Clearing the store too, so the *game* side —
        // which shares this keyring entry — doesn't find it either.
        let store = auth::KeyringStore::default();
        let session = store.load().filter(|s| {
            let ours = s.issued_by(&config.settings.auth_base_url);
            if !ours {
                let _ = store.clear();
            }
            ours
        });
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
            manifest_fetched_at: std::time::Instant::now(),
            session,
            auth_job: None,
            selected_version: None,
            hub_update_job: None,
            hub_update_hidden: false,
        };
        app.refresh_projects();
        // Fetch the available-versions list up front so the Installs tab is populated without
        // a manual click (best-effort — an offline start just shows an error there).
        app.start_manifest_fetch();
        // Proactively refresh a restored session whose access token is near expiry.
        app.refresh_session_if_stale();
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

    // ---- account (fopull.com sign-in) --------------------------------------

    /// Start the OAuth device flow on a worker thread: PKCE, `/oauth/device`, show the code and
    /// open the approval page, poll `/oauth/token` (cancellable, with the code's deadline),
    /// then fetch identity into a [`auth::Session`] and persist it — all off the UI thread.
    fn start_sign_in(&mut self) {
        if self.auth_job.is_some() {
            return;
        }
        let base = self.config.settings.auth_base_url.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_worker = cancel.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let provider = auth::HttpProvider::new(&base);
            let pkce = auth::Pkce::generate();
            let dev = match provider.start_device(&pkce.challenge) {
                Ok(d) => d,
                Err(e) => {
                    let _ = tx.send(AuthEvent::Failed(e));
                    return;
                }
            };
            let _ = tx.send(AuthEvent::Prompt {
                user_code: dev.user_code.clone(),
                approve_url: dev.approve_url().to_string(),
            });
            // Sleep in 1s steps so a cancel is observed promptly, not only between polls.
            let tokens = match auth::poll_until(
                &provider,
                &dev.device_code,
                &pkce.verifier,
                dev.interval,
                dev.expires_in,
                &cancel_worker,
                |s| {
                    for _ in 0..s {
                        if cancel_worker.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                },
            ) {
                Ok(t) => t,
                Err(e) => {
                    let _ = tx.send(AuthEvent::Failed(e));
                    return;
                }
            };
            // Identity is required — never persist an empty-identity "signed in" state.
            let who = match provider.userinfo(&tokens.access_token) {
                Ok(w) => w,
                Err(e) => {
                    let _ = tx.send(AuthEvent::Failed(e));
                    return;
                }
            };
            // The plan is secondary: a failed fetch shows "unknown" (not a wrong "free") and is
            // reconciled on the next refresh.
            let ent = provider
                .entitlements(&tokens.access_token)
                .unwrap_or(auth::Entitlements { tier: "unknown".into() });
            let session = auth::Session::from_parts(tokens, who, ent);
            let _ = auth::KeyringStore::default().save(&session);
            let _ = tx.send(AuthEvent::Signed(Box::new(session)));
        });
        self.auth_job = Some(AuthJob { rx, prompt: None, cancel, kind: AuthKind::SignIn });
    }

    /// Cancel a pending sign-in: signal the worker to stop and free the panel immediately.
    fn cancel_sign_in(&mut self) {
        if let Some(job) = &self.auth_job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.auth_job = None;
    }

    /// Sign out: forget the session locally now, and clear the keyring + revoke the refresh
    /// token server-side best-effort on a detached thread (not tracked as an auth_job, so it
    /// can't briefly show a spinner or block a re-sign-in).
    fn start_sign_out(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        self.toast = Some(("signed out".into(), false));
        let base = self.config.settings.auth_base_url.clone();
        std::thread::spawn(move || {
            let _ = auth::KeyringStore::default().clear();
            if let Some(rt) = session.refresh_token {
                let _ = auth::HttpProvider::new(&base).revoke(&rt);
            }
        });
    }

    /// If a restored session's access token is at/near expiry, refresh it off-thread (silently)
    /// so Cloud calls don't start with a stale token. A permanent `invalid_grant` signs the
    /// user out; a transient failure keeps the session.
    fn refresh_session_if_stale(&mut self) {
        let Some(session) = &self.session else {
            return;
        };
        if !session.needs_refresh(now_unix()) {
            return;
        }
        let Some(rt) = session.refresh_token.clone() else {
            return;
        };
        let old = session.clone();
        let base = self.config.settings.auth_base_url.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let provider = auth::HttpProvider::new(&base);
            match provider.refresh(&rt) {
                Ok(new) => {
                    let mut s = old;
                    s.access_token = new.access_token;
                    if let Some(r) = new.refresh_token {
                        s.refresh_token = Some(r);
                    }
                    // Reconcile identity + plan on refresh (fixes a prior "unknown" tier);
                    // tolerate a failed sub-call by keeping the previous values.
                    if let Ok(who) = provider.userinfo(&s.access_token) {
                        s.sub = who.sub;
                        s.email = who.email;
                    }
                    if let Ok(ent) = provider.entitlements(&s.access_token)
                        && !ent.tier.is_empty()
                    {
                        s.tier = ent.tier;
                    }
                    let _ = auth::KeyringStore::default().save(&s);
                    let _ = tx.send(AuthEvent::Signed(Box::new(s)));
                }
                Err(RefreshError::Invalid) => {
                    let _ = tx.send(AuthEvent::SessionExpired);
                }
                Err(RefreshError::Transient(e)) => {
                    let _ = tx.send(AuthEvent::Failed(e));
                }
            }
        });
        self.auth_job = Some(AuthJob { rx, prompt: None, cancel: Arc::new(AtomicBool::new(false)), kind: AuthKind::Refresh });
    }

    /// Poll the sign-in/refresh worker and apply its result once. Interactive vs silent
    /// (refresh) determines whether success/failure surfaces a toast.
    fn poll_auth(&mut self, ctx: &egui::Context) {
        use std::sync::mpsc::TryRecvError;
        let (event, kind) = match &self.auth_job {
            Some(job) => match job.rx.try_recv() {
                Ok(e) => (e, job.kind),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => (AuthEvent::Failed("worker stopped unexpectedly".into()), job.kind),
            },
            None => return,
        };
        match event {
            AuthEvent::Prompt { user_code, approve_url } => {
                if let Some(job) = &mut self.auth_job {
                    job.prompt = Some((user_code, approve_url.clone()));
                }
                // Auto-open the approval page (it's also shown as a clickable link).
                ctx.output_mut(|o| {
                    o.commands.push(egui::OutputCommand::OpenUrl(egui::OpenUrl::new_tab(approve_url)))
                });
            }
            AuthEvent::Signed(session) => {
                // The worker already persisted to the keyring.
                let who = session.display_name().to_string();
                self.session = Some(*session);
                self.auth_job = None;
                if kind == AuthKind::SignIn {
                    self.toast = Some((format!("signed in as {who}"), false));
                }
            }
            AuthEvent::SessionExpired => {
                self.session = None;
                self.auth_job = None;
                std::thread::spawn(|| {
                    let _ = auth::KeyringStore::default().clear();
                });
                self.toast = Some(("your session expired — please sign in again".into(), true));
            }
            AuthEvent::Failed(e) => {
                self.auth_job = None;
                if kind == AuthKind::SignIn {
                    self.toast = Some((format!("sign-in failed: {e}"), true));
                } else {
                    // Silent refresh: a transient blip must not alarm the user or sign them out.
                    log::warn!("session refresh failed (session kept): {e}");
                }
            }
        }
    }

    /// The Account panel (shown atop Settings): the pending device-code prompt while an
    /// interactive sign-in runs (a silent background refresh stays invisible), else the
    /// signed-in identity + plan, else a Sign in button.
    fn account_section(&mut self, ui: &mut egui::Ui) {
        let mut sign_in = false;
        let mut sign_out = false;
        let mut cancel = false;
        // Only an INTERACTIVE sign-in takes over the panel; a silent refresh does not.
        let signing_in = matches!(&self.auth_job, Some(j) if j.kind == AuthKind::SignIn);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(ico::ACCOUNT).size(16.0));
                ui.strong("Account");
            });
            if signing_in {
                match self.auth_job.as_ref().and_then(|j| j.prompt.as_ref()) {
                    Some((code, url)) => {
                        ui.label("Approve this code in your browser to finish signing in:");
                        ui.horizontal(|ui| {
                            ui.strong(egui::RichText::new(code).monospace().size(20.0));
                            ui.hyperlink_to("open approval page", url);
                        });
                        ui.small("waiting for approval…");
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("starting sign-in…");
                        });
                    }
                }
                if ui.button(format!("{} Cancel", ico::CLOSE)).clicked() {
                    cancel = true;
                }
            } else if let Some(session) = &self.session {
                ui.label(format!("Signed in as {}", session.display_name()));
                ui.small(format!("plan: {}", session.tier));
                if ui.button(format!("{} Sign out", ico::CLOSE)).clicked() {
                    sign_out = true;
                }
            } else {
                ui.small("Sign in to fopull.com to use Floptle Cloud (managed relay, matchmaking, hosting).");
                if ui.button(format!("{} Sign in", ico::SIGNIN)).clicked() {
                    sign_in = true;
                }
            }
        });
        if sign_in {
            self.start_sign_in();
        }
        if sign_out {
            self.start_sign_out();
        }
        if cancel {
            self.cancel_sign_in();
        }
    }

    /// The newest release on the user's channel that is strictly newer than
    /// everything installed, **actually changed the engine**, and ships a bundle for this
    /// platform — what the update banner offers. None while nothing is installed yet (the
    /// Installs tab is the front door there, not a nag).
    ///
    /// The engine check is why this isn't just "newest > installed". One tag builds the
    /// engine and the Hub together, so a Hub-only release raises the version number of an
    /// engine that did not change, and this banner would tell you to go and install it.
    fn update_available(&self) -> Option<crate::releases::ReleaseInfo> {
        let ManifestState::Loaded(m) = &self.manifest else { return None };
        let newest_installed = self
            .installs
            .iter()
            .map(|i| crate::releases::version_key(&i.version))
            .max()?;
        m.on_channel_refs(&self.config.settings.channel)
            .into_iter()
            .find(|r| {
                r.artifact_here().is_some()
                    && r.changes_engine()
                    && crate::releases::version_key(&r.version) > newest_installed
            })
            .cloned()
    }

    /// The newest release on the user's channel, whatever is installed.
    ///
    /// Not [`update_available`](Self::update_available): that one answers "should you be
    /// nagged", so it is None when you are current and None before your first install.
    /// The News tab wants the newest release unconditionally — "this is the latest and
    /// you have it" is a useful thing for a news page to say.
    fn newest_release(&self) -> Option<&crate::releases::ReleaseInfo> {
        let ManifestState::Loaded(m) = &self.manifest else { return None };
        m.on_channel_refs(&self.config.settings.channel).into_iter().next()
    }

    /// Which engine a release actually ships: the newest release below it that changed one.
    ///
    /// Only interesting for a Hub-only release, where the answer is not its own version —
    /// and where saying so out loud ("the same engine as 0.22.0") is the difference between
    /// a version number that looks wrong and one that explains itself.
    fn engine_behind(&self, version: &str) -> Option<String> {
        let ManifestState::Loaded(m) = &self.manifest else { return None };
        let k = crate::releases::version_key(version);
        m.on_channel_refs(&self.config.settings.channel)
            .into_iter()
            .find(|r| r.changes_engine() && crate::releases::version_key(&r.version) < k)
            .map(|r| r.version.clone())
    }

    /// This Hub's own version, or `None` for a dev build (`0.0.0`), which is never
    /// "out of date".
    fn hub_version() -> Option<&'static str> {
        let v = env!("CARGO_PKG_VERSION");
        (v != "0.0.0").then_some(v)
    }

    /// A newer **Hub** on the user's channel that ships a Hub bundle for this platform.
    ///
    /// Separate from [`update_available`](Self::update_available), which is about the
    /// engine. They move together in practice — one tag builds both — but they are
    /// different questions: one is "there's a new engine to install", the other is "the
    /// window you are looking at is old", and only the second can go stale silently.
    fn hub_update_available(&self) -> Option<crate::releases::ReleaseInfo> {
        let ManifestState::Loaded(m) = &self.manifest else { return None };
        let mine = crate::releases::version_key(Self::hub_version()?);
        m.on_channel_refs(&self.config.settings.channel)
            .into_iter()
            .find(|r| r.hub_artifact_here().is_some() && crate::releases::version_key(&r.version) > mine)
            .cloned()
    }

    fn start_hub_update(&mut self, artifact: crate::releases::Artifact) {
        if self.hub_update_job.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let paths = self.paths.clone();
        // The same session token the engine installs use — so a private-repo manifest can
        // be tested end to end rather than only up to the Hub's own bundle.
        let token = self.token_opt().map(str::to_string);
        std::thread::spawn(move || {
            crate::selfupdate::update(&artifact, &paths, token.as_deref(), &tx)
        });
        self.hub_update_job = Some(HubUpdateJob { rx, line: "starting…".into(), frac: 0.0 });
    }

    /// Drain the self-update worker. On success the Hub **relaunches and exits** — there
    /// is no in-place reload of a binary, and leaving the old one running beside a new
    /// one on disk is how a user ends up reporting a bug that was fixed.
    fn poll_hub_update(&mut self, ctx: &egui::Context) {
        let Some(job) = &mut self.hub_update_job else { return };
        let mut done: Option<PathBuf> = None;
        let mut failed = None;
        while let Ok(p) = job.rx.try_recv() {
            match p {
                crate::selfupdate::Progress::Downloading { done: d, total } => {
                    job.frac = if total > 0 { d as f32 / total as f32 } else { 0.0 };
                    job.line = format!("downloading {:.0}%", job.frac * 100.0);
                }
                crate::selfupdate::Progress::Verifying => {
                    job.line = "verifying".into();
                }
                crate::selfupdate::Progress::Swapping => {
                    job.frac = 1.0;
                    job.line = "installing".into();
                }
                crate::selfupdate::Progress::Done(exe) => done = Some(exe),
                crate::selfupdate::Progress::Failed(e) => failed = Some(e),
            }
        }
        if let Some(e) = failed {
            self.hub_update_job = None;
            self.toast = Some((format!("could not update the Hub: {e}"), true));
            return;
        }
        if let Some(exe) = done {
            self.hub_update_job = None;
            match crate::selfupdate::relaunch(&exe) {
                Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
                Err(e) => {
                    self.toast = Some((
                        format!("{e} — the new Hub is installed, start it again yourself"),
                        true,
                    ));
                }
            }
        }
    }

    /// One row of the Installs list: every version this Hub knows about, from either
    /// side, newest first.
    ///
    /// **Merged rather than two lists.** A version can be installed and absent from the
    /// manifest (a local build, or a release pulled after the fact), or listed and not
    /// installed, and the question the user has is "what about 0.21.0?" — one row per
    /// version answers it, where two lists made them check both and work out which of
    /// the two 0.21.0s they were looking at.
    fn version_rows(&self) -> Vec<VersionRow> {
        let mut rows: Vec<VersionRow> = Vec::new();
        let default = self.config.settings.default_version.as_deref();

        if let ManifestState::Loaded(m) = &self.manifest {
            for r in m.on_channel_refs(&self.config.settings.channel) {
                rows.push(VersionRow {
                    version: r.version.clone(),
                    date: r.date.clone(),
                    title: r.title.clone(),
                    notes_url: r.notes_url.clone(),
                    artifact: r.artifact_here().cloned(),
                    installed: None,
                    is_default: default == Some(r.version.as_str()),
                    hub_only: r.is_hub_only(),
                });
            }
        }
        for i in &self.installs {
            match rows.iter_mut().find(|r| r.version == i.version) {
                Some(row) => row.installed = Some(i.clone()),
                None => rows.push(VersionRow {
                    version: i.version.clone(),
                    date: String::new(),
                    title: String::new(),
                    notes_url: String::new(),
                    artifact: None,
                    installed: Some(i.clone()),
                    is_default: default == Some(i.version.as_str()),
                    hub_only: false,
                }),
            }
        }
        rows.sort_by_key(|r| std::cmp::Reverse(crate::releases::version_key(&r.version)));
        rows
    }

    // ---- background jobs ---------------------------------------------------

    fn start_manifest_fetch(&mut self) {
        self.manifest_fetched_at = std::time::Instant::now();
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
        // Remember this parent folder so the next "New project" starts there.
        let loc = form.location.trim().to_string();
        if self.config.settings.projects_dir.as_deref() != Some(loc.as_str()) {
            self.config.settings.projects_dir = Some(loc);
            self.save();
        }
        let bin = install.editor_bin();
        // Pin the project to the version the user PICKED, not the binary's compiled-in one
        // (a bundle reports its own version.json label; passing it explicitly is the
        // authority so the new project's engine matches an installed one and can be opened).
        let pin = install.version.clone();
        let label = format!("creating {name}…");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = match std::process::Command::new(&bin)
                .arg("--new")
                .arg(&path)
                .arg("--engine-version")
                .arg(&pin)
                .status()
            {
                Ok(s) if s.success() => {
                    // Authoritatively pin the picked version, correcting an older binary
                    // that stamped its own compiled-in version.
                    pin_engine_version(&path, &pin);
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

    /// The newest installed version strictly newer than the project's pinned one **whose
    /// engine is actually different** — the "Upgrade engine to X" target, if any.
    ///
    /// A bigger number is not by itself a reason to migrate a project. A Hub-only release
    /// carries the same engine as the version before it, so offering to move a project onto
    /// it is offering a migration with no result — and the version the project ends up
    /// pinned to then disagrees with the release notes, which say the engine didn't change.
    /// The manifest knows which releases touched the engine; when it hasn't loaded, fall
    /// back to comparing numbers, so an offline Hub behaves exactly as it used to.
    fn upgrade_target(&self, project: &Project) -> Option<Install> {
        let pinned = project.engine_version.as_deref()?;
        self.installs
            .iter()
            .filter(|i| match &self.manifest {
                ManifestState::Loaded(m) => m.engine_differs(pinned, &i.version),
                _ => crate::releases::version_key(&i.version) > crate::releases::version_key(pinned),
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
        // Stamp the exact target version (the install dir the Hub chose), so the project's
        // pinned engine reliably re-points even if the binary's own version.json differs.
        let pin = target.version.clone();
        let label = format!("upgrading {} to {}…", project.name, target.version);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = match std::process::Command::new(&bin)
                .arg("--migrate")
                .arg(&path)
                .arg("--engine-version")
                .arg(&pin)
                .status()
            {
                Ok(s) if s.success() => {
                    // The Hub is the authority: re-point the pin even if the target binary
                    // is old and re-stamped its own version.
                    pin_engine_version(&path, &pin);
                    ProcOutcome::Upgraded(idx)
                }
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
        self.poll_hub_update(ctx);
        self.poll_proc();
        self.poll_auth(ctx);
        // Long-running Hubs re-check for new releases every few hours, so the
        // update banner appears without a restart (a failed check just keeps
        // the last manifest; the next interval retries).
        if !matches!(self.manifest, ManifestState::Loading(_))
            && self.job.is_none()
            && self.manifest_fetched_at.elapsed() > std::time::Duration::from_secs(4 * 60 * 60)
        {
            self.start_manifest_fetch();
        }
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
        // Repaint at full rate only for the short, active jobs; the minutes-long device flow
        // and the toast countdown just need a gentle tick so they don't spin the CPU/GPU.
        if self.job.is_some() || self.proc.is_some() || matches!(self.manifest, ManifestState::Loading(_)) {
            ctx.request_repaint();
        } else if self.auth_job.is_some() || self.toast.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }

    // egui 0.35 hands the root `Ui`; panels are shown INTO it (top/bottom first, then the
    // central content).
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("{} Floptle Hub", ico::ROCKET));
                // THE HUB'S OWN VERSION, ALWAYS IN VIEW. Three different things share one
                // version number here — the Hub, the engine, and the engine a project is
                // pinned to — and until this line the only one with its name attached was
                // on the About tab. Somebody reading "0.22.1" in the Installs list had
                // nothing on screen telling them it was a different 0.22.1 from the window
                // they were reading it in.
                if let Some(v) = Self::hub_version() {
                    ui.weak(v).on_hover_text(
                        "the version of the Hub itself — the engine versions in Installs are numbered separately",
                    );
                }
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Projects, format!("{} Projects", ico::PROJECTS));
                ui.selectable_value(&mut self.tab, Tab::News, format!("{} News", ico::NEWS));
                ui.selectable_value(&mut self.tab, Tab::Installs, format!("{} Installs", ico::INSTALLS));
                ui.selectable_value(&mut self.tab, Tab::Settings, format!("{} Settings", ico::SETTINGS));
                ui.selectable_value(&mut self.tab, Tab::About, format!("{} About", ico::ABOUT));

                // THE CHIP THAT NEVER GOES AWAY. Both banners below can be put away — one
                // for the session, one for a version — and this cannot be put away at
                // all. It stays until the update is actually installed. That is the
                // difference between "we told you once" and "you cannot be running
                // something out of date without knowing it".
                //
                // The Hub outranks the engine when both are stale, because an old Hub is
                // what would stop you fixing the other one.
                let hub_new = self.hub_update_available();
                let engine_new = hub_new.is_none().then(|| self.update_available()).flatten();
                if let Some((label, hint, tab)) = hub_new
                    .as_ref()
                    .map(|r| {
                        (
                            format!("{} Hub {} ready", ico::UPGRADE, r.version),
                            "a newer Floptle Hub is available — click for what's in it",
                            Tab::About,
                        )
                    })
                    .or_else(|| {
                        engine_new.as_ref().map(|r| {
                            (
                                format!("{} Floptle {}", ico::UPGRADE, r.version),
                                "a newer engine is available — click to see what's in it",
                                Tab::Installs,
                            )
                        })
                    })
                {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(4.0);
                        if ui
                            .button(
                                egui::RichText::new(label)
                                    .color(egui::Color32::from_rgb(120, 200, 130)),
                            )
                            .on_hover_text(hint)
                            .clicked()
                        {
                            self.tab = tab;
                            self.hub_update_hidden = false;
                            if tab == Tab::Installs
                                && let Some(r) = &engine_new
                            {
                                self.selected_version = Some(r.version.clone());
                            }
                        }
                    });
                }
            });
        });

        // THE HUB ITSELF IS OUT OF DATE. Above the engine banner, because an old Hub is
        // the thing that would stop the rest of this working — and it is the one update a
        // user cannot perform any other way without leaving the app.
        if let Some(r) = self.hub_update_available()
            && !self.hub_update_hidden
        {
            let mut go: Option<crate::releases::Artifact> = None;
            let blocked = crate::selfupdate::can_self_update().err();
            egui::Panel::top("hub-update-banner").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(120, 200, 130),
                        format!("{} Floptle Hub {} is available", ico::UPGRADE, r.version),
                    );
                    if !r.title.is_empty() {
                        ui.small(format!("“{}”", r.title));
                    }
                    match (&blocked, &self.hub_update_job) {
                        (_, Some(job)) => {
                            ui.small(&job.line);
                            ui.add(egui::ProgressBar::new(job.frac).desired_width(120.0).desired_height(8.0));
                        }
                        (None, None) => {
                            if ui
                                .button(format!("{} Update and restart", ico::INSTALL))
                                .on_hover_text("downloads it, checks it, and reopens the Hub")
                                .clicked()
                                && let Some(a) = r.hub_artifact_here().cloned()
                            {
                                go = Some(a);
                            }
                            if ui.small_button("What's new").clicked() {
                                self.tab = Tab::About;
                            }
                            if ui.small_button("Later").on_hover_text("until you next open the Hub").clicked() {
                                self.hub_update_hidden = true;
                            }
                        }
                        // Can't self-update here — say why, and point at the download
                        // rather than showing a button that would fail.
                        (Some(b), None) => {
                            ui.small(b.message());
                            ui.hyperlink_to("download", RELEASES_URL);
                            if ui.small_button("Later").clicked() {
                                self.hub_update_hidden = true;
                            }
                        }
                    }
                });
            });
            if let Some(a) = go {
                self.start_hub_update(a);
            }
        }

        // UPDATE BANNER: a new engine version on the user's channel, newer than
        // anything installed, with a bundle for this platform. One click
        // installs; ✖ mutes the banner for THAT version (anything newer brings
        // it back). This is how users get notified of releases.
        if let Some(r) = self.update_available()
            && self.config.settings.dismissed_update.as_deref() != Some(r.version.as_str())
        {
            let mut act: Option<(bool, String)> = None; // (install?, version)
            egui::Panel::top("update-banner").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(
                        egui::Color32::LIGHT_GREEN,
                        format!("{} Floptle {} is available", ico::UPGRADE, r.version),
                    );
                    if !r.date.is_empty() {
                        ui.small(&r.date);
                    }
                    if self.job.is_none()
                        && ui.button(format!("{} Install now", ico::INSTALL)).clicked()
                    {
                        act = Some((true, r.version.clone()));
                    }
                    if !r.notes_url.is_empty() {
                        ui.hyperlink_to("notes", &r.notes_url);
                    }
                    if ui
                        .small_button(ico::CLOSE)
                        .on_hover_text("hide until the next version")
                        .clicked()
                    {
                        act = Some((false, r.version.clone()));
                    }
                });
            });
            match act {
                Some((true, v)) => {
                    if let Some(art) = r.artifact_here().cloned() {
                        self.start_install(v, art);
                    }
                }
                Some((false, v)) => {
                    self.config.settings.dismissed_update = Some(v);
                    self.save();
                }
                None => {}
            }
        }

        if let Some((msg, is_err)) = self.toast.clone() {
            let mut dismiss = false;
            egui::Panel::bottom("toast").show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.small_button(ico::CLOSE).clicked() {
                        dismiss = true;
                    }
                    let (color, mark) = if is_err {
                        (egui::Color32::LIGHT_RED, ico::WARN)
                    } else {
                        (egui::Color32::LIGHT_GREEN, ico::OK)
                    };
                    ui.colored_label(color, format!("{mark} {msg}"));
                });
            });
            if dismiss {
                self.toast = None;
                self.toast_seen = None;
            }
        }

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Projects => self.projects_tab(ui),
            Tab::News => self.news_tab(ui),
            Tab::Installs => self.installs_tab(ui),
            Tab::Settings => self.settings_tab(ui),
            Tab::About => self.about_tab(ui),
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
            if ui.add_enabled(!busy, egui::Button::new(format!("{} New project", ico::NEW))).clicked() {
                let version = self
                    .config
                    .settings
                    .default_version
                    .clone()
                    .or_else(|| self.installs.last().map(|i| i.version.clone()))
                    .unwrap_or_default();
                // Prefill the location with the remembered/default projects folder.
                let location = self.config.settings.projects_dir.clone().unwrap_or_default();
                self.new_project = Some(NewProjectForm { version, location, ..Default::default() });
            }
            ui.separator();
            ui.label("or add existing:");
            ui.text_edit_singleline(&mut self.add_path);
            if ui.button(format!("{} Add", ico::NEW)).clicked() {
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
            let mut reveal_loc = false;
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(format!("{} New project", ico::NEW));
                egui::Grid::new("new-proj-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut form.name);
                    ui.end_row();

                    ui.label("Location");
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut form.location);
                        if ui.button(ico::REVEAL).on_hover_text("open this folder in your file manager").clicked() {
                            reveal_loc = true;
                        }
                    });
                    ui.end_row();

                    ui.label("Engine");
                    egui::ComboBox::from_id_salt("new-proj-version")
                        .selected_text(if form.version.is_empty() { "(none installed)".into() } else { form.version.clone() })
                        .show_ui(ui, |ui| {
                            for i in &self.installs {
                                ui.selectable_value(&mut form.version, i.version.clone(), &i.version);
                            }
                        });
                    ui.end_row();
                });
                // Show exactly where it lands, so there are no surprises.
                if !form.name.trim().is_empty() && !form.location.trim().is_empty() {
                    let dest = PathBuf::from(form.location.trim()).join(form.name.trim());
                    ui.small(format!("will create {}", dest.display()));
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new(format!("{} Create", ico::OK))).clicked()
                        && self.start_create(&form)
                    {
                        keep = false;
                    }
                    if ui.button(format!("{} Cancel", ico::CLOSE)).clicked() {
                        keep = false;
                    }
                });
            });
            if reveal_loc {
                // A "show in file manager" affordance must never write to disk. Open the
                // folder if it exists, else its parent (so the user can see where it'll
                // land); the folder itself is created only on Create. Don't climb past the
                // parent — silently opening a far-off ancestor (or `/`) for a typo'd path is
                // more confusing than a toast.
                let trimmed = form.location.trim();
                let loc = PathBuf::from(trimmed);
                let target = if trimmed.is_empty() {
                    None
                } else if loc.is_dir() {
                    Some(loc.clone())
                } else {
                    loc.parent().filter(|p| p.is_dir()).map(|p| p.to_path_buf())
                };
                match target {
                    Some(dir) => {
                        if let Err(e) = launch::reveal(&dir) {
                            self.toast = Some((e, true));
                        }
                    }
                    None => {
                        self.toast = Some((
                            format!("{} doesn't exist yet — it'll be created when you click Create", loc.display()),
                            true,
                        ));
                    }
                }
            }
            if keep {
                self.new_project = Some(form);
            }
        }

        ui.separator();
        if self.config.projects.is_empty() {
            ui.add_space(8.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(ico::PROJECTS).size(28.0).weak());
                ui.label("No projects yet.");
                ui.small("Create one above, or add an existing project folder.");
            });
            return;
        }

        // Precompute per-project upgrade targets so the loop only reads immutably.
        let upgrades: Vec<Option<Install>> =
            self.config.projects.iter().map(|p| self.upgrade_target(p)).collect();
        let mut launch_idx = None;
        let mut remove = None;
        let mut reveal_idx = None;
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
                            let (mark, color) = if !p.exists() {
                                (format!("{} folder missing", ico::WARN), egui::Color32::LIGHT_RED)
                            } else if installed {
                                (format!("engine {}", ico::OK), egui::Color32::LIGHT_GREEN)
                            } else {
                                (format!("{} engine not installed", ico::WARN), egui::Color32::from_rgb(230, 180, 90))
                            };
                            ui.horizontal(|ui| {
                                ui.small(format!("engine: {ver}  ·"));
                                ui.small(egui::RichText::new(mark).color(color));
                            });
                        });
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(ico::REMOVE).on_hover_text("remove from Hub (doesn't delete files)").clicked() {
                                remove = Some(idx);
                            }
                            if ui.add_enabled(p.exists(), egui::Button::new(ico::REVEAL))
                                .on_hover_text("show the project folder in your file manager")
                                .clicked()
                            {
                                reveal_idx = Some(idx);
                            }
                            if ui.add_enabled(p.exists(), egui::Button::new(format!("{} Open", ico::OPEN))).clicked() {
                                launch_idx = Some(idx);
                            }
                            if let Some(target) = &upgrades[idx]
                                && p.exists()
                                // "⬆ 0.22.1" was a bare number on a screen with three
                                // different things numbered the same way. Name the one it
                                // means.
                                && ui
                                    .add_enabled(!busy, egui::Button::new(format!("{} Engine {}", ico::UPGRADE, target.version)))
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
        if let Some(idx) = reveal_idx
            && let Some(p) = self.config.projects.get(idx)
            && let Err(e) = launch::reveal(&p.path)
        {
            self.toast = Some((e, true));
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

    /// Versions, and what each one was. A list on the left, the selected release's
    /// notes on the right.
    ///
    /// The notes are the point of the redesign. The Hub used to show a version as a
    /// number, a date and an Install button, which told somebody deciding whether to
    /// upgrade precisely nothing — the answer lived on a GitHub page they had to go and
    /// find. They ship in the manifest now (`docs/releases/vX.Y.Z.md`, embedded at
    /// publish time), so every release explains itself here, including the ones already
    /// installed and the ones from before this existed.
    fn installs_tab(&mut self, ui: &mut egui::Ui) {
        let rows = self.version_rows();

        // Default the selection to the newest version there is. Somebody opening this
        // tab is nearly always asking about the newest release, and an empty right-hand
        // pane on arrival would make the notes look like something you have to hunt for.
        if self.selected_version.as_ref().is_none_or(|v| !rows.iter().any(|r| &r.version == v)) {
            self.selected_version = rows.first().map(|r| r.version.clone());
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong("Engine versions");
            let loading = matches!(self.manifest, ManifestState::Loading(_));
            if ui
                .add_enabled(!loading, egui::Button::new(format!("{} Check for versions", ico::REFRESH)))
                .clicked()
            {
                self.start_manifest_fetch();
            }
            if loading {
                ui.spinner();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add_space(6.0);
                ui.small(format!("{} channel", self.config.settings.channel));
            });
        });
        // SAY WHICH VERSIONS THESE ARE. "Engine versions" alone left the reader to work out
        // that the Hub is not in this list and does not update from it — and the Hub's own
        // updates arrive as a banner, from the same release, wearing the same number.
        ui.small(egui::RichText::new(
            "The engine your projects run. The Hub updates itself, separately from this list.",
        ).weak());

        // A running install job, above the split so it stays put while you browse.
        if let Some(job) = &self.job {
            ui.add_space(2.0);
            ui.small(format!("installing {} — {}", job.version, job.line));
            ui.add(egui::ProgressBar::new(job.frac).desired_height(6.0));
        }
        if let ManifestState::Error(e) = &self.manifest {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("{} could not load versions: {e}", ico::WARN));
        }
        ui.add_space(6.0);

        if rows.is_empty() {
            ui.label(match self.manifest {
                ManifestState::Loading(_) => "fetching the version list…",
                _ => "No versions yet — check for versions to see what there is to install.",
            });
            return;
        }

        // Actions are collected and applied AFTER the panes: every one of them mutates
        // `self`, and the panes are holding a borrow of the row list built from it.
        let mut to_install: Option<(String, crate::releases::Artifact)> = None;
        let mut set_default: Option<String> = None;
        let mut uninstall: Option<Install> = None;
        let mut reveal: Option<PathBuf> = None;
        let mut select: Option<String> = None;

        let newest_installed =
            self.installs.iter().map(|i| crate::releases::version_key(&i.version)).max();
        let selected = self.selected_version.clone().unwrap_or_default();
        let busy = self.job.is_some();

        // Two columns, hand-allocated rather than an egui SidePanel: a panel wants to be
        // a child of a window or another panel, and this is already inside the tab body.
        //
        // The list takes a QUARTER of the tab rather than a fixed 196 px. At 196 the
        // column was narrower than the thing it lists on a wide window — a stripe of
        // version numbers pinned to the edge with the notes sprawling beside it — and on
        // a narrow one the state word wrapped under the date. A share of the width reads
        // as a column at any size. The bounds stop it becoming a stripe on an ultrawide
        // or eating the notes on a small window.
        let split_height = ui.available_height();
        let list_w = (ui.available_width() * 0.26).clamp(220.0, 320.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(list_w, split_height),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                egui::ScrollArea::vertical().id_salt("version-list").show(ui, |ui| {
                    for r in &rows {
                        // "new" means a new ENGINE. A Hub-only release is newer than
                        // everything installed and still has nothing in it for this list.
                        let is_new = !r.hub_only
                            && newest_installed
                                .as_ref()
                                .is_some_and(|n| crate::releases::version_key(&r.version) > *n);

                        // ONE ROW, ONE HIT TARGET. This used to be a `selectable_label`
                        // for the version and an unclickable line of state under it — so
                        // the actual target was the width of the text "0.21.0" and one
                        // line tall, with dead space around it that looked clickable and
                        // wasn't. The whole card takes the click now: full column width,
                        // both lines, and the padding.
                        let row_h = if r.title.is_empty() { 42.0 } else { 46.0 };
                        let (rect, resp) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), row_h),
                            egui::Sense::click(),
                        );
                        let active = r.version == selected;
                        if active || resp.hovered() {
                            let v = ui.visuals();
                            ui.painter().rect_filled(
                                rect,
                                4.0,
                                if active { v.selection.bg_fill } else { v.widgets.hovered.bg_fill },
                            );
                        }
                        let mut inner = ui.new_child(
                            egui::UiBuilder::new()
                                .max_rect(rect.shrink2(egui::vec2(8.0, 5.0)))
                                .layout(egui::Layout::top_down(egui::Align::Min)),
                        );
                        inner.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            let mut label = egui::RichText::new(&r.version);
                            if r.installed.is_some() {
                                label = label.strong();
                            }
                            ui.label(label);
                            // WORDS, NOT GLYPHS. The Hub ships egui's default fonts, which
                            // have no ● and no ✔ — both draw as an empty box, and a list of
                            // empty boxes is worse than no marker at all. "installed" also
                            // needs no legend.
                            if r.is_default {
                                ui.small(
                                    egui::RichText::new("default")
                                        .color(ui.visuals().hyperlink_color)
                                        .strong(),
                                );
                            } else if r.installed.is_some() {
                                ui.small(egui::RichText::new("installed").strong());
                            } else if is_new {
                                ui.small(
                                    egui::RichText::new("new")
                                        .color(egui::Color32::from_rgb(120, 200, 130)),
                                );
                            } else if r.hub_only {
                                ui.small(egui::RichText::new("Hub only").weak());
                            }
                        });
                        // The release NAME, which the column had no room for before — it is
                        // what somebody actually remembers a version by.
                        inner.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 6.0;
                            if !r.title.is_empty() {
                                ui.weak(egui::RichText::new(format!("“{}”", r.title)).small());
                            }
                            if !r.date.is_empty() {
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.weak(egui::RichText::new(&r.date).small());
                                    },
                                );
                            }
                        });
                        if resp.clicked() {
                            select = Some(r.version.clone());
                        }
                        ui.add_space(2.0);
                    }
                });
                },
            );
            ui.separator();
            ui.vertical(|ui| {
            let Some(r) = rows.iter().find(|r| r.version == selected) else { return };
            // A right margin, so a heading or a wrapped line never runs into the edge
            // of the window.
            ui.set_max_width((ui.available_width() - 8.0).max(120.0));

            ui.horizontal(|ui| {
                ui.heading(&r.version);
                if !r.title.is_empty() {
                    ui.heading(egui::RichText::new(format!("“{}”", r.title)).weak());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add_space(6.0);
                    if !r.date.is_empty() {
                        ui.small(&r.date);
                    }
                });
            });

            // SAY IT BEFORE THE INSTALL BUTTON, NOT IN THE NOTES BELOW IT. v0.22.1's notes
            // did say the engine was unchanged — in an "Upgrading" section under several
            // screens of Hub changes, directly contradicted by the Install button at the
            // top of the same pane. One line, where the decision is actually made.
            if r.hub_only {
                ui.add_space(4.0);
                ui.small(egui::RichText::new(match self.engine_behind(&r.version) {
                    Some(v) => format!(
                        "A Hub release — the engine in it is the same one as {v}, so installing it changes nothing about how your projects run."
                    ),
                    None => "A Hub release — it changed the Hub, not the engine.".to_string(),
                }).weak());
            }

            // BUTTONS YOU CAN HIT. These were egui's defaults — text plus a few pixels of
            // padding, so "Install" was a ~60×20 target for the primary action of the
            // whole tab. A minimum size makes every one of them a deliberate object
            // rather than a word with a box round it, and the row gets breathing space
            // above and below so it stops reading as part of the heading.
            // A broken install is a sentence, not a chip wedged between two buttons. On
            // its own line it reads; inline it looked like a third control.
            if let Some(inst) = &r.installed
                && !inst.is_valid()
            {
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::LIGHT_RED,
                    format!("{} this install is incomplete — uninstall it and install it again", ico::WARN),
                );
            }

            ui.add_space(8.0);
            let btn = |ui: &mut egui::Ui, label: String| {
                ui.add_sized([132.0, 30.0], egui::Button::new(label))
            };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                match (&r.installed, &r.artifact) {
                    (Some(inst), _) => {
                        if !r.is_default && btn(ui, format!("{} Set default", ico::STAR)).clicked() {
                            set_default = Some(r.version.clone());
                        }
                        if btn(ui, format!("{} Show files", ico::REVEAL))
                            .on_hover_text("show this install in your file manager")
                            .clicked()
                        {
                            reveal = Some(inst.path.clone());
                        }
                        if btn(ui, format!("{} Uninstall", ico::REMOVE)).clicked() {
                            uninstall = Some(inst.clone());
                        }
                    }
                    (None, Some(art)) => {
                        if ui
                            .add_enabled_ui(!busy, |ui| {
                                ui.add_sized(
                                    [132.0, 30.0],
                                    egui::Button::new(
                                        egui::RichText::new(format!("{} Install", ico::INSTALL))
                                            .strong(),
                                    ),
                                )
                            })
                            .inner
                            .clicked()
                        {
                            to_install = Some((r.version.clone(), art.clone()));
                        }
                        ui.weak(format!("{:.0} MB download", art.size as f64 / 1_048_576.0));
                    }
                    (None, None) => {
                        ui.small(format!(
                            "{} no build for {} in this release",
                            ico::WARN,
                            crate::releases::platform_target()
                        ));
                    }
                }
            });
            ui.add_space(4.0);

            ui.add_space(6.0);
            ui.separator();

            // Looked up now rather than carried on every row: the rows are rebuilt every
            // frame and the notes are a few KB each, so copying all of them to draw one
            // was ~240 KB per frame of pure waste.
            let notes = match &self.manifest {
                ManifestState::Loaded(m) => m.release(&r.version).map(|x| x.notes.as_str()),
                _ => None,
            }
            .unwrap_or("");
            egui::ScrollArea::vertical().id_salt(("notes", &r.version)).show(ui, |ui| {
                if notes.trim().is_empty() {
                    ui.add_space(10.0);
                    // Honest about WHY rather than silent: the six earliest releases
                    // predate release notes entirely, and a blank pane reads as a Hub
                    // that failed to load something.
                    ui.weak("No release notes for this version.");
                    if !r.notes_url.is_empty() {
                        ui.add_space(4.0);
                        ui.hyperlink_to(
                            format!("{} the release page", ico::GLOBE),
                            r.notes_url.clone(),
                        );
                    }
                } else {
                    crate::notes::render(ui, notes);
                    if !r.notes_url.is_empty() {
                        ui.add_space(10.0);
                        ui.hyperlink_to(
                            format!("{} this release on the web", ico::GLOBE),
                            r.notes_url.clone(),
                        );
                    }
                    ui.add_space(12.0);
                }
            });
            });
        });

        if let Some(v) = select {
            self.selected_version = Some(v);
        }
        if let Some(v) = set_default {
            self.config.settings.default_version = Some(v);
            self.save();
        }
        if let Some(p) = reveal
            && let Err(e) = launch::reveal(&p)
        {
            self.toast = Some((e, true));
        }
        if let Some(i) = uninstall {
            let _ = std::fs::remove_dir_all(&i.path);
            if self.config.settings.default_version.as_deref() == Some(i.version.as_str()) {
                self.config.settings.default_version = None;
            }
            self.rescan_installs();
            self.save();
            self.toast = Some((format!("uninstalled {}", i.version), false));
        }
        if let Some((v, art)) = to_install {
            self.start_install(v, art);
        }
    }

    /// What the engine is working on, working towards, and just shipped.
    ///
    /// Release notes answer "what changed in 0.22.0". They cannot answer "is this thing
    /// alive and where is it going", which is what somebody deciding whether to build on
    /// an engine wants to know, and which they would otherwise go looking for on a
    /// website. It rides `releases.json`, so it costs no extra request and reads from
    /// cache with the network off.
    fn news_tab(&mut self, ui: &mut egui::Ui) {
        let news = match &self.manifest {
            ManifestState::Loaded(m) => m.news.clone(),
            _ => String::new(),
        };
        let latest = self.newest_release().map(|r| {
            (r.version.clone(), r.title.clone(), r.hub_artifact_here().is_some())
        });
        let installed_latest = latest.as_ref().is_some_and(|(v, _, _)| {
            self.installs.iter().any(|i| &i.version == v)
        });
        let mut go_installs = false;

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.strong(format!("{} What's happening", ico::NEWS));
            let loading = matches!(self.manifest, ManifestState::Loading(_));
            if ui
                .add_enabled(!loading, egui::Button::new(format!("{} Refresh", ico::REFRESH)))
                .clicked()
            {
                self.start_manifest_fetch();
            }
            if loading {
                ui.spinner();
            }
        });
        ui.add_space(8.0);

        egui::ScrollArea::vertical().id_salt("news").show(ui, |ui| {
            ui.set_max_width((ui.available_width() - 8.0).max(200.0));

            // The latest release as a card at the top, with the one action that follows
            // from reading about it. News that tells you a version exists and then makes
            // you find the Installs tab yourself is news that wasted your time.
            if let Some((version, title, _)) = &latest {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width() - 16.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.heading(format!("Floptle {version}"));
                        if !title.is_empty() {
                            ui.heading(egui::RichText::new(format!("“{title}”")).weak());
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                        if installed_latest {
                            ui.weak("This is the newest release, and you have it.");
                        } else if ui
                            .add_sized(
                                [176.0, 32.0],
                                egui::Button::new(
                                    egui::RichText::new(format!("{} Get {version}", ico::INSTALL))
                                        .strong(),
                                ),
                            )
                            .clicked()
                        {
                            go_installs = true;
                        }
                        if ui.add_sized([148.0, 32.0], egui::Button::new("Read the notes")).clicked()
                        {
                            go_installs = true;
                        }
                    });
                });
                ui.add_space(12.0);
            }

            if news.trim().is_empty() {
                // Honest about which of the two it is. A Hub that has never reached the
                // network and one whose manifest predates this field look identical from
                // here, and only the first is worth acting on.
                ui.weak(match self.manifest {
                    ManifestState::Loaded(_) => {
                        "No news in this version list yet — it arrives with the next release."
                    }
                    ManifestState::Loading(_) => "fetching…",
                    _ => "Couldn't reach the version list, so there's no news to show.",
                });
                ui.add_space(8.0);
                ui.hyperlink_to(format!("{} releases on the web", ico::GLOBE), RELEASES_URL);
            } else {
                crate::notes::render(ui, &news);
            }

            ui.add_space(16.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = 14.0;
                ui.hyperlink_to(format!("{} All releases", ico::GLOBE), RELEASES_URL);
                ui.hyperlink_to(format!("{} Documentation", ico::BOOK), DOCS_URL);
                ui.hyperlink_to(format!("{} Report a problem", ico::BUG), ISSUES_URL);
            });
            ui.add_space(12.0);
        });

        if go_installs {
            if let Some((v, _, _)) = latest {
                self.selected_version = Some(v);
            }
            self.tab = Tab::Installs;
        }
    }

    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        self.account_section(ui);
        ui.add_space(6.0);
        ui.strong(format!("{} Settings", ico::SETTINGS));
        let mut changed = false;
        let mut reveal_data = false;
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

            ui.label("Account (auth) URL");
            let r = ui.text_edit_singleline(&mut self.config.settings.auth_base_url);
            changed |= r.changed();
            r.on_hover_text("fopull.com in production; point at a dev instance to test sign-in");
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

            ui.label("New-project folder");
            let mut dir = self.config.settings.projects_dir.clone().unwrap_or_default();
            if ui.text_edit_singleline(&mut dir).changed() {
                self.config.settings.projects_dir = Some(dir);
                changed = true;
            }
            ui.end_row();

            ui.label("Auth token (session)");
            ui.add(egui::TextEdit::singleline(&mut self.token).password(true).hint_text("for a private repo — not saved"));
            ui.end_row();

            ui.label("Data folder");
            ui.horizontal(|ui| {
                ui.small(self.paths.data.display().to_string());
                if ui.small_button(ico::REVEAL).on_hover_text("open the Hub data folder").clicked() {
                    reveal_data = true;
                }
            });
            ui.end_row();
        });
        if changed {
            self.save();
        }
        if reveal_data && let Err(e) = launch::reveal(&self.paths.data) {
            self.toast = Some((e, true));
        }
        ui.separator();
        ui.small("Token is used only this session (a keyring store is a later hardening step). Point the manifest URL at a local releases.json to test against a locally-packaged bundle.");
    }

    fn about_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.vertical_centered(|ui| {
            ui.label(egui::RichText::new(ico::ROCKET).size(40.0));
            ui.heading("Floptle Hub");
            let v = env!("CARGO_PKG_VERSION");
            ui.label(if v == "0.0.0" { "dev build".to_string() } else { format!("version {v}") });
            ui.small(format!("platform: {}", crate::releases::platform_target()));
        });
        ui.add_space(8.0);

        // WHETHER THIS HUB IS CURRENT, answered plainly. "version 0.21.1" alone is a fact
        // nobody can act on — it only means something next to the version that exists.
        let update = self.hub_update_available();
        let mut go: Option<crate::releases::Artifact> = None;
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            match (&update, Self::hub_version()) {
                (None, Some(_)) => match self.manifest {
                    ManifestState::Loaded(_) => {
                        ui.horizontal(|ui| {
                            ui.colored_label(egui::Color32::LIGHT_GREEN, ico::OK);
                            ui.label("This is the newest Hub.");
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.add_space(4.0);
                                if ui.small_button(format!("{} Check again", ico::REFRESH)).clicked() {
                                    self.start_manifest_fetch();
                                }
                            });
                        });
                    }
                    _ => {
                        ui.horizontal(|ui| {
                            ui.small("haven't been able to check for a newer Hub yet");
                            if ui.small_button(format!("{} Check now", ico::REFRESH)).clicked() {
                                self.start_manifest_fetch();
                            }
                        });
                    }
                },
                (None, None) => {
                    ui.small("A dev build never updates itself — cargo owns this binary.");
                }
                (Some(r), _) => {
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(
                            egui::Color32::from_rgb(120, 200, 130),
                            egui::RichText::new(format!("Hub {} is available", r.version)).strong(),
                        );
                        if !r.title.is_empty() {
                            ui.label(egui::RichText::new(format!("“{}”", r.title)).weak());
                        }
                        match (crate::selfupdate::can_self_update(), &self.hub_update_job) {
                            (_, Some(job)) => {
                                ui.small(&job.line);
                                ui.add(egui::ProgressBar::new(job.frac).desired_width(140.0).desired_height(8.0));
                            }
                            (Ok(_), None) => {
                                if ui.button(format!("{} Update and restart", ico::INSTALL)).clicked()
                                    && let Some(a) = r.hub_artifact_here().cloned()
                                {
                                    go = Some(a);
                                }
                            }
                            (Err(b), None) => {
                                ui.small(b.message());
                                ui.hyperlink_to("download it", RELEASES_URL);
                            }
                        }
                    });
                    if !r.notes.trim().is_empty() {
                        ui.add_space(4.0);
                        egui::ScrollArea::vertical().max_height(220.0).id_salt("hub-update-notes").show(
                            ui,
                            |ui| crate::notes::render(ui, &r.notes),
                        );
                    }
                }
            }
        });
        if let Some(a) = go {
            self.start_hub_update(a);
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);

        egui::Grid::new("about-links").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
            ui.label(format!("{} Website", ico::GLOBE));
            ui.hyperlink_to(WEBSITE_URL, WEBSITE_URL);
            ui.end_row();

            ui.label(format!("{} Source code", ico::BOOK));
            ui.hyperlink_to(REPO_URL, REPO_URL);
            ui.end_row();

            ui.label(format!("{} Downloads", ico::INSTALLS));
            ui.hyperlink_to(RELEASES_URL, RELEASES_URL);
            ui.end_row();

            ui.label(format!("{} Report an issue", ico::BUG));
            ui.hyperlink_to(ISSUES_URL, ISSUES_URL);
            ui.end_row();
        });

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.small("Floptle is open source. Contributions, bug reports, and ideas are welcome.");
            ui.small("Built with Rust, wgpu, and egui.");
            ui.add_space(4.0);
            ui.small(format!("© 2026 {COMPANY}. All rights reserved."));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::pin_engine_version;

    /// Render the Installs tab to a PNG so a layout change can be LOOKED AT.
    ///
    /// Ignored: it needs a GPU, and CI has none. Run it deliberately —
    /// `cargo test -p floptle-hub -- --ignored --nocapture` — and open the path it
    /// prints. The same rule as the render crate's `*_probe` examples: a visual change
    /// that was only reasoned about is a visual change that was not checked.
    #[test]
    #[ignore = "renders a PNG for eyeballing; needs a GPU"]
    fn snapshot_the_installs_tab() {
        use super::*;

        // A manifest with the real v0.21.0 notes, so the snapshot shows what a release
        // actually looks like rather than filler that happens to fit.
        let notes = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/releases/v0.21.0.md"),
        )
        .unwrap();
        let body = notes.split_once('\n').map(|(_, b)| b.trim_start()).unwrap_or("").to_string();

        let build = |selected: &str| {
            let tmp = tempfile::tempdir().unwrap();
            let mut app = HubApp::new(Paths::at(tmp.path()));
            let mut m = Manifest { schema: 1, ..Default::default() };
            // 0.21.1 is HUB-ONLY on purpose: the "Hub only" chip and the line that tells you
            // which engine is really in it are the whole point of this snapshot.
            for (v, date, title) in [
                ("0.21.1", "2026-08-03", "Front Page"),
                ("0.21.0", "2026-08-03", "Who's Playing"),
                ("0.20.0", "2026-08-02", "Say It Simply"),
                ("0.19.2", "2026-07-31", "Stay Put"),
                ("0.19.1", "2026-07-31", "Say Which"),
            ] {
                m.versions.push(crate::releases::ReleaseInfo {
                    version: v.into(),
                    channel: "stable".into(),
                    date: date.into(),
                    notes_url: format!("https://example.invalid/v{v}"),
                    title: title.into(),
                    changed: if v == "0.21.1" {
                        vec!["hub".into()]
                    } else {
                        vec!["engine".into(), "hub".into()]
                    },
                    notes: if v == "0.21.0" { body.clone() } else { String::new() },
                    artifacts: [(
                        crate::releases::platform_target(),
                        crate::releases::Artifact {
                            url: "u".into(),
                            sha256: "s".into(),
                            size: 13_400_000,
                        },
                    )]
                    .into_iter()
                    .collect(),
                    hub_artifacts: Default::default(),
                });
            }
            app.manifest = ManifestState::Loaded(m);
            app.installs =
                vec![Install { version: "0.20.0".into(), path: tmp.path().join("versions/0.20.0") }];
            app.config.settings.default_version = Some("0.20.0".into());
            app.selected_version = Some(selected.to_string());
            // The temp dir has to outlive the render — the paths are read while drawing.
            (app, tmp)
        };

        // Both halves of the tab: a release you could install, one you already have (whose
        // buttons and empty-notes state are a different screen entirely), and a Hub-only
        // one, which has to explain itself before you reach the Install button.
        for (which, selected) in [("new", "0.21.0"), ("installed", "0.20.0"), ("hub-only", "0.21.1")]
        {
            let (mut app, _tmp) = build(selected);
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(960.0, 620.0))
                .build_ui(move |ui| app.installs_tab(ui));
            harness.run();
            let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../target/installs-tab-{which}.png"));
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            harness.render().expect("no GPU?").save(&out).unwrap();
            println!("wrote {}", out.display());
        }

        // The News tab, with the real docs/news.md — so the snapshot shows the page
        // somebody will actually read, and the 📰 in the tab strip gets LOOKED AT. The
        // Hub's fonts have holes in them and a tofu box passes every non-visual test
        // there is: right layout, right string, rectangular pixels.
        {
            let (mut app, _tmp) = build("0.21.0");
            let news = std::fs::read_to_string(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/news.md"),
            )
            .unwrap_or_default();
            if let ManifestState::Loaded(m) = &mut app.manifest {
                m.news = news;
            }
            app.tab = Tab::News;
            let mut harness = egui_kittest::Harness::builder()
                .with_size(egui::vec2(960.0, 720.0))
                .build_ui(move |ui| {
                    // The whole chrome, not just the body: the tab strip is where the new
                    // glyph lives.
                    use eframe::App as _;
                    let mut frame = eframe::Frame::_new_kittest();
                    app.ui(ui, &mut frame);
                });
            harness.run();
            let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/news-tab.png");
            std::fs::create_dir_all(out.parent().unwrap()).unwrap();
            harness.render().expect("no GPU?").save(&out).unwrap();
            println!("wrote {}", out.display());
        }

        // And the About tab, which is where "is this Hub current" gets answered. Needs a
        // release NEWER than this build carrying a HUB artifact, or the honest answer is
        // "up to date" and the card under test never draws.
        let (mut app, _tmp) = build("0.21.0");
        if let ManifestState::Loaded(m) = &mut app.manifest {
            let mut newer = m.versions[0].clone();
            newer.version = "99.0.0".into();
            newer.title = "Later Than This".into();
            newer.date = "2026-09-01".into();
            newer.hub_artifacts.insert(
                crate::releases::platform_target(),
                crate::releases::Artifact { url: "u".into(), sha256: "s".into(), size: 6_200_000 },
            );
            m.versions.insert(0, newer);
        }
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(960.0, 620.0))
            .build_ui(move |ui| app.about_tab(ui));
        harness.run();
        let out =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/about-update.png");
        harness.render().expect("no GPU?").save(&out).unwrap();
        println!("wrote {}", out.display());
    }

    /// Ty's report, exactly: 0.22.1 changed only the Hub, and the Projects tab offered to
    /// migrate every project onto it as if it were a new engine. The offer has to survive
    /// where it's real (0.21.2 skipped 0.22.0, which WAS an engine release) and disappear
    /// where it isn't (already on the engine 0.22.1 carries).
    #[test]
    fn a_hub_only_release_is_not_offered_as_a_project_upgrade() {
        use super::*;

        let tmp = tempfile::tempdir().unwrap();
        let mut app = HubApp::new(Paths::at(tmp.path()));
        let mut m = Manifest { schema: 1, ..Default::default() };
        for (v, changed) in [
            ("0.21.2", vec!["engine", "hub"]),
            ("0.22.0", vec!["engine", "hub"]),
            ("0.22.1", vec!["hub"]),
        ] {
            m.versions.push(crate::releases::ReleaseInfo {
                version: v.into(),
                channel: "stable".into(),
                changed: changed.into_iter().map(String::from).collect(),
                date: String::new(),
                notes_url: String::new(),
                title: String::new(),
                notes: String::new(),
                artifacts: Default::default(),
                hub_artifacts: Default::default(),
            });
        }
        app.manifest = ManifestState::Loaded(m);
        app.installs = ["0.21.2", "0.22.1"]
            .into_iter()
            .map(|v| Install { version: v.into(), path: tmp.path().join("versions").join(v) })
            .collect();

        let project = |v: &str| Project {
            name: "p".into(),
            path: tmp.path().to_path_buf(),
            engine_version: Some(v.into()),
            last_opened: None,
        };

        // Behind by a real engine release — 0.22.0 is inside the range, so the jump to
        // 0.22.1 genuinely changes the engine and the offer stands.
        assert_eq!(
            app.upgrade_target(&project("0.21.2")).map(|i| i.version),
            Some("0.22.1".into())
        );
        // Already on the engine 0.22.1 ships. Nothing to do, and the button that said
        // otherwise is what made the version numbers look wrong.
        assert!(app.upgrade_target(&project("0.22.0")).is_none());
        assert!(app.upgrade_target(&project("0.22.1")).is_none());
        // An unpinned project is never migrated by this button.
        assert!(app.upgrade_target(&project_unpinned(tmp.path())).is_none());

        // …and with no manifest (offline, or a first run) it falls back to comparing
        // numbers, exactly as it did before any of this existed.
        app.manifest = ManifestState::Idle;
        assert_eq!(
            app.upgrade_target(&project("0.22.0")).map(|i| i.version),
            Some("0.22.1".into())
        );
    }

    fn project_unpinned(path: &std::path::Path) -> super::Project {
        super::Project {
            name: "p".into(),
            path: path.to_path_buf(),
            engine_version: None,
            last_opened: None,
        }
    }

    #[test]
    fn pin_corrects_a_stale_engine_version() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_path = tmp.path().join("project.ron");
        // Simulate what an OLD editor binary wrote: pinned to the workspace 0.0.0.
        let stale = floptle_scene::ProjectConfigDoc {
            engine_version: Some("0.0.0".into()),
            ..floptle_scene::ProjectConfigDoc::default()
        };
        floptle_scene::save_project(&stale, &cfg_path).unwrap();

        // The Hub corrects it to the version it actually installed.
        pin_engine_version(tmp.path(), "0.1.0");
        assert_eq!(
            floptle_scene::load_project(&cfg_path).engine_version.as_deref(),
            Some("0.1.0")
        );
    }

    #[test]
    fn pin_is_a_noop_without_a_config() {
        let tmp = tempfile::tempdir().unwrap();
        // No project.ron — nothing to correct, and nothing is fabricated.
        pin_engine_version(tmp.path(), "0.1.0");
        assert!(!tmp.path().join("project.ron").exists());
    }
}
