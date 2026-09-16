//! Switches for driving the editor from a script: a frame dump of a tab that
//! nobody can click on is otherwise impossible.

impl crate::Editor {
    /// `FLOPTLE_AUTO_OPEN=particles:<effect key>` or `animation:<node>[:<state>[:<row px>]]`:
    /// put that editor in front with the thing open, once the scene is up.
    /// Called every frame; does nothing without the variable.
    #[cfg(feature = "editor-ui")]
    pub(crate) fn drive_auto_open(&mut self) {
        if self.gpu.is_some()
            && self.frame_no == 2
            && let Ok(spec) = std::env::var("FLOPTLE_AUTO_OPEN")
            && let Some((tab, what)) = spec.split_once(':')
        {
            let tab = match tab {
                "particles" => {
                    self.vfx_ui.open(what.to_string());
                    self.vfx_ui.sel_track = Some(0);
                    self.vfx_ui.expanded_tracks.insert(0);
                    self.vfx_ui.sel = Some(crate::vfx_ui::VfxSel::Clip(0, 0));
                    let anchor = self
                        .world
                        .query::<floptle_core::ParticleSystem>()
                        .find(|(_, p)| p.asset == what)
                        .map(|(e, _)| floptle_core::world_transform(&self.world, e).translation);
                    if let Some(at) = anchor {
                        self.focus_point(at, 6.0);
                    }
                    Some(crate::EditorTab::Particles)
                }
                "animation" => {
                    // `animation:<node>[:<state>[:<row height px>]]`.
                    let mut parts = what.splitn(3, ':');
                    let node_name = parts.next().unwrap_or_default();
                    let state = parts.next();
                    if let Some(px) = parts.next().and_then(|p| p.parse::<f32>().ok()) {
                        self.anim_ui.row_scale = px / 20.0;
                    }
                    let node = self
                        .world
                        .query::<floptle_core::Name>()
                        .find(|(_, n)| n.0 == node_name)
                        .map(|(e, _)| e);
                    if let Some(e) = node {
                        self.selection = vec![e];
                        self.anim_ui.target = Some(e);
                        self.anim_ui.sel_anim = state.map(str::to_string);
                    }
                    Some(crate::EditorTab::Animation)
                }
                _ => None,
            };
            if let Some(tab) = tab
                && let Some(dock) = self.dock_state.as_mut()
            {
                crate::dock::focus(dock, crate::EditorTab::Scene);
                crate::dock::focus(dock, tab);
            }
        }
    }

    #[cfg(not(feature = "editor-ui"))]
    pub(crate) fn drive_auto_open(&mut self) {}
}
