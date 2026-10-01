//! The editor's side of themes: `floptle_theme::host::Host`, which does the
//! choosing, watching and loading for the Hub and the editor alike, plus the
//! editor's native file dialogs for the requests it hands back.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use floptle_theme::host::{Host, Request};

#[derive(Default)]
pub(crate) struct ThemeHost {
    pub(crate) host: Host,
    import: Option<Receiver<Vec<PathBuf>>>,
    export: Option<(String, Receiver<PathBuf>)>,
    image: Option<(String, Receiver<Vec<PathBuf>>)>,
}

impl std::ops::Deref for ThemeHost {
    type Target = Host;
    fn deref(&self) -> &Host {
        &self.host
    }
}

impl std::ops::DerefMut for ThemeHost {
    fn deref_mut(&mut self) -> &mut Host {
        &mut self.host
    }
}

impl ThemeHost {
    pub(crate) fn load() -> Self {
        Self { host: Host::load(floptle_theme::source::config_dir()), ..Self::default() }
    }

    /// The host's own once-a-second check, and any dialog that has answered.
    pub(crate) fn poll(&mut self) {
        self.host.poll();
        use crate::native_dialog::{Answer, poll};
        if let Some(rx) = &self.import {
            match poll(rx) {
                Answer::Waiting => {}
                Answer::Closed => self.import = None,
                Answer::Chose(paths) => {
                    self.import = None;
                    for p in paths {
                        self.host.import_path(&p);
                    }
                }
            }
        }
        if let Some((id, rx)) = &self.export {
            match poll(rx) {
                Answer::Waiting => {}
                Answer::Closed => self.export = None,
                Answer::Chose(dest) => {
                    let id = id.clone();
                    self.export = None;
                    self.host.export_to(&id, &dest);
                }
            }
        }
        if let Some((region, rx)) = &self.image {
            match poll(rx) {
                Answer::Waiting => {}
                Answer::Closed => self.image = None,
                Answer::Chose(paths) => {
                    let region = region.clone();
                    self.image = None;
                    if let Some(p) = paths.first() {
                        self.host.image_picked(&region, p);
                    }
                }
            }
        }
    }

    /// Draw the theme settings, and open whatever dialog they asked for.
    pub(crate) fn settings_ui(&mut self, ui: &mut egui::Ui, show_pause_while_playing: bool) {
        for r in self.host.settings_ui(ui, show_pause_while_playing) {
            match r {
                Request::Import => {
                    let exts = vec![floptle_theme::source::EXTENSION.to_string()];
                    self.import = Some(crate::native_dialog::pick_files_filtered(
                        "Add a theme",
                        Some(("Floptle theme", &exts)),
                        true,
                    ));
                }
                Request::Export(id) => {
                    let name = format!("{id}.{}", floptle_theme::source::EXTENSION);
                    self.export = Some((id, crate::native_dialog::save_file("Export theme", &name)));
                }
                Request::PickImage(region) => {
                    let exts: Vec<String> = ["png", "jpg", "jpeg", "webp", "gif"].iter().map(|s| s.to_string()).collect();
                    self.image = Some((
                        region,
                        crate::native_dialog::pick_files_filtered("Choose an image", Some(("Images", &exts)), false),
                    ));
                }
                Request::OpenFolder(d) => crate::project::open_in_file_manager(&d),
            }
        }
    }

    /// Put the theme on `ctx`. The play-mode tint, which is the editor's and
    /// not the theme's, goes on top so play mode never passes for edit mode.
    pub(crate) fn apply(&self, ctx: &egui::Context, tint: Option<[u8; 3]>) {
        floptle_theme::apply(ctx, self.host.theme.clone(), &self.host.prefs);
        floptle_theme::paint::set_tint(ctx, tint);
        ctx.all_styles_mut(|s| {
            if let Some([tr, tg, tb]) = tint {
                let add = |c: egui::Color32| {
                    egui::Color32::from_rgba_premultiplied(
                        c.r().saturating_add(tr),
                        c.g().saturating_add(tg),
                        c.b().saturating_add(tb),
                        c.a(),
                    )
                };
                s.visuals.panel_fill = add(s.visuals.panel_fill);
                s.visuals.window_fill = add(s.visuals.window_fill);
                s.visuals.extreme_bg_color = add(s.visuals.extreme_bg_color);
            }
            // **Leave the scrollbar its own gutter.**
            //
            // egui's scroll bars float by default: they are drawn over the
            // contents and allocate no width. So the last few pixels of every
            // scrolling panel are behind a bar — a slider's label ellipsised
            // down to its first letter, a `…` menu half over the edge — and the
            // panel looks a little bit cut off everywhere, which is exactly
            // what it is. Allocating the bar's width moves that edge in to
            // where things can actually be seen, and every widget follows it.
            s.spacing.scroll.floating_allocated_width = s.spacing.scroll.bar_width;
        });
    }
}
