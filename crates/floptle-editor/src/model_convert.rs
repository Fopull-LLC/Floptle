//! **Turn a model the engine cannot open into one it can.**
//!
//! The Assets browser's *Convert to .glb* on an `.fbx`, `.obj`, `.stl`, `.ply`
//! or loose `.gltf`. All the actual work is [`floptle_convert`]; this is the
//! part that keeps the editor responsive while it happens and says what
//! happened afterwards.
//!
//! **Off the main thread.** The input is a file somebody else made and there is
//! no bound on its size — a character FBX is tens of megabytes. Blocking the
//! frame loop on that is reported as "the editor froze", never as "the
//! conversion took a while", so the work goes to a worker and the answer is
//! drained next frame. It is the same shape the file dialogs use, for the same
//! reason.

use std::path::{Path, PathBuf};

use floptle_script::LogLevel;

impl crate::Editor {
    /// Start converting `path`. Returns immediately.
    pub(crate) fn start_model_conversion(&mut self, path: &str) {
        if self.convert_rx.is_some() {
            self.toast = Some((
                "⏳  Still converting the last model — one at a time.".into(),
                3.0,
            ));
            return;
        }
        let src = PathBuf::from(path);
        let name = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string());

        // **Refuse before starting, not after converting.** The output existing
        // is the one failure that is certain up front, and finding out after a
        // twenty-second conversion is a waste of somebody's time.
        let out = floptle_convert::output_path(&src);
        if out.exists() {
            let msg = format!(
                "{} already exists — rename or delete it first.",
                out.file_name().unwrap_or_default().to_string_lossy()
            );
            self.console.push(LogLevel::Warn, format!("Convert: {msg}"), None);
            self.toast = Some((format!("⚠  {msg}"), 6.0));
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = floptle_convert::convert_file(&src).map_err(|e| e.to_string());
            let _ = tx.send(r);
        });
        self.convert_rx = Some((rx, name.clone()));
        self.toast = Some((format!("⏳  Converting {name}…"), 30.0));
    }

    /// Pick up a finished conversion. Called once a frame.
    pub(crate) fn poll_model_conversion(&mut self) {
        let Some((rx, name)) = &self.convert_rx else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            // The worker died without sending — nothing to report but the fact.
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the conversion stopped unexpectedly".to_string())
            }
        };
        let name = name.clone();
        self.convert_rx = None;

        match result {
            Ok((out, report)) => {
                let file = out.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.console.push(
                    LogLevel::Debug,
                    format!("Convert: {name} → {file} — {}", report.summary()),
                    None,
                );
                // Every warning on its own line. These are the things that make
                // a converted model look wrong later — a missing texture, a
                // scale that had to be guessed — and one line of summary is not
                // where somebody will find them.
                for w in &report.warnings {
                    self.console.push(LogLevel::Warn, format!("Convert: {w}"), None);
                }
                if !report.dropped.is_empty() {
                    self.console.push(
                        LogLevel::Warn,
                        format!(
                            "Convert: {} was not carried across — .glb here holds geometry, \
                             materials and textures.",
                            report.dropped.join(", ")
                        ),
                        None,
                    );
                }
                let extra = if report.warnings.is_empty() && report.dropped.is_empty() {
                    String::new()
                } else {
                    "  (see the Console)".to_string()
                };
                self.toast =
                    Some((format!("✔  {file} — {}{extra}", report.summary()), 8.0));
                self.after_convert(&out);
            }
            Err(e) => {
                self.console.push(LogLevel::Error, format!("Convert {name}: {e}"), None);
                self.toast = Some((format!("✖  {name} — see the Console"), 8.0));
            }
        }
    }

    /// Make the new file visible without anybody having to look for it.
    fn after_convert(&mut self, out: &Path) {
        // The Assets browser reads the folder afresh each frame, so there is
        // nothing to invalidate — but the file is new and the person who asked
        // for it is about to want it, so it is selected rather than left to be
        // hunted for in a folder of eighty things.
        self.selected_asset = Some(out.to_string_lossy().into_owned());
    }
}
