//! A program's side of themes: which one is chosen, keeping it current, and
//! carrying out what the theme settings ask for. The editor and the Hub each
//! hold one; only the file dialogs differ, and those come back as
//! [`Request`]s.
//!
//! - **One choice, two programs.** Both write the same `theme.ron`, and each
//!   checks it once a second, so choosing a theme in the Hub changes an open
//!   editor too, and the other way round.
//! - **Editing a theme by hand works live.** The chosen theme's own files are
//!   watched the same way: save `theme.ron` in a text editor and the window
//!   redraws in it. A save that does not parse keeps the last good version
//!   and says why, so a half-typed line never blanks the window.
//! - **Package themes.** A host passes its loaded packages; their `themes/`
//!   folders are listed. A theme chosen from one is copied into your themes
//!   folder first, so it stays yours whichever project is open, and in the
//!   Hub.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::model::Origin;
use crate::source::{self, Source};
use crate::ui::{Action, Settings};
use crate::{Library, Prefs, Theme};

/// Something only the program can do: open a dialog, a file manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Pick `.floptletheme` files to add; answer with [`Host::import_path`].
    Import,
    /// Pick where to save this theme; answer with [`Host::export_to`].
    Export(String),
    /// Pick an image for a region in the theme editor; answer with
    /// [`Host::image_picked`].
    PickImage(String),
    /// Show this folder in the file manager.
    OpenFolder(PathBuf),
}

pub struct Host {
    pub prefs: Prefs,
    pub library: Library,
    /// The theme being drawn: the chosen one, or the theme editor's preview.
    pub theme: Arc<Theme>,
    /// The chosen theme, kept while a preview is showing.
    chosen: Arc<Theme>,
    pub settings: Settings,
    /// Changes whenever the fonts must be set again.
    pub fonts_fingerprint: u64,
    config: Option<PathBuf>,
    packages: Vec<(String, PathBuf)>,
    prefs_seen: Option<SystemTime>,
    source_seen: Option<SystemTime>,
    last_check: Option<Instant>,
}

impl Default for Host {
    fn default() -> Self {
        let t = crate::default_theme();
        Self {
            prefs: Prefs::default(),
            library: Library::default(),
            fonts_fingerprint: crate::fonts::fingerprint(&t),
            theme: t.clone(),
            chosen: t,
            settings: Settings::default(),
            config: None,
            packages: Vec::new(),
            prefs_seen: None,
            source_seen: None,
            last_check: None,
        }
    }
}

impl Host {
    /// Read the saved choice from `config` (normally [`source::config_dir`])
    /// and load it. A theme that will not load leaves Floptle Dark showing
    /// and says why.
    pub fn load(config: Option<PathBuf>) -> Self {
        let mut h = Self { config, ..Self::default() };
        if let Some(c) = &h.config {
            h.prefs = source::load_prefs(c);
            h.prefs_seen = source::modified(&source::prefs_path(c));
        }
        h.rescan();
        h.reload_chosen();
        h
    }

    pub fn user_dir(&self) -> Option<PathBuf> {
        self.config.as_deref().map(source::user_dir)
    }

    pub fn rescan(&mut self) {
        self.library = Library::scan(self.user_dir().as_deref(), &self.packages);
        for r in &self.library.refused {
            log::warn!("theme not loaded: {}", r.error);
        }
    }

    fn reload_chosen(&mut self) {
        let id = self.prefs.theme.clone();
        match self.library.load(&id) {
            Ok(t) => {
                for w in &t.warnings {
                    log::warn!("theme {id}: {w}");
                }
                self.source_seen = self.library.find(&id).and_then(|e| source::source_modified(&e.source));
                self.set_chosen(Arc::new(t));
            }
            Err(e) => {
                log::warn!("theme {id} not loaded, using Floptle Dark: {e}");
                self.settings.notice = Some((e.to_string(), true));
                if self.chosen.id != id {
                    self.set_chosen(crate::default_theme());
                }
            }
        }
    }

    fn set_chosen(&mut self, t: Arc<Theme>) {
        self.chosen = t.clone();
        if !self.settings.is_editing() {
            self.show(t);
        }
    }

    fn show(&mut self, t: Arc<Theme>) {
        self.fonts_fingerprint = crate::fonts::fingerprint(&t);
        self.theme = t;
    }

    pub fn save_prefs(&mut self) {
        if let Some(c) = &self.config {
            source::save_prefs(c, &self.prefs);
            self.prefs_seen = source::modified(&source::prefs_path(c));
        }
    }

    /// Use `id`. A package's theme is copied into your folder first.
    pub fn choose(&mut self, id: &str) {
        if let Some(e) = self.library.find(id).cloned()
            && matches!(e.origin, Origin::Package { .. })
            && let Some(dir) = self.user_dir()
        {
            match source::install(&e.source, &dir) {
                Ok(_) => self.rescan(),
                Err(err) => {
                    self.settings.notice = Some((err.to_string(), true));
                    return;
                }
            }
        }
        self.prefs.theme = id.to_string();
        self.save_prefs();
        self.reload_chosen();
    }

    /// The packages that loaded, so their themes are listed. Cheap to call
    /// every frame: it rescans only when the set changed.
    pub fn set_packages(&mut self, pkgs: Vec<(String, PathBuf)>) {
        if pkgs != self.packages {
            self.packages = pkgs;
            self.rescan();
        }
    }

