//! `assets.warm`: textures, material shaders and project materials prepared
//! ahead of the frame that first uses them.
//!
//! The first use of an asset costs its whole preparation on that frame: an
//! image is decoded and uploaded, a material shader is compiled and its
//! pipeline built — the slow one, measured at over a second of driver time for
//! one large shader. Loading a world that met three of them at once stalled
//! one frame for well over a second, and nothing the game could do moved it.
//!
//! Warming does the slow halves on worker threads while the game keeps
//! drawing — decoding, compiling, building the pipeline — and the quick
//! halves here, a little each frame: an upload, or installing a pipeline that
//! is already built. A frame that then uses the asset finds it ready. Models
//! and sounds go through `assets.preload`'s own loaders.
//!
//! In a browser there are no threads, so the same work runs here instead, one
//! item a frame.

use std::collections::{HashMap, HashSet, VecDeque};

use floptle_core::time::SystemTime;
use floptle_script::PreloadKind;
use floptle_shader::CompiledFragment;

use crate::Editor;

/// How long a frame spends putting warmed images on the GPU. At least one
/// goes up every frame however long it takes.
const UPLOAD_BUDGET_MS: f32 = 4.0;

/// A piece of work running elsewhere: a worker thread, or (in a browser) a
/// closure the pump runs itself.
enum Job<T> {
    #[cfg(not(target_arch = "wasm32"))]
    Thread(std::sync::mpsc::Receiver<T>),
    #[cfg(target_arch = "wasm32")]
    Later(Option<Box<dyn FnOnce() -> T>>),
}

#[cfg(not(target_arch = "wasm32"))]
fn start<T: Send + 'static>(name: &str, work: impl FnOnce() -> T + Send + 'static) -> Option<Job<T>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()
        .map(|_| Job::Thread(rx))
}

#[cfg(target_arch = "wasm32")]
fn start<T: 'static>(_name: &str, work: impl FnOnce() -> T + 'static) -> Option<Job<T>> {
    Some(Job::Later(Some(Box::new(work))))
}

impl<T> Job<T> {
    /// The result, once there is one. `Some(None)`: the worker died. Work
    /// that runs here (in a browser) runs only while `inline` allows, one
    /// piece per unit.
    fn poll(&mut self, inline: &mut usize) -> Option<Option<T>> {
        let _ = &inline;
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Job::Thread(rx) => match rx.try_recv() {
                Ok(v) => Some(Some(v)),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(None),
            },
            #[cfg(target_arch = "wasm32")]
            Job::Later(work) => {
                if *inline == 0 {
                    return None;
                }
                *inline -= 1;
                Some(work.take().map(|w| w()))
            }
        }
    }
}

/// A shader compiled and built on a worker: the compile, its pipeline (or
/// why there is none), and the file's time when it was read.
type ShaderDone = (Option<SystemTime>, Result<(CompiledFragment, floptle_render::BuiltFlsl), String>);

/// What is being warmed, and what came of it.
#[derive(Default)]
pub(crate) struct Warm {
    /// Images being decoded, by path, with the settings they were asked with.
    decoding: HashMap<String, (crate::assets::TexSetting, Job<Option<floptle_render::TextureData>>)>,
    /// Decoded images waiting for their turn to upload, oldest first.
    uploads: VecDeque<(String, crate::assets::TexSetting, floptle_render::TextureData)>,
    /// Shaders compiling, by path.
    compiling: HashMap<String, Job<ShaderDone>>,
    /// Paths that could not be warmed: a missing file, a broken image, a
    /// shader that does not compile.
    failed: HashSet<String>,
    /// A project material's images and shader, by the path it was asked for.
    materials: HashMap<String, Vec<(String, PreloadKind)>>,
    /// Images and shaders put in use by warming, so far: what a test counts.
    pub(crate) installed: u64,
}

