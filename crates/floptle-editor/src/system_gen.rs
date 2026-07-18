//! "🪐 New Solar System" — roll a randomized star system (floptle-field's
//! `procgen`) and generate its terrain fields on a background thread (fields
//! take ~30–60 s), streaming progress to the Console, then open the scene.

use std::path::PathBuf;

use crate::Editor;

/// The dialog's knobs (persist while the window is open).
#[derive(Clone)]
pub(crate) struct SystemGenCfg {
    /// Seed text — blank = a fresh random system every Generate.
    pub seed_text: String,
    /// Planet count, 0 = seeded random (2..=4).
    pub planets: u32,
    /// Scene (and terrain-file) base name. Overwrites on collision.
    pub scene: String,
}

impl Default for SystemGenCfg {
    fn default() -> Self {
        Self { seed_text: String::new(), planets: 0, scene: "system".into() }
    }
}

/// Messages from the generation thread.
pub(crate) enum SysGenMsg {
    Progress(String),
    Done(Result<PathBuf, String>),
}

impl Editor {
    /// Kick off generation on a background thread (one at a time).
    pub(crate) fn start_system_gen(&mut self, cfg: SystemGenCfg) {
        if self.system_gen_job.is_some() {
            self.console.push(
                floptle_script::LogLevel::Warn,
                "🪐 a solar system is already generating".into(),
                None,
            );
            return;
        }
        let seed = cfg.seed_text.trim().parse::<u32>().ok().unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() ^ d.as_secs() as u32)
                .unwrap_or(11)
        });
        let scene: String = cfg
            .scene
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .collect();
        let scene = if scene.is_empty() { "system".to_string() } else { scene };
        let planets = (cfg.planets > 0).then_some(cfg.planets);
        let root = self.project_root.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.system_gen_job = Some(rx);
        self.console.push(
            floptle_script::LogLevel::Debug,
            format!("🪐 generating solar system \"{scene}\" (seed {seed}) …"),
            None,
        );
        std::thread::spawn(move || {
            let spec = floptle_field::procgen::SystemSpec::random(seed, planets);
            let ptx = tx.clone();
            let res = floptle_field::procgen::generate_system(
                &spec,
                &scene,
                &root.join("terrain"),
                &root.join("scenes"),
                &root.join("textures/terrain"),
                &mut |line| {
                    let _ = ptx.send(SysGenMsg::Progress(line));
                },
            );
            let _ = tx.send(SysGenMsg::Done(
                res.map(|g| g.scene_path).map_err(|e| e.to_string()),
            ));
        });
    }

    /// Pump the generation thread's messages (called once per frame). On
    /// completion: rescan assets and open the freshly written scene.
    pub(crate) fn poll_system_gen(&mut self) {
        let Some(rx) = &self.system_gen_job else { return };
        let mut done = None;
        let mut lines = Vec::new();
        while let Ok(m) = rx.try_recv() {
            match m {
                SysGenMsg::Progress(l) => lines.push(l),
                SysGenMsg::Done(r) => done = Some(r),
            }
        }
        for l in lines {
            self.console.push(floptle_script::LogLevel::Debug, format!("🪐 {l}"), None);
        }
        let Some(r) = done else { return };
        self.system_gen_job = None;
        match r {
            Ok(path) => {
                self.console.push(
                    floptle_script::LogLevel::Debug,
                    "🪐 solar system ready — opening the scene".into(),
                    None,
                );
                self.asset_tree = crate::assets::build_assets(&self.project_root);
                self.open_scene_file(&path.to_string_lossy());
            }
            Err(e) => self.console.push(
                floptle_script::LogLevel::Error,
                format!("🪐 system generation failed: {e}"),
                None,
            ),
        }
    }
}