    /// Once a second: has the other program chosen a theme, or has somebody
    /// saved the chosen theme's files?
    pub fn poll(&mut self) {
        if self.last_check.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.last_check = Some(Instant::now());
        if let Some(c) = self.config.clone() {
            let m = source::modified(&source::prefs_path(&c));
            if m != self.prefs_seen {
                self.prefs_seen = m;
                let p = source::load_prefs(&c);
                if p != self.prefs {
                    let new_theme = p.theme != self.prefs.theme;
                    self.prefs = p;
                    if new_theme {
                        self.rescan();
                        self.reload_chosen();
                    }
                }
            }
        }
        if let Some(e) = self.library.find(&self.prefs.theme) {
            let m = source::source_modified(&e.source);
            if m.is_some() && m != self.source_seen {
                self.source_seen = m;
                let (src, origin) = (e.source.clone(), e.origin.clone());
                match source::load(&src, origin, true) {
                    Ok(t) => {
                        self.settings.notice = None;
                        self.set_chosen(Arc::new(t));
                    }
                    Err(err) => {
                        self.settings.notice = Some((format!("{err} (still showing the last version that loaded)"), true));
                    }
                }
            }
        }
    }

    /// Add a theme from a `.floptletheme` or a folder (dropped on the window,
    /// or picked with Import), and choose it.
    pub fn import_path(&mut self, p: &Path) {
        let Some(dir) = self.user_dir() else { return };
        let src = if p.is_dir() { Source::Dir(p.to_path_buf()) } else { Source::Zip(p.to_path_buf()) };
        match source::install(&src, &dir) {
            Ok(id) => {
                self.rescan();
                let name = self.library.find(&id).map_or(id.clone(), |e| e.name.clone());
                self.choose(&id);
                self.settings.notice = Some((format!("Added {name}"), false));
            }
            Err(e) => self.settings.notice = Some((e.to_string(), true)),
        }
    }

    /// Is this a file the theme system takes, if somebody drops it on the window?
    pub fn is_theme_file(p: &Path) -> bool {
        p.extension().is_some_and(|x| x.eq_ignore_ascii_case(source::EXTENSION))
    }

    /// Save theme `id` as a `.floptletheme` at `dest`.
    pub fn export_to(&mut self, id: &str, dest: &Path) {
        let dest = if dest.extension().is_some_and(|x| x == source::EXTENSION) {
            dest.to_path_buf()
        } else {
            dest.with_extension(source::EXTENSION)
        };
        let r = self
            .library
            .load(id)
            .map_err(|e| e.to_string())
            .and_then(|t| source::write_zip(&t.file, &t.assets, &dest).map_err(|e| e.to_string()));
        self.settings.notice = Some(match r {
            Ok(()) => (format!("Saved {}", dest.display()), false),
            Err(e) => (format!("Could not export: {e}"), true),
        });
    }

    /// The image the user picked after [`Request::PickImage`].
    pub fn image_picked(&mut self, region: &str, p: &Path) {
        match std::fs::read(p) {
            Ok(bytes) => {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "image.png".into());
                if let Some(Action::Preview(t)) = self.settings.add_image(region, &name, bytes) {
                    self.show(t);
                }
            }
            Err(e) => self.settings.notice = Some((format!("{}: {e}", p.display()), true)),
        }
    }

    /// Draw the theme settings and do what they ask; what only the program
    /// can do comes back.
    pub fn settings_ui(&mut self, ui: &mut egui::Ui, show_pause_while_playing: bool) -> Vec<Request> {
        let rt_info = {
            let (draws, errors) = crate::paint::shared(ui.ctx())
                .map(|s| {
                    let rt = s.lock();
                    (rt.draws_per_second, rt.shader_errors.iter().map(|(a, b)| (a.clone(), b.clone())).collect())
                })
                .unwrap_or_default();
            crate::ui::Runtime { draws_per_second: draws, shader_errors: errors, show_pause_while_playing }
        };
        let current = self.chosen.clone();
        let actions = crate::ui::settings(ui, &mut self.settings, &self.library, &current, &mut self.prefs, &rt_info);
        let mut out = Vec::new();
        for a in actions {
            match a {
                Action::Choose(id) => self.choose(&id),
                Action::PrefsChanged => self.save_prefs(),
                Action::Rescan => {
                    self.rescan();
                    self.reload_chosen();
                }
                Action::Import => out.push(Request::Import),
                Action::OpenFolder => {
                    if let Some(d) = self.user_dir() {
                        let _ = std::fs::create_dir_all(&d);
                        out.push(Request::OpenFolder(d));
                    }
                }
                Action::Export(id) => out.push(Request::Export(id)),
                Action::Delete(id) => {
                    if let Some(e) = self.library.find(&id).cloned() {
                        let r = match &e.source {
                            Source::Dir(p) => std::fs::remove_dir_all(p),
                            Source::Zip(p) => std::fs::remove_file(p),
                            Source::Builtin(_) => Ok(()),
                        };
                        if let Err(err) = r {
                            self.settings.notice = Some((err.to_string(), true));
                        }
                        self.rescan();
                        self.choose(source::DEFAULT_ID);
                    }
                }
                Action::Preview(t) => self.show(t),
                Action::EndPreview => {
                    let t = self.chosen.clone();
                    self.show(t);
                }
                Action::Save(file, assets) => {
                    let Some(dir) = self.user_dir() else { continue };
                    match source::write_dir(&file, &assets, &dir) {
                        Ok(_) => {
                            self.rescan();
                            self.choose(&file.id);
                            self.settings.notice = Some((format!("Saved {}", file.name), false));
                        }
                        Err(e) => self.settings.notice = Some((format!("Could not save: {e}"), true)),
                    }
                }
                Action::PickImage(region) => out.push(Request::PickImage(region)),
            }
        }
        out
    }
}