impl Editor {
    /// Start, advance and answer `assets.warm`. Once a frame, in every host
    /// that plays the game.
    pub(crate) fn pump_warm(&mut self) {
        let t = floptle_core::profile::Span::new();
        for kind in [PreloadKind::Texture, PreloadKind::Shader, PreloadKind::Material] {
            for path in self.script_host.take_preload_requests(kind) {
                self.request_warm(&path, kind);
            }
        }
        self.advance_warm();
        for kind in [PreloadKind::Texture, PreloadKind::Shader, PreloadKind::Material] {
            let waiting = self.script_host.preload_waiting_on(kind);
            if !waiting.is_empty() {
                let status = waiting.into_iter().filter_map(|p| Some((p.clone(), self.warm_status(&p, kind)?))).collect();
                self.script_host.set_preload_status(kind, status);
            }
        }
        self.profile_record(floptle_core::profile::Bucket::Assets, t.ms());
    }

    /// Begin warming one path.
    pub(crate) fn request_warm(&mut self, path: &str, kind: PreloadKind) {
        if self.gpu.is_none() || self.raster.is_none() {
            return; // nothing to warm: every answer is "in"
        }
        match kind {
            PreloadKind::Texture => self.warm_texture(path),
            PreloadKind::Shader => self.warm_shader(path),
            PreloadKind::Material => self.warm_material(path),
            PreloadKind::Model => self.request_model(path),
            PreloadKind::Sound => {}
        }
    }

    fn warm_texture(&mut self, path: &str) {
        let want = crate::assets::tex_setting(&self.texture_settings, &self.project_root, path);
        let loaded = self.texture_registry_setting.get(path) == Some(&want) && self.texture_registry.contains_key(path);
        if loaded
            || crate::runtime_textures::names_no_file(path)
            || self.warm.decoding.contains_key(path)
            || self.warm.uploads.iter().any(|(p, ..)| p == path)
        {
            return;
        }
        self.warm.failed.remove(path);
        let Some(max) = self.gpu.as_ref().map(|g| g.device.limits().max_texture_dimension_2d) else { return };
        let file = self.resolve_asset_path(path);
        let name = path.to_string();
        match start("floptle-warm-texture", move || crate::project::decode_texture_file(&file, max, &name)) {
            Some(job) => {
                self.warm.decoding.insert(path.to_string(), (want, job));
            }
            None => {
                self.ensure_texture(path);
            }
        }
    }

    fn warm_shader(&mut self, rel: &str) {
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
        let current = self.flsl_cache.get(rel).is_some_and(|e| e.mtime == mtime && e.compiled.is_some());
        if current || self.warm.compiling.contains_key(rel) {
            return;
        }
        self.warm.failed.remove(rel);
        let Ok(src) = floptle_vfs::read_to_string(&full) else {
            self.warm.failed.insert(rel.to_string());
            return;
        };
        let (Some(gpu), Some(raster)) = (self.gpu.as_ref(), self.raster.as_ref()) else { return };
        let builder = raster.flsl_builder(gpu);
        let limits = gpu.device.limits();
        let work = move || -> ShaderDone {
            let built = crate::shaders::compile_fragment_validated(&src).and_then(|compiled| {
                floptle_render::flsl_fits_within(
                    compiled.textures.len(),
                    limits.max_sampled_textures_per_shader_stage,
                    limits.max_samplers_per_shader_stage,
                )?;
                let chunk = crate::shaders::flsl_chunk(&compiled);
                let built = builder.build(&chunk, compiled.textures.len(), crate::shaders::flsl_blend(compiled.blend));
                Ok((compiled, built))
            });
            (mtime, built)
        };
        match start("floptle-warm-shader", work) {
            Some(job) => {
                self.warm.compiling.insert(rel.to_string(), job);
            }
            None => self.ensure_flsl_shader(rel),
        }
    }

