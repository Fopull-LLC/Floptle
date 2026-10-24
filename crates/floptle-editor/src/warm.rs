//! `assets.warm`: textures, shaders and project materials prepared ahead of
//! the frame that first uses them.
//!
//! The first use of an asset costs its whole preparation on that frame: an
//! image is decoded and uploaded, a shader is compiled and its pipeline
//! built — the slow one, measured at over a second of driver time for one
//! large shader. Loading a world that met three of them at once stalled one
//! frame for well over a second, and nothing the game could do moved it.
//!
//! Warming does the slow halves on a few worker threads while the game keeps
//! drawing — decoding, compiling, building the pipeline — and the quick
//! halves here, a little each frame: an upload, or installing a pipeline that
//! is already built. A frame that then uses the asset finds it ready, and a
//! frame that uses one still on its way waits for that work rather than
//! starting it again beside it. Models and sounds go through
//! `assets.preload`'s own loaders.
//!
//! The workers are a small pool, not a thread per asset: a few decoding
//! images, and one building shaders. A loading screen warming a hundred and
//! fifty pictures and four shaders at once started a hundred and fifty-four
//! threads, up to eighty of them running together, and its own frames slowed
//! while they fought the game for every core.
//!
//! In a browser there are no threads, so the same work runs here instead, one
//! item a frame.

use std::collections::{HashMap, HashSet, VecDeque};

use floptle_core::time::SystemTime;
use floptle_script::PreloadKind;

use crate::Editor;

/// How long a frame spends putting warmed images on the GPU. At least one
/// goes up every frame however long it takes.
const UPLOAD_BUDGET_MS: f32 = 4.0;

/// How many images decode at once: half the machine's threads less the one
/// building shaders, at least one and at most three. The rest are the game's.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn image_workers() -> usize {
    std::thread::available_parallelism().map_or(2, |n| n.get() / 2).saturating_sub(1).clamp(1, 3)
}

/// Shaders build one at a time. Each build is a long driver call that keeps
/// threads of its own busy inside the driver, and four at once made the
/// frames drawn beside them slower than one did. A loading screen takes a
/// little longer; the game it shows stays smooth.
pub(crate) const SHADER_WORKERS: usize = 1;

#[cfg(not(target_arch = "wasm32"))]
mod pool {
    //! A warm's workers: a bounded number of threads, started when there is
    //! work and gone when there is none, so an editor that never warms
    //! anything holds none.

    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};

    type Work<T> = Box<dyn FnOnce() -> T + Send>;

    /// Where one piece of work is: waiting for a worker, on one, finished, or
    /// already handed over.
    enum State<T> {
        Queued(Work<T>),
        Running,
        /// `None`: the work panicked.
        Done(Option<T>),
        Taken,
    }

    /// One piece of work and its result, shared by the worker that runs it
    /// and whichever frame wants it first.
    pub(super) struct Slot<T> {
        state: Mutex<State<T>>,
        done: Condvar,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|p| p.into_inner())
    }

    impl<T> Slot<T> {
        /// The worker's half: run the work unless a frame took it first.
        fn run(&self) {
            let work = {
                let mut s = lock(&self.state);
                match std::mem::replace(&mut *s, State::Running) {
                    State::Queued(w) => w,
                    other => {
                        *s = other;
                        return;
                    }
                }
            };
            let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok();
            *lock(&self.state) = State::Done(out);
            self.done.notify_all();
        }

        /// The result, if the work has finished. `Some(None)`: it panicked.
        pub(super) fn try_take(&self) -> Option<Option<T>> {
            let mut s = lock(&self.state);
            match std::mem::replace(&mut *s, State::Taken) {
                State::Done(v) => Some(v),
                State::Taken => Some(None),
                other => {
                    *s = other;
                    None
                }
            }
        }

        /// The result now: waited for if a worker has it, run here if none
        /// has started it yet. Either way the work is done once.
        pub(super) fn wait(&self) -> Option<T> {
            let mut s = lock(&self.state);
            loop {
                match std::mem::replace(&mut *s, State::Taken) {
                    State::Queued(w) => {
                        drop(s);
                        return std::panic::catch_unwind(std::panic::AssertUnwindSafe(w)).ok();
                    }
                    State::Running => {
                        *s = State::Running;
                        s = self.done.wait(s).unwrap_or_else(|p| p.into_inner());
                    }
                    State::Done(v) => return v,
                    State::Taken => return None,
                }
            }
        }
    }

    struct Queue {
        tasks: VecDeque<Box<dyn FnOnce() + Send>>,
        /// Worker threads alive now.
        workers: usize,
    }

    struct Inner {
        queue: Mutex<Queue>,
        limit: usize,
        running: AtomicUsize,
        peak: AtomicUsize,
        started: AtomicUsize,
    }

    /// A bounded set of worker threads and the work waiting for them.
    pub(crate) struct Pool(Arc<Inner>);

    impl Pool {
        pub(crate) fn new(limit: usize) -> Self {
            Pool(Arc::new(Inner {
                queue: Mutex::new(Queue { tasks: VecDeque::new(), workers: 0 }),
                limit: limit.max(1),
                running: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                started: AtomicUsize::new(0),
            }))
        }

        /// Queue `work`. `None` when there is no worker and none could be
        /// started: the caller does the work itself.
        pub(super) fn submit<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> Option<Arc<Slot<T>>> {
            let slot = Arc::new(Slot { state: Mutex::new(State::Queued(Box::new(work))), done: Condvar::new() });
            let run = slot.clone();
            let mut q = lock(&self.0.queue);
            q.tasks.push_back(Box::new(move || run.run()));
            if q.workers < self.0.limit {
                let inner = self.0.clone();
                let spawned = std::thread::Builder::new().name("floptle-warm".into()).spawn(move || work_loop(&inner));
                match spawned {
                    Ok(_) => {
                        q.workers += 1;
                        self.0.started.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) if q.workers == 0 => {
                        q.tasks.pop_back();
                        return None;
                    }
                    Err(_) => {}
                }
            }
            Some(slot)
        }

        /// The most pieces of work that have run at once, and the threads
        /// started, so far: what a test counts.
        #[cfg(test)]
        pub(crate) fn peak(&self) -> usize {
            self.0.peak.load(Ordering::Relaxed)
        }

        #[cfg(test)]
        pub(crate) fn started(&self) -> usize {
            self.0.started.load(Ordering::Relaxed)
        }

        /// Nothing queued and nothing running.
        #[cfg(test)]
        pub(crate) fn idle(&self) -> bool {
            lock(&self.0.queue).tasks.is_empty() && self.0.running.load(Ordering::Relaxed) == 0
        }
    }

    /// A worker: take work until there is none, then end.
    fn work_loop(inner: &Inner) {
        loop {
            let task = {
                let mut q = lock(&inner.queue);
                match q.tasks.pop_front() {
                    Some(t) => {
                        // Counted under the lock, so work is never in neither
                        // the queue nor the count.
                        let now = inner.running.fetch_add(1, Ordering::Relaxed) + 1;
                        inner.peak.fetch_max(now, Ordering::Relaxed);
                        t
                    }
                    None => {
                        q.workers -= 1;
                        return;
                    }
                }
            };
            task();
            inner.running.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// The pool on its own, without a GPU, so it is checked on every machine.
    #[cfg(test)]
    mod tests {
        use super::Pool;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{mpsc, Arc};
        use std::time::Duration;

        /// **Never more running than the limit**, however much is queued.
        #[test]
        fn a_pool_runs_no_more_than_its_limit_at_once() {
            let pool = Pool::new(3);
            let jobs: Vec<_> = (0..40)
                .map(|i| {
                    pool.submit(move || {
                        std::thread::sleep(Duration::from_millis(2));
                        i
                    })
                    .expect("a worker")
                })
                .collect();
            let sum: usize = jobs.iter().map(|j| j.wait().expect("the job ran")).sum();
            assert_eq!(sum, (0..40).sum::<usize>());
            assert!(pool.peak() <= 3, "{} jobs ran at once on a pool of 3", pool.peak());
            assert!(pool.started() < 40, "a thread a job: {} threads for 40 jobs", pool.started());
        }

        /// **Waiting on work no worker has begun runs it here, once.** The
        /// worker that reaches it later finds it taken.
        #[test]
        fn waiting_on_queued_work_runs_it_here_exactly_once() {
            let pool = Pool::new(1);
            let (release, gate) = mpsc::channel::<()>();
            let busy = pool.submit(move || gate.recv().is_ok()).expect("a worker");
            let runs = Arc::new(AtomicUsize::new(0));
            let r = runs.clone();
            let queued = pool
                .submit(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                    std::thread::current().id()
                })
                .expect("queued");
            let ran_on = queued.wait().expect("ran");
            assert_eq!(ran_on, std::thread::current().id(), "queued work was not run by the one waiting");
            release.send(()).unwrap();
            assert_eq!(busy.wait(), Some(true));
            while !pool.idle() {
                std::thread::yield_now();
            }
            assert_eq!(runs.load(Ordering::SeqCst), 1, "the work ran twice");
        }

        /// **Waiting on work a worker is running waits for that run.**
        #[test]
        fn waiting_on_running_work_waits_for_it() {
            let pool = Pool::new(1);
            let (started, on) = mpsc::channel::<()>();
            let runs = Arc::new(AtomicUsize::new(0));
            let r = runs.clone();
            let job = pool
                .submit(move || {
                    let _ = started.send(());
                    std::thread::sleep(Duration::from_millis(30));
                    r.fetch_add(1, Ordering::SeqCst);
                    std::thread::current().id()
                })
                .expect("a worker");
            on.recv().unwrap();
            let ran_on = job.wait().expect("ran");
            assert_ne!(ran_on, std::thread::current().id(), "running work was run again here");
            assert_eq!(runs.load(Ordering::SeqCst), 1);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod pool {
    /// A page has no threads: every piece of work runs on the frame.
    pub(crate) struct Pool;

    impl Pool {
        pub(crate) fn new(_limit: usize) -> Self {
            Pool
        }
    }
}

/// A piece of work running elsewhere: on the pool, or (in a browser) a
/// closure the pump runs itself.
enum Job<T> {
    #[cfg(not(target_arch = "wasm32"))]
    Pool(std::sync::Arc<pool::Slot<T>>),
    #[cfg(target_arch = "wasm32")]
    Later(Option<Box<dyn FnOnce() -> T>>),
}

#[cfg(not(target_arch = "wasm32"))]
fn start<T: Send + 'static>(pool: &pool::Pool, work: impl FnOnce() -> T + Send + 'static) -> Option<Job<T>> {
    pool.submit(work).map(Job::Pool)
}

#[cfg(target_arch = "wasm32")]
fn start<T: 'static>(_pool: &pool::Pool, work: impl FnOnce() -> T + 'static) -> Option<Job<T>> {
    Some(Job::Later(Some(Box::new(work))))
}

impl<T> Job<T> {
    /// The result, once there is one. `Some(None)`: the work died. Work
    /// that runs here (in a browser) runs only while `inline` allows, one
    /// piece per unit.
    fn poll(&mut self, inline: &mut usize) -> Option<Option<T>> {
        let _ = &inline;
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Job::Pool(slot) => slot.try_take(),
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

    /// The result now, for a frame that cannot draw without it: waited for
    /// if it is under way, done here if it has not started.
    fn wait(self) -> Option<T> {
        match self {
            #[cfg(not(target_arch = "wasm32"))]
            Job::Pool(slot) => slot.wait(),
            #[cfg(target_arch = "wasm32")]
            Job::Later(work) => work.map(|w| w()),
        }
    }
}

/// A shader compiled and built on a worker, by the stage it declares.
enum BuiltShader {
    Material(Box<(floptle_shader::CompiledFragment, floptle_render::BuiltFlsl)>),
    Ui(floptle_shader::CompiledUi, floptle_render::BuiltUiShader),
    Post(floptle_shader::CompiledPost, floptle_render::BuiltPostShader),
    /// A stage nothing in this host draws (a UI shader with no UI pass):
    /// nothing to prepare.
    Nothing,
}

/// A shader's compile and build (or why there is none), and the file's time
/// when it was read.
type ShaderDone = (Option<SystemTime>, Result<BuiltShader, String>);

/// What is being warmed, and what came of it.
pub(crate) struct Warm {
    /// The workers decoding images, and the one building shaders.
    pub(crate) images: pool::Pool,
    pub(crate) shaders: pool::Pool,
    /// Images being decoded, by path, with the settings they were asked with.
    decoding: HashMap<String, (crate::assets::TexSetting, Job<Option<floptle_render::TextureData>>)>,
    /// Decoded images waiting for their turn to upload, oldest first.
    uploads: VecDeque<(String, crate::assets::TexSetting, floptle_render::TextureData)>,
    /// Shaders compiling, by path.
    compiling: HashMap<String, Job<ShaderDone>>,
    /// Shaders of a stage this host does not draw: ready by having nothing
    /// to do.
    nothing: HashSet<String>,
    /// Paths that could not be warmed: a missing file, a broken image, a
    /// shader that does not compile.
    failed: HashSet<String>,
    /// A project material's images and shader, by the path it was asked for.
    materials: HashMap<String, Vec<(String, PreloadKind)>>,
    /// Images and shaders put in use by warming, so far: what a test counts.
    pub(crate) installed: u64,
    /// Every image decode, warmed or not, on any thread: what a test counts
    /// to see that a first use waited for a warm instead of decoding again.
    pub(crate) decodes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[cfg(test)]
impl Warm {
    /// Anything still decoding, waiting to upload, or building.
    pub(crate) fn busy(&self) -> bool {
        !self.decoding.is_empty() || !self.uploads.is_empty() || !self.compiling.is_empty()
    }

    /// Images still decoding or waiting to upload.
    pub(crate) fn images_pending(&self) -> usize {
        self.decoding.len() + self.uploads.len()
    }

    /// Any shader still building or not yet landed.
    pub(crate) fn shaders_pending(&self) -> bool {
        !self.compiling.is_empty()
    }

    pub(crate) fn failed(&self) -> &HashSet<String> {
        &self.failed
    }
}

impl Default for Warm {
    fn default() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let images = image_workers();
        #[cfg(target_arch = "wasm32")]
        let images = 1;
        Self {
            images: pool::Pool::new(images),
            shaders: pool::Pool::new(SHADER_WORKERS),
            decoding: HashMap::new(),
            uploads: VecDeque::new(),
            compiling: HashMap::new(),
            nothing: HashSet::new(),
            failed: HashSet::new(),
            materials: HashMap::new(),
            installed: 0,
            decodes: Default::default(),
        }
    }
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
                let status = waiting
                    .into_iter()
                    .filter_map(|p| {
                        let key = self.warm_key(&p);
                        Some((p, self.warm_status(&key, kind)?))
                    })
                    .collect();
                self.script_host.set_preload_status(kind, status);
            }
        }
        self.profile_record(floptle_core::profile::Bucket::Assets, t.ms());
    }

    /// The name a warm keeps an asset under: the project-relative path a
    /// scene draws it by. A full path to a file inside the project, as
    /// `assets.getContents` once answered, is the same file, and warming it
    /// under that second name prepared it a second time for nothing to use.
    fn warm_key(&self, path: &str) -> String {
        let rel = crate::assets::asset_rel_path(path, &self.project_root);
        match rel.strip_prefix("./") {
            Some(r) => r.to_string(),
            None => rel,
        }
    }

    /// Begin warming one path.
    pub(crate) fn request_warm(&mut self, path: &str, kind: PreloadKind) {
        if self.gpu.is_none() || self.raster.is_none() {
            return; // nothing to warm: every answer is "in"
        }
        let key = self.warm_key(path);
        let path = key.as_str();
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
        let decodes = self.warm.decodes.clone();
        let work = move || {
            decodes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            crate::project::decode_texture_file(&file, max, &name)
        };
        match start(&self.warm.images, work) {
            Some(job) => {
                self.warm.decoding.insert(path.to_string(), (want, job));
            }
            None => {
                self.ensure_texture(path);
            }
        }
    }

    /// Is `rel` compiled and in use, as of the file's time `mtime`, in the
    /// cache of whichever stage it is?
    fn shader_current(&self, rel: &str, mtime: Option<SystemTime>) -> bool {
        self.flsl_cache.get(rel).is_some_and(|e| e.mtime == mtime && e.compiled.is_some())
            || self.ui_flsl_cache.get(rel).is_some_and(|e| e.mtime == mtime && e.compiled.is_some())
            || self.post_flsl_cache.get(rel).is_some_and(|e| e.mtime == mtime && e.compiled.is_some())
    }

    fn warm_shader(&mut self, rel: &str) {
        let full = self.resolve_asset_path(rel);
        let mtime = floptle_vfs::modified(&full);
        if self.shader_current(rel, mtime) || self.warm.compiling.contains_key(rel) {
            return;
        }
        self.warm.failed.remove(rel);
        self.warm.nothing.remove(rel);
        let Ok(src) = floptle_vfs::read_to_string(&full) else {
            self.warm.failed.insert(rel.to_string());
            return;
        };
        let Some(gpu) = self.gpu.as_ref() else { return };
        if self.post_shaders.is_none() {
            self.post_shaders = Some(floptle_render::PostShaders::new(gpu));
        }
        // Each pass's builder, so the worker can build whichever stage the
        // file turns out to be.
        let material = self.raster.as_ref().map(|r| r.flsl_builder(gpu));
        let ui = self.ui_render.as_ref().map(|u| u.ui_shader_builder(gpu));
        let post = self.post_shaders.as_ref().map(|p| p.builder(gpu));
        let limits = gpu.device.limits();
        let work = move || -> ShaderDone {
            use floptle_shader::Stage;
            let stage = floptle_shader::parse(&src).ok().and_then(|ir| ir.stage).unwrap_or(Stage::Fragment);
            let built = match stage {
                Stage::Fragment => crate::shaders::compile_fragment_validated(&src).and_then(|compiled| {
                    let Some(builder) = material else { return Ok(BuiltShader::Nothing) };
                    floptle_render::flsl_fits_within(
                        compiled.textures.len(),
                        limits.max_sampled_textures_per_shader_stage,
                        limits.max_samplers_per_shader_stage,
                    )?;
                    let chunk = crate::shaders::flsl_chunk(&compiled);
                    let built = builder.build(&chunk, compiled.textures.len(), crate::shaders::flsl_blend(compiled.blend));
                    Ok(BuiltShader::Material(Box::new((compiled, built))))
                }),
                Stage::Ui => crate::shaders::compile_ui_validated(&src).map(|compiled| match ui {
                    Some(builder) => {
                        let built = builder.build(&crate::shaders::ui_chunk(&compiled));
                        BuiltShader::Ui(compiled, built)
                    }
                    None => BuiltShader::Nothing,
                }),
                Stage::Post => crate::shaders::compile_post_validated(&src).map(|compiled| match post {
                    Some(builder) => {
                        let built = builder.build(&crate::shaders::post_module_source(&compiled.chunk));
                        BuiltShader::Post(compiled, built)
                    }
                    None => BuiltShader::Nothing,
                }),
                Stage::Sky => Err("a sky shader is built into the sky pass when a Skybox wears it, \
                                   and cannot be prepared ahead"
                    .into()),
                Stage::Sdf => Err("an sdf shader is built into the scene's field when a Field Shape \
                                   wears it, and cannot be prepared ahead"
                    .into()),
            };
            (mtime, built)
        };
        match start(&self.warm.shaders, work) {
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
        // One shader lands a frame. Each one that lands has every material
        // wearing it bound afresh on the next walk, and several landing
        // together put all of that on one frame.
        let mut landed = None;
        self.warm.compiling.retain(|rel, job| {
            if landed.is_some() {
                return true;
            }
            match job.poll(&mut inline) {
                None => true,
                Some(done) => {
                    landed = Some((rel.clone(), done));
                    false
                }
            }
        });
        if let Some((rel, done)) = landed {
            self.land_warm_shader(&rel, done);
        }
    }

    /// Put a shader a worker finished in use, unless a frame that needed it
    /// compiled it meanwhile or the file changed since: then the frame's own
    /// answer stands.
    fn land_warm_shader(&mut self, rel: &str, done: Option<ShaderDone>) {
        let Some((mtime, built)) = done else {
            self.warm.failed.insert(rel.to_string());
            return;
        };
        let built = match built {
            Ok(b) => b,
            Err(why) => {
                self.console.push(floptle_script::LogLevel::Error, format!("◈ assets.warm {rel}: {why}"), None);
                self.warm.failed.insert(rel.to_string());
                return;
            }
        };
        let full = self.resolve_asset_path(rel);
        if floptle_vfs::modified(&full) != mtime || self.shader_current(rel, mtime) {
            return;
        }
        let install = floptle_core::profile::Span::new();
        match built {
            BuiltShader::Material(b) => {
                let (compiled, shader) = *b;
                self.install_flsl(rel, mtime, compiled, Some(shader));
            }
            BuiltShader::Ui(compiled, shader) => self.install_ui_flsl(rel, mtime, compiled, shader),
            BuiltShader::Post(compiled, shader) => self.install_post_flsl(rel, mtime, compiled, shader),
            BuiltShader::Nothing => {
                self.warm.nothing.insert(rel.to_string());
                return;
            }
        }
        self.note_asset(rel, "shaderInstall", install.ms());
        self.warm.installed += 1;
    }

    /// A first use of an image a warm is still preparing: the decode it
    /// started, waited for (or run here if no worker has begun it), or the
    /// decoded picture waiting its turn to upload. `None`: no warm has it.
    pub(crate) fn adopt_warm_texture(&mut self, path: &str) -> Option<Option<floptle_render::TextureData>> {
        if let Some(at) = self.warm.uploads.iter().position(|(p, ..)| p == path) {
            return self.warm.uploads.remove(at).map(|(_, _, d)| Some(d));
        }
        let (_, job) = self.warm.decoding.remove(path)?;
        let wait = floptle_core::profile::Span::new();
        let data = job.wait().flatten();
        self.note_asset(path, "assetDecode", wait.ms());
        if data.is_none() {
            self.warm.failed.insert(path.to_string());
        }
        Some(data)
    }

    /// A first use of a shader a warm is still building: wait for that build
    /// (or run it here if no worker has begun it) and put it in use, so the
    /// frame does not build it a second time beside the warm.
    pub(crate) fn adopt_warm_shader(&mut self, rel: &str) {
        let Some(job) = self.warm.compiling.remove(rel) else { return };
        let wait = floptle_core::profile::Span::new();
        let done = job.wait();
        self.note_asset(rel, "pipelineCompile", wait.ms());
        self.land_warm_shader(rel, done);
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
                    Some(
                        self.warm.nothing.contains(path)
                            || self.flsl_cache.get(path).is_some_and(|e| e.compiled.is_some())
                            || self.ui_flsl_cache.get(path).is_some_and(|e| e.compiled.is_some())
                            || self.post_flsl_cache.get(path).is_some_and(|e| e.compiled.is_some()),
                    )
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