    /// A project material: everything it draws with. `materials/steel.ron`
    /// names the material `steel`.
    fn warm_material(&mut self, path: &str) {
        let name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or(path);
        // A file stem keeps a `.material` an author may have put before `.ron`.
        let name = name.strip_suffix(".material").unwrap_or(name);
        let Some(m) = self.bank_material(name) else {
            self.warm.failed.insert(path.to_string());
            return;
        };
        let mut parts: Vec<(String, PreloadKind)> = Vec::new();
        let images = m.texture.iter().chain(m.maps().into_iter().flatten()).chain(m.shader_textures.values());
        for p in images {
            parts.push((p.clone(), PreloadKind::Texture));
        }
        if let Some(s) = &m.shader {
            parts.push((s.clone(), PreloadKind::Shader));
        }
        for (p, k) in &parts {
            self.request_warm(p, *k);
        }
        self.warm.materials.insert(path.to_string(), parts);
    }

    /// Collect what the workers finished; upload images within the frame's
    /// budget; install built shaders.
    fn advance_warm(&mut self) {
        // Work with no thread to run on: one piece a frame.
        let mut inline = 1usize;
        let mut decoded = Vec::new();
        self.warm.decoding.retain(|path, (want, job)| match job.poll(&mut inline) {
            None => true,
            Some(data) => {
                decoded.push((path.clone(), *want, data.flatten()));
                false
            }
        });
        for (path, want, data) in decoded {
            match data {
                Some(d) => self.warm.uploads.push_back((path, want, d)),
                None => {
                    self.warm.failed.insert(path);
                }
            }
        }
        let t = floptle_core::profile::Span::new();
        while let Some((path, want, data)) = self.warm.uploads.pop_front() {
            // A frame that drew it already loaded it: nothing to do but answer.
            let current = self.texture_registry_setting.get(&path) == Some(&want);
            if !current && self.register_decoded_texture(&path, &data, want).is_some() {
                self.warm.installed += 1;
            }
            if t.ms() >= UPLOAD_BUDGET_MS {
                break;
            }
        }
        let mut built = Vec::new();
        self.warm.compiling.retain(|rel, job| match job.poll(&mut inline) {
            None => true,
            Some(done) => {
                built.push((rel.clone(), done));
                false
            }
        });
        for (rel, done) in built {
            match done {
                Some((mtime, Ok((compiled, shader)))) => {
                    // Compiled meanwhile by a frame that needed it, or edited
                    // since: the frame's own answer stands.
                    let full = self.resolve_asset_path(&rel);
                    let stale = floptle_vfs::modified(&full) != mtime;
                    let have = self.flsl_cache.get(&rel).is_some_and(|e| e.mtime == mtime && e.compiled.is_some());
                    if !stale && !have {
                        self.install_flsl(&rel, mtime, compiled, Some(shader));
                        self.warm.installed += 1;
                    }
                }
                Some((_, Err(why))) => {
                    self.console.push(floptle_script::LogLevel::Error, format!("◈ assets.warm {rel}: {why}"), None);
                    self.warm.failed.insert(rel);
                }
                None => {
                    self.warm.failed.insert(rel);
                }
            }
        }
    }

    /// `Some(true)` ready, `Some(false)` failed, `None` still on its way.
    fn warm_status(&self, path: &str, kind: PreloadKind) -> Option<bool> {
        if self.gpu.is_none() || self.raster.is_none() {
            return Some(true);
        }
        if self.warm.failed.contains(path) {
            return Some(false);
        }
        match kind {
            PreloadKind::Texture => {
                if self.warm.decoding.contains_key(path) || self.warm.uploads.iter().any(|(p, ..)| p == path) {
                    None
                } else {
                    Some(self.texture_registry.contains_key(path))
                }
            }
            PreloadKind::Shader => {
                if self.warm.compiling.contains_key(path) {
                    None
                } else {
                    Some(self.flsl_cache.get(path).is_some_and(|e| e.compiled.is_some()))
                }
            }
            PreloadKind::Material => {
                let parts = self.warm.materials.get(path)?;
                let mut all = true;
                for (p, k) in parts {
                    all &= self.warm_status(p, *k)?;
                }
                Some(all)
            }
            PreloadKind::Model => self.model_status(path),
            PreloadKind::Sound => Some(true),
        }
    }
}
